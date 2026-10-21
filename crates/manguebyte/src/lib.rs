//! The Manguebyte Audio Framework: real-time-safe audio for Rust.
//!
//! This crate gathers the whole framework under one name. Each area lives in
//! its own crate, which you can also depend on directly:
//!
//! | Module | Crate | What it holds |
//! |---|---|---|
//! | (root) | `manguebyte-core` | [`AudioBuffer`], [`Sample`], [`Smoothed`], atomics, [`spsc`] queue, [`NoDenormals`], [`source`] (pulled audio sources) |
//! | [`dsp`] | `manguebyte-dsp` | processor chains, sample-accurate events, MIDI, MPE and MIDI 2.0 read into per-note events, audio sources built from sources, polyphonic synth, instruments, filters, effects, dynamics with sidechain keying, convolution, wavetables, FFT, response curves for drawing |
//! | [`params`] | `manguebyte-params` | thread-safe parameters, ranges, smoothing, saved state |
//! | [`graph`] | `manguebyte-graph` | a processing graph that can be rewired while it plays |
//! | [`plugin`] | `manguebyte-plugin` | the format-independent plugin API and a test harness |
//! | [`midi`] | `manguebyte-midi` | MIDI 1.0 and 2.0 messages, stream parsing, MIDI files, MPE, MIDI-CI, sample-accurate event buffers |
//! | [`formats`] | `manguebyte-formats` | audio file reading and writing, metadata, waveform thumbnails; AAC, ALAC and WMA through the system's codecs (feature `platform-codecs`); streamed playback with a transport |
//! | `clap` | `manguebyte-clap` | builds CLAP plugins (feature `clap`) |
//! | `vst3` | `manguebyte-vst3` | builds VST 3 plugins (feature `vst3`) |
//! | `lv2` | `manguebyte-lv2` | builds LV2 plugins and their Turtle files (feature `lv2`) |
//! | `au` | `manguebyte-au` | builds Audio Units for macOS and their `Info.plist` (feature `au`) |
//! | `gui` | `manguebyte-gui` | opens a plugin's editor in the host's window (feature `gui`) |
//! | `ui` | `manguebyte-ui` | the widgets editors are made of (feature `gui`) |
//! | `sampler` | `manguebyte-sampler` | an SFZ sampler, from files or from memory (feature `sampler`) |
//! | `host` | `manguebyte-host` | loads CLAP, VST 3, LV2 and Audio Unit plugins and runs them as processors, and scans for them out of process (feature `host`) |
//! | `analysis` | `manguebyte-analysis` | loudness, true peak, level meters, spectrum and pitch (feature `analysis`, on by default) |
//! | `osc` | `manguebyte-osc` | Open Sound Control, with time tags landing on their frame (feature `osc`, on by default) |
//! | `standalone` | `manguebyte-standalone` | runs a plugin on the sound card and MIDI ports (feature `standalone`) |
//! | [`devices`] | `manguebyte-devices` | audio hosts, device input, output and duplex streams, a device manager with saved setups and hot-plug, MIDI ports (feature `devices`, on by default) |
//!
//! # Example
//!
//! Render a filtered sawtooth and save it as a WAV file:
//!
//! ```
//! use manguebyte::prelude::*;
//!
//! let spec = ProcessSpec { sample_rate: 48_000.0, max_block_size: 480, num_channels: 2 };
//! let mut voice = (
//!     Oscillator::new(Waveform::Saw, note_to_hz(45.0)),
//!     Filter::new(FilterShape::Lowpass, 1_200.0, 1.5),
//!     Gain::from_db(-12.0),
//! );
//! voice.prepare(&spec);
//!
//! let mut block = AudioBuffer::<f32>::new(2, 480);
//! voice.process(&mut block);
//!
//! let mut file = Vec::new();
//! wav::write(&mut file, &block, 48_000, WavFormat::Int16)?;
//! # Ok::<(), wav::WavError>(())
//! ```

#![forbid(unsafe_code)]

pub use manguebyte_core::log::{DrainedLog, LogLevel, LogQueue, LogRecord};
pub use manguebyte_core::{
    AtomicF32, AtomicF64, AudioBuffer, NoDenormals, ParamId, Sample, Smoothed, decibels, source,
    spsc,
};

