# Realtime Rules

The audio callback is a bounded numerical routine. It must not allocate or
free, block on a mutex, access filesystem metadata, wait for storage/network,
parse, log synchronously, spawn work, resize containers, or rebuild state.

Allowed operations are fixed-capacity voice/event iteration, atomics, reading
immutable prepared state, and reading sample pages that workers have made
resident. APIs named `open`, `prepare`, `index`, `prefault`, `load`, `compile`,
or `replace` are control-thread APIs.

Physical voices follow the same rule: waveguide, modal-body, noise, envelope,
and gesture-interpolation state are allocated when the voice pool is prepared.
Bow/pitch gestures only update fixed state and ramps during `process()`.
Optional FBMX runtimes are instantiated before activation; a disabled performer
or residual hook is a branch with no inference work.

Every callback receives caller-owned output memory. It clears and fills that
memory in-place. Note-on selects from a prebuilt 128-key index and writes into
an existing voice slot. Diagnostics are atomics sampled by the control or host
layer.

Mapped storage is not intrinsically realtime safe: a missing page may block.
The control layer must prefault attack data before starting the stream.
Adaptive read-ahead and generation-safe instrument replacement remain worker
responsibilities.
