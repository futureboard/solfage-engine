# Plug-in Architecture

VST3 and CLAP adapters depend on `solfege-host` and `solfege-engine`; the engine
does not depend on either plug-in API. Adapters translate notes, automation,
transport, state, and buffers into Solfege types. The same serialized engine
state is used by both plug-in formats.

The plug-in instance owns DSP/runtime state. Editor and presentation concerns
are outside this core repository; they must not be required for DSP operation.

The current VST3 and CLAP crates are compile-checked lifecycle skeletons. They
deliberately contain no C++ shim and no duplicate voice or sampler logic. SDK
entry points, host compliance tests, binary packaging, and state wiring are the
next adapter milestone.
