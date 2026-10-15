<p align="center">
  <img src="assets/logo.png" alt="Manguebyte logo: a crab wearing headphones and playing an electric guitar" width="280">
</p>

<h1 align="center">Manguebyte</h1>

<p align="center">
  <strong>A modern, real-time-safe audio framework for Rust.</strong><br>
  Our goal is to be for Rust what JUCE is for C++, and then go further.<br>
  <a href="#where-it-already-goes-beyond-juce">See where it already does.</a>
</p>

<p align="center">
  <a href="https://github.com/alessonqueirozdev-hub/manguebyte/actions/workflows/ci.yml"><img src="https://github.com/alessonqueirozdev-hub/manguebyte/actions/workflows/ci.yml/badge.svg" alt="CI status"></a>
  <img src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue" alt="License: MIT OR Apache-2.0">
  <img src="https://img.shields.io/badge/rust-1.85%2B-orange" alt="Rust 1.85+">
</p>

---

## The goal

C++ has [JUCE](https://juce.com): one framework that takes you from an empty
folder to a shipping synthesizer, effect plugin or audio application on every
major platform. Rust has excellent audio crates, but they are scattered pieces
that each project has to glue together on its own.

**Manguebyte intends to be the complete equivalent of JUCE for Rust**: audio
and MIDI device I/O, DSP, audio file formats, a parameter system, plugin
formats (CLAP, VST3, AU, LV2, standalone), plugin hosting, and a GUI toolkit,
all designed to work together and all under a permissive license.

We do not want a port. We want what a framework looks like when it is designed
for Rust from the first line:

- **Fearless real-time code.** Ownership rules catch data races between the
  audio thread and the UI at compile time, instead of as a crackle in
  production.
- **Zero-cost composition.** A tuple of processors is a processor. The chain is
  resolved at compile time, with no virtual calls and no allocation.
- **Small, independent crates.** Use the whole framework or only the buffer
  type. `manguebyte-core` has no dependencies at all.
- **Permissive licensing.** MIT or Apache-2.0, your choice. No copyleft, no
  per-seat fees, no splash screen.

## Where it already goes beyond JUCE

Matching JUCE is the baseline. These are the places where Manguebyte already
does something JUCE 9 does not, each one backed by a test that runs on every
commit. [docs/BEYOND_JUCE.md](docs/BEYOND_JUCE.md) explains every row, with
sources for what is said about JUCE and the limits of each claim.

| | JUCE 9 | Manguebyte | Proof |
|---|---|---|---|
| **Automation timing** | A processor reads each parameter once per block | Every change arrives with its frame and is applied on the exact sample | [test](crates/manguebyte-plugin/src/harness.rs) |
| **Block size** | Whether the sound depends on the host's buffer size is up to each plugin | The same song rendered with six block patterns must match bit for bit | [test](crates/manguebyte/tests/offline.rs) |
| **Changes from a UI or a restored state** | Found by polling, or by listeners that can run on the audio thread | Delivered as events in the same stream as automation; no listeners exist | [test](crates/manguebyte-plugin/src/harness.rs) |
| **Real-time safety** | A rule in the documentation | A counting allocator fails CI if any processor, or the plugin wrapper, touches the heap while processing | [test](crates/manguebyte-dsp/tests/realtime.rs) |
| **Per-note control** | Through MPE channels | Note expression and modulation events addressed to one note; modulation never alters the stored value | [test](crates/manguebyte-dsp/src/synth.rs) |
| **CLAP** | Not supported natively | One line, `export_clap!`; the example plugin passes the CLAP project's validator in CI | [CI](.github/workflows/ci.yml) |
| **Testing a plugin** | Needs a host or a hand-built rig | `Harness` plays notes and automation into a plugin inside a unit test | [example](plugins/pluck/src/lib.rs) |
| **Parameters and state across threads** | Each plugin synchronises state access itself | Parameters are atomics; saving and loading need no lock and cannot race | [test](crates/manguebyte-params/src/set.rs) |
| **Hosting plugins** | No CLAP hosting; a hosted plugin is one object called from every thread; parameter changes carry no frame | Hosts CLAP and VST 3 with the thread rules in the types, and every event on its frame: hosted output is identical bit for bit to the plugin run alone | [test](crates/manguebyte-host/tests/clap_pluck.rs) |
| **Loudness and broadcast meters** | None in the documented API | BS.1770 / EBU R 128 loudness, true peak, VU, PPM and correlation, checked against the EBU's own test signals | [test](crates/manguebyte-analysis/tests/ebu_tech_3341.rs) |
| **OSC timing** | Listeners on the message or network thread | A time-tagged bundle lands on its exact frame in the audio thread | [test](crates/manguebyte-osc/tests/bridge.rs) |
| **Slow-downs in silence** | A guard against subnormal numbers that each plugin places in its own callback | Set by the framework around every plugin and every device callback | [test](crates/manguebyte-vst3/tests/abi.rs) |
| **Licence** | AGPLv3 or commercial | MIT or Apache-2.0 | [licence](LICENSE-MIT) |

These checks find real problems. The block-size test failed the first time it
ran and exposed a voice that kept running after it had finished. The
allocation guard caught a mutex that allocates on macOS. The CLAP validator
and Miri each found a defect that the project's own tests had missed. The
VST 3 wrapper's tests found a synth that forgot where the pitch wheel was
between notes, and Steinberg's validator showed a class id that a session
moved between Windows and macOS would not have recognised. The CLAP
validator's timing of the example effect led to the framework guarding every
plugin against subnormal numbers. All of them are fixed, and the checks stay
in CI.

**Where JUCE is ahead.** JUCE has more than twenty years of production use
behind it and more breadth: a full GUI toolkit with plugin editors, AU, AAX
and LV2, hosting of AU and LV2 plugins and of plugins' editors, more file
formats, and mobile platforms. Manguebyte has none of those yet. [docs/JUCE_PARITY.md](docs/JUCE_PARITY.md)
tracks the gap module by module.

## Status

Manguebyte is **young**. What exists is tested and usable, and it is enough
to build and ship an instrument or effect as CLAP and VST 3, but it covers a
small part of the road ahead and the API will change before 1.0. See
[ROADMAP.md](ROADMAP.md) for what exists and what is planned.

| Crate | What it gives you today |
|---|---|
| [`manguebyte`](crates/manguebyte) | The whole framework under one name, plus a prelude |
| [`manguebyte-core`](crates/manguebyte-core) | `AudioBuffer`, the `Sample` trait, `Smoothed` values, atomic floats, a wait-free SPSC queue, a log the audio thread can write to, decibel maths, a guard against subnormal numbers |
| [`manguebyte-dsp`](crates/manguebyte-dsp) | The `Processor` trait and tuple chains, sample-accurate events and transport, a polyphonic `Synth` with per-note expression, band-limited oscillator, biquad filters (8 shapes), high-order IIR design (Butterworth, Chebyshev, elliptic), state-variable, one-pole and ladder filters, Linkwitz-Riley crossovers, ADSR envelope, interpolated delay lines and echo, chorus and flanger, phaser, compressor, noise gate, look-ahead limiter, reverb, waveshaper, dry/wet mixing, panner, envelope follower, oversampling, resampling at any ratio, noise, LFO, smoothed gain, FFT of any length, window functions, FIR design and a FIR filter, convolution with zero latency, true stereo and responses swapped while playing, an expander, sidechain keying for the dynamics, a band-limited wavetable oscillator |
| [`manguebyte-analysis`](crates/manguebyte-analysis) | Loudness to BS.1770 and EBU R 128, checked against the EBU's test signals; true peak, peak, RMS, VU and PPM meters; stereo correlation; a spectrum analyser with octave bands and weighting; pitch detection. All real-time safe, readable from any thread |
| [`manguebyte-graph`](crates/manguebyte-graph) | A processing graph that is rewired while it plays: edits reach the audio thread through a wait-free queue, nodes keep their state, removed nodes are freed off the audio thread, and paths of different latency are lined up automatically |
| [`manguebyte-params`](crates/manguebyte-params) | Lock-free float, integer, switch and choice parameters; linear, skewed and logarithmic ranges; text conversion; smoothing; saved state that survives version changes |
| [`manguebyte-plugin`](crates/manguebyte-plugin) | The `Plugin` trait every format wraps, plugin state, and a `Harness` that plays notes, automation and a sidechain into a plugin inside a unit test |
| [`manguebyte-clap`](crates/manguebyte-clap) | Exports a `Plugin` as a CLAP plugin with one line. Notes, expression, automation and modulation arrive on their exact frame. Passes the CLAP project's validator |
| [`manguebyte-vst3`](crates/manguebyte-vst3) | Exports the same `Plugin` as a VST 3 plugin with one line. Every automation point lands on its frame; notes carry ids, tuning and per-note expression; MIDI controllers arrive as MIDI again. Passes Steinberg's validator on Windows, Linux and macOS |
| [`manguebyte-lv2`](crates/manguebyte-lv2) | Exports the same `Plugin` as an LV2 plugin with one line, and generates its Turtle files. Tested through its C interface in process; not yet checked by `lv2lint` or loaded in a host |
| [`manguebyte-host`](crates/manguebyte-host) | Loads CLAP and VST 3 plugins made by anyone and runs them as processors, through one interface for both formats, with every event on its frame and nothing allocated while processing |
| [`manguebyte-midi`](crates/manguebyte-midi) | MIDI 1.0 channel and system messages, a byte-stream parser, standard MIDI files, sample-accurate `MidiBuffer`; MIDI 2.0 packets (UMP) and translation between the protocols; RPN and NRPN; MPE zones and note tracking; keyboard state |
| [`manguebyte-formats`](crates/manguebyte-formats) | WAV (with RF64), AIFF and FLAC, read and written, whole or block by block with exact seeking; a FLAC encoder and decoder written from the specification and checked against FFmpeg; wait-free bridges to play and record from an audio callback |
| [`manguebyte-osc`](crates/manguebyte-osc) | Open Sound Control over UDP and TCP, with a real-time bridge that puts time-tagged bundles on their exact frame |
| [`manguebyte-devices`](crates/manguebyte-devices) | Device listing and selection, playback, recording and full-duplex streams, in any hardware sample format; MIDI input and output ports, with arrival times mapped to frames so a player's timing survives the audio buffer |
| [`manguebyte-gfx`](crates/manguebyte-gfx) | 2D drawing on the CPU: paths, gradients, images, clips and layers recorded as a scene with an exact text dump; repaint limited to what changed, identical to a full repaint; text shaped and laid out with an embedded Inter subset |
| [`manguebyte-ui`](crates/manguebyte-ui) | Widgets for plugin editors: a retained tree, row and column layout, theme tokens, label, knob, slider, toggle, meter and canvas, parameter edits as gestures, an accessibility description required of every widget, and a harness that drives it all in a unit test |
| [`manguebyte-window`](crates/manguebyte-window) | Windows for plugin editors: a child of the host's window on Win32, presenting only what changed, and a headless backend that lets a test drive a window on any system |
| [`manguebyte-gui`](crates/manguebyte-gui) | The `Editor` a plugin returns for the CLAP and VST 3 wrappers to open: a widget tree in a child of the host's window, on Win32 so far, tested through both formats' real interfaces |
| [`manguebyte-standalone`](crates/manguebyte-standalone) | Runs a `Plugin` as an application: sound card out, MIDI keyboards in, microphone in for effects |
| [`cargo-mangue`](crates/cargo-mangue) | The command-line tool: `cargo mangue new` creates a plugin crate, `bundle`, `install` and `validate` turn it into a `.clap` and a `.vst3` |

Not there yet: an editor opened inside a real host. The editor interfaces
of both formats, the renderer, the widget set, windows on Win32 and the glue
that opens an editor in them exist and are tested
([ADR 0007](docs/adr/0007-user-interface-layer.md)); an editor for the
example effect and a test with two plugin binaries in one process are being
built, and windows on macOS and Linux come after. The LV2 wrapper is tested
in process, but nothing writes an `.lv2` bundle yet, so no LV2 host has
loaded a plugin made with it and `lv2lint` has not checked one. Also missing:
the Audio Unit and AAX formats; lossy formats (MP3, Ogg Vorbis, Opus);
plugin scanning and the editors of hosted plugins.

## Quick start

Manguebyte is not on crates.io yet. Depend on it straight from Git:

```toml
[dependencies]
manguebyte = { git = "https://github.com/alessonqueirozdev-hub/manguebyte" }
```

Build a synth voice by putting processors in a tuple, then render it to a WAV
file:

```rust,no_run
use manguebyte::prelude::*;

fn main() -> Result<(), wav::WavError> {
    let spec = ProcessSpec { sample_rate: 48_000.0, max_block_size: 48_000, num_channels: 2 };

    // A tuple of processors is itself a processor.
    let mut voice = (
        Oscillator::new(Waveform::Saw, note_to_hz(45.0)),
        Filter::new(FilterShape::Lowpass, 1_200.0, 1.5),
        Gain::from_db(-12.0),
    );
    voice.prepare(&spec);

    let mut second = AudioBuffer::<f32>::new(2, 48_000);
    voice.process(&mut second);

    wav::write_file("saw.wav", &second, 48_000, WavFormat::Int16)
}
```

Play it live instead:

```rust,no_run
use manguebyte::prelude::*;

fn main() -> Result<(), DeviceError> {
    let device = OutputDevice::open_default()?;
    let spec = ProcessSpec {
        sample_rate: device.sample_rate(),
        max_block_size: 512,
        num_channels: device.num_channels(),
    };

    let mut voice = (Oscillator::new(Waveform::Sine, 440.0), Gain::<f32>::from_db(-18.0));
    voice.prepare(&spec);

    // The closure runs on the audio thread.
    let _stream = device.start(512, move |buffer| voice.process(buffer))?;
    std::thread::sleep(std::time::Duration::from_secs(2));
    Ok(())
}
```

## Examples

```bash
# Render a short synth riff to a WAV file. Needs no audio hardware.
cargo run --example render_wav -- riff.wav

# Play the same riff on your speakers. The optional argument is a level in dB.
cargo run --example play_riff -- -12
```

On Linux the device backend needs the ALSA development headers
(`sudo apt install libasound2-dev` on Debian and Ubuntu).

## Why "Manguebyte"?

The name comes from Recife, the city in the north-east of Brazil where this
project was born.

Recife is built on mangroves, *mangue* in Portuguese: tidal forests where
rivers meet the sea, and one of the most fertile ecosystems on Earth. In the
early 1990s the city was in deep economic trouble, and a group of local
musicians answered with a manifesto called *Caranguejos com Cérebro*,
"Crabs with Brains". Its proposal was to plug the rich, muddy roots of the
region's culture into the global network of ideas. The image they chose was
a satellite dish stuck in the mangrove mud.

The movement was called **mangue bit**: *mangue* for the mangrove, *bit* for
the unit of digital information. The press heard "beat", and the world came to
know it as **manguebeat**. Bands such as Chico Science & Nação Zumbi and Mundo
Livre S/A mixed maracatu, coco and ciranda with rock, funk, hip-hop and
electronic music, and changed Brazilian music for good.

This project borrows that idea, and the name fits in more ways than one:

- **Mangue** is where we come from, and it is the home of the crab, which is
  both the symbol of the movement and, as it happens, the mascot of Rust.
- **Byte** is the bit grown up: eight of them, the unit that software is
  actually written in. It is our nod to *mangue bit*, one step further on.
- It is a **music** name for a music framework, taken from a movement that
  was about connecting local roots to global technology. That is what open
  source does.

So the logo is a crab with headphones and a guitar: a crab with brains, making
noise.

## Building a plugin

[`plugins/pluck`](plugins/pluck) is a complete instrument in one file: a
16-voice plucked synth with per-note expression and modulation, automation
on the exact sample, and a reverb. It becomes a CLAP plugin and a VST 3
plugin with one line each, from the same library:

```rust,ignore
manguebyte::clap::export_clap!(Pluck);
manguebyte::vst3::export_vst3!(Pluck);
```

Build it and bundle it as `.clap` and `.vst3` for any host, with the
framework's command-line tool:

```bash
cargo run -p cargo-mangue -- bundle -p mangue-pluck --release
```

`cargo mangue new my-synth --kind instrument` starts a plugin of your own
from a template; `install` copies a plugin into the user's plugin folders,
and `validate` runs both formats' validators on it.

[`plugins/duck`](plugins/duck) is the same for an effect: a sidechain ducker
with a main input, a sidechain input and a bypass switch that does not
click. Both examples go through the CLAP project's validator and Steinberg's
on every commit.

Or play the instrument with no host, straight on the sound card, with any
connected MIDI keyboard (the argument is the level in decibels):

```bash
cargo run -p mangue-pluck --features standalone --example play -- -12 hold
```

The same plugin runs inside a unit test with no host at all, through
`Harness`, which is how its pitch, its envelope and its independence from the
host's block size are checked on every commit.

## Design

- [docs/BEYOND_JUCE.md](docs/BEYOND_JUCE.md): what Manguebyte does that JUCE
  does not, how each claim is tested, and where JUCE is still ahead.
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): how the crates fit together,
  which thread runs what, and what the design learns from JUCE's history.
