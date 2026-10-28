//! Hosting Audio Units (version 2): finding components, instantiating
//! them and running them as a [`Processor`](manguebyte_dsp::Processor).
//!
//! # Components, instances, and two ways in
//!
//! An Audio Unit is a component, known by three four-character codes
//! ([`ComponentCodes`]): its type, subtype and manufacturer. macOS keeps a
//! registry of the components installed, read from the `AudioComponents`
//! entry of each `.component` bundle's `Info.plist`, and a host finds them
//! there with `AudioComponentFindNext` and makes an instance with
//! `AudioComponentInstanceNew`; no code of a component runs until then.
//! Everything after that goes through the calls of Apple's `AUComponent.h`:
//! properties, parameters, `AudioUnitRender`.
//!
//! | Type | What it is | Threads |
//! |---|---|---|
//! | [`AuLibrary`] | Components, and how to make their instances | Any; can be cloned and sent |
//! | [`AuPlugin`] | One instance: parameters, channels, state, activation | The thread that created it |
//! | [`AuProcessor`] | The same instance, rendering: a `Processor<f32>` | One at a time; can be sent |
//!
//! [`AuLibrary::system`] holds every component the registry lists and
//! [`AuLibrary::load`] the units of one bundle, both through AudioToolbox,
//! on macOS only. [`AuLibrary::from_factory`] holds a component whose
//! factory function the caller has, and makes its instances in this
//! process, as AudioToolbox does for a version 2 unit: the factory makes
//! the unit's `AudioComponentPlugInInterface`, and its methods are looked
//! up by selector. That works on every system, and is how this crate's
//! tests host the framework's own example units everywhere; on macOS they
//! are also found in the registry and hosted through AudioToolbox, and the
//! two have to sound the same, to the bit.
//!
//! [`components`] lists what the registry holds, and
//! [`read_info_plist`] what a bundle's `Info.plist` says, without running
//! anything. The [`scan`](crate::scan) module lists `.component` bundles
//! in a process of their own, instantiating each unit there, so that one
//! that crashes or hangs is reported.
//!
//! # Hosting a unit
//!
//! This example is compiled and not run: it needs a Mac with the
//! framework's example instrument installed.
//!
//! ```no_run
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
//! use manguebyte_dsp::{ProcessContext, Processor, Transport};
//! use manguebyte_host::au::AuLibrary;
//!
//! # fn main() -> Result<(), manguebyte_host::au::AuError> {
//! // SAFETY: the registered components are ones this program is willing
//! // to run.
//! let library = unsafe { AuLibrary::system() }?;
//! for unit in library.components() {
//!     println!("{} by {} ({})", unit.name, unit.vendor, unit.codes);
//! }
//! let pluck = library
//!     .components()
//!     .iter()
//!     .find(|unit| unit.name == "Mangue Pluck")
//!     .expect("the example instrument is installed")
//!     .codes;
//! let mut plugin = library.create(pluck)?;
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
//! | Audio Unit | Behaviour |
//! |---|---|
//! | Discovery | `AudioComponentFindNext` over every component, with its name (`AudioComponentCopyName`, split at `Maker: Name`), version and codes; output units and format converters are left out. A bundle's `Info.plist`, XML or binary |
//! | Instances | `AudioComponentInstanceNew` and `AudioComponentInstanceDispose`; or a factory function, `Open`, `Lookup` and `Close` |
//! | Elements | Input element 0 is the main input and input element 1 the sidechain, each if the unit has it; output element 0. Their channels are what their stream formats say, and [`AuPlugin::set_channels`] changes the main ones, as `SupportedNumChannels` allows |
//! | Stream format | 32-bit float, one buffer per channel, at the activation's sample rate, on every element |
//! | Slices | `MaximumFramesPerSlice` is the activation's block size; a unit that refuses it keeps its own, and longer blocks are rendered in slices of that |
//! | Input | A render callback on each input element, which copies the block's audio (and [`ProcessContext::sidechain`](manguebyte_dsp::ProcessContext::sidechain)) into the unit's buffers or lends the unit the host's own |
//! | Parameters | `ParameterList` and `ParameterInfo` in the global scope, in the unit's own unit and range: switches (`Boolean`) and menus (`Indexed`) move in steps; names from `cfNameString`; text both ways through `ParameterStringFromValue` and `ParameterValueFromString` for a unit with `ValuesHaveStrings` |
//! | Automation | Each [`EventKind::ParamValue`](manguebyte_dsp::event::EventKind::ParamValue) through `AudioUnitScheduleParameters`, as an immediate change on its frame of the slice; `AudioUnitSetParameter` from the main thread, which the unit hears from the next slice |
//! | MIDI | For music devices, music effects and MIDI processors, `MusicDeviceMIDIEvent` on the event's frame of the slice: notes as MIDI 1.0, pressure as polyphonic aftertouch, MIDI as it is |
//! | MIDI out | For a unit with `MIDIOutputCallbackInfo`, a `MIDIOutputCallback`, whose `MIDIPacketList`s are read into [`AuProcessor::output_events`] on the frames their packets are stamped with |
//! | Transport | `HostCallbacks`: beat and tempo, the frames to the next beat, the time signature and the bar's first beat, playing, recording, a change of either or a jump, the position in frames and the cycle, for each slice |
//! | Latency, tail | `Latency` and `TailTime`, in seconds, as frames at the activation's rate; latency read at activation and on [`AuPlugin::pump`] |
//! | State | `ClassInfo`, as the XML of its property list (see [`AuPlugin::save_state`]) |
//! | Bypass | `BypassEffect`, for a unit that has it ([`AuPlugin::set_bypass`]) |
//!
//! Events that reach a processor on one frame reach the unit in the order
//! they came, parameter changes and MIDI alike. An event at or beyond the
//! end of a block is delivered on the first frame of the next; 1,024 can
//! wait that way.
//!
//! # Limits
//!
//! No editors (`CocoaUI`), no Audio Unit version 3 extensions beyond what
//! `AudioComponentInstanceNew` makes of them, no factory presets, no MIDI
//! 2.0 read back from a unit (`MIDIOutputEventListCallback`), no parameter
//! changes read back from a unit, no channel layouts other
//! than the stream formats' channel counts, no input or output elements
//! beyond the main ones and one sidechain, and no 64-bit audio. A
//! parameter the unit itself changes is seen when it is read, not
//! reported. See `docs/TECH_DEBT.md`, item 93.

