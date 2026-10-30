//! The plugin API: write a processor once and run it anywhere.
//!
//! A [`Plugin`] is a set of parameters plus a function that processes audio
//! and events. It knows nothing about any plugin format. Format wrappers
//! (CLAP, VST3 and the others), the standalone application wrapper and the
//! test [`Harness`] all drive the same trait, so what is tested here is what
//! runs in a host.
//!
//! ```
//! use std::sync::Arc;
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! use manguebyte_params::{FloatParam, FloatRange, param_set};
//! use manguebyte_plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};
//!
//! struct GainParams { gain: FloatParam }
//! param_set!(GainParams { gain });
//!
//! struct Gain { params: Arc<GainParams> }
//!
//! impl Plugin for Gain {
//!     const INFO: PluginInfo = PluginInfo {
//!         id: "org.example.gain",
//!         name: "Gain",
//!         vendor: "Example",
//!         version: "1.0.0",
//!         description: "Turns things up and down",
//!         kind: PluginKind::Effect,
//!     };
//!     const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
//!     type Params = GainParams;
//!
//!     fn new() -> Self {
//!         let range = FloatRange::linear(0.0, 2.0);
//!         Self { params: Arc::new(GainParams { gain: FloatParam::new("gain", "Gain", range, 1.0) }) }
//!     }
//!     fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
//!     fn prepare(&mut self, _: &ProcessSpec) {}
//!     fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
//!         buffer.apply_gain(self.params.gain.value());
//!     }
//! }
//!
//! // No host needed to try it out.
//! let mut harness = Harness::<Gain>::new(48_000.0, 256);
//! harness.params().gain.set(0.5);
//!
//! let mut input = AudioBuffer::<f32>::new(2, 100);
//! input.channel_mut(0).fill(1.0);
//! let output = harness.process(&input);
//! assert_eq!(output.channel(0), &[0.5; 100]);
//! ```
//!
//! A plugin can also offer an editor, through [`Plugin::editor`] and the
//! contract in the [`editor`] module.
//!
//! # Identity
//!
//! What hosts know a plugin by, its id, name, vendor and version, is
//! [`Plugin::INFO`], fixed when it is compiled, unless the plugin works it
//! out when its library is loaded, in [`Plugin::info`]: one engine, built
//! once, can then be copied to many files, each a plugin of its own with
//! its identity in data the copy carries, which it finds through
//! [`library_path`]. Everything that reports the identity reads it through
//! [`plugin_info`], which asks `info` once and keeps the answer.
//!
//! # MIDI controllers
//!
//! An instrument or a note effect states which MIDI controllers it listens
//! to in [`Plugin::MIDI_CONTROLLERS`]. Most need do nothing: the default,
//! [`MidiControllers::MUSICAL`], covers the pitch wheel, channel pressure,
//! the usual performance controllers and pedals, and the channel mode
//! messages. The declaration matters to VST 3, which cannot pass MIDI
//! controllers through and gives the plugin one hidden parameter for each
//! one declared; formats that carry MIDI as it is ignore it. See
//! [`Plugin::MIDI_CONTROLLERS`] for the details.
//!
//! # Sending notes out
//!
//! A plugin that plays notes, such as an arpeggiator or a sequencer, sets
//! [`Plugin::NOTE_OUTPUT`] and writes its notes, per-note expressions and
//! MIDI 1.0 and 2.0 messages from [`Plugin::process_with_output`]. The
//! [`output`] module has the types, and the translation to MIDI 2.0
//! packets, MIDI 1.0 and MPE that ports and hosts need.
//!
//! # More than one output
//!
//! A plugin whose parts a host should mix apart, such as the divisions of
//! an organ or the pads of a drum machine, lists auxiliary output buses
//! in [`Plugin::AUX_OUTPUTS`] and writes them from
//! [`Plugin::process_with_aux`], where a bus the host switched off is not
//! given, so it costs nothing to leave silent.

#![forbid(unsafe_code)]

mod buses;
mod controllers;
pub mod editor;
mod harness;
mod identity;
pub mod output;
mod state;

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
pub use manguebyte_dsp::note_input::Mpe;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::ParamSet;

pub use buses::{AuxOutput, AuxOutputs};
pub use controllers::{MidiController, MidiControllers};
use editor::EditorSpec;
pub use harness::Harness;
pub use identity::{is_valid_id, library_path, plugin_info, set_library_path};
use output::OutputBuffer;
pub use state::{StateError, load_state, save_state};

/// What sort of plugin this is, so a host can list it in the right place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginKind {
    /// Makes sound from notes.
    Instrument,
    /// Changes sound.
    Effect,
    /// Changes or generates notes without touching sound.
    NoteEffect,
    /// Measures sound without changing it.
    Analyzer,
}

/// The facts that identify a plugin to hosts and people.
///
/// A plugin gives them as [`Plugin::INFO`], at compile time, and may work
/// them out at run time in [`Plugin::info`]; [`plugin_info`] gives what the
/// wrappers report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginInfo {
    /// A globally unique identifier in reverse-domain form, such as
    /// `"com.example.reverb"`. Hosts store it in sessions, so it must never
    /// change once a version has shipped. It must pass [`is_valid_id`]:
    /// not empty, with no white space or control characters. VST 3's class
    /// id, an Audio Unit's codes and the LV2 URI are all derived from it.
    pub id: &'static str,
    /// The name shown to people.
    pub name: &'static str,
    /// Who makes it.
    pub vendor: &'static str,
    /// Its version, such as `"1.2.0"`.
    pub version: &'static str,
    /// One line about what it does.
    pub description: &'static str,
    /// What sort of plugin it is. Always [`Plugin::INFO`]'s: see
    /// [`Plugin::info`].
    pub kind: PluginKind,
}

