//! An SFZ sampler.
//!
//! [SFZ](https://sfzformat.com) describes an instrument as plain text: which
//! sample plays for which keys and velocities, and how. This crate reads
//! that text ([`Sfz`]), loads the samples it names from wherever the caller
//! keeps them ([`SampleSource`]: a directory, or bytes in memory when there
//! is no file system, as in a web page), and plays the result
//! ([`Instrument`]) with a [`Sampler`] processor or, one note per voice,
//! with [`SamplerVoice`] inside a [`Synth`](manguebyte_dsp::Synth).
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::NoteRef;
//! use manguebyte_dsp::{ProcessSpec, Processor};
//! use manguebyte_formats::wav::{self, WavFormat};
//! use manguebyte_sampler::{Instrument, MemorySource, Sampler};
//!
//! // A sample: a second of A4 at 44.1 kHz, as a WAV file in memory.
//! let mut tone = AudioBuffer::<f32>::new(1, 44_100);
//! for (n, sample) in tone.channel_mut(0).iter_mut().enumerate() {
//!     *sample = (std::f64::consts::TAU * 440.0 * n as f64 / 44_100.0).sin() as f32;
//! }
//! let mut bytes = Vec::new();
//! wav::write(&mut bytes, &tone, 44_100, WavFormat::Float32).unwrap();
//! let mut source = MemorySource::new().with("piano/a4.wav", bytes);
//!
//! let instrument = Instrument::from_sfz(
//!     "<control> default_path=piano/
//!      <region> sample=a4.wav pitch_keycenter=69 lokey=57 hikey=81 ampeg_release=0.2",
//!     &mut source,
//! )?;
//!
//! // A sampler that starts with this instrument; its loader would send
//! // others.
//! let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 32);
//! sampler.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 512, num_channels: 2 });
//! sampler.note_on(NoteRef::new(0, 72), 0.8); // C5: the A4 sample, a minor third up
//! let mut block = AudioBuffer::<f32>::new(2, 512);
//! sampler.process(&mut block);
//! assert!(block.peak() > 0.1);
//! # Ok::<(), manguebyte_sampler::InstrumentError>(())
//! ```
//!
//! # Threads
//!
//! Parsing and loading read files, decode audio and allocate, so they run
//! off the audio thread. The finished [`Instrument`] is sent whole through
//! a [`SamplerLoader`]; the [`Sampler`] takes it at the start of a block,
//! and sends the one it replaces back to the loader, which frees it. The
//! audio thread never allocates, frees, locks or waits, and rendering does
//! not panic; `tests/realtime.rs` checks the allocator with every feature
//! in use.
//!
//! # Opcodes
//!
//! Headers: `<control>`, `<global>`, `<master>`, `<group>` and `<region>`,
//! each region inheriting from the global, master and group above it.
//! `<curve>`, `<effect>`, `<midi>` and `<sample>` are reported and their
//! contents ignored. Directives: `#define $NAME value` and
//! `#include "file"` (with a source). In `<control>`: `default_path`,
//! `note_offset` and `octave_offset` (both shift incoming notes), and
//! `set_ccN`, `set_hdccN` and `label_ccN` (see [Controllers](#controllers)).
//!
//! | Opcodes | What they do here |
//! |---|---|
//! | `sample` | The sample's path, or `*sine` (sounds the key's pitch) or `*silence`. |
//! | `lokey`, `hikey`, `key`, `lovel`, `hivel`, `lochan`, `hichan` | The notes that play the region. Keys may be numbers or names, with `c4` = 60; `hikey=-1` or `key=-1` (SFZ 2) keeps the region off the keyboard. |
//! | `pitch_keycenter`, `pitch_keytrack`, `transpose`, `tune` | Pitch: `(key - pitch_keycenter) * pitch_keytrack + 100 * transpose + tune` cents. `pitch_keycenter=sample` takes the WAV file's root key. |
//! | `bend_up`, `bend_down` | Cents at the ends of the pitch wheel. |
//! | `seq_length`, `seq_position` | Round robin. Each region counts the notes that match its keys, velocities, channels and trigger, and plays on its turn. |
//! | `lorand`, `hirand` | Each note draws one number in `0..1` from the instrument's seeded generator; a region plays when it falls in `lorand..hirand`. |
//! | `trigger` | `attack`, `release` (waits for the pedal), `release_key` (does not), `first`, `legato`. A release-triggered region plays with its note-on's velocity and never loops. |
//! | `mangue_loheld`, `mangue_hiheld` | Ours, as no SFZ opcode does it (`lotimer` and `hitimer` count from the last sample of a group, and ARIA does not play them): a release-triggered region plays only for a note held from `mangue_loheld` seconds (0 by default) up to, not including, `mangue_hiheld` (no limit by default), so that an organ's short and long releases follow how long the key was down. |
//! | `mangue_release_crossfade`, `mangue_release_align` | Ours too. A release-triggered region with `mangue_release_crossfade` (seconds) starts with an equal-power crossfade from the note's loudest sustaining voice: the sustain takes back its own release, keeps looping and falls as a cosine while the release rises as a sine, and then stops. With `mangue_release_align` (frames, up to 4096), the release starts where, within that many frames after its `offset`, its next 128 frames best match the sustain's next 128 (by normalised cross-correlation, worked out on the audio thread at the key's release: at most 4096 times 128 products), with the sustain's fraction of a frame, so that it carries the loop's phase on. Without a sustaining voice, the release starts at its `offset` at full level. |
//! | `volume`, `amplitude`, `pan`, `amp_veltrack` (also read as `ampeg_veltrack`), `rt_decay` | Gain. Velocity gives `1 - t + t * (v / 127)²` for `amp_veltrack` = `100 * t`; `rt_decay` takes decibels per second held off a release-triggered region. Mono samples pan at constant power, unity in the centre; stereo samples are balanced. |
//! | `offset`, `end` | The frames played; `end` is included, and `end=-1` silences the region (it still chokes). The interpolator hears silence outside them, so a region cut from a longer file never hears its neighbours. |
//! | `loop_mode`, `loop_start`, `loop_end`, `loop_crossfade` | `no_loop`, `one_shot`, `loop_continuous`, `loop_sustain`. Without them a WAV file's `smpl` loop is used, continuously. Loop points are read through the loop by the interpolator, so the join adds nothing; `loop_crossfade` fades the loop's last frames into those before its start, linearly in amplitude. A released `loop_sustain` leaves its loop at the first frame that reads the same with the loop as without it, so the release never clicks, even inside the crossfade. |
//! | `ampeg_delay`, `ampeg_start`, `ampeg_attack`, `ampeg_hold`, `ampeg_decay`, `ampeg_sustain`, `ampeg_release`, `ampeg_vel2*` | The amplitude envelope: a straight-line attack, a decay straight in decibels, and a release that falls 90 dB in its time. |
//! | `group`, `off_by`, `off_mode`, `off_time`, `polyphony` | Choke groups and voice limits. `polyphony` counts the voices of the region's `group` when a `group` opcode applies to it, and otherwise those of the regions under the header where it is written. |
//! | `cutoff`, `fil_type` (also `filtype`), `resonance` | The filter, on only when `cutoff` (in hertz) is given. Types played: `lpf_1p`, `hpf_1p`, `lpf_2p` (the default), `hpf_2p`, `bpf_2p`, `brf_2p` (with their `_sv` spellings), `lpf_4p`, `hpf_4p`, `lpf_6p`, `hpf_6p`; see [`FilterType`]. Any other type is reported and the region plays unfiltered. `resonance` (0 to 40 dB) raises the gain at the cutoff by its decibels; the one-pole types have none. |
//! | `fil_keytrack`, `fil_keycenter`, `fil_veltrack` | Cents the cutoff moves per key from `fil_keycenter` (60 by default), and at velocity 127 (in proportion below it). |
//! | `fileg_delay`, `fileg_start`, `fileg_attack`, `fileg_hold`, `fileg_decay`, `fileg_sustain`, `fileg_release`, `fileg_vel2*`, `fileg_depth`, `fileg_vel2depth` | The filter envelope, shaped as the amplitude envelope is (every stage 0 by default, the sustain included), moving the cutoff by its level times `fileg_depth` cents, plus `fileg_vel2depth` at velocity 127. It is released with the key. |
//! | `<opcode>_onccN` (also `<opcode>_ccN`, and SFZ 1's `ampeg_<stage>ccN`) | Controller `N` (0 to 511) modulates `amplitude` (percent), `volume` or `gain` (dB), `pan` (percent), `tune` or `pitch` (cents), `cutoff` (cents), `resonance` (dB), `fileg_depth` (cents), and each stage of `ampeg_*` (also `amp_*`) and `fileg_*` (seconds or percent): see [Controllers](#controllers). |
//!
//! Every other opcode is kept in [`Region::unknown`] and reported as a
//! [`Warning`]; values outside an opcode's range are clamped with a
//! warning; text that cannot be read is an [`SfzError`] naming its line.
//! Inherited opcodes are shared by the regions that inherit them, and a
//! parse refuses text whose regions would take more than
//! [`sfz::MAX_REGION_OPCODES`] opcodes in all, so the memory a parse holds
//! grows with the text (after `#define` replacement) and not with the
//! product of inherited opcodes and the regions that inherit them.
//!
//! Sample paths keep a leading `..`, so `../Samples/a.wav` (or
//! `default_path=../Samples/`) reads from beside the `.sfz` file's
//! directory with a [`FileSource`]; a [`MemorySource`] finds such a path
//! under the same name, `../` included.
//!
//! # Controllers
//!
//! A [`Sampler`] keeps [`MAX_CONTROLLERS`] controllers, each with a value
//! from 0 to 1, shared by every MIDI channel. Controllers 0 to 119 take
//! the value of MIDI control-change messages (divided by 127); every
//! controller, those from 128 to 511 included, takes the values a plugin
//! sets with [`Sampler::set_controller`], on the frame it names, without
//! a MIDI message. No MIDI message reaches a controller above 127 (SFZ 2's
//! extended controllers), so a plugin that binds its parameters there
//! never collides with a musician's controllers. SFZ 2 gives 128 to 143
//! meanings of their own (pitch wheel, aftertouch, velocity and others),
//! which this sampler does not compute: they hold whatever is set, so a
//! plugin's own parameters are best put from 256 up.
//!
//! Each controller starts at its `set_ccN` value (0 to 127, divided by
//! 127) or `set_hdccN` value (0 to 1), or at 0. A change, from either
//! source, moves the value along a straight line over
//! [`CONTROLLER_SMOOTHING_SECONDS`], so a jump never clicks. When a new
//! instrument arrives, controllers not set since the sampler was made or
//! last reset take its initial values; [`reset`](manguebyte_dsp::Processor::reset)
//! puts every one back to them.
//!
//! A modulation `<opcode>_onccN=depth` adds `depth` times controller
//! `N`'s value to the opcode, in the opcode's unit; several on one opcode
//! add up, and a later one for the same opcode and controller replaces the
//! earlier, as inherited opcodes are replaced. A voice reads its
//! controllers every 16 frames, and its gains and filter glide from one
//! reading to the next, while its pitch steps; volume, amplitude, pan,
//! pitch, cutoff, resonance and `fileg_depth` follow the controller while
//! the note plays. Envelope stages follow it as SFZ players do: each
//! stage's time is read when the stage starts, the start level when the
//! attack starts and the sustain level when the decay starts, so moving a
//! controller changes the stages to come and not the one under way. The
//! amplitude never goes below 0, the pan is held to -100..100, the
//! resonance to 0..40 dB, the cutoff between 1 Hz and 0.49 of the sample
//! rate, times to 0 or more and levels to 0..100 %.
//!
//! [`SamplerVoice`] sees no controller messages: its controllers stay at
//! their initial values.
//!
//! # Filters
//!
//! Each voice of a region with a `cutoff` runs its sample through a filter
//! of the region's [`FilterType`] before the amplitude envelope: the
//! bilinear transforms of Butterworth prototypes, as trapezoidal
//! state-variable sections whose state stays valid however fast the cutoff
//! moves. The cutoff is `cutoff` times two to the power of the cents of
//! `fil_keytrack`, `fil_veltrack`, the filter envelope and the
//! controllers, over 1200. It is worked out every 16 frames, the filter's
//! settings gliding in a straight line in between, so a sweep has no
//! steps; nothing is allocated, and the cutoff is held below Nyquist at
//! every sample rate. A mono sample is filtered once, a stereo one per
//! side.
//!
//! # Pitch
//!
//! A voice reads its sample at a position kept to 32 bits of fraction, so
//! pitch is exact to far better than a thousandth of a cent, and between
//! frames with the [`Interpolation`] chosen: a windowed sinc by default,
//! whose measured quality and cost are stated there. A sample played at its
//! own pitch and rate is read as it is.
//!
//! # Streaming from disk
//!
//! [`Instrument::load_streaming`] keeps in memory only the head of each
//! sample and its loops, and reads the rest while notes play, on reader
//! threads, into a lock-free ring per voice; [`StreamOptions`] sets the
//! preload, the rings, the voices, the threads and a memory budget. The
//! audio thread never waits for the disk: a voice whose frames have not
//! arrived fades out over 6 ms and is counted
//! ([`Instrument::stream_underruns`]). With a disk that keeps up, what
//! plays is the same, bit for bit, as from memory. A player with voices of
//! its own streams through the same layer with a [`SampleStore`] and a
//! [`StreamCursor`] per voice. See ADR 0013.

