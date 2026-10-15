# Changelog

What changed in Manguebyte, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break the API.

## Unreleased

Nothing has been published to crates.io yet. Everything below is on `main`.

### Plugins

- `manguebyte-clap`: native CLAP support. `export_clap!(MyPlugin)` turns a
  `Plugin` into a CLAP plugin with audio and note ports, sample-accurate
  events, parameters, state and latency. The example plugin passes the CLAP
  project's validator, which runs in CI.
- `manguebyte-vst3`: native VST 3 support. `export_vst3!(MyPlugin)` turns
  the same `Plugin` into a VST 3 plugin: audio, sidechain and event buses;
  every point of every automation queue on its frame; notes with ids,
  tuning, polyphonic pressure and note expression; MIDI controllers, channel
  pressure and pitch bend through hidden parameters; units; state; latency.
  The class id is derived from the plugin's text id and is the same on every
  platform. The example plugin passes all 537 tests of Steinberg's validator
  in its extensive mode, which runs in CI on Windows, Linux and macOS, and
  the wrapper's own tests run under Miri.
- `scripts/bundle-vst3.sh` builds a `.vst3` bundle.
- `manguebyte-lv2`: LV2 support. `export_lv2!(MyPlugin)` defines
  `lv2_descriptor` for the same `Plugin`, and `bundle_files::<MyPlugin>`
  gives the text of the bundle's `manifest.ttl` and of the plugin's own
  Turtle file. The
  plugin's URI is `urn:manguebyte:` followed by its id. Audio ports, with
  the sidechain marked `lv2:isSideChain`; one control port per parameter,
  in the parameter's own unit and range, whose changes become parameter
  events on frame 0; an atom input carrying MIDI, each message on its
  frame, and `time:Position`; an atom `notify` output, written empty; a
  latency port; state through `state:interface`. `run` never allocates
  (checked with the counting allocator) and flushes subnormal numbers; a
  panic inside the plugin leaves a silent block, and the next block gives
  the plugin every parameter's value again. Tested in process through the
  real C descriptor, acting as a host; the Turtle has not yet been read by
  `lv2lint` or a Turtle parser, and no LV2 host has loaded a plugin yet.
  - Transport: a block is cut at each `time:Position` inside it, and the
    transport moves on by itself between positions. LV2 counts beats in
    `time:beatUnit` notes and the framework in quarter notes, so tempo and
    positions are converted. The start of the bar is `time:beat` minus
    `time:barBeat` when the host gives both, and `time:bar` times
    `time:beatsPerBar` only when it does not give the running beat. A
    position stamped past the end of a block is moved back by the
    difference, at its own speed and tempo, and holds from the start of the
    next block. A value that does not come out finite is passed on as
    unknown.
  - Turtle: a parameter with whole-number steps over a whole-number range
    is an `lv2:integer` port (`lv2:toggled` for a switch), and its values
    are listed as scale points with `lv2:enumeration` only when they have
    names, as a choice's options do; any other stepped parameter gets
    `pprops:rangeSteps`, the number of values it can take.
  - Versions: LV2 gives `lv2:minorVersion` 0 to experimental plugins and
    odd numbers to development versions, so a semantic version is not
    copied across. `lv2:minorVersion` is 2 × (1000 × major + minor) + 2 and
    `lv2:microVersion` is 2 × patch + 2, or 2 × patch + 1 for a pre-release:
    a release is never marked experimental or in development, and versions
    keep their order. `0.1.0` becomes 4 and 2. The URI does not change
    with the major version.
- `manguebyte-plugin`: the `Plugin` trait every format wraps; `Harness`,
  which runs a plugin in a unit test with scheduled notes and automation;
  `ParamWatcher`, which turns parameter changes made outside the host into
  events; plugin state as text.
- `plugins/pluck`: an example 16-voice instrument, and
  `scripts/bundle-clap.sh` to build it into a `.clap`.