- [docs/JUCE_PARITY.md](docs/JUCE_PARITY.md): JUCE's 24 modules, one by one,
  with what Manguebyte has for each and where it intends to go further.
- [docs/TECH_DEBT.md](docs/TECH_DEBT.md): what the code owes, scored and in
  the order it will be paid back.
- [docs/adr/](docs/adr): the record of individual design decisions.

## The Manguebyte ecosystem

Manguebyte is the sound engine of two projects built on it, both from
Recife:

- **[Aratu](https://github.com/alessonqueirozdev-hub/aratu)**: a live-coding
  language that reads like music, with measures, beats and note values first,
  in Portuguese and English, in the browser and inside a DAW. In development:
  the language reads, checks and schedules every example in its manual, and
  its web editor runs in the browser with nothing to install
  ([try it](https://alessonqueirozdev-hub.github.io/aratu/)), playing with
  temporary built-in sounds that Manguebyte's engine will replace.
- **Parabolic** (planned): a tool for musicians to record their own
  instruments and turn them into plugins and samplers without writing code.

## Contributing

Manguebyte is built in the open and contributions are welcome, from typo
fixes to whole new crates. Read [CONTRIBUTING.md](CONTRIBUTING.md) to get
started, and [ROADMAP.md](ROADMAP.md) to see where help is most needed.

One rule matters above the others: **Manguebyte is a clean-room project.** It
is inspired by what JUCE makes possible, but it contains no JUCE code and must
never contain any. Please do not copy or translate source code from JUCE or
from any other project whose license is not compatible with ours.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

`manguebyte-gfx` embeds a subset of the [Inter](https://rsms.me/inter/)
typeface, which is under the SIL Open Font License 1.1
([crates/manguebyte-gfx/fonts/OFL.txt](crates/manguebyte-gfx/fonts/OFL.txt)).
A program that ships `manguebyte-gfx` ships the typeface, so it has to ship
that licence too: as a file beside the program, or on an about screen through
`manguebyte_gfx::TYPEFACE_LICENCE`.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this project by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

## Disclaimer

Manguebyte is an independent project. It is not affiliated with, sponsored by
or endorsed by the makers of JUCE. JUCE is a trademark of its respective owner
and is mentioned here only to describe the kind of framework this project
aims to be.
