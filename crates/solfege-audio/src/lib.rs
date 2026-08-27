//! Host-neutral audio primitives. Raw numbers are kept at I/O boundaries.

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SampleRate(f32);

impl SampleRate {
    pub fn new(hz: f32) -> Result<Self, AudioError> {
        if hz.is_finite() && (1.0..=768_000.0).contains(&hz) {
            Ok(Self(hz))
        } else {
            Err(AudioError::InvalidSampleRate(hz))
        }
    }

    pub const fn get(self) -> f32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FrameCount(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelCount(u16);

impl ChannelCount {
    pub fn new(channels: u16) -> Result<Self, AudioError> {
        if channels == 0 || channels > 64 {
            Err(AudioError::InvalidChannelCount(channels))
        } else {
            Ok(Self(channels))
        }
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tempo(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeSignature {
    pub numerator: u8,
    pub denominator: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SamplePosition(pub i64);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransportState {
    pub playing: bool,
    pub recording: bool,
    pub tempo: Tempo,
    pub time_signature: TimeSignature,
    pub sample_position: SamplePosition,
}

impl Default for TransportState {
    fn default() -> Self {
        Self {
            playing: false,
            recording: false,
            tempo: Tempo(120.0),
            time_signature: TimeSignature {
                numerator: 4,
                denominator: 4,
            },
            sample_position: SamplePosition(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ProcessContext {
    pub sample_rate: SampleRate,
    pub frames: FrameCount,
    pub transport: TransportState,
}

/// A non-owning interleaved output block.
pub struct AudioBlockMut<'a> {
    samples: &'a mut [f32],
    channels: ChannelCount,
}

impl<'a> AudioBlockMut<'a> {
    pub fn new(samples: &'a mut [f32], channels: ChannelCount) -> Result<Self, AudioError> {
        if !samples.len().is_multiple_of(channels.get() as usize) {
            return Err(AudioError::MisalignedBlock {
                samples: samples.len(),
                channels: channels.get(),
            });
        }
        Ok(Self { samples, channels })
    }

    pub fn clear(&mut self) {
        self.samples.fill(0.0);
    }

    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }

    pub fn channels(&self) -> ChannelCount {
        self.channels
    }

    pub fn frames(&self) -> FrameCount {
        FrameCount(self.samples.len() / self.channels.get() as usize)
    }
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("invalid sample rate: {0}")]
    InvalidSampleRate(f32),
    #[error("invalid channel count: {0}")]
    InvalidChannelCount(u16),
    #[error("{samples} samples are not divisible by {channels} channels")]
    MisalignedBlock { samples: usize, channels: u16 },
}