- `plugins/duck`: an example effect, a sidechain ducker with a bypass that
  does not click. Both examples are validated as CLAP and as VST 3 in CI.
- `Harness::process_with_sidechain` feeds an effect's sidechain in a test.
- CLAP: the `gui`, `timer-support` and `posix-fd-support` extensions for
  plugins with an editor, embedded windows only. Gestures reach the host as
  gesture events from `process` and from `params.flush`, which now writes
  its output list; when the host is not processing, an edit asks it for a
  flush. Events the host's list refuses are sent again. Every `gui` call is
  safe in any order, including from inside the editor's own calls.
- VST 3: the `IComponentHandler` the host gives is kept and used for edits
  (`beginEdit`, `performEdit` with the value the control set, `endEdit`, on
  the main thread). `createView` returns an `IPlugView` with content scale
  for plugins with an editor, keeps the `IPlugFrame` for resizing and uses
  the host's `IRunLoop` for timers. A view outliving its plugin, attached
  twice or removed without being attached does nothing harmful.
- Fixed: an edit that the interface stored while a block was being prepared
  could be undone by that block, in the CLAP and VST 3 wrappers and in the
  standalone engine.
- `cargo mangue`, the command-line tool: `new` writes a plugin crate from a
  template (instrument or effect) that builds and tests as it is; `bundle`
  builds a package and writes its `.clap` and `.vst3` with the right layout
  for the platform, finding the library from cargo's own output, with
  universal binaries and an ad-hoc signature on macOS; `install` and
  `uninstall` use the per-user plugin folders; `validate` runs both
  formats' validators and fails on any failed test. Bundle layouts are pure
  functions, tested for every platform on any machine. The two example
  plugins declare their names in `[package.metadata.manguebyte]`.

### User interface

Phase 1 of [ADR 0007](docs/adr/0007-user-interface-layer.md), up to its
item 8. Windows exist on Win32 only; everything else is tested without a
display, and no editor has been opened inside a real host yet.

- `manguebyte_plugin::editor`: the editor contract. `Plugin::editor` returns
  an optional `EditorSpec`, which is `Send + Sync + 'static`, so it cannot
  hold the processor, a borrow, an `Rc` or a raw pointer (compile-fail tests
  prove each). An open editor is `!Send` and closes when dropped. Editors
  reach the host through an `EditorContext` that the wrapper cuts off when
  the editor closes: open gestures are ended, timers and descriptor watches
  cancelled, and every clone does nothing from then on. `Gesture` is the
  only way an edit reaches the host: begin, set, end, and cancel to restore
  the starting value; gestures on one parameter from several places collapse
  into one. `Resize` rules with a pure `fit`, `ParentWindow`, key events,
  main-thread wakes, and test doubles (`RecordingEditorHost`, `ManualLoop`).
  `Harness::editor` exposes a plugin's editor in tests.
- `manguebyte-gfx`: scenes recorded as display lists (rectangles, rounded
  rectangles, circles, arcs and paths, filled or stroked, with solid colour,
  linear and radial gradients, images, clips, transforms and opacity layers,
  and a columns primitive for meters and spectra), an exact text dump of a
  scene, PNG reading and writing, and a CPU rasteriser behind a private seam
  on `tiny-skia`. A repaint limited to damage rectangles is identical to a
  full repaint, bit for bit, and leaves every other pixel untouched. Text:
  `TextStyle` (size, weight, optical size, tabular figures), `TextLayout`
  (line breaking, alignment, ellipsis, caret positions) and a bounded glyph
  cache, using an embedded subset of Inter (`TYPEFACE_LICENCE` carries its
  OFL text). Golden images and dumps, fuzzing and a benchmark.
