//! Callback-safe playback of the embedded Solfage neural voicebank.
//!
//! `VoicebankRenderer::new` is control-thread preparation. It takes ownership
//! of the verified `INDX`/`AUDO` model and allocates only bounded voice state.
//! Note/control handling and `render_frame` then use prepared memory: there is
//! no filesystem access, decompression, locking, or heap allocation in the
//! audio path.

#![forbid(unsafe_code)]

use std::sync::Arc;

use solfege_model::voicebank::{ARTICULATION_SUSTAIN_VIBRATO, VoicebankEntry, VoicebankModel};

const MAX_SAMPLE_STEP: f32 = 8.0;
const ATTACK_FADE_SECONDS: f32 = 0.005;
const RELEASE_FADE_SECONDS: f32 = 0.02;
/// How long a gain change takes to arrive, in seconds.
///
/// Dynamics and expression arrive as controller lanes: a stream of discrete
/// points, each a step change in level. Applying a step directly to a running
/// gain is a discontinuity in the waveform at every point — zipper noise, heard
/// as a rasp or a click on any moving fader. Ten milliseconds is short enough
/// that a fast crescendo still reads as fast and long enough that the step
/// spreads over ~480 samples at 48 kHz, well below audibility.
const GAIN_GLIDE_SECONDS: f32 = 0.01;
/// How long the read position takes to move to a new playback rate.
///
/// A drawn pitch curve reaches the engine as a stream of targets a few
/// milliseconds apart. Writing each one straight into the read step made the
/// rate a staircase, and a staircase in playback rate is a staircase in pitch:
/// audible as a warble riding on the drawn line. Three milliseconds is short
/// enough that the curve still arrives on time — the glide is linear and
/// *reaches* its target rather than approaching it asymptotically, so a held
/// target is exact, not merely close.
const PITCH_GLIDE_SECONDS: f32 = 0.003;
/// Crossfade applied when the read position is spliced into the release
/// region at note-off.
///
/// The jump is from wherever the sustain loop had reached to the start of the
/// recorded release, two uncorrelated points in the waveform, so splicing them
/// directly puts a step discontinuity in the output on *every* note-off. Five
/// milliseconds is long enough to hide the seam and short enough not to smear
/// the release transient.
const RELEASE_SPLICE_SECONDS: f32 = 0.005;

#[derive(Debug, Clone, Copy)]
struct VoicebankVoice {
    active: bool,
    note: u8,
    note_id: i32,
    /// Where the voice's dynamic level is heading, and where it is now. Split
    /// so a controller lane can move the target every few milliseconds without
    /// stepping the gain the audio is actually multiplied by.
    velocity: f32,
    velocity_current: f32,
    primary_entry: Option<VoicebankEntry>,
    secondary_entry: Option<VoicebankEntry>,
    /// Read positions and steps are `f64` because a recorded sustain runs to
    /// ~720_000 frames, where an `f32` position quantises the interpolation
    /// fraction to 16 values and turns a held note into distortion. See
    /// `solfege_model::voicebank::sample_entry`.
    primary_cursor: f64,
    secondary_cursor: f64,
    primary_step: f64,
    secondary_step: f64,
    /// Where the read step is heading, and how fast it gets there. See
    /// [`PITCH_GLIDE_SECONDS`].
    primary_step_target: f64,
    secondary_step_target: f64,
    primary_step_delta: f64,
    secondary_step_delta: f64,
    /// The read position abandoned by the note-off splice, still advancing, and
    /// how many frames of crossfade it has left. See
    /// [`RELEASE_SPLICE_SECONDS`].
    splice_cursor: f64,
    splice_entry: Option<VoicebankEntry>,
    splice_remaining: u32,
    splice_length: u32,
    primary_gain: f32,
    secondary_gain: f32,
    target_pitch_hz: f32,
    base_pitch_hz: f32,
    attack_gain: f32,
    release_gain: f32,
    released: bool,
}