/// How many channels a plugin reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioLayout {
    /// Channels of the main input. Zero for an instrument.
    pub inputs: usize,
    /// Channels of the main output.
    pub outputs: usize,
    /// Channels of the sidechain input. Zero if there is none.
    pub sidechain: usize,
}

impl AudioLayout {
    /// Stereo in, stereo out.
    pub const STEREO_EFFECT: Self = Self {
        inputs: 2,
        outputs: 2,
        sidechain: 0,
    };
    /// Mono in, mono out.
    pub const MONO_EFFECT: Self = Self {
        inputs: 1,
        outputs: 1,
        sidechain: 0,
    };
    /// No input, stereo out.
    pub const STEREO_INSTRUMENT: Self = Self {
        inputs: 0,
        outputs: 2,
        sidechain: 0,
    };
    /// No input, mono out.
    pub const MONO_INSTRUMENT: Self = Self {
        inputs: 0,
        outputs: 1,
        sidechain: 0,
    };

    /// The same layout with a sidechain of `channels` channels, or none for
    /// zero.
    ///
    /// ```
    /// use manguebyte_plugin::AudioLayout;
    ///
    /// let ducker = AudioLayout::STEREO_EFFECT.with_sidechain(2);
    /// assert_eq!((ducker.inputs, ducker.outputs, ducker.sidechain), (2, 2, 2));
    /// ```
    #[must_use]
    pub const fn with_sidechain(self, channels: usize) -> Self {
        Self {
            sidechain: channels,
            ..self
        }
    }

    /// Whether this layout has the buses of `base`, with any channel
    /// counts: a main input if and only if `base` has one, a main output
    /// if and only if `base` has one, and a sidechain only if `base` has
    /// one. These are the layouts a wrapper can offer in place of
    /// [`Plugin::LAYOUT`], since the formats fix how many buses a plugin
    /// has and let a host change only their widths, or switch a sidechain
    /// off.
    ///
    /// ```
    /// use manguebyte_plugin::AudioLayout;
    ///
    /// let base = AudioLayout::STEREO_EFFECT.with_sidechain(2);
    /// assert!(AudioLayout::MONO_EFFECT.fits(&base));
    /// assert!(AudioLayout::MONO_EFFECT.with_sidechain(1).fits(&base));
    /// // An instrument has no main input; a stereo effect has no sidechain.
    /// assert!(!AudioLayout::STEREO_INSTRUMENT.fits(&base));
    /// assert!(!base.fits(&AudioLayout::STEREO_EFFECT));
    /// ```
    pub const fn fits(&self, base: &Self) -> bool {
        (self.inputs > 0) == (base.inputs > 0)
            && (self.outputs > 0) == (base.outputs > 0)
            && (self.sidechain == 0 || base.sidechain > 0)
    }
}

/// The layouts a wrapper offers a host for `P`: [`Plugin::LAYOUT`] first,
/// then each other entry of [`Plugin::LAYOUTS`] that
/// [`fits`](AudioLayout::fits) it, once each, in their order. Allocates.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessContext, ProcessSpec};
/// use manguebyte_params::param_set;
/// use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, offered_layouts};
///
/// struct NoParams {}
/// param_set!(NoParams {});
/// struct Thru(Arc<NoParams>);
///
/// impl Plugin for Thru {
///     const INFO: PluginInfo = PluginInfo {
///         id: "org.example.thru",
///         name: "Thru",
///         vendor: "Example",
///         version: "1.0.0",
///         description: "Passes audio through",
///         kind: PluginKind::Effect,
///     };
///     const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
///     const LAYOUTS: &'static [AudioLayout] = &[
///         AudioLayout::STEREO_EFFECT,
///         AudioLayout::MONO_EFFECT,
///         // Not offered: an effect without a main input.
///         AudioLayout::STEREO_INSTRUMENT,
///     ];
///     type Params = NoParams;
///     fn new() -> Self { Self(Arc::new(NoParams {})) }
///     fn params(&self) -> Arc<NoParams> { Arc::clone(&self.0) }
///     fn prepare(&mut self, _: &ProcessSpec) {}
///     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
/// }
///
/// assert_eq!(offered_layouts::<Thru>(), [AudioLayout::STEREO_EFFECT, AudioLayout::MONO_EFFECT]);
/// ```
pub fn offered_layouts<P: Plugin>() -> Vec<AudioLayout> {
    let mut offered = vec![P::LAYOUT];
    for layout in P::LAYOUTS {
        if layout.fits(&P::LAYOUT) && !offered.contains(layout) {
            offered.push(*layout);
        }
    }
    offered
}

/// How many voices a polyphonic plugin plays, for a host that shows it or
/// plans per-note modulation around it (CLAP's `voice-info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VoiceInfo {
    /// How many notes sound at once at most, as the plugin is set now.
    pub voices: u32,
    /// The most it could be set to.
    pub capacity: u32,
    /// Whether two notes on the same key and channel can sound at once,
    /// told apart by their note identifiers, so that a host can address
    /// each on its own.
    pub overlapping_notes: bool,
}

/// A name for a key, such as a drum's in a kit (CLAP's `note-name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NoteName {
    /// The name shown in the host's piano roll.
    pub name: &'static str,
    /// The key, `0..=127`.
    pub key: u8,
    /// The channel, `0..=15`, or `None` for every channel.
    pub channel: Option<u8>,
}

