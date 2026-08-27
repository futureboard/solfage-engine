//! Host-independent sampler runtime.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use solfege_audio::SampleRate;
use solfege_core::{GestureControl, PhysicalModel, RuntimeInstrument};
use solfege_event::{Event, TimedEvent};
use solfege_voice::{EnvelopeConfig, VoicePool};
use solfege_zone::TriggerMode;

#[cfg(feature = "fbmx")]
pub mod fbmx;

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
        }
    }

    pub fn record_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
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
            voices: VoicePool::with_physical_config(
                config.polyphony,
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

    pub fn metrics(&self) -> &Arc<SharedMetrics> {
        &self.metrics
    }

    /// Prepared voice/DSP working memory. Mapped sample bytes are reported
    /// separately through `SharedMetrics::snapshot().mapped_bytes`.
    pub fn working_memory_bytes(&self) -> usize {
        self.voices.memory_bytes()
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
        match event {
            Event::NoteOn {
                note,
                velocity,
                note_id,
            } => {
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
                    if instrument.model.zones[zone_index].matches(
                        note,
                        velocity_midi,
                        TriggerMode::Attack,
                    ) {
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
            Event::Articulation { articulation, .. } =>
            {
                #[cfg(feature = "fbmx")]
                if let Some(hooks) = self.fbmx.as_mut() {
                    hooks.set_residual_articulation(articulation);
                }
            }
            Event::Parameter { .. } | Event::Transport { .. } => {}
            Event::Sostenuto(_) => {}
        }
    }

    /// Host-neutral render entry point. `events` are sorted by sample offset
    /// within this caller-owned block; the interleaved buffer is cleared and
    /// filled in place.
    pub fn process(&mut self, output: &mut [f32], channels: usize, events: &[TimedEvent]) {
        self.process_interleaved(output, channels, events);
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
        for frame in 0..frames {
            while let Some(event) = events.get(event_index) {
                if event.sample_offset() as usize > frame {
                    break;
                }
                self.handle_event(event.event);
                event_index += 1;
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
        }
        #[cfg(feature = "fbmx")]
        if let Some(hooks) = self.fbmx.as_mut() {
            hooks.apply_residual(output);
        }
        for sample in output.iter_mut() {
            *sample *= self.master_gain;
        }
        self.publish_metrics(output, frames, started);
    }

    fn publish_metrics(&self, output: &[f32], frames: usize, started: Instant) {
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
        self.metrics
            .active_voices
            .store(self.voices.active_count(), Ordering::Relaxed);
        self.metrics
            .peak_voices
            .store(self.voices.peak_active(), Ordering::Relaxed);
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
