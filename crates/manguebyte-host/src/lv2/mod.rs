//! Hosting [LV2](https://lv2plug.in) plugins: reading a bundle, loading
//! its plugin and running it as a [`Processor`](manguebyte_dsp::Processor).
//!
//! # A bundle, its Turtle, and the plugin
//!
//! An LV2 plugin is a folder, a bundle, with a `manifest.ttl`, more Turtle
//! files it points at, and the plugin's library. Everything a host needs
//! to list the plugin and connect its ports is in the Turtle files, so
//! [`read_bundle`] describes a bundle's plugins without loading anything:
//! it is safe, and it is how a scanner lists LV2 plugins. The Turtle is
//! read by this crate's own reader, which takes any bundle as hostile
//! input: limits on lengths, nesting and counts, and errors with the line
//! they are on.
//!
//! | Type | What it is | Threads |
//! |---|---|---|
//! | [`Lv2Library`] | A bundle with its libraries loaded | Any; can be cloned and sent |
//! | [`Lv2Plugin`] | One instance: parameters, state, activation | Any, one at a time; can be sent |
//! | [`Lv2Processor`] | The same instance, running: a `Processor<f32>` | One at a time; can be sent |
//!
//! LV2 has no main thread. It sorts a plugin's functions into classes
//! that must not overlap: creating, activating, deactivating, restoring
//! state and cleaning up on one side, connecting ports and running on the
//! other, and saving state, which may happen while the plugin runs. The
//! two halves keep to those rules: the plugin and its processor each take
//! `&mut self`, `run` never shares a lock with anything, and the calls of
//! the first class and saving take one lock between them, which `run`
//! never takes.
//!
//! # Hosting a plugin
//!
//! This example is compiled and not run, since it needs a bundle on disk.
//!
//! ```no_run
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
//! use manguebyte_dsp::{ProcessContext, Processor, Transport};
//! use manguebyte_host::lv2::{Lv2Library, read_bundle};
//!
//! # fn main() -> Result<(), manguebyte_host::lv2::Lv2Error> {
//! // Listing runs nothing from the bundle.
//! for plugin in read_bundle("Mangue Pluck.lv2")? {
//!     println!("{} ({})", plugin.name, plugin.uri);
//! }
//!
//! // SAFETY: the bundle's library is one this program is willing to run.
//! let library = unsafe { Lv2Library::load("Mangue Pluck.lv2") }?;
//! let mut plugin = library.create_with("urn:manguebyte:org.manguebyte.pluck", 48_000.0, 512)?;
//! for param in plugin.params() {
//!     println!("{}: {}..{} {}", param.name, param.min, param.max, param.unit);
//! }
//! let mut processor = plugin.activate(48_000.0, 512)?;
//!
//! let mut events = EventBuffer::with_capacity(16);
//! let note = NoteRef::new(0, 60);
//! events.push(Event { offset: 0, kind: EventKind::NoteOn { note, velocity: 0.8 } }).ok();
//! let transport = Transport::playing_at(120.0);
//! let mut block = AudioBuffer::<f32>::new(2, 512);
//! processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
//!
//! let state = plugin.save_state()?;
//! drop(processor);
//! plugin.load_state(&state)?;
//! # Ok(())
//! # }
//! ```
//!
//! # What is covered
//!
//! | LV2 | Behaviour |
//! |---|---|
//! | Turtle | `manifest.ttl` and the files its plugins point at with `rdfs:seeAlso`, inside the bundle; prefixes, bases, relative IRIs, blank nodes, collections, literals |
//! | Library | `lv2_descriptor`, from a library that must be inside the bundle, or a function of this process that stands for it ([`Lv2Library::from_entry`]) |
//! | Features | `urid:map`, `urid:unmap`, `options:options` (`buf-size:minBlockLength`, `maxBlockLength`, `nominalBlockLength`, `sequenceSize`, `param:sampleRate`), `buf-size:boundedBlockLength`, `log:log`. A plugin that requires any other is refused by name |
//! | Audio ports | One channel each: main inputs, inputs marked `lv2:isSideChain` from the sidechain, main outputs. Never in place |
//! | Control ports | Inputs are the parameters, in their own unit and range, changed on their frames by running the block in pieces; `lv2:freeWheeling` is held at 0 and `lv2:enabled` starts on; the output designated `lv2:latency` (or marked `lv2:reportsLatency`) is the latency |
//! | Atom ports | The input designated `lv2:control`, or the first that takes MIDI, gets MIDI 1.0 on its frames and a `time:Position` per piece if it takes one; others get empty sequences. Outputs are given room before every call of `run`; those that support `midi:MidiEvent` are read after it, and the MIDI 1.0 the plugin sent there is in [`Lv2Processor::output_events`], on its frames |
//! | CV ports | Inputs silent, outputs discarded |
//! | State | `state:interface`, saved together with every parameter's value, by URI and port symbol (see [`Lv2Plugin::save_state`]) |
//!
//! # Limits
//!
//! No plugin user interfaces (`ui:`), no worker (`worker:schedule`), no
//! file paths in state (`state:mapPath`, `state:makePath`), no reading of
//! atoms a plugin writes other than MIDI events (no patch messages, no
//! MIDI 2.0, which LV2 has no event type for), no 64-bit audio, no
//! presets. `log`
//! shows a message whose format is a lone `%s` as its string, and any other
//! format as it is, its conversions unexpanded: C's variadic arguments
//! cannot be read portably from stable Rust (see the comment in the source
//! of the log feature).

