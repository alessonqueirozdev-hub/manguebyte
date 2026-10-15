# Roadmap

Manguebyte aims to become the complete equivalent of JUCE for Rust. That is
years of work, so this document splits it into phases. Each phase leaves the
framework useful for a wider set of projects than the one before.

Nothing here is a promise or a schedule. It is a statement of direction, and it
changes as people show up and build things. If an item interests you, open an
issue and say so.

Legend: ✅ done · 🚧 in progress · ⬜ not started

## Phase 0: foundation (v0.1) ✅

The types everything else is built on, and enough around them to make sound.

- ✅ `manguebyte-core`: `AudioBuffer`, `Sample`, `Smoothed`, atomic floats,
  wait-free SPSC queue, decibel conversions
- ✅ `manguebyte-dsp`: `Processor` trait, compile-time tuple chains,
  band-limited oscillator, biquad filters, ADSR, delay line and echo, gain
- ✅ `manguebyte-midi`: channel voice messages, sample-accurate `MidiBuffer`
- ✅ `manguebyte-formats`: WAV reading and writing
- ✅ `manguebyte-devices`: playback on the default output device
- ✅ `manguebyte`: facade crate and prelude

## Phase 1: a complete audio toolbox

Everything a standalone audio application needs, short of a user interface.

**Devices**
- ✅ Audio input and full-duplex streams
- ✅ Device enumeration and selection; choosing sample rate and buffer size
- ⬜ Hot-plug and default-device change notifications
- ⬜ Clock-drift compensation between duplex devices by adaptive resampling
  (the resampler it needs now exists)
- ⬜ Choosing the audio host (ASIO, JACK, PipeWire)
- ✅ MIDI input and output ports, with jitter-free timing into the audio
  thread

**MIDI**
- ✅ System common, system real-time and SysEx messages
- ✅ Running status and a byte-stream parser
- ✅ Standard MIDI File reading and writing, with tempo-aware timing
- ✅ MIDI 2.0: Universal MIDI Packets with every message type, and MIDI 1.0
  to 2.0 translation as the specification prescribes
- ✅ RPN and NRPN, MPE zones, channel allocation and per-note tracking, and
  a keyboard state an interface can read while the audio thread writes
- ⬜ MIDI 2.0 and MPE delivered to processors as per-note events, and
  through the device and plugin layers

**DSP**
- ✅ FFT of any length, real FFT and window functions
- ✅ Spectrum analysis: short-time Fourier transform with any size, hop and
  window, ballistics, peak hold, fractional-octave bands, A and C weighting
  (`manguebyte-analysis`)
- ⬜ Overlap-add resynthesis for spectral processing
- ✅ Resampling at any ratio, and oversampling by 2, 4, 8 or 16
- ✅ State-variable filter, stable under audio-rate modulation
- ✅ FIR low-pass design (Kaiser-windowed sinc)
- ✅ One-pole and ladder filters, stable under audio-rate modulation
- ✅ High-order IIR design: Butterworth, Chebyshev I and II, elliptic, order
  1 to 16, gliding between designs without blowing up
- ✅ Linkwitz-Riley crossovers, 2 to 5 bands that sum flat
- ✅ FIR filter processor, choosing direct or FFT convolution by length
- ✅ Dynamics: compressor with soft knee, noise gate
- ✅ Look-ahead limiter whose ceiling holds for every sample
- ✅ Expander, and sidechain keying for the compressor, gate and expander,
  chosen per processor
- ✅ Reverb (feedback delay network)
- ✅ Chorus and flanger, phaser, dry/wet mixing with latency compensation,
  panner, envelope follower, interpolated delay lines
- ✅ Convolution: uniformly partitioned, with zero latency or a latency of
  one partition, true stereo, and responses swapped with a crossfade while
  playing. A non-uniformly partitioned scheme, cheaper for long responses
  at small block sizes, is not started
- ✅ Noise (white, pink, brown), LFO with tempo sync, waveshaper, DC blocker
- ✅ Wavetable oscillator: band-limited by a mip-map, with morphing between
  frames and phase modulation
- ⬜ Sampler: an SFZ player (multisamples, velocity layers, round robin,
  loop points, release samples), streamed from disk, open so that any
  program can play the instruments Parabolic exports
