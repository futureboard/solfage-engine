//! Host-independent sampler runtime.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use solfege_acoustic::VoicebankRenderer;
use solfege_audio::SampleRate;
use solfege_core::{GestureControl, PhysicalModel, RuntimeInstrument};
use solfege_event::{Event, TimedEvent};
use solfege_voice::{EnvelopeConfig, VoicePool};
use solfege_zone::TriggerMode;

#[cfg(feature = "fbmx")]
pub mod fbmx;
pub mod sfm;

#[cfg(test)]
mod allocation_probe {
    use std::{
        alloc::{GlobalAlloc, Layout, System},
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct CountingAllocator;

    thread_local! {
        static TRACKING: Cell<bool> = const { Cell::new(false) };
    }

    static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

    // SAFETY: every operation delegates to the process System allocator with
    // the original pointer/layout. The extra bookkeeping is atomic and does
    // not change allocation semantics.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if TRACKING.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            // SAFETY: delegated with the caller's unchanged valid layout.
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
            // SAFETY: delegated with the caller's unchanged pointer/layout.
            unsafe { System.dealloc(pointer, layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if TRACKING.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            // SAFETY: delegated with the caller's unchanged valid layout.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
            if TRACKING.try_with(Cell::get).unwrap_or(false) {
                ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
            }
            // SAFETY: delegated with the caller's unchanged pointer/layout and
            // requested size.
            unsafe { System.realloc(pointer, layout, size) }
        }
    }

    #[global_allocator]
    static GLOBAL: CountingAllocator = CountingAllocator;

    pub fn count(action: impl FnOnce()) -> usize {
        TRACKING.with(|tracking| tracking.set(true));
        ALLOCATIONS.store(0, Ordering::Relaxed);
        action();
        TRACKING.with(|tracking| tracking.set(false));
        ALLOCATIONS.load(Ordering::Relaxed)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    pub sample_rate: SampleRate,
    pub max_block_frames: usize,
    pub polyphony: usize,
    pub gesture_smoothing_samples: u32,
    pub pitch_bend_semitones: f32,
}

impl EngineConfig {
    pub fn realtime(sample_rate: SampleRate) -> Self {
        Self {
            sample_rate,
            max_block_frames: 2048,
            polyphony: 128,
            gesture_smoothing_samples: 32,
            pitch_bend_semitones: 2.0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum EngineCommand {
    Event(Event),
    SetEnvelope(EnvelopeConfig),
    SetMasterGain(f32),
}

#[derive(Default)]
pub struct SharedMetrics {
    active_voices: AtomicUsize,
    peak_voices: AtomicUsize,
    render_micros: AtomicU64,
    output_peak_bits: AtomicU32,
    output_rms_bits: AtomicU32,
    underruns: AtomicU64,
    mapped_bytes: AtomicU64,
    sample_rate_bits: AtomicU32,
    block_frames: AtomicUsize,
    /// Continuous-pitch events a host could not hand to this engine because
    /// its per-block list was already full.
    ///
    /// The lists are fixed-capacity so the audio thread never reallocates, but
    /// a full list used to mean the event simply vanished. A dropped pitch
    /// point is audible — the line stops following the drawn curve — so it is
    /// counted and reported rather than absorbed.
    dropped_pitch_events: AtomicU64,
    /// Articulation events dropped for the same reason.
    dropped_articulation_events: AtomicU64,
}

impl SharedMetrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            active_voices: self.active_voices.load(Ordering::Relaxed),
            peak_voices: self.peak_voices.load(Ordering::Relaxed),
            render_micros: self.render_micros.load(Ordering::Relaxed),
            output_peak: f32::from_bits(self.output_peak_bits.load(Ordering::Relaxed)),
            output_rms: f32::from_bits(self.output_rms_bits.load(Ordering::Relaxed)),
            underruns: self.underruns.load(Ordering::Relaxed),
            mapped_bytes: self.mapped_bytes.load(Ordering::Relaxed),
            sample_rate: f32::from_bits(self.sample_rate_bits.load(Ordering::Relaxed)),
            block_frames: self.block_frames.load(Ordering::Relaxed),
            dropped_pitch_events: self.dropped_pitch_events.load(Ordering::Relaxed),
            dropped_articulation_events: self.dropped_articulation_events.load(Ordering::Relaxed),
        }
    }

    pub fn record_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// One continuous-pitch event was discarded because the block list was
    /// full. Realtime-safe: one relaxed increment.
    pub fn record_dropped_pitch_event(&self) {
        self.dropped_pitch_events.fetch_add(1, Ordering::Relaxed);
    }

    /// One articulation event was discarded because the block list was full.
    pub fn record_dropped_articulation_event(&self) {
        self.dropped_articulation_events
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct MetricsSnapshot {
    pub active_voices: usize,
    pub peak_voices: usize,
    pub render_micros: u64,
    pub output_peak: f32,
    pub output_rms: f32,
    pub underruns: u64,
    pub mapped_bytes: u64,
    pub sample_rate: f32,
    pub block_frames: usize,
    pub dropped_pitch_events: u64,
    pub dropped_articulation_events: u64,
}

pub struct SamplerEngine {
    config: EngineConfig,
    instrument: Option<RuntimeInstrument>,
    voices: VoicePool,
    envelope: EnvelopeConfig,
    master_gain: f32,
    sustain: bool,
    held: [bool; 128],
    deferred_release: [bool; 128],
    voicebank: Option<VoicebankRenderer>,
    peak_active_voices: usize,
    metrics: Arc<SharedMetrics>,
    #[cfg(feature = "fbmx")]
    fbmx: Option<fbmx::FbmxHooks>,
}

/// General engine name for new callers; `SamplerEngine` remains as a source
/// compatible alias for existing hosts.
pub type Engine = SamplerEngine;

impl SamplerEngine {
    pub fn new(
        config: EngineConfig,
        instrument: Option<RuntimeInstrument>,
        metrics: Arc<SharedMetrics>,
    ) -> Self {
        metrics
            .sample_rate_bits
            .store(config.sample_rate.get().to_bits(), Ordering::Relaxed);
        metrics.mapped_bytes.store(
            instrument
                .as_ref()
                .map_or(0, RuntimeInstrument::mapped_bytes),
            Ordering::Relaxed,
        );
        let physical_config = instrument
            .as_ref()
            .and_then(RuntimeInstrument::physical_model)
            .map_or_else(
                solfege_core::BowedStringConfig::default,
                |model| match model {
                    PhysicalModel::BowedString(config) => config,
                },
            );
        Self {
            // A voicebank-only SFM still owns the shared VoicePool type, but
            // does not need a 128-voice physical pool. Keeping one spare
            // physical voice makes the object shape stable while avoiding a
            // large unused working set for the clean acoustic path.
            voices: VoicePool::with_physical_config(
                instrument.as_ref().map_or(1, |_| config.polyphony),
                config.sample_rate.get(),
                physical_config,
            ),
            config,
            instrument,
            envelope: EnvelopeConfig::default(),
            master_gain: 0.8,
            sustain: false,
            held: [false; 128],
            deferred_release: [false; 128],
            voicebank: None,
            peak_active_voices: 0,
            metrics,
            #[cfg(feature = "fbmx")]
            fbmx: None,
        }
    }

    /// Lifecycle-oriented constructor matching host activation terminology.
    /// Preparation is control-thread work; the returned engine owns all state
    /// needed by the realtime `process` methods.
    pub fn prepare(
        config: EngineConfig,
        instrument: Option<RuntimeInstrument>,
        metrics: Arc<SharedMetrics>,
    ) -> Self {
        Self::new(config, instrument, metrics)
    }

    /// Clone a prepared engine for a new runtime graph without reopening or
    /// reparsing model assets. Mutable voice state is reset, while immutable
    /// instrument/voicebank/neural weights remain shared or cheaply cloned.
    pub fn clone_prepared(&self) -> Self {
        let mut clone = Self::new(
            self.config,
            self.instrument.clone(),
            Arc::new(SharedMetrics::default()),
        );
        clone.envelope = self.envelope;
        clone.master_gain = self.master_gain;
        clone.voicebank = self
            .voicebank
            .as_ref()
            .map(VoicebankRenderer::clone_prepared);
        #[cfg(feature = "fbmx")]
        {
            clone.fbmx = self.fbmx.clone();
        }
        clone
    }

    /// Prepare a complete SFM instrument on the control thread. The returned
    /// engine owns the physical voice state and the complete embedded
    /// voicebank. When this crate is built with `fbmx`, it also owns the
    /// verified embedded residual runtime. The audio callback only sees the
    /// already-prepared engine.
    pub fn prepare_sfm(
        config: EngineConfig,
        model: solfege_model::SfmFile,
        metrics: Arc<SharedMetrics>,
        mode: sfm::SfmMode,
    ) -> Result<Self, sfm::SfmEngineError> {
        Self::prepare_sfm_staged(config, model, metrics, mode, &mut |_| true)
    }

    /// [`SamplerEngine::prepare_sfm`] with the stage boundaries exposed.
    ///
    /// `stage` is called as each step begins and returns `false` to abandon the
    /// load, which fails with [`sfm::SfmEngineError::Cancelled`]. Preparation
    /// builds into locals and only moves them into the engine once every stage
    /// has succeeded, so an abandoned load leaves no partially prepared
    /// instrument behind for a caller to install.
    #[allow(unused_mut)]
    pub fn prepare_sfm_staged(
        config: EngineConfig,
        model: solfege_model::SfmFile,
        metrics: Arc<SharedMetrics>,
        mode: sfm::SfmMode,
        stage: &mut dyn FnMut(sfm::SfmLoadStage) -> bool,
    ) -> Result<Self, sfm::SfmEngineError> {
        if !stage(sfm::SfmLoadStage::LoadingPhysicalModel) {
            return Err(sfm::SfmEngineError::Cancelled);
        }
        let profile = model.physical_profile()?;
        let metadata = model.metadata_json()?;
        let name = metadata
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("SFM instrument")
            .to_owned();
        let physical = PhysicalModel::BowedString(sfm::to_bowed_string_config(&profile));
        let instrument = if matches!(mode, sfm::SfmMode::VoicebankOnly) {
            None
        } else {
            Some(
                RuntimeInstrument::prepare_physical(
                    solfege_zone::Instrument {
                        name,
                        groups: Vec::new(),
                        zones: Vec::new(),
                    },
                    physical,
                )
                .map_err(|error| sfm::SfmEngineError::Physical(error.to_string()))?,
            )
        };
        let voicebank = if matches!(mode, sfm::SfmMode::Hybrid | sfm::SfmMode::VoicebankOnly) {
            if !stage(sfm::SfmLoadStage::LoadingVoicebank) {
                return Err(sfm::SfmEngineError::Cancelled);
            }
            model.voicebank_model()?
        } else {
            None
        };

        // Only `Hybrid` installs the residual, and that is a deliberate hold
        // rather than an oversight.
        //
        // The residual is trained against the *voicebank's* own output, so on
        // the merits it belongs on `VoicebankOnly` too — the mode the DAW
        // actually plays. It is not enabled there because no model has passed
        // `neural/scripts/bypass_baseline.py`, which asks the only question
        // that decides it: on held-out data, is `model(dry)` closer to the
        // reference than `dry` already was? The current best answers no on
        // balance. It improves transposed notes by 2.3 dB and colours
        // exactly-resolved ones by 18 dB, and exactly-resolved notes are the
        // common case in normal playing.
        //
        // Add `SfmMode::VoicebankOnly` to this match when a model beats bypass
        // on *both* pair kinds. Nothing else needs to change: the hooks, the
        // per-channel runtimes and the silent-block skip are all already in
        // place. The model tools render `Hybrid`, so measurement is unaffected
        // by this hold.
        #[cfg(feature = "fbmx")]
        let residual = if matches!(mode, sfm::SfmMode::Hybrid) {
            match model.section(solfege_model::FBMX_RESIDUAL_TAG) {
                Some(raw) => {
                    if !stage(sfm::SfmLoadStage::LoadingNeuralModel) {
                        return Err(sfm::SfmEngineError::Cancelled);
                    }
                    let model = fbmx_runtime::FbmxModel::from_bytes(raw)
                        .map_err(|error| sfm::SfmEngineError::Fbmx(error.to_string()))?;
                    Some(
                        fbmx::FbmxHooks::from_models(None, Some(model))
                            .map_err(|error| sfm::SfmEngineError::Fbmx(error.to_string()))?,
                    )
                }
                None => None,
            }
        } else {
            None
        };

        if !stage(sfm::SfmLoadStage::PreparingEngine) {
            return Err(sfm::SfmEngineError::Cancelled);
        }
        let mut engine = Self::prepare(config, instrument, metrics);
        engine.voicebank = voicebank.map(|voicebank_model| {
            VoicebankRenderer::new(
                voicebank_model,
                config.sample_rate.get() as u32,
                config.polyphony,
            )
        });

        #[cfg(feature = "fbmx")]
        if let Some(hooks) = residual {
            engine.set_fbmx_hooks(hooks);
        }

        #[cfg(not(feature = "fbmx"))]
        let _ = mode;

        Ok(engine)
    }

    pub fn metrics(&self) -> &Arc<SharedMetrics> {
        &self.metrics
    }

    /// Prepared voice/DSP working memory. Mapped sample bytes are reported
    /// separately through `SharedMetrics::snapshot().mapped_bytes`.
    pub fn working_memory_bytes(&self) -> usize {
        self.voices.memory_bytes()
            + self
                .voicebank
                .as_ref()
                .map_or(0, solfege_acoustic::VoicebankRenderer::memory_bytes)
    }

    pub fn instrument_name(&self) -> Option<&str> {
        self.instrument
            .as_ref()
            .map(|instrument| instrument.model.name.as_str())
    }

    #[cfg(feature = "fbmx")]
    pub fn set_fbmx_hooks(&mut self, hooks: fbmx::FbmxHooks) {
        self.fbmx = Some(hooks);
    }

    #[cfg(feature = "fbmx")]
    pub fn clear_fbmx_hooks(&mut self) {
        self.fbmx = None;
    }

    #[cfg(feature = "fbmx")]
    pub fn process_performer_sample(&mut self, input: f32) -> Option<f32> {
        self.fbmx
            .as_mut()
            .and_then(|hooks| hooks.process_performer_sample(input))
    }

    pub fn reset(&mut self) {
        self.voices.reset();
        self.sustain = false;
        self.held.fill(false);
        self.deferred_release.fill(false);
        self.peak_active_voices = 0;
        if let Some(voicebank) = self.voicebank.as_mut() {
            voicebank.reset();
        }
        #[cfg(feature = "fbmx")]
        if let Some(hooks) = self.fbmx.as_mut() {
            hooks.reset();
        }
    }

    pub fn handle_command(&mut self, command: EngineCommand) {
        match command {
            EngineCommand::Event(event) => self.handle_event(event),
            EngineCommand::SetEnvelope(envelope) => self.envelope = envelope.sanitized(),
            EngineCommand::SetMasterGain(gain) => self.master_gain = gain.clamp(0.0, 2.0),
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        if !matches!(event, Event::NoteOn { .. }) {
            self.handle_voicebank_event(&event);
        }
        match event {
            Event::NoteOn {
                note,
                velocity,
                note_id,
            } => self.handle_note_on(note, velocity, note_id, None),
            Event::NoteOff {
                note,
                velocity: _,
                note_id,
            } => {
                self.held[note.min(127) as usize] = false;
                if self.sustain {
                    self.deferred_release[note.min(127) as usize] = true;
                } else {
                    self.voices
                        .note_off(note, note_id, self.config.sample_rate.get());
                }
            }
            Event::Sustain(enabled) => {
                if self.sustain && !enabled {
                    for note in 0_u8..=127 {
                        if self.deferred_release[note as usize] && !self.held[note as usize] {
                            self.voices
                                .note_off(note, -1, self.config.sample_rate.get());
                            self.deferred_release[note as usize] = false;
                        }
                    }
                }
                self.sustain = enabled;
            }
            Event::AllNotesOff => {
                self.voices.all_notes_off(self.config.sample_rate.get());
                self.held.fill(false);
                self.deferred_release.fill(false);
            }
            Event::ControlChange {
                controller: 64,
                value,
                ..
            } => self.handle_event(Event::Sustain(value >= 0.5)),
            Event::PolyPressure { note_id, value, .. } => {
                self.set_gesture(note_id, GestureControl::Pressure, value)
            }
            Event::ChannelPressure { value, .. } => {
                self.set_gesture(-1, GestureControl::Pressure, value)
            }
            Event::ControlChange {
                controller, value, ..
            } => self.handle_compatibility_control(controller, value),
            Event::PitchBend { value, .. } => self.voices.set_pitch_bend(
                value,
                self.config.pitch_bend_semitones,
                self.config.gesture_smoothing_samples,
            ),
            Event::NoteExpression {
                note_id,
                pitch,
                pressure,
                slide,
            } => {
                if pitch.is_finite() && pitch > 0.0 {
                    self.voices
                        .set_pitch(note_id, pitch, self.config.gesture_smoothing_samples);
                }
                self.set_gesture(note_id, GestureControl::Pressure, pressure);
                self.set_gesture(note_id, GestureControl::BowPosition, slide);
            }
            Event::Pitch { note_id, hz } => {
                self.voices
                    .set_pitch(note_id, hz, self.config.gesture_smoothing_samples)
            }
            Event::Expression { note_id, value } => {
                self.set_gesture(note_id, GestureControl::Expression, value)
            }
            Event::Gesture {
                note_id,
                control,
                value,
            } => self.set_gesture(note_id, control, value),
            Event::Articulation { articulation, .. } => {
                let _ = articulation;
                #[cfg(feature = "fbmx")]
                if let Some(hooks) = self.fbmx.as_mut() {
                    hooks.set_residual_articulation(articulation);
                }
            }
            Event::Parameter { .. } | Event::Transport { .. } => {}
            Event::Sostenuto(_) => {}
        }
    }

    /// Start a note while forcing one prepared voicebank entry. This is a
    /// control/offline hook for deterministic source-replacement tests and
    /// pair generation; normal hosts should send [`Event::NoteOn`] and let
    /// the resolver choose the source.
    pub fn handle_note_on_with_voicebank_entry(
        &mut self,
        note: u8,
        velocity: f32,
        note_id: i32,
        entry_index: usize,
    ) {
        self.handle_note_on(note, velocity, note_id, Some(entry_index));
    }

    fn handle_note_on(
        &mut self,
        note: u8,
        velocity: f32,
        note_id: i32,
        voicebank_entry: Option<usize>,
    ) {
        if let Some(voicebank) = self.voicebank.as_mut() {
            if let Some(entry_index) = voicebank_entry {
                // `note` is honoured here. It used to be silently discarded in
                // favour of the entry's own recorded pitch, which made it
                // impossible to ask for "this recording, played at that pitch"
                // — the exact situation the runtime is in for every note the
                // bank does not contain.
                voicebank.note_on_entry_at(entry_index, note, velocity, note_id);
            } else {
                voicebank.note_on(note, velocity, note_id);
            }
        }
        #[cfg(feature = "fbmx")]
        if let Some(hooks) = self.fbmx.as_mut() {
            hooks.set_residual_note(note as f32, velocity);
        }
        self.held[note.min(127) as usize] = true;
        self.deferred_release[note.min(127) as usize] = false;
        let Some(instrument) = self.instrument.as_ref() else {
            return;
        };
        if instrument.is_physical() {
            self.voices
                .note_on(note, velocity, note_id, 0, instrument, self.envelope);
            return;
        }
        let velocity_midi = (velocity.clamp(0.0, 1.0) * 127.0).round() as u8;
        for &zone_index in instrument.zone_index.candidates(note) {
            if instrument.model.zones[zone_index].matches(note, velocity_midi, TriggerMode::Attack)
            {
                self.voices.note_on(
                    note,
                    velocity,
                    note_id,
                    zone_index,
                    instrument,
                    self.envelope,
                );
            }
        }
    }

    fn handle_voicebank_event(&mut self, event: &Event) {
        let Some(voicebank) = self.voicebank.as_mut() else {
            return;
        };
        match *event {
            Event::NoteOff { note, note_id, .. } => voicebank.note_off(note, note_id),
            Event::Sustain(enabled) => voicebank.set_sustain(enabled),
            Event::ControlChange {
                controller, value, ..
            } => voicebank.control_change(controller, value),
            Event::Expression { note_id, value } => {
                voicebank.set_expression(value);
                voicebank.set_dynamic(note_id, value);
            }
            Event::Pitch { note_id, hz } => voicebank.set_pitch(note_id, hz),
            Event::PitchBend { value, .. } => {
                voicebank.set_pitch_bend(value, self.config.pitch_bend_semitones)
            }
            Event::Gesture {
                note_id,
                control,
                value,
            } => {
                if matches!(
                    control,
                    GestureControl::Expression
                        | GestureControl::Pressure
                        | GestureControl::BowPressure
                ) {
                    voicebank.set_dynamic(note_id, value);
                }
            }
            Event::NoteExpression {
                note_id,
                pitch,
                pressure,
                ..
            } => {
                voicebank.set_pitch(note_id, pitch);
                voicebank.set_dynamic(note_id, pressure);
            }
            Event::Articulation { articulation, .. } => {
                let index = match articulation {
                    solfege_event::Articulation::Attack | solfege_event::Articulation::Legato => {
                        solfege_model::voicebank::ARTICULATION_SUSTAIN_VIBRATO
                    }
                    solfege_event::Articulation::Release => return,
                    solfege_event::Articulation::Custom(value) => (value as u8).min(7),
                };
                voicebank.set_articulation(index);
            }
            Event::AllNotesOff => voicebank.all_notes_off(),
            _ => {}
        }
    }

    /// Host-neutral render entry point. `events` are sorted by sample offset
    /// within this caller-owned block; the interleaved buffer is cleared and
    /// filled in place.
    pub fn process(&mut self, output: &mut [f32], channels: usize, events: &[TimedEvent]) {
        self.process_interleaved(output, channels, events);
    }

    /// Render directly into separate stereo planes. This keeps a stereo
    /// voicebank stereo in hosts that already have left/right track buffers
    /// and avoids an interleave/deinterleave scratch buffer.
    pub fn process_stereo(&mut self, left: &mut [f32], right: &mut [f32], events: &[TimedEvent]) {
        let started = Instant::now();
        left.fill(0.0);
        right.fill(0.0);
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        let mut event_index = 0;
        #[cfg(feature = "fbmx")]
        self.refresh_residual_activity();
        for frame in 0..frames {
            #[cfg_attr(not(feature = "fbmx"), allow(unused_mut, unused_variables))]
            let mut notes_changed = false;
            while let Some(event) = events.get(event_index) {
                if event.sample_offset() as usize > frame {
                    break;
                }
                self.handle_event(event.event);
                event_index += 1;
                notes_changed = true;
            }
            let mut stereo = [0.0_f32; 2];
            if let Some(instrument) = self.instrument.as_ref() {
                self.voices
                    .render_frame(&mut stereo, 2, self.config.sample_rate.get(), instrument);
            }
            if let Some(voicebank) = self.voicebank.as_mut() {
                let sample = voicebank.render_frame();
                stereo[0] += sample[0];
                stereo[1] += sample[1];
            }
            #[cfg(feature = "fbmx")]
            {
                if notes_changed {
                    self.refresh_residual_activity();
                } else {
                    self.track_residual_activity();
                }
                if let Some(hooks) = self.fbmx.as_mut() {
                    hooks.process_residual_frame(&mut stereo);
                }
            }
            left[frame] = stereo[0];
            right[frame] = stereo[1];
        }
        for (left, right) in left[..frames].iter_mut().zip(right[..frames].iter_mut()) {
            *left *= self.master_gain;
            *right *= self.master_gain;
        }
        self.publish_stereo_metrics(&left[..frames], &right[..frames], frames, started);
    }

    fn set_gesture(&mut self, note_id: i32, control: GestureControl, value: f32) {
        self.voices.set_gesture(
            note_id,
            control,
            value,
            self.config.gesture_smoothing_samples,
        );
    }

    fn handle_compatibility_control(&mut self, controller: u8, value: f32) {
        match controller {
            1 => self.set_gesture(-1, GestureControl::VibratoDepth, value),
            2 => self.set_gesture(-1, GestureControl::BreathPressure, value),
            11 => self.set_gesture(-1, GestureControl::Expression, value),
            74 => self.set_gesture(-1, GestureControl::BowPosition, value),
            _ => {}
        }
    }

    /// Renders sorted sample-offset events into caller-owned interleaved memory.
    /// No allocation, locking, parsing, or file operations occur here.
    pub fn process_interleaved(
        &mut self,
        output: &mut [f32],
        channels: usize,
        events: &[TimedEvent],
    ) {
        let started = Instant::now();
        output.fill(0.0);
        if channels == 0 || !output.len().is_multiple_of(channels) {
            return;
        }
        let frames = output.len() / channels;
        let mut event_index = 0;
        #[cfg(feature = "fbmx")]
        self.refresh_residual_activity();
        for frame in 0..frames {
            #[cfg_attr(not(feature = "fbmx"), allow(unused_mut, unused_variables))]
            let mut notes_changed = false;
            while let Some(event) = events.get(event_index) {
                if event.sample_offset() as usize > frame {
                    break;
                }
                self.handle_event(event.event);
                event_index += 1;
                notes_changed = true;
            }
            let frame_start = frame * channels;
            if let Some(instrument) = self.instrument.as_ref() {
                self.voices.render_frame(
                    &mut output[frame_start..frame_start + channels],
                    channels,
                    self.config.sample_rate.get(),
                    instrument,
                );
            }
            if let Some(voicebank) = self.voicebank.as_mut() {
                let sample = voicebank.render_frame();
                if channels == 1 {
                    output[frame_start] += (sample[0] + sample[1]) * 0.5;
                } else if channels >= 2 {
                    output[frame_start] += sample[0];
                    output[frame_start + 1] += sample[1];
                }
            }
            #[cfg(feature = "fbmx")]
            {
                if notes_changed {
                    self.refresh_residual_activity();
                } else {
                    self.track_residual_activity();
                }
                if let Some(hooks) = self.fbmx.as_mut() {
                    hooks.process_residual_frame(
                        &mut output[frame_start..frame_start + channels],
                    );
                }
            }
        }
        for sample in output.iter_mut() {
            *sample *= self.master_gain;
        }
        self.publish_metrics(output, frames, started);
    }

    /// Voices the instrument is currently sounding, physical plus voicebank.
    ///
    /// Read before the residual runs so its state lifetime follows the notes
    /// rather than the host's buffer size.
    /// Push the current voice count into the residual hook.
    ///
    /// Called once per block and again after any event, so the 0 -> sounding
    /// edge that resets the model's state lands on the note-on sample rather
    /// than on a block boundary.
    #[cfg(feature = "fbmx")]
    fn refresh_residual_activity(&mut self) {
        if self.fbmx.is_none() {
            return;
        }
        self.voices.refresh_active_count();
        let active_voices = self.sounding_voices();
        if let Some(hooks) = self.fbmx.as_mut() {
            hooks.set_active_voices(active_voices);
        }
    }

    /// Push the post-render voice count into the residual hook.
    ///
    /// `VoicePool::render_frame` retires voices whose envelope has ended, so
    /// this is the only place the 1 -> 0 edge can be seen on the exact sample
    /// it happens rather than at the next block boundary.
    #[cfg(feature = "fbmx")]
    #[inline]
    fn track_residual_activity(&mut self) {
        let active_voices = self.sounding_voices();
        if let Some(hooks) = self.fbmx.as_mut() {
            hooks.set_active_voices(active_voices);
        }
    }

    fn sounding_voices(&self) -> usize {
        let voicebank_active = self
            .voicebank
            .as_ref()
            .map_or(0, VoicebankRenderer::active_voices);
        self.voices.active_count() + voicebank_active
    }

    fn publish_metrics(&mut self, output: &[f32], frames: usize, started: Instant) {
        let mut peak = 0.0_f32;
        let mut squares = 0.0_f64;
        for &sample in output {
            peak = peak.max(sample.abs());
            squares += (sample as f64) * (sample as f64);
        }
        let rms = if output.is_empty() {
            0.0
        } else {
            (squares / output.len() as f64).sqrt() as f32
        };
        let voicebank_active = self
            .voicebank
            .as_ref()
            .map_or(0, VoicebankRenderer::active_voices);
        let active_voices = self.voices.active_count() + voicebank_active;
        self.peak_active_voices = self
            .peak_active_voices
            .max(self.voices.peak_active())
            .max(active_voices);
        self.metrics
            .active_voices
            .store(active_voices, Ordering::Relaxed);
        self.metrics
            .peak_voices
            .store(self.peak_active_voices, Ordering::Relaxed);
        self.metrics.render_micros.store(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.metrics
            .output_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
        self.metrics
            .output_rms_bits
            .store(rms.to_bits(), Ordering::Relaxed);
        self.metrics.block_frames.store(frames, Ordering::Relaxed);
    }

    fn publish_stereo_metrics(
        &mut self,
        left: &[f32],
        right: &[f32],
        frames: usize,
        started: Instant,
    ) {
        let mut peak = 0.0_f32;
        let mut squares = 0.0_f64;
        for (&left, &right) in left.iter().zip(right.iter()) {
            peak = peak.max(left.abs()).max(right.abs());
            squares += (left as f64) * (left as f64) + (right as f64) * (right as f64);
        }
        let sample_count = left.len().saturating_add(right.len());
        let rms = if sample_count == 0 {
            0.0
        } else {
            (squares / sample_count as f64).sqrt() as f32
        };
        let voicebank_active = self
            .voicebank
            .as_ref()
            .map_or(0, VoicebankRenderer::active_voices);
        let active_voices = self.voices.active_count() + voicebank_active;
        self.peak_active_voices = self
            .peak_active_voices
            .max(self.voices.peak_active())
            .max(active_voices);
        self.metrics
            .active_voices
            .store(active_voices, Ordering::Relaxed);
        self.metrics
            .peak_voices
            .store(self.peak_active_voices, Ordering::Relaxed);
        self.metrics.render_micros.store(
            started.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
        self.metrics
            .output_peak_bits
            .store(peak.to_bits(), Ordering::Relaxed);
        self.metrics
            .output_rms_bits
            .store(rms.to_bits(), Ordering::Relaxed);
        self.metrics.block_frames.store(frames, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solfege_core::PreparedSample;
    use solfege_model::{
        AUDIO_TAG, INDEX_TAG, METADATA_TAG, PHYSICAL_TAG, PhysicalProfile, SfmBuilder, SfmFile,
    };
    use solfege_storage::{PcmFormat, PreloadedStorage, SampleStorage, WavLayout};
    use solfege_zone::{Instrument, Zone};

    fn test_instrument() -> RuntimeInstrument {
        let pcm: Vec<u8> = [0_i16, 16_384, 32_767, 16_384, 0, -16_384]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect();
        let storage: Arc<dyn SampleStorage> = Arc::new(PreloadedStorage::from_bytes(pcm));
        let sample = PreparedSample::new(
            storage,
            WavLayout {
                sample_rate: 48_000,
                channels: 1,
                format: PcmFormat::Signed16,
                data_offset: 0,
                data_len: 12,
                frames: 6,
                block_align: 2,
            },
        )
        .unwrap();
        RuntimeInstrument::prepare(
            Instrument {
                name: "test".to_owned(),
                groups: Vec::new(),
                zones: vec![Zone::mapped_sample(0, 60)],
            },
            vec![sample],
        )
        .unwrap()
    }

    fn test_voicebank_sfm() -> SfmFile {
        use solfege_model::voicebank::{
            ARTICULATION_SUSTAIN_VIBRATO, FrameRange, VoicebankEntry, encode_audio, encode_index,
        };

        let entry = VoicebankEntry {
            id: 42,
            midi_note: 60,
            root_pitch_hz: solfege_core::midi_note_to_hz(60),
            articulation: ARTICULATION_SUSTAIN_VIBRATO,
            dynamic: 0,
            dynamic_value: 0.2,
            round_robin: None,
            audio_offset: 0,
            audio_size: 64 * 2 * 2,
            frame_count: 64,
            sample_rate: 48_000,
            channels: 2,
            loop_region: FrameRange::new(8, 48),
            attack_region: FrameRange::new(0, 8),
            sustain_region: FrameRange::new(8, 48),
            release_region: FrameRange::new(48, 64),
        };
        let index = encode_index(&[entry], 48_000, 2, 1, 64).unwrap();
        let samples: Vec<i16> = (0..64).flat_map(|_| [12_000_i16, 6_000_i16]).collect();
        let audio = encode_audio(48_000, 2, 1, 1, 64, &samples).unwrap();
        let profile = PhysicalProfile::default();
        let bytes = SfmBuilder::new()
            .add_section(PHYSICAL_TAG, 0, profile.to_json_bytes().unwrap())
            .unwrap()
            .add_section(
                METADATA_TAG,
                0,
                serde_json::to_vec(&serde_json::json!({
                    "name": "test voicebank",
                    "architecture": "neural-voicebank"
                }))
                .unwrap(),
            )
            .unwrap()
            .add_section(INDEX_TAG, 0, index)
            .unwrap()
            .add_section(AUDIO_TAG, 0, audio)
            .unwrap()
            .build()
            .unwrap();
        SfmFile::from_bytes(&bytes).unwrap()
    }

    #[test]
    fn empty_engine_renders_silence() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let metrics = Arc::new(SharedMetrics::default());
        let mut engine = SamplerEngine::new(EngineConfig::realtime(rate), None, metrics);
        let mut output = [1.0_f32; 32];
        engine.process_interleaved(&mut output, 2, &[]);
        assert!(output.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn mapped_pcm_plays_at_root_pitch_through_internal_events() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let metrics = Arc::new(SharedMetrics::default());
        let mut engine = SamplerEngine::new(
            EngineConfig::realtime(rate),
            Some(test_instrument()),
            metrics.clone(),
        );
        engine.handle_command(EngineCommand::SetEnvelope(EnvelopeConfig {
            attack_seconds: 0.0,
            decay_seconds: 0.0,
            sustain_level: 1.0,
            release_seconds: 0.01,
        }));
        let events = [TimedEvent::immediate(Event::NoteOn {
            note: 60,
            velocity: 1.0,
            note_id: 1,
        })];
        let mut output = [0.0_f32; 12];
        engine.process_interleaved(&mut output, 2, &events);

        assert!(output.iter().any(|sample| sample.abs() > 0.1));
        assert_eq!(metrics.snapshot().active_voices, 0);
        assert_eq!(metrics.snapshot().peak_voices, 1);
    }

    #[test]
    fn physical_voice_renders_at_sample_offset_and_accepts_gestures() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let metrics = Arc::new(SharedMetrics::default());
        let instrument = RuntimeInstrument::bowed_string(
            "generic bowed string",
            solfege_core::BowedStringConfig::default(),
        );
        let mut config = EngineConfig::realtime(rate);
        config.gesture_smoothing_samples = 8;
        let mut engine = SamplerEngine::prepare(config, Some(instrument), metrics.clone());
        let events = [
            TimedEvent::at_sample(
                16,
                Event::NoteOn {
                    note: 60,
                    velocity: 0.8,
                    note_id: 9184,
                },
            ),
            TimedEvent::at_sample(
                32,
                Event::Gesture {
                    note_id: 9184,
                    control: GestureControl::BowPressure,
                    value: 0.9,
                },
            ),
            TimedEvent::at_sample(
                48,
                Event::Pitch {
                    note_id: 9184,
                    hz: 523.251_1,
                },
            ),
        ];
        let mut output = [0.0_f32; 128];
        engine.process(&mut output, 1, &events);
        assert!(output[..16].iter().all(|sample| *sample == 0.0));
        assert!(output[16..].iter().any(|sample| sample.abs() > 1e-7));
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert_eq!(metrics.snapshot().peak_voices, 1);
    }

    #[test]
    fn physical_render_does_not_allocate_after_prepare() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let instrument = RuntimeInstrument::bowed_string(
            "generic bowed string",
            solfege_core::BowedStringConfig::default(),
        );
        let mut engine = SamplerEngine::new(
            EngineConfig::realtime(rate),
            Some(instrument),
            Arc::new(SharedMetrics::default()),
        );
        let note = [TimedEvent::immediate(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 1,
        })];
        let mut output = [0.0_f32; 128];
        engine.process(&mut output, 1, &note);
        let gesture = [TimedEvent::immediate(Event::Gesture {
            note_id: 1,
            control: GestureControl::VibratoDepth,
            value: 0.5,
        })];
        let allocations = crate::allocation_probe::count(|| {
            engine.process(&mut output, 1, &gesture);
        });
        assert_eq!(allocations, 0);
    }

    #[test]
    fn non_finite_physical_controls_cannot_poison_output() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let instrument = RuntimeInstrument::bowed_string(
            "generic bowed string",
            solfege_core::BowedStringConfig::default(),
        );
        let mut engine = SamplerEngine::new(
            EngineConfig::realtime(rate),
            Some(instrument),
            Arc::new(SharedMetrics::default()),
        );
        let events = [
            TimedEvent::immediate(Event::NoteOn {
                note: 60,
                velocity: 0.8,
                note_id: 4,
            }),
            TimedEvent::at_sample(
                4,
                Event::Gesture {
                    note_id: 4,
                    control: GestureControl::BowVelocity,
                    value: f32::NAN,
                },
            ),
            TimedEvent::at_sample(
                8,
                Event::Pitch {
                    note_id: 4,
                    hz: f32::INFINITY,
                },
            ),
        ];
        let mut output = [0.0_f32; 128];
        engine.process(&mut output, 1, &events);
        assert!(output.iter().all(|sample| sample.is_finite()));
    }

    #[test]
    fn physical_render_is_block_size_invariant() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let make_engine = || {
            SamplerEngine::new(
                EngineConfig::realtime(rate),
                Some(RuntimeInstrument::bowed_string(
                    "generic bowed string",
                    solfege_core::BowedStringConfig::default(),
                )),
                Arc::new(SharedMetrics::default()),
            )
        };
        let note = [TimedEvent::immediate(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 1,
        })];
        let mut whole = [0.0_f32; 512];
        let mut blocks = [0.0_f32; 512];
        let mut first = make_engine();
        first.process(&mut whole, 1, &note);
        let mut second = make_engine();
        second.process(&mut blocks[..64], 1, &note);
        for block in blocks[64..].chunks_exact_mut(64) {
            second.process(block, 1, &[]);
        }
        assert_eq!(whole, blocks);
    }

    /// The voicebank must render the same samples however the host chops the
    /// stream up. There was no such test for this path, only for the physical
    /// one, and the voicebank is what the DAW actually plays.
    #[test]
    fn voicebank_render_is_block_size_invariant() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let make_engine = || {
            SamplerEngine::prepare_sfm(
                EngineConfig::realtime(rate),
                test_voicebank_sfm(),
                Arc::new(SharedMetrics::default()),
                sfm::SfmMode::VoicebankOnly,
            )
            .unwrap()
        };
        let note = [TimedEvent::immediate(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 7,
        })];
        let mut whole = [0.0_f32; 1024];
        let mut first = make_engine();
        first.process(&mut whole, 2, &note);

        for block_frames in [16_usize, 32, 64, 128] {
            let mut chunked = [0.0_f32; 1024];
            let mut engine = make_engine();
            let stride = block_frames * 2;
            let (head, tail) = chunked.split_at_mut(stride);
            engine.process(head, 2, &note);
            for block in tail.chunks_mut(stride) {
                engine.process(block, 2, &[]);
            }
            assert_eq!(
                whole, chunked,
                "voicebank output changed at {block_frames}-frame blocks"
            );
        }
    }

    /// A note that stops part-way through must produce the same stream at
    /// every block size, including the silence after it and anything that
    /// starts again later.
    ///
    /// This is the partition property the residual's old silent-block reset
    /// broke: it keyed "is the instrument idle" off whether a particular
    /// buffer happened to be all zeros, which is a fact about the host's block
    /// boundaries and not about the notes. The note-off here is placed at a
    /// fixed sample, so every block size renders the same music.
    #[test]
    fn note_lifecycle_render_is_block_size_invariant() {
        const FRAMES: usize = 1024;
        const NOTE_OFF_FRAME: usize = 300;

        let rate = SampleRate::new(48_000.0).unwrap();
        let render = |block_frames: usize| -> Vec<f32> {
            let mut engine = SamplerEngine::prepare_sfm(
                EngineConfig::realtime(rate),
                test_voicebank_sfm(),
                Arc::new(SharedMetrics::default()),
                sfm::SfmMode::VoicebankOnly,
            )
            .unwrap();
            let mut out = vec![0.0_f32; FRAMES * 2];
            let mut frame = 0;
            while frame < FRAMES {
                let frames_now = block_frames.min(FRAMES - frame);
                let mut events: Vec<TimedEvent> = Vec::new();
                if frame == 0 {
                    events.push(TimedEvent::immediate(Event::NoteOn {
                        note: 60,
                        velocity: 0.8,
                        note_id: 11,
                    }));
                }
                if (frame..frame + frames_now).contains(&NOTE_OFF_FRAME) {
                    events.push(TimedEvent::at_sample(
                        (NOTE_OFF_FRAME - frame) as u32,
                        Event::NoteOff {
                            note: 60,
                            velocity: 0.0,
                            note_id: 11,
                        },
                    ));
                }
                let start = frame * 2;
                let end = (frame + frames_now) * 2;
                engine.process(&mut out[start..end], 2, &events);
                frame += frames_now;
            }
            out
        };

        let reference = render(FRAMES);
        for block_frames in [16_usize, 32, 64, 128, 256] {
            assert_eq!(
                reference,
                render(block_frames),
                "output diverged at {block_frames}-frame blocks"
            );
        }
    }

    #[test]
    fn prepared_sfm_voicebank_renders_and_reports_voice_activity() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let metrics = Arc::new(SharedMetrics::default());
        let mut engine = SamplerEngine::prepare_sfm(
            EngineConfig::realtime(rate),
            test_voicebank_sfm(),
            metrics.clone(),
            sfm::SfmMode::VoicebankOnly,
        )
        .unwrap();
        engine.handle_event(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 42,
        });
        let mut output = [0.0_f32; 32];
        engine.process_interleaved(&mut output, 1, &[]);
        assert!(output.iter().any(|sample| sample.abs() > 1.0e-5));
        assert_eq!(metrics.snapshot().active_voices, 1);
        assert_eq!(metrics.snapshot().peak_voices, 1);
    }

    #[test]
    fn prepared_sfm_stereo_render_preserves_voicebank_channels() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let mut engine = SamplerEngine::prepare_sfm(
            EngineConfig::realtime(rate),
            test_voicebank_sfm(),
            Arc::new(SharedMetrics::default()),
            sfm::SfmMode::VoicebankOnly,
        )
        .unwrap();
        engine.handle_event(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 42,
        });
        let mut left = [0.0_f32; 32];
        let mut right = [0.0_f32; 32];
        engine.process_stereo(&mut left, &mut right, &[]);
        assert!(left.iter().any(|sample| sample.abs() > 1.0e-5));
        assert!(right.iter().any(|sample| sample.abs() > 1.0e-5));
        assert!(
            left.iter()
                .zip(right)
                .any(|(left, right)| (left - right).abs() > 1.0e-5)
        );
    }

    #[test]
    fn prepared_sfm_steady_state_render_does_not_allocate() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let mut engine = SamplerEngine::prepare_sfm(
            EngineConfig::realtime(rate),
            test_voicebank_sfm(),
            Arc::new(SharedMetrics::default()),
            sfm::SfmMode::VoicebankOnly,
        )
        .unwrap();
        engine.handle_event(Event::NoteOn {
            note: 60,
            velocity: 0.8,
            note_id: 42,
        });
        let mut output = [0.0_f32; 64];
        engine.process_interleaved(&mut output, 1, &[]);
        let allocations = crate::allocation_probe::count(|| {
            engine.process_interleaved(&mut output, 1, &[]);
        });
        assert_eq!(allocations, 0);
    }

    #[test]
    fn steady_state_render_does_not_allocate() {
        let rate = SampleRate::new(48_000.0).unwrap();
        let mut engine = SamplerEngine::new(
            EngineConfig::realtime(rate),
            Some(test_instrument()),
            Arc::new(SharedMetrics::default()),
        );
        let events = [TimedEvent::immediate(Event::NoteOn {
            note: 60,
            velocity: 1.0,
            note_id: 7,
        })];
        let mut output = [0.0_f32; 12];
        engine.process_interleaved(&mut output, 2, &events);

        let allocations = crate::allocation_probe::count(|| {
            engine.process_interleaved(&mut output, 2, &events);
        });
        assert_eq!(allocations, 0);
    }
}
