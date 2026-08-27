//! Callback-safe playback of the measured VSCO-derived SFM acoustic assets.
//!
//! `AcousticRenderer::new` is a control-thread operation. It allocates the
//! bounded voice state and takes ownership of the parsed ACOU/AUDO data. Once
//! prepared, note/control handling and `render_frame` perform no file I/O,
//! locking, or heap allocation.

#![forbid(unsafe_code)]

use solfege_model::acoustic::{
    AcousticModel, AcousticProfile, SEGMENT_ATTACK, SEGMENT_BODY, SEGMENT_BOW_NOISE,
    SEGMENT_RELEASE,
};

const MAX_SAMPLE_STEP: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceStage {
    Attack,
    Body,
    Release,
}

#[derive(Debug, Clone, Copy)]
struct AcousticVoice {
    active: bool,
    note: u8,
    note_id: i32,
    velocity: f32,
    profile_index: usize,
    attack_segment: usize,
    body_segment: usize,
    noise_segment: usize,
    release_segment: usize,
    pitch_step: f32,
    attack_pos: f32,
    body_pos: f32,
    noise_pos: f32,
    release_pos: f32,
    stage: VoiceStage,
}

impl Default for AcousticVoice {
    fn default() -> Self {
        Self {
            active: false,
            note: 0,
            note_id: -1,
            velocity: 0.0,
            profile_index: 0,
            attack_segment: 0,
            body_segment: 0,
            noise_segment: 0,
            release_segment: 0,
            pitch_step: 1.0,
            attack_pos: 0.0,
            body_pos: 0.0,
            noise_pos: 0.0,
            release_pos: 0.0,
            stage: VoiceStage::Attack,
        }
    }
}

/// A measured-acoustic renderer prepared from paired SFM ACOU/AUDO sections.
pub struct AcousticRenderer {
    model: AcousticModel,
    voices: Vec<AcousticVoice>,
    sample_rate: f32,
    articulation: u8,
    expression: f32,
    sustain: bool,
}

impl AcousticRenderer {
    pub fn new(model: AcousticModel, sample_rate: u32, polyphony: usize) -> Self {
        let sample_rate = sample_rate.max(1) as f32;
        let voices = vec![AcousticVoice::default(); polyphony.max(1)];
        Self {
            model,
            voices,
            sample_rate,
            articulation: 0,
            expression: 1.0,
            sustain: false,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.model.sample_rate()
    }

    pub fn source_file_count(&self) -> u32 {
        self.model.source_file_count()
    }

    pub fn source_frame_count(&self) -> u64 {
        self.model.source_frame_count()
    }

    pub fn segment_count(&self) -> usize {
        self.model.segments().len()
    }

    pub fn profile_count(&self) -> usize {
        self.model.profiles().len()
    }

    pub fn memory_bytes(&self) -> usize {
        self.model.samples().len() * std::mem::size_of::<i16>()
            + self.model.profiles().len() * std::mem::size_of::<AcousticProfile>()
            + self.model.segments().len()
                * std::mem::size_of::<solfege_model::acoustic::AudioSegment>()
            + self.voices.len() * std::mem::size_of::<AcousticVoice>()
    }

    pub fn active_voices(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active).count()
    }

    pub fn set_articulation(&mut self, articulation: u8) {
        self.articulation = articulation.min(3);
    }

