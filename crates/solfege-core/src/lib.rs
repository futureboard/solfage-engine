//! Immutable runtime instrument state prepared outside the audio callback.

use std::{path::Path, sync::Arc};

use solfege_storage::{
    AccessHint, MappedStorage, PcmFormat, SampleStorage, StorageError, WavLayout, parse_wav,
};
use solfege_zone::{Instrument, Zone, ZoneError, ZoneIndex};
use thiserror::Error;

/// A bounded set of instrument-specific gesture channels. The standard
/// fields below cover the controls shared by most instruments; named
/// instrument controls are mapped into this fixed-capacity extension bank so
/// the realtime representation never needs a per-instrument allocation.
pub const INSTRUMENT_GESTURE_CHANNELS: usize = 16;
pub const GESTURE_STATE_VERSION: u32 = 1;
pub const PHYSICAL_INSTRUMENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GestureControl {
    Pressure,
    Velocity,
    Position,
    Expression,
    VibratoDepth,
    VibratoRate,
    Attack,
    Release,
    BowPressure,
    BowVelocity,
    BowPosition,
    BowDirection,
    BreathPressure,
    Embouchure,
    ReedPressure,
    LipTension,
    FingerPosition,
    FingerPressure,
    PluckPosition,
    MalletForce,
    ContinuousPitch,
    Ornament(u8),
}

/// Continuous performance state consumed by synthesis backends.
///
/// It deliberately contains no MIDI controller numbers and keeps pitch in Hz
/// rather than quantising it to a note grid. Host adapters translate MIDI,
/// MPE, or native performance messages into this semantic state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureState {
    pub pitch_hz: f32,
    pub pressure: f32,
    pub velocity: f32,
    pub position: f32,
    pub expression: f32,
    pub vibrato_depth: f32,
    pub vibrato_rate: f32,
    pub attack: f32,
    pub release: f32,
    pub instrument: [f32; INSTRUMENT_GESTURE_CHANNELS],
}

impl Default for GestureState {
    fn default() -> Self {
        Self {
            pitch_hz: 440.0,
            pressure: 0.0,
            velocity: 0.0,
            position: 0.5,
            expression: 1.0,
            vibrato_depth: 0.0,
            vibrato_rate: 5.0,
            attack: 0.0,
            release: 0.0,
            instrument: [0.0; INSTRUMENT_GESTURE_CHANNELS],
        }
    }
}

impl GestureState {
    pub fn for_note(note: u8, velocity: f32) -> Self {
        let velocity = finite_unit(velocity);
        let mut state = Self {
            pitch_hz: midi_note_to_hz(note),
            velocity: velocity.clamp(0.0, 1.0),
            pressure: velocity.clamp(0.0, 1.0),
            attack: velocity.clamp(0.0, 1.0),
            ..Self::default()
        };
        state.set(GestureControl::BowPressure, state.pressure);
        state.set(GestureControl::BowVelocity, state.velocity);
        state.set(GestureControl::BowPosition, state.position);
        state
    }

    pub fn sanitized(self) -> Self {
        let mut result = self;
        result.pitch_hz = if self.pitch_hz.is_finite() {
            self.pitch_hz.max(0.0)
        } else {
            0.0
        };
        result.pressure = finite_unit(self.pressure);
        result.velocity = finite_unit(self.velocity);
        result.position = finite_unit(self.position);
        result.expression = finite_unit(self.expression);
        result.vibrato_depth = finite_unit(self.vibrato_depth);
        result.vibrato_rate = if self.vibrato_rate.is_finite() {
            self.vibrato_rate.max(0.0)
        } else {
            0.0
        };
        result.attack = finite_unit(self.attack);
        result.release = finite_unit(self.release);
        for value in &mut result.instrument {
            *value = finite_unit(*value);
        }
        result
    }