#[cfg(feature = "analysis")]
pub use manguebyte_analysis as analysis;
#[cfg(feature = "au")]
pub use manguebyte_au as au;
#[cfg(feature = "clap")]
pub use manguebyte_clap as clap;
#[cfg(feature = "devices")]
pub use manguebyte_devices as devices;
pub use manguebyte_dsp as dsp;
pub use manguebyte_formats as formats;
pub use manguebyte_graph as graph;
#[cfg(feature = "gui")]
pub use manguebyte_gui as gui;
#[cfg(feature = "host")]
pub use manguebyte_host as host;
#[cfg(feature = "lv2")]
pub use manguebyte_lv2 as lv2;
pub use manguebyte_midi as midi;
#[cfg(feature = "osc")]
pub use manguebyte_osc as osc;
pub use manguebyte_params as params;
pub use manguebyte_plugin as plugin;
#[cfg(feature = "sampler")]
pub use manguebyte_sampler as sampler;
#[cfg(feature = "standalone")]
pub use manguebyte_standalone as standalone;
#[cfg(feature = "gui")]
pub use manguebyte_ui as ui;
#[cfg(feature = "vst3")]
pub use manguebyte_vst3 as vst3;

/// The names most programs need, in one import.
pub mod prelude {
    pub use crate::dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
    pub use crate::dsp::note_input::{Mpe, NoteInput};
    pub use crate::dsp::sources::{
        ChannelRemapSource, MixerSource, ProcessedSource, ResamplingSource, SourceProcessor,
    };
    pub use crate::dsp::{
        Adsr, AdsrParams, Biquad, BiquadCascade, BiquadCoefficients, Chorus, Compressor,
        Convolution, ConvolutionEngine, ConvolutionLoader, ConvolutionRouting, ConvolutionSettings,
        Crossover, CrossoverSlope, DcBlocker, Delay, DelayInterpolation, DelayLine, DryWet,
        EnvelopeFollower, Expander, Filter, FilterShape, FirFilter, FirMode, Gain, IirFilter,
        KeySource, LadderFilter, LadderMode, LevelDetector, Lfo, LfoShape, Limiter, MixLaw, Noise,
        NoiseColor, NoiseGate, OnePole, OnePoleMode, Oscillator, Oversampled, PanMode, Panner,
        Phaser, ProcessContext, ProcessSpec, Processor, Resampler, Reverb, ShaperCurve,
        StateVariableFilter, SvfMode, Synth, Transport, Voice, Waveform, Waveshaper, Wavetable,
        WavetableOscillator, resample, split_at_events, split_at_events_with,
    };
    pub use crate::formats::aiff::{self, AiffFormat};
    pub use crate::formats::wav::{self, WavFormat};
    pub use crate::formats::{
        AudioFile, AudioReader, AudioWriter, FileFormat, FormatError, RecordingWriter,
        StreamingPlayer, TransportControl, TransportSource, flac, mp3, vorbis,
    };
    pub use crate::graph::{Graph, GraphEngine, GraphError, NodeId};
    pub use crate::midi::file::{MetaEvent, MidiFile, Track};
    pub use crate::midi::{
        MidiBuffer, MidiEvent, MidiMessage, StreamEvent, StreamParser, SystemMessage, hz_to_note,
        note_to_hz,
    };
    pub use crate::params::{
        BoolParam, Changed, ChoiceParam, FloatParam, FloatRange, IntParam, Param, ParamSet,
        ParamSmoother, ParamState, Smoothing, param_set,
    };
    pub use crate::plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};
    pub use crate::source::{AudioSource, BufferSource, PositionableSource};
    pub use crate::{
        AtomicF32, AtomicF64, AudioBuffer, NoDenormals, ParamId, Sample, Smoothed, decibels, spsc,
    };

    #[cfg(feature = "devices")]
    pub use crate::devices::{
        AudioHost, CallbackInfo, DeviceError, DeviceInfo, DeviceIo, DeviceManager, DeviceSetup,
        DuplexStream, InputDevice, InputStream, OutputDevice, OutputStream, StreamSettings,
        input_devices, output_devices,
    };
}

/// Compiles the code blocks of the README as tests, so they cannot go stale.
#[cfg(all(doctest, feature = "devices"))]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;