- `manguebyte-ui`: a retained widget tree whose views are plain functions;
  values are sources polled once per frame, with no listeners; a layout of
  its own (row, column, stack, padding, spacing, weights, limits); theme
  tokens; label, knob, slider, toggle, meter and canvas widgets; parameter
  edits made only through `Gesture`, showing the value being sent while a
  gesture is open; an accessibility model whose method every widget must
  implement; damage tracking, so a steady interface repaints nothing and
  touches no memory; and a harness that finds widgets by role and name and
  drives them with pointer, wheel and keys on a clock the test controls.
- `manguebyte-window`: platform windows. `Window::open` makes a child of the
  host's window, on Win32 so far; `Window::headless` makes a window with no
  system window behind it, on every system, which a test drives through a
  `Headless` controller. A `WindowHandler` receives `Event`s on the
  window's thread: paint with its damage, pointer down, move, up, leave and
  cancel, wheel turns in lines, keys with their modifiers and a repeat
  flag, text, focus, resizes, scale changes, timers and closing. A window
  presents a premultiplied pixmap through a back buffer of its own,
  converting and copying only the damaged rectangles, and answers an
  expose from that buffer without asking the handler. The two backends
  share the damage, back buffer, pointer, timer and dispatch code.
  - Win32: a `WS_CHILD` window presenting with `SetDIBitsToDevice`; the
    pointer captured from the first button down to the last button up;
    timers owned by the window. A message that arrives while the handler
    runs waits and is delivered after it returns, so the handler is never
    re-entered; while it waits, repeated timer ticks, pointer moves and
    paints are merged, at most `MAX_QUEUED` (1024) events wait, and
    `Window::dropped_events` counts any beyond that. Keys, characters and
    wheel turns the handler declines go on to the parent window; Tab, Enter
    and Escape are left to a host's dialog unless
    `Window::set_wants_all_keys(true)` asks for them. A parent window owned
    by another process is refused. The class, hook, thread and DPI rules
    of ADR 0007 are kept and recorded there.
  - The `testing` feature adds `win32::testing`: test parent windows,
    message injection, message loops and pixel read-back. `ReadBack::Screen`
    reads the pixels on the window's surface; `ReadBack::PrintClient` only
    checks the `WM_PRINTCLIENT` answer. A pixel test fails when the session
    cannot read windows back, unless `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is
    set, which CI never does.
  - `examples/probe.rs` presents a known pattern and reads it back: it
    exits 0 on `PASS` (or `SKIP` on a system with no window backend yet),
    1 on `FAIL` and 2 on `CANNOT`, when the session cannot read windows
    back. CI runs it on Windows.
- `manguebyte-gui`: the glue between the plugin wrappers and the widgets.
  `Editor` is an `EditorSpec` made from a size in logical pixels, resize
  rules, an optional theme and a function that builds the view; a plugin
  returns it from `Plugin::editor`. Opened, it makes a `manguebyte-window`
  child of the host's window (Win32 only; other parents are refused with
  `EditorError::Unsupported`), builds a `Root` on the wrapper's
  `EditorContext`, sends it the window's input in logical pixels, and runs
  a frame about every 16 ms (`FRAME_PERIOD_MS`) from a timer the window
  owns while the editor is visible. A frame where nothing changed polls the
  parameters and sources and then lays out, draws and presents nothing;
  the timer is stopped while the editor is hidden. A hidden editor only
  takes note of new sizes and scales, and draws them once when shown; a
  new scale and the size the host grants for it are drawn in one frame. No
  size the editor opens at, accepts or asks for is larger than
  `manguebyte_ui::MAX_BUFFER_SIDE` device pixels a side. An interface that
  panics while it handles an event or draws is dropped at once, which ends
  its gestures and calls `EditorContext::closed`. The editor is visible as
  soon as it opens, since VST 3 has no call that shows it.
  - The `headless` feature adds `Editor::on_headless_open` and
    `HeadlessEditor`, and only adds API: an editor opens headless only when
    it was given a hook, so `cargo test --workspace` leaves every other
    editor alone. Tests drive an editor with a headless hook end to end
    through the real CLAP interface and VST 3's COM interfaces, open and
    close it thousands of times while `process` runs under the counting
    allocator, fuzz the hosts' calls, and compare it with golden images at
    scales 1 and 2. On Windows, a test embeds it in a real window and reads
    its pixels back.
- `Param::stored_plain` says what a parameter would store for a value,
  without storing it.

### Hosting

- `manguebyte-host`: load CLAP and VST 3 plugins made by anyone and run them
  as a `Processor<f32>`, alone or as a node of a graph. The thread rules are
  types: a main-thread handle owns loading, parameters, state and the
  plugin's requests, and activating it yields the audio-thread half, which
  is `Send` and allocates nothing while processing. Every event of an
  `EventBuffer` reaches the plugin on its frame (VST 3 parameter changes as
  queues with one point per event; MIDI controllers through the plugin's own
  mapping), and what the plugin sends back is collected. Strings, counts and
  pointers from a plugin are checked and bounded, and a plugin that fails
  leaves silence and an error, not a crashed host. `any` is one interface
  over both formats. The example plugins, hosted through their real entry
  points, produce output identical bit for bit to `Harness` running them in
  process, in either format. Loading a library is `unsafe`; everything after
  it is safe.

### Metering and analysis

- `manguebyte-analysis`, a new crate. `LoudnessMeter` to ITU-R BS.1770-4,
  EBU R 128 and EBU Tech 3341 and 3342: K-weighting for any sample rate,
  channel weights, momentary, short-term and integrated loudness with both
  gates, loudness range, in bounded memory; every Tech 3341 and 3342 case
  that can be synthesised reads within the document's tolerance.
  `TruePeakMeter` by oversampling. `PeakMeter`, `RmsMeter`, `VuMeter`,
  `PpmMeter` (IEC 60268-10 Type I) and `StereoMeter`.
- `SpectrumAnalyzer`: any size, hop and window, ballistics, peak hold, A and
  C weighting, fractional-octave bands to IEC 61260-1, published whole to a
  user interface through a wait-free triple buffer.
- `PitchDetector`: YIN and the McLeod pitch method with a band-limited
  refinement; 1.8 cents worst case at 44.1 and 48 kHz on the tested tones,
  with no octave errors on them; `Note` names the nearest note.
- All of it runs on the audio thread without allocating and publishes its
  readings through atomics any thread can read.

### Open Sound Control

- `manguebyte-osc`, a new crate: messages, bundles and every argument type;
  encoding into the caller's buffer and strict decoding, including a
  borrowed decoder that does not allocate; address patterns with the 1.1
  `//` wildcard, matched in time bounded by the pattern and address
  lengths; UDP (unicast, broadcast, multicast) and TCP with the 1.1 SLIP
  framing. A real-time bridge decodes on a network thread and hands fixed
  events to the audio thread, where a time-tagged bundle lands on its exact
  frame.

