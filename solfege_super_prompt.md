# SUPER PROMPT — Rebuild OpenSampler as **Solfege**
## Pure Rust, mmap-first, lossless-first Instrument & Sampler Player

You are rebuilding the existing **OpenSampler** project from scratch as a new product named **Solfege**.

Do **not** incrementally patch or preserve the old C++ architecture unless a specific behavior, file format detail, or algorithm is genuinely reusable. Treat the old project as a behavioral/reference source only. The new codebase must be designed as a modern, modular, production-grade, **Pure Rust 100%** sampler/instrument engine.

Primary product references for UX and capabilities:

- UVI Falcon
- Native Instruments Kontakt
- Splice Instruments

Do not clone their UI or proprietary behavior. Use them only as product-level references for workflow, library browsing, instrument mapping, modulation, performance, and scalability.

---

# 1. Product Identity

Product name:

**Solfege**

Positioning:

> A modern, Rust-native sampler player and instrument engine for standalone and plug-in use.

Primary goals:

- Extremely fast startup.
- Low memory overhead.
- Realtime-safe audio processing.
- Lossless sample storage and playback pipeline.
- mmap-first sample access.
- Huge library support.
- Modern GPU UI.
- Cross-platform architecture.
- Extensible instrument format.
- Clean separation between DSP, runtime, UI, host wrappers, and asset format.
- Able to grow beyond a simple sampler into a general instrument engine later.

The product must initially ship as:

- Standalone application
- VST3 instrument plug-in
- CLAP instrument plug-in

Design the engine so AU and DAUx can be added later without changing the core.

---

# 2. Non-Negotiable Technology Stack

## Language

Use **Rust** for the entire project.

Requirements:

- No C++ application code.
- No JUCE.
- No iPlug2.
- No Qt.
- No Electron.
- No Tauri.
- No CEF for the primary UI.
- Native/system APIs may be accessed through Rust crates or Rust FFI when required.
- Platform-specific glue must still live behind safe Rust abstractions wherever practical.

Use the latest stable Rust edition supported by the project toolchain.

---

# 3. GUI Stack

Use:

- **egui**
- **WGPU**
- **WGSL**
- **Mona Sans**
- **Remix Icon** SVG assets

The interface should feel:

- clean
- modern
- premium
- musical
- dense but readable
- fast
- hardware-inspired without skeuomorphic clutter
- suitable for both beginners and advanced sound designers

Visual direction:

- Kontakt / Falcon depth
- Splice Instruments simplicity
- modern desktop tool density
- subtle hardware feel
- high-quality analyzer and modulation visuals
- sharp typography
- minimal wasted space

Do not build the UI as a web application.

---

# 4. UI Rendering Architecture

Use egui for application structure and controls.

Use WGPU directly for rendering workloads where egui primitives are not ideal, including:

- waveform rendering
- spectrum visualization
- sample overview
- modulation scopes
- large piano keyboard visualization if needed
- GPU-heavy meters
- shader effects
- custom backgrounds
- advanced visual feedback
- future spectrogram rendering

WGSL shaders must be isolated into their own shader module hierarchy.

Example:

```text
crates/
  solfege-ui/
  solfege-render/
shaders/
  waveform.wgsl
  spectrum.wgsl
  meter.wgsl
  background.wgsl
```

Do not make shader effects mandatory for basic UI usability.

The interface must remain usable when optional visual effects are disabled.

---

# 5. Typography and Icons

## Font

Primary UI font:

**Mona Sans**

Create a centralized typography system.

Example semantic roles:

- Display
- Title
- Heading
- Body
- Label
- Caption
- Numeric
- Monospace/debug fallback

Do not scatter font sizes throughout UI code.

Create theme tokens for:

- font family
- font size
- font weight
- line height
- spacing
- radius
- stroke width
- elevation
- opacity
- interaction state

## Icons

Use SVG icons from **Remix Icon**.

Create an icon registry rather than loading arbitrary paths directly from UI widgets.

Example:

```rust
enum Icon {
    Play,
    Stop,
    Search,
    Folder,
    Settings,
    ChevronDown,
    Plus,
    Trash,
    Piano,
    Waveform,
}
```

Convert or cache SVG assets efficiently.

Do not parse the same SVG every frame.

---

# 6. Core Architectural Principle

The central rule:

> Solfege Core must not know whether it is running as Standalone, VST3, CLAP, or inside another host.

The engine must be reusable.

Target high-level architecture:

```text
Standalone
VST3
CLAP
   │
   ▼
Host Adapter Layer
   │
   ▼
Solfege Runtime
   │
   ├─ Event Engine
   ├─ Instrument Engine
   ├─ Voice Engine
   ├─ Modulation Engine
   ├─ DSP Graph
   ├─ Streaming / Mapping Engine
   ├─ OSMP Runtime
   └─ Preset / State Engine
```

The UI communicates with a runtime/controller layer.