impl Default for VoicebankVoice {
    fn default() -> Self {
        Self {
            active: false,
            note: 0,
            note_id: -1,
            velocity: 0.0,
            velocity_current: 0.0,
            primary_entry: None,
            secondary_entry: None,
            primary_cursor: 0.0,
            secondary_cursor: 0.0,
            primary_step: 1.0,
            secondary_step: 1.0,
            primary_step_target: 1.0,
            secondary_step_target: 1.0,
            primary_step_delta: 0.0,
            secondary_step_delta: 0.0,
            splice_cursor: 0.0,
            splice_entry: None,
            splice_remaining: 0,
            splice_length: 0,
            primary_gain: 1.0,
            secondary_gain: 0.0,
            target_pitch_hz: 440.0,
            base_pitch_hz: 440.0,
            attack_gain: 0.0,
            release_gain: 1.0,
            released: false,
        }
    }
}

/// A prepared, indexed source voicebank renderer.
pub struct VoicebankRenderer {
    model: Arc<VoicebankModel>,
    voices: Vec<VoicebankVoice>,
    active_slots: Vec<usize>,
    sample_rate: f32,
    attack_step: f32,
    release_step: f32,
    /// Per-sample increment used to approach a new gain. See
    /// [`GAIN_GLIDE_SECONDS`].
    gain_step: f32,
    /// Frames a read step takes to reach a new target. See
    /// [`PITCH_GLIDE_SECONDS`].
    pitch_glide_frames: f64,
    /// Frames of crossfade over the note-off read-position splice. See
    /// [`RELEASE_SPLICE_SECONDS`].
    release_splice_frames: u32,
    articulation: u8,
    expression: f32,
    expression_current: f32,
    sustain: bool,
    held: [bool; 128],
    deferred_release: [bool; 128],
    round_robin: u16,
}

impl VoicebankRenderer {
    pub fn new(model: VoicebankModel, sample_rate: u32, polyphony: usize) -> Self {
        let sample_rate = sample_rate.max(1) as f32;
        Self {
            model: Arc::new(model),
            voices: vec![VoicebankVoice::default(); polyphony.max(1)],
            active_slots: Vec::with_capacity(polyphony.max(1)),
            sample_rate,
            attack_step: 1.0 / (ATTACK_FADE_SECONDS * sample_rate).max(1.0),
            release_step: 1.0 / (RELEASE_FADE_SECONDS * sample_rate).max(1.0),
            gain_step: 1.0 / (GAIN_GLIDE_SECONDS * sample_rate).max(1.0),
            pitch_glide_frames: (PITCH_GLIDE_SECONDS * sample_rate).max(1.0) as f64,
            release_splice_frames: (RELEASE_SPLICE_SECONDS * sample_rate).max(1.0) as u32,
            articulation: ARTICULATION_SUSTAIN_VIBRATO,
            expression: 1.0,
            expression_current: 1.0,
            sustain: false,
            held: [false; 128],
            deferred_release: [false; 128],
            round_robin: 0,
        }
    }