mod atom;
mod description;
mod features;
mod library;
mod plugin;
mod processor;
mod state;
mod sys;
mod turtle;

use std::fmt;
use std::path::PathBuf;

use manguebyte_core::ParamId;

pub use description::{
    Direction, Lv2PluginInfo, MAX_PLUGINS_PER_BUNDLE, MAX_PORTS, PortGroup, PortInfo, PortKind,
    PortProperties, read_bundle,
};
pub use library::{Lv2Entry, Lv2Library};
pub use plugin::{Lv2Event, Lv2Failure, Lv2Param, Lv2Plugin};
pub use processor::Lv2Processor;
pub use state::MAX_STATE_BYTES;

/// Largest block size a plugin can be activated with.
pub const MAX_BLOCK_SIZE: usize = 1 << 20;

/// Largest latency taken from a plugin, in frames.
pub const MAX_LATENCY_FRAMES: u32 = 1 << 24;

/// Most events a plugin sends that are kept for one block, however many
/// pieces the block is run in, since that is what
/// [`Lv2Processor::output_events`] returns. Further events are dropped and
/// counted, never allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// Most events that wait for the next block.
pub(crate) const CARRIED_EVENTS: usize = 1_024;

/// How many parameter changes can wait for the audio thread.
pub(crate) const QUEUE_CAPACITY: usize = 1_024;

/// Why hosting an LV2 plugin did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Lv2Error {
    /// Nothing exists at this path.
    NotFound(PathBuf),
    /// The path is not a folder with a `manifest.ttl`.
    NotABundle(PathBuf),
    /// A file of the bundle could not be read.
    Read {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// A Turtle file of the bundle is not valid Turtle, or is beyond this
    /// host's limits.
    Turtle {
        /// The file.
        path: PathBuf,
        /// What is wrong, and on which line.
        reason: String,
    },
    /// A plugin's description is not one this host can use.
    Malformed {
        /// The plugin.
        uri: String,
        /// What is wrong with it.
        what: &'static str,
    },
    /// The operating system could not load a library.
    Load {
        /// The library.
        path: PathBuf,
        /// What the operating system said.
        reason: String,
    },
    /// A library exports no `lv2_descriptor`.
    NoEntryPoint(PathBuf),
    /// The bundle, or its library, has no plugin with this URI.
    UnknownPlugin(String),
    /// The plugin requires a feature this host does not give.
    MissingFeature(String),
    /// The plugin's `instantiate` returned nothing. Holds what it logged.
    InstantiateFailed(String),
    /// The plugin needs something this host does not do.
    Unsupported(&'static str),
    /// An argument made no sense.
    InvalidArgument(&'static str),
    /// A processor exists; this needs the plugin without one.
    ProcessorAlive,
    /// A processor exists already. Drop it first.
    AlreadyActive,
    /// The plugin has no parameter with this id.
    UnknownParam(ParamId),
    /// Too many parameter changes are waiting for the audio thread.
    QueueFull,
    /// The plugin's state extension reported this status when saving, or
    /// stored a value that is not plain data.
    StateSaveFailed(u32),
    /// The state is larger than [`MAX_STATE_BYTES`].
    StateTooLarge,
    /// The bytes are not a state this host saved.
    BadState,
    /// The plugin's state extension reported this status when restoring.
    StateLoadFailed(u32),
}

impl fmt::Display for Lv2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(path) => write!(f, "there is nothing at {}", path.display()),
            Self::NotABundle(path) => {
                write!(
                    f,
                    "{} is not an LV2 bundle: no manifest.ttl",
                    path.display()
                )
            }
            Self::Read { path, reason } => {
                write!(f, "{} could not be read: {reason}", path.display())
            }
            Self::Turtle { path, reason } => write!(f, "{}: {reason}", path.display()),
            Self::Malformed { uri, what } => write!(f, "the plugin {uri} is malformed: {what}"),
            Self::Load { path, reason } => {
                write!(f, "{} could not be loaded: {reason}", path.display())
            }
            Self::NoEntryPoint(path) => {
                write!(f, "{} has no lv2_descriptor", path.display())
            }
            Self::UnknownPlugin(uri) => write!(f, "there is no plugin {uri}"),
            Self::MissingFeature(uri) => {
                write!(
                    f,
                    "the plugin requires {uri}, which this host does not give"
                )
            }
            Self::InstantiateFailed(why) if why.is_empty() => {
                f.write_str("the plugin refused to be created")
            }
            Self::InstantiateFailed(why) => write!(f, "the plugin refused to be created: {why}"),
            Self::Unsupported(what) => write!(f, "not supported: {what}"),
            Self::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Self::ProcessorAlive => f.write_str("the plugin's processor still exists"),
            Self::AlreadyActive => f.write_str("the plugin is already active"),
            Self::UnknownParam(id) => write!(f, "the plugin has no parameter with id {}", id.0),
            Self::QueueFull => {
                f.write_str("too many parameter changes are waiting for the audio thread")
            }
            Self::StateSaveFailed(status) => {
                write!(f, "the plugin could not save its state (status {status})")
            }
            Self::StateTooLarge => f.write_str("the plugin's state is larger than this host keeps"),
            Self::BadState => f.write_str("the bytes are not a saved LV2 state"),
            Self::StateLoadFailed(status) => {
                write!(f, "the plugin refused the state (status {status})")
            }
        }
    }
}

impl std::error::Error for Lv2Error {}