Do not place sampler logic inside plug-in wrappers.

---

# 7. Workspace Layout

Start with a Rust workspace similar to:

```text
solfege/
├─ Cargo.toml
├─ rust-toolchain.toml
├─ LICENSE
├─ README.md
├─ crates/
│  ├─ solfege-core/
│  ├─ solfege-audio/
│  ├─ solfege-engine/
│  ├─ solfege-event/
│  ├─ solfege-voice/
│  ├─ solfege-zone/
│  ├─ solfege-modulation/
│  ├─ solfege-dsp/
│  ├─ solfege-resampler/
│  ├─ solfege-storage/
│  ├─ solfege-osmp/
│  ├─ solfege-library/
│  ├─ solfege-preset/
│  ├─ solfege-midi/
│  ├─ solfege-state/
│  ├─ solfege-ui/
│  ├─ solfege-render/
│  ├─ solfege-platform/
│  ├─ solfege-host/
│  ├─ solfege-vst3/
│  ├─ solfege-clap/
│  ├─ solfege-standalone/
│  ├─ solfege-import-sfz/
│  └─ solfege-tools/
├─ apps/
│  └─ solfege/
├─ assets/
│  ├─ fonts/
│  ├─ icons/
│  └─ themes/
├─ shaders/
├─ examples/
├─ benches/
├─ tests/
└─ docs/
```

Adjust the exact crate boundaries if necessary, but preserve strong separation of responsibilities.

Avoid a giant `solfege-core` crate containing everything.

---

# 8. Realtime Audio Rules

These rules are absolute for the audio callback.

The realtime audio thread must not:

- allocate memory
- free memory
- lock a blocking mutex
- open files
- perform filesystem metadata queries
- wait for disk I/O
- perform network I/O
- run async executors
- parse JSON
- parse TOML
- decode images
- log synchronously
- compile scripts
- rebuild graphs
- resize vectors
- perform uncontrolled page faults where avoidable

Prefer:

- fixed-capacity buffers
- preallocated voice pools
- atomics
- lock-free queues
- immutable shared state
- double-buffered state
- RCU-style state replacement where useful
- bounded work per block
- worker threads for slow work

Every realtime-sensitive API must make its safety expectations obvious.

---

# 9. Audio Processing Model

Use a block-based floating-point processing model.

Recommended internal audio representation:

- `f32` realtime DSP
- optional `f64` for offline/reference paths if useful

Create explicit types for:

```rust
SampleRate
FrameCount
ChannelCount
AudioBlock
ProcessContext
TransportState
Tempo
TimeSignature
SamplePosition
```

Do not spread raw primitive values everywhere.

Support:

- mono
- stereo

Design interfaces so multi-channel and multi-output can be added later.

---

# 10. Instrument Model

The engine must be zone/group based.

Hierarchy:

```text
Instrument
├─ Groups
│  ├─ Zones
│  ├─ Group Modulation
│  └─ Group DSP
├─ Instrument Modulation
├─ Instrument DSP
└─ Output Routing
```

A Zone must support at least:

- sample reference
- key range
- velocity range
- root key
- coarse tune
- fine tune
- gain
- pan
- start frame
- end frame
- loop start
- loop end
- loop mode
- trigger mode
- round robin index/group
- random probability
- exclusive/choke group
- release trigger
- one-shot behavior

Design conditions generically enough to later support:

- CC range
- keyswitch
- aftertouch
- MPE pressure
- MPE slide
- pitch bend state
- pedal state
- release velocity
- articulation
- sequence position

---

# 11. Voice Engine

Implement a deterministic voice allocator.

Support:

- configurable polyphony
- preallocated voice pool
- voice stealing
- oldest voice stealing
- quietest voice stealing
- release-aware stealing
- note retrigger
- mono
- poly
- legato
- sustain pedal
- sostenuto
- release triggers
- choke groups
- one-shot playback

Target initial practical polyphony:

- default: 128
- configurable: 32–1024

Do not allocate voices on note-on.

Voice state should contain only data required for realtime playback.

---

# 12. Pitch and Resampling

Create a dedicated resampling layer.

Quality modes may include:

- Draft
- Realtime
- High
- Ultra / Offline

Possible algorithms:

- linear for draft/debug only
- cubic or Hermite for lightweight realtime
- polyphase sinc for production realtime
- high-quality windowed sinc for offline/high modes

Optimize later with architecture-specific paths:

- AVX2
- AVX-512 where appropriate
- NEON
- portable scalar fallback

Never require a specific SIMD ISA to run.

Feature detection must happen safely at runtime or via appropriately separated builds.

---

# 13. Lossless-First Requirement

All sample handling is **lossless-first**.

Solfege must never silently:

- normalize
- resample
- change bit depth
- dither
- apply gain
- alter channel count
- transcode to lossy audio

Any destructive transform must be explicitly requested by the instrument author/tooling.

For stored source audio, support lossless representations.

Initial priorities:

- PCM16
- PCM24
- PCM32
- Float32
- Float64
- FLAC or an equivalent lossless block codec

Preserve decoded PCM equivalence.

A round-trip through OSMP must not alter sample PCM unless an explicit conversion option was selected.

---

# 14. `.osmp` Format

The native instrument/container format is:

**`.osmp`**

The OSMP format must support two primary modes:

1. **Monolithic container**
2. **Header/index binary referencing external sample data**

The runtime must use the same logical model for both.

---

# 15. OSMP Design Principles

OSMP must be:

- binary
- versioned
- chunked
- 64-bit offset capable
- mmap friendly
- random-access friendly
- lossless
- resilient to unknown future chunks
- capable of storing huge instruments
- safe to validate before use
- suitable for optional signing
- suitable for tooling and inspection

Do not make the runtime dependent on JSON parsing.

Human-readable authoring files may exist separately, but the runtime OSMP should be optimized binary.

---

# 16. OSMP Header

Use a small fixed-size binary header.

Conceptually:

```rust
#[repr(C)]
struct OsmpHeader {
    magic: [u8; 4],          // "OSMP"
    format_major: u16,
    format_minor: u16,
    header_size: u32,

    flags: u64,

    file_size: u64,

    chunk_table_offset: u64,
    chunk_count: u32,

    alignment: u32,
    default_block_size: u32,

    instrument_id: [u8; 16],

    reserved: [u8; N],
}
```

Do not rely on blindly transmuting untrusted bytes into structs.

Implement explicit validated parsing.

Take endianness and alignment into account.

Prefer a canonical byte order.

---

# 17. OSMP Chunks

Suggested logical chunk types:

```text
META  Metadata
INST  Instrument
GRUP  Groups
ZONE  Zones
MODS  Modulation
DSPG  DSP Graph
PRST  Presets
SCRP  Script/compiled logic
RSRC  Resources
SMPX  Sample Index
SMPD  Sample Data
UIST  UI State
SIGN  Signature
HASH  Integrity Data
```

Unknown chunks must be skippable when the format version permits it.

Chunk descriptors should use 64-bit offsets and sizes.

---

# 18. mmap-First Storage Architecture

The default storage strategy is:

> mmap first, preload only as fallback.

Preferred hierarchy:

```text
Whole-file mmap
   ↓ fallback
Windowed mmap
   ↓ fallback
Buffered file access
   ↓ fallback
Per-sample preload
   ↓ fallback
Instrument preload
```

Abstract this behind a storage interface.

Concept:

```rust
trait SampleStorage {
    fn len(&self) -> u64;
    fn view(&self, offset: u64, len: usize) -> Result<SampleView<'_>>;
    fn advise(&self, range: Range<u64>, hint: AccessHint);
    fn prefault(&self, range: Range<u64>);
}
```

Potential implementations:

- WholeFileMappedStorage
- WindowedMappedStorage
- BufferedStorage
- PreloadedStorage

The sampler/voice engine must not care which backend is active.

---

# 19. mmap Realtime Safety

Do not assume mmap automatically makes disk access realtime-safe.

A page fault can block the audio thread.

Therefore implement a **Page Residency / Mapping Manager**.

Responsibilities:

- track active voices
- predict near-future sample regions
- request/prefault pages on worker threads
- keep sample attacks resident
- maintain configurable read-ahead windows
- mark old regions cold
- adapt to storage speed
- expose underrun metrics

Conceptual path:

```text
OSMP mmap
   │
   ├─ Residency Worker
   │     ├─ prefault
   │     ├─ access hints
   │     └─ read-ahead
   │
   └─ Audio Thread
         └─ reads expected-resident pages
```

The audio callback must not intentionally trigger new disk I/O.

---

# 20. Attack Residency Instead of Traditional Preload

For mapped raw PCM, avoid duplicating attack data into a separate preload buffer.

Instead:

- map the sample
- prefault/touch the first configurable region
- allow the OS page cache to provide residency

Example defaults can be adaptive.

Potential attack residency size:

- 64 KiB
- 128 KiB
- 256 KiB
- 512 KiB

Do not hardcode one value globally.

---

# 21. Adaptive Read-Ahead

Implement storage-aware read-ahead.

Possible tiers:

### Fast NVMe
- small ahead window

### SATA SSD
- medium ahead window

### HDD
- larger ahead window

### Network/removable/unreliable storage
- very large window or fallback backend

Track:

- average read latency
- worst-case latency
- underrun count
- active voice count
- bytes/sec demand

Use bounded adaptation.

Never make realtime behavior depend on a complex uncontrolled predictor.

---

# 22. Raw and Compressed Lossless Sample Modes

Support two storage paths.

## RawMapped

```text
mmap
→ PCM reader
→ sample converter
→ resampler
→ voice DSP
```

This is the preferred low-overhead runtime mode.

## LosslessCompressedMapped

