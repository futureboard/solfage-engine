# Solfege Core

Solfege is a Rust-native sampler and instrument runtime. This repository now
contains the core libraries and host/format adapters only; the standalone app,
egui UI, and GPU presentation layer have been removed.

## Verify

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The realtime engine is built from immutable prepared samples, bounded event and
voice state, and mmap-first storage. Host integrations translate their events
at the adapter boundary without adding UI or application-shell dependencies.

The same engine now also exposes a first-class physical backend. Construct a
`RuntimeInstrument::bowed_string(...)` to render the generic bowed-string
waveguide through the existing `solfege-engine`/`solfege-voice` path; sample
instruments remain unchanged. Enable the optional `fbmx` feature to install
the repository's pure-Rust `fbmx-runtime` performer/residual hooks.

Render a deterministic offline reference sequence with:

```powershell
cargo run -p solfege-tools --bin solfege-render -- bowed-string bowed-string-reference.wav 8
```

The legacy OpenSampler sources remain under `oldcode/` as a behavior and file
format reference only. They are not members of the Rust workspace.
