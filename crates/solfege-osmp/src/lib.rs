//! Validated parser for the chunked, canonical little-endian OSMP container.

use std::{ops::Range, path::Path, sync::Arc};

use solfege_storage::{MappedStorage, SampleStorage, SampleView, StorageError};
use thiserror::Error;

pub const MAGIC: [u8; 4] = *b"OSMP";
pub const FORMAT_MAJOR: u16 = 1;
pub const FORMAT_MINOR: u16 = 0;
pub const HEADER_SIZE: u32 = 128;
pub const CHUNK_DESCRIPTOR_SIZE: usize = 40;
pub const MAX_CHUNKS: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OsmpHeader {
    pub format_major: u16,
    pub format_minor: u16,
    pub header_size: u32,
    pub alignment: u32,
    pub flags: u64,
    pub file_size: u64,
    pub chunk_table_offset: u64,
    pub chunk_count: u32,
    pub default_block_size: u32,
    pub instrument_id: [u8; 16],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChunkKind(pub [u8; 4]);

impl ChunkKind {
    pub const META: Self = Self(*b"META");
    pub const INST: Self = Self(*b"INST");
    pub const GRUP: Self = Self(*b"GRUP");
    pub const ZONE: Self = Self(*b"ZONE");
    pub const MODS: Self = Self(*b"MODS");
    pub const DSPG: Self = Self(*b"DSPG");
    pub const PRST: Self = Self(*b"PRST");
    pub const SCRP: Self = Self(*b"SCRP");
    pub const RSRC: Self = Self(*b"RSRC");
    pub const SMPX: Self = Self(*b"SMPX");
    pub const SMPD: Self = Self(*b"SMPD");
    pub const UIST: Self = Self(*b"UIST");
    pub const SIGN: Self = Self(*b"SIGN");
    pub const HASH: Self = Self(*b"HASH");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkDescriptor {
    pub kind: ChunkKind,
    pub flags: u32,
    pub offset: u64,
    pub stored_size: u64,
    pub logical_size: u64,
    pub alignment: u32,
    pub checksum: u32,
}

impl ChunkDescriptor {
    pub fn range(self) -> Option<Range<u64>> {
        self.offset
            .checked_add(self.stored_size)
            .map(|end| self.offset..end)
    }
}

pub struct OsmpFile {
    storage: Arc<dyn SampleStorage>,
    pub header: OsmpHeader,
    pub chunks: Vec<ChunkDescriptor>,
}

impl OsmpFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OsmpError> {
        let storage: Arc<dyn SampleStorage> = Arc::new(MappedStorage::open(path)?);
        Self::parse(storage)
    }

    pub fn parse(storage: Arc<dyn SampleStorage>) -> Result<Self, OsmpError> {
        let header_bytes = storage
            .view(0, HEADER_SIZE as usize)
            .map_err(|_| OsmpError::TruncatedHeader)?;
        let header = parse_header(header_bytes.as_bytes())?;
        validate_header(&header, storage.len())?;

        let table_size = (header.chunk_count as usize)
            .checked_mul(CHUNK_DESCRIPTOR_SIZE)
            .ok_or(OsmpError::ChunkTableOverflow)?;
        let table = storage
            .view(header.chunk_table_offset, table_size)
            .map_err(|_| OsmpError::ChunkTableOutOfBounds)?;
        let mut chunks = Vec::with_capacity(header.chunk_count as usize);
        for descriptor in table.as_bytes().chunks_exact(CHUNK_DESCRIPTOR_SIZE) {
            chunks.push(parse_descriptor(descriptor));
        }
        validate_chunks(&header, &chunks)?;
        Ok(Self {
            storage,
            header,
            chunks,
        })
    }

    pub fn chunks_of_kind(&self, kind: ChunkKind) -> impl Iterator<Item = &ChunkDescriptor> {
        self.chunks.iter().filter(move |chunk| chunk.kind == kind)
    }

    pub fn view_chunk(&self, chunk: &ChunkDescriptor) -> Result<SampleView<'_>, OsmpError> {
        let len = usize::try_from(chunk.stored_size).map_err(|_| OsmpError::ChunkOutOfBounds {
            kind: chunk.kind,
            offset: chunk.offset,
            size: chunk.stored_size,
        })?;
        self.storage
            .view(chunk.offset, len)
            .map_err(|_| OsmpError::ChunkOutOfBounds {
                kind: chunk.kind,
                offset: chunk.offset,
                size: chunk.stored_size,
            })
    }
}

