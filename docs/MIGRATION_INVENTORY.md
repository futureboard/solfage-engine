# OpenSampler Migration Inventory

## Reusable behavior

- MIDI note-on/off and a stereo block-processing contract.
- Root-key pitch ratio: `2^((note-root+tune+cents/100)/12)`.
- Key and velocity bounds, gain/pan, loop metadata, and release behavior.
- Fixed voice capacity with oldest-voice stealing.
- ADSR stages and the requirement that all matching notes can be released.
- The drummer prototype's key-bucket zone lookup, CC condition vocabulary,
  round-robin fields, and raw S24LE/S32LE conversion knowledge.

These are reimplemented and tested in Rust; source compatibility is not kept.

## Reusable file-format knowledge

The older Rust prototype established four useful facts: OSMP needs a magic and
version, 64-bit sample offsets, packed signed 24-bit decoding, and mapping-based
raw access. Its fixed 64-byte sample table, hard capacity, embedded runtime
JSON, and rewrite-to-append workflow do not scale to the new format. The new
OSMP format is chunked, validates every offset, and skips unknown chunks.

## Reusable tests

Pitch-at-root/octave, velocity layers, CC gating, pan defaults, key-bucket
counts, and malformed/truncated container cases are useful behavioral tests.
They are being recast at the new crate boundaries.

## Obsolete architecture / delete after replacement proof

- iPlug2 application and editor base classes.
- CMake plug-in configuration and C++ host wrappers.
- C++ `SampleBank` ownership and full-file `Vec<float>` loading.
- Linear full-table zone search on every note-on.
- React/Vite/TypeScript UI and the auxiliary DSP process.
- UI and application-shell code is intentionally outside this core repository.
- Optional FluidSynth, sfizz, ZynAddSubFX, and NAM application coupling.

The `oldcode/` tree stays temporarily as a read-only migration reference and is
excluded from all builds.

## Missing legacy tests to replace before deleting reference code

- Steady-state allocation checks and callback deadline stress.
- Deterministic stealing and release-aware stealing.
- Sustain/sostenuto, choke groups, release triggers, and note retrigger.
- Loop-boundary interpolation and end-of-sample bounds.
- PCM16/24/32, Float32/64 equivalence and corrupt WAV chunks.
- OSMP overflow, overlap, alignment, unknown chunks, hash, and fuzz cases.
- mmap fallback, residency/read-ahead, and underrun accounting.
- Host state and device restart tests.
