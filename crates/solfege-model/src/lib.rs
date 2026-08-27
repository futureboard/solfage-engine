//! SFM v1: the complete playable Solfege Model container.
//!
//! SFM deliberately contains no realtime code. It is parsed and verified on a
//! control thread, then its sections are handed to the physical and optional
//! FBMX runtimes. The format is a small indexed file rather than a directory
//! of loose development artifacts, so a packaged model has no training-path
//! dependency.
//!
//! ```text
//! 0       4       magic "SFM\0"
//! 4       2       little-endian format version (1)
//! 6       2       fixed header size (32)
//! 8       4       flags
//! 12      4       section count
//! 16      8       section table offset
//! 24      8       total file size
//! 32      ...     section index entries, 56 bytes each
//! ...             16-byte aligned section payloads
//! EOF-32  32      SHA-256 of every byte before this trailer
//! ```
//!
//! Every section has its own SHA-256 and checked `(offset, size)` range. A
//! loader rejects malformed lengths, overlapping sections, truncated files,
//! invalid UTF-8/JSON profile data, and checksum mismatches before returning a
//! model. No section access performs file I/O or allocation.

#![forbid(unsafe_code)]

use std::{fs, path::Path};

use fbmx_runtime::sha256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod acoustic;

pub const MAGIC: [u8; 4] = *b"SFM\0";
pub const FORMAT_VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 32;
pub const INDEX_ENTRY_SIZE: usize = 56;
pub const TRAILER_SIZE: usize = 32;
pub const ALIGNMENT: usize = 16;
pub const MAX_SECTIONS: usize = 64;
pub const MAX_FILE_BYTES: usize = 1024 * 1024 * 1024;
pub const PHYSICAL_TAG: [u8; 4] = *b"PHYS";
pub const BODY_TAG: [u8; 4] = *b"BODY";
pub const FBMX_RESIDUAL_TAG: [u8; 4] = *b"RESI";
pub const ACOUSTIC_TAG: [u8; 4] = *b"ACOU";
pub const AUDIO_TAG: [u8; 4] = *b"AUDO";
pub const METADATA_TAG: [u8; 4] = *b"META";