fn parse_header(bytes: &[u8]) -> Result<OsmpHeader, OsmpError> {
    if bytes.get(0..4) != Some(MAGIC.as_slice()) {
        return Err(OsmpError::BadMagic);
    }
    let mut instrument_id = [0_u8; 16];
    instrument_id.copy_from_slice(&bytes[48..64]);
    Ok(OsmpHeader {
        format_major: le_u16(bytes, 4),
        format_minor: le_u16(bytes, 6),
        header_size: le_u32(bytes, 8),
        alignment: le_u32(bytes, 12),
        flags: le_u64(bytes, 16),
        file_size: le_u64(bytes, 24),
        chunk_table_offset: le_u64(bytes, 32),
        chunk_count: le_u32(bytes, 40),
        default_block_size: le_u32(bytes, 44),
        instrument_id,
    })
}

fn validate_header(header: &OsmpHeader, actual_size: u64) -> Result<(), OsmpError> {
    if header.format_major != FORMAT_MAJOR {
        return Err(OsmpError::UnsupportedVersion {
            major: header.format_major,
            minor: header.format_minor,
        });
    }
    if header.header_size < HEADER_SIZE || header.header_size as u64 > actual_size {
        return Err(OsmpError::InvalidHeaderSize(header.header_size));
    }
    if header.file_size != actual_size {
        return Err(OsmpError::FileSizeMismatch {
            declared: header.file_size,
            actual: actual_size,
        });
    }
    if !header.alignment.is_power_of_two() || !(8..=1_048_576).contains(&header.alignment) {
        return Err(OsmpError::InvalidAlignment(header.alignment));
    }
    if header.chunk_count > MAX_CHUNKS {
        return Err(OsmpError::TooManyChunks(header.chunk_count));
    }
    if header.chunk_table_offset < header.header_size as u64
        || !header
            .chunk_table_offset
            .is_multiple_of(header.alignment as u64)
    {
        return Err(OsmpError::ChunkTableOutOfBounds);
    }
    let table_size = (header.chunk_count as u64)
        .checked_mul(CHUNK_DESCRIPTOR_SIZE as u64)
        .ok_or(OsmpError::ChunkTableOverflow)?;
    let table_end = header
        .chunk_table_offset
        .checked_add(table_size)
        .ok_or(OsmpError::ChunkTableOverflow)?;
    if table_end > header.file_size {
        return Err(OsmpError::ChunkTableOutOfBounds);
    }
    Ok(())
}

fn parse_descriptor(bytes: &[u8]) -> ChunkDescriptor {
    ChunkDescriptor {
        kind: ChunkKind([bytes[0], bytes[1], bytes[2], bytes[3]]),
        flags: le_u32(bytes, 4),
        offset: le_u64(bytes, 8),
        stored_size: le_u64(bytes, 16),
        logical_size: le_u64(bytes, 24),
        alignment: le_u32(bytes, 32),
        checksum: le_u32(bytes, 36),
    }
}

fn validate_chunks(header: &OsmpHeader, chunks: &[ChunkDescriptor]) -> Result<(), OsmpError> {
    let table_end =
        header.chunk_table_offset + header.chunk_count as u64 * CHUNK_DESCRIPTOR_SIZE as u64;
    let table_range = header.chunk_table_offset..table_end;
    let mut ranges = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let alignment = if chunk.alignment == 0 {
            header.alignment
        } else {
            chunk.alignment
        };
        if !alignment.is_power_of_two() || alignment > 1_048_576 {
            return Err(OsmpError::InvalidChunkAlignment {
                kind: chunk.kind,
                alignment,
            });
        }
        if !chunk.offset.is_multiple_of(alignment as u64) {
            return Err(OsmpError::MisalignedChunk {
                kind: chunk.kind,
                offset: chunk.offset,
                alignment,
            });
        }
        let end =
            chunk
                .offset
                .checked_add(chunk.stored_size)
                .ok_or(OsmpError::ChunkOutOfBounds {
                    kind: chunk.kind,
                    offset: chunk.offset,
                    size: chunk.stored_size,
                })?;
        if chunk.offset < header.header_size as u64 || end > header.file_size {
            return Err(OsmpError::ChunkOutOfBounds {
                kind: chunk.kind,
                offset: chunk.offset,
                size: chunk.stored_size,
            });
        }
        if ranges_overlap(&(chunk.offset..end), &table_range) {
            return Err(OsmpError::ChunkOverlapsTable(chunk.kind));
        }
        ranges.push((chunk.offset, end, chunk.kind));
    }
    ranges.sort_unstable_by_key(|range| range.0);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(OsmpError::OverlappingChunks(pair[0].2, pair[1].2));
        }
    }
    Ok(())
}