/// An audio plugin, independent of any plugin format.
///
/// # Threads
///
/// The plugin value itself lives on the audio thread: [`process`] and
/// [`reset`] are called there and must not allocate, lock, block or panic.
/// [`prepare`] is called while audio is stopped and may take its time.
///
/// Everything another thread needs goes through the parameters, which are
/// shared in an [`Arc`] and are safe to read and write from anywhere. A host
/// saving or restoring state, a user interface moving a control and the
/// audio thread reading a value never need to coordinate. An editor gets
/// what it shares with the plugin once, from [`editor`], and never sees the
/// plugin itself.
///
/// [`process`]: Self::process
/// [`reset`]: Self::reset
/// [`prepare`]: Self::prepare
/// [`editor`]: Self::editor
pub trait Plugin: Send + 'static {
    /// The facts that identify the plugin, fixed when it is compiled.
    ///
    /// The wrappers report [`info`](Self::info) instead, which is this
    /// unless overridden. `INFO` itself is still what is used where a
    /// constant is needed: the [`kind`](PluginInfo::kind), and whatever
    /// reads the plugin's identity in a `const` context.
    const INFO: PluginInfo;
    /// How many channels it reads and writes, unless the host chooses
    /// another of [`LAYOUTS`](Self::LAYOUTS).
    const LAYOUT: AudioLayout;
    /// Every layout the plugin can run with. Just [`LAYOUT`](Self::LAYOUT)
    /// unless overridden.
    ///
    /// The formats fix which buses a plugin has, so a wrapper offers only
    /// the entries that have the buses of `LAYOUT` (see
    /// [`AudioLayout::fits`] and [`offered_layouts`]): a host can make a
    /// bus wider or narrower, as a VST 3 host does through
    /// `setBusArrangements`, a CLAP host through `audio-ports-config` and an
    /// Audio Unit host through the elements' stream formats, and it can
    /// switch a sidechain off (VST 3's `activateBus`, CLAP's
    /// `audio-ports-activation`), which picks the same layout without the
    /// sidechain if it is listed. Whatever is chosen reaches
    /// [`set_layout`](Self::set_layout) before [`prepare`](Self::prepare).
    /// LV2 describes its ports in the bundle and always runs `LAYOUT`.
    ///
    /// ```
    /// use manguebyte_plugin::{AudioLayout, Plugin};
    /// # use std::sync::Arc;
    /// # use manguebyte_core::AudioBuffer;
    /// # use manguebyte_dsp::{ProcessContext, ProcessSpec};
    /// # use manguebyte_params::param_set;
    /// # use manguebyte_plugin::{PluginInfo, PluginKind};
    /// # struct NoParams {}
    /// # param_set!(NoParams {});
    /// /// A compressor that runs in mono or stereo, keyed by a sidechain
    /// /// or by its own input.
    /// struct Compressor { channels: usize, keyed: bool, params: Arc<NoParams> }
    ///
    /// impl Plugin for Compressor {
    /// #   const INFO: PluginInfo = PluginInfo {
    /// #       id: "org.example.compressor", name: "Compressor", vendor: "Example",
    /// #       version: "1.0.0", description: "", kind: PluginKind::Effect,
    /// #   };
    ///     const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT.with_sidechain(2);
    ///     const LAYOUTS: &'static [AudioLayout] = &[
    ///         AudioLayout::STEREO_EFFECT.with_sidechain(2),
    ///         AudioLayout::STEREO_EFFECT,
    ///         AudioLayout::MONO_EFFECT.with_sidechain(1),
    ///         AudioLayout::MONO_EFFECT,
    ///     ];
    ///     # type Params = NoParams;
    ///     # fn new() -> Self { Self { channels: 2, keyed: true, params: Arc::new(NoParams {}) } }
    ///     # fn params(&self) -> Arc<NoParams> { Arc::clone(&self.params) }
    ///     fn set_layout(&mut self, layout: AudioLayout) {
    ///         self.channels = layout.outputs;
    ///         self.keyed = layout.sidechain > 0;
    ///     }
    ///     fn prepare(&mut self, _: &ProcessSpec) {}
    ///     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    /// }
    /// ```
    const LAYOUTS: &'static [AudioLayout] = &[Self::LAYOUT];
    /// Its parameters.
    type Params: ParamSet + 'static;

    /// The facts that identify the plugin, worked out when its library is
    /// loaded. [`INFO`](Self::INFO) unless overridden, so a plugin that
    /// knows its identity when it is compiled writes nothing.
    ///
    /// This is for a plugin whose identity is data: one engine, compiled
    /// once and copied to many files, each copy with the id, name, vendor
    /// and version of the instrument it plays, found next to or inside its
    /// own file through [`library_path`]. Two copies loaded in one host are
    /// then two plugins.
    ///
    /// The wrappers do not call this directly but through
    /// [`plugin_info`], which calls it once per library, the first time a
    /// host asks about the plugin (CLAP's descriptor, VST 3's factory, the
    /// LV2 descriptor, an Audio Unit's factory), before any instance exists,
    /// and keeps the result until the library is unloaded. So it may take
    /// its time, read files and allocate, and strings it builds can be
    /// leaked to make them `'static`, which costs one copy per process.
    /// A panic here is caught at the boundary, and the host finds no
    /// plugin.
    ///
    /// Two of the facts are not taken from here:
    ///
    /// - the [`kind`](PluginInfo::kind) is always `INFO`'s, because the
    ///   buses a plugin has, its [`LAYOUT`](Self::LAYOUT), and VST 3's
    ///   parameters for MIDI controllers are fixed when it is compiled;
    /// - an id that [`is_valid_id`] refuses makes the whole of `INFO` the
    ///   identity.
    ///
    /// Saved state carries the id, and a plugin refuses state saved under
    /// another, so two copies with different ids never load each other's
    /// state, as two different plugins would not.
    ///
    /// Formats that keep the identity in a bundle's files, an Audio Unit's
    /// `Info.plist` and an LV2 bundle's Turtle, have them written by `cargo
    /// mangue bundle` from what this returns in a program it builds and
    /// runs; a plugin whose identity depends on where its library is needs
    /// those files written for each copy. See ADR 0012.
    fn info() -> PluginInfo
    where
        Self: Sized,
    {
        Self::INFO
    }

    /// Creates the plugin, with its parameters at their defaults.
    fn new() -> Self
    where
        Self: Sized;

    /// The plugin's parameters, shared with whoever else needs them.
    fn params(&self) -> Arc<Self::Params>;

    /// The layout the plugin is to run with, one of
    /// [`LAYOUTS`](Self::LAYOUTS). Called while audio is stopped, right
    /// before every [`prepare`](Self::prepare), so `prepare` always knows
    /// it; with [`LAYOUT`](Self::LAYOUT) unless the host chose another.
    /// Does nothing unless overridden, which a plugin with one layout never
    /// needs to.
    fn set_layout(&mut self, layout: AudioLayout) {
        let _ = layout;
    }

    /// Gets ready to process under `spec`. Called before the first block and
    /// again whenever the sample rate, the block size or the layout
    /// changes. `spec.num_channels` is the layout's outputs.
    fn prepare(&mut self, spec: &ProcessSpec);

    /// Processes one block in place.
    ///
    /// `buffer` has as many channels as the layout last given to
    /// [`set_layout`](Self::set_layout) has
    /// [outputs](AudioLayout::outputs). The first
    /// [inputs](AudioLayout::inputs) of them hold the input; any others
    /// are silent. `context` carries the events of the block with the frame
    /// each falls on, the transport, and the sidechain if the layout has
    /// one.
    ///
    /// A plugin can learn about parameter changes in either of two ways,
    /// and both are always correct:
    ///
    /// - **Poll.** The parameters already hold the value they will have at
    ///   the end of the block. Reading them once per block is all a simple
    ///   plugin needs.
    /// - **Follow the events.** Every change is also in `context.events`
    ///   with its exact frame, whoever made it: host automation, a user
    ///   interface writing to the parameter, or restored state. A plugin
    ///   that tracks a value from these events alone never misses a change
    ///   and never needs to poll.
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>);

    /// Processes one block in place, as [`process`](Self::process) does,
    /// and sends notes and MIDI out through `output`. Every wrapper, the
    /// standalone runner and the [`Harness`] call this; unless overridden
    /// it calls `process` and sends nothing.
    ///
    /// Events go into `output` with their frame in the block, as those of
    /// `context.events` have, and must not allocate: `output` has room set
    /// aside, and refuses what does not fit. A plugin that sends notes
    /// sets [`NOTE_OUTPUT`](Self::NOTE_OUTPUT), does its work here, and
    /// lets `process` call this with a buffer that has no room, for the
    /// formats that have no output:
    ///
    /// ```
    /// use std::sync::Arc;
    /// use manguebyte_core::AudioBuffer;
    /// use manguebyte_dsp::event::{EventKind, NoteRef};
    /// use manguebyte_dsp::{ProcessContext, ProcessSpec};
    /// use manguebyte_params::param_set;
    /// use manguebyte_plugin::output::{OutputBuffer, OutputKind};
    /// use manguebyte_plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};
    ///
    /// struct NoParams {}
    /// param_set!(NoParams {});
    ///
    /// /// Plays every note a fifth higher as well.
    /// struct Fifths(Arc<NoParams>);
    ///
    /// impl Plugin for Fifths {
    ///     const INFO: PluginInfo = PluginInfo {
    ///         id: "org.example.fifths",
    ///         name: "Fifths",
    ///         vendor: "Example",
    ///         version: "1.0.0",
    ///         description: "Adds a fifth to every note",
    ///         kind: PluginKind::NoteEffect,
    ///     };
    ///     const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    ///     const NOTE_OUTPUT: bool = true;
    ///     type Params = NoParams;
    ///
    ///     fn new() -> Self { Self(Arc::new(NoParams {})) }
    ///     fn params(&self) -> Arc<NoParams> { Arc::clone(&self.0) }
    ///     fn prepare(&mut self, _: &ProcessSpec) {}
    ///     fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
    ///         self.process_with_output(buffer, context, &mut OutputBuffer::new());
    ///     }
    ///     fn process_with_output(
    ///         &mut self,
    ///         _: &mut AudioBuffer<f32>,
    ///         context: &ProcessContext<'_, f32>,
    ///         output: &mut OutputBuffer,
    ///     ) {
    ///         for event in context.events {
    ///             let fifth = |note: NoteRef| NoteRef::new(note.channel, note.key.saturating_add(7));
    ///             match event.kind {
    ///                 EventKind::NoteOn { note, velocity } => {
    ///                     output.note_on(event.offset, note, velocity);
    ///                     output.note_on(event.offset, fifth(note), velocity);
    ///                 }
    ///                 EventKind::NoteOff { note, velocity } => {
    ///                     output.note_off(event.offset, note, velocity);
    ///                     output.note_off(event.offset, fifth(note), velocity);
    ///                 }
    ///                 _ => {}
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// let mut harness = Harness::<Fifths>::new(48_000.0, 64);
    /// harness.note(10, 20, 60, 1.0);
    /// harness.render(100);
    /// let sent = harness.take_output();
    /// assert_eq!(sent.len(), 4);
    /// assert_eq!(sent[1], (10, OutputKind::NoteOn { note: NoteRef::new(0, 67), velocity: 1.0 }));
    /// ```
    fn process_with_output(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
    ) {
        let _ = output;
        self.process(buffer, context);
    }

    /// Processes one block as [`process_with_output`](Self::process_with_output)
    /// does, and writes the auxiliary output buses of
    /// [`AUX_OUTPUTS`](Self::AUX_OUTPUTS) into `aux`. Every wrapper, the
    /// standalone runner and the [`Harness`] call this; unless overridden it
    /// calls `process_with_output` and leaves every bus silent.
    ///
    /// `aux` holds the declared buses in their order, each the length of
    /// `buffer` and silent; a bus the host has switched off is not given
    /// ([`AuxOutputs::bus_mut`] returns `None`), so rendering only the
    /// buses that are there costs nothing for those nobody listens to.
    /// Nothing here may allocate, as in `process`.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use manguebyte_core::AudioBuffer;
    /// use manguebyte_dsp::{ProcessContext, ProcessSpec};
    /// use manguebyte_params::param_set;
    /// use manguebyte_plugin::output::OutputBuffer;
    /// use manguebyte_plugin::{AudioLayout, AuxOutput, AuxOutputs, Harness, Plugin, PluginInfo, PluginKind};
    ///
    /// struct NoParams {}
    /// param_set!(NoParams {});
    ///
    /// /// A tone on the main output, and each division on a bus of its own.
    /// struct Divisions(Arc<NoParams>);
    ///
    /// impl Plugin for Divisions {
    ///     const INFO: PluginInfo = PluginInfo {
    ///         id: "org.example.divisions",
    ///         name: "Divisions",
    ///         vendor: "Example",
    ///         version: "1.0.0",
    ///         description: "One bus per division",
    ///         kind: PluginKind::Instrument,
    ///     };
    ///     const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    ///     const AUX_OUTPUTS: &'static [AuxOutput] =
    ///         &[AuxOutput::stereo("Great"), AuxOutput::stereo("Swell").inactive()];
    ///     type Params = NoParams;
    ///
    ///     fn new() -> Self { Self(Arc::new(NoParams {})) }
    ///     fn params(&self) -> Arc<NoParams> { Arc::clone(&self.0) }
    ///     fn prepare(&mut self, _: &ProcessSpec) {}
    ///     fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
    ///         self.process_with_aux(buffer, context, &mut OutputBuffer::new(), &mut AuxOutputs::new());
    ///     }
    ///     fn process_with_aux(
    ///         &mut self,
    ///         buffer: &mut AudioBuffer<f32>,
    ///         _: &ProcessContext<'_, f32>,
    ///         _: &mut OutputBuffer,
    ///         aux: &mut AuxOutputs,
    ///     ) {
    ///         buffer.channel_mut(0).fill(0.1);
    ///         for (index, level) in [0.2, 0.3].into_iter().enumerate() {
    ///             // An inactive division is not rendered at all.
    ///             if let Some(bus) = aux.bus_mut(index) {
    ///                 bus.channel_mut(0).fill(level);
    ///             }
    ///         }
    ///     }
    /// }
    ///
    /// let mut harness = Harness::<Divisions>::new(48_000.0, 64);
    /// harness.render(10);
    /// assert_eq!(harness.aux_output(0).unwrap().channel(0), &[0.2; 10]);
    /// assert!(harness.aux_output(1).is_none(), "off until switched on");
    /// harness.set_aux_active(1, true);
    /// harness.render(10);
    /// assert_eq!(harness.aux_output(1).unwrap().channel(0), &[0.3; 10]);
    /// ```
    fn process_with_aux(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
        aux: &mut AuxOutputs,
    ) {
        let _ = aux;
        self.process_with_output(buffer, context, output);
    }

    /// Clears internal state such as filter memory and tails, without
    /// touching parameters.
    fn reset(&mut self) {}

    /// The MIDI controllers the plugin listens to, and whether their
    /// channel matters. [`MidiControllers::MUSICAL`] unless overridden.
    /// Only instruments and note effects receive MIDI; for other kinds this
    /// is not used.
    ///
    /// # What each format does with it
    ///
    /// - **VST 3** delivers controllers only as parameters, which the
    ///   plugin has to declare. The wrapper declares one hidden parameter
    ///   for each controller in the set and maps every channel's controller
    ///   to it through `IMidiMapping`, so the channel a move came from is
    ///   lost. It turns each move back into MIDI on its frame **on all 16
    ///   channels**, one message per channel in channel order, so that a
    ///   plugin which keeps controllers per channel, as
    ///   `manguebyte_dsp::Synth` does, applies the wheel, the pedals and
    ///   "all notes off" to notes on every channel. The cost is that the
    ///   controllers of one channel reach the notes of the others: the
    ///   sustain pedal of channel 10 also holds a note on channel 1. With
    ///   [`per_channel`](MidiControllers::per_channel) it declares one
    ///   parameter per controller and channel instead, and each message
    ///   arrives once, on its own channel. A controller outside the set
    ///   never reaches the plugin. Some hosts, REAPER among them, list
    ///   hidden parameters next to the plugin's own, so a small set keeps
    ///   the plugin's list short: [`MidiControllers::ALL`] with channels is
    ///   2,080 entries.
    /// - **CLAP**, and anything else that carries MIDI as it is, such as
    ///   the [`Harness`], deliver every message unchanged, once, on its own
    ///   channel, whatever the set. A plugin that must behave the same
    ///   everywhere can drop the others with [`MidiControllers::wants`].
    ///
    /// So a plugin that plays notes on several channels and needs each
    /// channel's controllers kept to that channel, such as an MPE
    /// instrument or a multitimbral one, asks for channels. Any other
    /// plugin can leave them shared and treat a controller as applying to
    /// every channel.
    ///
    /// The VST 3 wrapper passes at most 4,096 events to one block; each
    /// move of a shared controller takes 16 of them.
    ///
    /// ```
    /// use std::sync::Arc;
    /// use manguebyte_core::AudioBuffer;
    /// use manguebyte_dsp::{ProcessContext, ProcessSpec};
    /// use manguebyte_params::{FloatParam, FloatRange, param_set};
    /// use manguebyte_plugin::{
    ///     AudioLayout, MidiController, MidiControllers, Plugin, PluginInfo, PluginKind,
    /// };
    ///
    /// struct OrganParams { level: FloatParam }
    /// param_set!(OrganParams { level });
    /// struct Organ { params: Arc<OrganParams> }
    ///
    /// impl Plugin for Organ {
    ///     const INFO: PluginInfo = PluginInfo {
    ///         id: "org.example.organ",
    ///         name: "Organ",
    ///         vendor: "Example",
    ///         version: "1.0.0",
    ///         description: "Drawbars and a swell pedal",
    ///         kind: PluginKind::Instrument,
    ///     };
    ///     const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    ///     // An organ has no pitch wheel, but a swell pedal and a sustain
    ///     // pedal, and it must stop when told to.
    ///     const MIDI_CONTROLLERS: MidiControllers = MidiControllers::list(&[
    ///         MidiController::control(11).unwrap(),
    ///         MidiController::control(64).unwrap(),
    ///         MidiController::control(120).unwrap(),
    ///         MidiController::control(123).unwrap(),
    ///     ]);
    ///     type Params = OrganParams;
    ///
    ///     fn new() -> Self {
    ///         let level = FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 0.5);
    ///         Self { params: Arc::new(OrganParams { level }) }
    ///     }
    ///     fn params(&self) -> Arc<OrganParams> { Arc::clone(&self.params) }
    ///     fn prepare(&mut self, _: &ProcessSpec) {}
    ///     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    /// }
    ///
    /// assert_eq!(Organ::MIDI_CONTROLLERS.len(), 4);
    /// // In VST 3 the organ has four hidden parameters and never hears the
    /// // wheel. In CLAP it hears whatever the host sends, and can drop the
    /// // same messages itself:
    /// use manguebyte_midi::MidiMessage;
    /// let wheel = MidiMessage::PitchBend { channel: 0, value: 100 };
    /// assert!(!Organ::MIDI_CONTROLLERS.wants(&wheel));
    /// ```
    const MIDI_CONTROLLERS: MidiControllers = MidiControllers::MUSICAL;

    /// How MIDI from the host is read as MPE. [`Mpe::Follow`] unless
    /// overridden: zones exist once the stream configures them, and a
    /// stream that never does is plain MIDI.
    ///
    /// Every wrapper, the standalone runner and the [`Harness`] read the
    /// MIDI they are given through a
    /// [`NoteInput`](manguebyte_dsp::note_input::NoteInput) set up with
    /// this, so a note of an MPE zone or of MIDI 2.0 reaches
    /// [`process`](Self::process) as a [`NoteOn`](EventKind::NoteOn) with
    /// an identifier, shaped by per-note
    /// [`NoteExpression`](EventKind::NoteExpression)s, exactly as a CLAP
    /// host's own notes and expressions do. An instrument made to be
    /// played from an MPE controller says [`Mpe::LOWER_ZONE`], because
    /// many hosts never pass the configuration message on; one that reads
    /// MPE from raw MIDI itself says [`Mpe::Off`].
    const MPE: Mpe = Mpe::Follow;

    /// Whether the plugin sends notes and MIDI out, from
    /// [`process_with_output`](Self::process_with_output). `false` unless
    /// overridden.
    ///
    /// With it set, a plugin of any kind gets an output: a CLAP note port,
    /// a VST 3 event bus, the MIDI output port of the standalone runner,
    /// and in the [`Harness`], [`take_output`](Harness::take_output).
    /// Without it, the buffer `process_with_output` is given has no room,
    /// and whatever the plugin writes there is dropped.
    const NOTE_OUTPUT: bool = false;

    /// The output buses the plugin has besides its main output, in the
    /// order hosts list them after it. None unless overridden. The plugin
    /// writes them in [`process_with_aux`](Self::process_with_aux).
    ///
    /// They are fixed, whatever layout of [`LAYOUTS`](Self::LAYOUTS) the
    /// host chooses, and a host can switch each on or off, which the
    /// plugin sees as the bus being given or not:
    ///
    /// - **CLAP**: one audio port each, after the main output, with its
    ///   name and channel count; `audio-ports-activation` switches them.
    ///   CLAP says every port starts active, so
    ///   [`active_by_default`](AuxOutput::active_by_default) is not used.
    /// - **VST 3**: one auxiliary audio output bus each, flagged active by
    ///   default as declared; `activateBus` switches them.
    /// - **Audio Units**: one output element each, after element 0, with
    ///   its name. Audio Units cannot switch an element off, so every bus
    ///   is rendered.
    /// - **LV2**: one output port per channel after the main outputs,
    ///   grouped per bus with the port-groups extension; a bus whose ports
    ///   the host leaves unconnected is off.
    /// - **The standalone runner**: each bus on device channels of its own,
    ///   by default the ones after the main output's.
    const AUX_OUTPUTS: &'static [AuxOutput] = &[];

    /// How many frames late the output is compared with the input.
    fn latency_samples(&self) -> usize {
        0
    }

    /// How many voices the plugin plays, for an instrument that says so.
    /// `None` unless overridden. Read when the plugin is activated, after
    /// [`prepare`](Self::prepare), as the latency is, and offered to CLAP
    /// hosts as `voice-info`.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use manguebyte_core::AudioBuffer;
    /// # use manguebyte_dsp::{ProcessContext, ProcessSpec, Synth, Voice};
    /// # use manguebyte_dsp::event::NoteRef;
    /// # use manguebyte_params::param_set;
    /// # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, VoiceInfo};
    /// # #[derive(Default)] struct Beep;
    /// # impl Voice for Beep {
    /// #     fn prepare(&mut self, _: &ProcessSpec) {}
    /// #     fn note_on(&mut self, _: NoteRef, _: f32) {}
    /// #     fn note_off(&mut self, _: f32) {}
    /// #     fn choke(&mut self) {}
    /// #     fn is_active(&self) -> bool { false }
    /// #     fn render(&mut self, _: &mut AudioBuffer) {}
    /// # }
    /// # struct NoParams {}
    /// # param_set!(NoParams {});
    /// struct Organ { synth: Synth<Beep>, params: Arc<NoParams> }
    ///
    /// impl Plugin for Organ {
    /// #   const INFO: PluginInfo = PluginInfo {
    /// #       id: "org.example.organ", name: "Organ", vendor: "Example",
    /// #       version: "1.0.0", description: "", kind: PluginKind::Instrument,
    /// #   };
    /// #   const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    /// #   type Params = NoParams;
    /// #   fn new() -> Self { Self { synth: Synth::with_voices(16, Beep::default), params: Arc::new(NoParams {}) } }
    /// #   fn params(&self) -> Arc<NoParams> { Arc::clone(&self.params) }
    /// #   fn prepare(&mut self, _: &ProcessSpec) {}
    /// #   fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    ///     // ...
    ///     fn voice_info(&self) -> Option<VoiceInfo> {
    ///         Some(VoiceInfo {
    ///             voices: self.synth.voice_limit() as u32,
    ///             capacity: self.synth.polyphony() as u32,
    ///             overlapping_notes: true,
    ///         })
    ///     }
    /// }
    /// # assert_eq!(Organ::new().voice_info().unwrap().capacity, 16);
    /// ```
    fn voice_info(&self) -> Option<VoiceInfo> {
        None
    }

    /// Names for keys, such as the drums of a kit, which a host shows in
    /// its piano roll. None unless overridden. Offered to CLAP hosts as
    /// `note-name`.
    const NOTE_NAMES: &'static [NoteName] = &[];

    /// How to open the plugin's editor, if it has one. `None` unless
    /// overridden, so a plugin without an editor writes nothing.
    ///
    /// Whatever runs the plugin calls this once, on the thread that creates
    /// the plugin, right after [`params`](Self::params) and before any audio
    /// runs, as the [`Harness`] does. It keeps what it gets for the life of
    /// the plugin instance and opens editors from it on the host's main
    /// thread, while the plugin itself runs on the audio thread. That is
    /// why the spec is `Send + Sync + 'static`: it can hold `Arc`s of what
    /// it shares with the plugin, such as the parameters and meter
    /// readings, and the compiler refuses anything that points at the
    /// plugin. See the [`editor`] module for an example.
    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        None
    }
}

