//! Hosting [VST 3](https://steinbergmedia.github.io/vst3_dev_portal/) plugins:
//! loading a plugin made by anyone and running it as a
//! [`Processor`](manguebyte_dsp::Processor).
//!
//! # The three types
//!
//! | Type | What it is | Threads |
//! |---|---|---|
//! | [`Vst3Module`] | A loaded module (a bundle or a bare library) and the classes its factory offers | Any; can be cloned and sent |
//! | [`Vst3Plugin`] | One plugin, as the main thread deals with it: buses, parameters, state, activation, the editor | The thread that created it, and no other |
//! | [`Vst3Processor`] | The same plugin, as the audio thread deals with it: a `Processor<f32>` | One thread at a time; can be sent |
//!
//! These are the same three roles the [CLAP host](crate::clap) has, with
//! the same rules made into types. `Vst3Plugin` is not `Send`.
//! [`Vst3Plugin::activate`] hands out the one `Vst3Processor`, which is
//! `Send` and processes through `&mut self`. A parameter set on the main
//! thread travels to the audio thread through a wait-free queue and reaches
//! the plugin as a point on the first frame of the next block; what the
//! plugin reports travels back through another queue.
//!
//! # Component and controller
//!
//! A VST 3 plugin is two objects: a component, which makes the sound, and
//! an edit controller, which owns the parameters a person sees. A plugin
//! may implement both in one object, as the framework's own wrapper does,
//! or ship them as two classes. Both are handled, and look the same from
//! here: when there are two, they are connected to each other through
//! their connection points, and the controller is given the component's
//! state when the plugin is created and whenever a state is loaded. A
//! plugin whose controller cannot be made is still hosted, without
//! parameters, and [`Vst3Plugin::pump`] says so.
//!
//! # A session from start to end
//!
//! This example runs as a test. It hosts Mangue Pluck, the framework's
//! example instrument, through the factory it exports, without loading a
//! file: [`Vst3Module::from_factory`] takes the factory of a plugin that is
//! part of the program. A module from a file, through [`Vst3Module::load`],
//! is used in exactly the same way.
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
//! use manguebyte_dsp::{ProcessContext, Processor, Transport};
//! use manguebyte_host::vst3::{ClassId, Vst3Module};
//!
//! // SAFETY: the factory is the one Mangue Pluck exports, made by the
//! // framework's VST 3 wrapper, which keeps to the VST 3 interfaces. Its
//! // one reference is handed over.
//! let module = unsafe {
//!     Vst3Module::from_factory(mangue_pluck::GetPluginFactory().cast(), "Mangue Pluck")
//! }?;
//! let class = module.classes()[0].clone();
//! assert_eq!(class.name, "Mangue Pluck");
//! assert!(class.sub_categories.iter().any(|word| word == "Instrument"));
//!
//! // Main thread: create the plugin and look at it.
//! let mut plugin = module.create(class.id)?;
//! assert_eq!(plugin.audio_outputs()[0].channels, 2);
//! let level = plugin
//!     .params()
//!     .iter()
//!     .find(|param| param.title == "Level")
//!     .expect("the plugin has a level")
//!     .clone();
//! // Values are normalized, as VST 3 automates them; the plugin gives the
//! // plain value and the text.
//! let text = plugin.param_to_text(level.id, level.default).expect("text");
//! assert!(text.starts_with("-4"), "{text}");
//!
//! // Activating hands out the audio half.
//! let mut processor = plugin.activate(48_000.0, 256)?;
//!
//! // Audio thread: a note on frame 10, and the level turned all the way
//! // up on frame 100, as a point in the plugin's queue for that parameter.
//! let mut events = EventBuffer::with_capacity(16);
//! let note = NoteRef::with_id(0, 57, 1);
//! events.push(Event { offset: 10, kind: EventKind::NoteOn { note, velocity: 0.9 } }).ok();
//! events.push(Event { offset: 100, kind: EventKind::ParamValue { param: level.id, value: 1.0 } }).ok();
//! let transport = Transport::playing_at(120.0);
//! let mut block = AudioBuffer::<f32>::new(2, 256);
//! processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
//! assert!(block.channel(0)[..10].iter().all(|&sample| sample == 0.0));
//! assert!(block.channel(0).iter().any(|&sample| sample != 0.0));
//!
//! // Main thread, now and then: let the plugin have its say. The host has
//! // kept the controller up to date with what it sent the plugin.
//! plugin.pump();
//! assert_eq!(plugin.param_value(level.id), Some(1.0));
//! let state = plugin.save_state()?;
//!
//! // The audio half goes first; then the plugin can be deactivated, and a
//! // saved state brings it back to where it was.
//! drop(processor);
//! plugin.deactivate()?;
//! plugin.set_param(level.id, 0.0)?;
//! plugin.load_state(&state)?;
//! assert_eq!(plugin.param_value(level.id), Some(1.0));
//! # Ok::<(), manguebyte_host::vst3::Vst3Error>(())
//! ```
//!
//! # What is covered
//!
//! | VST 3 feature | Behaviour |
//! |---|---|
//! | Module | A bundle folder, resolved for this system and machine, or a bare library; `InitDll`/`ExitDll`, `ModuleEntry`/`ModuleExit` or `bundleEntry`/`bundleExit` called once per loaded file |
//! | Factory | Factory information and every class, with the richest class information the factory offers, as bounded text |
//! | Component and controller | One object or two; two are connected and kept in step |
//! | Buses | Every audio and event bus listed. The main input, the main output, the first auxiliary input (the sidechain), the first event input and the first event output are switched on and used, and so is every bus the plugin wants on from the start; the others are switched off. The plugin's own speaker arrangements are kept. 32-bit float |
//! | Parameters | Information, normalized and plain values, text both ways |
//! | Automation | Every point on its frame, in one queue per parameter; what the plugin writes back is read |
//! | Notes | Note on and off with ids, polyphonic pressure, note expression (volume, pan, tuning, vibrato, brightness) |
//! | MIDI controllers | Controllers, pitch bend and channel pressure are sent to the parameter the plugin maps them to through `IMidiMapping` |
//! | Host objects | Host application, component handler (begin, perform and end of an edit, restart requests, dirty state), memory streams, messages, parameter changes, event lists; everything the audio thread uses is allocated at activation |
//! | State | Component and controller state in one run of bytes. A loaded state replaces every parameter value still on its way between the halves |
//! | Latency, tail | Read on the main thread, as VST 3 asks: both at activation, the tail again at every `pump`. The audio half reports the values read there |
//! | Editor | The `"editor"` view in a window of the application's (`HWND`, `NSView` or X11), with the host's frame, a content scale, resizing both ways and, on Linux and other X11 systems, the host's run loop for timers and file descriptors; see [Editors](#editors) |
//!
//! # Editors
//!
//! A plugin's editor goes inside a window the application owns:
//! [`Vst3Plugin::open_editor`] takes a [`ParentWindow`](crate::editor::ParentWindow)
//! and the scale the window is drawn at, asks the controller for its
//! view, and attaches the view to the window, which puts it on screen.
//! [`Vst3Plugin::close_editor`] takes it out again; dropping the plugin
//! closes it first. What the editor draws, and how, is the plugin's
//! business.
//!
//! This crate has no windows and no event loop, so what the editor asks of
//! its host is passed on rather than done. The view's frame records its
//! `resizeView` requests and answers yes, and [`Vst3Plugin::pump`] returns
//! each as [`HostEvent::Editor`] with an
//! [`EditorRequest::Resize`](crate::editor::EditorRequest::Resize). The
//! application resizes its window and then tells the view with
//! [`Vst3Plugin::set_editor_size`], which also serves for a window the
//! person resized: a resizable view is asked for the nearest size it can
//! take first. Until then the view keeps its old size. VST 3 lets a host
//! call `onSize` from inside `resizeView` or after it; this host always
//! does it after, since only the application can resize the window, and
//! the window should have the size before the view is told it has.
//!
//! On Linux and the other systems whose windows are X11's, the frame also
//! offers `IRunLoop`, because an editor there runs from its host's loop.
//! Its timers fire in `pump`, which
//! [`Vst3Plugin::next_editor_timer`] says when to call next, and its file
//! descriptors are listed by [`Vst3Plugin::editor_fds`] for the
//! application to watch and report with [`Vst3Plugin::editor_fd_ready`].
//! A handler is always called with nothing of the host's locked or
//! borrowed, so it may register and unregister anything, itself included.
//! Elsewhere editors keep their own timers and the lists stay empty.
//!
//! What a person does with the editor's controls reaches the controller,
//! which reports it to the host as edits; `pump` passes them on to the
//! processor, which hears them at the start of its next block, and returns
//! them as [`HostEvent::GestureBegin`], [`HostEvent::ParamValue`] and
//! [`HostEvent::GestureEnd`], as for any edit the controller makes.
//!
//! The frame and the run loop are the main thread's: a call from another
//! thread is refused and remarked on. A frame belongs to one opening of
//! the editor, and refuses everything once the editor has closed.
//!
//! # A plugin is not trusted
//!
//! As in the CLAP host, loading is the one `unsafe` step:
//! [`Vst3Module::load`] and [`Vst3Module::from_factory`] run code that this
//! crate cannot check, and whoever calls them vouches that it keeps to the
//! VST 3 interfaces. What this module guarantees is that the plugin's
//! answers are not taken on faith:
//!
//! - every string is read within its field or up to a limit, and need not
//!   be valid text;
//! - every count is held to a limit, listed in the constants of this
//!   module, before anything is allocated for it;
//! - every pointer is checked for null before it is used, and an
//!   interface the plugin does not have is an error, not a crash;
//! - values that are not numbers are dropped, and normalized values are
//!   held to `0..=1` both ways;
//! - a plugin that refuses to initialise, activate, start or process
//!   leaves the host running. The failure comes back as a [`Vst3Error`], or,
//!   on the audio thread, as silence at the output with the reason in
//!   [`Vst3Processor::status`] and in the events from [`Vst3Plugin::pump`].
//!
//! # Limits
//!
//! No 64-bit audio, no program lists, no unit information, no changes to
//! the bus layout, and no view but the editor. Further audio buses carry
//! silence in and are discarded out; further event buses are not used. A
//! note expression reaches only a note that has an id, since VST 3
//! addresses expression by id alone. VST 3 has no choke and no parameter
//! modulation: a choke is sent as a note-off, and modulation is not sent.
//!
//! Note expression travels in VST 3's scales, which are positions between
//! 0 and 1, so a value that goes to a plugin and comes back in what the
//! plugin reports is rounded twice in 64-bit floating point. Within their
//! scales, vibrato and brightness come back exactly, and so does any
//! volume from 2⁻¹⁰²⁰ up. Tuning comes back within 240 × 2⁻⁵² semitones
//! (about 5 × 10⁻¹⁴) and pan within 2⁻⁵². These are absolute errors: near
//! zero they are many units in the last place.
//!
//! VST is a trademark of Steinberg Media Technologies GmbH.