    /// Clone the realtime state while sharing the immutable indexed audio bank.
    /// Runtime graph mirrors use this when a project edit is published; copying
    /// the bank itself would otherwise duplicate tens or hundreds of megabytes.
    pub fn clone_prepared(&self) -> Self {
        Self {
            model: Arc::clone(&self.model),
            voices: vec![VoicebankVoice::default(); self.voices.len()],
            active_slots: Vec::with_capacity(self.voices.len()),
            sample_rate: self.sample_rate,
            attack_step: self.attack_step,
            release_step: self.release_step,
            gain_step: self.gain_step,
            pitch_glide_frames: self.pitch_glide_frames,
            release_splice_frames: self.release_splice_frames,
            articulation: self.articulation,
            expression: self.expression,
            expression_current: self.expression,
            sustain: false,
            held: [false; 128],
            deferred_release: [false; 128],
            round_robin: self.round_robin,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.model.sample_rate()
    }

    pub fn channels(&self) -> u16 {
        self.model.channels()
    }

    pub fn entry_count(&self) -> usize {
        self.model.entries().len()
    }

    pub fn audio_bytes(&self) -> usize {
        self.model.audio_bytes()
    }

    pub fn source_file_count(&self) -> u32 {
        self.model.source_file_count()
    }

    pub fn source_frame_count(&self) -> u64 {
        self.model.source_frame_count()
    }

    pub fn decoded_frame_count(&self) -> u64 {
        self.model.decoded_frame_count()
    }

    pub fn decoded_duration_seconds(&self) -> f64 {
        self.model.decoded_duration_seconds()
    }

    pub fn memory_bytes(&self) -> usize {
        self.model.audio_bytes()
            + self.model.entries().len() * std::mem::size_of::<VoicebankEntry>()
            + self.voices.len() * std::mem::size_of::<VoicebankVoice>()
            + self.active_slots.capacity() * std::mem::size_of::<usize>()
    }

    pub fn active_voices(&self) -> usize {
        self.active_slots.len()
    }

    pub fn set_articulation(&mut self, articulation: u8) {
        self.articulation = articulation.min(7);
    }

    pub fn set_expression(&mut self, expression: f32) {
        self.expression = finite_or(expression, 1.0).clamp(0.0, 2.0);
    }

    pub fn set_dynamic(&mut self, note_id: i32, value: f32) {
        let value = finite_or(value, 0.5).clamp(0.0, 1.0);
        for slot in self.active_slots.iter().copied() {
            let voice = &mut self.voices[slot];
            if voice.active && matches_note_id(voice.note_id, note_id) {
                // The source pair is selected on attack. Continuous
                // expression remains smooth and does not tear a transient.
                voice.velocity = value.max(0.05);
            }
        }
    }

    pub fn note_on(&mut self, note: u8, velocity: f32, note_id: i32) {
        let note = note.min(127);
        let velocity = finite_or(velocity, 0.0).clamp(0.0, 1.0);
        let Some(selection) =
            self.model
                .resolve(note.min(127), self.articulation, velocity, self.round_robin)
        else {
            return;
        };
        self.held[note as usize] = true;
        self.deferred_release[note as usize] = false;
        self.round_robin = self.round_robin.wrapping_add(1);
        self.start_voice(
            note,
            velocity,
            note_id,
            selection.primary,
            selection.secondary,
            selection.secondary_mix,
        );
    }

    /// Start one exact indexed source entry at that entry's own pitch. Used by
    /// offline pair generation and source-replacement verification; normal
    /// performance input should use [`Self::note_on`] so pitch/dynamic/
    /// round-robin lookup remains in the realtime path.
    pub fn note_on_entry(&mut self, entry_index: usize, velocity: f32, note_id: i32) {
        let Some(entry) = self.model.entries().get(entry_index).copied() else {
            return;
        };
        self.note_on_entry_at(entry_index, entry.midi_note, velocity, note_id);
    }

    /// Start one exact indexed source entry, **transposed to `note`**.
    ///
    /// This is the case the runtime is in whenever the requested pitch is not
    /// one of the recorded ones — the resolver picks a neighbour and the
    /// resampler shifts it. Being able to reproduce it deterministically is
    /// what makes it possible to build training pairs for the correction that
    /// transposition actually needs, and to measure that correction against the
    /// real recording of the target pitch.
    pub fn note_on_entry_at(&mut self, entry_index: usize, note: u8, velocity: f32, note_id: i32) {
        if self.model.entries().get(entry_index).is_none() {
            return;
        }
        let note = note.min(127);
        self.held[note as usize] = true;
        self.deferred_release[note as usize] = false;
        self.start_voice(note, velocity, note_id, entry_index, None, 0.0);
    }

    fn start_voice(
        &mut self,
        note: u8,
        velocity: f32,
        note_id: i32,
        primary: usize,
        secondary: Option<usize>,
        secondary_mix: f32,
    ) {
        let slot = self
            .voices
            .iter()
            .position(|voice| !voice.active)
            .or_else(|| self.voices.iter().position(|voice| voice.released))
            .unwrap_or(0);
        let Some(primary_entry) = self.model.entries().get(primary).copied() else {
            return;
        };
        let secondary_entry = secondary.and_then(|index| self.model.entries().get(index).copied());
        let target_pitch_hz = midi_to_hz(note);
        let primary_step = pitch_step_for(&primary_entry, target_pitch_hz, self.sample_rate);
        let secondary_step = secondary_entry.map_or(primary_step, |entry| {
            pitch_step_for(&entry, target_pitch_hz, self.sample_rate)
        });
        let mix = secondary_mix.clamp(0.0, 1.0);
        let was_active = self.voices[slot].active;
        self.voices[slot] = VoicebankVoice {
            active: true,
            note,
            note_id,
            velocity: velocity.max(0.05),
            // A new voice starts *at* its level: the attack ramp already
            // covers the onset, and gliding up from zero on top of it would
            // soften every transient.
            velocity_current: velocity.max(0.05),
            primary_entry: Some(primary_entry),
            secondary_entry,
            primary_cursor: 0.0,
            secondary_cursor: 0.0,
            primary_step,
            secondary_step,
            // A new note starts *at* its rate; there is nothing to glide from.
            primary_step_target: primary_step,
            secondary_step_target: secondary_step,
            primary_step_delta: 0.0,
            secondary_step_delta: 0.0,
            splice_cursor: 0.0,
            splice_entry: None,
            splice_remaining: 0,
            splice_length: 0,
            primary_gain: (1.0 - mix).sqrt(),
            secondary_gain: mix.sqrt(),
            target_pitch_hz,
            base_pitch_hz: target_pitch_hz,
            attack_gain: 0.0,
            release_gain: 1.0,
            released: false,
        };
        if !was_active {
            self.active_slots.push(slot);
        }
    }

    pub fn note_off(&mut self, note: u8, note_id: i32) {
        let note = note.min(127);
        self.held[note as usize] = false;
        if self.sustain {
            self.deferred_release[note as usize] = true;
            return;
        }
        self.release_matching(note, note_id);
    }

    fn release_matching(&mut self, note: u8, note_id: i32) {
        let splice_frames = self.release_splice_frames;
        for slot in self.active_slots.iter().copied() {
            let voice = &mut self.voices[slot];
            if voice.active && voice.note == note && matches_note_id(voice.note_id, note_id) {
                begin_release(voice, splice_frames);
            }
        }
    }

    pub fn set_pitch(&mut self, note_id: i32, hz: f32) {
        let hz = finite_or(hz, 0.0).max(1.0);
        let glide_frames = self.pitch_glide_frames;
        let sample_rate = self.sample_rate;
        for slot in self.active_slots.iter().copied() {
            let voice = &mut self.voices[slot];
            if voice.active && matches_note_id(voice.note_id, note_id) {
                voice.target_pitch_hz = hz;
                aim_step(voice, hz, sample_rate, glide_frames);
            }
        }
    }

    pub fn set_pitch_bend(&mut self, value: f32, semitone_range: f32) {
        let ratio = 2.0_f32.powf(finite_or(value, 0.0).clamp(-1.0, 1.0) * semitone_range / 12.0);
        let glide_frames = self.pitch_glide_frames;
        let sample_rate = self.sample_rate;
        for slot in self.active_slots.iter().copied() {
            let voice = &mut self.voices[slot];
            if voice.active {
                let hz = voice.base_pitch_hz * ratio;
                voice.target_pitch_hz = hz;
                aim_step(voice, hz, sample_rate, glide_frames);
            }
        }
    }

    /// The continuous controls a sampled voicebank can actually honour.
    ///
    /// Deliberately short. A voicebank is recorded sound, not a model of an
    /// instrument: there is no bow to press and no embouchure to tighten, so a
    /// controller for one of those has nothing here to move. Accepting such a
    /// controller silently — writing project data that changes no sound — is
    /// worse than not offering it, so the host's lane table is kept in step
    /// with this list rather than the other way round.
    ///
    /// * **CC 1** — dynamic. The convention in orchestral libraries, and here
    ///   it moves the same per-voice dynamic that `Event::Expression` sets, so
    ///   a "Dynamics" lane in the editor means dynamics.
    /// * **CC 11** — expression, a level trim over the whole instrument.
    /// * **CC 64** — sustain pedal.
    pub fn control_change(&mut self, controller: u8, value: f32) {
        let value = finite_or(value, 0.0).clamp(0.0, 1.0);
        match controller {
            1 => self.set_dynamic(-1, value),
            11 => self.set_expression(value * 2.0),
            64 => self.set_sustain(value >= 0.5),
            _ => {}
        }
    }

    pub fn set_sustain(&mut self, enabled: bool) {
        if self.sustain && !enabled {
            for note in 0_u8..=127 {
                if self.deferred_release[note as usize] && !self.held[note as usize] {
                    self.release_matching(note, -1);
                    self.deferred_release[note as usize] = false;
                }
            }
        }
        self.sustain = enabled;
    }

    pub fn all_notes_off(&mut self) {
        let splice_frames = self.release_splice_frames;
        for slot in self.active_slots.iter().copied() {
            if self.voices[slot].active {
                begin_release(&mut self.voices[slot], splice_frames);
            }
        }
        self.held.fill(false);
        self.deferred_release.fill(false);
    }

    pub fn reset(&mut self) {
        for voice in &mut self.voices {
            *voice = VoicebankVoice::default();
        }
        self.active_slots.clear();
        self.articulation = ARTICULATION_SUSTAIN_VIBRATO;
        self.expression = 1.0;
        self.expression_current = 1.0;
        self.sustain = false;
        self.held.fill(false);
        self.deferred_release.fill(false);
        self.round_robin = 0;
    }

    /// Render one stereo frame from the indexed embedded source bank.
    #[inline]
    pub fn render_frame(&mut self) -> [f32; 2] {
        let mut output = [0.0_f32; 2];
        let mut active_index = 0;
        while active_index < self.active_slots.len() {
            let slot = self.active_slots[active_index];
            let sample = render_voice(
                &self.model,
                &mut self.voices[slot],
                self.attack_step,
                self.release_step,
                self.gain_step,
            );
            if !self.voices[slot].active {
                self.active_slots.swap_remove(active_index);
            } else {
                active_index += 1;
            }
            output[0] += sample[0];
            output[1] += sample[1];
        }
        self.expression_current = glide(self.expression_current, self.expression, self.gain_step);
        [
            output[0] * self.expression_current,
            output[1] * self.expression_current,
        ]
    }
}

fn render_voice(
    model: &VoicebankModel,
    voice: &mut VoicebankVoice,
    attack_step: f32,
    release_step: f32,
    gain_step: f32,
) -> [f32; 2] {
    let Some(primary_entry) = voice.primary_entry else {
        voice.active = false;
        return [0.0; 2];
    };
    let primary = model.sample_stereo_entry(primary_entry, voice.primary_cursor);
    let secondary = voice.secondary_entry.map_or([0.0; 2], |entry| {
        model.sample_stereo_entry(entry, voice.secondary_cursor)
    });
    let mut output = [
        primary[0] * voice.primary_gain + secondary[0] * voice.secondary_gain,
        primary[1] * voice.primary_gain + secondary[1] * voice.secondary_gain,
    ];

    // Note-off splice: fade the abandoned sustain position out under the
    // release position that replaced it. Equal power, because the two are
    // different points in the recording and therefore uncorrelated — a linear
    // fade between uncorrelated signals dips ~3 dB in the middle.
    if voice.splice_remaining > 0 {
        if let Some(entry) = voice.splice_entry {
            let old = model.sample_stereo_entry(entry, voice.splice_cursor);
            let t = 1.0 - (voice.splice_remaining as f32 / voice.splice_length.max(1) as f32);
            let (fade_in, fade_out) = (
                (t * std::f32::consts::FRAC_PI_2).sin(),
                (t * std::f32::consts::FRAC_PI_2).cos(),
            );
            output[0] = output[0] * fade_in + old[0] * voice.primary_gain * fade_out;
            output[1] = output[1] * fade_in + old[1] * voice.primary_gain * fade_out;
            advance_cursor(&mut voice.splice_cursor, voice.primary_step, entry, false);
        }
        voice.splice_remaining -= 1;
        if voice.splice_remaining == 0 {
            voice.splice_entry = None;
        }
    }

    voice.attack_gain = (voice.attack_gain + attack_step).min(1.0);
    if voice.released {
        voice.release_gain = (voice.release_gain - release_step).max(0.0);
    }
    voice.velocity_current = glide(voice.velocity_current, voice.velocity, gain_step);
    let gain = voice.velocity_current * voice.attack_gain * voice.release_gain * 0.9;
    output[0] *= gain;
    output[1] *= gain;

    // Reach the requested playback rate over a few milliseconds rather than
    // stepping to it, so a drawn pitch curve is a curve and not a staircase.
    voice.primary_step = approach(
        voice.primary_step,
        voice.primary_step_target,
        voice.primary_step_delta,
    );
    voice.secondary_step = approach(
        voice.secondary_step,
        voice.secondary_step_target,
        voice.secondary_step_delta,
    );

    advance_cursor(
        &mut voice.primary_cursor,
        voice.primary_step,
        primary_entry,
        voice.released,
    );
    if let Some(entry) = voice.secondary_entry {
        advance_cursor(
            &mut voice.secondary_cursor,
            voice.secondary_step,
            entry,
            voice.released,
        );
    }
    if voice.primary_cursor >= primary_entry.frame_count as f64
        || (voice.released && voice.release_gain <= 0.0)
    {
        voice.active = false;
    }
    output
}

fn advance_cursor(cursor: &mut f64, step: f64, entry: VoicebankEntry, released: bool) {
    *cursor += step.clamp(0.01, MAX_SAMPLE_STEP as f64);
    if !released {
        if let Some(loop_region) = entry.loop_region {
            if *cursor >= loop_region.end as f64 {
                // Skip the head portion already used by the short tail/head
                // crossfade in the model reader. This makes the first sample
                // after wrap equal to the final crossfade sample instead of
                // introducing a discontinuity at every loop boundary.
                let crossfade = 96_u64.min(loop_region.len() / 2) as f64;
                let length = (loop_region.len() as f64 - crossfade).max(1.0);
                *cursor = loop_region.start as f64
                    + crossfade
                    + (*cursor - loop_region.end as f64).rem_euclid(length);
            }
        }
    }
}

/// Point a voice's read step at the rate for `hz`, to be reached over
/// `glide_frames`.
///
/// The approach is linear and arrives exactly, so a target that stops moving
/// is played at precisely that rate. That matters beyond taste: the drawn-pitch
/// acceptance test measures the rendered frequency against the drawn one, and
/// an asymptotic approach would always land a little short.
fn aim_step(voice: &mut VoicebankVoice, hz: f32, sample_rate: f32, glide_frames: f64) {
    if let Some(entry) = voice.primary_entry {
        voice.primary_step_target = pitch_step_for(&entry, hz, sample_rate);
        voice.primary_step_delta =
            (voice.primary_step_target - voice.primary_step).abs() / glide_frames;
    }
    if let Some(entry) = voice.secondary_entry {
        voice.secondary_step_target = pitch_step_for(&entry, hz, sample_rate);
        voice.secondary_step_delta =
            (voice.secondary_step_target - voice.secondary_step).abs() / glide_frames;
    }
}

/// Move `current` toward `target` by `delta`, stopping exactly on it.
#[inline]
fn approach(current: f64, target: f64, delta: f64) -> f64 {
    if delta <= 0.0 || (target - current).abs() <= delta {
        target
    } else {
        current + delta.copysign(target - current)
    }
}

fn begin_release(voice: &mut VoicebankVoice, splice_frames: u32) {
    voice.released = true;
    voice.release_gain = 1.0;
    if let Some(range) = voice.primary_entry.and_then(|entry| entry.release_region) {
        let spliced = voice.primary_cursor.max(range.start as f64);
        // Only crossfade when the position actually moves. A voice already
        // inside its release region carries on from where it is, and fading it
        // against itself would only cost work.
        if spliced > voice.primary_cursor {
            voice.splice_cursor = voice.primary_cursor;
            voice.splice_entry = voice.primary_entry;
            voice.splice_remaining = splice_frames;
            voice.splice_length = splice_frames;
        }
        voice.primary_cursor = spliced;
    }
    if let Some(range) = voice.secondary_entry.and_then(|entry| entry.release_region) {
        voice.secondary_cursor = voice.secondary_cursor.max(range.start as f64);
    }
}

fn pitch_step_for(entry: &VoicebankEntry, target_hz: f32, sample_rate: f32) -> f64 {
    let ratio =
        finite_or(target_hz, entry.root_pitch_hz) as f64 / entry.root_pitch_hz.max(1.0) as f64;
    (ratio * entry.sample_rate as f64 / sample_rate.max(1.0) as f64)
        .clamp(0.01, MAX_SAMPLE_STEP as f64)
}

fn midi_to_hz(note: u8) -> f32 {
    440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0)
}