```text
mmap
→ indexed compressed block
→ decode worker
→ bounded decode cache
→ resampler
→ voice DSP
```

Lossless compressed samples must be broken into independently decodable blocks.

Do not store an entire massive sample as one sequential compressed stream requiring decode from frame zero.

---

# 23. Compression Rules

If compression is used:

- lossless only
- block based
- random-access capable
- checksummed
- independently decodable
- configurable block size

Potential block sizes:

- 64 KiB
- 128 KiB
- 256 KiB
- 512 KiB

Benchmark before choosing the default.

The format must allow future codecs without redesigning OSMP.

---

# 24. Modulation Engine

Do not hardcode fixed modulation routes.

Implement a generic modulation matrix/graph.

Sources:

- velocity
- note/key
- key tracking
- CC
- pitch bend
- channel pressure
- poly pressure
- MPE pressure
- MPE slide
- MPE pitch
- envelope
- LFO
- random
- macro
- step modulator later
- audio follower later

Destinations:

- pitch
- gain
- pan
- filter cutoff
- resonance
- sample start
- loop position
- envelope parameters
- effect parameters
- group parameters
- macro values

A route should conceptually contain:

```rust
ModulationRoute {
    source,
    destination,
    amount,
    transform,
    smoothing,
    polarity,
}
```

Avoid rebuilding modulation topology in the audio callback.

---

# 25. Envelopes

At minimum:

- Amp ADSR
- Mod ADSR

Design the envelope engine for future:

- multi-stage envelopes
- breakpoint envelopes
- tempo-sync
- curve control

Realtime-safe and sample-rate aware.

---

# 26. LFO

Support:

- sine
- triangle
- saw
- square
- random/sample-and-hold

Controls:

- rate
- sync/free
- phase
- retrigger
- fade-in
- polarity

Plan for custom shapes later.

---

# 27. Filters

Initial filter set should be small but high-quality.

At least:

- low-pass
- high-pass
- band-pass
- notch

Prefer a stable topology such as SVF/TPT for initial implementation.

Add more character models later.

Avoid putting a giant synth filter collection into V1 before the base sampler is stable.

---

# 28. DSP Graph

Support processing scopes:

```text
Voice DSP
→ Group DSP
→ Instrument DSP
→ Output
```

Initial modules:

- filter
- EQ
- compressor
- saturation
- chorus
- delay
- reverb
- stereo utility
- limiter

Do not require all DSP modules to be complete before the basic player works.

The DSP graph must be built outside the audio callback and swapped safely.

---

# 29. MIDI and Event Engine

Create an internal event representation independent of VST3 and CLAP event types.

Support:

- Note On
- Note Off
- Poly Pressure
- Channel Pressure
- CC
- Pitch Bend
- Sustain
- Sostenuto
- Program/Preset events where appropriate
- host transport info
- basic MPE

Convert host-specific events into Solfege events at the adapter boundary.

---

# 30. Parameter System

Create stable parameter IDs.

Requirements:

- deterministic
- automation-safe
- preset-safe
- host-safe
- versionable

Separate:

- engine parameters
- instrument parameters
- macro parameters
- UI-only state

Do not serialize transient UI state as audio parameter automation.

Implement smoothing for parameters that can click or zipper.

---

# 31. Presets and State

Support:

- factory presets
- user presets
- host state serialization
- instrument state
- macro state
- UI state where appropriate

The same instrument state must restore consistently in:

- Standalone
- VST3
- CLAP

State loading must not perform destructive work on the audio callback.

Use staged loading and atomic state publication.

---

# 32. Library System

Create a local library index.

Possible metadata:

- library ID
- product
- author
- instrument name
- category
- tags
- mood
- artwork
- version
- OSMP path
- preset count
- required engine version

Use an efficient local database/index.

SQLite is acceptable if isolated from realtime code.

Library categories should support at least:

- Piano
- Keys
- Bass
- Guitar
- Strings
- Brass
- Woodwinds
- Drums
- Percussion
- Synth
- World
- FX
- Vocal
- Experimental

---

# 33. SFZ Import

Implement SFZ import as a separate crate.

Important rule:

> Solfege Runtime does not become an SFZ runtime.

Instead:

```text
SFZ
→ parser/importer
→ Solfege Instrument Model
→ optional OSMP compile
→ runtime
```

Support a practical subset first, then expand.

Document unsupported opcodes clearly.

Later importers may include:

- SF2
- DecentSampler
- other open formats

Do not attempt proprietary Kontakt library compatibility.

---

# 34. Standalone Application

The standalone app must provide:

- audio device selection
- MIDI input selection
- sample rate selection
- buffer size selection
- instrument/library browser
- preset browser
- virtual keyboard
- performance meters
- diagnostics
- settings
- drag/drop OSMP loading

Abstract platform audio through a dedicated layer.

Do not entangle the standalone audio backend with the core engine.

---

# 35. VST3

Implement a VST3 instrument target.

Requirements:

- instrument plug-in
- stereo output initially
- MIDI/note input
- host automation
- host state save/restore
- editor resize
- DPI scaling
- reliable open/close behavior
- no engine duplication in wrapper code

Use Rust-native implementation/bindings where possible.

Do not introduce C++ project files just to wrap the engine.

If a system/compiler SDK component is unavoidable, keep it outside the Solfege application architecture and document the boundary.

---

# 36. CLAP

Implement CLAP as a first-class target.

Support:

- note events
- parameter automation
- state
- GUI
- resize
- host lifecycle
- latency reporting if later required

Keep the CLAP adapter thin.

---

# 37. GUI Information Architecture

The default user experience should be approachable.

Suggested primary layout:

```text
┌──────────────────────────────────────────────────────┐
│ Solfege                         Search      Settings │
├─────────────┬────────────────────────────────────────┤
│ Library     │ Instrument / Performance View          │
│ Browser     │                                        │
│             │ Artwork / Main Controls                │
│             │                                        │
│             │ Macros / Performance Controls          │
│             │                                        │
│             │ Keyboard / Status                      │
└─────────────┴────────────────────────────────────────┘
```

Primary modes:

- Play
- Edit
- Mapping
- Modulation
- Effects
- Advanced

A normal musician should be able to load and play an instrument without seeing complex mapping internals.

An advanced user should be able to drill down into the complete instrument.

---

# 38. Mapping Editor

Provide an advanced mapping page with:

- piano keyboard
- zone rectangles
- velocity axis
- drag zone ranges
- resize zone ranges
- multi-select
- root-note visualization
- sample list
- round-robin grouping
- velocity layer grouping
- audition
- zoom/pan

For thousands of zones, do not instantiate expensive UI widgets for every zone if virtualization/batched rendering is more appropriate.

Use WGPU when it materially improves large mapping visualization.

---

# 39. Waveform Editor

Provide:

- waveform
- sample start/end
- loop start/end
- loop enable
- zero-crossing helpers
- zoom
- pan
- audition
- root key
- tuning
- gain
- normalize command only as an explicit destructive authoring operation

Rendering should remain smooth for long samples.

Do not redraw/re-upload an entire waveform unnecessarily each frame.

Precompute waveform overview levels.

---

# 40. Performance Page

The simple performance page should emphasize:

- artwork
- instrument name
- preset
- 4–8 macro knobs
- output level
- voice count
- CPU
- disk/mmap activity
- virtual keyboard

This is the Splice-Instruments-like simple path.

Advanced controls remain available behind deeper pages.

---

# 41. Theme System

Build a theme/token layer.

Do not hardcode appearance across widgets.

Theme should include:

- background hierarchy
- panel surfaces
- text hierarchy
- accent
- warning
- error
- success
- border
- control states
- knob/fader visuals
- selection
- focus

Support future light/dark/custom themes.

Initial design can be dark-first.

---

# 42. Shader Policy

WGSL is for visual rendering only unless a later GPU audio experiment is explicitly requested.

Do not move realtime audio DSP to GPU in V1.

WGSL modules must:

- compile at startup/build validation
- expose clear uniforms/bind groups
- have CPU fallback or disabled state for nonessential effects
- avoid gratuitous visual cost

Prioritize UI responsiveness over eye candy.

---

# 43. Threading Model

Recommended thread groups:

```text
Audio Thread
UI/Main Thread
Mapping/Residency Workers
Lossless Decode Workers
Library/Database Worker
Background Analysis Workers
```

Potential background tasks:

- sample metadata scan
- waveform overview generation
- checksum validation
- artwork decode
- library indexing
- OSMP compile
- compressed block decode
- mmap prefault/read-ahead

Use bounded worker pools.

Do not spawn unbounded threads per voice/sample.

---

# 44. Communication Between Threads

Use:

- bounded channels
- lock-free queues where justified
- atomics for tiny state
- immutable snapshots
- generation IDs
- cancellation tokens for background jobs

Avoid broad shared mutable state.

A UI action that changes the instrument should produce a new validated state/graph and publish it safely.

---

# 45. Error Handling

Create typed errors by subsystem.

Examples:

- OsmpError
- StorageError
- DecodeError
- InstrumentError
- HostError
- AudioDeviceError
- LibraryError

Do not use `unwrap()` in production runtime paths.

`expect()` is acceptable only for programmer invariants that cannot be triggered by user data, and should be rare.

Untrusted OSMP data must be fully bounds checked.

---

# 46. Security and Validation

Treat OSMP files as untrusted input.

Validate:

- magic
- version
- header size
- file size
- offset overflow
- chunk overlap
- chunk bounds
- integer overflow
- decompressed size
- block count
- sample metadata
- resource size
- string sizes
- index references

Do not allow malformed files to cause out-of-bounds reads through mmap.

Do not trust internal offsets before validation.

Fuzz the OSMP parser.

---

# 47. OSMP Signing

