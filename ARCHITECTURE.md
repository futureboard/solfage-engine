# Solfege Core Architecture

Solfege has one host-independent sampler runtime. Host and format adapters
translate events and state at their boundary; none of them owns sampler
behavior.

```text
MIDI / MPE / native -> timed semantic events -> solfege-engine
                                                  |
                                                  v
                                          modulation / GestureState
                                                  |
                                                  v
                                           solfege-voice pool
                                             /             \
                                            /               \
                                   sample voice       physical voice
                                  PCM/resampler       bowed string DSP
                                        |                 |
                                        +-------> audio bus
                                                  |
                                         optional FBMX residual
```

## Crate boundaries

- `solfege-audio`: strongly typed sample-rate, frame, channel, block, and
  process-context primitives.
- `solfege-event`: host-neutral, sample-offset event vocabulary.
- `solfege-zone`: authoring instrument/zone model and 128-key candidate index.
- `solfege-storage`: bounded byte views, mmap and preload backends, WAV layout
  validation, and explicit prefault hints.
- `solfege-core`: immutable prepared samples and runtime instruments.
- `solfege-resampler`: interpolation policy with scalar fallback.
- `solfege-modulation`: semantic control routes and fixed-capacity gesture/
  parameter ramps.
- `solfege-dsp`: reusable DSP contracts plus the fractional waveguide, bow
  friction, modal body, noise, and radiation primitives.
- `solfege-voice`: preallocated common voice slots, ADSR, deterministic
  stealing, sample rendering, and physical voice state.
- `solfege-engine`: event handling, sustain behavior, physical/sample
  dispatch, rendering, optional FBMX hooks, and atomic diagnostics.
- `solfege-platform`: native audio/MIDI boundary; it owns device resources.
- `solfege-host`, `solfege-vst3`, `solfege-clap`: thin host contracts and
  lifecycle skeletons.
- `solfege-osmp`: canonical little-endian, chunked OSMP parser.

`solfege-engine` has an opt-in `fbmx` feature that depends on the existing
repository-level `fbmx-runtime` crate. Model loading/instantiation happens on
the control side. `FbmxHooks` exposes a performer curve hook and a bypassable
residual path; it does not introduce an ML graph executor or a second plugin
DSP implementation.

## Realtime ownership

Instruments are opened, validated, mapped, indexed, and prefaulted off the
audio thread. A staged loader can use generation IDs plus deferred reclamation;
it must not destroy an old graph on the audio thread.

The callback uses a fixed voice pool and bounded event queue. Steady-state
rendering performs no heap allocation, locking, logging, parsing, or file I/O.
Mapped reads are only made after an explicit residency preparation step.

## Extension seams

Voice, group, instrument, and output DSP scopes are separate concepts. Sample
and physical backends share the same event scheduler, gesture state, voice
allocator, and host path. `RuntimeInstrument::prepare` preserves sample/SFZ/
OSMP behavior; `RuntimeInstrument::bowed_string` selects the new backend, and
`prepare_hybrid` reserves the combined representation without changing the
sample format. Multi-output and additional host formats extend adapters rather
than adding a second engine.

## Physical approximation

The first physical model is intentionally generic rather than violin-specific:
pitch is continuous Hz, bow pressure/velocity/position/direction are semantic
controls, and body modes are configurable. The string is a fractional delay
line with damped feedback; bow contact uses a saturating `tanh` friction curve;
bridge output drives four configurable damped second-order modes; a one-pole
high-pass stage approximates radiation/microphone coloration. These choices
are stable and controllable, but are not physically exact.