/// Move `current` toward `target` by at most `step`, exactly reaching it.
///
/// A linear approach rather than a one-pole: it arrives in a bounded time
/// (`GAIN_GLIDE_SECONDS`) instead of asymptotically, so a fader left alone
/// settles rather than creeping, and there is no denormal tail to flush.
#[inline]
fn glide(current: f32, target: f32, step: f32) -> f32 {
    let delta = target - current;
    if delta.abs() <= step {
        target
    } else {
        current + step.copysign(delta)
    }
}

fn matches_note_id(voice_note_id: i32, requested: i32) -> bool {
    requested < 0 || voice_note_id == requested
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solfege_model::voicebank::{FrameRange, VoicebankEntry, encode_audio, encode_index};

    fn fixture() -> VoicebankModel {
        let entries = vec![VoicebankEntry {
            id: 1,
            midi_note: 60,
            root_pitch_hz: 261.625_55,
            articulation: 0,
            dynamic: 0,
            dynamic_value: 0.2,
            round_robin: Some(1),
            audio_offset: 0,
            audio_size: 128,
            frame_count: 32,
            sample_rate: 48_000,
            channels: 2,
            loop_region: FrameRange::new(8, 24),
            attack_region: FrameRange::new(0, 8),
            sustain_region: FrameRange::new(8, 24),
            release_region: FrameRange::new(24, 32),
        }];
        let index = encode_index(&entries, 48_000, 2, 1, 32).unwrap();
        let audio = encode_audio(48_000, 2, 1, 1, 32, &[6_000_i16; 64]).unwrap();
        VoicebankModel::from_sections(&index, &audio).unwrap()
    }

    #[test]
    fn indexed_source_produces_audio_and_reset() {
        let mut renderer = VoicebankRenderer::new(fixture(), 48_000, 4);
        renderer.note_on(60, 0.8, 1);
        let mut energy = 0.0;
        for _ in 0..64 {
            let frame = renderer.render_frame();
            energy += frame[0].abs() + frame[1].abs();
        }
        assert!(energy > 0.0);
        renderer.reset();
        assert_eq!(renderer.active_voices(), 0);
    }

    #[test]
    fn pitch_and_release_are_applied_to_prepared_voice() {
        let mut renderer = VoicebankRenderer::new(fixture(), 48_000, 1);
        renderer.note_on(60, 1.0, 7);
        renderer.set_pitch(7, 300.0);
        for _ in 0..8 {
            let _ = renderer.render_frame();
        }
        renderer.note_off(60, 7);
        assert!(renderer.active_voices() > 0);
    }
}