Design the format so signing can be added.

Possible use cases:

- official Futureboard/Solfege libraries
- marketplace integrity
- tamper detection

Do not make signing mandatory for open/community libraries.

Keep integrity hashes separate from optional publisher identity/signatures.

---

# 48. Performance Metrics

Expose runtime metrics:

- active voices
- peak voices
- CPU render time
- render deadline percentage
- mapped bytes
- resident estimate if available
- decode cache usage
- decode worker utilization
- underrun count
- page/read misses where observable
- output peak
- output RMS
- sample rate
- block size

Diagnostics must not spam logs from realtime code.

Use counters sampled by the UI.

---

# 49. Benchmarks

Create benchmarks for:

- voice mixing
- resampling
- zone lookup
- note-on allocation
- round robin selection
- modulation evaluation
- raw mapped PCM access
- lossless decode blocks
- OSMP parsing
- preset/state serialization
- large zone tables

Test representative loads:

- 32 voices
- 128 voices
- 256 voices
- 512 voices
- 1024 voices

Test libraries with:

- 100 zones
- 1,000 zones
- 10,000 zones
- 50,000+ zones

---

# 50. Testing

Required test categories:

## Unit
- parsers
- zone matching
- envelopes
- modulation
- allocator
- sample addressing
- loop boundaries

## Integration
- Standalone engine
- VST3 state
- CLAP state
- OSMP load
- external-sample OSMP
- monolithic OSMP

## Golden/reference
- known resampler output
- known envelope output
- known modulation output
- state serialization

## Stress
- rapid note spam
- sustain-heavy piano
- thousands of zone matches
- repeated instrument load/unload
- corrupted OSMP
- storage fallback transitions

## Fuzz
- OSMP header
- chunk table
- sample index
- compressed block index

---

# 51. Logging and Diagnostics

Use structured logging outside realtime paths.

Recommended levels:

- error
- warn
- info
- debug
- trace

Subsystem tags:

- engine
- audio
- osmp
- mmap
- decode
- library
- vst3
- clap
- ui

Realtime thread should write only into lightweight counters/ring diagnostics if needed.

---

# 52. Build Profiles

Provide tuned Cargo profiles.

Examples:

- dev
- release
- profiling
- distribution

Release should consider:

- LTO
- codegen units
- panic strategy
- stripping where appropriate

Do not make debug builds unusably slow to iterate on.

---

# 53. Feature Flags

Use feature flags sparingly.

Potential flags:

```text
standalone
vst3
clap
sfz
lossless-flac
simd
diagnostics
```

Do not create a combinatorial explosion of tiny optional features.

Core behavior must remain testable.

---

# 54. Platform Targets

Primary:

- Windows x86_64
- macOS Apple Silicon
- macOS Intel where practical
- Linux x86_64

Design SIMD abstraction with future ARM/NEON in mind.

Do not make Windows-specific assumptions inside core crates.

---

# 55. Accessibility and Input

The egui interface must support:

- keyboard navigation where practical
- sensible focus
- high-DPI displays
- mouse wheel
- trackpad scrolling
- drag precision
- shift/ctrl modifiers
- text scaling
- clear hover/focus/active states

Avoid controls that only work via tiny drag targets.

---

# 56. Plugin Window Requirements

The editor must:

- resize smoothly
- scale correctly on high DPI
- preserve state on close/reopen
- not leak GPU resources
- not recreate the entire engine when the editor opens
- allow audio engine operation while UI is closed

UI lifetime and DSP lifetime must be separate.

---

# 57. Instrument Loading Model

Instrument loading should be staged:

```text
Request
→ validate OSMP
→ map/index
→ resolve metadata
→ prepare sample storage
→ prefault attack regions
→ build runtime graph
→ publish new instrument
```

The currently playing instrument must remain valid until the replacement is ready.

Do not tear down the active instrument immediately when the user selects another file.

Use generation IDs/cancellation to prevent stale loads from winning races.

---

# 58. Hot Reload for Development

In development builds, support optional hot reload of:

- authoring instrument metadata
- UI theme
- WGSL shaders
- selected resources

Do not make hot reload part of production realtime behavior.

---

# 59. Authoring vs Runtime Data

Separate authoring data from optimized runtime data.

Authoring representation may be convenient/verbose.

Runtime OSMP representation must be:

- compact
- validated
- indexed
- efficient
- directly usable without expensive transformation

Provide an `osmpc` compiler/tool:

```text
osmpc build instrument-project/ -o Piano.osmp
osmpc inspect Piano.osmp
osmpc verify Piano.osmp
osmpc extract Piano.osmp out/
```

Potential future commands:

```text
osmpc benchmark
osmpc sign
osmpc migrate
```

---

# 60. OSMP Monolithic Mode

A single `.osmp` may contain:

- metadata
- instrument graph
- presets
- artwork
- UI resources
- samples
- indexes
- hashes
- optional signature

