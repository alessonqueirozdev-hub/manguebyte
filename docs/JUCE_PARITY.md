# JUCE parity

Manguebyte's baseline goal is to offer everything JUCE offers. This page
tracks that goal module by module, against the 24 modules in JUCE's `master`
branch (version 9.0.4 when this was written, October 2026).

The module list is taken from the JUCE repository and its documentation
index. The feature summaries are a working description based on JUCE's public
documentation and will be refined into per-feature checklists as each area is
built. JUCE is a reference for scope here, never a source of code: see
[ADR 0004](adr/0004-juce-as-reference.md).

Parity means the same capability, not the same classes. Where Rust or its
ecosystem already solves something well, the plan says so instead of
rebuilding it.

Legend: ✅ done · 🚧 partly there · ⬜ not started · ➖ not planned as part of
Manguebyte

## Audio

| JUCE module | What it covers | Manguebyte | Status |
|---|---|---|---|
| `juce_audio_basics` | Audio buffers, sample conversion, MIDI messages and buffers, MIDI files, MPE, UMP, synthesiser voices, smoothed values, basic sources, ADSR | `core`, `midi`, `dsp` | 🚧 Buffers, smoothing, channel and system messages, SysEx, stream parsing, MIDI files, sample-accurate MIDI buffer and ADSR are done. Voice allocation is done too, with per-note expression. MIDI 2.0 packets (UMP) with MIDI 1.0 translation, RPN/NRPN, MPE zones and tracking, and keyboard state are done. Missing: audio sources, a sampler (planned as an SFZ player), MPE and MIDI 2.0 delivered to processors as per-note events |
| `juce_audio_devices` | Device enumeration and selection, input and output streams, per-platform backends (CoreAudio, WASAPI, ASIO, ALSA, JACK, Oboe, and others), MIDI ports, device manager with saved setup | `devices` | 🚧 Listing, selection by stable id, sample rate and buffer size, output, input, duplex and MIDI ports are done. Missing: host selection (ASIO, JACK), hot-plug notification, a device manager with saved setup, virtual MIDI ports |
| `juce_audio_formats` | Readers and writers for WAV, AIFF, FLAC, Ogg Vorbis, MP3 and platform codecs; streaming sources; thumbnails cache; sampler | `formats` | 🚧 WAV (with RF64) and AIFF read and write, FLAC decoding and encoding, format detection, streaming readers and writers with exact seeking, and wait-free play and record bridges for audio callbacks. Missing: Ogg Vorbis, MP3, metadata, thumbnails |
| `juce_audio_processors` | The plugin and processor model, parameters and their state tree, plugin hosting for VST3, AU, LV2 and others, processor graph, plugin scanning | `params`, later `plugin`, `host`, `graph` | 🚧 Parameters, state and the plugin model are done, with a different design ([ADR 0003](adr/0003-parameters-without-listeners.md), [ADR 0005](adr/0005-one-timestamped-event-stream.md)). The processing graph is done (`graph`). Hosting CLAP and VST 3 plugins is done (`host`), through one interface for both, with every event on its frame. Missing: scanning, AU and LV2 hosting, hosted editors |
| `juce_audio_processors_headless` | The same, without GUI dependencies (added in JUCE 8.0.11) | the same crates | ✅ by construction: no processing crate depends on a GUI |
| `juce_audio_plugin_client` | Wrappers that turn a processor into a VST3, AU, AUv3, LV2, AAX, Unity or standalone plugin | `clap`, `vst3`, `lv2`, `standalone`, later one crate per format | 🚧 CLAP and VST 3 are done and pass their projects' validators in CI, and so is the standalone wrapper. LV2 is built and tested in process through its C interface, acting as a host, but its Turtle files have not been checked by `lv2lint` and no LV2 host has loaded it yet; it offers no editor. CLAP and VST 3 implement their editor interfaces (CLAP `gui`, VST 3 `IPlugView` and the component handler), and `gui` opens editors in a child window on Win32, tested headless through both formats' real interfaces and embedded in a real window; no editor has been opened in a real host yet. Next: AU. JUCE has no native CLAP |
| `juce_audio_utils` | Ready-made audio widgets and helpers: device selector, keyboard, thumbnail, transport-style player | later `gui` | ⬜ |
| `juce_dsp` | FFT, windows, IIR and FIR filters with design methods, state-variable and ladder filters, oversampling, convolution, delay lines, dynamics, reverb, chorus, phaser, waveshaping, oscillator, panner, SIMD wrappers, lookup tables | `dsp` | 🚧 Processor chains, oscillator, biquad designs, state-variable filter, delay, gain, compressor, noise gate, reverb, FFT of any length and windows are done. Oversampling, resampling, look-ahead limiter, waveshaper, noise, LFO and FIR design are done as well, and so are the one-pole and ladder filters (the ladder's cutoff stops at 0.45 of the sample rate), high-order IIR design (Butterworth, Chebyshev I and II, elliptic, orders 1 to 16), Linkwitz-Riley crossovers, chorus, flanger, phaser, dry/wet mixing, panner, envelope follower and interpolated delay lines. Convolution (uniformly partitioned, with zero latency or a latency of one partition, true stereo, responses swapped while playing), a FIR filter processor, an expander, sidechain keying for the dynamics and a band-limited wavetable oscillator are done too. Missing: SIMD, lookup tables |
| `juce_midi_ci` | MIDI Capability Inquiry: discovery, profiles, property exchange | later, in `midi` | ⬜ |
| `juce_osc` | Open Sound Control messages over UDP | `osc` | ✅ Messages, bundles, address patterns, UDP and TCP (OSC 1.1 framing), and a real-time bridge whose time-tagged bundles land on their frame. Missing: the OSC 1.0 TCP framing |

## User interface

| JUCE module | What it covers | Manguebyte | Status |
|---|---|---|---|
| `juce_graphics` | 2D drawing: paths, colours, gradients, images, fonts and text layout, software and platform renderers | `gfx` (CPU first, see ADR 0007) | 🚧 Paths, strokes, solid and gradient paint, images, clips, transforms and layers, recorded as a scene with an exact text dump; a CPU rasteriser behind a seam whose damage-limited repaint equals a full one; text shaping and layout with an embedded Latin typeface. Missing: platform and GPU renderers, font fallback, complex scripts, bidirectional text |
| `juce_gui_basics` | Component tree, windows, mouse and keyboard, layout, look-and-feel, the standard widgets, menus, drag and drop, accessibility | `ui`, `window`, `gui` | 🚧 Retained tree, pointer, wheel, key and focus input, row, column and stack layout, theme tokens, label, knob, slider, toggle, meter and canvas, damage tracking, and an accessibility description every widget must give. Windows on Win32 only: a child of a host's window, with a headless backend for tests on every system. Missing: windows on macOS and Linux, menus, popups, drag and drop, text entry, platform accessibility |
| `juce_gui_extra` | Code editor, web view, system tray, and other specialised widgets | later `gui` | ⬜ |
| `juce_animation` | Animators and easing | later `gui` | ⬜ |
| `juce_opengl` | OpenGL contexts and rendering into components | later, behind the renderer seam of `gfx`; not the default while CI cannot test GPU pixels (ADR 0007) | ⬜ |
| `juce_video` | Video playback and camera capture | ➖ | Use a dedicated crate |

## Foundation

A large part of JUCE is a general-purpose C++ toolkit that predates the
modern standard library. Rust's standard library and ecosystem cover most of
it, so the plan here is mostly to not rebuild it.

| JUCE module | What it covers | Manguebyte | Status |
|---|---|---|---|
| `juce_core` | Strings, containers, files, streams, threads, time, XML, JSON, networking, memory utilities, logging | Rust `std` and established crates | ➖ except audio-specific pieces: lock-free queue ✅, atomic floats ✅, denormal guard ✅ (applied automatically, ADR 0006), logging that is safe on the audio thread ✅, high-resolution timer ⬜ |
| `juce_events` | Message loop, timers, async updaters, change broadcasters, inter-process connections | later `gui` for the event loop | ➖ as a general facility. The audio side deliberately has no broadcaster ([ADR 0002](adr/0002-real-time-thread-model.md)) |
| `juce_data_structures` | Value trees, undo manager, properties files | later, if the GUI needs it | ⬜ Undo for parameter changes is planned in the plugin layer |
| `juce_cryptography` | Hashes, RSA, Blowfish | ➖ | Use RustCrypto |
| `juce_javascript` | Embedded JavaScript engine | ➖ | Use a dedicated crate |
| `juce_box2d` | Bundled 2D physics | ➖ | Use a dedicated crate |
| `juce_analytics` | Analytics event collection | ➖ | Out of scope |
| `juce_product_unlocking` | Licence key and online unlock helpers | ➖ for now | Revisit with the plugin tooling |

## Beyond parity

Things JUCE does not offer, or offers only in part, that Manguebyte commits
to. Each one is tied to something observable.
[BEYOND_JUCE.md](BEYOND_JUCE.md) explains the main ones in detail: the
problem, what JUCE does, what Manguebyte does, the test that proves it and
the limits of the claim.

| Area | JUCE today | Manguebyte |
|---|---|---|
| Real-time safety | A convention; the framework's own parameter attachments post to the OS message queue from the audio thread | Enforced: an allocation in `process` fails CI ✅ |
| Parameter threading | Listeners run on whichever thread changes the value, including the audio thread | No listeners; atomics and polling ✅ |
| State and processing | Host may call state functions concurrently with processing; each plugin synchronises | Parameter state is safe from any thread with no lock ✅ |
| Old sessions after a range change | Position-based automation and state can shift meaning | State stores real values by id; loading is tolerant and reports ✅ |
| Headless use | Split out in 8.0.11, still needs the event module | No GUI or event loop below the GUI crate ✅ |
| CLAP | Not available natively; promised for JUCE 9, not in 9.0.x | Native. Both example plugins pass every applicable test of `clap-validator` in CI (the instrument 35 of 44, the effect 33 of 44, none failed) ✅ |
| Sample-accurate automation | Not in `AudioProcessor` | Timestamped events in the process call, applied on their exact frame ✅ |
| Note expression, per-voice and non-destructive modulation | Not in the processor model | Part of the event model; the synth routes each to the right voice ✅ (MIDI 2.0 UMP transport ⬜) |
| Arbitrary FFT length | Powers of two | Any length ✅ |
| Compile-time chains | Template chains exist; graph is dynamic | Tuples of processors inline end to end ✅ |
| Block-size independence | Left to each plugin; splitting a block is manual | Tested: a song rendered with six block patterns must match bit for bit ✅ |
| Testing a plugin | Needs a host, or a hand-built test rig | `Harness` schedules notes and automation and renders in a unit test, on CI machines with no sound card ✅ |
| Changes from a UI or a state restore | Visible only to code that polls or listens | Turned into events on frame 0, so the event stream is a complete record of every change ✅ |
| Licence | AGPLv3 or commercial | MIT or Apache-2.0 ✅ |

Sources for the JUCE column: the JUCE repository and changelog, the JUCE
roadmap update of 2024, and the forum threads cited in ADRs 0002 and 0003.
JUCE has announced work on an "AudioProcessor v2" covering sample-accurate
automation and CLAP features, so some rows may change in later 9.x releases.