    pub fn set_expression(&mut self, expression: f32) {
        self.expression = if expression.is_finite() {
            expression.clamp(0.0, 2.0)
        } else {
            1.0
        };
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, note_id: i32) {
        let velocity = if velocity.is_finite() {
            velocity.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let dynamic = dynamic_for_velocity(velocity);
        let profile_index = self.model.profile_index(self.articulation, dynamic);
        let Some(profile) = self.model.profiles().get(profile_index) else {
            return;
        };
        let source_note = profile.midi_note_mean;
        let pitch_ratio = if source_note.is_finite() {
            2.0_f32.powf((note as f32 - source_note) / 12.0)
        } else {
            1.0
        };
        let source_rate_ratio = self.model.sample_rate() as f32 / self.sample_rate;
        let pitch_step = (pitch_ratio * source_rate_ratio).clamp(0.01, MAX_SAMPLE_STEP);
        let voice_index = self
            .voices
            .iter()
            .position(|voice| !voice.active)
            .or_else(|| {
                self.voices
                    .iter()
                    .position(|voice| voice.stage == VoiceStage::Release)
            })
            .unwrap_or(0);
        let key = (profile.articulation, profile.dynamic);
        let segment_for = |kind| {
            self.model
                .segment_index(profile_index, kind)
                .or_else(|| {
                    self.model.segments().iter().position(|segment| {
                        segment.kind == kind
                            && segment.articulation == key.0
                            && segment.dynamic == key.1
                    })
                })
                .unwrap_or(0)
        };
        self.voices[voice_index] = AcousticVoice {
            active: true,
            note,
            note_id,
            velocity,
            profile_index,
            attack_segment: segment_for(SEGMENT_ATTACK),
            body_segment: segment_for(SEGMENT_BODY),
            noise_segment: segment_for(SEGMENT_BOW_NOISE),
            release_segment: segment_for(SEGMENT_RELEASE),
            pitch_step,
            attack_pos: 0.0,
            body_pos: 0.0,
            noise_pos: 0.0,
            release_pos: 0.0,
            stage: VoiceStage::Attack,
        };
    }

    pub fn note_off(&mut self, note: u8, note_id: i32) {
        for voice in &mut self.voices {
            if voice.active && voice.note == note && (note_id < 0 || voice.note_id == note_id) {
                if !self.sustain {
                    voice.stage = VoiceStage::Release;
                    voice.release_pos = 0.0;
                }
            }
        }
    }

    pub fn control_change(&mut self, controller: u8, value: f32) {
        let value = if value.is_finite() {
            value.clamp(0.0, 1.0)
        } else {
            0.0
        };
        match controller {
            11 => self.set_expression(value * 2.0),
            64 => self.set_sustain(value >= 0.5),
            _ => {}
        }
    }

    pub fn set_sustain(&mut self, enabled: bool) {
        if self.sustain && !enabled {
            for voice in &mut self.voices {
                if voice.active {
                    voice.stage = VoiceStage::Release;
                    voice.release_pos = 0.0;
                }
            }
        }
        self.sustain = enabled;
    }

    pub fn all_notes_off(&mut self) {
        for voice in &mut self.voices {
            if voice.active {
                voice.stage = VoiceStage::Release;
                voice.release_pos = 0.0;
            }
        }
    }

    pub fn reset(&mut self) {
        for voice in &mut self.voices {
            *voice = AcousticVoice::default();
        }
        self.sustain = false;
        self.expression = 1.0;
    }

    /// Render one mono frame from the embedded measured assets.
    pub fn render_frame(&mut self) -> f32 {
        let mut output = 0.0_f32;
        for index in 0..self.voices.len() {
            let sample = Self::render_voice(&self.model, &mut self.voices[index]);
            output += sample * self.expression;
        }
        output.clamp(-1.0, 1.0)
    }

    fn render_voice(model: &AcousticModel, voice: &mut AcousticVoice) -> f32 {
        if !voice.active {
            return 0.0;
        }
        let Some(profile) = model.profiles().get(voice.profile_index) else {
            voice.active = false;
            return 0.0;
        };
        match voice.stage {
            VoiceStage::Release => {
                let sample = read_interpolated(model, voice.release_segment, voice.release_pos);
                voice.release_pos += voice.pitch_step;
                let segment_len = model
                    .segments()
                    .get(voice.release_segment)
                    .map_or(0.0, |segment| segment.frame_count as f32);
                if voice.release_pos >= segment_len {
                    voice.active = false;
                }
                let gain = (0.55 + 0.45 * profile.release_ms.clamp(0.0, 1_500.0) / 1_500.0)
                    * voice.velocity;
                sample * gain
            }
            VoiceStage::Attack => {
                let attack = read_interpolated(model, voice.attack_segment, voice.attack_pos);
                let noise = read_interpolated(model, voice.noise_segment, voice.noise_pos);
                voice.attack_pos += voice.pitch_step;
                voice.noise_pos += voice.pitch_step;
                let attack_len = model
                    .segments()
                    .get(voice.attack_segment)
                    .map_or(0.0, |segment| segment.frame_count as f32);
                if voice.attack_pos >= attack_len {
                    voice.stage = VoiceStage::Body;
                }
                let noise_gain = profile.noise_ratio.clamp(0.0, 2.0);
                let harmonic_gain = profile.harmonics[0].clamp(0.0, 1.5);
                (attack * (0.75 + 0.25 * harmonic_gain) + noise * noise_gain * 0.35)
                    * voice.velocity
            }
            VoiceStage::Body => {
                let body = read_interpolated(model, voice.body_segment, voice.body_pos);
                voice.body_pos += voice.pitch_step;
                let body_len = model
                    .segments()
                    .get(voice.body_segment)
                    .map_or(0.0, |segment| segment.frame_count as f32);
                if voice.body_pos >= body_len {
                    voice.body_pos = 0.0;
                }
                let mode_gain = profile
                    .body_modes
                    .iter()
                    .map(|mode| mode.gain)
                    .sum::<f32>()
                    .clamp(0.05, 3.0)
                    / profile.body_modes.len() as f32;
                let spectral_gain = (profile.spectral_centroid_hz / 2_500.0).clamp(0.2, 1.5);
                body * (0.5 + mode_gain + 0.15 * spectral_gain) * voice.velocity
            }
        }
    }
}

fn dynamic_for_velocity(velocity: f32) -> u8 {
    if velocity < 0.35 {
        0
    } else if velocity > 0.75 {
        1
    } else if velocity < 0.55 {
        2
    } else {
        3
    }
}

fn read_interpolated(model: &AcousticModel, segment_index: usize, position: f32) -> f32 {
    let Some(samples) = model.segment_samples(segment_index) else {
        return 0.0;
    };
    if samples.is_empty() {
        return 0.0;
    }
    let position = position.clamp(0.0, samples.len().saturating_sub(1) as f32);
    let index = position.floor() as usize;
    let next = (index + 1).min(samples.len() - 1);
    let fraction = position - index as f32;
    let left = samples[index] as f32 / i16::MAX as f32;
    let right = samples[next] as f32 / i16::MAX as f32;
    left + (right - left) * fraction
}

#[cfg(test)]
mod tests {
    use super::*;
    use solfege_model::acoustic::{
        ACOUSTIC_BODY_MODES, ACOUSTIC_HARMONIC_BINS, ACOUSTIC_HEADER_SIZE, ACOUSTIC_PROFILE_SIZE,
        AUDIO_DESCRIPTOR_SIZE, AUDIO_HEADER_SIZE,
    };

