# Architecture

This document explains how Manguebyte is put together and why. Read it before
adding a crate, changing a core trait, or touching anything that runs on the
audio thread. Individual decisions are recorded in [`adr/`](adr/).

## What we are building

A framework that covers what JUCE covers for C++ (devices, MIDI, DSP, files,
parameters, plugins, user interfaces, hosting) and that is better where JUCE
is known to hurt. [JUCE_PARITY.md](JUCE_PARITY.md) tracks the first half of
that sentence module by module. This document is about the second half.

Four requirements shape everything:

| Requirement | What it means in practice |
|---|---|
| **Real-time safe by construction** | Code on the audio thread never allocates, locks, blocks or panics, and the framework never does so on the user's behalf. Tests enforce it. |
| **Light** | You pay only for what you use. Each area is its own crate; the core has no dependencies; nothing needs a GUI or an event loop to process audio. |
| **Complete** | One coherent set of types from the sound card to the plugin format, so the parts fit without glue. |
| **Scalable** | The same processor runs in a test, an offline render, a standalone app and every plugin format, in `f32` or `f64`. |

## Crates

Dependencies only point downwards. A crate may use anything below it and
nothing beside or above it.

```text
                         manguebyte            facade: re-exports + prelude
                              |
      +-----------+-----------+-----------+
      |           |           |
      |           |         plugin          plugin API + test harness
      |           |          /   \
   devices     formats     dsp   params
   (cpal)         |        /  \     |
      |           |     midi   \    |
      +-----------+-------------+---+
                        |
                      core                  no dependencies
```

| Crate | Role | Depends on |
|---|---|---|
| `manguebyte-core` | `AudioBuffer`, `Sample`, smoothing, atomics, wait-free queue, real-time log | nothing |
| `manguebyte-midi` | MIDI 1.0 and 2.0 messages, translation, MPE, MIDI files, sample-accurate event buffers | nothing |
| `manguebyte-params` | Parameters, ranges, smoothing, saved state | core |
| `manguebyte-dsp` | `Processor`, events, transport, synth, filters, effects, dynamics with sidechain keying, convolution, FIR filtering, wavetable oscillator, FFT | core, midi |
| `manguebyte-formats` | Audio files: whole or streamed, read and written, with play and record bridges | core |
| `manguebyte-osc` | Open Sound Control, with a real-time bridge | core |
| `manguebyte-analysis` | Loudness, true peak, level meters, spectrum, pitch | core, dsp |
| `manguebyte-devices` | Sound card input, output and duplex; MIDI ports | core, midi, `cpal`, `midir` |
| `manguebyte-plugin` | `Plugin` trait, state, `Harness` for testing without a host | core, midi, dsp, params |
| `manguebyte-graph` | A processing graph that is rewired while it plays | core, dsp |
| `manguebyte-standalone` | Runs a `Plugin` on the sound card and MIDI ports | core, midi, dsp, params, plugin, devices |
| `manguebyte-clap` | Turns a `Plugin` into a CLAP plugin | core, midi, dsp, params, plugin, `clap-sys` |
| `manguebyte-vst3` | Turns a `Plugin` into a VST 3 plugin | core, midi, dsp, params, plugin, `vst3` |
| `manguebyte-lv2` | Turns a `Plugin` into an LV2 plugin, and generates its Turtle files | core, midi, dsp, params, plugin |
| `manguebyte-host` | Loads CLAP and VST 3 plugins and runs them as processors | core, midi, dsp, `clap-sys`, `vst3`, `libloading` |
| `manguebyte-gfx` | Scenes, a CPU rasteriser behind a private seam, text with an embedded typeface | `tiny-skia`, `skrifa`, `harfrust` |
| `manguebyte-ui` | Retained widget tree, layout, theme, accessibility model, widgets, a test harness | core, gfx, params, plugin (for the editor contract) |
| `manguebyte-window` | Platform windows: a child of a host's window (Win32 so far) and a headless backend for tests | gfx, plugin (for the editor contract), `windows-sys` on Windows |
| `manguebyte-gui` | Glue between the plugin wrappers and the interface: an `EditorSpec` that opens a widget tree in a child window | gfx, plugin, ui, window |
| `cargo-mangue` | The command-line tool: new, bundle, install, validate | `serde_json` |
| `manguebyte` | Everything under one name | all of the above except `manguebyte-lv2`, the interface crates and `cargo-mangue` |

Planned crates slot into the same picture: each further plugin format
gets a crate beside `manguebyte-clap`, `manguebyte-vst3` and
`manguebyte-lv2`, on top of `manguebyte-plugin`. The interface crates of
[ADR 0007](adr/0007-user-interface-layer.md), `manguebyte-gfx`,
`manguebyte-ui`, `manguebyte-window` and `manguebyte-gui`, sit beside them,
and no crate that processes audio may depend on any of them, directly or
through another crate; `scripts/check-layering.sh` checks this in CI with
every feature and every target resolved.