    pub fn get(&self, control: GestureControl) -> f32 {
        match control {
            GestureControl::Pressure => self.pressure,
            GestureControl::Velocity => self.velocity,
            GestureControl::Position => self.position,
            GestureControl::Expression => self.expression,
            GestureControl::VibratoDepth => self.vibrato_depth,
            GestureControl::VibratoRate => self.vibrato_rate,
            GestureControl::Attack => self.attack,
            GestureControl::Release => self.release,
            GestureControl::ContinuousPitch => self.pitch_hz,
            GestureControl::BowPressure
            | GestureControl::BowVelocity
            | GestureControl::BowPosition
            | GestureControl::BowDirection
            | GestureControl::BreathPressure
            | GestureControl::Embouchure
            | GestureControl::ReedPressure
            | GestureControl::LipTension
            | GestureControl::FingerPosition
            | GestureControl::FingerPressure
            | GestureControl::PluckPosition
            | GestureControl::MalletForce
            | GestureControl::Ornament(_) => self.instrument[control.instrument_index()],
        }
    }

    pub fn set(&mut self, control: GestureControl, value: f32) {
        let value = if value.is_finite() { value } else { 0.0 };
        match control {
            GestureControl::Pressure => self.pressure = value.clamp(0.0, 1.0),
            GestureControl::Velocity => self.velocity = value.clamp(0.0, 1.0),
            GestureControl::Position => self.position = value.clamp(0.0, 1.0),
            GestureControl::Expression => self.expression = value.clamp(0.0, 1.0),
            GestureControl::VibratoDepth => self.vibrato_depth = value.clamp(0.0, 1.0),
            GestureControl::VibratoRate => self.vibrato_rate = value.max(0.0),
            GestureControl::Attack => self.attack = value.clamp(0.0, 1.0),
            GestureControl::Release => self.release = value.clamp(0.0, 1.0),
            GestureControl::ContinuousPitch => self.pitch_hz = value.max(0.0),
            GestureControl::BowPressure
            | GestureControl::BowVelocity
            | GestureControl::BowPosition
            | GestureControl::BowDirection
            | GestureControl::BreathPressure
            | GestureControl::Embouchure
            | GestureControl::ReedPressure
            | GestureControl::LipTension
            | GestureControl::FingerPosition
            | GestureControl::FingerPressure
            | GestureControl::PluckPosition
            | GestureControl::MalletForce
            | GestureControl::Ornament(_) => {
                self.instrument[control.instrument_index()] = value.clamp(0.0, 1.0)
            }
        }
    }
}

impl GestureControl {
    const fn instrument_index(self) -> usize {
        match self {
            Self::BowPressure => 0,
            Self::BowVelocity => 1,
            Self::BowPosition => 2,
            Self::BowDirection => 3,
            Self::BreathPressure => 4,
            Self::Embouchure => 5,
            Self::ReedPressure => 6,
            Self::LipTension => 7,
            Self::FingerPosition => 8,
            Self::FingerPressure => 9,
            Self::PluckPosition => 10,
            Self::MalletForce => 11,
            Self::Ornament(index) => 12 + index as usize % 4,
            _ => 0,
        }
    }
}