    fn fixture() -> (Vec<u8>, Vec<u8>) {
        let mut acou = vec![0_u8; ACOUSTIC_HEADER_SIZE + ACOUSTIC_PROFILE_SIZE];
        acou[0..4].copy_from_slice(b"ACU1");
        acou[4..6].copy_from_slice(&1_u16.to_le_bytes());
        acou[6..8].copy_from_slice(&(ACOUSTIC_HEADER_SIZE as u16).to_le_bytes());
        acou[8..12].copy_from_slice(&48_000_u32.to_le_bytes());
        acou[12..16].copy_from_slice(&1_u32.to_le_bytes());
        acou[16..20].copy_from_slice(&(ACOUSTIC_HARMONIC_BINS as u32).to_le_bytes());
        acou[20..24].copy_from_slice(&(ACOUSTIC_BODY_MODES as u32).to_le_bytes());
        acou[24..28].copy_from_slice(&1_u32.to_le_bytes());
        acou[32..40].copy_from_slice(&32_u64.to_le_bytes());
        acou[40..44].copy_from_slice(&(ACOUSTIC_PROFILE_SIZE as u32).to_le_bytes());
        acou[64] = 0;
        acou[65] = 0;
        for offset in (4..396).step_by(4) {
            acou[64 + offset..68 + offset].copy_from_slice(&1.0_f32.to_le_bytes());
        }
        let frames = 32_u32;
        let mut audio =
            vec![0_u8; AUDIO_HEADER_SIZE + AUDIO_DESCRIPTOR_SIZE * 4 + frames as usize * 2 * 4];
        audio[0..4].copy_from_slice(b"AUO1");
        audio[4..6].copy_from_slice(&1_u16.to_le_bytes());
        audio[6..8].copy_from_slice(&(AUDIO_HEADER_SIZE as u16).to_le_bytes());
        audio[8..12].copy_from_slice(&48_000_u32.to_le_bytes());
        audio[12..14].copy_from_slice(&1_u16.to_le_bytes());
        audio[16..20].copy_from_slice(&4_u32.to_le_bytes());
        audio[20..24].copy_from_slice(&1_u32.to_le_bytes());
        audio[24..32].copy_from_slice(&32_u64.to_le_bytes());
        audio[32..36].copy_from_slice(&(AUDIO_DESCRIPTOR_SIZE as u32).to_le_bytes());
        audio[36..44].copy_from_slice(&(256_u64).to_le_bytes());
        audio[44..52].copy_from_slice(&(frames as u64 * 2 * 4).to_le_bytes());
        for index in 0..4 {
            let start = AUDIO_HEADER_SIZE + index * AUDIO_DESCRIPTOR_SIZE;
            audio[start] = index as u8;
            audio[start + 4..start + 6].copy_from_slice(&60_i16.to_le_bytes());
            audio[start + 20..start + 24].copy_from_slice(&frames.to_le_bytes());
            audio[start + 24..start + 32]
                .copy_from_slice(&(index as u64 * frames as u64).to_le_bytes());
            audio[start + 32..start + 36].copy_from_slice(&48_000_u32.to_le_bytes());
            audio[start + 36..start + 48].copy_from_slice(&1.0_f32.to_le_bytes().repeat(3));
        }
        for sample in audio[256..].chunks_exact_mut(2) {
            sample.copy_from_slice(&8_000_i16.to_le_bytes());
        }
        (acou, audio)
    }

    #[test]
    fn measured_assets_produce_audio_and_reset() {
        let (acou, audio) = fixture();
        let model = AcousticModel::from_sections(&acou, &audio).unwrap();
        let mut renderer = AcousticRenderer::new(model, 48_000, 4);
        renderer.note_on(60, 0.8, 1);
        let mut sum = 0.0;
        for _ in 0..64 {
            sum += renderer.render_frame().abs();
        }
        assert!(sum > 0.0);
        renderer.reset();
        assert_eq!(renderer.active_voices(), 0);
    }
}
