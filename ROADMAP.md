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
- ✅ Hot-plug: devices and MIDI ports added and removed, the default
  changing and a stream's device lost, reported by `DeviceManager` and
  `DeviceWatcher`, which poll (every second by default) because no backend
  notifies; the systems' own change notifications are not used
- ✅ Clock-drift compensation between duplex devices by adaptive
  resampling of the input
- ✅ Choosing the audio host: WASAPI, CoreAudio, ALSA and AAudio, JACK behind
  the `jack` feature, and ASIO by either path below. No PipeWire host of
  its own, and JACK has not been run on real hardware
- ✅ ASIO drivers, by two paths. The opt-in crate `manguebyte-pro-io`
  speaks to the drivers itself and runs behind the device manager through
  the public `AudioBackend` seam; it is under the GPLv3 because it declares
  Steinberg's interface under the SDK's GPLv3 option, and has run on three
  drivers on Windows x86-64. Closed-source programs use `cpal`'s own `asio`
  feature instead, with an SDK they license themselves.
  ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
- ✅ Changing devices under a running engine (`DeviceManager::start_engine`):
  the callback is prepared once at a fixed `EngineFormat`, and the devices
  change under it with a crossfade where the backend runs two streams at
  once, or a short faded gap where it cannot (an ASIO driver, the same
  device opened again, a driver's reset request). The crossfade is proven
  on the headless backend only: two real outputs have not crossfaded yet
- ✅ MIDI input and output ports, with jitter-free timing into the audio
  thread, and packets sent from the audio thread when their frame is heard
  (`midi_output_queue`, `MidiOutput`)
- 🚧 MIDI output through the systems' own Universal MIDI Packet APIs
  (`MidiOutput::open_with_protocol`): MIDI 2.0 packets leave unchanged
  through CoreMIDI's `MIDISendEventList` on macOS and iOS and the ALSA
  sequencer's UMP events on Linux, to ports that speak MIDI 2.0, with
  MIDI 1.0 bytes and the default translation everywhere else. Checked
  in CI by a loopback to the test's own endpoints (macOS, and Linux
  where the runner has the sequencer), not yet with a MIDI 2.0 device.
  Windows still sends MIDI 1.0 through WinMM: Windows MIDI
  Services' API is not reachable from the `windows` crate (TECH_DEBT
  90)

**MIDI**
- ✅ System common, system real-time and SysEx messages
- ✅ Running status and a byte-stream parser
- ✅ Standard MIDI File reading and writing, with tempo-aware timing
- ✅ MIDI 2.0: Universal MIDI Packets with every message type, and MIDI 1.0
  to 2.0 translation as the specification prescribes
- ✅ RPN and NRPN, MPE zones, channel allocation and per-note tracking, and
  a keyboard state an interface can read while the audio thread writes
- ✅ MIDI 2.0 and MPE delivered to processors as per-note events, and
  through the device and plugin layers: one `NoteInput` that devices, the
  standalone runner, the harness and every wrapper read MIDI through
- ✅ MIDI-CI: discovery, profiles, property exchange and process inquiry,
  and a device agent an application drives
- ✅ MIDI 2.0 out of plugins: notes, per-note expressions and any MIDI 2.0
  channel voice message, written as MIDI 2.0 packets, as MIDI 1.0 by the
  default translation or as MPE, and sent through CLAP, VST 3, AU (its
  MIDI output callback, MIDI 1.0), LV2 (the notify port, MIDI 1.0) and the
  standalone runner. A `manguebyte-dsp` `Processor` cannot send events by
  itself. `manguebyte-host` reads what a hosted plugin sends in all four
  formats, through one `AnyProcessor::output_events` (MIDI 1.0 from LV2
  and AU)

**DSP**
- ✅ FFT of any length, real FFT and window functions
- ✅ Spectrum analysis: short-time Fourier transform with any size, hop and
  window, ballistics, peak hold, fractional-octave bands, A and C weighting
  (`manguebyte-analysis`)