#[derive(Debug, Error)]
pub enum SfmError {
    #[error("SFM file is too short: {len} bytes, need at least {need}")]
    TooShort { len: usize, need: usize },
    #[error("bad SFM magic {0:?}")]
    BadMagic([u8; 4]),
    #[error("unsupported SFM version {found}; this build reads {supported}")]
    UnsupportedVersion { found: u16, supported: u16 },
    #[error("SFM header size is {found}; expected {expected}")]
    BadHeaderSize { found: u16, expected: usize },
    #[error("SFM section count {count} exceeds cap {cap}")]
    TooManySections { count: usize, cap: usize },
    #[error("SFM file size {declared} does not match actual size {actual}")]
    FileSizeMismatch { declared: u64, actual: usize },
    #[error("SFM range is outside the file: {what}, offset={offset}, size={size}, file={file_len}")]
    OutOfBounds {
        what: &'static str,
        offset: u64,
        size: u64,
        file_len: usize,
    },
    #[error("SFM section index overlaps the file trailer or payload")]
    InvalidIndex,
    #[error("SFM section {tag:?} overlaps another section")]
    OverlappingSections { tag: [u8; 4] },
    #[error("SFM section {tag:?} checksum mismatch")]
    SectionChecksum { tag: [u8; 4] },
    #[error("SFM file checksum mismatch")]
    FileChecksum,
    #[error("SFM section {tag:?} is not valid UTF-8: {source}")]
    Utf8 {
        tag: [u8; 4],
        source: std::str::Utf8Error,
    },
    #[error("SFM JSON section {tag:?} is invalid: {source}")]
    Json {
        tag: [u8; 4],
        source: serde_json::Error,
    },
    #[error("SFM section {tag:?} is missing")]
    MissingSection { tag: [u8; 4] },
    #[error("SFM section tag must be exactly four ASCII bytes")]
    InvalidTag,
    #[error("SFM physical profile is invalid: {0}")]
    InvalidPhysicalProfile(String),
    #[error("SFM acoustic asset is invalid: {0}")]
    InvalidAcousticAsset(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SfmError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SectionIndex {
    pub tag: [u8; 4],
    pub flags: u32,
    pub offset: u64,
    pub size: u64,
    pub checksum: [u8; 32],
}

impl SectionIndex {
    pub fn name(&self) -> String {
        String::from_utf8_lossy(&self.tag).into_owned()
    }
}

/// A parsed and fully verified SFM file. Keeping the owned bytes here makes
/// all section slices stable for the lifetime of any prepared runtime.
#[derive(Debug, Clone)]
pub struct SfmFile {
    bytes: Vec<u8>,
    sections: Vec<SectionIndex>,
    flags: u32,
}

impl SfmFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&fs::read(path)?)
    }

    pub fn from_bytes(raw: &[u8]) -> Result<Self> {
        if raw.len() < HEADER_SIZE + TRAILER_SIZE {
            return Err(SfmError::TooShort {
                len: raw.len(),
                need: HEADER_SIZE + TRAILER_SIZE,
            });
        }
        if raw.len() > MAX_FILE_BYTES {
            return Err(SfmError::OutOfBounds {
                what: "file",
                offset: 0,
                size: raw.len() as u64,
                file_len: MAX_FILE_BYTES,
            });
        }
        let magic = [raw[0], raw[1], raw[2], raw[3]];
        if magic != MAGIC {
            return Err(SfmError::BadMagic(magic));
        }
        let version = u16::from_le_bytes([raw[4], raw[5]]);
        if version != FORMAT_VERSION {
            return Err(SfmError::UnsupportedVersion {
                found: version,
                supported: FORMAT_VERSION,
            });
        }
        let header_size = u16::from_le_bytes([raw[6], raw[7]]);
        if header_size as usize != HEADER_SIZE {
            return Err(SfmError::BadHeaderSize {
                found: header_size,
                expected: HEADER_SIZE,
            });
        }
        let flags = read_u32(raw, 8)?;
        let count = read_u32(raw, 12)? as usize;
        if count > MAX_SECTIONS {
            return Err(SfmError::TooManySections {
                count,
                cap: MAX_SECTIONS,
            });
        }
        let table_offset = read_u64(raw, 16)?;
        let declared_size = read_u64(raw, 24)?;
        if declared_size != raw.len() as u64 {
            return Err(SfmError::FileSizeMismatch {
                declared: declared_size,
                actual: raw.len(),
            });
        }
        let table_size = (count)
            .checked_mul(INDEX_ENTRY_SIZE)
            .ok_or(SfmError::InvalidIndex)?;
        let table_end = table_offset
            .checked_add(table_size as u64)
            .ok_or(SfmError::InvalidIndex)?;
        let body_end = raw.len() - TRAILER_SIZE;
        if table_offset < HEADER_SIZE as u64 || table_end > body_end as u64 {
            return Err(SfmError::InvalidIndex);
        }

        if sha256::digest(&raw[..body_end]) != raw[body_end..] {
            return Err(SfmError::FileChecksum);
        }

        let mut sections = Vec::with_capacity(count);
        for index in 0..count {
            let start = table_offset as usize + index * INDEX_ENTRY_SIZE;
            let tag = [raw[start], raw[start + 1], raw[start + 2], raw[start + 3]];
            let flags = read_u32(raw, start + 4)?;
            let offset = read_u64(raw, start + 8)?;
            let size = read_u64(raw, start + 16)?;
            let mut checksum = [0u8; 32];
            checksum.copy_from_slice(&raw[start + 24..start + 56]);
            let end = offset.checked_add(size).ok_or(SfmError::OutOfBounds {
                what: "section",
                offset,
                size,
                file_len: body_end,
            })?;
            if offset < table_end || end > body_end as u64 {
                return Err(SfmError::OutOfBounds {
                    what: "section",
                    offset,
                    size,
                    file_len: body_end,
                });
            }
            if sha256::digest(&raw[offset as usize..end as usize]) != checksum {
                return Err(SfmError::SectionChecksum { tag });
            }
            if sections.iter().any(|other: &SectionIndex| {
                offset < other.offset + other.size && other.offset < end
            }) {
                return Err(SfmError::OverlappingSections { tag });
            }
            sections.push(SectionIndex {
                tag,
                flags,
                offset,
                size,
                checksum,
            });
        }
        Ok(Self {
            bytes: raw.to_vec(),
            sections,
            flags,
        })
    }

    pub fn flags(&self) -> u32 {
        self.flags
    }

    pub fn sections(&self) -> &[SectionIndex] {
        &self.sections
    }

    pub fn section(&self, tag: [u8; 4]) -> Option<&[u8]> {
        self.sections
            .iter()
            .find(|entry| entry.tag == tag)
            .map(|entry| &self.bytes[entry.offset as usize..(entry.offset + entry.size) as usize])
    }

    pub fn required_section(&self, tag: [u8; 4]) -> Result<&[u8]> {
        self.section(tag).ok_or(SfmError::MissingSection { tag })
    }

    pub fn metadata_json(&self) -> Result<serde_json::Value> {
        let raw = self.required_section(METADATA_TAG)?;
        serde_json::from_slice(raw).map_err(|source| SfmError::Json {
            tag: METADATA_TAG,
            source,
        })
    }

    pub fn physical_profile(&self) -> Result<PhysicalProfile> {
        let raw = self.required_section(PHYSICAL_TAG)?;
        serde_json::from_slice(raw).map_err(|source| SfmError::Json {
            tag: PHYSICAL_TAG,
            source,
        })
    }

    /// Parse the paired ACOU/AUDO sections into an owned runtime asset. A
    /// package without both sections intentionally returns `Ok(None)` so the
    /// physical-only fallback remains usable; malformed present sections are
    /// rejected before a renderer is prepared.
    pub fn acoustic_model(&self) -> Result<Option<acoustic::AcousticModel>> {
        match (self.section(ACOUSTIC_TAG), self.section(AUDIO_TAG)) {
            (Some(acoustic), Some(audio)) => {
                acoustic::AcousticModel::from_sections(acoustic, audio).map(Some)
            }
            _ => Ok(None),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Debug, Clone)]