A 100 GB monolithic OSMP must be valid in principle.

Do not read it all into memory.

---

# 61. OSMP Header/External Mode

An `.osmp` may reference external sample files.

Use this mode for:

- development
- editable libraries
- source trees
- large authoring projects

External references must be relocatable where possible.

Support:

- relative paths
- content hashes
- optional search roots

Do not silently bind absolute developer-machine paths into distributable libraries.

---

# 62. Sample Lookup

Zone matching must be efficient.

Do not scan all zones for every note event.

Index by relevant dimensions.

Potential approach:

- key bucket
- velocity intervals
- condition lists
- precomputed trigger tables

Round-robin and random conditions should run only on the candidate set.

Keep note-on bounded and predictable.

---

# 63. Round Robin

Implement deterministic and optional randomized round robin.

Features:

- sequential
- random without immediate repeat
- reset on transport/start
- reset after timeout
- per-group counters

Ensure preset/state restore does not accidentally produce broken counters.

---

# 64. Looping

Support:

- no loop
- forward loop
- sustain loop
- release-aware loop

Future:

- ping-pong
- crossfade loop

Handle interpolation correctly around loop boundaries.

Avoid clicks at loop wrap.

---

# 65. Sample Start and Release Trigger

Support configurable sample start offset.

Release-trigger zones should receive:

- original note
- release velocity when available
- held duration if useful later

Prevent runaway release voice stacking.

---

# 66. MPE

Initial MPE support:

- per-note pitch
- per-note pressure
- per-note timbre/slide

Map into internal note-expression state.

Do not make the voice engine directly parse host-specific MPE structures.

---

# 67. Future Expansion Hooks

Architectural extension points should allow later addition of:

- granular source
- wavetable source
- oscillator source
- physical modeling source
- time stretching
- warp markers
- convolution
- multi-mic mixer
- articulation management
- step sequencer
- arpeggiator
- script engine
- custom instrument UI
- multi-output
- surround
- library marketplace integration

Do not implement all of these in V1.

Only make sure V1 does not block them.

---

# 68. Sound Source Abstraction

Long-term, avoid hard-coding the entire architecture around samples.

A conceptual future interface may resemble:

```rust
trait SoundSource {
    fn note_on(&mut self, event: NoteEvent);
    fn note_off(&mut self, event: NoteEvent);
    fn process(&mut self, ctx: &ProcessContext, output: &mut AudioBlock);
}
```

Initial implementation:

- SampleSource

Future implementations:

- GranularSource
- WavetableSource
- OscillatorSource
- PhysicalModelSource

Do not over-generalize V1 to the point that the sample engine becomes hard to optimize.

---

# 69. Coding Style

Prefer:

- explicit ownership
- small modules
- descriptive types
- documented invariants
- bounded APIs
- minimal unsafe
- benchmarks for unsafe/SIMD changes

When unsafe is required:

- isolate it
- document why it is safe
- test it
- fuzz boundary parsers
- do not expose unsafe assumptions across crates

---

# 70. Dependency Policy

Prefer mature, focused Rust crates.

Avoid dependency bloat.

Before adding a dependency, ask:

1. Is it maintained?
2. Is the license compatible?
3. Is the runtime overhead acceptable?
4. Does it allocate in realtime paths?
5. Can a small internal implementation be safer/simpler?
6. Does it pull in large unrelated frameworks?

Keep license metadata documented.

---

# 71. First Development Milestone

The first milestone is **not** “beautiful UI”.

Milestone 1 must prove the architecture.

Required:

- workspace compiles
- standalone app launches
- audio output works
- MIDI note input works
- one WAV/PCM sample maps through mmap
- sample plays across keyboard pitch
- ADSR works
- voice allocation works
- basic polyphony works
- no allocations in steady-state render path
- minimal egui UI
- diagnostics show voice count and CPU
- basic `.osmp` header/index parser exists

---

# 72. Milestone 2

Add:

- multi-zone instruments
- key ranges
- velocity layers
- root notes
- round robin
- release trigger
- looping
- instrument switching
- OSMP monolithic raw PCM
- OSMP external sample mode
- mmap fallback strategy
- attack prefault/residency worker
- waveform overview
- basic library browser

---

# 73. Milestone 3

Add:

- modulation matrix
- filters
- LFO
- mod envelope
- macro controls
- lossless compressed OSMP blocks
- decode worker/cache
- VST3 target
- CLAP target
- host automation
- host state restore

---

# 74. Milestone 4

Add:

- advanced mapping editor
- SFZ importer
- preset browser
- library index
- artwork/resources
- DSP graph
- effects
- more diagnostics
- optimized WGPU waveform/mapping renderer

---

# 75. Milestone 5

Production hardening:

- parser fuzzing
- crash recovery paths
- corrupted file handling
- stress tests
- memory leak tests
- plug-in lifecycle tests
- DPI tests
- Windows/macOS/Linux validation
- performance profiling
- storage profiling
- SIMD optimization
- installer/distribution integration
- documentation

