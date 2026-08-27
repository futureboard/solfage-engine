//! Checked, immutable sample byte access. Opening and prefaulting are explicitly
//! non-realtime operations.

use std::{
    fs::File,
    hint::black_box,
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
};

use memmap2::{Mmap, MmapOptions};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessHint {
    Normal,
    Sequential,
    Random,
    WillNeed,
    Cold,
}

#[derive(Debug, Clone, Copy)]
pub struct SampleView<'a> {
    bytes: &'a [u8],
}

impl<'a> SampleView<'a> {
    pub const fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

pub trait SampleStorage: Send + Sync {
    fn len(&self) -> u64;
    fn view(&self, offset: u64, len: usize) -> Result<SampleView<'_>, StorageError>;
    fn advise(&self, _range: Range<u64>, _hint: AccessHint) {}
    fn prefault(&self, range: Range<u64>) -> Result<(), StorageError>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[derive(Debug)]
pub struct MappedStorage {
    path: PathBuf,
    mapping: Mmap,
}

impl MappedStorage {
    /// Maps a file read-only. The file must not be truncated or mutated while
    /// this storage is alive. Solfege opens immutable library assets and keeps
    /// the mapping private, satisfying that invariant for normal operation.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path).map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        let metadata = file.metadata().map_err(|source| StorageError::Io {
            path: path.clone(),
            source,
        })?;
        if metadata.len() == 0 {
            return Err(StorageError::EmptyFile(path));
        }
        // SAFETY: the descriptor is held for map creation and the mapping owns
        // its OS handle afterwards. The mapping is read-only. Callers must not
        // externally truncate/mutate the mapped library file, as documented.
        let mapping =
            unsafe { MmapOptions::new().map(&file) }.map_err(|source| StorageError::Io {
                path: path.clone(),
                source,
            })?;
        Ok(Self { path, mapping })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SampleStorage for MappedStorage {
    fn len(&self) -> u64 {
        self.mapping.len() as u64
    }

    fn view(&self, offset: u64, len: usize) -> Result<SampleView<'_>, StorageError> {
        checked_view(&self.mapping, offset, len)
    }

    fn prefault(&self, range: Range<u64>) -> Result<(), StorageError> {
        prefault_bytes(&self.mapping, range)
    }
}

#[derive(Debug, Clone)]
pub struct PreloadedStorage {
    bytes: Arc<[u8]>,
}

impl PreloadedStorage {
    pub fn from_bytes(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| StorageError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Ok(Self::from_bytes(bytes))
    }
}

impl SampleStorage for PreloadedStorage {
    fn len(&self) -> u64 {
        self.bytes.len() as u64
    }

    fn view(&self, offset: u64, len: usize) -> Result<SampleView<'_>, StorageError> {
        checked_view(&self.bytes, offset, len)
    }

    fn prefault(&self, range: Range<u64>) -> Result<(), StorageError> {
        prefault_bytes(&self.bytes, range)
    }
}

fn checked_view(bytes: &[u8], offset: u64, len: usize) -> Result<SampleView<'_>, StorageError> {
    let start = usize::try_from(offset).map_err(|_| StorageError::OutOfBounds {
        offset,
        len,
        storage_len: bytes.len() as u64,
    })?;
    let end = start.checked_add(len).ok_or(StorageError::OutOfBounds {
        offset,
        len,
        storage_len: bytes.len() as u64,
    })?;
    let slice = bytes.get(start..end).ok_or(StorageError::OutOfBounds {
        offset,
        len,
        storage_len: bytes.len() as u64,
    })?;
    Ok(SampleView { bytes: slice })
}