mod bundle;
mod changes;
mod convert;
mod frame;
mod module;
mod objects;
mod plugin;
mod processor;
mod shared;
mod state;
mod text;

use std::fmt;
use std::path::PathBuf;

use manguebyte_core::ParamId;

use crate::editor::WindowApi;

pub use ::vst3;
pub use module::{ClassInfo, FactoryInfo, Vst3Module};
pub use plugin::{BusInfo, HostEvent, ParamFlags, ParamInfo, Tail, Vst3Plugin};
pub use processor::{ProcessStatus, Vst3Processor};
pub use text::{ClassId, ParseClassIdError};

/// Most classes listed from one module.
pub const MAX_CLASSES: usize = 1_024;

/// Most UTF-16 units read from any one string a plugin passes through a
/// pointer. Text in a fixed field is read within the field.
pub const MAX_TEXT_UNITS: usize = 4_096;

/// Most parameters listed from one plugin.
pub const MAX_PARAMS: usize = 65_536;

/// Most buses read of each kind (audio, events) in each direction. A plugin
/// that declares more audio buses cannot be activated, because its process
/// call would expect a buffer for every one of them.
pub const MAX_BUSES: usize = 32;

/// Most channels one audio bus may have. VST 3 itself describes a bus's
/// speakers in a 64-bit mask.
pub const MAX_CHANNELS_PER_BUS: usize = 64;

