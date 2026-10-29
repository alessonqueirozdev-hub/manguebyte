# Changelog

What changed in Manguebyte, newest first. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/). Before 1.0, a minor version may
break the API.

## Unreleased

### Core

- `vector`: SIMD-accelerated operations on slices of samples (issue #10).
  `scale`, `copy_scaled`, `add`, `add_scaled`, `multiply`, `scale_ramp`,
  `peak`, `dot`, `sum_of_squares`, `complex_multiply_add`,
  `interleave_stereo`, `deinterleave_stereo`, and the conversions
  `i16_le_to_f32`, `i24_le_to_f32`, `i32_le_to_f32`, `f32_to_i16_le` and
  `f32_to_i24_le`, for any `Sample` (`f32` and `f64` on the vector units).
  `Backend` names the instruction sets (`Scalar`, `Sse2`, `Avx`, `Avx2`,
  `Neon`, `Simd128`), runs any one of them, and `Backend::detect` picks
  the fastest the processor has: asked once with `cpuid`, no system call,
  and kept in an atomic. WebAssembly uses SIMD when built with `simd128`.
  Nothing allocates, the crate still has no dependencies, and Miri runs the
  scalar code. The element-wise operations and the conversions give the
  scalar loop's results bit for bit on every backend (a product then a sum,
  no fused multiply-add); the tests compare every backend with the scalar
  definitions over lengths from 0 to 4 099 and every alignment, all 65 536
  16-bit values and the 24-bit ones. Sums add in one fixed order, the same
  on every processor: 16 running sums for `f32` (8 for `f64`) added in
  pairs, then what is left over. `examples/vector_speed.rs` times them. On
  an Intel Core i3-1215U, against the loops they replace, in blocks of 512
  (AVX2): `peak` 11 to 13 times faster, `sum_of_squares` 6.5 to 7.2,
  `scale_ramp` 3.3 (`f64`) to 6.6 (`f32`), stereo interleaving 3.5 to 6,
  `f32_to_i16_le` 4.6, `i24_le_to_f32` 4.9, `i32_le_to_f32` 5.5,
  `complex_multiply_add` 1.7 to 2.4, `dot` 1.5 to 2.2 and gain and mixing
  1.4 to 2.3 (the compiler already vectorised those with SSE2); dot
  products shorter than about a hundred elements are slower than the old
  four-sum loop (TECH_DEBT item 92).
- `AudioBuffer`'s `apply_gain`, `apply_gain_ramp`, `mix_from`, `peak`,
  `rms` and its interleaving copies of one and two channels run on them.
- `module::file` names the file of the binary the code is linked into,
  a plugin's own library in a plugin, by asking the system about an
  address inside it: `GetModuleHandleExW` with
  `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS` and `GetModuleFileNameW` on
  Windows, `dladdr` elsewhere, `None` under Miri and on WebAssembly
  (ADR 0012).

### DSP

- `ConvolutionEngine` and `FirFilter` run on `manguebyte_core::vector`.
  The direct head works across the frames of a piece of the block, a
  scaled copy of the input per tap, in groups of 32 taps; the products of
  spectra are `complex_multiply_add`, and spectra are kept as interleaved
  real and imaginary parts for it. Ten seconds of stereo through a
  two-second response at partition 256 (`examples/convolution_speed.rs`):
  1.25 times faster in `f32` and 1.5 in `f64`; a 63-tap direct FIR is as
  fast as before in `f32` and 1.1 times faster in `f64`. The output is
  still the same however the audio is cut into blocks.

### Formats