pub struct SfmBuilder {
    flags: u32,
    sections: Vec<PendingSection>,
}

#[derive(Debug, Clone)]
struct PendingSection {
    tag: [u8; 4],
    flags: u32,
    payload: Vec<u8>,
}

impl Default for SfmBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl SfmBuilder {
    pub fn new() -> Self {
        Self {
            flags: 0,
            sections: Vec::new(),
        }
    }

    pub fn flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    pub fn add_section(mut self, tag: [u8; 4], flags: u32, payload: Vec<u8>) -> Result<Self> {
        if tag.iter().any(|byte| !byte.is_ascii_graphic()) {
            return Err(SfmError::InvalidTag);
        }
        if self.sections.iter().any(|section| section.tag == tag) {
            return Err(SfmError::OverlappingSections { tag });
        }
        if self.sections.len() == MAX_SECTIONS {
            return Err(SfmError::TooManySections {
                count: self.sections.len() + 1,
                cap: MAX_SECTIONS,
            });
        }
        self.sections.push(PendingSection {
            tag,
            flags,
            payload,
        });
        Ok(self)
    }

    pub fn build(self) -> Result<Vec<u8>> {
        let table_offset = HEADER_SIZE;
        let payload_start = align_up(
            table_offset
                .checked_add(
                    self.sections
                        .len()
                        .checked_mul(INDEX_ENTRY_SIZE)
                        .ok_or(SfmError::InvalidIndex)?,
                )
                .ok_or(SfmError::InvalidIndex)?,
            ALIGNMENT,
        );
        let mut sections = Vec::with_capacity(self.sections.len());
        let mut cursor = payload_start;
        for pending in &self.sections {
            cursor = align_up(cursor, ALIGNMENT);
            let end = cursor
                .checked_add(pending.payload.len())
                .ok_or(SfmError::InvalidIndex)?;
            sections.push(SectionIndex {
                tag: pending.tag,
                flags: pending.flags,
                offset: cursor as u64,
                size: pending.payload.len() as u64,
                checksum: sha256::digest(&pending.payload),
            });
            cursor = end;
        }
        let file_size = cursor
            .checked_add(TRAILER_SIZE)
            .ok_or(SfmError::InvalidIndex)?;
        if file_size > MAX_FILE_BYTES {
            return Err(SfmError::OutOfBounds {
                what: "built file",
                offset: 0,
                size: file_size as u64,
                file_len: MAX_FILE_BYTES,
            });
        }
        let mut bytes = vec![0u8; file_size];
        bytes[0..4].copy_from_slice(&MAGIC);
        bytes[4..6].copy_from_slice(&FORMAT_VERSION.to_le_bytes());
        bytes[6..8].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
        bytes[8..12].copy_from_slice(&self.flags.to_le_bytes());
        bytes[12..16].copy_from_slice(&(sections.len() as u32).to_le_bytes());
        bytes[16..24].copy_from_slice(&(table_offset as u64).to_le_bytes());
        bytes[24..32].copy_from_slice(&(file_size as u64).to_le_bytes());
        for (index, entry) in sections.iter().enumerate() {
            let start = table_offset + index * INDEX_ENTRY_SIZE;
            bytes[start..start + 4].copy_from_slice(&entry.tag);
            bytes[start + 4..start + 8].copy_from_slice(&entry.flags.to_le_bytes());
            bytes[start + 8..start + 16].copy_from_slice(&entry.offset.to_le_bytes());
            bytes[start + 16..start + 24].copy_from_slice(&entry.size.to_le_bytes());
            bytes[start + 24..start + 56].copy_from_slice(&entry.checksum);
        }
        for (pending, entry) in self.sections.iter().zip(&sections) {
            let start = entry.offset as usize;
            bytes[start..start + pending.payload.len()].copy_from_slice(&pending.payload);
        }
        let checksum = sha256::digest(&bytes[..file_size - TRAILER_SIZE]);
        bytes[file_size - TRAILER_SIZE..].copy_from_slice(&checksum);
        Ok(bytes)
    }