/// Largest block size a plugin can be activated with.
pub const MAX_BLOCK_SIZE: usize = 1 << 20;

/// Most samples of working memory allocated for one activation: every
/// channel of every audio bus times the block size. 64 MiB of `f32`.
pub const MAX_SCRATCH_SAMPLES: usize = 1 << 24;

/// Largest state accepted from a plugin, for each of its two parts, so that
/// one that never stops writing cannot exhaust memory.
pub const MAX_STATE_BYTES: usize = 64 << 20;

/// Largest latency taken from a plugin, in frames. A larger report is cut
/// to this, since a host allocates delay lines by it.
pub const MAX_LATENCY_FRAMES: u32 = 1 << 24;

/// Most note events handed to a plugin in one call of its process function,
/// and most kept of what it reports during one call of
/// [`process_with`](manguebyte_dsp::Processor::process_with). As in the CLAP
/// host, the limit on the way in applies to each call of the plugin, and
/// the limit on the way out to the caller's whole block. Further events are
/// dropped and counted, never allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// Most automation points handed to a plugin in one call of its process
/// function, over all parameters together. Further points are dropped and
/// counted.
pub const POINTS_PER_BLOCK: usize = 4_096;

/// Most parameters that can change in one call of the plugin's process
/// function.
pub const QUEUES_PER_BLOCK: usize = 1_024;