#![forbid(unsafe_code)]

mod controllers;
mod envelope;
mod filter;
mod instrument;
mod interpolate;
mod packed;
mod player;
mod region;
mod sampler;
pub mod sfz;
mod source;
mod store;
mod stream;
mod voice;

pub use controllers::{CONTROLLER_SMOOTHING_SECONDS, MAX_CONTROLLERS};
pub use instrument::{
    DEFAULT_SEED, Instrument, InstrumentBuilder, InstrumentError, LoadError, MAX_SAMPLE_FRAMES,
};
pub use interpolate::Interpolation;
pub use region::{
    EnvelopeSpec, EnvelopeStage, FilterType, KeyCenter, LoopMode, MAX_RELEASE_ALIGN, Modulation,
    ModulationTarget, OffMode, Region, Trigger,
};
pub use sampler::{
    MAX_HELD_NOTES, MAX_LOADER_CAPACITY, MAX_PENDING_CONTROLLER_CHANGES, MAX_POLYPHONY, Sampler,
    SamplerLoader,
};
pub use sfz::{Control, Location, Opcode, Sfz, SfzError, SfzErrorKind, Warning, WarningKind};
pub use source::{FileSource, MemorySource, SampleSource, normalize_path};
pub use store::{CursorRead, SampleHandle, SampleStore, StoreError, StreamCursor};
pub use stream::{
    DEFAULT_LOOP_CACHE_FRAMES, DEFAULT_PRELOAD_FRAMES, DEFAULT_RING_FRAMES, DEFAULT_STREAM_VOICES,
    ReaderStart, StreamOptions,
};
pub use voice::{MAX_LAYERS, SamplerVoice};

/// Makes the tables every instrument shares, if they are not made yet: the
/// windowed-sinc kernel of [`Interpolation::Sinc`], 512 phases of 64 taps
/// and the curve they come from, about 256 KiB. They are made once per
/// program, the first time an instrument is loaded, and kept until it ends;
/// making them takes a few milliseconds. Call this where that does not
/// matter, so that no load has to: `manguebyte-web` does when an engine is
/// made, since it loads instruments on a browser's audio thread, between
/// render quanta.
pub fn prepare_tables() {
    drop(interpolate::SincTable::shared());
}