### Real-time safety

- `NoDenormals`: a guard that makes the processor treat subnormal numbers
  as zero on the current thread, so a fading sound cannot slow it down. The
  CLAP and VST 3 wrappers, the standalone engine, the device streams and
  `Harness` hold it around every block, so plugins need not. See ADR 0006.
- Every crate that needs no `unsafe` code forbids it
  (`#![forbid(unsafe_code)]`): the facade, devices, signal processing, file
  formats, graph, MIDI, parameters, standalone, OSC, plugin, graphics,
  widgets, the interface glue (`manguebyte-gui`) and the command-line tool;
  `manguebyte-window` denies it everywhere but its Win32 backend. The
  `unsafe` that remains is listed in `docs/ARCHITECTURE.md`.
- `LogQueue` (`manguebyte_core::log`): a log the audio thread can write to.
  Posting is wait-free and never allocates, even with formatting
  (`post_fmt(level, format_args!(..))` formats into a buffer on the stack);
  messages below the queue's level are dropped before being formatted; any
  number of threads may post; the draining thread gets the messages in the
  order they were posted, across drains, and a count of the ones that found
  no room. A counting-allocator test and Miri check it.

### Processing model

- `Processor::process_with` takes a `ProcessContext` carrying one
  timestamped `EventBuffer` (notes, per-note expression, parameter values,
  modulation, MIDI), the `Transport` and a sidechain.
