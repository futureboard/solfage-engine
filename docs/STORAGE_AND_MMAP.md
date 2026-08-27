# Storage and mmap

`SampleStorage` exposes immutable, checked byte views. `MappedStorage` maps a
whole file and is preferred. `PreloadedStorage` is the explicit fallback and
uses the same interface, so voices do not know the backend.

The storage hierarchy is whole-file mmap, windowed mmap, buffered access,
per-sample preload, then instrument preload. This milestone implements the
first and last endpoints; the trait preserves the intermediate seams.

WAV loading scans RIFF chunks without converting sample data. Supported input
representations are PCM16/24/32 and Float32/64. Frame reads convert only into
the engine's realtime `f32`; bytes on disk are never normalized, resampled, or
rewritten. Invalid chunk bounds, block alignment, channel count, and offsets
fail before publication.

`prefault(range)` touches one byte per OS page on a non-realtime thread. It is
an explicit preparation operation, not a callback-side guarantee. Future
residency workers will combine voice position, attack pinning, storage latency,
and bounded read-ahead tiers while publishing underrun counters.

