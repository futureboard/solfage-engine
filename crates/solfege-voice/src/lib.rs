//! Common realtime execution layer for sample and physical voices.
//!
//! Voice slots, envelopes, gesture ramps, and physical state are all created
//! before rendering. Note-on and event processing only select/reset existing
//! state; the audio path never allocates a new voice or DSP buffer.

use solfege_core::{
    GestureControl, GestureState, PhysicalModel, RuntimeInstrument, SynthesisBackend,
};
use solfege_dsp::BowedString;
use solfege_modulation::GestureInterpolator;
use solfege_resampler::linear;
use solfege_zone::LoopMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeStage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvelopeConfig {
    pub attack_seconds: f32,
    pub decay_seconds: f32,
    pub sustain_level: f32,
    pub release_seconds: f32,
}

impl Default for EnvelopeConfig {
    fn default() -> Self {
        Self {
            attack_seconds: 0.01,
            decay_seconds: 0.1,
            sustain_level: 0.7,
            release_seconds: 0.2,
        }
    }
}

impl EnvelopeConfig {
    pub fn sanitized(self) -> Self {
        Self {
            attack_seconds: finite_nonnegative(self.attack_seconds),
            decay_seconds: finite_nonnegative(self.decay_seconds),
            sustain_level: if self.sustain_level.is_finite() {
                self.sustain_level.clamp(0.0, 1.0)
            } else {
                0.0
            },
            release_seconds: finite_nonnegative(self.release_seconds),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AdsrEnvelope {
    config: EnvelopeConfig,
    stage: EnvelopeStage,
    level: f32,
    release_delta: f32,
}

impl AdsrEnvelope {
    pub fn new(config: EnvelopeConfig) -> Self {
        Self {
            config: config.sanitized(),
            stage: EnvelopeStage::Idle,
            level: 0.0,
            release_delta: 0.0,
        }
    }

    pub fn trigger(&mut self, config: EnvelopeConfig) {
        self.config = config.sanitized();
        self.level = 0.0;
        self.stage = EnvelopeStage::Attack;
        self.release_delta = 0.0;
    }

    pub fn release(&mut self, sample_rate: f32) {
        if self.stage == EnvelopeStage::Idle || self.stage == EnvelopeStage::Release {
            return;
        }
        self.stage = EnvelopeStage::Release;
        self.release_delta = if self.config.release_seconds <= 0.0 {
            self.level
        } else {
            self.level / (self.config.release_seconds * sample_rate).max(1.0)
        };
    }

    #[inline]
    pub fn next(&mut self, sample_rate: f32) -> f32 {
        match self.stage {
            EnvelopeStage::Idle => self.level = 0.0,
            EnvelopeStage::Attack => {
                if self.config.attack_seconds <= 0.0 {
                    self.level = 1.0;
                    self.stage = EnvelopeStage::Decay;
                } else {
                    self.level += 1.0 / (self.config.attack_seconds * sample_rate).max(1.0);
                    if self.level >= 1.0 {
                        self.level = 1.0;
                        self.stage = EnvelopeStage::Decay;
                    }
                }
            }
            EnvelopeStage::Decay => {
                if self.config.decay_seconds <= 0.0 {
                    self.level = self.config.sustain_level;
                    self.stage = EnvelopeStage::Sustain;
                } else {
                    let delta = (1.0 - self.config.sustain_level)
                        / (self.config.decay_seconds * sample_rate).max(1.0);
                    self.level -= delta;
                    if self.level <= self.config.sustain_level {
                        self.level = self.config.sustain_level;
                        self.stage = EnvelopeStage::Sustain;
                    }
                }
            }
            EnvelopeStage::Sustain => self.level = self.config.sustain_level,
            EnvelopeStage::Release => {
                self.level -= self.release_delta;
                if self.level <= 0.0 || self.config.release_seconds <= 0.0 {
                    self.level = 0.0;
                    self.stage = EnvelopeStage::Idle;
                }
            }
        }
        self.level
    }

    pub const fn stage(&self) -> EnvelopeStage {
        self.stage
    }

    pub const fn level(&self) -> f32 {
        self.level
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceKind {
    Sample,
    Physical,
    Hybrid,
}

#[derive(Debug, Clone)]
struct Voice {
    active: bool,
    kind: VoiceKind,
    note: u8,
    note_id: i32,
    zone_index: usize,
    cursor: f64,
    pitch_ratio: f64,
    base_pitch_hz: f32,
    velocity_gain: f32,
    started_at: u64,
    envelope: AdsrEnvelope,
    released: bool,
    gesture: GestureInterpolator,
    physical: BowedString,
}

impl Voice {
    fn new(sample_rate: f32, physical_config: solfege_core::BowedStringConfig) -> Self {
        Self {
            active: false,
            kind: VoiceKind::Sample,
            note: 0,
            note_id: -1,
            zone_index: 0,
            cursor: 0.0,
            pitch_ratio: 1.0,
            base_pitch_hz: 440.0,
            velocity_gain: 0.0,
            started_at: 0,
            envelope: AdsrEnvelope::new(EnvelopeConfig::default()),
            released: false,
            gesture: GestureInterpolator::new(GestureState::default()),
            physical: BowedString::new(sample_rate, physical_config),
        }
    }

    fn reset(&mut self) {
        self.active = false;
        self.kind = VoiceKind::Sample;
        self.note = 0;
        self.note_id = -1;
        self.zone_index = 0;
        self.cursor = 0.0;
        self.pitch_ratio = 1.0;
        self.base_pitch_hz = 440.0;
        self.velocity_gain = 0.0;
        self.started_at = 0;
        self.envelope = AdsrEnvelope::new(EnvelopeConfig::default());
        self.released = false;
        self.gesture = GestureInterpolator::new(GestureState::default());
        self.physical.reset();
    }

    fn matches(&self, note_id: i32) -> bool {
        note_id < 0 || self.note_id == note_id
    }
}

pub struct VoicePool {
    voices: Vec<Voice>,
    serial: u64,
    peak_active: usize,
}

impl VoicePool {
    pub fn new(polyphony: usize) -> Self {
        Self::with_physical_config(
            polyphony,
            48_000.0,
            solfege_core::BowedStringConfig::default(),
        )
    }

    pub fn with_physical_config(
        polyphony: usize,
        sample_rate: f32,
        physical_config: solfege_core::BowedStringConfig,
    ) -> Self {
        let polyphony = polyphony.clamp(1, 1024);
        let voices = (0..polyphony)
            .map(|_| Voice::new(sample_rate, physical_config))
            .collect();
        Self {
            voices,
            serial: 0,
            peak_active: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.voices.len()
    }

    pub fn memory_bytes(&self) -> usize {
        self.voices.capacity() * std::mem::size_of::<Voice>()
            + self
                .voices
                .iter()
                .map(|voice| voice.physical.memory_bytes())
                .sum::<usize>()
    }

    pub fn active_count(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    pub const fn peak_active(&self) -> usize {
        self.peak_active
    }

    pub fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.reset();
        }
        self.peak_active = 0;
    }

    pub fn note_on(
        &mut self,
        note: u8,
        velocity: f32,
        note_id: i32,
        zone_index: usize,
        instrument: &RuntimeInstrument,
        envelope: EnvelopeConfig,
    ) {
        let backend = instrument.backend;
        let Some(slot) = self.select_slot() else {
            return;
        };
        self.serial = self.serial.wrapping_add(1);
        let voice = &mut self.voices[slot];
        voice.reset();
        voice.active = true;
        voice.note = note;
        voice.note_id = note_id;
        voice.zone_index = zone_index;
        voice.velocity_gain = finite_unit(velocity);
        voice.started_at = self.serial;
        voice.envelope.trigger(envelope);

        match backend {
            SynthesisBackend::Sample => {
                let Some(zone) = instrument.model.zones.get(zone_index) else {
                    voice.active = false;
                    return;
                };
                if instrument.samples.get(zone.sample_index).is_none() {
                    voice.active = false;
                    return;
                }
                let semitones = note as f64 - zone.root_key as f64
                    + zone.coarse_tune as f64
                    + zone.fine_tune_cents as f64 / 100.0;
                voice.kind = VoiceKind::Sample;
                voice.cursor = zone.start_frame as f64;
                voice.pitch_ratio = 2.0_f64.powf(semitones / 12.0);
            }
            SynthesisBackend::Physical(model) | SynthesisBackend::Hybrid(model) => {
                match model {
                    PhysicalModel::BowedString(_) => {}
                }
                voice.kind = if matches!(backend, SynthesisBackend::Hybrid(_)) {
                    VoiceKind::Hybrid
                } else {
                    VoiceKind::Physical
                };
                let gesture = GestureState::for_note(note, velocity);
                voice.base_pitch_hz = gesture.pitch_hz;
                voice.gesture = GestureInterpolator::new(gesture);
                voice.physical.note_on(gesture);
            }
        }
        self.peak_active = self.peak_active.max(self.active_count());
    }

    fn select_slot(&self) -> Option<usize> {
        self.voices
            .iter()
            .position(|voice| !voice.active)
            .or_else(|| {
                self.voices
                    .iter()
                    .enumerate()
                    .filter(|(_, voice)| voice.envelope.stage() == EnvelopeStage::Release)
                    .min_by_key(|(_, voice)| voice.started_at)
                    .or_else(|| {
                        self.voices
                            .iter()
                            .enumerate()
                            .min_by_key(|(_, voice)| voice.started_at)
                    })
                    .map(|(index, _)| index)
            })
    }

    pub fn note_off(&mut self, note: u8, note_id: i32, sample_rate: f32) {
        for voice in &mut self.voices {
            if voice.active && voice.note == note && voice.matches(note_id) && !voice.released {
                voice.released = true;
                voice.envelope.release(sample_rate);
                if matches!(voice.kind, VoiceKind::Physical | VoiceKind::Hybrid) {
                    voice.physical.note_off();
                }
            }
        }
    }

    pub fn all_notes_off(&mut self, sample_rate: f32) {
        for voice in &mut self.voices {
            if voice.active && !voice.released {
                voice.released = true;
                voice.envelope.release(sample_rate);
                if matches!(voice.kind, VoiceKind::Physical | VoiceKind::Hybrid) {
                    voice.physical.note_off();
                }
            }
        }
    }

    pub fn set_gesture(
        &mut self,
        note_id: i32,
        control: GestureControl,
        value: f32,
        ramp_samples: u32,
    ) {
        for voice in &mut self.voices {
            if voice.active
                && voice.matches(note_id)
                && matches!(voice.kind, VoiceKind::Physical | VoiceKind::Hybrid)
            {
                let mut target = voice.gesture.current();
                target.set(control, value);
                voice.gesture.set_target(target, ramp_samples);
            }
        }
    }

    pub fn set_pitch(&mut self, note_id: i32, hz: f32, ramp_samples: u32) {
        for voice in &mut self.voices {
            if voice.active
                && voice.matches(note_id)
                && matches!(voice.kind, VoiceKind::Physical | VoiceKind::Hybrid)
            {
                let mut target = voice.gesture.current();
                target.pitch_hz = hz;
                voice.gesture.set_target(target, ramp_samples);
            }
        }
    }

    pub fn set_pitch_bend(&mut self, value: f32, semitone_range: f32, ramp_samples: u32) {
        let ratio = 2.0_f32.powf(value.clamp(-1.0, 1.0) * semitone_range / 12.0);
        for voice in &mut self.voices {
            if voice.active && matches!(voice.kind, VoiceKind::Physical | VoiceKind::Hybrid) {
                let mut target = voice.gesture.current();
                target.pitch_hz = voice.base_pitch_hz * ratio;
                voice.gesture.set_target(target, ramp_samples);
            }
        }
    }

    #[inline]
    pub fn render_frame(
        &mut self,
        output: &mut [f32],
        channels: usize,
        sample_rate: f32,
        instrument: &RuntimeInstrument,
    ) {
        for voice in &mut self.voices {
            if !voice.active {
                continue;
            }
            let envelope = voice.envelope.next(sample_rate);
            if voice.envelope.stage() == EnvelopeStage::Idle {
                voice.active = false;
                continue;
            }
            match voice.kind {
                VoiceKind::Sample => {
                    render_sample_voice(voice, output, channels, envelope, sample_rate, instrument)
                }
                VoiceKind::Physical | VoiceKind::Hybrid => {
                    let gesture = voice.gesture.advance();
                    let value = finite_or_zero(voice.physical.process(gesture) * envelope);
                    if channels == 1 {
                        output[0] += value;
                    } else if channels >= 2 {
                        let stereo = value * 0.707_106_77;
                        output[0] += stereo;
                        output[1] += stereo;
                    }
                    if voice.physical.is_quiet() && voice.envelope.stage() == EnvelopeStage::Release
                    {
                        voice.active = false;
                    }
                }
            }
        }
    }
}

fn render_sample_voice(
    voice: &mut Voice,
    output: &mut [f32],
    channels: usize,
    envelope: f32,
    sample_rate: f32,
    instrument: &RuntimeInstrument,
) {
    let Some(zone) = instrument.model.zones.get(voice.zone_index) else {
        voice.active = false;
        return;
    };
    let Some(sample) = instrument.samples.get(zone.sample_index) else {
        voice.active = false;
        return;
    };
    let end = zone.end_frame.unwrap_or(sample.layout().frames);
    if voice.cursor >= end as f64 || end == 0 {
        voice.active = false;
        return;
    }
    let base = voice.cursor.floor() as u64;
    let fraction = (voice.cursor - base as f64) as f32;
    let should_loop = match zone.loop_mode {
        LoopMode::Forward => true,
        LoopMode::Sustain | LoopMode::ReleaseAware => !voice.released,
        LoopMode::None => false,
    } && zone.loop_end > zone.loop_start;
    let next = if should_loop && base + 1 >= zone.loop_end {
        zone.loop_start
    } else {
        (base + 1).min(end.saturating_sub(1))
    };
    let left = linear(
        sample.sample_f32(base, 0),
        sample.sample_f32(next, 0),
        fraction,
    );
    let right_channel = if sample.layout().channels > 1 { 1 } else { 0 };
    let right = linear(
        sample.sample_f32(base, right_channel),
        sample.sample_f32(next, right_channel),
        fraction,
    );
    let gain = zone.gain * voice.velocity_gain * envelope;
    let pan = zone.pan.clamp(-1.0, 1.0);
    let left_gain = ((1.0 - pan) * 0.5).sqrt();
    let right_gain = ((1.0 + pan) * 0.5).sqrt();
    if channels == 1 {
        output[0] += (left + right) * 0.5 * gain;
    } else if channels >= 2 {
        output[0] += left * gain * left_gain;
        output[1] += right * gain * right_gain;
    }

    let rate_ratio = sample.layout().sample_rate as f64 / sample_rate as f64;
    voice.cursor += voice.pitch_ratio * rate_ratio;
    if should_loop && voice.cursor >= zone.loop_end as f64 {
        let loop_len = (zone.loop_end - zone.loop_start) as f64;
        voice.cursor =
            zone.loop_start as f64 + (voice.cursor - zone.loop_start as f64).rem_euclid(loop_len);
    } else if voice.cursor >= end as f64 {
        voice.active = false;
    }
}

fn finite_nonnegative(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[inline]
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adsr_reaches_sustain_and_releases() {
        let config = EnvelopeConfig {
            attack_seconds: 0.01,
            decay_seconds: 0.01,
            sustain_level: 0.5,
            release_seconds: 0.01,
        };
        let mut envelope = AdsrEnvelope::new(config);
        envelope.trigger(config);
        for _ in 0..20 {
            envelope.next(1_000.0);
        }
        assert_eq!(envelope.stage(), EnvelopeStage::Sustain);
        assert!((envelope.level() - 0.5).abs() < 1e-5);
        envelope.release(1_000.0);
        for _ in 0..10 {
            envelope.next(1_000.0);
        }
        assert_eq!(envelope.stage(), EnvelopeStage::Idle);
    }

    #[test]
    fn voice_capacity_is_preallocated_and_bounded() {
        let pool = VoicePool::new(128);
        assert_eq!(pool.capacity(), 128);
        assert_eq!(pool.active_count(), 0);
    }
}