- `AudioBuffer::windowed` narrows a buffer to a range of frames without
  copying, and `split_at_events` uses it to apply each event on its exact
  frame.
- `ProcessContext` has a new public field, `offset`: where the buffer
  begins within the host's block, so that the pieces of a split block read
  the sidechain at their own frames. `split_at_events_with` hands each
  piece its own context, as does `ProcessContext::for_piece`, and
  `sidechain_peak` gives the sidechain's level at a frame of the piece. The
  struct is `#[non_exhaustive]`, so the field breaks no other crate. A processor
  that reads the sidechain must be given the piece's context; the older
  pattern of `split_at_events` with `without_events()` leaves the
  sidechain misaligned after the first piece.
- `Synth` and `Voice`: polyphonic voice allocation with per-note expression
  and modulation, sustain pedal and voice stealing. The synth remembers
  where the pitch wheel and each controller stand on every channel and
  tells a voice before it starts a note, so a note struck while the wheel
  is held starts bent, whichever voice it lands on.
- `Processor::latency_samples`, summed by chains.
- `ParamId`: a stable 32-bit hash of a parameter's text identifier.

### Graph

- `manguebyte-graph`: a processing graph that is edited while it plays.
  `Graph` is the description and `GraphEngine` the processor; a commit
  compiles a plan and swaps it in through a wait-free queue. Nodes keep
  their state across edits, removed nodes are freed off the audio thread,
  and paths of different latency are aligned automatically.

### Signal processing

