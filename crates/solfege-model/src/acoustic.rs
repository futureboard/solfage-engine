//! Checked binary ACOU/AUDO assets extracted from real instrument recordings.
//!
//! The format is intentionally independent of WAV containers.  ACOU contains
//! measured profile records; AUDO contains the fixed-rate PCM segments that a
//! realtime renderer can read without opening files.  Parsing copies the
//! payload into owned vectors on the control thread.  The renderer crate then
//! uses these immutable vectors from its callback-safe state.

use crate::{Result, SfmError};

pub const ACOUSTIC_MAGIC: [u8; 4] = *b"ACU1";
pub const AUDIO_MAGIC: [u8; 4] = *b"AUO1";
pub const ASSET_VERSION: u16 = 1;
pub const ACOUSTIC_HEADER_SIZE: usize = 64;
pub const ACOUSTIC_PROFILE_SIZE: usize = 396;
pub const ACOUSTIC_HARMONIC_BINS: usize = 64;
pub const ACOUSTIC_BODY_MODES: usize = 8;
pub const AUDIO_HEADER_SIZE: usize = 64;
pub const AUDIO_DESCRIPTOR_SIZE: usize = 48;
pub const MAX_PROFILES: usize = 64;
pub const MAX_AUDIO_SEGMENTS: usize = 512;
pub const MAX_AUDIO_SAMPLES: usize = 64 * 1024 * 1024;

pub const ARTICULATION_SUSTAIN_VIBRATO: u8 = 0;
pub const ARTICULATION_PIZZICATO: u8 = 1;
pub const ARTICULATION_SPICCATO: u8 = 2;
pub const ARTICULATION_TREMOLO: u8 = 3;

pub const DYNAMIC_P: u8 = 0;
pub const DYNAMIC_F: u8 = 1;
pub const DYNAMIC_V1: u8 = 2;
pub const DYNAMIC_V2: u8 = 3;

pub const SEGMENT_ATTACK: u8 = 0;
pub const SEGMENT_RELEASE: u8 = 1;
pub const SEGMENT_BOW_NOISE: u8 = 2;
pub const SEGMENT_BODY: u8 = 3;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcousticBodyMode {
    pub frequency_hz: f32,
    pub gain: f32,
    pub decay_seconds: f32,
}