/// Most parameters the plugin can report a change for in one call of its
/// process function, and most points it can report for each.
const OUTPUT_QUEUES: usize = 512;
const OUTPUT_POINTS_PER_QUEUE: usize = 32;

/// Most events that wait for the next block: the ones that fall beyond the
/// end of a block, and the ones of blocks the plugin could not be called
/// for.
pub(crate) const CARRIED_EVENTS: usize = 1_024;

/// How many parameter changes can wait for the audio thread, and how many
/// reports can wait for the main thread.
pub(crate) const QUEUE_CAPACITY: usize = 1_024;

/// Why a block came out silent.
///
/// More reasons may be added, so a `match` on this needs a catch-all arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ProcessFailure {
    /// The plugin's process function returned an error.
    Error,
    /// The plugin refused to start processing. It is asked again on every
    /// block.
    StartRefused,
    /// The processor was prepared for a sample rate other than the one the
    /// plugin was activated with. Deactivate the plugin and activate it
    /// again at the new rate.
    SampleRateMismatch,
}

impl ProcessFailure {
    const ALL: [Self; 3] = [Self::Error, Self::StartRefused, Self::SampleRateMismatch];

    /// This failure as one bit of a set.
    pub(crate) fn bit(self) -> u32 {
        match self {
            Self::Error => 1 << 0,
            Self::StartRefused => 1 << 1,
            Self::SampleRateMismatch => 1 << 2,
        }
    }

    /// The failures whose bits are set in `bits`.
    pub(crate) fn from_bits(bits: u32) -> impl Iterator<Item = Self> {
        Self::ALL
            .into_iter()
            .filter(move |failure| bits & failure.bit() != 0)
    }
}

impl fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Error => "the plugin reported an error while processing",
            Self::StartRefused => "the plugin refused to start processing",
            Self::SampleRateMismatch => {
                "the plugin was activated for a different sample rate than it is asked to run at"
            }
        })
    }
}