- `Chorus` (1 to 8 voices, stereo spread, feedback, so also a flanger),
  `Phaser` (2 to 12 stages), `DryWet` (dry path delayed to match the
  effect's latency, seven mix laws), `Panner` (the standard pan laws) and
  `EnvelopeFollower` (peak or RMS ballistics).
- `DelayLine` reads between samples by linear, cubic or all-pass (Thiran)
  interpolation.
- `OnePole` and `LadderFilter` (a four-pole ladder solved per sample,
  12 and 24 dB low, high and band-pass, self-oscillating at full
  resonance), both stable under audio-rate modulation.
- `iir`: Butterworth, Chebyshev I and II and elliptic designs of order 1 to
  16 in four band types; `BiquadCascade` and `IirFilter` glide between any
  two designs without the state gaining energy.
- `Crossover`: Linkwitz-Riley splitters of 2 to 5 bands whose sum is flat.
- `StateVariableFilter`, stable while its cutoff is modulated at audio rate.
- `Compressor` with soft knee and linked channels; `NoiseGate` with hold.
- `Expander`: downward expansion with threshold, ratio (an infinite ratio
  makes it a gate), soft knee, attack, release, a range of at most 240 dB
  and a gain-reduction meter.
- Sidechain keying for `Compressor`, `NoiseGate` and `Expander`, opt-in:
  `with_key_source(KeySource::Sidechain)` makes one listen to the context's
  sidechain, and `KeySource::Input`, the default, keeps it on its own
  input. A wrapper that always passes a sidechain, silent when the host
  leaves it unconnected, changes nothing unless a processor asks for it,
  and each processor in a chain chooses on its own. `Oversampled` does not
  pass the sidechain to the processor inside it.
- `Convolution` with an impulse response of any length, by uniformly
  partitioned overlap-save in the frequency domain. In zero-latency mode,
  the default, the first partition is convolved directly in the time
  domain and the output is not delayed; otherwise it is late by exactly one
  partition. The output is identical bit for bit whatever the host's block
  sizes, while the response stays the same. Routings: one response for
  every channel or one per channel, and true stereo (left to left, left to
  right, right to left, right to right). The sums of the older partitions
  are spread over the samples of each partition, so with host blocks
  smaller than a partition no block carries all of them. Against direct
  convolution in double precision, the largest error measured is 1.47e-6
  of the output's RMS level in single precision, for responses of 1 to
  100,000 samples (the test fails above 2e-6), and below 1e-13 in double
  precision up to 20,000 samples. A `ConvolutionEngine` is built off the
  audio thread and handed to the playing `Convolution` by its
  `ConvolutionLoader` through a wait-free queue, with an optional
  crossfade; the old engine travels back to be dropped there, so the audio
  thread never allocates or frees. An engine that arrives during a
  crossfade waits for it to end. `ConvolutionLoader::latency` gives the
  latency of the engine sent last, so a plugin can tell when a new
  response changes it.
- `FirFilter`: a FIR filter processor for any coefficients, such as those
  of the `fir` design functions, with no latency. It chooses on its own
  between direct convolution and the partitioned FFT engine, by the number
  of multiplications per sample, or takes `FirMode` to force one.
- `WavetableOscillator` and `Wavetable`: single-cycle frames of any
  length, split into at most 2,048 harmonics and built off the audio thread
  into a mip-map of two levels per octave, each read with fifth-order
  Lagrange interpolation; the oscillator plays the level whose harmonics
  stay below half the sample rate. It morphs between frames, with an
  optional glide, takes phase modulation from its input, and is identical
  bit for bit across block sizes. A table is shared through an `Arc`, and
  `set_table` hands the old one back so that the audio thread never drops
  it. A sawtooth played at 48 kHz from A0 to C8 keeps the energy away from
  its harmonics below -125 dB, the test's limit (the worst measured was
  -131.4 dB, with the measurement's own floor below -150 dB), and stays
  within -120 dB of the ideal band-limited sawtooth.
- `Reverb`: an eight-line feedback delay network.
- `fft::Fft` and `fft::RealFft` for any length; `window::Window` with nine
  shapes.
- `Adsr::silence`.
- `Oversampled`: runs any processor at 2, 4, 8 or 16 times the sample rate
  through linear-phase half-band filters, with its delay reported.
- `Resampler` and `resample`: band-limited conversion between any two
  sample rates, in a stream or for a whole buffer.
- `Limiter`: look-ahead peak limiter whose ceiling holds for every sample.
- `Waveshaper` (hard clip, tanh, soft clip, fold), `DcBlocker`, `Noise`
  (white, pink, brown), `Lfo` with tempo sync, and `fir` design functions.

### Parameters

- `manguebyte-params`: `FloatParam`, `IntParam`, `BoolParam`, `ChoiceParam`;
  linear, skewed, symmetric and logarithmic ranges; text conversion;
  `ParamSmoother`; `ParamSet` with saved state that tolerates version
  changes.

### MIDI

- System common and real-time messages; `StreamParser` for raw byte streams
  with running status and SysEx; reading and writing standard MIDI files.
- `ump`: Universal MIDI Packets with typed access to every message of
  UMP 1.1.1, flex data and stream messages included, checked against the
  specification's own examples.
- `translate`: MIDI 1.0 to 2.0 and back as the specification prescribes,
  scaling verified exhaustively for every 7-bit and 14-bit input.
- `rpn` (RPN and NRPN), `mpe` (zones, configuration message, channel
  allocator, note tracker) and `keyboard` (keys down with the pedal, read by
  an interface while the audio thread writes, never showing a held key as
  released).

### File formats

- `AudioReader` reads WAV, AIFF and FLAC block by block from any reader,
  and seeks to any frame exactly (FLAC through its seek table, which is
  never trusted blindly, or by bisection); memory does not grow with the
  file. `AudioWriter` writes WAV, AIFF and FLAC block by block.
- `StreamingPlayer` and `RecordingWriter` play from and record to disk from
  an audio callback: a worker thread does the I/O and the audio thread only
  touches wait-free queues, counting underruns and overruns instead of
  waiting.
- A FLAC encoder written from RFC 9639: fixed and LPC predictors, all four
  stereo decorrelations, Rice coding with partition search, MD5 and seek
  table, levels 0 to 8, 4 to 32 bits. Everything it writes decodes exactly
  with this crate and with FFmpeg.
- WAV files larger than 4 GB are written as RF64, and `wav::read`, `decode`
  and `read_file` now read RF64 instead of refusing it.
- `flac::read` and `decode` now return an error for a frame whose
  checksums pass but whose samples do not fit the stream's bit depth;
  before, it panicked in debug builds and decoded to noise in release.
- `flac`: a FLAC decoder written from RFC 9639, covering every subframe
  type, both Rice codings, wasted bits, the three stereo decorrelations and
  4 to 32 bits per sample, with frame checksums verified.
- `aiff`: AIFF and AIFF-C reading (big and little-endian PCM, float) and
  writing (16 and 24-bit PCM, 32-bit float).
- `decode` and `read_file` recognise WAV, AIFF and FLAC from the contents.
- The decoders are tested against files encoded by FFmpeg and must match
  their source sample for sample.

### Devices

- Device listing with stable identifiers; choice of sample rate, channel
  count and buffer size; `InputDevice`; `DuplexStream`.
- `devices::midi`: MIDI input and output ports. `MidiReceiver` stamps each
  message with its arrival time and places it inside the next block at the
  matching frame, so a player's timing is not quantised to the buffer size.
- `manguebyte-standalone`: `Standalone::<P>::start` runs a plugin on the
  default output device, listens to every MIDI input, gives an effect the
  default input, and accepts notes from the program itself.

### Quality

- A counting allocator fails the build if any processor, or the CLAP
  wrapper's `process`, touches the heap while processing.
- The same material rendered with different block sizes must be identical
  bit for bit.
- Miri runs the tests of the places that contain `unsafe` code that
  handles memory: the queue, the triple buffer, the CLAP, VST 3 and LV2
  wrappers and the host. It also runs the window crate's
  platform-independent code and its headless backend.
- CI checks that no crate that processes audio depends on an interface
  crate (`scripts/check-layering.sh`), checks every dependency's licence
  with `cargo deny` against the list in `deny.toml`, and runs the window
  probe on Windows.
- CI runs `cargo mangue`'s end-to-end test on Windows, Linux and macOS:
  generate a plugin of each kind, build it, test it and bundle it.

### Fixed

- `Gain` was not identical bit for bit across block sizes while it glided:
  `Smoothed::skip` added a multiple of the step where stepping adds it once
  per sample, and setting the same target again restarted the glide. Both
  are fixed, with a test that glides through blocks of many sizes.
- `AudioBuffer::peak` passed over a NaN, so a test such as `peak() < 1.0`
  could pass on garbage. A NaN now makes the peak NaN.
- `Transport::advance` could overflow in a debug build; it now saturates.
- The CLAP wrapper passed note-expression pan through unconverted: CLAP's
  runs from 0 to 1 and the framework's from -1 to 1.
- `scripts/bundle-vst3.sh` named the 32-bit Linux folder `x86-linux`; VST 3
  calls it `i386-linux`.
- A voice that finished in the middle of a block kept advancing until the
  end of it, so the next note depended on the host's block size.
- `ParamSmoother` could stall just short of its target on a long exponential
  ramp and never report that it had finished.
- A value just below zero was displayed as `-0.0`, which read back as a
  different text.

## 0.1.0 (not published)

The foundation: `AudioBuffer`, `Sample`, `Smoothed`, atomic floats, a
wait-free queue, the `Processor` trait and tuple chains, oscillator, biquad
filters, ADSR, delay, gain, MIDI channel messages, WAV files and playback on
the default output device.
