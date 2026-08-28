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

use std::{fmt, fs, path::Path};

use fbmx_runtime::sha256;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod voicebank;

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
/// An FBMX **Performer**: predicts how a player performs a written score, and
/// produces no audio.
///
/// Its own section rather than a second `RESI` because the two are different
/// kinds of model and are consumed at different points. A residual runs inside
/// the audio path, per sample; a Performer runs before playback, once per note,
/// and its output is ordinary project data a user can edit afterwards. A host
/// that understands one and not the other has to be able to say so, which it
/// cannot do if both arrive under the same tag.
pub const FBMX_PERFORMER_TAG: [u8; 4] = *b"PERF";
/// An FBMX **Accent Analyzer**: estimates how strongly each note of a score
/// should be emphasised, and by which means.
///
/// A third model kind rather than a second `PERF`, for the same reason `PERF`
/// is not a second `RESI`: a host that can run one and not the other has to be
/// able to say which. It also runs at a different point — an accent analysis is
/// something the user asks for and then edits, while a Performer runs over the
/// result of that editing — so a package may reasonably carry either alone.
///
/// The model in this section produces a *correction* to a fitted linear rule,
/// not a standalone prediction. The rule travels with the host rather than with
/// the package, because it is small, because a host with no model still needs
/// it, and because a rule embedded per package would let two instruments
/// disagree about what a downbeat is.
pub const FBMX_ACCENT_TAG: [u8; 4] = *b"ACNT";
pub const INDEX_TAG: [u8; 4] = *b"INDX";
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
    #[error("{} section overlaps another section", TagName(*tag))]
    OverlappingSections { tag: [u8; 4] },
    #[error("{} section checksum mismatch", TagName(*tag))]
    SectionChecksum { tag: [u8; 4] },
    #[error("SFM file checksum mismatch")]
    FileChecksum,
    #[error("SFM section {} is not valid UTF-8: {source}", TagName(*tag))]
    Utf8 {
        tag: [u8; 4],
        source: std::str::Utf8Error,
    },
    #[error("SFM JSON section {} is invalid: {source}", TagName(*tag))]
    Json {
        tag: [u8; 4],
        source: serde_json::Error,
    },
    #[error("SFM section {} is missing", TagName(*tag))]
    MissingSection { tag: [u8; 4] },
    #[error("SFM section tag must be exactly four ASCII bytes")]
    InvalidTag,
    #[error("SFM physical profile is invalid: {0}")]
    InvalidPhysicalProfile(String),
    #[error("SFM acoustic asset is invalid: {0}")]
    InvalidAcousticAsset(String),
    #[error("SFM voicebank is invalid: {0}")]
    InvalidVoicebank(String),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// The host asked to abandon this load through [`SfmLoadObserver::cancelled`].
    /// Distinct from every failure variant so a caller can tell "the user moved
    /// on" apart from "this package is broken".
    #[error("SFM load was cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, SfmError>;

/// Renders a four-byte section tag as the ASCII name the packaging tools print
/// (`AUDO`), not as the byte array a `{:?}` would produce. Section tags reach
/// end users inside load-failure messages, so they have to read as names.
struct TagName([u8; 4]);

impl fmt::Display for TagName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            if byte.is_ascii_graphic() {
                write!(f, "{}", byte as char)?;
            } else {
                write!(f, "\\x{byte:02x}")?;
            }
        }
        Ok(())
    }
}

/// Payload bytes hashed between two cancellation/progress checkpoints.
///
/// Verification hashes every byte of the file twice — once for the trailer
/// digest and once for the section it belongs to — so on a 146 MB voicebank
/// package a single unchunked digest is seconds of uninterruptible work. At
/// 4 MiB a cancel lands well inside a frame and the chunking itself costs
/// nothing measurable next to the hash.
pub const VERIFY_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Observes SFM verification so a host can report real progress and abandon a
/// load whose result is no longer wanted.
///
/// Progress is reported in bytes hashed rather than as a step count: hashing is
/// what the time actually goes into, and its cost is known exactly from the
/// section table before any of it is paid.
pub trait SfmLoadObserver {
    /// `hashed` of `total` payload bytes digested so far.
    fn verified(&mut self, hashed: u64, total: u64);

    /// Checked between chunks. Returning `true` aborts with
    /// [`SfmError::Cancelled`] and leaves no partially built [`SfmFile`].
    fn cancelled(&mut self) -> bool {
        false
    }
}