fn prefault_bytes(bytes: &[u8], range: Range<u64>) -> Result<(), StorageError> {
    let len = range.end.saturating_sub(range.start);
    let view = checked_view(
        bytes,
        range.start,
        usize::try_from(len).map_err(|_| StorageError::OutOfBounds {
            offset: range.start,
            len: usize::MAX,
            storage_len: bytes.len() as u64,
        })?,
    )?;
    let resident = view.as_bytes();
    for offset in (0..resident.len()).step_by(4096) {
        black_box(resident[offset]);
    }
    if let Some(last) = resident.last() {
        black_box(*last);
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmFormat {
    Signed16,
    Signed24,
    Signed32,
    Float32,
    Float64,
}

impl PcmFormat {
    pub const fn bytes_per_sample(self) -> usize {
        match self {
            Self::Signed16 => 2,
            Self::Signed24 => 3,
            Self::Signed32 | Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavLayout {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: PcmFormat,
    pub data_offset: u64,
    pub data_len: u64,
    pub frames: u64,
    pub block_align: u16,
}

pub fn parse_wav(storage: &dyn SampleStorage) -> Result<WavLayout, StorageError> {
    let header = storage.view(0, 12)?.as_bytes();
    if &header[0..4] != b"RIFF" || &header[8..12] != b"WAVE" {
        return Err(StorageError::InvalidWav("missing RIFF/WAVE signature"));
    }

    let mut offset = 12_u64;
    let mut format_chunk = None;
    let mut data_chunk = None;
    while offset
        .checked_add(8)
        .is_some_and(|end| end <= storage.len())
    {
        let chunk = storage.view(offset, 8)?.as_bytes();
        let id = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let size = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]) as u64;
        let body = offset
            .checked_add(8)
            .ok_or(StorageError::InvalidWav("chunk offset overflow"))?;
        let end = body
            .checked_add(size)
            .ok_or(StorageError::InvalidWav("chunk size overflow"))?;
        if end > storage.len() {
            return Err(StorageError::InvalidWav("chunk extends beyond file"));
        }
        if &id == b"fmt " {
            if size < 16 {
                return Err(StorageError::InvalidWav("short fmt chunk"));
            }
            let fmt = storage.view(body, 16)?.as_bytes();
            format_chunk = Some((
                u16::from_le_bytes([fmt[0], fmt[1]]),
                u16::from_le_bytes([fmt[2], fmt[3]]),
                u32::from_le_bytes([fmt[4], fmt[5], fmt[6], fmt[7]]),
                u16::from_le_bytes([fmt[12], fmt[13]]),
                u16::from_le_bytes([fmt[14], fmt[15]]),
            ));
        } else if &id == b"data" {
            data_chunk = Some((body, size));
        }
        offset = end
            .checked_add(size & 1)
            .ok_or(StorageError::InvalidWav("chunk padding overflow"))?;
    }

    let (encoding, channels, sample_rate, block_align, bits) =
        format_chunk.ok_or(StorageError::InvalidWav("missing fmt chunk"))?;
    let (data_offset, data_len) =
        data_chunk.ok_or(StorageError::InvalidWav("missing data chunk"))?;
    if channels == 0 || channels > 64 || sample_rate == 0 {
        return Err(StorageError::InvalidWav(
            "invalid channel count or sample rate",
        ));
    }
    let format = match (encoding, bits) {
        (1, 16) => PcmFormat::Signed16,
        (1, 24) => PcmFormat::Signed24,
        (1, 32) => PcmFormat::Signed32,
        (3, 32) => PcmFormat::Float32,
        (3, 64) => PcmFormat::Float64,
        _ => return Err(StorageError::UnsupportedWavFormat { encoding, bits }),
    };
    let expected_align = channels as usize * format.bytes_per_sample();
    if block_align as usize != expected_align || data_len % block_align as u64 != 0 {
        return Err(StorageError::InvalidWav("invalid block alignment"));
    }
    Ok(WavLayout {
        sample_rate,
        channels,
        format,
        data_offset,
        data_len,
        frames: data_len / block_align as u64,
        block_align,
    })
}

#[derive(Debug, Error)]
pub enum StorageError {
    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot map empty file: {0}")]
    EmptyFile(PathBuf),
    #[error("byte view {offset}+{len} exceeds storage length {storage_len}")]
    OutOfBounds {
        offset: u64,
        len: usize,
        storage_len: u64,
    },
    #[error("invalid WAV: {0}")]
    InvalidWav(&'static str),
    #[error("unsupported WAV format code {encoding}, {bits} bits")]
    UnsupportedWavFormat { encoding: u16, bits: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tiny_wav() -> Vec<u8> {
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&40_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&48_000_u32.to_le_bytes());
        wav.extend_from_slice(&96_000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&4_u32.to_le_bytes());
        wav.extend_from_slice(&[-1_i16, 1_i16].map(i16::to_le_bytes).concat());
        wav
    }

    #[test]
    fn parses_pcm_without_decoding_or_copying() {
        let storage = PreloadedStorage::from_bytes(tiny_wav());
        let layout = parse_wav(&storage).unwrap();
        assert_eq!(layout.sample_rate, 48_000);
        assert_eq!(layout.frames, 2);
        assert_eq!(layout.format, PcmFormat::Signed16);
        assert_eq!(
            storage
                .view(layout.data_offset, 4)
                .unwrap()
                .as_bytes()
                .len(),
            4
        );
    }

    #[test]
    fn checked_view_rejects_overflow() {
        let storage = PreloadedStorage::from_bytes(Vec::from([1, 2, 3]));
        assert!(storage.view(u64::MAX, 2).is_err());
    }

    #[test]
    fn whole_file_mapping_exposes_checked_views() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&[10_u8, 20, 30, 40]).unwrap();
        file.flush().unwrap();
        let storage = MappedStorage::open(file.path()).unwrap();

        assert_eq!(storage.len(), 4);
        assert_eq!(storage.view(1, 2).unwrap().as_bytes(), &[20, 30]);
        storage.prefault(0..4).unwrap();
    }
}