/// Notices parameter changes that did not come from the host, and turns
/// them into events.
///
/// A parameter can be written at any moment by a user interface or by a
/// state restore. Those writes go straight to the parameter and would
/// otherwise be visible only to a plugin that polls. A wrapper keeps one of
/// these, and before each block asks it to add a [`EventKind::ParamValue`]
/// event at frame 0 for every parameter whose value is not what the plugin
/// last saw. The event stream is then a complete record of every change.
///
/// All methods after [`new`](Self::new) are real-time safe.
#[derive(Debug)]
pub struct ParamWatcher {
    /// The value of each parameter as the plugin last saw it, in the order
    /// the set visits them.
    seen: Vec<f64>,
}

impl ParamWatcher {
    /// Starts watching `params` from their current values. Allocates.
    pub fn new(params: &dyn ParamSet) -> Self {
        let mut seen = Vec::with_capacity(params.len());
        params.visit(&mut |param| seen.push(param.plain()));
        Self { seen }
    }

    /// Adds an event at frame 0 for every parameter that has changed since
    /// [`remember`](Self::remember) was last called.
    ///
    /// Call it on an empty buffer, before the host's own events are added,
    /// so that host automation on frame 0 comes later in the stream and
    /// takes precedence. If the buffer fills up, the remaining changes are
    /// picked up on the next block.
    pub fn emit_changes(&mut self, params: &dyn ParamSet, events: &mut EventBuffer) {
        let mut seen = self.seen.iter_mut();
        params.visit(&mut |param| {
            let Some(seen) = seen.next() else {
                return;
            };
            let value = param.plain();
            if value != *seen {
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value,
                };
                if events.push(Event { offset: 0, kind }).is_ok() {
                    *seen = value;
                }
            }
        });
    }

    /// Records the parameters' values as seen. Call it after the block's
    /// events have been stored with [`apply_param_events`].
    pub fn remember(&mut self, params: &dyn ParamSet) {
        let mut seen = self.seen.iter_mut();
        params.visit(&mut |param| {
            if let Some(seen) = seen.next() {
                *seen = param.plain();
            }
        });
    }

    /// Stores the parameter values among `events` into `params`, and
    /// records each as seen. Real-time safe.
    ///
    /// This takes the place of [`apply_param_events`] followed by
    /// [`remember`](Self::remember) where another thread may write the
    /// parameters while a block is being prepared, as an editor does when
    /// it stores its edits straight into them. Pass only the events that
    /// did not come from [`emit_changes`](Self::emit_changes), such as the
    /// host's: the announced values were read from the parameters, so
    /// storing them again could undo an edit made in the meantime, and
    /// `emit_changes` has recorded them already.
    ///
    /// No parameter is read back. What is recorded as seen is what the
    /// parameter holds after storing the event's value, as
    /// [`Param::stored_plain`](manguebyte_params::Param::stored_plain)
    /// works it out, so a value another thread stores at any moment after
    /// `emit_changes`, even right after this stores the host's, is not
    /// taken as seen, and the next block announces it.
    ///
    /// ```
    /// use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
    /// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
    /// use manguebyte_plugin::ParamWatcher;
    ///
    /// struct Params { level: FloatParam, pan: FloatParam }
    /// param_set!(Params { level, pan });
    /// let params = Params {
    ///     level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0),
    ///     pan: FloatParam::new("pan", "Pan", FloatRange::linear(-1.0, 1.0), 0.0),
    /// };
    /// let mut watcher = ParamWatcher::new(&params);
    /// let mut events = EventBuffer::with_capacity(8);
    ///
    /// // An editor sets the level; the block announces it.
    /// params.level.set(2.0);
    /// watcher.emit_changes(&params, &mut events);
    /// let announced = events.len();
    /// // The editor sets it again while the block is being prepared, and the
    /// // host moves the pan on frame 10.
    /// params.level.set(3.0);
    /// let pan = params.pan.param_id();
    /// events.push(Event { offset: 10, kind: EventKind::ParamValue { param: pan, value: 0.5 } }).unwrap();
    ///
    /// watcher.apply_and_remember(&params, &events.as_slice()[announced..]);
    /// assert_eq!(params.pan.value(), 0.5);
    /// // The newer edit is kept, and the next block announces it.
    /// assert_eq!(params.level.value(), 3.0);
    /// events.clear();
    /// watcher.emit_changes(&params, &mut events);
    /// assert_eq!(events.as_slice(), [Event {
    ///     offset: 0,
    ///     kind: EventKind::ParamValue { param: params.level.param_id(), value: 3.0 },
    /// }]);
    /// ```
    pub fn apply_and_remember(&mut self, params: &dyn ParamSet, events: &[Event]) {
        for event in events {
            let EventKind::ParamValue { param, value } = event.kind else {
                continue;
            };
            let mut index = 0;
            let mut found = None;
            params.visit(&mut |candidate| {
                if found.is_none() && candidate.param_id() == param {
                    found = Some((index, candidate));
                }
                index += 1;
            });
            if let Some((index, target)) = found {
                target.set_plain(value);
                // Not read back: another thread may have stored a newer
                // value already. The parameter may round or clamp what it
                // is given; `stored_plain` says what it made of it.
                if let (Some(seen), Some(stored)) =
                    (self.seen.get_mut(index), target.stored_plain(value))
                {
                    *seen = stored;
                }
            }
        }
    }
}