/// Why hosting a VST 3 plugin did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Vst3Error {
    /// Nothing exists at this path.
    NotFound(PathBuf),
    /// The path is a bundle folder with no library for this system and
    /// machine in it.
    BundleWithoutBinary(PathBuf),
    /// The operating system could not load the file as a shared library.
    Load {
        /// The file.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
    /// The file is a shared library but exports no `GetPluginFactory`: it
    /// is not a VST 3 module.
    NoEntryPoint(PathBuf),
    /// The module's entry function (`InitDll`, `ModuleEntry` or
    /// `bundleEntry`) returned false.
    EntryRefused,
    /// `GetPluginFactory` returned nothing.
    NoFactory,
    /// The module offers no class with this identifier.
    UnknownClass(ClassId),
    /// The class exists but is not an audio plugin: its category is not
    /// `"Audio Module Class"`.
    NotAnAudioModule(ClassId),
    /// The factory offered the class but made nothing when asked to.
    CreateFailed(ClassId),
    /// The plugin lacks an interface no plugin can be hosted without. The
    /// text names it.
    MissingInterface(&'static str),
    /// The plugin's `initialize` returned an error.
    InitializeFailed,
    /// The plugin does not offer what was asked for, or needs something
    /// this host does not provide. The text says what.
    Unsupported(&'static str),
    /// An argument made no sense. The text says which.
    InvalidArgument(&'static str),
    /// The plugin is already active. Deactivate it first.
    AlreadyActive,
    /// The audio half still exists, so the plugin may be processing. Drop
    /// the [`Vst3Processor`] first.
    ProcessorAlive,
    /// The plugin refused the processing setup: the sample rate, the block
    /// size or 32-bit samples.
    SetupRefused,
    /// The plugin's `setActive` returned an error.
    ActivationFailed,
    /// The plugin has no parameter with this id.
    UnknownParam(ParamId),
    /// Too many parameter changes are waiting for the audio thread, which
    /// is not running or cannot keep up. Nothing was changed; try again.
    QueueFull,
    /// The plugin could not save its state.
    StateSaveFailed,
    /// The plugin tried to save more than [`MAX_STATE_BYTES`].
    StateTooLarge,
    /// The bytes are not a state this host saved. The text says what is
    /// wrong with them.
    NotAState(&'static str),
    /// The state was saved from another class.
    StateOfAnotherClass(ClassId),
    /// The plugin refused the state it was given.
    StateLoadFailed,
    /// The plugin has no controller, and so no editor.
    NoController,
    /// The plugin's controller made no editor when asked for one: it has
    /// none.
    NoView,
    /// The editor is open already. Close it first.
    EditorAlreadyOpen,
    /// No editor is open.
    EditorNotOpen,
    /// The plugin's editor cannot be put in a window of this kind.
    PlatformUnsupported(WindowApi),
    /// The plugin's editor refused a call, and nothing was changed. The
    /// text names the call. An editor that refused to open has been closed
    /// again, and nothing of it is left.
    EditorRefused(&'static str),
}

impl fmt::Display for Vst3Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "there is nothing at {}", path.display()),
            Self::BundleWithoutBinary(path) => write!(
                f,
                "the bundle {} has no library for this system and machine",
                path.display()
            ),
            Self::Load { path, reason } => {
                write!(f, "{} could not be loaded: {reason}", path.display())
            }
            Self::NoEntryPoint(path) => write!(
                f,
                "{} is not a VST 3 module: it has no GetPluginFactory",
                path.display()
            ),
            Self::EntryRefused => f.write_str("the module refused to initialise"),
            Self::NoFactory => f.write_str("the module has no plugin factory"),
            Self::UnknownClass(id) => write!(f, "the module has no class {id}"),
            Self::NotAnAudioModule(id) => write!(f, "the class {id} is not an audio plugin"),
            Self::CreateFailed(id) => write!(f, "the class {id} could not be created"),
            Self::MissingInterface(what) => write!(f, "the plugin does not implement {what}"),
            Self::InitializeFailed => f.write_str("the plugin refused to initialise"),
            Self::Unsupported(what) => write!(f, "not supported: {what}"),
            Self::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Self::AlreadyActive => f.write_str("the plugin is already active"),
            Self::ProcessorAlive => {
                f.write_str("the plugin's audio half still exists and may be processing")
            }
            Self::SetupRefused => f.write_str("the plugin refused the processing setup"),
            Self::ActivationFailed => f.write_str("the plugin refused to activate"),
            Self::UnknownParam(id) => write!(f, "the plugin has no parameter with id {}", id.0),
            Self::QueueFull => {
                f.write_str("too many parameter changes are waiting for the audio thread")
            }
            Self::StateSaveFailed => f.write_str("the plugin could not save its state"),
            Self::StateTooLarge => {
                f.write_str("the plugin's state is larger than this host accepts")
            }
            Self::NotAState(what) => write!(f, "not a saved state: {what}"),
            Self::StateOfAnotherClass(id) => write!(f, "the state belongs to the class {id}"),
            Self::StateLoadFailed => f.write_str("the plugin refused the state"),
            Self::NoController => f.write_str("the plugin has no controller, and so no editor"),
            Self::NoView => f.write_str("the plugin has no editor"),
            Self::EditorAlreadyOpen => f.write_str("the plugin's editor is open already"),
            Self::EditorNotOpen => f.write_str("the plugin's editor is not open"),
            Self::PlatformUnsupported(api) => {
                write!(f, "the plugin's editor cannot be put in a {api:?} window")
            }
            Self::EditorRefused(call) => write!(f, "the plugin's editor refused {call}"),
        }
    }
}

impl std::error::Error for Vst3Error {}

/// What the main thread sends to the audio thread.
///
/// Every message carries the generation it was sent in, the number of
/// states loaded before it; see `shared`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ToAudio {
    /// A parameter is to take a normalized value.
    ParamValue {
        id: u32,
        value: f64,
        generation: u64,
    },
}

/// What the audio thread sends to the main thread, with its generation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FromAudio {
    /// The plugin changed a parameter by itself, to this normalized value.
    ParamValue {
        id: u32,
        value: f64,
        generation: u64,
    },
}