- `opus`: an Ogg Opus encoder of the CELT layer, written from RFC 6716 and
  RFC 7845 (issue #2). `opus::write`, `opus::write_file`,
  `AudioWriter::opus` and `AudioWriter::create_opus` write a constant
  (`OpusOptions::constant`) or variable (`OpusOptions::variable`) bitrate
  of 6 to 510 kbit/s a channel, in frames of 2.5 to 20 ms
  (`FrameDuration`), with the comments and pictures of a `Metadata`
  (`OpusOptions::with_metadata`) in `OpusTags`. One or two channels go
  into one stream, three to eight into the coupled pairs of the Vorbis
  layouts (mapping family 1), more into a stream each (family 255). Audio
  at another rate is taken to 48 kHz with a copy of `manguebyte-dsp`'s
  resampler, and the identification header records its rate. The
  pre-skip and the granule position of the last page make every stream
  decode to exactly the frames written (at 48 kHz), and the bytes are the
  same however the audio is cut into calls. On the crate's stereo test
  signal the noise lies 1.4, 8.7 and 15.7 dB under the masking threshold
  of the tests' model at 64, 128 and 256 kbit/s. SILK and hybrid frames
  are not written; see TECH_DEBT item 89.
- WAV reading and writing convert samples with `manguebyte_core::vector`,
  a run of samples at a time instead of one call per sample, and stereo is
  split from and into interleaved frames with it; the bytes and samples are
  the same as before. Ten seconds at 48 kHz in memory
  (`examples/pcm_speed.rs`), mono and stereo: 16-bit written 3.2 to 7 times
  faster, 24-bit and float 1.7 to 2.7; read in blocks of 512 frames 6 to
  16 times faster, whole 1.1 to 1.8; six channels 1.2 to 2.5 times
  faster, except whole float files, about as fast as before (0.9).
- The CELT bit allocation, band shapes and pulse coding of the Opus
  decoder are shared with the encoder, through a range encoder (RFC 6716,
  section 5.1) in place of the decoder: the decoder decodes as before, bit
  for bit.
- The Ogg page writer of the Vorbis encoder moved to the `ogg` module,
  shared with the Opus encoder; Vorbis files are unchanged.

### Signal processing

- Overlap-add resynthesis for spectral processing, in
  `manguebyte_dsp::stft` (issue #13). `Stft` takes a frame of
  `fft_size` samples every `hop`, applies the analysis window, runs the
  real FFT, hands the spectrum to a `SpectralProcessor` (a trait, or a
  closure through `Stft::from_fn`), transforms back, applies the
  synthesis window and overlap-adds. Any transform size from 1 to 65 536
  and any hop up to it are accepted, with any `Window` or its square root
  (`StftWindow::SquareRoot`) on either side, as long as the two windows
  overlap-add to a constant at that hop: `Stft::new` computes the sum
  and refuses the rest with `StftError::NotOverlapAdd`, which says by how
  much the sum strays. The synthesis window is divided by the sum, so an
  unchanged spectrum gives back the input exactly (within 1e-13 in double
  precision) after the latency, which is `fft_size - 1` less the
  synthesis window's leading zeros and reported by `latency_samples`.
  Frames fall at fixed points of the stream, so the output is the same bit
  for bit however the host cuts its blocks. Mono and any number of
  channels; nothing is allocated after `prepare`.
- Three effects built on it: `SpectralGate`, a gate per bin with a
  common threshold, thresholds of each bin's own, a floor and a learned
  noise profile, which makes it a simple denoiser (22 dB less white noise
  in the tests, a tone about 30 dB above it kept within 0.02 %);
  `SpectralFreeze`, which holds the magnitudes of a frame and keeps each
  bin's phase turning at its measured rate; and `PitchShift`, a basic
  phase-vocoder pitch shifter that moves each spectral peak with its
  region, after Laroche and Dolson (1999). See TECH_DEBT item 94 for what
  they leave over.

### Graph

- The engine mixes the inputs of a bus and its delay lines with
  `manguebyte_core::vector::add`, a run of frames at a time; the sums are
  the same, bit for bit. The browser engine of `manguebyte-web` adds its
  parts and a steady reverb send the same way.
- Sidechains, events and feedback are routed through `manguebyte-graph`
  (issue #9). Every change is additive; nothing in 0.1.0 was removed or
  changed its signature.
  - **Buses and channels.** Every node has a main input and a sidechain
    (`Bus`), which it receives as `ProcessContext::sidechain`, as a plugin
    receives its second bus. `connect_sidechain` feeds the sidechain, and
    `connect_with` takes a `Route`: the bus, a range of channels from the
    source into a range of the target, and an optional delay.
    `Graph::SIDECHAIN` is the sidechain the engine itself is given, as a
    source. A node with nothing routed to its sidechain receives the
    engine's own, as before. A `Compressor` keyed by another node sounds
    bit for bit as one keyed directly, in five block patterns, and so does
    Mangue Duck hosted as a CLAP and keyed through a graph
    (`manguebyte-host/tests/clap_duck.rs`).
  - **Events.** `connect_events` sends one node's events to another, from
    `Graph::INPUT` (the engine's events) and to `Graph::OUTPUT`
    (`GraphEngine::output_events`). `add_with_events` adds a node that
    sends events, with a closure that collects them after each block, for
    an arpeggiator of one's own or a hosted plugin's `output_events`.
    Events arrive on their frames, held back with the audio when latency
    compensation delays a path; on a shared frame they are merged in a
    fixed order (the engine's first, then by node in the order the nodes
    were added), whatever the block sizes and the order of connecting. A
    node with no event connection receives the engine's events, as
    before. Each input holds `EVENT_CAPACITY` (1,024) events a block;
    `GraphEngine::lost_events` counts what did not fit.
  - **Feedback.** `connect_feedback(from, to, frames)`, or a `Route` with
    a delay, is the only connection that may close a loop, a node's loop
    onto itself included; any other loop is refused with
    `GraphError::Cycle`, whose message now says that feedback needs a
    delay. The delay is in frames, not blocks, so a loop does not change
    with the block size: a delay of at least the largest block is free,
    and a shorter one, down to one frame, makes the engine run the graph in
    pieces no longer than it. A comb filter built this way matches the
    recursion computed frame by frame, bit for bit, with delays of 1, 3, 8
    and 13 frames in five block patterns.
  - **Latency compensation** now also lines up sidechains and events with
    the main inputs, and events with the graph's output. Feedback
    connections are not compensated: they delay by exactly what they say.
  - **Live edits.** A new plan takes over, from the plan it replaces, the
    contents of every delay both have (feedback, compensation, events held
    back), with a swap of vectors on the audio thread. An edit elsewhere in
    the graph is no longer heard on a compensated path or in a loop. The
    engine stays free of allocation, locks and frees while it processes
    and while it applies edits, which the allocation guard in
    `tests/graph.rs` checks for a graph that uses all of the above.
  - A node with a single input receives it copied rather than summed into
    silence, so a negative zero keeps its sign and a hosted plugin in a
    graph sounds bit for bit as it does alone.
  - Limits: TECH_DEBT 88.

### Fixed

- `cargo mangue` built the programs that write an LV2 bundle's Turtle files
  and an Audio Unit's `Info.plist` in the workspace's target directory,
  where cargo could give their builds of the plugin and of the framework the
  same file names as the workspace's own: after a bundle was made in
  release, `cargo test --release` could fail with "multiple different
  versions of crate `manguebyte_plugin`". They are now built in
  `target/manguebyte/helpers`.
- A block longer than a graph was prepared for was run in pieces with all
  of its events given to the first piece, at offsets past that piece's
  end, so they took effect at the end of the first piece instead of on
  their frames; the later pieces were also given the context's offset
  unchanged, which misaligned a sidechain. Each piece now gets the events
  that fall in it, on their frames, and an offset that says where it
  begins.
### Application state

- `manguebyte-state`, a new crate for what an editor edits, undoes, saves
  and hands to the audio thread: the part of JUCE's
  `juce_data_structures` a plugin with a real editor needs, designed in
  ADR 0008. A `Document` is a tree of typed nodes (numbers, text,
  switches, bytes, references to parameters and to other nodes) with ids
  that never change and are never reused. Every change is a named
  transaction through `Document::edit`, taken back whole if it fails, and
  undone and redone as one; `begin_gesture` and `end_gesture` make a knob
  drag one step, `edit_merging` merges a series by key, a new step
  discards the redo branch, and `HistoryLimits` bound the history by steps
  and bytes. There are no listeners: readers drain a change journal with
  cursors of their own (`Document::changes`), or poll a `VersionWatch`
  from another thread. `Document::link_params` mirrors a plugin's
  parameters on a node: `Tx::set_param` is an undoable step that writes
  the atomic, `sync_params` copies host automation in without touching
  the history. A versioned text form (`to_text`) and a compact binary one
  (`to_bytes`) load tolerantly, reporting what they skipped, and
  `Migrations` bring an application's older schemas up to date.
  `Settings` keeps keys and values in the user's configuration folder.
  `snapshot::channel` hands snapshots built from a document to the audio
  thread, which never allocates, frees or locks to read them (tested
  under the counting allocator). Random edit sequences, gestures, series
  and failed edits are undone back to every earlier tree and checked
  exactly. Limits: TECH_DEBT 87.
- `manguebyte-params`: `ExtraState`, state a `ParamSet` keeps beside its
  values, such as a document. `param_set!` lists such fields after
  `extra`; `ParamSet::save_extra` and `load_extra` reach them, and
  default to nothing.
- `manguebyte-plugin`: `save_state` writes a parameter set's extra state
  after the values, as named sections, in plugin state version 2;
  `load_state` reads versions 1 and 2. State without extra state is
  version 1, exactly as before. `manguebyte_state::SharedDocument` is
  such state, so a plugin's saved state, in every format, carries its
  document.
### Transport and timeline

- A tempo map, a transport clock and timeline primitives, in
  `manguebyte_dsp::timeline` (issue #8). `TempoMap` holds tempo points,
  each a step or a linear ramp to the next, and time signatures on bar
  lines, and converts between frames, seconds, quarter notes (PPQ
  positions) and bars, beats and ticks, in closed form both ways. A ramp
  is linear in time, as CLAP's `tempo_inc` describes one, so the tempo
  squared is linear in beats and every conversion is an algebraic formula
  that stays exact as a ramp flattens; the type's documentation derives
  them. A position lands on the first frame at or after it, and a frame's
  tempo and bar follow the same rule, so where a change lands and what a
  frame reports always agree. `TempoMap::from_midi_file` reads a Standard
  MIDI File's tempo and time signatures, and gives the same times as
  `MidiFile::tick_to_seconds`.
- `TransportClock` plays a map block by block: play, stop, seeking by
  frame or beat, a loop in beats or frames, a new map or sample rate that
  keeps the musical position. Each `ClockBlock` gives the `Transport` for
  the block, the `Piece`s it is cut into where the loop goes round (as
  many times as a short loop needs), and, as a `BlockTimeline` with its
  map, the beat, tempo and bar of every frame, the frames any beat lands
  on and the tempo points and time signatures inside the block. The
  position is a whole frame and everything musical is worked out from it,
  so the positions are bit for bit the same over six block patterns, at
  44.1, 48 and 96 kHz, through a ramp, two time signatures and a loop.
  `ClockControl` works a clock from another thread: commands and new maps
  go through the wait-free queue, the replaced map comes back to be freed
  there, and the position, tempo and state are published as atomics.
  Advancing the clock is under the allocation guard
  (`crates/manguebyte-dsp/tests/realtime.rs`).
- `ProcessContext::timeline` (`with_timeline`) hands a processor the map
  and the block's place on it when the transport comes from a clock.
- `Harness` plays a tempo map: `with_tempo_map`, `set_tempo_map`,
  `tempo_map`, `transport_clock`, `play`, `stop`, `set_recording`,
  `seek_beat`, `seek_frame` and `set_loop_beats`, so a test automates
  tempo and meter and checks what the plugin did with them. The plugin
  sees the map in its context.
- The standalone runner has a real transport: `Standalone::transport`
  gives a `ClockControl` (play, stop, seek, loop, a new tempo map while it
  plays), and `Engine::with_transport_clock` and `Engine::transport_clock`
  put a clock of the caller's in the engine.
- The CLAP wrapper reads the host's position in seconds, bar number and
  tempo increment, and the LV2 wrapper `time:bar`. The CLAP host sends
  them on when the transport knows them.
- Limits: TECH_DEBT 86.

### Plugins

- **Identity at run time** ([ADR 0012](docs/adr/0012-plugin-identity-at-run-time.md)).
  `Plugin::info()` gives a plugin's id, name, vendor, version and
  description, `Plugin::INFO` unless overridden, so one pre-built engine
  copied to several files, each with its own identity in data the copy
  carries, is several plugins. `plugin_info::<P>()` asks it once per
  plugin type, outside any lock but a brief one, and keeps it for the life
  of the library; the kind is always `INFO`'s, and an id that
  `is_valid_id` refuses (empty, white space, control characters) makes
  the whole of `INFO` the identity. `library_path()` names the file the
  plugin's code was loaded from, from what a wrapper recorded with
  `set_library_path` or else from `manguebyte_core::module::file`, and is
  known inside `info`.
  - CLAP: the descriptor and the id `create_plugin` takes come from
    `plugin_info`; `clap_entry.init` records the path the host gives when
    it names a file.
  - VST 3: `getFactoryInfo`, every class info and the class id
    (`class_id` of the run-time id, worked out once when the factory is
    made).
  - AU: the codes in the state dictionary and in the `Info.plist`
    `export_au!`'s hidden function writes.
  - LV2: `plugin_descriptor::<P>()` builds the descriptor on first use
    with the run-time URI and leaks it once, as the CLAP descriptor is;
    `descriptor!` now calls it, and `lv2_descriptor` gives null rather
    than unwinding if `info` panics. `plugin_uri` and the Turtle follow.
  - Saved state names the run-time id and takes only state with it: two
    copies with different ids refuse each other's state
    (`StateError::WrongPlugin`). The `Harness` checks and names that id.
  - `plugins/identity-probe` (not published) reads its identity from
    bytes appended to its library; its test hosts two copies of one
    library with different identities in one process through
    `manguebyte-host`: two CLAP descriptors, two VST 3 class ids, audio
    and state in each, and on Windows both editors open at once in real
    windows, through CLAP and VST 3, with window classes of two modules.
  - `cargo-mangue`: `layout::plan` and `bundle::write` are documented as
    public API for programs that bundle plugins without cargo, with an
    example of `write`.
- **Layouts** (issue #15). `Plugin::LAYOUTS` lists every layout a plugin
  can run with, and `Plugin::set_layout` tells it the one the host chose,
  right before every `prepare`. A wrapper offers the entries with the
  buses of `Plugin::LAYOUT`, any width, the sidechain optional
  (`AudioLayout::fits`, `offered_layouts`); `AudioLayout::with_sidechain`
  and `AudioLayout::MONO_INSTRUMENT` help write the list, and
  `Harness::with_layout` runs one in a test.
  - VST 3: `setBusArrangements` accepts the arrangements of the layouts
    offered and refuses others, `getBusArrangement` and `getBusInfo`
    report the choice, and `activateBus` switching the sidechain off runs
    the same layout without it when the plugin lists one.
  - CLAP: `audio-ports-config` offers each layout as a configuration, and
    `audio-ports-activation` switches the sidechain off.
  - AU: `SupportedNumChannels` lists the layouts, each element's stream
    format takes any width they give it, and `Initialize` refuses widths
    that add up to none of them.
  - LV2 describes its ports in the bundle and runs `LAYOUT`.
- **64-bit audio.** VST 3 accepts `kSample64` and every CLAP port declares
  `CLAP_AUDIO_PORT_SUPPORTS_64BITS`. Both run the plugin's 32-bit path:
  64-bit input is rounded to `f32` and the output widened, so the result
  is the 32-bit one exactly, whatever the block sizes, as the tests check
  through each format's interface. Steinberg's validator now runs its
  double-precision suite on both example plugins, and passes it.
- **More CLAP extensions** (issue #14).
  - Per-note automation: `CLAP_EVENT_PARAM_VALUE` with a note id or a key
    becomes `EventKind::NoteParamValue` for a parameter marked
    `per_note()` (`ParamFlags::per_note`), which also declares the
    per-note-id and per-key automation and modulation flags. `Synth`
    hands it to the voice of that note through `Voice::set_note_param`;
    the parameter keeps its value, and `flush` never stores such a value.
  - A modulation amount is converted around the parameter's value on the
    modulation's frame, as automation earlier in the block left it, so
    per-note modulation no longer depends on where the host cuts blocks
    (TECH_DEBT 26).
  - `voice-info` from `Plugin::voice_info` (`VoiceInfo`), read on each
    activation, with the host told when it changed; `note-name` from
    `Plugin::NOTE_NAMES` (`NoteName`).
  - `CLAP_EVENT_TRANSPORT` inside a block cuts it there: each piece
    reaches the plugin with its own events, sidechain, output frames and
    transport, as LV2 does with `time:Position` (TECH_DEBT 86).
  - The CLAP host (`manguebyte-host`) sends `NoteParamValue` to a hosted
    plugin as a per-note parameter value.
- **Notes out of AU and LV2** (TECH_DEBT 82). An Audio Unit that sends
  notes has one MIDI output (`kAudioUnitProperty_MIDIOutputCallbackInfo`)
  and hands the host's `kAudioUnitProperty_MIDIOutputCallback` a
  `MIDIPacketList` after each render, each message stamped with its
  frame. An LV2 plugin that sends notes writes them on its notify port,
  whose Turtle then says `atom:supports midi:MidiEvent`. Both send MIDI
  1.0 by the default translation, through `OutputTranslator`, from memory
  set aside before audio runs.
- Limits: TECH_DEBT 91.

### Hosting

- `manguebyte-host` reads the notes and MIDI that hosted LV2 and Audio
  Unit plugins send: `Lv2Processor::output_events` reads the output atom
  sequences that carry `midi:MidiEvent` after each run, and
  `AuProcessor::output_events` what the unit sends through
  `kAudioUnitProperty_MIDIOutputCallback`, registered when the unit offers
  one; each event on its frame of the whole block, however the block is
  cut, at most 4,096 per block, the rest counted in `lost_events`, and
  nothing allocated while processing. MIDI 1.0 becomes framework events as
  the CLAP and VST 3 hosts already make it (notes, poly pressure as note
  pressure, the rest as `EventKind::Midi`). `Plugin::activate_processor`
  gives an `any::AnyProcessor`, whose `output_events` hands over a hosted
  plugin's events in one type for all four formats; `Plugin::activate`
  is unchanged. `Lv2Library::from_entry` hosts an LV2 plugin linked into
  the program, from its bundle's Turtle files. Tested through the real LV2 and AU wrappers in process, at
  four block and piece patterns, with an overflowing plugin, and the same
  plugin as CLAP, LV2 and AU giving the same events, and as VST 3 the
  same but for note ids and 7-bit values (TECH_DEBT 82).

- `manguebyte-host` opens the editors of the CLAP and VST 3 plugins it
  hosts (issue #5), through the same methods on `ClapPlugin` and
  `Vst3Plugin`: `open_editor` in a parent window (`editor::ParentWindow`,
  the type a plugin made with the framework receives), `editor_size`,
  `set_editor_size`, `set_editor_scale`, `close_editor`. What an editor
  asks for comes back from `pump` as `HostEvent::Editor` (a new size, to
  be shown or hidden, closed), and `pump` also fires the timers the editor
  registered; `next_editor_timer` says when they are next due, and
  `editor_fds` and `editor_fd_ready` hand the file descriptors it watches
  to the application's loop. CLAP: the host's `gui`, `timer-support` and
  `posix-fd-support`; VST 3: `IPlugView` with an `IPlugFrame`, content
  scale and, on X11 systems, `IRunLoop`. Dropping a plugin closes its
  editor first. The example plugins' editors are opened, resized,
  rescaled and closed in real Win32 windows, and a knob dragged in each
  format reaches the hosted processor; fake plugins check the order of
  calls, failures part-way through opening, calls from inside callbacks
  and from other threads. Not yet on macOS and Linux in real windows, and
  not for LV2 or AU (TECH_DEBT 95).

- `manguebyte-host` hosts Audio Units, version 2 (issue #4), in a new
  `au` module behind a feature of the same name, on by default.
  `au::components` lists what the system's registry holds
  (`AudioComponentFindNext`), `AuLibrary::system` and `AuLibrary::load`
  (a `.component` bundle, its `Info.plist` in XML or binary form) make
  instances through AudioToolbox (`AudioComponentInstanceNew`), on macOS;
  `AuLibrary::from_factory` makes them in process from a component's
  factory function, through its `AudioComponentPlugInInterface`, on every
  system. `AuPlugin` sets each element's stream format (32-bit float, one
  buffer a channel), `MaximumFramesPerSlice` (a unit that refuses keeps
  its own, and longer blocks are rendered in slices), the main channels
  as `SupportedNumChannels` allows (`set_channels`), and the host's
  transport callbacks; lists parameters in their own unit and range,
  with the unit's own text both ways; reads latency and tail; saves and
  restores `ClassInfo` as the XML of its property list (`.aupreset`
  form); and bypasses with `BypassEffect`. `AuProcessor` is a
  `Processor<f32>` that feeds the main input and the sidechain through
  render callbacks, schedules every parameter change on its frame with
  `AudioUnitScheduleParameters` and every note and MIDI message with
  `MusicDeviceMIDIEvent`, answers the transport callbacks for each slice
  from the `Transport` it is given, and allocates nothing while it
  renders. The example plugins hosted as Audio Units sound the same, bit
  for bit, as with no host, however the blocks fall.
- The one interface (`any`) has `Format::Au`, `Library::load` takes a
  `.component` bundle, and `Library::from(AuLibrary)`, `as_au` and
  `as_au_mut` reach the format's own types. The scanner finds
  `.component` bundles in `~/Library/Audio/Plug-Ins/Components` and
  `/Library/Audio/Plug-Ins/Components` and instantiates each unit in a
  process of its own, so a unit that crashes or hangs is reported
  (`manguebyte-scan --manguebyte-scan au <path>`).
- The AU host shares the AU wrapper's declarations of Apple's C interface
  (`manguebyte_au::sys`) rather than declaring them again; the MIDI 1.0
  conversion of notes moved from the LV2 host to a module both use.
- Limits: TECH_DEBT 93.

### User interface

Scrolling, lists, drag and drop and pictures in `manguebyte-ui`
([ADR 0009](docs/adr/0009-scrolling-lists-and-drag-and-drop.md)), all
tested headless (`crates/manguebyte-ui/tests/views.rs`).

- `scroll_view(name, child)`: a viewport onto one child larger than
  itself, up and down, sideways (`.horizontal()`) or both (`.both()`).
  The wheel scrolls `Sizes::scroll_line` (40) a notch, and a touchpad's
  fractions of a notch add up exactly; Shift turns it sideways. The bars
  are drawn from the theme at the right and bottom edges, in room kept
  for them whether or not the child overflows: drag the thumb, press the
  track for a page. With the keyboard focus, which the view takes only
  when its child overflows, the arrows, Page Up, Page Down, Home and End
  scroll it. A view at its edge leaves the wheel to the view around it.
  Tab or assistive technology giving the focus to a widget out of sight
  scrolls every view it is in to it. `.position(State<Point>)` lets the
  application read and set the offset. Layout gains
  `Arrange::Scroll { x, y }`, `scroll_viewport` and
  `Placement::from_device`; the root moves what a view holds by its
  offset in whole device pixels and clips it to the viewport, so what is
  out of sight is neither drawn nor hit, and scrolling damages only the
  view, with the result equal to a whole repaint bit for bit at 1x,
  1.25x and 2x. `Root::scroll_metrics`, `UpdateCx::scroll`,
  `EventCx::scroll`, `EventCx::scroll_to` and `ScrollMetrics` let a
  widget of one's own be a scroll node.
- `list_view(name, data, state)`: rows an application keeps through
  `ListData` (row count, name, a revision number, and optionally its own
  row drawing, drag payloads and which drops it accepts), of which only
  those in sight are drawn and described. Rows are `Sizes::list_row` (24)
  tall; rows of different heights are not supported. Click, Ctrl+click
  and Shift+click selection; the arrows, Shift and Ctrl with them,
  Ctrl+Space, Page Up, Page Down, Home, End and Ctrl+A; typing the first
  letters of a row's name; activation by double click or Enter; the wheel
  and a bar. Everything is polled from a `ListState` (`Send`, so an
  editor spec can keep it): the selection, the keyboard's row, the offset,
  and `ListEvent::Activated` and `ListEvent::Dropped`. A test with 100 000
  rows checks that building, scrolling to the end and back, describing
  and selecting everything draw and ask for only the rows in sight (10 or
  11), that the offset stays exact at 2.4 million pixels, and that a
  steady list draws nothing.
- Drag and drop inside the interface (`manguebyte_ui::drag`): a widget
  starts a drag with `EventCx::start_drag(DragPayload::new(data,
  description))`, any `Send + Sync` type of the application's; targets
  accept or refuse it by type and place through `Widget::drag`
  (`DragEvent::Over`, `Left`, `Drop`, and `Ended` to the source); the root
  draws a ghost with the description over everything (a `DragItem` to
  assistive technology), scrolls scroll views and lists near whose edges
  the pointer rests, and cancels on Escape, a release where nothing
  accepts, a second button, `PointerCancel` or losing the window's focus.
  Lists drag their selected rows and show where a drop would go; tests
  reorder rows within a list and move them between two. `Root::dragging`
  and `Root::drop_target` say what is going on.
- `image(name, image)`: a picture fitted by `Fit::Contain`, `Cover`,
  `Stretch` or `None`, with a version per density
  (`.density`, `.with_density`): the least dense version with at least as
  many pixels as the screen is drawn. A 2x picture at scale 2 is copied
  pixel for pixel.
- Accessibility: roles `ScrollView`, `List`, `ListItem` and `DragItem`, a
  `selected` state, `AccessItem` for parts of a widget that are nodes of
  their own (`Widget::access_items`, `Widget::act_item`, `Root::act_item`,
  `AccessEntry::item`, `Found::item`), and `assert_accessible` checks that
  lists and scroll views have names and list items say whether they are
  selected. `Harness::wheel_by` sends fractions of a notch and sideways
  turns.
- `examples/views_demo.rs`: two lists with drag between them inside a
  scroll view, driven headless and written to PNG files.
- Limits: TECH_DEBT 96.

Application windows, files dropped from other programs and the system's
file dialogs ([ADR 0010](docs/adr/0010-application-windows.md)), tested
headless on every system and with real windows on Windows
(`crates/manguebyte-window/tests/app.rs`, `win32_app.rs`,
`crates/manguebyte-gui/tests/app_window.rs`, `win32_app_window.rs`).

- `manguebyte_window::App`: an application's own top-level windows and
  the loop that runs them. `App::open_window(AppWindowOptions, handler)`
  makes a window with a title, an inside of a size in logical pixels
  (made at the scale of the monitor it opens on), resizable or `fixed`,
  with `with_min_size` and `with_max_size`; it runs the same code as a
  child window for drawing, timers, input, the clipboard and the scale.
  The close button, Alt+F4, the window menu and the taskbar reach the
  handler as `Event::CloseRequested` and nothing else: the application
  closes the window with `close()`, or not. `App::run` is the thread's
  message loop until the last of the application's windows closes
  (`RunEnd::LastWindowClosed`), `quit` is asked (`RunEnd::Quit`) or
  someone posts `WM_QUIT` (`RunEnd::QuitMessage`); child windows of a
  host never need it. On Win32 `WM_GETMINMAXINFO` gives the system the
  limits, `WM_DPICHANGED` changes the scale and then takes the suggested
  rectangle, a minimised window keeps its back buffer, and keys the
  handler leaves go to the default handling. `set_title`, `title`,
  `size_limits` and `is_app_window` on every window. `App::headless`
  runs steps a test posts (`HeadlessApp::post`) in place of messages, and
  `Headless` gains `request_close`, `drag_frame_to` and `move_to_monitor`.
- File drops: a window made `with_file_drops` hears `Event::DragEnter`,
  `DragOver`, `DragLeave` and `Drop`, reads what the drag carries
  (`DragData`: paths and text) with `drag_data`, and takes or refuses it
  by its answer, which the system shows with its cursor (a copy, or a
  link where only that is allowed, never a move). A drag that comes while
  the handler runs is refused and heard at the next move. On Win32 it is
  an `IDropTarget` of the crate's own, registered with `RegisterDragDrop`
  only on a single-threaded apartment and revoked in `WM_DESTROY`; an
  `App` initialises OLE for its thread, a child window never does for a
  host's. `Headless::drag_enter`, `drag_over`, `drop_at`, `drop_files`.
- File dialogs: `open_files`, `pick_folder` and `save_file` on a window,
  or through a `Dialogs` handle an interface keeps, described by a
  `FileDialog` (title, `FileFilter`s, starting folder, the name a save
  offers, several files or one), return `Option<Vec<PathBuf>>` or
  `Option<PathBuf>`, `None` for a cancel. On Win32 they are
  `IFileOpenDialog` and `IFileSaveDialog`, modal to the window's
  top-level window. A headless window answers what the test scripted
  (`Headless::answer_next_dialog`, `on_next_dialog`, which may drive the
  window while the dialog is "open") and records what was asked
  (`take_dialogs`). Showing the system's dialogs is tested by hand only;
  the tests configure them through COM and read them back
  (`testing::prepare_dialog`).
- `manguebyte-ui`: `Root::file_drop(FileDrop, &DropData)` runs a drag
  from another program as a drag of the interface's own with no source
  and no ghost, so widgets take files through `Widget::drag` with a
  payload holding the `DropData` (`DropData::has_file_with_extension`).
- `manguebyte-gui`: `Editor::open_in_app` and `open_app_window` show an
  interface in an application window and return an `AppEditor`
  (`on_close_request`, `dialogs`, `set_visible`); gestures set the
  parameters given, as a standalone program's host would.
  `Editor::with_file_drops` lets an editor inside a host take drops.
  `HeadlessEditor` drags files. `examples/app_window.rs` lists the files
  dropped on it and opens the three dialogs.
- `win32::testing` (feature `testing`): `Drag` plays a drag against a
  window's registered drop target with a real `IDataObject` of its own,
  `Ole`, `is_drop_target`, `tracking_limits`, `dpi_changed`,
  `window_title`, `window_rect`, `is_resizable`, `post_quit`, and
  `Message::Close` and `Message::SysCommandClose`.
- Limits: TECH_DEBT 97.

Audio widgets in `manguebyte-ui`, for a sampler's screen: a waveform
view, a piano keyboard, an envelope editor and a zone map, each tested
headless for its behaviour, its accessibility, and a partial repaint
equal to a whole one bit for bit at 1x, 1.25x and 2x
(`crates/manguebyte-ui/tests/waveform.rs`, `keyboard.rs`, `envelope.rs`,
`zone_map.rs`).

- `Waveform`: a sample buffer of any number of `f32` channels
  (`new`, `from_shared`, `from_interleaved`) with a summary of the lowest
  and highest sample of every block of 64, 128, ... frames, about a
  sixteenth of the buffer's size. `min_max(channel, frames)` is exact (a
  test checks it against reading every sample) and reads at most 126
  samples and two entries a level; `samples_read` counts what queries
  read, and a test draws a ten-minute file at 2x reading less than 1.2 %
  of it, and nothing more while the playhead moves. `zero_crossing_near`
  and `zero_crossing_beyond`.
- `waveform_view(name, WaveformState)`: a lane per channel, a span per
  device pixel column (`Scene::columns`), or a line through the samples
  past a frame per device pixel, kept until the frames shown change.
  Ctrl and the wheel, or a touchpad's pinch, zoom around the pointer
  (`ZOOM_STEP`, at most `MAX_PIXELS_PER_FRAME`); the wheel scrolls and
  hands on at an end. `Marker`s the application defines are dragged
  from where they were taken, within `MarkerOrder` rules, onto the
  nearest zero crossing if `set_snap_to_zero`; `set_sample_points` makes
  a sampler's start, end and loop with their rules. A drag elsewhere
  selects; a click reports where. Up and Down choose a marker, Left and
  Right move it a pixel's worth (Shift a frame, or to the next zero
  crossing), Home and End as far as its rules allow, `+` and `-` zoom.
  `WaveformState` holds the sample, the frames shown, the selection,
  the markers and the playhead, and `WaveformEvent`s (`MarkerMoved`,
  `Selected`, `Clicked`). Markers are sliders to assistive technology,
  in seconds and frames.
- `KeySpan`: notes laid out a column each, the white keys sharing an
  octave as a piano's do (`column`, `white_key`, `note_at`, `PIANO`,
  `KEYS_61`), so that widgets over the same span line up key for key.
  `note_name` ("C4" for 60) and `note_name_portuguese` ("Dó4").
- `piano_keyboard(name, span, KeyboardState)`: a press plays a key with
  a velocity from 1 at its top to 127 at its bottom, a drag plays a
  glissando, and with the focus the letters A to ' play an octave and a
  half (Z and X move it, `computer_keys(false)` turns them off), the
  arrows choose a key and Space plays it. `NoteEvent::On` and `Off` are
  polled from the state, one of each per key however many ways it is
  held; closing the interface stops what it holds. The application
  lights notes (`set_sounding`) and marks `KeyRange`s with their roots.
  Each key is a button named by its note (`note_names` for others),
  pressed to hold it.
- `envelope(name, attack, decay, sustain, release)`: an ADSR curve whose
  corners are dragged as gestures on the parameters, as a knob's are
  (two at once for the end of the decay); `attack_curve`,
  `decay_curve` and `release_curve` bend its segments and drag up and
  down. Escape cancels a drag, Alt and a click reset, a double click or
  Enter types a value, the wheel and the keys step the part chosen
  (Left and Right choose it); each part is a slider to assistive
  technology.
- `zone_map(name, span, ZoneMapState)`: `Zone`s of keys by velocities,
  selected by clicks and by a band, moved by whole keys and velocities
  within the map, resized by their edges and corners, with overlaps
  marked; Page Up and Down, the arrows (with Shift, Alt, Alt and Shift),
  Delete, Ctrl+A and Escape do the same from the keyboard. Drags it
  `takes` make zones where they land, one a key: files from other
  programs by default, rows of a `list_view` when the application says
  how to name them. `ZoneEvent::Edited`, `Created` and `Removed`. Zones
  are list items to assistive technology, saying their keys, velocities,
  root and overlaps.
- The theme gains `Palette::key_white` and `key_black`.
- `examples/sampler_screen.rs` composes the four with a list of samples
  and writes the screen to PNG files at 2x and 1.25x;
  `manguebyte-gui`'s `examples/sampler_window.rs` opens it in an
  application window on Windows.
- Limits: TECH_DEBT 98. The spectrum view and the EQ editor are not
  started.

### Breaking

- In `manguebyte-ui`: `Arrange` has a new variant, `Scroll`, so a `match`
  on it without a wildcard arm needs an arm for it; `AccessState` gains
  `selected`, `AccessEntry` and `testing::Found` gain `item`, and
  `Sizes` gains `scroll_bar`, `scroll_line` and `list_row`, so struct
  literals of them must name the new fields. The new `Widget` methods
  have defaults.
- `AudioBuffer::apply_gain_ramp` works the gain of frame `i` out as
  `start + step * i` instead of adding `step` frame after frame, so a ramp
  no longer drifts on a long block, and its samples can differ in the last
  bits. `AudioBuffer::rms` sums the squares in the fixed order of
  `vector::sum_of_squares` and can differ in the last bits; the peak of a
  buffer holding a NaN is the canonical NaN. `ConvolutionEngine`'s direct
  head adds its taps in another order (see DSP above), which also changes
  the last bits.

- `EventKind` has a new variant, `NoteParamValue`, and `ParamFlags` a new
  field, `per_note`: a `match` on `EventKind` without a wildcard arm needs
  an arm for it, and a `ParamFlags` struct literal must name it or end
  with `..ParamFlags::default()`. `ParamDescription::to_json` writes
  `"per_note":true` in the flags of a parameter that has it.
- Every CLAP audio port now has the `CLAP_AUDIO_PORT_SUPPORTS_64BITS` flag,
  and a VST 3 plugin whose `LAYOUTS` include its layout without the
  sidechain runs without one until the host switches the sidechain bus on
  with `activateBus`, which is when VST 3 hosts route something to it. A
  plugin that lists only `LAYOUT`, the default, behaves as before.
- `Transport` has three new public fields, `position_seconds`,
  `bar_number` and `tempo_increment`. A struct literal that names every
  field must name them too, or end with `..Transport::STOPPED`.
  `Transport::advance` moves the first two on.
- `Harness::with_tempo` panics on a tempo that is not positive and finite,
  where it used to report it as it was, and `Harness::new` on such a sample
  rate. The harness's transport now comes from a clock: its beats are
  worked out from the frame instead of added up block by block, which can
  differ in the last bits, and it gives the three new fields.

### Devices

- MIDI 2.0 through the systems' own Universal MIDI Packet output (issue
  #1). Every change is additive, and MIDI 1.0 stays the default.
  - `MidiProtocol::Midi2`, and `MidiOutput::open_with_protocol(id,
    MidiProtocol::Midi2)`, which opens a port through the system's UMP
    output when the system has one and the port speaks MIDI 2.0, and
    sends every packet unchanged: a MIDI 2.0 note with its 16-bit
    velocity, its attribute and its per-note controllers reaches the
    port as it left the plugin. Anywhere else the port is opened as
    `MidiOutput::open` opens it, with MIDI 1.0 bytes and the default
    translation, and `MidiOutput::fallback` says why
    (`ProtocolFallback`: not asked for, the port speaks MIDI 1.0, the
    system cannot, or the UMP connection failed).
  - `midi_output_ports_with_protocols` lists every output port with the
    protocol it would speak and why; `ump_output_backend` names the
    system's UMP output or says why there is none.
  - **macOS and iOS:** CoreMIDI's `MIDISendEventList` with event lists in
    `kMIDIProtocol_2_0`, to destinations whose `kMIDIPropertyProtocolID`
    is MIDI 2.0 (macOS 11, iOS 14).
  - **Linux:** the ALSA sequencer's UMP events, from a client in the MIDI
    2.0 protocol to a port of a MIDI 2.0 client (alsa-lib 1.2.10, Linux
    6.5).
  - **Windows:** MIDI 1.0 through WinMM, as before. Windows MIDI
    Services' API is a WinRT component the `windows` crate does not
    project (see TECH_DEBT 90); `ump_output_backend` says whether its
    service and its App SDK runtime are installed.
  - Both libraries are opened at run time, so a program built with this
    runs on an older system and speaks MIDI 1.0 there, saying what is
    missing.
  - The output thread (`MidiOutput::spawn`) hands a MIDI 2.0 port its
    packets unchanged, and the packets due at one wake-up go to the
    system in one call; the audio thread's side, `midi_output_queue`,
    is unchanged and still does not allocate (`tests/realtime.rs`). On a
    MIDI 2.0 port, `send`, `send_system` and `send_raw` send packets on
    group 0: MIDI-CI replies go out as 7-bit system exclusive packets.
  - `manguebyte-ump-io`, a new crate for the foreign calls, which the
    device crate, which forbids `unsafe`, cannot make: CoreMIDI's
    `MIDIEventList` and the sequencer's `snd_seq_ump_event_t` laid out in
    Rust and tested on every system, and the calls themselves, looked up
    with `dlopen` and `dlsym`. Its only dependency is `libc`, already in
    the tree.
  - The example `midi2_output` lists the output ports with their protocol
    and, only with `--send <port id>`, sends one a short, quiet note. CI
    sends a MIDI 2.0 note with a per-note controller to a UMP endpoint of
    its own on macOS (a CoreMIDI virtual destination) and, where the
    runner has the sequencer, on Linux, and checks that every word
    arrives.
  - Limits: TECH_DEBT 82 and 90.

## [0.1.0] - 2026-10-19

The first release on crates.io: 25 crates, `manguebyte-pro-io` under the
GPLv3 and every other one under MIT or Apache-2.0. `cargo mangue new` writes
a crate that depends on this release from crates.io; `--manguebyte-git`
follows the repository instead, and `--manguebyte-path` a checkout.

### Plugins

- Aratu Bridge (`plugins/aratu-bridge`), an example CLAP, VST 3 and Audio
  Unit instrument: share the music, not the audio. An Aratu page sends it
  over a WebSocket on 127.0.0.1 what it sends its own engine (control
  calls, event records with a beat for a frame, SFZ files as bytes), and
  the plugin renders them with `manguebyte-web`'s engine in the DAW, on
  the DAW's device and in time with its transport: tempo changes, loops
  going round inside a block (with the next lap sent ahead), jumps and
  stops, sorted out by epochs. The handshake checks the Origin against an
  allow-list, the Host and a subprotocol; messages are limited in size;
  WebSocket, SHA-1 and base 64 are written from RFC 6455, 3174 and 4648.
  The protocol is `docs/ARATU_BRIDGE.md`. A session sent over a real
  socket renders bit for bit as the engine renders it alone, through
  `Harness` in several block patterns and hosted as a CLAP
  (`manguebyte-host/tests/clap_aratu_bridge.rs`); `process` never touches
  the heap while a page sends everything it can. Tried by hand as a CLAP
  in REAPER 7.82 on Windows, with a Chromium page on `http://127.0.0.1`:
  the page sent 44 events and the plugin applied 44, none late or
  dropped, following REAPER's tempo. Limits: TECH_DEBT 84.
- `SamplerLoader::load_boxed` and `take_retired` hand instruments made
  on another thread to a sampler, and back, without allocating or
  freeing on the thread the loader is on.
- Plugins send notes, expressions and MIDI out. A plugin that sets
  `Plugin::NOTE_OUTPUT` writes, from the new `process_with_output`
  (whose default calls `process`), into an `output::OutputBuffer` with
  room set aside: notes, chokes and per-note expressions as the framework
  has them, any MIDI 2.0 channel voice message (per-note controllers,
  attributes, per-note management, 16- and 32-bit values) and MIDI 1.0.
  `output::OutputTranslator` writes it as MIDI 2.0 packets, as MIDI 1.0
  by the default translation, or as MPE, the inverse of how `NoteInput`
  reads them, and a note number an expression shaped is detached and
  reset before its next note; `output::NoteIds` finds notes named by id
  alone. `Harness::take_output` keeps what a plugin sent on the
  timeline. `manguebyte-clap` offers an output note port and sends
  CLAP's own notes and note expressions, `CLAP_EVENT_MIDI2` where the
  host takes MIDI 2.0 and `CLAP_EVENT_MIDI` otherwise, or MIDI 2.0, MPE
  or MIDI 1.0 to a host without CLAP's dialect. `manguebyte-vst3` offers
  an output event bus with note on and off, polyphonic pressure, note
  expression values and `LegacyMIDICCOutEvent` for channel messages.
  `manguebyte-standalone` sends to a MIDI output port
  (`Standalone::start_with_midi_output`, `Engine::with_midi_output`), as
  MIDI 1.0 or MPE. All of it is tested for heap use, and against the
  framework's own input; LV2 has no output yet.
- MPE and MIDI 2.0 reach plugins as per-note events in every format,
  through the plugin's `NoteInput`. `manguebyte-clap` offers the MPE and
  MIDI 2.0 note dialects beside CLAP and MIDI and reads
  `CLAP_EVENT_MIDI2` packets; a phrase played as CLAP notes and
  expressions, as MPE and as MIDI 2.0 gives the same output.
  `manguebyte-vst3` reads notes on the channels of a zone keeping the
  host's note ids, and puts the host's parameter changes and events in
  time order before reading them, so a member channel's bend lands before
  the note after it. `manguebyte-lv2` reads its MIDI atoms the same way.
  The CLAP, VST 3 and LV2 wrappers forget the notes they followed when
  the plugin is reset or deactivated.
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
  real C descriptor, acting as a host. `cargo mangue bundle --format lv2`
  writes the bundle (below), and CI loads the example plugins' bundles
  with lilv's `lv2ls` and `lv2info`; `lv2lint`, which the Ubuntu archive
  does not carry, has not read them, and no LV2 host has run a plugin yet.
  `export_lv2!` also defines a hidden function at the crate root,
  `__manguebyte_lv2_bundle_files`, through which `cargo mangue` gets the
  Turtle files.
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
- `manguebyte-au`: Audio Units (version 2) for macOS. `export_au!(MyPlugin)`
  exports `ManguebyteAudioUnitFactory`, which hands a host an
  `AudioComponentPlugInInterface` for the same `Plugin`, through the plain
  C AudioUnit interface declared by hand from Apple's headers. The type is
  `aumu` for an instrument, `aufx` for an effect and `aumf` for a note
  effect; the subtype and manufacturer are hashed from the plugin's id
  (`subtype_code`, `manufacturer_code`), and are the same on every machine.
  Fixed elements (main input, sidechain, output) in 32-bit float with one
  buffer per channel at any sample rate; input pulled from the render
  callback or the connection the host set; parameters with their units,
  value strings and text both ways; `ScheduleParameters` and
  `MusicDeviceMIDIEvent` (and `StartNote`, `StopNote`) on their frames; the transport from the host's
  callbacks; `ClassInfo` holding the codes, the present preset's name and
  the state text; the present preset (`PresentPreset` and the deprecated
  `CurrentPreset`, -1 and "Untitled" until the host sets one); latency,
  tail, maximum frames per slice, channel layouts, bypass for effects,
  property listeners and render notifications. Render allocates nothing
  and never waits. The wrapper builds on every system: elsewhere than
  macOS a stand-in for the few CoreFoundation calls it makes lets
  `tests/abi.rs` act as a host through the factory, with test plugins
  that sound the same as with no host, bit for bit, and no CoreFoundation
  object leaked. `info_plist` writes the bundle's `Info.plist`, which
  `export_au!` makes available to `cargo mangue` through a hidden
  `__manguebyte_au_info_plist`. Both example plugins export an AU. A
  macOS CI job runs the wrapper's tests against the system's
  CoreFoundation, installs both examples as components and runs Apple's
  `auval` on them, which both pass. No DAW has loaded them yet.
- `cargo mangue bundle --format au` writes `Name.component` on macOS (the
  library, `PkgInfo`, and the `Info.plist` from the plugin's code through a
  small program, as for LV2's Turtle files), signed ad hoc; it is refused
  for Windows and Linux. `install --format au` copies it to
  `~/Library/Audio/Plug-Ins/Components` (`/Library/...` with `--system`),
  and `validate --format au` runs `auval -v` with the codes the bundle
  names, judging its last word. The facade has an `au` feature.
- `manguebyte-plugin`: the `Plugin` trait every format wraps; `Harness`,
  which runs a plugin in a unit test with scheduled notes and automation;
  `ParamWatcher`, which turns parameter changes made outside the host into
  events; plugin state as text.
- `plugins/pluck`: an example 16-voice instrument, and
  `scripts/bundle-clap.sh` to build it into a `.clap`.
- `plugins/duck`: an example effect, a sidechain ducker with a bypass that
  does not click. Both examples are validated as CLAP and as VST 3 in CI,
  and both are exported as LV2 too.
- Plugins declare the MIDI controllers they listen to:
  `Plugin::MIDI_CONTROLLERS`, a `MidiControllers` set built in a const
  (`NONE`, `MUSICAL`, the default, `ALL`, `list`, `with`, `without`,
  `per_channel`, `contains`, `wants` and more) of `MidiController`s (a
  control change, made with `MidiController::control(n)`, which refuses a
  number above 127, or `CHANNEL_PRESSURE` and `PITCH_BEND`). `MUSICAL` is
  pitch bend, channel pressure and CC 1, 2, 7, 10, 11, 64, 66, 67, 120, 121
  and 123. The VST 3 wrapper exposes only those: one hidden parameter for
  each, shared by every channel, so a VST 3 instrument shows 13 hidden
  parameters by default instead of 2,080 (Mangue Pluck shows 6 + 13). A
  move of a shared controller reaches the plugin on all 16 channels, one
  message per channel in order, so that `Synth` applies it to notes on any
  channel; `per_channel()` gives each channel parameters of its own and
  keeps channels apart. The ids of channels 2 to 16 are gone unless a
  plugin asks for them, and a NaN point on a controller parameter is
  dropped. CLAP is not affected.
- VST 3: the view shows the editor with `OpenEditor::set_visible(true)`
  once `attached` has succeeded, since VST 3 has no call that shows a view,
  and hides it with `set_visible(false)` at the start of `removed`.
- Mangue Duck has an editor: a title, the Bypass switch, knobs for
  Threshold, Depth, Attack and Release showing their values in the
  parameters' own words, and a Reduction meter from 0 to 40 dB, in a fixed
  layout of 440 by 210 logical pixels. The processor feeds the meter
  through a `ReductionMeter` (`Duck::reduction_meter`), written without
  allocating once per block; `editor()` takes it, and the meter falls to 0
  dB once nothing has been published for `METER_HOLD` (500 ms). A sample
  of the sidechain that is not finite counts as silence, where one
  infinite sample used to stop the ducking until a reset.
- Mangue Pluck has an editor with a knob for each of its six parameters
  (520 by 160). It is now in tune while its filter closes, within 0.15 cent
  at the settings its tests measure, by following the filter's phase with
  the new `StateVariableFilter::phase_response`; it never had the
  Karplus-Strong delay its documentation once spoke of. Its output stays
  under -1 dBFS with every parameter at its maximum: a resonance
  compensation, a trimmed reverb, a share of the headroom per voice, a
  soft knee from -3 dBFS (`OUTPUT_KNEE_DB`) and a limiter at -1 dBFS
  (`OUTPUT_CEILING_DB`) with no latency, so nothing comes out flat at the
  ceiling. The reverb's blend and the level follow their events on the
  exact frame, so renders are identical bit for bit whatever the blocks and
  after any reset. The default level is now -4 dB (`DEFAULT_LEVEL_DB`), up
  from -12 dB: with the new headroom a note at -12 dB had become about 12
  dB quieter than before, and at -4 dB a chord of three or four notes
  around middle C peaks at about -6 dBFS. NaN and extreme values in note
  expressions, cutoff modulation and the pitch wheel are guarded against.
- Both example plugins read every parameter event through
  `Param::stored_plain`: a NaN is ignored and a value out of range is held
  to the range.
- A test loads both example plugins' libraries into one process on
  Windows, opens both editors in real windows through the CLAP `gui`
  extension, reads their pixels back, fuzzes the `gui` calls and unloads
  the libraries in either order, leaving no window, GDI object or window
  class behind (ADR 0007, item 8).
- The facade crate has the features `lv2`, `gui` (re-exporting
  `manguebyte-gui` as `gui` and `manguebyte-ui` as `ui`) and `sampler`,
  and its prelude has `Convolution` and its engine, loader, routing and
  settings, `FirFilter`, `FirMode`, `Expander`, `KeySource`, `Wavetable`,
  `WavetableOscillator` and `split_at_events_with`.
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
- `cargo mangue bundle --format lv2` writes `<Name>.lv2`: the library
  (`Name.dll`, `Name.so` or `Name.dylib`, one universal binary on macOS,
  where the library is signed ad hoc) and the Turtle files. Only the
  plugin's code can describe it, so the tool writes, builds and runs a
  small program against the plugin's crate, in the plugin's target folder
  with its profile, its features and a copy of its workspace's
  `Cargo.lock`; the crate must invoke `export_lv2!` at its root and have
  the `rlib` type. LV2 is bundled only when asked for (`Format::DEFAULT`
  is CLAP and VST 3); `--all-features` is refused for it. `install` and
  `uninstall` know LV2's folders, and `validate`, which has no LV2
  validator, refuses LV2. New crates from `cargo mangue new` enable the
  facade's `lv2` feature and export LV2 too.

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
  its gestures and calls `EditorContext::closed`. An editor opens hidden,
  with no frame timer, and starts when the host shows it through
  `OpenEditor::set_visible`: CLAP's `show`, or the VST 3 view once
  `attached` has succeeded.
  - The `headless` feature adds `Editor::on_headless_open` and
    `HeadlessEditor`, and only adds API: an editor opens headless only when
    it was given a hook, so `cargo test --workspace` leaves every other
    editor alone. Tests drive an editor with a headless hook end to end
    through the real CLAP interface and VST 3's COM interfaces, open and
    close it thousands of times while `process` runs under the counting
    allocator, fuzz the hosts' calls, and compare it with golden images at
    scales 1 and 2. On Windows, a test embeds it in a real window and reads
    its pixels back.
- Text entry. `manguebyte-ui` has `text_field`, one line of text kept in a
  `State<String>`: a caret and a selection, by the pointer (click, drag,
  double and triple click, Shift) and by keys (arrows by cluster, and by
  word with Control or Option; Home, End, Backspace, Delete, Control with
  A, C, X and V, and the Insert forms), with a filter hook for characters,
  a validation hook for the text, a bounded length, Enter to take the edit,
  Escape to throw it away and losing the focus to take it. Knobs and
  sliders open their value to be typed on a double click or Enter, read by
  the parameter's own parser (`ParamHandle::parse`) and sent as one
  gesture, or not at all if the text was left as it was; a click with Alt,
  Control or Command now puts the default back,
  which a double click used to. Typed characters arrive as `Event::Text`,
  in the person's layout; input methods have a protocol of their own
  (`Event::Composition`), which the widgets follow and no backend sends
  yet. Copy and paste go through a `Clipboard` the root is given, a
  `MemoryClipboard` unless it is given another. Widgets hear focus changes
  (`Widget::focus_changed`) and give their caret (`Widget::caret`); the
  root says when the focused widget takes text (`Root::wants_text`), and
  uses the release of every key whose press it used. Accessibility: the
  `TextInput` role and an editable state, and `assert_accessible` checks
  text inputs and the contrast of text on the selection.
- `manguebyte-window` has the clipboard (`Window::clipboard`, the system's
  Unicode text on Win32, opened per call; one of the window's own when
  headless, read and filled through `Headless`), `take_focus`, and
  `set_text_caret`: on Win32 a hidden system caret, made only while the
  window has the keyboard, and the input method asked to compose there
  and list its candidates below. Typed text from input methods arrives as
  the system turns it into `WM_CHAR`. The `testing` feature adds
  `system_caret`, `composition_window` and `HeldKeys`, which holds keys in
  the thread's keyboard state.
- `manguebyte-gui` gives typed text to the interface, the window's
  clipboard to its root, and the caret to the window. While the interface
  takes text the window asks a host's dialog for every key, and the editor
  takes the keyboard when typing starts while the host has it. Keys a
  VST 3 host delivers through `onKeyDown` type their characters too, and a
  keystroke that arrives both through the host and through the window
  before the next frame is taken once. The crate documents what each
  format does about the keyboard on Win32.
- Popup menus and a combo box. A widget opens a `Menu` of `MenuItem`s
  (choices with ids, check marks, disabled items, separators and submenus)
  with `EventCx::open_menu`, below or above itself as there is room and at
  least as wide, or with `EventCx::open_menu_at`, at a point; it hears the
  result as `Event::Menu` (`MenuEvent::Chosen` or `Closed`). Menus are
  drawn by the interface's own renderer over everything else, inside the
  editor's window, as ordinary widgets added after the interface's own;
  submenus open beside their item, on the left where the right has no
  room. The pointer hovers, clicks, and chooses by press, drag and
  release; a press outside closes every menu and does nothing else. Keys:
  Up, Down, Home, End, Enter, Space, Right and Left for submenus, Escape
  and Tab, and a typed letter to jump; every key is the menu's while one
  is open. The keyboard focus is on the highlighted item and comes back
  to the widget that had it. The window losing the keyboard closes them.
  `combo` lists a stepped parameter's values in its own words (at most
  `MAX_CHOICES`), the current one checked, and sends a choice as one
  gesture; Up and Down and the wheel step through the values while it is
  closed. Accessibility: the `ComboBox`, `Menu`, `MenuItem` and
  `Separator` roles, an expanded state, and `assert_accessible` checks
  combo boxes and text on the track colour that marks a menu's
  highlighted item. `layout::natural_size` gives a tree's natural size.
  `manguebyte-gui` asks a host's dialog for every key while a menu is open.
- Hover and tooltips. The widget under the pointer, while no button is
  held, is the hovered one: it hears it (`Widget::hover_changed`) and can
  draw it (`PaintCx::is_hovered`); `Root::hovered` says which it is, and
  `Event::PointerLeave` (from the window's own) ends it. A view given a
  `tooltip` shows it in a small panel below the pointer once the pointer
  has rested on the view, or on a widget inside it, for `TOOLTIP_DELAY`
  (500 ms) on the frame clock; it goes when the pointer leaves, and with a
  press, a key, a wheel turn, a menu or the window losing the keyboard,
  and after a press or a key comes back only once the pointer has been
  elsewhere. The text is also the view's description for assistive
  technology (`AccessNode::description`), shown or not, and a shown
  tooltip has the `Tooltip` role.
- Labels measure their text rounded up and a pixel more, as the controls
  do, so a label given just its natural width is no longer cut with an
  ellipsis.
- Breaking, in `manguebyte-ui`: `AccessState` gains `editable` and
  `expanded`, and `AccessNode` gains `description`, so building either
  with a struct literal needs the new fields (`AccessNode::new` and its
  `with_` methods are unchanged); a double click on a knob or a slider
  types a value instead of putting the default back.
- `Param::stored_plain` says what a parameter would store for a value,
  without storing it.

### Sampler

- `manguebyte-sampler`, a new crate: an SFZ player. `Sfz::parse` reads the
  `<control>`, `<global>`, `<master>`, `<group>` and `<region>` headers
  with inheritance, comments, `#define` and `#include` (through a sample
  source, with cycle, depth and count limits), `default_path`,
  `note_offset` and `octave_offset`; opcodes it does not play are kept and
  reported as warnings, out-of-range values are clamped with a warning, and
  unreadable text is an `SfzError` with its file and line. Inherited
  opcodes are shared, and the opcodes a region takes are capped
  (`sfz::MAX_REGION_OPCODES`).
- `Instrument::from_sfz` loads the samples through a `SampleSource`:
  `FileSource` for a folder, `MemorySource` for bytes handed over by a
  browser. They are decoded by `manguebyte-formats`; a WAV `smpl` chunk
  gives loop points and the root key, and `*sine` and `*silence` are
  generated.
- `Sampler` is a processor that takes its instrument whole from a
  `SamplerLoader` through the framework's swap pattern: notes already
  playing keep the old instrument, which goes back to the loader to be
  freed once its last note ends. Positions are fixed-point 32.32, so pitch
  is exact. Interpolation is a 32-tap windowed sinc whose kernel stretches
  to keep pitched-up samples from aliasing, or `Interpolation::Cubic`, the
  cheaper choice. Sample-accurate events, velocity layers, round robin,
  seeded random layers, release, first and legato triggers, `rt_decay`, all
  four loop modes with a precomputed crossfade, a DAHDSR envelope, choke
  groups, group polyphony and voice stealing that fades instead of
  cutting; the pitch wheel, the sustain pedal, all sound off, reset all
  controllers, all notes off, note chokes and per-note volume, pan and
  tuning. `SamplerVoice` plays an
  instrument as a `Voice` for `Synth`, with documented limits.
- It builds for `wasm32-unknown-unknown`. `examples/wasm_smoke.rs`, run in
  Node.js, plays notes with no host imports and measures their pitch
  within 0.0003 cents; `examples/sampler_bench.rs` and
  `examples/wasm_bench.rs` time it natively and in WebAssembly.
- `InstrumentBuilder` loads an instrument one sample at a time (`paths`,
  `load`, `finish`), for threads that may only work in short steps, and
  gives what `Instrument::load` gives. `Instrument::covers_key` says
  whether a key starts a region. `Sampler::release_all(shortest)` lets go
  of every note as all-notes-off would, but never faster than `shortest`
  (one-shots and short releases fade in a straight line instead), and
  `Sampler::fade_out_all(seconds)` fades every voice; both leave every key
  and pedal up. The windowed-sinc table is now made once per program and
  shared by every instrument, instead of once per instrument (a few
  milliseconds each); `prepare_tables` makes it ahead of time.

### Web

- Control calls in two halves, for an engine on a real-time thread:
  `DetachedPart`, `DetachedInstrument`, `DetachedStages` and `DetachedSfz`
  are made off the audio thread from the engine's configuration, and
  `Engine::attach_part`, `detach_part`, `swap_instrument`,
  `set_chain_from` and `give_sfz` hand them over without touching the
  heap, leaving what the engine lets go of in the slot;
  `take_finished_instrument` hands back the SFZ instruments sampler parts
  are done with. The output is bit for bit that of the plain control calls
  (`tests/handoff.rs`, with a counting allocator).
- `js/bridge.js`: `AratuBridge`, a `ManguebyteEngine` that plays through
  the Aratu Bridge plugin in a DAW, with the same methods, times in
  seconds mapped onto the DAW's position or in beats, its state, the
  DAW's transport and the plugin's counters (`js/tests/bridge.test.mjs`,
  against a stand-in socket and golden bytes shared with the Rust side).
- `manguebyte-web`, a new crate: an engine for the browser that builds,
  runs and is tested natively too. Numbered parts play an eight-voice synth
  or a General MIDI drum kit through an optional low-pass filter, an
  optional reverb, gain and pan. Events are 32-byte little-endian records
  (frame, kind, part, note, channel, velocity, parameter, value) that land
  on their exact frame; the output is identical bit for bit whatever the
  size of the quanta. Records arrive through a single-producer
  single-consumer ring with a fixed byte layout, which in a browser lives
  in a `SharedArrayBuffer`, or are copied in messages without shared
  memory. Parameters have stable text ids and are described as JSON in the
  `manguebyte-params` format; the drum map gives each drum its General
  MIDI note and the symbol Aratu writes for it. The record layout, the
  ring and the drum map are the contract with Aratu, documented in the
  crate and its README.
- Built for `wasm32-unknown-unknown`, it is a module with plain
  `extern "C"` exports (ABI version 2) and no imports. `js/worklet.js` is
  the `AudioWorkletProcessor`, `js/engine.js` the main-thread helper that
  maps `AudioContext` time to frames, and `js/ring.js` the ring; 39 tests
  run them in Node.js against the built module. `examples/web` is a demo
  page with a server that sends the cross-origin isolation headers.
- The output goes through a limiter with no look-ahead that keeps every
  sample within `OUTPUT_CEILING` (about -0.3 dB). Sample rates from 3 to
  768 kHz are accepted.
- Accents: byte 9 of a record, which was padding, is now `flags`, and its
  bit 0 accents a note on, as Aratu marks a note written in capitals; the
  other bits are reserved. An accented synth note is 3.5 dB louder, opens
  its filter an octave further and attacks twice as fast; an accented drum
  is 3 dB louder with its attack sharpened. A 0 there means no accent, so
  records from older writers play as before and the ABI version stays 2.
  `EventRecord::with_accent`, `encodeRecord({ accent })` and the sixth
  argument of `noteOn` in `js/engine.js` write it.
- Five more drums, after the first six in the drum map: high tom (50,
  `t`), mid tom (47, `m`), floor tom (43, `f`), side stick (37, `k`) and a
  guitar body tap (88, `guitar_tap`, no symbol: `null` in the JSON). The
  toms are tuned membranes that fall in pitch as they settle, with the
  stick's noise; the side stick a short woody knock; the tap the thump of
  a guitar's air and top resonances with a nail's click. Each has a level
  parameter (`high_tom.level` and so on). The `drum_wavs` example writes
  every drum to a WAV file to listen to.
- Sampler parts (`InstrumentKind::Sampler`, code 2, `'sampler'` in
  `js/engine.js`) play SFZ instruments through `manguebyte-sampler`, with
  velocity, accents (3 dB louder, `SAMPLER_ACCENT_DB`), note off, choke
  and an all-notes-off that never takes less than 8 ms
  (`PANIC_FADE_SECONDS`); samples play at their own rate at the right
  ratio, read with cubic interpolation, or a windowed sinc through the
  `sampler.interpolation` parameter. `SfzLoad` loads an instrument a file
  at a time and `Engine::load_instrument` gives it to a part, whose
  sounding notes ring on with the old one. `await engine.loadSfz(part,
  url)` fetches the SFZ file and its samples; the worklet decodes them
  between render quanta, a step of at most about one file at a time, so
  loading never holds the audio up and `mb_render` never allocates. New
  exports `mb_text_ptr`, `mb_input_ptr`, `mb_sfz_begin`,
  `mb_sfz_files_json`, `mb_sfz_add_file`, `mb_sfz_finish`,
  `mb_sfz_abandon` and `mb_collect` are added beside the old ones, which
  mean what they did, so the ABI version stays 2; `js/worklet.js` looks
  for them by name. Every instrument Aratu ships loads with no warning
  (`tests/aratu.rs`, when Aratu's samples are on the machine).
- Sampler kits (`InstrumentKind::SamplerKit`, code 3, `'sampler_kit'`):
  sampler parts marked as drum kits, which play the synthesized drum for a
  note of the drum map their SFZ text has no region for (Aratu's kit has
  no guitar tap, 88), at its velocity, with its accent and through the
  part's chain, and every drum until their samples arrive. They have the
  kit's parameters for those drums. The fallback is chosen with the kind
  of part, not guessed, so that a melodic instrument with a gap in its
  keys never plays a drum.
- Playing live. Record kind 6, cancel (`EventRecord::cancel`,
  `engine.cancel(part, time)`): every event of its part due on its frame
  or later that reached the engine before it is dropped (counted in
  `EngineStats::cancelled`, `mb_stat` 10), and on that frame the part's
  sounding notes fade out over 8 ms (`CANCEL_FADE_SECONDS`), on every kind
  of part, so an edited bar can be replaced mid-way; the code was one every
  engine ignored, so the ABI version stays 2. All notes off is shown never
  to click on any kind of part: the largest step after it is never larger
  than the largest before (a cut adds 73 to 92% of the peak).
  `Engine::latency_frames`, `mb_latency_frames` and `engine.latency()` give
  the engine's latency, one render quantum, and with the context's output
  latency the whole of it.
- Effects. A part's chain can have, beside the low-pass and the reverb, a
  high-pass, a three-band EQ, drive, a bitcrusher, a compressor, a chorus,
  a flanger, a phaser, a tremolo, an auto-pan and a delay (time or tempo
  sync, feedback, damping, ping-pong), always run in that order
  (`Effect::PART_ORDER`). Putting an effect in the chain (`Chain` bits 2 to
  12) makes its memory; its switch, `<effect>.on`, is a parameter, so it
  lands on an exact frame with its settings. An effect off costs nothing,
  switching fades over 10 ms (the delay and the reverb ring on), every
  setting glides, and nothing clicks. The signal processing is
  `manguebyte-dsp`'s filters, `Compressor`, `Chorus`, `Phaser`,
  `ShaperCurve`, `Lfo`, `DelayLine` and `Reverb`. Every parameter has an id,
  a range, a unit, a scale and a default in the parameter JSON, which now
  gives each effect's parameters `"effect"` and `"visual"` (what an editor
  draws: `filter_curve`, `eq_curve`, `transfer_curve`, `lfo_shape`,
  `delay_taps`, `reverb_decay`, `bitcrush_steps`); `Engine::effect_curve`
  and the export `mb_effect_curve` give the points, worked out from the
  parameters outside the render call. Measured in `tests/effects.rs`: echo
  times to the frame and feedback exact, chorus and flanger sweeps within
  0.02 ms of their depth, phaser notches below -40 dB where the formula
  puts them, EQ gains within 0.1 dB at their centres, compressor gain
  reduction within 0.1 dB, bitcrusher steps exact, curves within 0.1 dB of
  what is measured, the same samples in quanta of 1, 7, 128 and 512, and
  no value that is not a number stuck in any effect.
- A master bus: the parts and a shared reverb (each part has a `send`)
  through a glue compressor (off until `master.glue.on`), `master.gain` and
  `manguebyte-dsp`'s look-ahead `Limiter`, made true-peak aware by a key
  channel that reads the waveform between samples eight times oversampled:
  it keeps the output under `master.limiter.ceiling`, -0.3 dBTP by default,
  and leaves it exactly as it is below. It replaces the limiter that did
  not look ahead (`LIMITER_HOLD_SECONDS` is gone). The look-ahead, 111
  frames at 48 kHz (`master_latency_frames`), is counted in
  `latency_frames`; the engine renders its parts that far ahead of the
  output, so events are still heard on their frames, and only a late event
  lands later than before. `master.tempo` and `master.beat` drive
  tempo-synced effects. Master parameters ride in records naming any part
  and no cancel drops them; `Engine::master_params_json` and
  `mb_master_params_json` describe them. The bus is skipped after 200 ms
  of digital silence, as are the delay and the reverbs while nothing
  reaches them.
- `js/engine.js`: `setEffect(part, 'delay', [0.25, 0.5], time)` (settings
  by name, by position or as one number, which is how a method chain such
  as `.delay(0.25, 0.5)` maps onto it), `setEffects(part, { delay: [0.25],
  chorus: [] }, time)` for a whole chain, `removeEffect`, `setSend`,
  `setMaster`, `effectCurve`, `effectRunning`, `masterParams`,
  `partChain`, `limiterReduction`, and the lists `EFFECT`, `CHAIN`,
  `EFFECT_ORDER`, `EFFECT_PARAMS`, `VISUAL` and `SHAPES`. Everything is
  added beside what was there, so the ABI version stays 2.
- The instruments of `manguebyte_dsp::instruments` in parts, instrument
  codes 4 to 9: `plucked` (nylon and steel guitar, finger and slap bass,
  eight voices), `fm` (electric piano, tubular bell, FM bass, eight),
  `organ` (drawbars, percussion, click, leakage, sixteen voices on one
  manual, through a rotating speaker that is part of the instrument, its
  speed a parameter, `organ.rotary`), `analog_kit` (the 808-style machine
  on the drum map, its voices sharing a choke group; the ride and the
  guitar tap, which it does not have, are the synthesized kit's),
  `kick_bass` and `acid` (one voice with legato, so a note on before the
  last note's note off slides). Each has a preset where it has several
  (`<kind>.preset`, which moves the knobs it covers on its frame) and its
  main knobs, with units, ranges, scales and defaults in the parameter
  JSON; the crate's README lists them all. Accents reach every voice as an
  argument of the note (`Synth::note_on_accented`), the synth's too.
  `Engine::set_instrument`, `mb_set_instrument` and `setInstrument` give a
  part another kind and keep its chain; an instrument's memory is made
  there or by `create_part`, never while rendering. A cancel fades the
  notes sounding over 8 ms while the notes after it play in full, as on
  every kind (each of these parts keeps two synths of its voices for it),
  and all notes off fades the organ and the drum machine. `setPreset`,
  `setRotary`, `PRESETS` and `ROTARY` in `js/engine.js`. Measured in
  `tests/voices.rs`: every kind within a cent of equal temperament (0.7
  cents at worst), velocity and accents louder and brighter, slides that
  glide, the hats choked and the fallback drums identical to the drum
  kit's, the same samples in quanta of 1, 7, 128 and 512, finite whatever
  the knobs. The ABI version stays 2: codes, parameters and the export are
  added. The `aratu_song` example renders eight bars of all of them
  through the engine to WAV files.
- The curves of the effects and the master bus are `manguebyte-dsp`'s
  response curves (`SvfResponse`, `BiquadResponse`, `CompressorTransfer`,
  `LimiterTransfer`, `ChorusSweep`, `PhaserSweep`, `LfoCycle`,
  `DelayTaps`, `ReverbDecay`), from the settings the parameters hold; the
  axes are as they were, and the reverb's tail is drawn at 1 kHz with its
  damping.
- Every kind of instrument and every preset plays at one loudness: its
  standard phrase (`Phrase`: a two-bar melody, a bass line, two chords or
  a groove, at velocity 0.8) comes out of a part at -24 LUFS
  (`LOUDNESS_TARGET_LUFS`, ITU-R BS.1770-4, measured with
  `manguebyte-analysis`), within 1 LU, by a make-up gain per kind and
  preset (`InstrumentKind::calibration`, `preset_param`). Before, they
  spread over 18.9 LU, the steel guitar 9 LU under the target and the
  kick bass 10 over. The make-ups are pure gains, so velocity and accents
  keep their dynamics, and -24 is the loudest target at which no single
  note of any kind, accented at full velocity, reaches the limiter (the
  drum machine's snare peaks at -0.6 dBTP). Levels change: the drums play
  4.8 dB lower, the kick bass 9.9, the steel guitar 9 dB higher. A kind or
  a preset without its row does not compile, and `tests/loudness.rs`
  fails for a row off the target, saying what its make-up should be.
- Sampler parts measure each instrument as it loads, playing their kind's
  phrase offline through the sampler's voices (the melody in the
  instrument's range; for a sampler kit the groove on its own drums), and
  play it at the make-up that brings it to the target
  (`Engine::sampler_make_up_db`), held back where its peaks would pass
  -1 dBFS at full velocity, accented (`NORMALIZE_PEAK_DB`), within 24 dB
  (`NORMALIZE_RANGE_DB`): two instruments recorded 12 dB apart land on
  -24.00 LUFS both. `sampler.normalize` (on by default) switches it, the
  level gliding over 20 ms while notes sound;
  `Engine::load_instrument_unmeasured` loads an instrument as it is. The
  measurement is a control call's: `Engine::load_instrument` makes it at
  once, and `js/worklet.js` between render quanta through the new export
  `mb_sfz_measure`, 8 192 frames a quantum (`PROBE_STEP_FRAMES`), 28
  quanta for a load. `manguebyte-analysis` is now a dependency of
  `manguebyte-web`. The ABI version stays 2.
- A sampler or sampler kit part plays its instrument as soon as the
  samples are decoded, before its loudness is measured: Aratu's first
  preview of a freshly loaded kit was silent, the notes landing before the
  instrument. Until the measurement ends, some 75 ms later in a worklet,
  the part plays at a provisional make-up estimated from the energy the
  phrase's notes read from the samples (within 0.22 dB of the measured
  one on every fixture), then glides to the make-up measured over 20 ms.
  A clone of the instrument is measured, so the part's round robins never
  hear it and its output does not depend on the measurement's steps.
  `loadSfz` resolves once the part plays the instrument, with the same
  summary, and its promise has `normalized`, which resolves with
  `{ makeUp }` when the measurement is done; natively
  `Engine::install_instrument`, `measure_instrument`,
  `sampler_measuring` and `sampler_provisional_make_up_db`; the new exports
  `mb_sfz_install`, `mb_sampler_measure` and `mb_sampler_make_up_db`, and
  status -102 in JavaScript. The ABI version stays 2.
- `manguebyte-sampler`: an `Instrument` is `Clone`, cheaply, its clones
  sharing the decoded samples and regions and keeping round robins and
  random numbers of their own; `Instrument::note_energy` estimates the
  energy of what a note plays, without rendering it.
- `manguebyte-web` plays the newer voices of `manguebyte-dsp`, as six new
  instrument kinds, codes 10 to 15: `drum_kit` (`DrumKitVoice`: the
  acoustic kit by modal synthesis, the 808, 909, 606 and CR-78, and kits
  for trap, techno, house, drum and bass and lo-fi, `drum_kit.preset`),
  `poly` (the polyphonic synthesiser and its 68 presets, `poly.preset`,
  with macros on the cutoff, resonance, attack, release, unison detune and
  spread, the modulation wheel and `poly.voices`), `mallet`, `bowed`,
  `wind` and `choir`, each with its presets and main knobs. A drum kit
  answers the drum map, and a drum of the map its kit has no piece for
  (the guitar tap on every kit, the 606's side stick, the CR-78's toms) is
  the synthesized drum, to the bit; Aratu's clap and shaker keys play the
  kit's clap and maracas where it has them. The newer voices render in
  stereo. The synthesiser's monophonic and legato presets slide, and so
  do the winds (`wind.legato`, on by default) and the strings and the
  choir with `bowed.legato` and `choir.legato`. Every kind and every
  preset, 102 rows more, plays its phrase at -24 LUFS (all within 0.07
  LU); the drum kits, the violin's pizzicato and Plucked String play
  narrower dynamics, so that no accented note at full velocity reaches the
  master limiter (the acoustic kit's kick peaks at -0.7 dBTP;
  `drum_kit.velocity` and `drum_kit.accent` widen a kit's again). Voices
  are capped per part from what they cost in WebAssembly, measured by the
  new `examples/voice_cost.mjs` on a 15 W laptop: 12 for the kits and the
  mallets, 6 for the strings and the choir, 4 for the winds, and for the
  synthesiser's polyphonic presets 4 to 8 by preset, as many as keep a
  part within 40% of a core there (a voice costs 3 to 16%). Rendering
  stays allocation free and the same in every quantum. `js/engine.js`
  names the kinds and their presets (`INSTRUMENT`, `PRESETS`);
  `examples/new_voices_songs.rs` renders a song for each family. The ABI
  version stays 2.
- `manguebyte-web` keeps a scope and levels for every part, for Aratu's
  editor to draw: its latest 4 096 frames (`SCOPE_FRAMES`) of what it puts
  into the master bus, after its effects, gain and pan, and its peak (rising
  at once, falling 20 dB in 1.7 s, `PEAK_RETURN_SECONDS`) and RMS (0.3 s,
  `RMS_SECONDS`) levels per channel. The render call writes them into a
  ring per part, a frame at a time, never allocating, and they are the same
  bits whatever the quanta. `Engine::part_scope`, `part_scope_into`,
  `part_scope_end` and `part_levels` read them (`PartScope`, `PartLevels`),
  as do the new exports `mb_scope_frames`, `mb_part_scope`,
  `mb_scope_ptr`, `mb_part_scope_end`, `mb_part_levels` and
  `mb_levels_ptr`, which never allocate. In a cross-origin isolated page
  the worklet copies them after every quantum into a `SharedArrayBuffer`
  of one sequence-locked slot per part (`js/scope.js`, 33 KiB a part),
  which `engine.partScope(part, frames)` and `engine.partLevels(part)` read
  without a message; without shared memory, or with `scopes: false`, they
  ask the worklet. A part made again keeps its scope. Status -103 says a
  module has no scopes. The ABI version stays 2.
- `manguebyte-web`'s drive has a fifth curve, `drive.shape` 4, the tube
  (`'tube'` in `SHAPES`, `tube_curve`): `¾·(tanh(1.5x + ½ ln 2) − ⅓)`, a
  hyperbolic tangent biased a third of the way up, in the manner of a
  triode, so that the positive half flattens sooner than the negative and
  a wave gains even harmonics (18 dB below a kilohertz at the defaults,
  where the odd curves give none); a first-order 5 Hz high-pass
  (`TUBE_DC_HZ`) after it takes out the offset that leaves. Its slope at
  rest is 1, and a phrase plays within 0.6 LU of the tanh's loudness.
- Aratu's Brazilian percussion keys sound on every `drum_kit` preset:
  the agogô, ganzá, cuíca, triangle, surdo, pandeiro, zabumba, tamborim,
  alfaia, gonguê and cajón of `DrumKit::brazilian`, and its hand clap on
  the kits without one (the 606 and the CR-78), on the keys of the
  crate's README. They are the same instruments at the same level on
  every kit: each kit's make-up is taken off them and their velocity
  made that of a 24 dB range at 0.8, so that a line of eighth notes of
  each sits 6 to 17 LU under the target (`tests/percussion.rs`, every
  key on every kit), and none, accented at full velocity, reaches the
  master limiter (`tests/loudness.rs` now strikes them too).

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
- `manguebyte-host::lv2`: hosting LV2 plugins. `read_bundle` describes a
  bundle's plugins from its `manifest.ttl` and the files its plugins point
  at, with the host's own Turtle reader (prefixes and bases in both
  spellings, relative IRIs resolved by RFC 3986, blank nodes, collections,
  literals with their escapes), which runs no code and takes a bundle as
  hostile input: limits on document length, nesting, triples, IRI length
  and distinct text, and an error with its line. A library outside its
  bundle is refused. `Lv2Library` loads the libraries, `Lv2Plugin` creates
  an instance with `urid:map` and `unmap`, `options:options` (block sizes,
  sequence size, sample rate), `buf-size:boundedBlockLength` and `log:log`
  (posted to a wait-free queue; a lone `%s` is shown as its string), and a
  plugin that requires any other feature is refused by name. Control
  inputs are its parameters, in their own unit and range; `Lv2Processor`
  runs a block in pieces at every parameter change, so each lands on its
  frame, and writes MIDI 1.0 and a `time:Position` into the event input as
  an atom sequence. State is the plugin's `state:interface` values and
  every control value, kept by URI and port symbol. LV2 fixes the sample
  rate at instantiation, so activating at another rate creates the instance
  again and carries its state over. The `any` interface speaks for LV2
  too (`Format::Lv2`). Both example plugins, hosted from bundles their own
  `export_lv2!` describes, sound the same, bit for bit, as with no host at
  every block pattern tried, with no heap use in the host.
- `manguebyte-host::scan`: finding installed plugins. `standard_paths`
  gives each format's folders on each system (with `CLAP_PATH` and
  `LV2_PATH`), `find` finds candidates by name, and a `Scanner` lists each
  CLAP and VST 3 file in a scanner process of its own, reading its
  descriptions without creating a plugin, waits a set time and kills it
  after; LV2 bundles are read in the host from their Turtle files. The
  report gives every plugin and every failure (crashed, with the exit code
  and the end of its error output; timed out; refused, with the reason). A
  `ScanCache`, saved as text, keeps what was found by path and fingerprint
  (newest write, size, file count), so only new and changed files are
  scanned again. The scanner process is any program that calls
  `serve_if_asked` first, or the crate's `manguebyte-scan` binary. Tested
  with the example plugins in all three formats and fake plugins built by
  the test that abort, hang and refuse in `init`.

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

- `dsp::note_input::NoteInput`: one reader of MIDI for the framework.
  Plain MIDI 1.0 is read exactly as `Event::from_midi` reads it, plus the
  high-resolution velocity prefix (controller 88). In an MPE zone, set up
  by the configuration message in the stream or by `Mpe::Zones`, a note
  gets an identifier and is reported on the zone's master channel, and
  its member channel's pitch bend (under the member bend range, 48
  semitones until registered parameter 0 says otherwise), channel
  pressure and controller 74 become `Tuning`, `Pressure` and `Brightness`.
  MIDI 2.0 notes get identifiers and their 16-bit velocities; per-note
  pitch bend (range from registered controller 0/7, 48 by default), poly
  pressure, the pitch attribute and the registered per-note controllers
  for pitch, volume and expression, pan, brightness and vibrato depth
  become expressions, and per-note management detaches and resets them.
  Channel-wide messages stay channel-wide MIDI 1.0. MIDI 1.0 values are
  widened with the specification's min-centre-max method first, so an MPE
  stream and its MIDI 2.0 equivalent give identical events. Identifiers
  made here have their top bit set and never equal a host's. Bounded,
  and real-time safe after construction.
- `Plugin::MPE` (`Mpe::Follow` by default, `Mpe::LOWER_ZONE`, `Mpe::Off`):
  how the wrappers, the standalone runner and the harness read a plugin's
  MIDI as MPE. A stream that never configures a zone reads as before.
- `Harness::schedule_midi` and `Harness::schedule_ump` deliver MIDI 1.0
  messages and Universal MIDI Packets through the plugin's `NoteInput`,
  as a wrapper does.
- `core::source`: `AudioSource`, pulled a block at a time,
  `PositionableSource`, and `BufferSource`, audio in memory played once
  or looped, positionable to the frame.
- `dsp::sources`: `MixerSource` (inputs with gains), `ResamplingSource`
  (the band-limited `Resampler`, pulled in fixed chunks so the output does
  not depend on the size of the pulls, lined up with the input),
  `ChannelRemapSource` (one channel to many, or to none),
  `ProcessedSource` (a source through a processor) and `SourceProcessor`
  (a source as a processor, for a graph node or a device callback).
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
- `Synth`: a free voice is chosen by its position, not as the oldest, and
  `reset` also clears the clock of voice ages (it keeps the pitch wheel and
  the controllers, as before), so playing the same material again, with or
  without a reset, gives the same output bit for bit. `note_on` and
  `note_off` hold the velocity to 0 to 1 and take a velocity that is not
  finite as 0.
- `Synth::note_on_accented` and `Voice::note_on_accented`: an accent,
  a second dimension of how hard a note is played (a drum machine's or a
  step sequencer's, or a note Aratu writes in capitals), passed as an
  argument of the note so that it can never be left over for the next.
  The event stream has no accent; a voice that does not implement the
  method hears a plain `note_on`.
- `Synth::set_legato` and `Voice::legato`: with legato on, a note that
  starts while a key is held on its channel takes over the newest held
  note's voice and slurs from it, and releasing the replaced note's key
  changes nothing. A voice that does not implement `legato` is struck
  again in place. Both additions have defaults, so every existing voice
  works unchanged.
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
  A sample that is not finite comes out as silence and leaves the gain as
  it was.
- `StateVariableFilter::phase_response`, the filter's exact phase at a
  frequency in every mode, and `StateVariableFilter::coefficients`.
- `Waveshaper` (hard clip, tanh, soft clip, fold), `DcBlocker`, `Noise`
  (white, pink, brown), `Lfo` with tempo sync, and `fir` design functions.
- `curve`: the curves an interface draws, computed from the settings the
  processor runs on rather than imitated. Each is a small `Copy` value,
  taken from the processor (`Filter::response`, `Compressor::transfer`,
  `Adsr::shape`, `Chorus::sweep`, `Delay::taps`, `Reverb::decay` and so
  on) or made from parameter values on any thread with the same limits;
  nothing allocates. Frequency responses (`FrequencyResponse`: complex,
  magnitude in dB, phase) of `Biquad` and `Filter` (with their
  coefficients rounded as they are held), `BiquadCascade`, `IirFilter` and
  `iir::Design` (from the form the cascade runs, exact where the
  coefficients are not), `StateVariableFilter`, `OnePole`, the linear model
  of `LadderFilter`, every band of a `Crossover` and their sum, and
  `FirFilter`; a slice, array or tuple of them is a chain whose decibels
  add up. Static transfer curves of `Compressor` (knee and makeup),
  `Expander`, `NoiseGate`, `Limiter` and `Waveshaper`; the corners of an
  `Adsr`; one cycle of an `Lfo`; the delay of each `Chorus` voice and the
  frequency of the `Phaser` stages over a cycle; the time and gain of each
  echo of a `Delay`; and the RT60 of a `Reverb` against frequency, from an
  energy model of its network. `CurveSpec` says what a curve is and how to
  draw its axes (unit, linear or logarithmic, a fitted range), and
  `log_spaced`, `Axis::fill` and `unwrap_phase` make the grids. Every
  curve was checked against the processor itself: frequency responses
  against impulse responses up to 0.45 of the sample rate (the largest
  difference measured was 4e-9 dB and 2e-11 rad, the tests allow 1e-7)
  and steady sines (1e-12 dB), transfer curves against settled steady
  levels (within 1e-9 dB), envelopes, sweeps and echoes sample by sample,
  and the reverb's RT60 against impulse responses rendered offline (see
  `ReverbDecay` for how close). The `response_curves` example writes them
  all as SVG and CSV.

### Instruments

- `Synth::set_voice_limit` lets new notes take only the
  first voices of a synth, real-time safe, so that a synth made with as
  many voices as it may need can play fewer where each costs more. A
  `PolySynthVoice` renders the last of a note out of its decimator before
  it is done, so the end of a note no longer depends on where a block
  starts.
- `manguebyte_dsp::instruments`: voices for `Synth` that sound like the
  instruments they are named after, built from the crate's own delay
  lines, filters and oscillators. Each has a settings struct with presets
  whose values are limited so that no setting can make it put out
  anything but finite numbers, answers velocity with level and timbre,
  takes accents, hands a sounding note it is taken from to a 4 ms fade
  instead of cutting it, never touches the heap while it plays (in the
  counting-allocator test) and gives the same samples whatever the block
  size. `cargo run -p manguebyte-dsp --example instrument_wavs --
  <directory>` renders a phrase on every preset to WAV files.
- `PluckedString`: Karplus-Strong with the Jaffe-Smith extensions. The
  loop's delay at the fundamental, its filters included, is solved exactly
  and an all-pass tuned at the fundamental supplies the last fraction of a
  sample: the worst error measured from 33 Hz to 2.6 kHz is 0.003 cents. A
  loss filter designed per note from the decay of the fundamental and of
  3 kHz (the decays measured are the ones set), the string's shape as it
  is let go (a triangle for a pull, a step for a strike) with the pluck
  position, pick noise, a velocity low-pass, stiffness all-passes, a
  pickup comb, body resonances and a fret barrier for slaps. Presets:
  nylon and steel guitar, finger and slap bass, whose accent is a pop.
- `FmVoice`: four operators in six algorithms, envelopes even in decibels
  that run faster up the keyboard, keyboard level scaling, velocity per
  operator, feedback, and an accent on carriers and indices. Each note
  limits its indices so that its sidebands stay under 0.45 of the sample
  rate: a held stack of full indices with feedback at C5 has -13.5 dB of
  inharmonic energy without the limit and -92 dB with it. Presets:
  electric piano (tine and tone bar), tubular bell, FM bass.
- `OrganVoice`: nine drawbars on tempered tonewheels folded back at the
  generator's ends, key click from contacts that close at different
  moments, percussion (second or third, fast or slow, soft or normal)
  single-triggered across voices through an `OrganManual`, leakage, and
  drawbars that glide. Presets jazz, gospel, full and ballad, and any
  registration written as digits. `RotarySpeaker`, a processor: a horn and
  a drum behind a Linkwitz-Riley crossover, each accelerating at its own
  pace, with Doppler, tremolo and the horn dulling as it turns away, heard
  by two microphones.
- `AnalogKitVoice`: sixteen drums in the manner of the 808 on the General
  MIDI keys, modelled on the published descriptions of its circuits
  (bridged-T resonators, filtered noise, the bank of six square
  oscillators), each with level, tune, decay, tone and snap; the closed
  hi-hat chokes the open one through a `ChokeGroup`. `KickBassVoice`: the
  long kick played as a bass, with an antiderivative-anti-aliased
  saturator and slides on legato.
- `AcidVoice`: the 303-style bass: a diode ladder (each capacitor loaded
  by the next) solved per sample at twice the rate, a resonance whose
  height at the cut-off is set in decibels, the accent's short decay and
  sweep that builds over consecutive accents, and a constant-time slide
  on legato that does not strike the envelopes again.
- `DrumKitVoice`: a drum kit whose piece on each General MIDI key is
  chosen and tuned in a `DrumKit` (`Piece`: model, level, tune, decay,
  three timbre settings, pan, choke group; `LoFi`: a sampler's saturation,
  rate, word length and filter on every piece). Acoustic pieces by modal
  synthesis: drum heads in the modes of a circular membrane (the zeros of
  the Bessel functions, checked against `J_m`), struck by a Hertzian
  pulse at a radius that weights the modes, each batter mode coupled to
  the resonant head's through the air into a close pair, the pitch sharp
  after a hard strike, and snare wires as noise gated by the snare head's
  motion; cymbals as 280 to 460 resonators with a plate's modal density,
  a measured decay curve and a cascade that carries energy up the bands
  after the strike (the shimmer). The snare and the crash are calibrated
  against recordings of real ones, and tests check the recorded targets:
  the crash's decay by octave (within 0.6 to 1.5 times), the high bands
  peaking 0.1 to 0.4 s after the strike, its centroid every 100 ms
  (within 25 %) and its partial clusters; the snare's body and wire
  decays, the wires peaking 40 ms after the body, its centroid and its
  wire-to-body balance. Drum machines as recipes of their circuits' parts:
  the 909, 606 and CR-78 (with its guiro, maracas, tambourine, cowbell,
  bongos and conga), and the 808 of `AnalogKitVoice`. Presets: acoustic,
  808, 909, 606, CR-78, trap, techno, house, drum and bass, lo-fi.
  Restruck while it rings, an acoustic piece adds the new strike to its
  ringing. A ringing cymbal costs about 3.5 % of one core at 48 kHz, a
  snare 1.3 %. `cargo run --release -p manguebyte-dsp --example
  drum_kit_wavs -- <directory>` renders a tour of every kit and grooves.
- `PolySynthVoice` (`instruments::poly`): a deep polyphonic synthesiser
  and a library of 68 presets. Three oscillators (sine, triangle, saw, a
  pulse whose width is modulated, and twelve wavetables built from
  formulas, each of sixteen frames to morph through) of up to sixteen
  unison voices, detuned more densely in the middle as the classic
  supersaw is and spread with pitch neighbours on opposite sides, each at
  a random phase if asked; hard sync to oscillator 1, phase and ring
  modulation by any oscillator, unison voice by unison voice; a sub
  oscillator; white, pink and brown noise. Two filters in series or in
  parallel, each one of fifteen models: the transistor ladder (24 and
  12 dB low-pass, high-pass, band-pass), the linear state-variable filter,
  an SEM-like one morphing low-pass, notch or band-pass, high-pass, an
  MS-20-like Sallen-Key low- and high-pass with a saturating loop, combs
  with positive and negative feedback tuned for their damping, and a
  formant filter on Peterson and Barney's vowels; key tracking, envelope,
  drive and morph. Four envelopes with a curve per segment (the
  amplifier's and the filters' wired), three LFOs with six shapes, tempo
  sync, retrigger and fade-in, and a sixteen-slot matrix of source, `via`
  source and amount over 27 destinations, with velocity, accent,
  aftertouch, the wheel, the key, a random value per note and brightness
  as sources. Glide; poly, mono and legato modes (`PolySynthVoice::synth`
  sets a `Synth` up for each). Everything runs at twice the sample rate
  and comes down through a 100 dB half-band filter (20.5 samples late):
  the classic shapes are band-limited by four-point polyBLEP and polyBLAMP
  residuals that also correct sync and phase-modulation jumps, the
  wavetables are mip-mapped an octave a level. The tests measure tuning
  (within 0.012 cent from key 21 to 108), aliasing below 20 kHz from key
  72 to 103 (at most -82 dB of a saw or square, -109 dB of a triangle,
  -87 dB of the wavetables), hard sync (-97 dB between the master's
  harmonics), the phase-modulation sidebands against the Bessel functions
  (within 0.1 dB), ring modulation (the carriers 80 dB down), unison
  level, density and decorrelation, every filter's quiet response against
  `curve` (within 1e-5), key tracking, envelope and matrix cutoff against
  `LadderResponse`, envelope and LFO timing, every matrix destination and
  source, glide time and the voice modes, clicks at note on, off, steal
  and setting jumps, block-size independence and finite output under
  garbage. One held voice costs 3 % to 29 % of a core of a 15 W laptop
  processor at 48 kHz, 2 % to 12 % for the presets. The presets are
  constants (`PRESETS`, `preset`), documented in a table: basses (Moog,
  reese, wobble, sub, FM growl, house organ, acid, garage, 808 glide and
  more), leads (supersaw, square, sync, chiptune, hoover, talking),
  pads (warm, string machine, glass, drone, choir, formant, PWM, comb
  wash), plucks (house, bell, marimba, a Karplus-Strong string,
  kalimba), keys (synth piano, clavinet, FM piano, organ, vibraphone),
  brass (synth brass, stabs, a rave chord stab) and effects (riser,
  downlifter, sweeps, zap, wind, impact). `cargo run --release -p
  manguebyte-dsp --example poly_synth_presets -- <directory>` plays each
  in a phrase of its category to WAV files.
- `MalletVoice`: struck bars, tines, plates and tubes by modal synthesis
  (Fletcher and Rossing; Rossing): up to ten modes in a bank of complex
  resonators, each with its ratio, level and decay, weighted by the mode
  shapes of a free bar or a clamped tine where the mallet lands, struck
  by a smooth contact whose length is the mallet's hardness (velocity
  makes it harder, so louder and brighter), a resonator tube tuned to the
  note that blooms after the strike, the vibraphone's motor opening and
  closing the tubes and its damper on release, a steel pan's notes
  driving their octave harder the harder they are struck, a knock, and
  the box of a kalimba. Presets: marimba (1:4:10), xylophone (1:3:6),
  vibraphone, glockenspiel, celesta, kalimba, steel pan, tubular bells.
  Measured: the worst mode within 0.0074 % of its ratio, decays as set
  (a marimba's 1.60 s, tubular bells' 7.72 s), 13 to 16 dB and a higher
  centroid from soft to loud, an accent 3 dB louder and brighter, the
  tube's bloom 8.1 dB after the strike against 8.3 dB designed, the motor
  at 5.42 Hz for 5.5 Hz set, the damper's decay the release. A ringing
  voice costs 0.1 to 0.5 % of a core at 48 kHz.
- `BowedString`: a bowed string by digital waveguide (McIntyre,
  Schumacher and Woodhouse; Smith): two delay lines either side of the
  bow, the friction of rosin as a hyperbolic curve solved exactly each
  sample with its hysteresis, the bow's force, speed and position
  (Schelleng's window), losses and dispersion from the string and the
  finger, a player's ear that corrects the half sample by which friction
  flattens the note, a body of eight named modes (for the violin A0, the
  centre bout, B1-, B1+, C4, a mode near 1 kHz and the bridge hill) and
  24 narrow, unevenly spread modes of the wood above them (`wood`),
  radiated only above the air resonance, vibrato that waits and then
  grows, quickening as it does, legato as a change of finger (the string
  at its new length within 2 ms, with the finger's knock; `portamento`
  slides instead) that keeps the string or moves to the next, the bow's
  scratch as it bites (`scratch`), each player's drift of pitch, the
  bow's waver and the swell of a held note (`drift`, `fluctuation`,
  `swell`), sections of up to four players tuned apart, and pizzicato.
  `BODY_MODES` is 8 and `WOOD_MODES` 24. Presets: violin, viola, cello,
  double bass, string section, and violin, cello and bass pizzicato.
  Measured: Helmholtz motion (one slip a period) at every bow force
  across the window, 112 of 117 notes within 2 cents and their drift
  averaged over two seconds (the worst 3.9), the level of a held note
  steady to 1.1 dB without the player's swell; a slurred string at its
  new pitch within 3 to 14 ms, a portamento passing through 16 periods
  between; the violin's body peaking within 3% of each named mode and
  17 times between 800 Hz and 7 kHz (none without the wood); an accented
  attack 18 to 25 dB under the tone below half the air resonance. A
  voice costs about 1.8 % of a core, a section of four 4.2 %.
- `WindVoice`: wind instruments by digital waveguide: a reed or the lips
  as a mass and spring with Bernoulli flow (Guillemain, Kergomard and
  Voinier), or an air jet with its delay and breath noise (Fletcher),
  driving a bore with a bell and losses, brass that brightens as it is
  blown harder (Hirschberg et al.), breath from velocity, pressure, the
  breath controller and the expression pedal, vibrato and tremolo that
  wait, and a player's ear. The flute is a jet-drive model (Fletcher;
  Verge, Fabre and Hirschberg): shaped turbulence that grows with the
  blowing pressure disturbs the jet (as in Cook's flute) and is heard
  from the embouchure, vortex shedding holds a soft note's swing down,
  a harder breath drives the jet further over its threshold and further
  off the edge, and a tongue's release bursts the breath (the chiff).
  Every note is tongued (`articulation`; the lips start below a brass
  note), the player drifts, wavers and swells (`drift`, `fluctuation`,
  `swell`), and legato changes the bore's length at once, as a key or a
  valve does, with a short break in the air; `portamento` glides
  instead, as a trombone's slide can. The bell radiates the incident
  wave less its own reflection, through a high-pass for brass and the
  saxophone, so nothing comes out below the note as a thump. Presets:
  flute, clarinet (a cylinder, odd harmonics), saxophone, trumpet,
  trombone. Measured: each instrument's mean tuning error 1.0 to 2.1
  cents over two seconds of a note, its drift on; a held note steady to
  0.4 dB and 1.6 cents without the player's swell, waver and drift; a
  slur at its new pitch within 3 to 6 ms (up to 40 ms for the lips),
  no click, high-frequency burst or thump at a slur, the trombone's
  slide passing through 25 periods only with portamento; the flute's
  breath 31 dB under its tone at mezzo-forte (29 soft, 34 loud) and 15
  dB more of the sound in its attack, its tone within 6 dB of its level
  70 ms in, its second harmonic 7 dB under the fundamental at
  mezzo-forte (13 soft, 4 loud) and its harmonics falling 27 dB an
  octave. A voice costs about 1.2 to 1.6 % of a core.
- `ChoirVoice`: sung vowels by formant synthesis (Fant's source and
  filter): a glottal pulse after Klatt and Klatt with a band-limited
  closure and breath noise, darker and more open the softer it is sung,
  through five formants in cascade (Klatt's resonators) with the vowels
  a, e, i, o and u from Peterson and Barney's averages for men and
  women, blended from basses to sopranos across the keyboard, the first
  formant raised above a high note (Sundberg); a choir of up to six
  singers to a note, tuned apart and starting a few milliseconds apart,
  each with a vocal tract, a vibrato and a wobble of their own. The
  brightness expression moves the vowel. Presets: "aah" choir, "ooh"
  choir, solo voice. Measured: every formant within 5 % of its target,
  tuning within 0.0001 cents, vibrato at 5.61 Hz and 39.6 cents for 5.6
  and 40 set, a choir's level swaying 5.8 dB where a single singer's is
  steady. Six singers cost about 4.5 % of a core.
- `cargo run --release -p manguebyte-dsp --example physical_model_wavs --
  <directory>` renders every preset of the four in a phrase of its own
  idiom, normalised to -3 dBFS, and prints what a voice of each costs.
- Brazilian percussion in `DrumKitVoice`, by physical modelling:
  `DrumKit::brazilian()` puts 25 strokes on Aratu's keys
  (`instruments::percussion`, 39 and 67 to 103; `KIT_KEYS` now reaches
  103), and `DrumKit::with_brazilian_percussion` adds them to any kit on
  the keys it leaves free. The agogô's two bells and the gonguê ring the
  ring-like bending modes of an iron shell (1 : 2.83 : 5.42 : 8.77, after
  Fletcher and Rossing, section 2.20) in doublets split by the seam, with
  a second family above; the triangle a steel rod's free-bar modes (1 :
  2.76 : 5.40 : 8.93, section 2.17) in two families, radiating its high
  ones; the cuíca the axisymmetric modes of a membrane (1 : 2.30 : 3.60 :
  4.90) driven at the centre by a damp cloth's stick-slip, its pitch
  rising into the squeak and falling through the groan; the surdo, the
  zabumba (mallet and the thin stick on its bottom head), the alfaia, the
  tamborim and the pandeiro are drums with heads, their muted strokes a
  hand or the mallet on the head, the surdo's open stroke falling in
  pitch as it settles; the pandeiro's jingles are small free plates (1 :
  1.73 : 2.33 : 3.91) rattling as stochastic collisions, and the ganzá's
  beads collide in a tube of two resonances (after P. R. Cook's PhISM,
  1997); the cajón is a rectangular plate and the box's Helmholtz
  resonance near 80 Hz, its slap rattling the loose top edge; the hand
  clap is three clappers' bursts of the air between their palms (after
  B. H. Repp, 1987). Muted strokes choke open ones (choke groups 2 to
  7). Tests measure each piece's fundamental and mode structure in its
  spectrum, its level, that it falls silent at 3 to 96 kHz, that the
  output does not depend on the block size, and that it does not touch
  the heap. `cargo run --release -p manguebyte-dsp --example
  brazilian_percussion_wavs -- <directory>` renders a tour of every
  stroke and grooves of maracatu, samba, baião and coco.

### Parameters

- `manguebyte-params`: `FloatParam`, `IntParam`, `BoolParam`, `ChoiceParam`;
  linear, skewed, symmetric and logarithmic ranges; text conversion;
  `ParamSmoother`; `ParamSet` with saved state that tolerates version
  changes.
- Parameter metadata as plain data: `Param::describe` gives a
  `ParamDescription` (id, `param_id`, name, unit, min, max, default, a
  `ParamScale`, an optional smoothing and the flags), and
  `ParamSet::descriptions` and `descriptions_json` give every parameter's,
  in declaration order. A `ParamScale` is linear, logarithmic, skewed or
  stepped (with the step, the names of a choice's options and, for a
  stepped float on a curve or one whose last step falls short of the end
  of the range, its `Curve` and `end`); each documents its formula from
  value to position, so an editor in another language draws the same
  curve. The JSON is written by hand, one object per parameter with the
  keys `id`, `param_id`, `name`, `unit`, `min`, `max`, `default`, `scale`,
  `smoothing_ms` (only when the parameter declares a smoothing) and
  `flags`, escaped as RFC 8259 asks and with U+2028 and U+2029 escaped too;
  a float parameter's numbers are the shortest decimals that read back as
  the same `f32`.
- `Param::display_plain` and `Param::parse_display` turn any value into
  text with its unit and back; `ParamSet::value_to_text` and
  `text_to_value` do it by id. A choice's or a switch's names are matched
  exactly first, then trimmed, then ignoring ASCII case.
- A smoothing hint on every parameter type (`with_smoothing`, `smoothing`,
  and `smoother` on `FloatParam`), reported as `smoothing_ms`; nothing
  else about the parameter changes.

### MIDI

- `translate::Midi2ToMpe`: MIDI 2.0 per-note messages written out as MPE,
  the alternate translation for what the default one drops (M2-104-UM,
  D.2.8). Each note gets a member channel from the MPE allocator, its
  per-note pitch bend, pitch attribute and per-note controller 3, its
  polyphonic pressure and its per-note controller 74 become the
  channel's pitch bend, pressure and controller 74, sent before the
  note-on (RP-053, 3.2 and 3.3), and channel messages go to the master
  channel. The configuration message goes first, per-note management
  detaches and resets, and a wider per-note bend range is scaled to the
  members' 48 semitones.
- `ci`: MIDI Capability Inquiry, written from MIDI-CI 1.2 (M2-101-UM) and
  the profile and property exchange common rules. `parse` and `encode`
  read and write every message: discovery and its reply, endpoint
  information, invalidate MUID, ACK and NAK, profile inquiry, set on and
  off, the enabled, disabled, added and removed reports, profile details
  and profile data, property exchange capabilities and the chunked get,
  set, subscribe and notify messages, process inquiry with the MIDI
  message report, and the protocol negotiation that 1.2 deprecates.
  Reading borrows from the bytes and never panics or allocates; older
  messages read with their version's defaults and newer bytes are
  ignored. `Muid::from_seed` makes a MUID a device may take.
- `ci::property`: property data cut into chunks that fit a receiver's
  announced message size, an `Assembler` that puts chunks back together
  with bounds on memory and on messages under way and tells a chunk out
  of sequence, and Mcoded7 for binary data.
- `ci::agent::Agent`: a responder and initiator in one, with no I/O. It
  answers discovery and endpoint inquiries, lists and turns its profiles
  on and off on request and reports them, reports property requests for
  the application to answer with `reply_property`, chunks requests and
  replies for the receiver, times unanswered requests out with `tick`,
  ends them on a NAK, answers deprecated and unsupported inquiries with a
  NAK and malformed messages meant for it with one, and takes a new MUID
  and invalidates the old when another device uses it. Peers, the
  outgoing queue and reassembly are bounded. Not yet checked against
  another implementation.
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

- `opus`: an Ogg Opus decoder written from RFC 6716, its updates in RFC
  8251, and RFC 7845: SILK in fixed point, bit for bit as the reference
  decodes it, with its stereo prediction and resampler; CELT at every
  bandwidth and frame size, transients, intensity and dual stereo and the
  post-filter; hybrid frames; the redundant frames of a change of mode;
  packets of up to 120 ms; multistream packets of up to 255 channels with
  the mappings of RFC 7845, handed out in the order of WAV. Pre-skip, end
  trimming and the output gain are applied: files libopus wrote decode to
  exactly the length encoded, SILK alone bit for bit as libopus decodes
  it, CELT within 8e-5 (within 1.1e-6 with the `2^x` approximation of the
  libopus FFmpeg links). `AudioReader` reads it block by block and seeks
  exactly within reach of what it has decoded, and with 800 ms of
  pre-roll elsewhere. Lost and empty packets come out as silence: there is
  no packet loss concealment. `FileFormat` has the variant `OggOpus` and
  `FormatError` the variant `Opus`.
- `vorbis::write` and `AudioWriter::vorbis`: an Ogg Vorbis encoder written
  from the Vorbis I specification, at a variable bitrate of quality 0 to
  10 (`VorbisOptions`), 1 to 255 channels at any rate. The forward MDCT
  with long and short blocks switched at transients, floors of type 1
  fitted to a masking model (Zwicker's critical bands, Johnston's
  tonality, Terhardt's threshold in quiet), residues of type 2 for one or
  two channels and of type 1 for more, square polar stereo coupling, and
  Huffman codebooks designed from counts of what it codes, sent in the
  setup header and read back with the decoder's own parser. Comments and
  pictures (`METADATA_BLOCK_PICTURE`) come from `Metadata`. Lengths are
  exact through this decoder, FFmpeg's and libvorbis. On the stereo test
  signal at 44.1 kHz: 77, 124 and 173 kbit/s at qualities 0, 5 and 10,
  the noise 0.4, 11 and 23 dB under an independent masking measure, 40 to
  300 times faster than real time.
- `mp3::write` and `AudioWriter::mp3`: an MP3 encoder written from
  ISO/IEC 11172-3, MPEG-1 layer III at 32, 44.1 and 48 kHz, mono or
  stereo, at a constant bitrate of 32 to 320 kbit/s or a variable one of
  quality 0 to 10 (`Mp3Options`). The polyphase analysis filter bank and
  MDCT of Annex C with the decoder's tables, short blocks at attacks,
  mid/side stereo when it is cheaper, quantisation to the masking model,
  the bit reservoir, Huffman tables and regions chosen for the fewest
  bits, and a Xing or Info header with a LAME tag of delay and padding, so
  that this decoder and FFmpeg give back exactly the samples encoded. Tags
  and pictures go into an ID3v2.4 tag. FFmpeg's floating-point decoder
  comes within 1.2e-6 of this crate's on what it writes. On the stereo
  test signal at 44.1 kHz the noise lies 2.8, 7.6 and 14.5 dB under the
  masking measure at 128, 192 and 320 kbit/s; 25 to 50 times faster than
  real time in stereo.
- The masking model and transient detector the two encoders share are
  in a module of their own; a test helper measures the noise-to-mask
  ratio of a decode against its source with a model apart from theirs.
- `metadata`: one model for what files say beside their audio, the same
  for every format. Typed fields for title, artist, album, date, track,
  genre, comment, copyright, tempo, key, ISRC, the Broadcast Wave
  originator and time reference with the rest of `bext` (EBU Tech 3285,
  loudness included), `iXML` kept as text, an instrument (root note, fine
  tune, gain, key and velocity range), loops, cue points with labels and
  lengths, and pictures; other text as tags, and every chunk not
  understood kept as it was, so that a file read and written again loses
  nothing the model does not interpret. WAV reads and writes `LIST/INFO`,
  `bext`, `iXML`, `cue ` with `LIST/adtl`, `smpl` and `inst`, and reads the
  tempo of `acid`; AIFF reads and writes `NAME`, `AUTH`, `(c) `, `ANNO`,
  `MARK` and `INST`; FLAC reads its Vorbis comment and pictures, and
  `FlacOptions::with_metadata` and `with_picture` write them with any other
  block; Ogg Vorbis comments fill the same fields, pictures included; MP3
  reads ID3v2.2, 2.3 and 2.4 (unsynchronisation, extended headers, every
  text encoding, `APIC`) and ID3v1. `wav::write_with_metadata`,
  `aiff::write_with_metadata` and `AudioWriter::set_metadata` write it;
  `AudioReader::metadata` and the `metadata` field of every decoded file
  read it. Damaged metadata never costs the audio, and no more than the
  file holds is allocated: 16 MiB a chunk, 64 MiB a file.
- `mp3` decodes MPEG audio layers I and II too, written from ISO/IEC
  11172-3 and 13818-3: every allocation table, grouped quantisers, the
  selection of scale factors, joint stereo and the CRC, with exact seeking
  like layer III. Files FFmpeg's encoder wrote decode within 3.2e-5 of its
  own decoder.
- `thumbnail`: waveform overviews. `Thumbnail` keeps the minimum, maximum
  and RMS level of every bucket of a few hundred frames and of every
  coarser level by fours, grows as audio is pushed into it, and answers a
  range at any zoom from a few dozen cells, exactly for the buckets the
  range touches; `ThumbnailBuilder` reads a file on a thread of its own
  and shares the overview as it grows; `write_cache` and `read_cache` keep
  it in six bytes a bucket and channel, checked by a checksum.
- `platform`, behind the new `platform-codecs` feature (and the facade's
  feature of that name): AAC and ALAC in MPEG-4 files, AAC in ADTS, WMA and
  CAF decoded by the system's codecs, Media Foundation on Windows and
  AudioToolbox on macOS, through `read_file`, `decode` and `AudioReader`,
  from a file or from memory. On Windows the priming an MPEG-4 file's edit
  list states is left out, which Media Foundation does not do itself.
  Elsewhere, and without the feature, these formats are recognised and
  refused with `FormatError::Unsupported`. A new CI job builds and tests
  the feature on Linux, Windows and macOS.
- Breaking: `AudioFile`, `WavFile`, `AiffFile`, `FlacFile`, `VorbisFile`
  and `Mp3File` have a `metadata` field; `FileFormat` has the variants
  `Mp4`, `Aac`, `Wma` and `Caf`, and needs 16 bytes to recognise every
  format (`FileFormat::DETECT_LEN`); `FormatError` has the variants
  `Unsupported` and `Platform`. A layer I or II file, refused before, now
  decodes, and the headers of layers I and II of MPEG-2.5, which does not
  exist, are no longer taken for frames.
- `TransportSource`: a streamed file as an `AudioSource` with a
  transport, worked from any thread through a `TransportControl` (play,
  pause, stop, seek, loop, gain) that also tells the position, whether it
  is playing or seeking, and the underruns. The existing worker reads
  ahead; both queues are wait-free on the audio side. Seeks land on the
  exact frame, loops go round sample for sample, a late worker is counted
  and nothing is skipped, and declicking (on by default) fades starts and
  pauses and glides the gain over one block.
- `vorbis`: an Ogg Vorbis decoder written from the Vorbis I specification
  and RFC 3533: floors of type 1, residues of types 0, 1 and 2, codebooks
  of every lookup type, channel coupling, both block sizes, 1 to 255
  channels handed out in the order of WAV, and the comments. A floor of
  type 0, which no encoder in use writes, is refused with an error that
  says so. Files libvorbis wrote decode to exactly the length encoded.
- `mp3`: an MP3 decoder written from ISO/IEC 11172-3 and 13818-3: layer
  III of MPEG-1, 2 and 2.5 at every sample rate, mono, dual channel,
  stereo and joint stereo with mid/side and intensity coding, long, short
  and mixed blocks, the bit reservoir and CRCs. ID3v2 tags are stepped
  over; a Xing, Info or VBRI header gives the length, and a LAME tag the
  samples the encoder added, which are removed, so that the audio decoded
  starts where the audio encoded did and has the length the tag states.
  Layers I and II and free format are refused.
- `AudioReader`, `decode` and `read_file` recognise both from the
  contents, and the reader seeks in both exactly: what is read after a
  seek is, bit for bit, what reading from the start gives there. In Ogg
  Vorbis by bisection over the pages, decoding two packets; in MP3 by
  counting the frames once, from their headers.
- `FileFormat` has two new variants, `OggVorbis` and `Mp3`, and
  `FormatError` two, `Vorbis` and `Mp3`: a `match` on either without a
  wildcard needs the new arms. Lossy files report 32 bits per sample and
  floating point.
- Both decoders run at a hundred times real time or more for stereo at
  44.1 kHz in a release build (`cargo run --release -p manguebyte-formats
  --example decode_speed`: about 340 times for Vorbis, 140 for MP3, on a
  busy machine).
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
  their source sample for sample; the lossy ones, against files libvorbis
  and LAME encoded from signals the tests make, must match their length
  exactly and their audio within measured bounds.

### Devices

- Devices change under a running engine without stopping it.
  `DeviceManager::start_engine` runs the callback at a fixed
  `EngineFormat` (rate, largest block, channels), prepared once; from then
  on `apply`, `restart` and `follow` change the devices under it in the
  middle of a song, and it is never stopped or prepared again: its
  `CallbackInfo::frame` counts on. Where the backend runs two streams at
  once and the output device changes, the new device fades in while the
  old one fades out, both playing, the old device copying what the engine
  renders into a wait-free ring until it hands the engine itself over
  through a wait-free queue. Where they cannot (an ASIO driver per
  process, the same device opened again, a driver's reset request) the
  old device fades out, closes, and the new one opens and fades in, the
  engine waiting so that no frame is lost; a lost device is replaced
  without a fade-out. A device at another rate gets the engine through a
  resampler (0.7 ms more), whose output does not depend on its blocks;
  one at the engine's rate with its channels gets the engine's output
  directly, with nothing copied or delayed. Channel counts are mapped by
  repeating or folding. `set_crossfade` (30 ms by default) and
  `last_swap` (`SwapReport`: how, the silence, the time to reopen,
  underruns). `AudioBackend::supports_concurrent_streams` says whether a
  backend can crossfade. `MockBackend` runs several streams when told to
  (`set_concurrent_streams`, `render_on`, `capture_on`,
  `stream_count`), and asks for restarts (`request_restart`). Proven by
  `the_song_goes_on_while_the_device_changes_under_it` in
  `tests/hotswap.rs`; measured on FlexASIO and ASIO4ALL. Limits:
  TECH_DEBT 85.
- MIDI output from the audio thread: `midi::midi_output_queue` makes a
  `MidiTransmitter`, which stamps each Universal MIDI Packet with the
  moment its frame is heard, one block late as input is, and
  `MidiOutput::spawn` the thread that sends it then. `PacketWriter`
  writes packets as MIDI 1.0 bytes: MIDI 2.0 by the default translation,
  7-bit system exclusive put back together. `MidiOutput::send_packet` and
  `DeviceManager::send_midi_packet` send one from other threads. Every
  port speaks MIDI 1.0 for now (`MidiProtocol`): the systems' own UMP
  APIs are not used yet.
- `manguebyte-pro-io`, a new crate, opt-in and under the GPLv3 alone,
  speaks to ASIO drivers itself. Its driver interface is declared by hand
  from the Steinberg ASIO SDK 2.3.4 interface definitions, used under the
  SDK's GPLv3 option; no SDK file is in the repository. It lists the
  installed drivers from the registry, loads one through COM on a thread of
  its own (a single-threaded apartment, one driver per process), asks it its
  channels, sample types, latencies, buffer sizes, rates and clocks, and
  runs a duplex callback on planar `f32` blocks, with every PCM sample type
  converted and the DSD ones refused. The host callbacks reach the stream
  through a static, without allocating or locking (tested with a counting
  allocator); `asioMessage` answers the selectors the SDK documents,
  `bufferSwitchTimeInfo` is taken when offered and `outputReady` called when
  asked for. A reset request, a new buffer size or sample rate and a loss
  of sync are only counted in the callback; `ProIoBackend` turns them into
  a restart that a `DeviceManager` carries out off the audio thread.
  `FakeDevice`, a COM-shaped driver written in Rust, runs it all in CI. Run
  here on a Behringer UMC404HD (its own driver), FlexASIO and ASIO4ALL,
  whose reset request at start is now followed by a restart instead of a
  stream that stops after half a second. A program that uses the crate is
  under the GPLv3; closed-source programs keep `cpal`'s own `asio`
  feature. No other crate depends on it, which CI checks.
  ASIO is a trademark of Steinberg Media Technologies GmbH, registered in Europe and other countries.
- An audio backend from outside the crate can run behind the device
  manager: `AudioBackend`, given to `DeviceManager::with_audio_backend`,
  with `BackendCallback` (block splitting without copies, silent output,
  frame counting), `StreamStatus` (a lost device or a restart request, both
  real-time safe), `BackendRequest`, `BackendFormat` and `BackendStream`. A
  restart request reaches the manager as `StreamLost` with the new
  `LossCause::RestartRequested`, and `follow` restarts the stream.
  `DeviceError::Other` carries such a backend's own errors.
- `MidiReceiver::read_events` and `DeviceIo::read_midi_events` hand MIDI
  to the audio thread as events, read through a `NoteInput`, so that MPE
  arrives as per-note events; the standalone runner uses them.
- `MidiInput::open_with_sysex` also hands every system exclusive message
  to a handler on the system's MIDI thread, as MIDI-CI needs.
- Device listing with stable identifiers; choice of sample rate, channel
  count and buffer size; `InputDevice`; `DuplexStream`.
- `devices::midi`: MIDI input and output ports. `MidiReceiver` stamps each
  message with its arrival time and places it inside the next block at the
  matching frame, so a player's timing is not quantised to the buffer size.
- Audio hosts: `audio_hosts` lists the system's audio APIs that can be used
  now, and `AudioHost` lists and opens the devices of one of them: WASAPI,
  CoreAudio, ALSA, AAudio, and JACK with the new `jack` feature (on Linux
  it needs the JACK development files to build). ASIO comes from `cpal`'s
  own `asio` feature, which an application turns on itself with
  Steinberg's SDK; the documentation says why this crate has no feature for
  it.
- Timing: `start_timed` on `OutputDevice`, `InputDevice` and `DuplexStream`
  hands the callback a `CallbackInfo` with the frame position, the
  backend's clock and the latencies it reports. Streams tell how many frames
  they have processed and whether the backend reported their device lost.
- `DuplexStream` follows the drift between two devices' clocks by
  resampling the input instead of dropping or padding it
  (`DuplexSettings::drift_correction`, on by default;
  `DuplexStream::clock_drift_ppm`). `Resampler::set_ratio` changes a
  converter's ratio while it runs.
- `DeviceManager`: opens the input, output and MIDI ports a `DeviceSetup`
  names and runs one `AudioCallback` on them, keeping it across restarts.
  A setup is saved as versioned text and restored with fallbacks, each
  listed in a `RestoreReport`: a device found by its name when its
  identifier changed, the default when it is missing, a rate, channel count
  or buffer size the device can do. MIDI from every open port is merged
  into the callback's `DeviceIo::read_midi`.
- Hot-plug: `DeviceManager::poll_devices` and `DeviceWatcher` report
  devices and MIDI ports added and removed, the default changing, and a
  stream whose device was lost (unplugged, reported by the backend, or a
  callback silent for two seconds). The devices are polled, every second
  by default, because no backend notifies. `DeviceManager::follow` restarts
  on the saved device when it returns and on the default when the stream's
  device is lost.
- Virtual MIDI ports: `MidiInput::create_virtual` and
  `MidiOutput::create_virtual` on macOS, iOS, Linux and the BSDs;
  `MidiError::VirtualPortsUnsupported` on Windows and Android.
- `MockBackend`: a headless stand-in for the system's devices, driven by
  hand, through which the manager, hot-plug, saved setups and duplex
  timing are tested on every system, and the device path is checked for
  allocations.
- The core, DSP, MIDI, formats and device crates and the facade build for
  Android (`aarch64-linux-android`) and iOS (`aarch64-apple-ios`) in CI, and
  the crates that do not touch devices for WebAssembly.
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
- CI runs on pushes to branches named `ci/...` as well as `main`, so a
  change can be checked on every system before it is merged.
- CI runs `cargo mangue`'s end-to-end test on Windows, Linux and macOS:
  generate a plugin of each kind, build it, test it and bundle it, as CLAP,
  VST 3 and LV2.
- The block-size test of the facade also renders the song through a
  partitioned convolution in both latency modes, FIR filters in both
  modes, an expander and a phase-modulated wavetable oscillator, and
  requires identical output for every block pattern.
- CI builds `manguebyte-web` and `manguebyte-sampler` for WebAssembly,
  plays the sampler's module in Node.js and runs the engine's JavaScript
  tests against its module, failing if any test was skipped; Miri runs the
  native tests of the engine's WebAssembly exports; a job bundles both
  example plugins as LV2 and loads them with lilv; the Windows job runs the
  two-binaries test on the release bundles; and a check fails if two
  packages have an example of the same name, which cargo would build to
  one file.
- `docs/HOST_TESTS.md` records the tests done by hand in real hosts:
  REAPER 7.82 on Windows on 14 October 2026 (both example plugins as CLAP
  and VST 3) and on 19 October 2026 (Mangue Pluck's CLAP editor opened,
  its output under its -1 dBFS ceiling with every parameter at its
  maximum, and the Aratu Bridge applying every event a page sent).
  `docs/RELEASING.md` asks for the example plugins to be installed again
  before such a test.

### Fixed

- A released `WindVoice` could go on sounding by itself at low sample
  rates: the flute's G4 at 3 kHz held at about -25 dBFS, its B5 at 6 kHz
  too. The bore's loop high-pass, a DC blocker whose gain rises to
  `2 / (1 + R)` above its corner, gained up to 10 % there once its corner
  (a quarter of the note) was a large share of the sample rate, more than
  the bell lost, and the vortices' loss at the embouchure held the swing
  at a level. The loop now uses the high-pass scaled by `(1 + R) / 2`,
  passive at every frequency, so the loop's gain stays below the bore's
  at every rate; a test releases every preset across its range at 3 to
  22 kHz and `manguebyte-web` checks the winds at every rate again.
- After a panic in the plugin, the CLAP and VST 3 wrappers did not give it
  the parameter values the host had sent in that block, so a plugin that
  follows events alone kept the old ones. The next block now starts with
  every parameter's value, as the LV2 wrapper's does.
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