    pub fn write_to(self, path: impl AsRef<Path>) -> Result<()> {
        fs::write(path, self.build()?)?;
        Ok(())
    }
}

fn align_up(value: usize, alignment: usize) -> usize {
    (value + alignment - 1) & !(alignment - 1)
}

fn read_u32(raw: &[u8], offset: usize) -> Result<u32> {
    let bytes = raw.get(offset..offset + 4).ok_or(SfmError::InvalidIndex)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(raw: &[u8], offset: usize) -> Result<u64> {
    let bytes = raw.get(offset..offset + 8).ok_or(SfmError::InvalidIndex)?;
    Ok(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PhysicalProfile {
    pub version: u32,
    pub sample_rate: u32,
    pub min_frequency_hz: f32,
    pub max_frequency_hz: f32,
    pub string_decay: f32,
    pub bow_friction: f32,
    pub bow_stiffness: f32,
    pub bridge_coupling: f32,
    pub body_mix: f32,
    pub noise_amount: f32,
    pub radiation_damping: f32,
    pub body_modes: [BodyModeProfile; 4],
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BodyModeProfile {
    pub frequency_hz: f32,
    pub decay_seconds: f32,
    pub gain: f32,
}

impl Default for PhysicalProfile {
    fn default() -> Self {
        Self {
            version: 1,
            sample_rate: 48_000,
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
                BodyModeProfile {
                    frequency_hz: 220.0,
                    decay_seconds: 0.35,
                    gain: 0.55,
                },
                BodyModeProfile {
                    frequency_hz: 440.0,
                    decay_seconds: 0.25,
                    gain: 0.35,
                },
                BodyModeProfile {
                    frequency_hz: 710.0,
                    decay_seconds: 0.18,
                    gain: 0.22,
                },
                BodyModeProfile {
                    frequency_hz: 1_120.0,
                    decay_seconds: 0.12,
                    gain: 0.12,
                },
            ],
        }
    }
}

impl PhysicalProfile {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(SfmError::InvalidPhysicalProfile(format!(
                "unsupported profile version {}",
                self.version
            )));
        }
        if !(8_000..=384_000).contains(&self.sample_rate) {
            return Err(SfmError::InvalidPhysicalProfile(
                "sample_rate must be a valid audio rate".to_owned(),
            ));
        }
        let values = [
            self.min_frequency_hz,
            self.max_frequency_hz,
            self.string_decay,
            self.bow_friction,
            self.bow_stiffness,
            self.bridge_coupling,
            self.body_mix,
            self.noise_amount,
            self.radiation_damping,
        ];
        if values.iter().any(|value| !value.is_finite()) {
            return Err(SfmError::InvalidPhysicalProfile(
                "profile contains NaN or infinity".to_owned(),
            ));
        }
        if self.min_frequency_hz <= 0.0 || self.max_frequency_hz < self.min_frequency_hz {
            return Err(SfmError::InvalidPhysicalProfile(
                "frequency range is invalid".to_owned(),
            ));
        }
        if self.body_modes.iter().any(|mode| {
            !mode.frequency_hz.is_finite()
                || !mode.decay_seconds.is_finite()
                || !mode.gain.is_finite()
        }) {
            return Err(SfmError::InvalidPhysicalProfile(
                "body mode contains NaN or infinity".to_owned(),
            ));
        }
        Ok(())
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec_pretty(self).map_err(|source| SfmError::Json {
            tag: PHYSICAL_TAG,
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        SfmBuilder::new()
            .add_section(METADATA_TAG, 0, br#"{"name":"test"}"#.to_vec())
            .unwrap()
            .add_section(
                PHYSICAL_TAG,
                0,
                PhysicalProfile::default().to_json_bytes().unwrap(),
            )
            .unwrap()
            .build()
            .unwrap()
    }

    #[test]
    fn round_trip_and_index_are_verified() {
        let bytes = fixture();
        let file = SfmFile::from_bytes(&bytes).unwrap();
        assert_eq!(file.sections().len(), 2);
        assert_eq!(file.metadata_json().unwrap()["name"], "test");
        assert_eq!(file.physical_profile().unwrap().sample_rate, 48_000);
    }

    #[test]
    fn file_checksum_rejects_mutation() {
        let mut bytes = fixture();
        let position = bytes.len() - TRAILER_SIZE - 1;
        bytes[position] ^= 1;
        assert!(matches!(
            SfmFile::from_bytes(&bytes),
            Err(SfmError::FileChecksum)
        ));
    }

    #[test]
    fn section_checksum_rejects_rebuilt_file_with_stale_index() {
        let mut bytes = fixture();
        let payload_offset = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        bytes[payload_offset] ^= 1;
        let file_checksum = sha256::digest(&bytes[..bytes.len() - TRAILER_SIZE]);
        let end = bytes.len() - TRAILER_SIZE;
        bytes[end..].copy_from_slice(&file_checksum);
        assert!(matches!(
            SfmFile::from_bytes(&bytes),
            Err(SfmError::SectionChecksum { .. })
        ));
    }

    #[test]
    fn truncation_is_rejected() {
        let bytes = fixture();
        assert!(SfmFile::from_bytes(&bytes[..bytes.len() - 1]).is_err());
    }
}