- ✅ Polyphonic voice allocation with per-note expression and modulation
- ✅ Metering: loudness to ITU-R BS.1770 and EBU R 128 (checked against the
  EBU's test signals), true peak, peak, RMS, VU, PPM, stereo correlation
- ✅ Pitch detection (YIN and the McLeod method) with note naming
- ✅ Sample-accurate events, transport and sidechain in the process call
- ⬜ SIMD-accelerated buffer operations

**Formats**
- ✅ Streaming readers and writers that do not load whole files, with
  sample-exact seeking, RF64, and wait-free bridges for playing and
  recording from an audio callback
- ✅ AIFF and AIFF-C reading and writing
- ✅ FLAC decoding
- ✅ Recognising a file's format from its contents
- ⬜ Ogg Vorbis, Opus and MP3 decoding
- ✅ FLAC encoding, checked against FFmpeg
- ⬜ Ogg Vorbis encoding
- ⬜ Metadata, cue points and loop markers

**Networking**
- ✅ Open Sound Control: codec, address patterns, UDP and TCP, and a
  real-time bridge that puts time-tagged bundles on their exact frame

**Core**
- ✅ Parameter system: ranges, skew, text conversion, smoothing
- ✅ Sample-accurate parameter automation events
- ✅ Change gestures (begin and end of a user edit): a `Gesture` guard in
  the editor contract, sent to CLAP hosts as gesture events and to VST 3
  hosts as `beginEdit`, `performEdit` and `endEdit`
- ✅ Parameter state save and restore, tolerant of version changes
- ✅ Denormal protection: `NoDenormals`, held by the wrappers, the
  standalone engine, the device streams and the test harness (ADR 0006)
- ⬜ Allocation checking in debug builds (today it is a test-only allocator)
- ⬜ `no_std` support for `manguebyte-core` and `manguebyte-dsp`

## Phase 2: plugins

Write a processor once and ship it in every format.

- ✅ `manguebyte-plugin`: the format-independent plugin API, plugin state,
  and a harness that runs a plugin in a unit test with no host
- ✅ CLAP: audio and note ports, sample-accurate events in, parameters,
  state, latency, reporting of parameter changes to the host. Checked in CI
  by the CLAP project's validator
- 🚧 CLAP `gui`, `timer-support` and `posix-fd-support` for plugins with an
  editor, tested through the C interface against fake editors and hosts,
  including `manguebyte-gui` editors in a headless window; not yet opened
  in a real host
- ⬜ CLAP extensions still to come: per-note automation, note-end and voice
  info, 64-bit audio, tail, presets, remote controls
- ✅ VST 3: audio and event buses, every automation point on its frame,
  notes with ids, tuning and note expression, MIDI controllers through
  hidden parameters, parameters, units, state, latency. Checked in CI by
  Steinberg's validator on Windows, Linux and macOS
- 🚧 VST 3 editor interfaces: the component handler kept and used for
  edits, `IPlugView` with content scale, `IPlugFrame` and `IRunLoop`, tested
  against fake host objects, including `manguebyte-gui` editors in a
  headless window; not yet opened in a real host
- ⬜ VST 3 still to come: 64-bit audio, changing the bus layout, program
  lists
- ⬜ Audio Unit (AUv2 and AUv3)
- 🚧 LV2: audio and sidechain ports, parameters as control ports, MIDI and
  transport on their frames, state, latency and generated Turtle files,
  tested in process through the C interface. Not yet bundled by
  `cargo mangue`, checked by `lv2lint` or loaded in a host; no editor
- ✅ Standalone application wrapper: sound card, MIDI ports, input for
  effects
- 🚧 Tooling: `cargo mangue` bundles, installs and validates plugins and
  creates new plugin crates from templates; macOS bundles get an ad-hoc
  signature and universal binaries. Signing with a real identity and
  notarisation are not started

AAX depends on a proprietary SDK and will be considered once the other formats
are solid.

## Phase 3: user interface

The design is in [ADR 0007](docs/adr/0007-user-interface-layer.md): an editor
contract in the plugin API, rendering on the CPU first, a thin window layer of
our own, and nothing claimed before its test exists.

- ✅ Editor contract and parameter gestures in the plugin API and both
  wrappers, each tested through the format's real interface against fake
  hosts
- ✅ 2D renderer on the CPU behind a seam (`manguebyte-gfx`): paths,
  gradients, images, clips, layers, a columns primitive for meters and
  spectra, damage-limited repaint that matches a full repaint bit for bit,
  and text with an embedded Inter subset. GPU rendering later, never the
  default while CI cannot test its pixels
- 🚧 Windowing, including embedding inside a plugin host: Win32 child
  windows inside a host's window and a headless backend for tests
  (`manguebyte-window`), and the glue that opens an editor in them
  (`manguebyte-gui`), tested on Windows; macOS and Linux follow as CI
  proves each
- 🚧 Layout, styling and a widget set (`manguebyte-ui`): row, column and
  stack layout, theme tokens, label, knob, slider, toggle, meter and canvas
  done; text entry, menus and popups not started
- ⬜ Audio widgets: waveform, spectrum, keyboard, envelope and EQ editors
- 🚧 Accessibility: every widget describes itself (the method has no
  default) and the test harness finds widgets only that way; platform
  adapters not started
- 🚧 High-DPI: scale flows from the host through the contract to the
  renderer, and a Win32 window takes the system's scale when the host gives
  none; a change of scale while a window is open, such as moving it to a
  monitor with another scale, is handled but untested

## Phase 4: hosting and graphs

- ✅ Audio processing graph, rewired while it plays, with automatic latency
  compensation
- ⬜ Graph extras: sidechain and per-node event routing, feedback through an
  explicit delay, crossfaded swaps, buffer reuse
- ✅ Hosting CLAP and VST 3 plugins as processors, through one interface
  for both formats, with every event on its frame
- ⬜ Plugin scanning (out of process), hosted plugins' editors, AU and LV2
- ⬜ Transport, tempo map and timeline primitives

## Phase 5: everywhere

- ⬜ iOS and Android
- ⬜ WebAssembly and Web Audio: the engine running in a browser's
  AudioWorklet, fed timestamped events; its first user will be
  [Aratu](https://github.com/alessonqueirozdev-hub/aratu)
- ⬜ Embedded targets

## Beyond parity

Matching JUCE is the baseline, not the finish line. Places where a framework
designed for Rust can do better:

- Compile-time checked real-time safety for user code
- Processing graphs that the compiler can inline end to end
- Deterministic offline rendering for testing audio code in CI
- First-class CLAP support, including its newer extensions
- A plugin API in which sharing state between threads incorrectly does not
  compile