/// Observer for callers that only want the parse.
pub struct IgnoreSfmProgress;

impl SfmLoadObserver for IgnoreSfmProgress {
    fn verified(&mut self, _hashed: u64, _total: u64) {}
}

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

/// Bounds-checked fixed header, shared by the verifying and peeking paths so
/// the two can never disagree about where the section table lives.
struct SfmHeader {
    flags: u32,
    count: usize,
    table_offset: u64,
    table_end: u64,
    body_end: usize,
}

/// SHA-256 `data`, yielding to `observer` every [`VERIFY_CHUNK_BYTES`] so the
/// hash of a 146 MB section stays both reportable and interruptible.
fn digest_chunked(
    data: &[u8],
    hashed: &mut u64,
    total: u64,
    observer: &mut dyn SfmLoadObserver,
) -> Result<[u8; 32]> {
    if observer.cancelled() {
        return Err(SfmError::Cancelled);
    }
    let mut hasher = sha256::Sha256::new();
    for chunk in data.chunks(VERIFY_CHUNK_BYTES) {
        if observer.cancelled() {
            return Err(SfmError::Cancelled);
        }
        hasher.update(chunk);
        *hashed += chunk.len() as u64;
        observer.verified(*hashed, total);
    }
    Ok(hasher.finalize())
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
        Self::from_vec(fs::read(path)?)
    }

    /// Verify `raw` and copy it into the returned model.
    ///
    /// Prefer [`SfmFile::from_vec`] whenever the buffer is already owned: this
    /// entry point has to duplicate it, which on a packaged voicebank means a
    /// second 146 MB allocation live at the same time as the first.
    pub fn from_bytes(raw: &[u8]) -> Result<Self> {
        Self::from_vec(raw.to_vec())
    }

    /// Verify `raw` in place and take ownership of the buffer.
    pub fn from_vec(raw: Vec<u8>) -> Result<Self> {
        Self::from_vec_observed(raw, &mut IgnoreSfmProgress)
    }

    /// Verify `raw` in place, reporting hashed bytes to `observer` and stopping
    /// as soon as it asks to cancel.
    ///
    /// Either the whole model is returned or nothing is: a cancelled or failed
    /// verification drops the buffer and every section index built so far, so
    /// no caller can end up holding a half-checked package.
    pub fn from_vec_observed(raw: Vec<u8>, observer: &mut dyn SfmLoadObserver) -> Result<Self> {
        let (sections, flags) = Self::verify(&raw, observer)?;
        Ok(Self {
            bytes: raw,
            sections,
            flags,
        })
    }

    /// Section table as declared by the header, with its ranges bounds-checked
    /// but no payload digested.
    ///
    /// A host uses this to weight a load's stages by the bytes each one will
    /// actually touch before paying for any of them. The entries are *not*
    /// verified — only [`SfmFile::from_vec_observed`] and friends return a
    /// model whose payloads are known to match their checksums.
    pub fn peek_sections(raw: &[u8]) -> Result<Vec<SectionIndex>> {
        let header = Self::parse_header(raw)?;
        Self::read_section_table(raw, &header)
    }

    fn verify(raw: &[u8], observer: &mut dyn SfmLoadObserver) -> Result<(Vec<SectionIndex>, u32)> {
        let header = Self::parse_header(raw)?;
        let declared = Self::read_section_table(raw, &header)?;

        // Every payload byte is hashed twice: once inside the trailer digest
        // over the whole body, once for its own section. Both halves are known
        // here, so the denominator never has to be guessed.
        let section_bytes = declared
            .iter()
            .fold(0u64, |sum, entry| sum.saturating_add(entry.size));
        let total = (header.body_end as u64).saturating_add(section_bytes);
        let mut hashed = 0u64;

        if digest_chunked(&raw[..header.body_end], &mut hashed, total, observer)?
            != raw[header.body_end..]
        {
            return Err(SfmError::FileChecksum);
        }

        let mut sections = Vec::with_capacity(declared.len());
        for entry in declared {
            let end = entry.offset + entry.size;
            let payload = &raw[entry.offset as usize..end as usize];
            if digest_chunked(payload, &mut hashed, total, observer)? != entry.checksum {
                return Err(SfmError::SectionChecksum { tag: entry.tag });
            }
            if sections.iter().any(|other: &SectionIndex| {
                entry.offset < other.offset + other.size && other.offset < end
            }) {
                return Err(SfmError::OverlappingSections { tag: entry.tag });
            }
            sections.push(entry);
        }
        Ok((sections, header.flags))
    }

    fn parse_header(raw: &[u8]) -> Result<SfmHeader> {
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
        Ok(SfmHeader {
            flags,
            count,
            table_offset,
            table_end,
            body_end,
        })
    }

    fn read_section_table(raw: &[u8], header: &SfmHeader) -> Result<Vec<SectionIndex>> {
        let mut sections = Vec::with_capacity(header.count);
        for index in 0..header.count {
            let start = header.table_offset as usize + index * INDEX_ENTRY_SIZE;
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
                file_len: header.body_end,
            })?;
            if offset < header.table_end || end > header.body_end as u64 {
                return Err(SfmError::OutOfBounds {
                    what: "section",
                    offset,
                    size,
                    file_len: header.body_end,
                });
            }
            sections.push(SectionIndex {
                tag,
                flags,
                offset,
                size,
                checksum,
            });
        }
        Ok(sections)
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

    /// Parse the paired `INDX`/`AUDO` sections into the complete playable
    /// voicebank. A package without both sections returns `Ok(None)` so the
    /// physical-only fallback remains usable; malformed present sections are
    /// rejected before a renderer is prepared.
    pub fn voicebank_model(&self) -> Result<Option<voicebank::VoicebankModel>> {
        match (self.section(INDEX_TAG), self.section(AUDIO_TAG)) {
            (Some(index), Some(audio)) => {
                voicebank::VoicebankModel::from_sections(index, audio).map(Some)
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

    #[derive(Default)]
    struct RecordingObserver {
        samples: Vec<(u64, u64)>,
        cancel_after: Option<usize>,
    }

    impl SfmLoadObserver for RecordingObserver {
        fn verified(&mut self, hashed: u64, total: u64) {
            self.samples.push((hashed, total));
        }

        fn cancelled(&mut self) -> bool {
            self.cancel_after
                .is_some_and(|limit| self.samples.len() >= limit)
        }
    }

    #[test]
    fn peeked_section_sizes_match_the_verified_ones() {
        let bytes = fixture();
        let peeked = SfmFile::peek_sections(&bytes).unwrap();
        let verified = SfmFile::from_vec(bytes).unwrap();
        assert_eq!(peeked.len(), verified.sections().len());
        for (peeked, verified) in peeked.iter().zip(verified.sections()) {
            assert_eq!(peeked, verified);
        }
    }

    #[test]
    fn progress_total_is_the_body_plus_every_section() {
        let bytes = fixture();
        let sections = SfmFile::peek_sections(&bytes).unwrap();
        let expected_total = (bytes.len() - TRAILER_SIZE) as u64
            + sections.iter().map(|entry| entry.size).sum::<u64>();

        let mut observer = RecordingObserver::default();
        SfmFile::from_vec_observed(bytes, &mut observer).unwrap();

        assert!(!observer.samples.is_empty());
        assert!(
            observer
                .samples
                .iter()
                .all(|(_, total)| *total == expected_total)
        );
        // Progress is monotonic and lands exactly on the denominator, so a bar
        // driven by it never rewinds and never stops short of full.
        assert!(
            observer
                .samples
                .windows(2)
                .all(|pair| pair[0].0 <= pair[1].0)
        );
        assert_eq!(observer.samples.last().unwrap().0, expected_total);
    }

    #[test]
    fn cancellation_aborts_without_returning_a_model() {
        let bytes = fixture();
        let mut observer = RecordingObserver {
            cancel_after: Some(1),
            ..RecordingObserver::default()
        };
        assert!(matches!(
            SfmFile::from_vec_observed(bytes, &mut observer),
            Err(SfmError::Cancelled)
        ));
    }

    #[test]
    fn section_tags_read_as_names_in_failure_messages() {
        let mut bytes = fixture();
        let payload_offset = u64::from_le_bytes(bytes[40..48].try_into().unwrap()) as usize;
        bytes[payload_offset] ^= 1;
        let file_checksum = sha256::digest(&bytes[..bytes.len() - TRAILER_SIZE]);
        let end = bytes.len() - TRAILER_SIZE;
        bytes[end..].copy_from_slice(&file_checksum);
        let message = SfmFile::from_vec(bytes).unwrap_err().to_string();
        assert_eq!(message, "META section checksum mismatch");
    }
}