- ✅ Overlap-add resynthesis for spectral processing: an STFT processor
  with any transform size and hop whose windows overlap-add to a constant
  (others are refused), exact reconstruction, a reported latency and
  output independent of the host's blocks, with a spectral gate and
  denoiser, a spectral freeze and a basic phase-vocoder pitch shifter
  built on it (`manguebyte_dsp::stft`)
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
- ✅ Workstation effects, each measured: bitcrusher and sample-rate
  reducer, tremolo and auto-pan with tempo sync, ring modulator, frequency
  shifter (an elliptic all-pass 90-degree network, image 90 dB down),
  stereo widener with bass mono, transient shaper, three-band multiband
  compressor, tape saturation with wow, flutter and drift, a channel
  vocoder of 8 to 32 bands, a granular delay with a fixed grain pool, and
  a formant filter morphing A-E-I-O-U. Not yet timed, nor in
  `manguebyte-web` or Parabolic (TECH_DEBT 104)
- ✅ Convolution: uniformly partitioned, with zero latency or a latency of
  one partition, true stereo, and responses swapped with a crossfade while
  playing. A non-uniformly partitioned scheme, cheaper for long responses
  at small block sizes, is not started
- ✅ Noise (white, pink, brown), LFO with tempo sync, waveshaper, DC blocker
- ✅ Wavetable oscillator: band-limited by a mip-map, with morphing between
  frames and phase modulation
- ✅ Sampler: an SFZ player (`manguebyte-sampler`): multisamples, velocity
  layers, round robin and random layers, loop points with crossfades,
  release triggers, choke groups and polyphony limits, from files or from
  memory, in WebAssembly too, open so that any program can play the
  instruments Parabolic exports. Filters per region (one to six poles,
  band-pass and band-reject) with their envelope, and modulation by
  controllers (`<opcode>_onccN`, `set_ccN`, controllers up to 511 that a
  plugin sets without MIDI) are done and measured. Samples stream from
  disk with a preload in memory, packed without loss if asked, and other
  players stream through the same layer (`SampleStore`; ADR 0014,
  TECH_DEBT 106); releases are chosen by the time a key was held and cross
  from the sustain in phase, and a model outside moves each key's pitch
  and level frame by frame (`modulate_key`, `SincReader`); LFOs, controller
  curves and smoothing
  per opcode, a second filter, the other filter types and keyswitches are
  not started (TECH_DEBT 101)
- ✅ Polyphonic voice allocation with per-note expression and modulation,
  accents and legato slides
- ✅ Instruments: physically modelled plucked strings (guitars and basses),
  four-operator FM, a tonewheel organ with a rotary speaker, the drums and
  kick bass of the classic analogue drum machine, and an acid bass; each a
  `Voice` with presets, measured for tuning, decay, velocity and accent
- ✅ Acoustic instruments by physical modelling, with no samples: mallets
  by modal synthesis, bowed strings and winds by digital waveguide, and
  choirs and a solo voice by formant synthesis
- ✅ Drum kits: acoustic drums and cymbals by modal synthesis, calibrated
  against recordings of a real snare and crash; the 909, 606, CR-78 and
  808; kits for trap, techno, house, drum and bass and lo-fi
- ✅ A deep polyphonic synthesiser: three band-limited oscillators with
  sixteen-voice unison, wavetables, sync, FM and ring modulation; two
  filters of fifteen models; four envelopes, three LFOs and a sixteen-slot
  modulation matrix; 68 presets for electronic and pop music