---

# 76. V1 Product Scope

Target V1 feature set:

- Pure Rust engine
- Standalone
- VST3
- CLAP
- egui UI
- WGPU rendering
- WGSL visual shaders
- Mona Sans
- Remix Icon SVGs
- `.osmp`
- monolithic OSMP
- external-sample OSMP
- lossless-first sample storage
- mmap-first playback
- preload fallback
- adaptive residency/read-ahead
- raw PCM
- lossless compressed blocks
- polyphony
- voice stealing
- velocity layers
- round robin
- release triggers
- looping
- ADSR
- LFO
- filter
- modulation matrix
- MIDI CC
- basic MPE
- presets
- library browser
- mapping editor
- waveform editor
- SFZ import

---

# 77. Explicit Non-Goals for Early V1

Do not delay the core engine for:

- Kontakt proprietary library import
- Falcon proprietary library import
- GPU audio DSP
- scripting language
- granular synthesis
- wavetable synthesis
- physical modeling
- surround
- huge effect catalog
- cloud marketplace
- collaboration
- DRM
- encrypted libraries
- complex custom UI scripting

Leave clean extension points only.

---

# 78. Acceptance Criteria

The architecture is acceptable when:

1. The same Solfege engine runs in Standalone, VST3, and CLAP.
2. No sampler logic is duplicated across host wrappers.
3. Large OSMP files can be opened without loading them into RAM.
4. mmap is the preferred backend.
5. preload works as a fallback.
6. audio sample storage is lossless.
7. realtime audio does not perform blocking filesystem I/O.
8. active sample regions are prefaulted/read ahead outside the audio thread.
9. note-on does not allocate memory.
10. steady-state audio render does not allocate memory.
11. 128+ voices are practical on normal modern desktop hardware.
12. malformed OSMP files fail safely.
13. UI can close while DSP keeps running.
14. host state restores correctly.
15. egui remains responsive while libraries load.
16. WGPU resources are recreated safely on device/surface events.
17. the codebase contains no C++ application layer.
18. the core engine has no dependency on VST3, CLAP, or egui.
19. large zone counts do not require a linear full-table scan on every note.
20. the project is structured so future instrument sources can be added.

---

# 79. Engineering Constitution

Treat these as the permanent rules of Solfege:

> **No lossy transform unless explicitly requested.**

> **No sample copy unless required.**

> **mmap first, preload as fallback.**

> **No blocking disk I/O on the audio thread.**

> **No host-specific logic in the instrument engine.**

> **No UI lifetime dependency for DSP operation.**

> **No giant monolithic crate.**

> **No unsafe code without a documented invariant.**

> **No unvalidated offset from an OSMP file is ever dereferenced.**

> **Measure before optimizing, but design the realtime boundaries correctly from day one.**

---

# 80. Implementation Instructions

Begin by inspecting the current OpenSampler repository.

Produce a short migration inventory:

- reusable behavior
- reusable tests
- reusable file-format knowledge
- obsolete architecture
- C++-specific components to delete
- missing tests that should be created before behavior is replaced

Then create the new Rust workspace.

Do not spend time preserving source compatibility with the old C++ code.

When replacing subsystems:

1. Define the Rust interface.
2. Add tests.
3. Implement a minimal correct path.
4. Integrate it.
5. Profile it.
6. Optimize only after correctness.
7. Remove the old implementation once the replacement is proven.

Keep the repository buildable at every meaningful checkpoint.

Do not leave large batches of commented-out legacy code.

---

# 81. Required Initial Deliverables

Create:

1. `ARCHITECTURE.md`
2. `docs/OSMP_FORMAT.md`
3. `docs/REALTIME_RULES.md`
4. `docs/STORAGE_AND_MMAP.md`
5. `docs/PLUGIN_ARCHITECTURE.md`
6. Rust workspace and crate skeleton
7. minimal standalone application
8. minimal audio callback
9. minimal MIDI event path
10. minimal voice engine
11. minimal mmap storage backend
12. preload fallback backend
13. minimal OSMP parser
14. minimal egui interface
15. WGPU/WGSL rendering module skeleton
16. VST3 wrapper skeleton
17. CLAP wrapper skeleton
18. unit test structure
19. benchmark structure
20. CI build matrix

---

# 82. Final Target

Solfege should eventually feel like:

- the accessibility of Splice Instruments
- the sampler depth of Kontakt
- the modular potential of Falcon
- with a modern Rust-native architecture
- lossless OSMP libraries
- mmap-first playback
- realtime-safe DSP
- GPU-accelerated native UI
- one engine shared by standalone and plug-in targets

But the implementation must remain its own product and architecture.

The most important objective is not feature count.

The most important objective is:

> **A clean, fast, reliable instrument runtime that can scale from one-shot samples to massive professional libraries without turning the codebase into a legacy sampler again.**

Build that foundation first.