/// Stores every parameter value found in `events` into `params`.
///
/// A wrapper calls this before each block so that anything polling a
/// parameter sees the value automation has given it. When a parameter
/// changes more than once in the block, the last change wins. Real-time
/// safe.
pub fn apply_param_events(params: &dyn ParamSet, events: &EventBuffer) {
    for event in events {
        if let EventKind::ParamValue { param, value } = event.kind {
            if let Some(target) = params.find_param(param) {
                target.set_plain(value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
    use manguebyte_params::{FloatParam, FloatRange, Param, ParamInfo, param_set};

    use super::ParamWatcher;

    type After = Box<dyn FnOnce(&FloatParam) + Send>;

    /// A float parameter that runs `after` once, right after the next value
    /// is stored: the moment another thread could store one of its own.
    struct Hooked {
        param: FloatParam,
        after: Mutex<Option<After>>,
    }

    impl Hooked {
        fn then(&self, after: impl FnOnce(&FloatParam) + Send + 'static) {
            *self.after.lock().unwrap() = Some(Box::new(after));
        }
    }

    impl Param for Hooked {
        fn info(&self) -> &ParamInfo {
            self.param.info()
        }
        fn plain(&self) -> f64 {
            self.param.plain()
        }
        fn set_plain(&self, value: f64) {
            self.param.set_plain(value);
            let after = self.after.lock().unwrap().take();
            if let Some(after) = after {
                after(&self.param);
            }
        }
        fn stored_plain(&self, value: f64) -> Option<f64> {
            self.param.stored_plain(value)
        }
        fn default_plain(&self) -> f64 {
            self.param.default_plain()
        }
        fn normalized_to_plain(&self, normalized: f64) -> f64 {
            self.param.normalized_to_plain(normalized)
        }
        fn plain_to_normalized(&self, plain: f64) -> f64 {
            self.param.plain_to_normalized(plain)
        }
        fn step_count(&self) -> Option<u32> {
            self.param.step_count()
        }
        fn plain_to_text(&self, plain: f64) -> String {
            self.param.plain_to_text(plain)
        }
        fn text_to_plain(&self, text: &str) -> Option<f64> {
            self.param.text_to_plain(text)
        }
    }

    struct Params {
        cutoff: Hooked,
    }
    param_set!(Params { cutoff });

    /// An edit stored right after the host's value, before anything could
    /// read the parameter back, is announced on the next block, and the
    /// host's value, rounded as the parameter stores it, is not.
    #[test]
    fn an_edit_stored_right_after_the_hosts_value_is_announced() {
        let params = Params {
            cutoff: Hooked {
                param: FloatParam::new("cutoff", "Cutoff", FloatRange::linear(0.0, 4.0), 1.0),
                after: Mutex::new(None),
            },
        };
        let cutoff = params.cutoff.param_id();
        let mut watcher = ParamWatcher::new(&params);
        let mut events = EventBuffer::with_capacity(8);

        // The host sets 1.2, which an `f32` cannot hold exactly; the person's
        // edit to 3 lands right after.
        params.cutoff.then(|param| param.set(3.0));
        let kind = EventKind::ParamValue {
            param: cutoff,
            value: 1.2,
        };
        watcher.apply_and_remember(&params, &[Event { offset: 0, kind }]);
        assert_eq!(params.cutoff.param.value(), 3.0);

        watcher.emit_changes(&params, &mut events);
        assert_eq!(
            events.as_slice(),
            [Event {
                offset: 0,
                kind: EventKind::ParamValue {
                    param: cutoff,
                    value: 3.0
                }
            }]
        );

        // With no edit in between, the rounded host value is not announced.
        let kind = EventKind::ParamValue {
            param: cutoff,
            value: 1.2,
        };
        watcher.apply_and_remember(&params, &[Event { offset: 0, kind }]);
        events.clear();
        watcher.emit_changes(&params, &mut events);
        assert!(events.is_empty(), "announced {:?}", events.as_slice());
    }
}
