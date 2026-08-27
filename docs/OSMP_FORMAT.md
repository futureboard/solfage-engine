# OSMP 1.0 Binary Format

All integers are canonical little-endian. Files are untrusted input. Readers
must use explicit decoding and validate multiplication, addition, ranges,
alignment, overlap, and references before exposing any mapped view.

## Fixed header (128 bytes)

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | ASCII `OSMP` |
| 4 | 2 | major version |
| 6 | 2 | minor version |
| 8 | 4 | header size (at least 128) |
| 12 | 4 | default power-of-two alignment |
| 16 | 8 | flags |
| 24 | 8 | exact file size |
| 32 | 8 | chunk table offset |
| 40 | 4 | chunk count |
| 44 | 4 | default independently decodable block size |
| 48 | 16 | instrument UUID bytes |
| 64 | 64 | reserved, zero in version 1 writers |

## Chunk descriptor (40 bytes)

| Offset | Size | Field |
| ---: | ---: | --- |
| 0 | 4 | FourCC kind |
| 4 | 4 | flags / codec ID namespace |
| 8 | 8 | stored-data offset |
| 16 | 8 | stored byte size |
| 24 | 8 | logical/uncompressed size |
| 32 | 4 | alignment, or zero for header default |
| 36 | 4 | checksum field (algorithm selected by flags) |

Known kinds are `META`, `INST`, `GRUP`, `ZONE`, `MODS`, `DSPG`, `PRST`,
`SCRP`, `RSRC`, `SMPX`, `SMPD`, `UIST`, `SIGN`, and `HASH`. Unknown kinds are
skipped when the major version is supported. Chunks may not overlap the header,
table, or each other.

`SMPX` will describe either raw mapped PCM extents in `SMPD`, independently
decodable lossless blocks, or relocatable external paths plus content hashes.
Runtime metadata is binary; authoring JSON/TOML is compiled before publication.

Large monolithic files are valid because every size and offset is 64-bit and a
reader maps/views only the required ranges. Optional signatures identify a
publisher; integrity hashes remain independently usable by open libraries.