- ✅ Metering: loudness to ITU-R BS.1770 and EBU R 128 (checked against the
  EBU's test signals), true peak, peak, RMS, VU, PPM, stereo correlation
- ✅ Pitch detection (YIN and the McLeod method) with note naming
- ✅ Sample-accurate events, transport and sidechain in the process call
- ✅ SIMD-accelerated buffer operations: `manguebyte_core::vector`, with
  SSE2, AVX, AVX2, NEON and WebAssembly SIMD chosen at run time and the same
  results on every processor; used by `AudioBuffer`, the WAV reader and
  writer, convolution and the graph's and browser engine's mixing. The FFT,
  resampler, oversampler and filters are still scalar (TECH_DEBT 92)
- ✅ Additive synthesis: `PartialBank`, a bank of sine partials with
  frequencies, gain ramps and pans set every control block, rendered a
  partial per vector lane (`vector::add_partials`), the same bit for bit on
  every backend and Caecilia's kernel bit for bit for every partial;
  amplitude held within 3e-5 for as long as it runs. 3 000 partials take
  0.16 of a core on AVX2 against a whole core for the scalar kernel. NEON
  and WebAssembly SIMD are not yet run, and a block biquad across lanes is
  not started (TECH_DEBT 103)

**Formats**
- ✅ Streaming readers and writers that do not load whole files, with
  sample-exact seeking, RF64, and wait-free bridges for playing and
  recording from an audio callback
- ✅ Audio sources pulled by a device callback or a graph: a transport
  over a streamed file (play, pause, seek, loop from any thread), audio in
  memory, a mixer, a sample rate converter and a channel router
- ✅ AIFF and AIFF-C reading and writing
- ✅ FLAC decoding
- ✅ Recognising a file's format from its contents
- ✅ Ogg Vorbis and MP3 decoding, with sample-exact seeking
- ✅ Opus decoding: SILK, CELT, hybrid and multistream, gapless, with
  seeking
- ✅ FLAC encoding, checked against FFmpeg
- ✅ Ogg Vorbis encoding, at qualities 0 to 10
- ✅ MP3 encoding: MPEG-1 layer III, constant and variable bitrate,
  gapless
- 🚧 Opus encoding: CELT (music), constant and variable bitrate, frames of
  2.5 to 20 ms, up to 255 channels, any input rate, gapless, with
  metadata; SILK and hybrid frames (speech) are still to come
- ✅ Metadata, cue points and loop markers: one model for every format,
  read and written where the format allows, unknown chunks kept
- ✅ MPEG audio layers I and II
- ✅ Waveform thumbnails with a cache file
- ✅ AAC, ALAC and WMA through the system's codecs on Windows and macOS
  (`platform-codecs`)

**Networking**
- ✅ Open Sound Control: codec, address patterns, UDP and TCP, and a
  real-time bridge that puts time-tagged bundles on their exact frame

**Core**
- ✅ Parameter system: ranges, skew, text conversion, smoothing
- ✅ Parameter descriptions as data and as JSON (range, curve, steps,
  names, smoothing), for editors outside Rust such as Aratu's
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
  including `manguebyte-gui` editors in a headless window. Mangue Pluck's
  CLAP editor has been opened by hand in REAPER on Windows; no other host
  has opened one
- ✅ CLAP extensions (issue #14): per-note automation and modulation with
  the per-note-id and per-key flags of `ParamFlags::per_note`, modulation
  converted around the value on its frame, `voice-info`, `note-name`,
  `audio-ports-config` and `audio-ports-activation` from
  `Plugin::LAYOUTS`, 64-bit ports through the 32-bit path, and transport
  events inside a block, which cut it there
- ⬜ CLAP extensions still to come: note-end events, tail (issue #25),
  presets (`preset-load` and the preset-discovery factory), remote
  controls, `render`, `thread-pool`, a native 64-bit path (TECH_DEBT 91)
- ✅ VST 3: audio and event buses, every automation point on its frame,
  notes with ids, tuning and note expression, the MIDI controllers a
  plugin declares through hidden parameters, parameters, units, state,
  latency. Checked in CI by Steinberg's validator on Windows, Linux and
  macOS
- 🚧 VST 3 editor interfaces: the component handler kept and used for
  edits, `IPlugView` with content scale, showing the editor once attached,
  `IPlugFrame` and `IRunLoop`, tested against fake host objects, including
  `manguebyte-gui` editors in a headless window; not yet opened in a real
  host
- ✅ VST 3 64-bit audio and bus layouts (issue #15): `kSample64` through
  the plugin's 32-bit path, `setBusArrangements` accepting the layouts in
  `Plugin::LAYOUTS` and `activateBus` switching the sidechain off
- ⬜ VST 3 still to come: program lists, note names, a native 64-bit path
  (TECH_DEBT 91)
- 🚧 Audio Unit, version 2: `export_au!` through the plain C interface,
  with formats, parameters, scheduled parameters and MIDI on their
  frames, the host's transport and state; `.component` bundles written and
  installed by `cargo mangue`; tested through its C interface on every
  system, and both example plugins pass Apple's `auval` in a macOS CI job.
  MIDI output through `kAudioUnitProperty_MIDIOutputCallback`, and the
  elements' channel counts from `Plugin::LAYOUTS`. Not yet loaded in a
  DAW. No editor, MIDI 2.0 output or factory presets
- ⬜ Audio Unit, version 3 (app extension)
- 🚧 LV2: audio and sidechain ports, parameters as control ports, MIDI and
  transport on their frames, state, latency and generated Turtle files,
  tested in process through the C interface; `.lv2` bundles written by
  `cargo mangue bundle --format lv2`, and the examples' found and read by
  lilv in CI. Not yet checked by `lv2lint` or run in any host but the
  framework's own (`manguebyte-host`); MIDI output on the notify port; no
  editor, one fixed layout
- 🚧 Plugin identity at run time ([ADR 0012](docs/adr/0012-plugin-identity-at-run-time.md)):
  `Plugin::info`, asked once per library through `plugin_info`, and
  `library_path`, read by the CLAP, VST 3, LV2 and AU wrappers, saved
  state and the harness; two copies of one library with different
  identities appended are hosted side by side in one process, through
  CLAP and VST 3, editors included, in a test run on Windows. Not yet run
  on Linux, not run on macOS (appended data breaks a signature there), and
  AU `Info.plist` and LV2 Turtle files are not rewritten per copy
  (TECH_DEBT 100)
- ✅ Standalone application wrapper: sound card, MIDI ports in and out,
  input for effects
- 🚧 Aratu Bridge (`plugins/aratu-bridge`): an example CLAP, VST 3 and AU
  instrument that plays an Aratu page's music in a DAW, in time with its
  transport, from the control calls and events the page sends over a
  local WebSocket ([docs/ARATU_BRIDGE.md](docs/ARATU_BRIDGE.md)). Tried
  by hand once, as a CLAP in REAPER on Windows with a Chromium page on
  `http://127.0.0.1`; other DAWs, other browsers and `https://` pages are
  not tried yet. No editor, one stereo output
- 🚧 Tooling: `cargo mangue` bundles (CLAP, VST 3, LV2 and AU), installs and
  validates plugins and creates new plugin crates from templates; macOS
  bundles get an ad-hoc signature and universal binaries. Signing with a
  real identity and notarisation are not started, and there is no LV2
  validator in it

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
  (`manguebyte-gui`), tested on Windows, with two plugin binaries'
  editors open in one process. Application windows with their loop,
  close requests, size limits and monitor changes, files dropped from
  other programs onto widgets, and the open, folder and save dialogs
  ([ADR 0010](docs/adr/0010-application-windows.md)), tested headless and
  with real Win32 windows; showing the dialogs and a real drag from the
  file manager are tested by hand only, and not yet recorded. macOS and
  Linux follow as CI proves each ([TECH_DEBT](docs/TECH_DEBT.md) item 97)
- 🚧 Editors for the example plugins: Mangue Duck's (knobs, a bypass switch
  and a gain-reduction meter) and Mangue Pluck's (a knob for each
  parameter), tested headless and in real Win32 windows. Mangue Pluck's
  CLAP editor has been opened by hand in REAPER on Windows, showing its
  title and knobs; working its knobs there, and Mangue Duck's editor in a
  host, are not tested by hand yet
- 🚧 Layout, styling and a widget set (`manguebyte-ui`): row, column and
  stack layout, theme tokens, label, knob, slider, toggle, meter and canvas
  done; text entry (a text field, and values typed into knobs and
  sliders, with the clipboard), popup menus with submenus, a combo box,
  hover and tooltips done; scroll views (wheel, touchpad, bars, keys,
  nested views, Tab scrolling to what it focuses), lists of rows the
  application keeps with only those in sight drawn (tested with 100 000),
  selection, typing to find a row and activation, drag and drop inside
  the interface with a typed payload, a ghost, auto-scrolling and
  cancelling, and pictures fitted and sharp at 2x done
  ([ADR 0009](docs/adr/0009-scrolling-lists-and-drag-and-drop.md)); files
  dropped from other programs onto the same drop targets done
  ([ADR 0010](docs/adr/0010-application-windows.md)); tabs and a sidebar
  of pages switched without rebuilding the window, hidden pages costing
  nothing (not polled, laid out, drawn or asking for frames, tested),
  action buttons in three looks and a confirmation dialog inside the
  window with a focus trap done
  ([ADR 0013](docs/adr/0013-pages-and-modal-dialogs.md)); dragging out of the window, rows of
  different heights, multi-line text, drag and drop by keyboard, dialogs
  with content of their own and overflowing tab bars not started
  ([TECH_DEBT](docs/TECH_DEBT.md) items 96, 97 and 105)
- 🚧 Application state with undo and redo (`manguebyte-state`,
  [ADR 0008](docs/adr/0008-application-state.md)): a document tree with
  stable ids, named transactions, gestures as one step, merging by key,
  history limits, change journals polled instead of listeners, a mirror
  of the parameters that keeps host automation out of the history, the
  document saved with the plugin's state as text or binary with
  migrations, a settings file, and snapshots handed to the audio thread
  without allocating. Tested, including random edit sequences undone to
  every earlier tree; no example plugin's editor uses it yet, and it has
  not been in a DAW ([TECH_DEBT](docs/TECH_DEBT.md) item 87)
- 🚧 Audio widgets: a waveform view drawn from a multi-resolution summary
  (zoom around the pointer, markers within rules the application gives
  and onto zero crossings, a selection, a playhead), a piano keyboard
  (glissando, velocity from where a key is pressed, the computer's
  letters, keys lit and ranges marked by the application), an ADSR
  editor whose handles are gestures on four parameters, and a zone map
  of keys by velocities lined up with the keyboard, taking samples
  dragged from a list or from other programs, all tested headless;
  spectrum and EQ editors not started, and none worked by hand in a
  real window yet ([TECH_DEBT](docs/TECH_DEBT.md) item 98)
- ✅ Design tokens and motion
  ([ADR 0011](docs/adr/0011-design-tokens-and-motion.md)): layered tokens
  (surfaces, hairlines, three text levels, one accent, categorical hues,
  semantic roles, elevation, radius, spacing, type, motion), a deep dark
  theme with glow as the default and a light editorial one, switchable at
  run time; cached, deterministic shadows and glows in the renderer;
  eased hover, press, focus and selection on the frame clock; the focus
  ring after the keyboard only; every widget restyled. Tested: an idle
  interface draws and asks for nothing, a transition repaints only its
  widget and stops, shadows are blurred once and partial renders through
  them equal whole ones, motion turns off with the system's setting.
  Not worked by hand in a host yet ([TECH_DEBT](docs/TECH_DEBT.md) item 99)
- 🚧 Accessibility: every widget describes itself (the method has no
  default) and the test harness finds widgets only that way; each frame
  lists the widgets whose description changed (`Frame::access`), such as
  names that follow the language through `TextSource`, tested headless
  with a language switch across eight widgets
  ([TECH_DEBT](docs/TECH_DEBT.md) item 102); platform adapters not
  started, so nothing reads that list yet
- 🚧 High-DPI: scale flows from the host through the contract to the
  renderer, and a Win32 window takes the system's scale when the host gives
  none; a change of scale while a window is open, such as moving it to a
  monitor with another scale, is handled but untested

## Phase 4: hosting and graphs

- ✅ Audio processing graph, rewired while it plays, with automatic latency
  compensation
- ✅ Graph extras: sidechain routing with buses and channels, per-node event
  routing (nodes that send events included), feedback through an explicit
  delay in frames that keeps the output independent of the block size, and
  latency compensation across all of them, all editable while it plays
- ⬜ Graph extras still to come: crossfaded swaps, buffer reuse and pruning
- ✅ Hosting CLAP, VST 3 and LV2 plugins as processors, through one
  interface for all three formats, with every event on its frame; LV2
  bundles read with the host's own Turtle reader
- ✅ Plugin scanning: the standard folders of each format, each CLAP and
  VST 3 file listed in a process of its own with a time limit, crashes and
  hangs reported, a cache keyed by path, time and size
- ✅ Hosting Audio Units, version 2 (issue #4): components found with
  `AudioComponentFindNext` and made with `AudioComponentInstanceNew` on
  macOS, or in process from a factory function anywhere; stream formats,
  `MaximumFramesPerSlice`, channels as `SupportedNumChannels` allows, the
  main input and sidechain through render callbacks, parameters with
  `AudioUnitScheduleParameters` and MIDI with `MusicDeviceMIDIEvent` on
  their frames, the host's transport callbacks, latency, tail, `ClassInfo`
  state and bypass, through the one interface and the out-of-process
  scanner. The example plugins sound the same, bit for bit, as with no
  host, through their factories on every system and, installed, through
  AudioToolbox in the macOS CI job, where the scanner also reports a unit
  that crashes and one that hangs. No AUv3 as such, editors or factory
  presets (TECH_DEBT 93)
- 🚧 Hosted plugins' editors (issue #5): CLAP's `gui` (embedded),
  `timer-support` and `posix-fd-support`, and VST 3's `IPlugView` with
  `IPlugFrame`, content scale and, on X11 systems, `IRunLoop`, behind the
  same methods on `ClapPlugin` and `Vst3Plugin`: open in a parent window,
  size, scale, close, and the editor's requests and timers through
  `pump`. The example plugins' editors are opened, resized, rescaled and
  closed in real Win32 windows, and a knob dragged in each format reaches
  the hosted processor. Not yet opened in a real window on macOS or Linux,
  where the framework's own editors do not open; no LV2 or AU editors
  (TECH_DEBT 95)
- ✅ Transport, tempo map and timeline primitives: a tempo map with steps,
  linear ramps and time signatures, exact conversions between frames,
  seconds, quarter notes and bars, beats and ticks, and a real-time
  transport clock with loops and seeking, driving the harness and the
  standalone runner

## Phase 5: everywhere

- 🚧 iOS and Android: the core, DSP, MIDI, formats and device crates and
  the facade are built in CI for `aarch64-apple-ios` and
  `aarch64-linux-android` (CoreAudio and CoreMIDI; AAudio and AMidi), but
  not run on a device; no plugin format for either
- 🚧 WebAssembly and Web Audio: `manguebyte-web`, an engine running in a
  browser's AudioWorklet, fed 32-byte timestamped records through a
  shared-memory ring, with a synth, a drum kit, an SFZ sampler, the
  instruments of `manguebyte-dsp` (plucked strings, FM, an organ, an
  808-style kit and bass, an acid bass, drum kits with Brazilian
  percussion on every kit, the polyphonic synthesiser, mallets, bowed
  strings, winds and a choir), thirteen effects (the drive with a tube
  curve), a master bus with a true-peak limiter, and a scope and levels
  for every part; tested natively and in Node.js, its demo tried once in
  Chromium, not yet under load. The same engine plays in a DAW through the
  Aratu Bridge. Its first user is
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
