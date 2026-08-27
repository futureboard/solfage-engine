# Third-party licenses

Solfege is MIT licensed. Direct runtime dependencies are selected from focused
Rust projects with compatible permissive licensing. Package metadata and
`Cargo.lock` are the exact dependency inventory for a build.

- memmap2, crossbeam-channel, thiserror: MIT or Apache-2.0 dual licensed.
- cpal: Apache-2.0.
- midir: MIT.

Plug-in SDK dependencies and optional codecs must be added to this inventory
before their features ship.