The editor contract is the one piece of the interface below that line. It
lives in `manguebyte-plugin` as `manguebyte_plugin::editor`, with no
dependency and no `unsafe`, and the only part of it the audio thread touches
is `GestureReader`, which reads two atomics per parameter. The interface
crates depend on the plugin API, never the other way round: a plugin
returns a `manguebyte_gui::Editor` as its `EditorSpec`, and the wrappers
open it without knowing which toolkit it uses.

Rules that keep this healthy:

- **`core` stays dependency-free.** It is the one crate everybody compiles.
- **Processing never depends on presentation.** JUCE's plugin processing and
  hosting code depended on its GUI modules until 8.0.11, when a headless
  variant was split out. Here that dependency cannot be written: the GUI
  crate is above the plugin API, not below it.
- **A dependency needs a reason.** Platform audio is a reason. Convenience is
  not.

## Threads

Audio software has one thread that must never be late, and several that may
take their time. Every design decision follows from keeping them apart.

```text
   UI / host / worker threads                      audio thread
   (may allocate, lock, block)                (must do none of those)

   set a parameter  ----- atomic store ------>  read the value
   send a command   ----- spsc queue -------->  pop, apply
   show a meter     <---- atomic / spsc ------  publish level
   load a file  -> build new state
                    ----- spsc queue -------->  swap it in
                    <---- spsc queue ---------  send the old one back to be freed
```

What crosses between the two sides:

| Mechanism | Use it for | Type |
|---|---|---|
| Atomic value | One number the other side polls: a parameter, a meter | `AtomicF32`, the `Param` types |
| Wait-free queue | Ordered messages, blocks of audio, ownership of buffers | `spsc::channel` |
| Triple buffer | The latest whole frame of something large, such as a spectrum, for one reader who only wants the newest | `manguebyte_analysis::triple_buffer` |
| Nothing else | | |

The triple buffer is the queue's sibling for data where only the newest copy
matters: the writer never waits and never overwrites what the reader holds.
Beyond these three the framework has **no listener callbacks**: a callback that fires on whichever thread wrote a value is how
user-interface code ends up running on the audio thread. In JUCE, parameter
listeners are invoked synchronously on the calling thread, including the
audio thread during host automation, and the attachment classes respond by
posting to the operating system's message queue, which takes a lock. Here the
audio thread polls a `ParamSmoother` or a `Changed`, and an interface polls on
its own timer. See [ADR 0003](adr/0003-parameters-without-listeners.md).

### The real-time contract

`Processor::prepare` may allocate and take its time. `Processor::process` and
`Processor::reset` run on the audio thread and must not:

- allocate or free memory,
- lock, block or wait,
- do I/O of any kind,
- panic.

This is checked, not hoped for. Each crate with audio-thread code has a
`tests/realtime.rs` that installs a counting allocator and fails if anything
touches the heap while processing: processors in `manguebyte-dsp`, meters and
analysers in `manguebyte-analysis`, bridges in `manguebyte-formats`,
`manguebyte-osc` and `manguebyte-midi`, the log in `manguebyte-core`, and the
wrappers and the host through their own tests. Every new processor is added
to the test of its crate.

Printing is I/O, so the audio thread logs through a `LogQueue` instead
(`manguebyte_core::log`). Posting is wait-free and never allocates: the text
is formatted into a buffer on the stack and copied into one of a fixed set of
slots, and a message below the queue's level is dropped before it is
formatted. Another thread drains the queue and receives the messages in the
order they were posted, with a count of any that found no free slot.

The only `unsafe` code in the framework is in these places: the wait-free
queue; the triple buffer; the boundaries where the CLAP, VST 3 and LV2
wrappers meet a host; the boundaries where the host meets a plugin (loading
a library, the CLAP and VST 3 interfaces, and on macOS three CoreFoundation
calls); the Win32 backend of `manguebyte-window`, which calls the system;
and the two instructions that tell the processor to treat subnormal numbers
as zero ([ADR 0006](adr/0006-subnormal-numbers-are-flushed.md)). All but the
last two run under Miri in CI, except where a test has to load a real
shared library, which Miri cannot do. The Win32 backend cannot run under
Miri either, so it is tested against real windows on Windows; the last is
checked by what the processor then does. Every crate outside that list
forbids `unsafe` code outright (`#![forbid(unsafe_code)]`), so the list
cannot grow without someone deciding it should; `manguebyte-window` denies
it everywhere but its Win32 module. The example plugins are the exception:
the macros that export them expand to the `extern "C"` entry points a host
looks for.

### State