impl Default for AcousticBodyMode {
    fn default() -> Self {
        Self {
            frequency_hz: 0.0,
            gain: 0.0,
            decay_seconds: 0.1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AcousticProfile {
    pub articulation: u8,
    pub dynamic: u8,
    pub midi_note_mean: f32,
    pub fundamental_hz: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub sustain_rms: f32,
    pub noise_ratio: f32,
    pub spectral_centroid_hz: f32,
    pub spectral_flatness: f32,
    pub body_rms: f32,
    pub peak: f32,
    pub harmonics: [f32; ACOUSTIC_HARMONIC_BINS],
    pub body_modes: [AcousticBodyMode; ACOUSTIC_BODY_MODES],
    pub record_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioSegment {
    pub kind: u8,
    pub articulation: u8,
    pub dynamic: u8,
    pub midi_note: i16,
    pub source_file_index: u32,
    pub source_start_frame: u64,
    pub frame_count: u32,
    pub sample_offset: usize,
    pub source_sample_rate: u32,
    pub gain: f32,
    pub rms: f32,
    pub peak: f32,
}

#[derive(Debug, Clone)]
pub struct AcousticModel {
    sample_rate: u32,
    profiles: Vec<AcousticProfile>,
    segments: Vec<AudioSegment>,
    samples: Vec<i16>,
    source_file_count: u32,
    source_frame_count: u64,
}

impl AcousticModel {
    pub fn from_sections(acoustic: &[u8], audio: &[u8]) -> Result<Self> {
        let (sample_rate, profiles, source_file_count, source_frame_count) =
            parse_acoustic_profiles(acoustic)?;
        let (audio_sample_rate, segments, samples, audio_source_files, audio_source_frames) =
            parse_audio_segments(audio)?;
        if sample_rate != audio_sample_rate {
            return Err(SfmError::InvalidAcousticAsset(
                "ACOU and AUDO sample rates differ".to_owned(),
            ));
        }
        if source_file_count != audio_source_files || source_frame_count != audio_source_frames {
            return Err(SfmError::InvalidAcousticAsset(
                "ACOU and AUDO source totals differ".to_owned(),
            ));
        }
        if profiles.is_empty() || segments.is_empty() {
            return Err(SfmError::InvalidAcousticAsset(
                "acoustic assets must contain profiles and audio segments".to_owned(),
            ));
        }
        for segment in &segments {
            if segment.articulation >= 4
                || segment.dynamic >= 4
                || !matches!(
                    segment.kind,
                    SEGMENT_ATTACK | SEGMENT_RELEASE | SEGMENT_BOW_NOISE | SEGMENT_BODY
                )
            {
                return Err(SfmError::InvalidAcousticAsset(
                    "audio segment key is invalid".to_owned(),
                ));
            }
            let end = segment
                .sample_offset
                .checked_add(segment.frame_count as usize)
                .ok_or_else(|| {
                    SfmError::InvalidAcousticAsset("audio segment range overflows".to_owned())
                })?;
            if end > samples.len() {
                return Err(SfmError::InvalidAcousticAsset(
                    "audio segment range is outside AUDO samples".to_owned(),
                ));
            }
        }
        Ok(Self {
            sample_rate,
            profiles,
            segments,
            samples,
            source_file_count,
            source_frame_count,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn profiles(&self) -> &[AcousticProfile] {
        &self.profiles
    }

    pub fn segments(&self) -> &[AudioSegment] {
        &self.segments
    }

    pub fn samples(&self) -> &[i16] {
        &self.samples
    }

    pub fn source_file_count(&self) -> u32 {
        self.source_file_count
    }

    pub fn source_frame_count(&self) -> u64 {
        self.source_frame_count
    }

    pub fn profile_index(&self, articulation: u8, dynamic: u8) -> usize {
        self.profiles
            .iter()
            .position(|profile| profile.articulation == articulation && profile.dynamic == dynamic)
            .or_else(|| {
                self.profiles
                    .iter()
                    .position(|profile| profile.articulation == articulation)
            })
            .unwrap_or(0)
    }

    pub fn segment_index(&self, profile_index: usize, kind: u8) -> Option<usize> {
        let profile = self.profiles.get(profile_index)?;
        self.segments.iter().position(|segment| {
            segment.kind == kind
                && segment.articulation == profile.articulation
                && segment.dynamic == profile.dynamic
        })
    }

    pub fn segment_samples(&self, segment_index: usize) -> Option<&[i16]> {
        let segment = self.segments.get(segment_index)?;
        let end = segment
            .sample_offset
            .checked_add(segment.frame_count as usize)?;
        self.samples.get(segment.sample_offset..end)
    }
}

fn invalid(message: impl Into<String>) -> SfmError {
    SfmError::InvalidAcousticAsset(message.into())
}

fn parse_acoustic_profiles(raw: &[u8]) -> Result<(u32, Vec<AcousticProfile>, u32, u64)> {
    if raw.len() < ACOUSTIC_HEADER_SIZE {
        return Err(invalid("ACOU header is truncated"));
    }
    if raw[0..4] != ACOUSTIC_MAGIC {
        return Err(invalid("ACOU magic is invalid"));
    }
    if read_u16(raw, 4)? != ASSET_VERSION || read_u16(raw, 6)? as usize != ACOUSTIC_HEADER_SIZE {
        return Err(invalid("ACOU version or header size is unsupported"));
    }
    let sample_rate = read_u32(raw, 8)?;
    let profile_count = read_u32(raw, 12)? as usize;
    let harmonic_bins = read_u32(raw, 16)? as usize;
    let body_modes = read_u32(raw, 20)? as usize;
    let source_file_count = read_u32(raw, 24)?;
    let source_frame_count = read_u64(raw, 32)?;
    let profile_stride = read_u32(raw, 40)? as usize;
    if profile_count == 0 || profile_count > MAX_PROFILES {
        return Err(invalid("ACOU profile count exceeds bounds"));
    }
    if harmonic_bins != ACOUSTIC_HARMONIC_BINS
        || body_modes != ACOUSTIC_BODY_MODES
        || profile_stride != ACOUSTIC_PROFILE_SIZE
    {
        return Err(invalid("ACOU dimensions do not match v1"));
    }
    let payload_size = profile_count
        .checked_mul(profile_stride)
        .ok_or_else(|| invalid("ACOU profile payload overflows"))?;
    let end = ACOUSTIC_HEADER_SIZE
        .checked_add(payload_size)
        .ok_or_else(|| invalid("ACOU payload overflows"))?;
    if end != raw.len() {
        return Err(invalid("ACOU payload length does not match header"));
    }
    let mut profiles = Vec::with_capacity(profile_count);
    for index in 0..profile_count {
        let start = ACOUSTIC_HEADER_SIZE + index * profile_stride;
        let articulation = raw[start];
        let dynamic = raw[start + 1];
        let record_count = read_u16(raw, start + 2)? as u32;
        if articulation >= 4 || dynamic >= 4 {
            return Err(invalid("ACOU profile key is invalid"));
        }
        let mut scalar = [0.0_f32; 10];
        for (field, value) in scalar.iter_mut().enumerate() {
            *value = read_f32(raw, start + 4 + field * 4)?;
            if !value.is_finite() {
                return Err(invalid("ACOU profile contains NaN or infinity"));
            }
        }
        let mut harmonics = [0.0_f32; ACOUSTIC_HARMONIC_BINS];
        for (field, value) in harmonics.iter_mut().enumerate() {
            *value = read_f32(raw, start + 44 + field * 4)?;
            if !value.is_finite() || *value < 0.0 {
                return Err(invalid("ACOU harmonic profile is invalid"));
            }
        }
        let mut body_modes = [AcousticBodyMode::default(); ACOUSTIC_BODY_MODES];
        for (mode, value) in body_modes.iter_mut().enumerate() {
            let offset = start + 300 + mode * 12;
            value.frequency_hz = read_f32(raw, offset)?;
            value.gain = read_f32(raw, offset + 4)?;
            value.decay_seconds = read_f32(raw, offset + 8)?;
            if !value.frequency_hz.is_finite()
                || !value.gain.is_finite()
                || !value.decay_seconds.is_finite()
                || value.frequency_hz < 0.0
                || value.gain < 0.0
                || value.decay_seconds <= 0.0
            {
                return Err(invalid("ACOU body mode is invalid"));
            }
        }
        profiles.push(AcousticProfile {
            articulation,
            dynamic,
            midi_note_mean: scalar[0],
            fundamental_hz: scalar[1],
            attack_ms: scalar[2],
            release_ms: scalar[3],
            sustain_rms: scalar[4],
            noise_ratio: scalar[5],
            spectral_centroid_hz: scalar[6],
            spectral_flatness: scalar[7],
            body_rms: scalar[8],
            peak: scalar[9],
            harmonics,
            body_modes,
            record_count,
        });
    }
    Ok((sample_rate, profiles, source_file_count, source_frame_count))
}

fn parse_audio_segments(raw: &[u8]) -> Result<(u32, Vec<AudioSegment>, Vec<i16>, u32, u64)> {
    if raw.len() < AUDIO_HEADER_SIZE {
        return Err(invalid("AUDO header is truncated"));
    }
    if raw[0..4] != AUDIO_MAGIC {
        return Err(invalid("AUDO magic is invalid"));
    }
    if read_u16(raw, 4)? != ASSET_VERSION || read_u16(raw, 6)? as usize != AUDIO_HEADER_SIZE {
        return Err(invalid("AUDO version or header size is unsupported"));
    }
    let sample_rate = read_u32(raw, 8)?;
    let channels = read_u16(raw, 12)?;
    let descriptor_count = read_u32(raw, 16)? as usize;
    let source_file_count = read_u32(raw, 20)?;
    let source_frame_count = read_u64(raw, 24)?;
    let descriptor_size = read_u32(raw, 32)? as usize;
    let payload_offset = read_u64(raw, 36)? as usize;
    let payload_bytes = read_u64(raw, 44)? as usize;
    if channels != 1
        || descriptor_count == 0
        || descriptor_count > MAX_AUDIO_SEGMENTS
        || descriptor_size != AUDIO_DESCRIPTOR_SIZE
    {
        return Err(invalid("AUDO dimensions or count are invalid"));
    }
    let descriptor_end = AUDIO_HEADER_SIZE
        .checked_add(
            descriptor_count
                .checked_mul(descriptor_size)
                .ok_or_else(|| invalid("AUDO descriptor table overflows"))?,
        )
        .ok_or_else(|| invalid("AUDO descriptor table overflows"))?;
    if payload_offset != descriptor_end
        || payload_bytes != raw.len().saturating_sub(payload_offset)
        || payload_offset > raw.len()
        || payload_bytes % 2 != 0
    {
        return Err(invalid("AUDO payload bounds do not match header"));
    }
    let sample_count = payload_bytes / 2;
    if sample_count > MAX_AUDIO_SAMPLES {
        return Err(invalid("AUDO sample payload exceeds bounds"));
    }
    let mut segments = Vec::with_capacity(descriptor_count);
    for index in 0..descriptor_count {
        let start = AUDIO_HEADER_SIZE + index * descriptor_size;
        let kind = raw[start];
        let articulation = raw[start + 1];
        let dynamic = raw[start + 2];
        let midi_note = read_i16(raw, start + 4)?;
        let source_file_index = read_u32(raw, start + 8)?;
        let source_start_frame = read_u64(raw, start + 12)?;
        let frame_count = read_u32(raw, start + 20)?;
        let sample_offset = read_u64(raw, start + 24)? as usize;
        let source_sample_rate = read_u32(raw, start + 32)?;
        let gain = read_f32(raw, start + 36)?;
        let rms = read_f32(raw, start + 40)?;
        let peak = read_f32(raw, start + 44)?;
        if frame_count == 0
            || source_sample_rate == 0
            || !gain.is_finite()
            || !rms.is_finite()
            || !peak.is_finite()
            || gain < 0.0
            || rms < 0.0
            || peak < 0.0
        {
            return Err(invalid("AUDO descriptor contains invalid values"));
        }
        let end = sample_offset
            .checked_add(frame_count as usize)
            .ok_or_else(|| invalid("AUDO segment range overflows"))?;
        if end > sample_count {
            return Err(invalid("AUDO segment is outside PCM payload"));
        }
        segments.push(AudioSegment {
            kind,
            articulation,
            dynamic,
            midi_note,
            source_file_index,
            source_start_frame,
            frame_count,
            sample_offset,
            source_sample_rate,
            gain,
            rms,
            peak,
        });
    }
    let mut samples = Vec::with_capacity(sample_count);
    for chunk in raw[payload_offset..].chunks_exact(2) {
        samples.push(i16::from_le_bytes([chunk[0], chunk[1]]));
    }
    Ok((
        sample_rate,
        segments,
        samples,
        source_file_count,
        source_frame_count,
    ))
}

fn read_u16(raw: &[u8], offset: usize) -> Result<u16> {
    let bytes = raw
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("acoustic asset integer is truncated"))?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_i16(raw: &[u8], offset: usize) -> Result<i16> {
    let bytes = raw
        .get(offset..offset + 2)
        .ok_or_else(|| invalid("acoustic asset integer is truncated"))?;
    Ok(i16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32(raw: &[u8], offset: usize) -> Result<u32> {
    let bytes = raw
        .get(offset..offset + 4)
        .ok_or_else(|| invalid("acoustic asset integer is truncated"))?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(raw: &[u8], offset: usize) -> Result<u64> {
    let bytes = raw
        .get(offset..offset + 8)
        .ok_or_else(|| invalid("acoustic asset integer is truncated"))?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

fn read_f32(raw: &[u8], offset: usize) -> Result<f32> {
    Ok(f32::from_bits(read_u32(raw, offset)?))
}