fn ranges_overlap(left: &Range<u64>, right: &Range<u64>) -> bool {
    left.start < right.end && right.start < left.end
}

fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

#[derive(Debug, Error)]
pub enum OsmpError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("OSMP header is truncated")]
    TruncatedHeader,
    #[error("bad OSMP magic")]
    BadMagic,
    #[error("unsupported OSMP {major}.{minor}")]
    UnsupportedVersion { major: u16, minor: u16 },
    #[error("invalid header size {0}")]
    InvalidHeaderSize(u32),
    #[error("declared file size {declared} does not match actual size {actual}")]
    FileSizeMismatch { declared: u64, actual: u64 },
    #[error("invalid OSMP alignment {0}")]
    InvalidAlignment(u32),
    #[error("chunk count {0} exceeds parser limit")]
    TooManyChunks(u32),
    #[error("chunk table size overflows")]
    ChunkTableOverflow,
    #[error("chunk table is outside the file or misaligned")]
    ChunkTableOutOfBounds,
    #[error("chunk {kind:?} at {offset}+{size} is outside the file")]
    ChunkOutOfBounds {
        kind: ChunkKind,
        offset: u64,
        size: u64,
    },
    #[error("chunk {kind:?} has invalid alignment {alignment}")]
    InvalidChunkAlignment { kind: ChunkKind, alignment: u32 },
    #[error("chunk {kind:?} at {offset} is not aligned to {alignment}")]
    MisalignedChunk {
        kind: ChunkKind,
        offset: u64,
        alignment: u32,
    },
    #[error("chunk {0:?} overlaps the chunk table")]
    ChunkOverlapsTable(ChunkKind),
    #[error("chunks {0:?} and {1:?} overlap")]
    OverlappingChunks(ChunkKind, ChunkKind),
}

#[cfg(test)]
mod tests {
    use super::*;
    use solfege_storage::PreloadedStorage;

    fn container(descriptors: &[([u8; 4], u64, u64)], file_size: usize) -> Vec<u8> {
        let mut bytes = vec![0_u8; file_size];
        bytes[0..4].copy_from_slice(b"OSMP");
        bytes[4..6].copy_from_slice(&FORMAT_MAJOR.to_le_bytes());
        bytes[6..8].copy_from_slice(&FORMAT_MINOR.to_le_bytes());
        bytes[8..12].copy_from_slice(&HEADER_SIZE.to_le_bytes());
        bytes[12..16].copy_from_slice(&8_u32.to_le_bytes());
        bytes[24..32].copy_from_slice(&(file_size as u64).to_le_bytes());
        bytes[32..40].copy_from_slice(&128_u64.to_le_bytes());
        bytes[40..44].copy_from_slice(&(descriptors.len() as u32).to_le_bytes());
        bytes[44..48].copy_from_slice(&65_536_u32.to_le_bytes());
        for (index, (kind, offset, size)) in descriptors.iter().enumerate() {
            let start = 128 + index * CHUNK_DESCRIPTOR_SIZE;
            bytes[start..start + 4].copy_from_slice(kind);
            bytes[start + 8..start + 16].copy_from_slice(&offset.to_le_bytes());
            bytes[start + 16..start + 24].copy_from_slice(&size.to_le_bytes());
            bytes[start + 24..start + 32].copy_from_slice(&size.to_le_bytes());
            bytes[start + 32..start + 36].copy_from_slice(&8_u32.to_le_bytes());
        }
        bytes
    }

    fn parse(bytes: Vec<u8>) -> Result<OsmpFile, OsmpError> {
        let storage: Arc<dyn SampleStorage> = Arc::new(PreloadedStorage::from_bytes(bytes));
        OsmpFile::parse(storage)
    }

    #[test]
    fn accepts_unknown_skippable_chunks() {
        let file = parse(container(&[(*b"FUTR", 192, 8)], 200)).unwrap();
        assert_eq!(file.chunks[0].kind, ChunkKind(*b"FUTR"));
        assert_eq!(
            file.view_chunk(&file.chunks[0]).unwrap().as_bytes().len(),
            8
        );
    }

    #[test]
    fn rejects_chunk_offset_overflow() {
        let error = parse(container(&[(*b"META", u64::MAX - 7, 16)], 192));
        assert!(matches!(error, Err(OsmpError::ChunkOutOfBounds { .. })));
    }

    #[test]
    fn rejects_overlapping_chunks() {
        let error = parse(container(&[(*b"META", 208, 24), (*b"INST", 224, 16)], 240));
        assert!(matches!(error, Err(OsmpError::OverlappingChunks(..))));
    }
}