fn finite_unit(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The MIDI-note conversion is an adapter convenience, not an internal pitch
/// representation. Physical backends consume the resulting continuous Hz
/// value and can subsequently slide or retune it without quantisation.
pub fn midi_note_to_hz(note: u8) -> f32 {
    440.0 * 2.0_f32.powf((note as f32 - 69.0) / 12.0)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyMode {
    pub frequency_hz: f32,
    pub decay_seconds: f32,
    pub gain: f32,
}

impl BodyMode {
    pub const fn new(frequency_hz: f32, decay_seconds: f32, gain: f32) -> Self {
        Self {
            frequency_hz,
            decay_seconds,
            gain,
        }
    }

    fn sanitized(self, sample_rate: f32) -> Self {
        Self {
            frequency_hz: if self.frequency_hz.is_finite() {
                self.frequency_hz.clamp(1.0, (sample_rate * 0.45).max(1.0))
            } else {
                220.0
            },
            decay_seconds: if self.decay_seconds.is_finite() {
                self.decay_seconds.max(0.001)
            } else {
                0.2
            },
            gain: if self.gain.is_finite() {
                self.gain.clamp(-1.0, 1.0)
            } else {
                0.0
            },
        }
    }
}

/// Parameters for the experimental generic bowed-string backend.
///
/// This is a controllable digital waveguide approximation, not an exact
/// solution of string, bridge, or body mechanics. The fields are intentionally
/// instrument-neutral so a future violin, saw, or other bowed string can own
/// its tuning and body data without changing the DSP interfaces.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BowedStringConfig {
    pub min_frequency_hz: f32,
    pub max_frequency_hz: f32,
    pub string_decay: f32,
    pub bow_friction: f32,
    pub bow_stiffness: f32,
    pub bridge_coupling: f32,
    pub body_mix: f32,
    pub noise_amount: f32,
    pub radiation_damping: f32,
    pub body_modes: [BodyMode; 4],
}

impl BowedStringConfig {
    pub fn sanitized(self, sample_rate: f32) -> Self {
        let sample_rate = if sample_rate.is_finite() {
            sample_rate.max(1.0)
        } else {
            48_000.0
        };
        let min_frequency_hz = if self.min_frequency_hz.is_finite() {
            self.min_frequency_hz.max(1.0)
        } else {
            20.0
        };
        let max_frequency_hz = if self.max_frequency_hz.is_finite() {
            self.max_frequency_hz.max(min_frequency_hz)
        } else {
            2_000.0_f32.max(min_frequency_hz)
        };
        Self {
            min_frequency_hz,
            max_frequency_hz,
            string_decay: if self.string_decay.is_finite() {
                self.string_decay.clamp(0.0, 0.99999)
            } else {
                0.9995
            },
            bow_friction: if self.bow_friction.is_finite() {
                self.bow_friction.max(0.0)
            } else {
                4.0
            },
            bow_stiffness: if self.bow_stiffness.is_finite() {
                self.bow_stiffness.max(0.01)
            } else {
                2.5
            },
            bridge_coupling: if self.bridge_coupling.is_finite() {
                self.bridge_coupling.clamp(0.0, 1.0)
            } else {
                0.35
            },
            body_mix: if self.body_mix.is_finite() {
                self.body_mix.clamp(0.0, 1.0)
            } else {
                0.35
            },
            noise_amount: if self.noise_amount.is_finite() {
                self.noise_amount.clamp(0.0, 1.0)
            } else {
                0.015
            },
            radiation_damping: if self.radiation_damping.is_finite() {
                self.radiation_damping.clamp(0.0, 0.99999)
            } else {
                0.995
            },
            body_modes: self.body_modes.map(|mode| mode.sanitized(sample_rate)),
        }
    }
}

impl Default for BowedStringConfig {
    fn default() -> Self {
        Self {
            min_frequency_hz: 20.0,
            max_frequency_hz: 2_000.0,
            string_decay: 0.9995,
            bow_friction: 4.0,
            bow_stiffness: 2.5,
            bridge_coupling: 0.35,
            body_mix: 0.35,
            noise_amount: 0.015,
            radiation_damping: 0.995,
            body_modes: [
                BodyMode::new(220.0, 0.35, 0.55),
                BodyMode::new(440.0, 0.25, 0.35),
                BodyMode::new(710.0, 0.18, 0.22),
                BodyMode::new(1_120.0, 0.12, 0.12),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PhysicalModel {
    BowedString(BowedStringConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SynthesisType {
    Sample,
    Physical,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SynthesisBackend {
    Sample,
    Physical(PhysicalModel),
    Hybrid(PhysicalModel),
}

#[derive(Clone)]
pub struct PreparedSample {
    storage: Arc<dyn SampleStorage>,
    layout: WavLayout,
}

impl PreparedSample {
    pub fn new(storage: Arc<dyn SampleStorage>, layout: WavLayout) -> Result<Self, CoreError> {
        let data_end = layout
            .data_offset
            .checked_add(layout.data_len)
            .ok_or(CoreError::InvalidSample("PCM extent overflows u64"))?;
        if data_end > storage.len() {
            return Err(CoreError::InvalidSample("PCM extent exceeds storage"));
        }
        Ok(Self { storage, layout })
    }

    pub const fn layout(&self) -> WavLayout {
        self.layout
    }

    pub fn mapped_bytes(&self) -> u64 {
        self.storage.len()
    }

    /// Converts one stored PCM value into the engine's f32 processing domain.
    /// It never allocates and never performs I/O itself; mapped pages must have
    /// been made resident by control-side preparation.
    #[inline]
    pub fn sample_f32(&self, frame: u64, channel: u16) -> f32 {
        if frame >= self.layout.frames || channel >= self.layout.channels {
            return 0.0;
        }
        let bytes_per_sample = self.layout.format.bytes_per_sample();
        let Some(frame_offset) = frame.checked_mul(self.layout.block_align as u64) else {
            return 0.0;
        };
        let Some(channel_offset) = (channel as u64).checked_mul(bytes_per_sample as u64) else {
            return 0.0;
        };
        let Some(offset) = self
            .layout
            .data_offset
            .checked_add(frame_offset)
            .and_then(|value| value.checked_add(channel_offset))
        else {
            return 0.0;
        };
        let Ok(view) = self.storage.view(offset, bytes_per_sample) else {
            return 0.0;
        };
        decode_pcm(self.layout.format, view.as_bytes())
    }

    pub fn prefault_attack(&self, bytes: usize) -> Result<(), CoreError> {
        let requested = u64::try_from(bytes).unwrap_or(u64::MAX);
        let len = requested.min(self.layout.data_len);
        let end = self
            .layout
            .data_offset
            .checked_add(len)
            .ok_or(CoreError::InvalidSample("attack range overflow"))?;
        self.storage
            .advise(self.layout.data_offset..end, AccessHint::WillNeed);
        self.storage.prefault(self.layout.data_offset..end)?;
        Ok(())
    }
}

#[inline]
fn decode_pcm(format: PcmFormat, bytes: &[u8]) -> f32 {
    match format {
        PcmFormat::Signed16 => i16::from_le_bytes([bytes[0], bytes[1]]) as f32 / 32_768.0,
        PcmFormat::Signed24 => {
            let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
            let signed = (raw << 8) >> 8;
            signed as f32 / 8_388_608.0
        }
        PcmFormat::Signed32 => {
            i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f32 / 2_147_483_648.0
        }
        PcmFormat::Float32 => f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
        PcmFormat::Float64 => f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]) as f32,
    }
}

#[derive(Clone)]
pub struct RuntimeInstrument {
    pub model: Instrument,
    pub zone_index: ZoneIndex,
    pub samples: Vec<PreparedSample>,
    pub backend: SynthesisBackend,
}

impl RuntimeInstrument {
    pub fn prepare(model: Instrument, samples: Vec<PreparedSample>) -> Result<Self, CoreError> {
        for zone in &model.zones {
            let Some(sample) = samples.get(zone.sample_index) else {
                return Err(CoreError::MissingSample(zone.sample_index));
            };
            if zone.start_frame >= sample.layout.frames {
                return Err(CoreError::InvalidSample("zone start is outside sample"));
            }
            if zone
                .end_frame
                .is_some_and(|end| end <= zone.start_frame || end > sample.layout.frames)
            {
                return Err(CoreError::InvalidSample("invalid zone end"));
            }
        }
        let zone_index = ZoneIndex::build(&model.zones)?;
        Ok(Self {
            model,
            zone_index,
            samples,
            backend: SynthesisBackend::Sample,
        })
    }

    /// Prepare a physical instrument without introducing a second runtime
    /// instrument type. Sample zones remain available on the same prepared
    /// object for a future hybrid backend.
    pub fn prepare_physical(
        model: Instrument,
        physical_model: PhysicalModel,
    ) -> Result<Self, CoreError> {
        let zone_index = ZoneIndex::build(&model.zones)?;
        Ok(Self {
            model,
            zone_index,
            samples: Vec::new(),
            backend: SynthesisBackend::Physical(physical_model),
        })
    }

    pub fn prepare_hybrid(
        model: Instrument,
        physical_model: PhysicalModel,
        samples: Vec<PreparedSample>,
    ) -> Result<Self, CoreError> {
        for zone in &model.zones {
            if samples.get(zone.sample_index).is_none() {
                return Err(CoreError::MissingSample(zone.sample_index));
            }
        }
        let zone_index = ZoneIndex::build(&model.zones)?;
        Ok(Self {
            model,
            zone_index,
            samples,
            backend: SynthesisBackend::Hybrid(physical_model),
        })
    }

    pub fn bowed_string(name: impl Into<String>, config: BowedStringConfig) -> Self {
        Self::prepare_physical(
            Instrument {
                name: name.into(),
                groups: Vec::new(),
                zones: Vec::new(),
            },
            PhysicalModel::BowedString(config),
        )
        .expect("an empty physical instrument always has a valid zone index")
    }

    pub const fn is_physical(&self) -> bool {
        matches!(
            self.backend,
            SynthesisBackend::Physical(_) | SynthesisBackend::Hybrid(_)
        )
    }

    pub const fn synthesis_type(&self) -> SynthesisType {
        match self.backend {
            SynthesisBackend::Sample => SynthesisType::Sample,
            SynthesisBackend::Physical(_) => SynthesisType::Physical,
            SynthesisBackend::Hybrid(_) => SynthesisType::Hybrid,
        }
    }

    pub const fn physical_model(&self) -> Option<PhysicalModel> {
        match self.backend {
            SynthesisBackend::Sample => None,
            SynthesisBackend::Physical(model) | SynthesisBackend::Hybrid(model) => Some(model),
        }
    }

    pub fn from_mapped_wav(
        path: impl AsRef<Path>,
        root_key: u8,
        attack_residency_bytes: usize,
    ) -> Result<Self, CoreError> {
        let storage: Arc<dyn SampleStorage> = Arc::new(MappedStorage::open(path.as_ref())?);
        let layout = parse_wav(storage.as_ref())?;
        let sample = PreparedSample::new(storage, layout)?;
        sample.prefault_attack(attack_residency_bytes)?;
        let name = path
            .as_ref()
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("Mapped sample")
            .to_owned();
        let zone = Zone::mapped_sample(0, root_key.min(127));
        Self::prepare(
            Instrument {
                name,
                groups: Vec::new(),
                zones: vec![zone],
            },
            vec![sample],
        )
    }

    pub fn mapped_bytes(&self) -> u64 {
        self.samples.iter().map(PreparedSample::mapped_bytes).sum()
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Zone(#[from] ZoneError),
    #[error("instrument references missing sample index {0}")]
    MissingSample(usize),
    #[error("invalid prepared sample: {0}")]
    InvalidSample(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use solfege_storage::PreloadedStorage;

    #[test]
    fn signed_24_conversion_sign_extends() {
        assert!((decode_pcm(PcmFormat::Signed24, &[0xff, 0xff, 0x7f]) - 0.999_999_9).abs() < 1e-6);
        assert_eq!(decode_pcm(PcmFormat::Signed24, &[0x00, 0x00, 0x80]), -1.0);
    }

    #[test]
    fn reads_interleaved_pcm_without_an_intermediate_buffer() {
        let bytes: Arc<[u8]> = vec![0x00, 0x80, 0xff, 0x7f].into();
        let storage: Arc<dyn SampleStorage> = Arc::new(PreloadedStorage::from_bytes(bytes));
        let sample = PreparedSample::new(
            storage,
            WavLayout {
                sample_rate: 48_000,
                channels: 1,
                format: PcmFormat::Signed16,
                data_offset: 0,
                data_len: 4,
                frames: 2,
                block_align: 2,
            },
        )
        .unwrap();
        assert_eq!(sample.sample_f32(0, 0), -1.0);
        assert!(sample.sample_f32(1, 0) > 0.99);
    }

    #[test]
    fn gesture_state_uses_semantic_controls_and_continuous_pitch() {
        let mut gesture = GestureState::for_note(60, 0.5);
        assert!((gesture.pitch_hz - 261.625_55).abs() < 0.001);
        gesture.set(GestureControl::BowPressure, 0.75);
        gesture.set(GestureControl::ContinuousPitch, 300.0);
        assert_eq!(gesture.get(GestureControl::BowPressure), 0.75);
        assert_eq!(gesture.pitch_hz, 300.0);
    }

    #[test]
    fn physical_instruments_share_runtime_instrument_representation() {
        let instrument = RuntimeInstrument::bowed_string("bowed", BowedStringConfig::default());
        assert!(instrument.is_physical());
        assert_eq!(instrument.synthesis_type(), SynthesisType::Physical);
        assert!(instrument.samples.is_empty());
    }
}