// The constants keep Apple's names, which are not upper case.
#![allow(non_upper_case_globals)]

mod cf;
mod channels;
mod component;
mod events;
mod library;
mod params;
mod plist;
mod plugin;
mod processor;
mod state;
mod sys;
mod transport;
mod unit;

use std::fmt;
use std::path::PathBuf;

use manguebyte_core::ParamId;

pub use channels::AuBus;
pub use component::{
    AuComponentInfo, ComponentCodes, NotComponentCodes, read_info_plist, version_text,
};
pub use library::{AuFactory, AuLibrary, components};
pub use params::AuParam;
pub use plugin::{AuEvent, AuFailure, AuPlugin};
pub use processor::AuProcessor;
pub use state::MAX_STATE_BYTES;

/// Largest block size a unit can be activated with.
pub const MAX_BLOCK_SIZE: usize = 1 << 20;

/// Largest latency taken from a unit, in frames.
pub const MAX_LATENCY_FRAMES: u32 = 1 << 24;

/// Largest `Info.plist` read from a bundle.
pub(crate) const MAX_INFO_PLIST_BYTES: usize = 4 << 20;

/// Most events a unit sends that are kept for one block, however many
/// slices the block is rendered in, since that is what
/// [`AuProcessor::output_events`] returns. Further events are dropped and
/// counted, never allocated for.
pub const EVENTS_PER_BLOCK: usize = 4_096;

/// Most events that wait for the next block.
pub(crate) const CARRIED_EVENTS: usize = 1_024;

/// What a call on a unit returns: `noErr` or the status it gave.
pub(crate) type Status = Result<(), i32>;

/// Why hosting an Audio Unit did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuError {
    /// Finding and instantiating registered components needs AudioToolbox,
    /// which only macOS has. A unit can still be hosted from its factory.
    NotOnThisSystem,
    /// Nothing exists at this path.
    NotFound(PathBuf),
    /// The path is not a `.component` bundle with an `Info.plist`.
    NotABundle(PathBuf),
    /// A bundle's `Info.plist` could not be read, or does not list units.
    BadInfoPlist(String),
    /// The library has no component with these codes, or the system has
    /// none registered.
    UnknownComponent(String),
    /// The unit could not be made: what it, or AudioToolbox, answered.
    InstantiateFailed(Option<i32>),
    /// The unit needs something this host does not do.
    Unsupported(&'static str),
    /// The unit refused a call, with this status.
    Status {
        /// What was asked of it.
        call: &'static str,
        /// The `OSStatus` it gave.
        status: i32,
    },
    /// The unit does not take this many channels.
    Channels {
        /// Main input channels asked for.
        inputs: u32,
        /// Output channels asked for.
        outputs: u32,
    },
    /// An argument made no sense.
    InvalidArgument(&'static str),
    /// A processor exists; this needs the unit without one.
    ProcessorAlive,
    /// A processor exists already. Drop it first.
    AlreadyActive,
    /// The unit has no parameter with this id.
    UnknownParam(ParamId),
    /// The unit gave no state to save.
    StateSaveFailed,
    /// The state is larger than [`MAX_STATE_BYTES`].
    StateTooLarge,
    /// The bytes are not a state this host saved.
    BadState,
}

impl fmt::Display for AuError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotOnThisSystem => {
                f.write_str("Audio Unit components can be found and loaded on macOS only")
            }
            Self::NotFound(path) => write!(f, "there is nothing at {}", path.display()),
            Self::NotABundle(path) => write!(
                f,
                "{} is not a component bundle: no Contents/Info.plist",
                path.display()
            ),
            Self::BadInfoPlist(why) => write!(f, "the bundle's Info.plist is unusable: {why}"),
            Self::UnknownComponent(codes) => write!(f, "there is no component {codes}"),
            Self::InstantiateFailed(Some(status)) => {
                write!(f, "the unit could not be made (status {status})")
            }
            Self::InstantiateFailed(None) => f.write_str("the unit could not be made"),
            Self::Unsupported(what) => write!(f, "not supported: {what}"),
            Self::Status { call, status } => {
                write!(f, "the unit refused {call} (status {status})")
            }
            Self::Channels { inputs, outputs } => write!(
                f,
                "the unit does not take {inputs} input and {outputs} output channels"
            ),
            Self::InvalidArgument(what) => write!(f, "invalid argument: {what}"),
            Self::ProcessorAlive => f.write_str("the unit's processor still exists"),
            Self::AlreadyActive => f.write_str("the unit is already active"),
            Self::UnknownParam(id) => write!(f, "the unit has no parameter with id {}", id.0),
            Self::StateSaveFailed => f.write_str("the unit gave no state"),
            Self::StateTooLarge => f.write_str("the unit's state is larger than this host keeps"),
            Self::BadState => f.write_str("the bytes are not a saved Audio Unit state"),
        }
    }
}

impl std::error::Error for AuError {}

/// The error for a call the unit refused.
pub(crate) fn refused(call: &'static str) -> impl Fn(i32) -> AuError {
    move |status| AuError::Status { call, status }
}