Hosts call for a plugin's state from threads of their own choosing, while
audio is running. JUCE leaves the synchronisation to each plugin author. Here
parameter values are the single source of truth and live in atomics, so
`ParamSet::save` and `ParamSet::load` are safe from any thread at any time
with no lock. Larger state (a sample, a wavetable) follows the swap pattern
in the diagram above: build off-thread, hand over through a queue, return the
old value through a queue to be freed. `manguebyte-graph` is that pattern in
full: a whole compiled graph is swapped in, and the one it replaces travels
back to be dropped.

## Data flow

```text
  file ---> formats ---+                              +---> formats ---> file
                       |                              |
  sound card -> devices -+-> AudioBuffer -> Processor -+-> devices -> sound card
                       |     (planar)        chain    |
  MIDI port --> midi --+-> MidiBuffer ----^           +---> midi ---> MIDI port
                              params -----^
```

- **`AudioBuffer<S>`** is planar: one contiguous slice per channel, which is
  what DSP loops and SIMD want. Devices and files speak interleaved; the
  conversion happens once, at the edge.
- **`Sample`** is `f32` or `f64`. Processors are generic over it. Coefficient
  design is done in `f64` regardless.
- **`Processor`** transforms a buffer in place. A tuple of processors is a
  processor, resolved at compile time, so a fixed chain costs the same as
  calling each stage by hand and the compiler can inline across stages.
  `Box<dyn Processor>` covers chains decided at run time.
- **`EventBuffer`** carries everything that happens during a block, each
  item with the frame it falls on: notes, per-note expression, parameter
  values, modulation and raw MIDI, in one ordered stream.
- **`ProcessContext`** hands a processor that stream together with the
  transport and an optional sidechain. `split_at_events` cuts the block at
  each event without copying, so every change lands on its exact frame and
  the output does not depend on how the host sized its blocks.
  `split_at_events_with` also hands each piece a context whose `offset`
  says where the piece begins, so a processor keyed by the sidechain reads
  it at the piece's own frames.
- **`Synth`** allocates voices from those events. Per-note expression and
  modulation reach one voice; parameters reach all of them.

## What the design learns from JUCE

JUCE is the reference for what a complete audio framework contains, and its
issue tracker, forum and changelog are a record of what goes wrong in one
over twenty years. The table lists the lessons already built in, and the ones
that the plugin API must honour when it is written.

| Observed in JUCE | Decision here | Status |
|---|---|---|
| Parameter listeners run on the audio thread during automation | No listeners; both sides poll atomics | Done |
| Attachments post to the OS message queue from the audio thread | The only bridges are atomics and a wait-free queue | Done |
| State callbacks race with processing; each plugin must synchronise | Parameters are atomic and are the source of truth; a restored state reaches the audio thread as ordinary events | Done for parameters |
| Saved state can drift from parameter values | State is derived from the values on demand, never cached | Done |
| Audio thread reads parameters through a string lookup | Typed struct fields; ids are for hosts and interfaces only | Done |
| Processing and hosting depended on GUI modules | The GUI crate sits above the plugin API | By construction |
| No native CLAP | CLAP is the first plugin format, and passes the CLAP project's own validator | Done |
| The VST3 wrapper applies only the last automation point of each block | Every point is an event on its frame; while blocks run, the value a host sets on the controller is kept for display and never competes with the points | Done |
| No sample-accurate automation in `AudioProcessor` | The process call carries timestamped events; `split_at_events` applies each on its frame | Done |
| No MIDI 2.0, note expression or per-voice modulation in the processor model | Notes carry ids and float velocity; expression and modulation are per note; modulation never alters the stored value | Done in the model; UMP transport planned |
| Real-time safety rests on convention | A counting-allocator test and Miri in CI | Done, grows with the code |
| Each plugin has to remember `ScopedNoDenormals` in its callback | The wrappers and the device streams hold the guard around every block | Done |

Sources for the left column are in
[ADR 0002](adr/0002-real-time-thread-model.md) and
[ADR 0003](adr/0003-parameters-without-listeners.md); the event model is
in [ADR 0005](adr/0005-one-timestamped-event-stream.md). JUCE is studied as a
reference; its code is never copied. See
[ADR 0004](adr/0004-juce-as-reference.md).

## What we would revisit as it grows

- **`ProcessContext` has no output events yet.** An arpeggiator or a MIDI
  effect needs to emit events as well as receive them. The struct is
  non-exhaustive so the field can be added without breaking anyone.
- **Duplex streams absorb clock drift by dropping or padding.** Once a
  resampler exists, they should correct drift by adaptive resampling.
- **The FFT is scalar radix-2.** Split-radix and SIMD kernels are a
  self-contained optimisation behind the same API.
- **`core` uses `std`** for `Arc` and `Vec`. `no_std` with `alloc` is a
  mechanical change, wanted for embedded targets.
