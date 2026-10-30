//! The main-thread half of a hosted plugin.

mod editor;

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap_sys::events::{clap_event_header, clap_output_events};
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, clap_plugin_audio_ports_activation,
};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::log::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::posix_fd_support::{CLAP_EXT_POSIX_FD_SUPPORT, clap_plugin_posix_fd_support};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_plugin_timer_support};
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, clap_process_status};
use clap_sys::stream::{clap_istream, clap_ostream};
use manguebyte_core::ParamId;
use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_dsp::event::EventKind;

use super::events::{
    InEvent, NoteRoute, OutputKind, ParamSlot, find_param, from_clap, held_to_declared, to_clap,
};
use super::host::{Shared, guard, request};
use super::library::{ClapLibrary, PluginDescriptor};
use super::processor::{Calls, ClapProcessor, Setup, input_events};
use super::text::{bounded_text, field_text};
use super::{
    ClapError, EVENTS_PER_BLOCK, FromAudio, HostInfo, MAX_BLOCK_SIZE, MAX_CHANNELS_PER_PORT,
    MAX_LATENCY_FRAMES, MAX_PARAMS, MAX_PORTS, MAX_SCRATCH_SAMPLES, MAX_STATE_BYTES,
    ProcessFailure, QUEUE_CAPACITY, ToAudio,
};
use crate::editor::EditorRequest;

/// Room given to a plugin for the text of one parameter value.
const VALUE_TEXT_BYTES: usize = 256;

/// Most bytes read of the word that names a port's channel layout.
const PORT_TYPE_BYTES: usize = 64;

/// What a plugin says about one of its parameters.
///
/// Values are in the plugin's own scale, which runs from
/// [`min`](Self::min) to [`max`](Self::max) and means whatever the plugin
/// decides: a frequency, a position between 0 and 1, a number of steps.
/// [`ClapPlugin::param_to_text`] turns a value into what a person should
/// see.
#[derive(Debug, Clone, PartialEq)]
pub struct ParamInfo {
    /// The number that stands for the parameter in events, automation and
    /// every call here. It stays the same from session to session.
    pub id: ParamId,
    /// The name shown to people.
    pub name: String,
    /// The group the plugin files the parameter under, as a path with
    /// slashes, or empty.
    pub module: String,
    /// The lowest value.
    pub min: f64,
    /// The highest value.
    pub max: f64,
    /// The value the parameter starts at, held to the range.
    pub default: f64,
    /// What may be done with the parameter.
    pub flags: ParamFlags,
}

/// What may be done with a parameter, as the plugin declares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ParamFlags {
    /// The value moves in whole steps.
    pub stepped: bool,
    /// The value wraps around, like an angle.
    pub periodic: bool,
    /// Not to be shown to people.
    pub hidden: bool,
    /// The host may read it but not change it.
    pub read_only: bool,
    /// This is the plugin's bypass switch.
    pub bypass: bool,
    /// May be automated.
    pub automatable: bool,
    /// May be modulated: offset without changing the stored value.
    pub modulatable: bool,
    /// A change only takes effect once audio is processed.
    pub requires_process: bool,
    /// The steps are named choices, not numbers.
    pub enumerated: bool,
    /// Every flag as CLAP defines it, for the ones not broken out above:
    /// per-note, per-key, per-channel and per-port automation and
    /// modulation.
    pub bits: u32,
}

impl ParamFlags {
    fn from_bits(bits: u32) -> Self {
        let has = |flag: u32| bits & flag != 0;
        Self {
            stepped: has(CLAP_PARAM_IS_STEPPED),
            periodic: has(CLAP_PARAM_IS_PERIODIC),
            hidden: has(CLAP_PARAM_IS_HIDDEN),
            read_only: has(CLAP_PARAM_IS_READONLY),
            bypass: has(CLAP_PARAM_IS_BYPASS),
            automatable: has(CLAP_PARAM_IS_AUTOMATABLE),
            modulatable: has(CLAP_PARAM_IS_MODULATABLE),
            requires_process: has(CLAP_PARAM_REQUIRES_PROCESS),
            enumerated: has(CLAP_PARAM_IS_ENUM),
            bits,
        }
    }
}

/// What a plugin says about one of its audio ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPortInfo {
    /// The port's identifier, which stays the same when ports are added or
    /// removed.
    pub id: u32,
    /// The name shown to people.
    pub name: String,
    /// How many channels the port has.
    pub channels: u32,
    /// Whether this is the main port of its direction. CLAP allows one,
    /// and it has to be the first.
    pub is_main: bool,
    /// How the channels are laid out, as CLAP's word for it: `"mono"`,
    /// `"stereo"`, `"surround"`, `"ambisonic"`. `None` if the plugin does
    /// not say.
    pub port_type: Option<String>,
    /// The identifier of the port on the other side that this one may
    /// share memory with. This host always gives a plugin separate memory,
    /// which every plugin has to accept.
    pub in_place_pair: Option<u32>,
    /// Whether the plugin could also process this port in 64-bit floats.
    /// This host uses 32-bit.
    pub supports_64_bit: bool,
}

/// What a plugin says about one of its note ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotePortInfo {
    /// The port's identifier.
    pub id: u32,
    /// The name shown to people.
    pub name: String,
    /// Whether the port takes CLAP's own note events, with note ids and
    /// note expression.
    pub clap: bool,
    /// Whether the port takes MIDI 1.0 messages.
    pub midi: bool,
    /// Whether the port takes MIDI with MPE. This host sends such a port
    /// plain MIDI, also when MPE is the only kind of MIDI it declares: each
    /// note and each [`EventKind::Midi`] message goes out on the channel it
    /// carries. The host does not arrange notes into MPE zones; an
    /// application that wants MPE proper chooses the channels itself.
    pub midi_mpe: bool,
    /// Whether the port takes MIDI 2.0. This host does not send it.
    pub midi2: bool,
}

/// How long a plugin goes on sounding after its input stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tail {
    /// This many frames.
    Frames(u32),
    /// For ever: an oscillator, a held note, a frozen reverb.
    Infinite,
}

impl Tail {
    /// CLAP reports a tail as a frame count in which anything from
    /// `i32::MAX` up means "infinite".
    pub(crate) fn from_frames(frames: u32) -> Self {
        if frames >= i32::MAX as u32 {
            Self::Infinite
        } else {
            Self::Frames(frames)
        }
    }
}

/// How serious a logged message is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LogSeverity {
    /// For developers.
    Debug,
    /// Something happened.
    Info,
    /// Something may be wrong.
    Warning,
    /// Something is wrong.
    Error,
    /// Something is wrong and the plugin cannot go on.
    Fatal,
    /// The plugin says the host broke a rule.
    HostMisbehaving,
    /// The plugin broke a rule. Messages of this kind also come from the
    /// host itself, when it refuses something the plugin did.
    PluginMisbehaving,
}

impl LogSeverity {
    fn from_clap(severity: clap_log_severity) -> Self {
        match severity {
            CLAP_LOG_DEBUG => Self::Debug,
            CLAP_LOG_INFO => Self::Info,
            CLAP_LOG_WARNING => Self::Warning,
            CLAP_LOG_ERROR => Self::Error,
            CLAP_LOG_FATAL => Self::Fatal,
            CLAP_LOG_HOST_MISBEHAVING => Self::HostMisbehaving,
            CLAP_LOG_PLUGIN_MISBEHAVING => Self::PluginMisbehaving,
            // A level CLAP does not define is worth a look.
            _ => Self::Warning,
        }
    }
}

/// What about a plugin's parameters has changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct ParamsRescan {
    /// Values changed without events. Read them with
    /// [`ClapPlugin::param_value`].
    pub values: bool,
    /// The text for some values changed.
    pub text: bool,
    /// Names, groups or flags changed. [`ClapPlugin::params`] already
    /// shows the new ones.
    pub info: bool,
    /// The whole list was replaced. [`ClapPlugin::params`] already shows
    /// the new one.
    pub all: bool,
}

/// Something a plugin asked for or reported, as returned by
/// [`ClapPlugin::pump`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum HostEvent {
    /// The plugin asks to be deactivated and activated again, usually
    /// because its latency or its ports are about to change. Drop the
    /// [`ClapProcessor`], call [`ClapPlugin::deactivate`], then
    /// [`ClapPlugin::activate`].
    RestartRequested,
    /// The plugin has something to do and asks to be processed again, for
    /// example after it went to sleep.
    ProcessRequested,
    /// The plugin has parameter changes to exchange and asks for a block
    /// to be processed or for [`ClapProcessor::flush`]. Only reported
    /// while an audio half exists; otherwise the flush has already been
    /// done.
    FlushRequested,
    /// The plugin changed a parameter by itself, typically from its own
    /// interface. The value is in the plugin's own scale, held to the range
    /// the plugin declared for the parameter. A report about a parameter
    /// that was not in the plugin's list when it was made does not get this
    /// far: it is dropped, and remarked on in a [`Log`](Self::Log) event.
    ParamValue {
        /// The parameter.
        param: ParamId,
        /// Its new value.
        value: f64,
    },
    /// A control in the plugin's interface was taken hold of.
    GestureBegin {
        /// The parameter.
        param: ParamId,
    },
    /// The control was let go.
    GestureEnd {
        /// The parameter.
        param: ParamId,
    },
    /// Something about the parameters changed without events.
    ParamsRescanned(ParamsRescan),
    /// The plugin asks the host to forget what it has recorded for a
    /// parameter.
    ParamClearRequested {
        /// The parameter.
        param: ParamId,
        /// Forget every reference to it.
        all: bool,
        /// Forget its automation.
        automation: bool,
        /// Forget its modulation.
        modulation: bool,
    },
    /// The plugin's state differs from what was last saved or loaded.
    StateDirty,
    /// The plugin's latency is now this many frames.
    LatencyChanged(u32),
    /// How long the plugin rings out has changed. See
    /// [`ClapPlugin::tail`].
    TailChanged,
    /// The plugin's audio ports changed, and the lists have been read
    /// again.
    AudioPortsChanged,
    /// The plugin's note ports changed, and the lists have been read
    /// again.
    NotePortsChanged,
    /// At least one block came out silent, for this reason, since the last
    /// call. [`ClapPlugin::failed_blocks`] counts them.
    ProcessFailed(ProcessFailure),
    /// A message from the plugin, or from the host about the plugin.
    Log {
        /// How serious it is.
        severity: LogSeverity,
        /// The text, cut to a fixed length.
        message: String,
    },
    /// The plugin's editor asks the application for something its window
    /// has to do: see [`EditorRequest`]. Only reported while the editor is
    /// open. A [`EditorRequest::Closed`] comes after the editor has been
    /// destroyed, so nothing is left to close.
    Editor(EditorRequest),
}

impl From<FromAudio> for HostEvent {
    fn from(report: FromAudio) -> Self {
        match report {
            FromAudio::ParamValue { id, value } => Self::ParamValue {
                param: ParamId(id),
                value,
            },
            FromAudio::GestureBegin(id) => Self::GestureBegin { param: ParamId(id) },
            FromAudio::GestureEnd(id) => Self::GestureEnd { param: ParamId(id) },
        }
    }
}

/// The plugin's side of the extensions this host uses, copied once.
#[derive(Default)]
struct Extensions {
    params: Option<clap_plugin_params>,
    state: Option<clap_plugin_state>,
    audio_ports: Option<clap_plugin_audio_ports>,
    note_ports: Option<clap_plugin_note_ports>,
    latency: Option<clap_plugin_latency>,
    tail: Option<clap_plugin_tail>,
    ports_activation: Option<clap_plugin_audio_ports_activation>,
    /// Only kept if it has every function an embedded editor needs.
    gui: Option<clap_plugin_gui>,
    timer: Option<clap_plugin_timer_support>,
    fd: Option<clap_plugin_posix_fd_support>,
}

/// The main thread's ends of the two queues to the audio half.
struct Link {
    to_audio: Producer<ToAudio>,
    from_audio: Consumer<FromAudio>,
}

/// A hosted plugin as the main thread deals with it.
///
/// It owns everything CLAP reserves for the main thread: the lists of
/// ports and parameters, values and their text, state, activation, and the
/// plugin's requests to the host. Audio is processed by the
/// [`ClapProcessor`] that [`activate`](Self::activate) returns.
///
/// The value cannot leave the thread it was created on, and the compiler
/// enforces that:
///
/// ```compile_fail,E0277
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::clap::ClapPlugin>();
/// ```
///
/// The same check passes for the audio half and for the library, which
/// may be sent:
///
/// ```
/// fn can_be_sent<T: Send>() {}
/// can_be_sent::<manguebyte_host::clap::ClapProcessor>();
/// can_be_sent::<manguebyte_host::clap::ClapLibrary>();
/// ```
///
/// Call [`pump`](Self::pump) regularly, for instance from a timer: it is
/// where the plugin's requests are answered.
///
/// The example in the [module documentation](super) takes a plugin through
/// all of this, from creation to a restored state, and runs as a test.
///
/// # Dropping
///
/// Dropping the plugin deactivates and destroys it. If its audio half
/// still exists, the plugin lives on until that is dropped too. Should
/// that happen on another thread after this value is gone, the plugin can
/// no longer be destroyed where CLAP requires, and is left loaded instead:
/// drop the audio half first. This holds whether or not the thread that
/// created the plugin is still running by then: no later thread is ever
/// taken for it.
pub struct ClapPlugin {
    shared: Arc<Shared>,
    plugin: *const clap_plugin,
    table: clap_plugin,
    activate: unsafe extern "C" fn(*const clap_plugin, f64, u32, u32) -> bool,
    deactivate: unsafe extern "C" fn(*const clap_plugin),
    process: unsafe extern "C" fn(*const clap_plugin, *const clap_process) -> clap_process_status,
    descriptor: PluginDescriptor,
    extensions: Extensions,
    params: Vec<ParamInfo>,
    /// What the audio thread needs of each parameter, sorted by id.
    slots: Vec<ParamSlot>,
    audio_inputs: Vec<AudioPortInfo>,
    audio_outputs: Vec<AudioPortInfo>,
    note_inputs: Vec<NotePortInfo>,
    note_outputs: Vec<NotePortInfo>,
    /// Why the plugin's ports cannot be served, if they cannot.
    layout_problem: Option<&'static str>,
    link: Option<Link>,
    /// Events gathered between calls to `pump`.
    waiting: Vec<HostEvent>,
    /// Most bytes a state stream moves per call.
    stream_chunk: usize,
    /// The editor, while it is open.
    editor: Option<editor::OpenEditor>,
    /// The ids of the timers due in one `pump`, gathered before any of
    /// them fires. Kept between calls so that it is allocated once.
    due_timers: Vec<u32>,
}

impl ClapPlugin {
    /// Creates and initialises the plugin `id` of `library`.
    pub(crate) fn create(
        library: ClapLibrary,
        id: &str,
        info: &HostInfo,
    ) -> Result<Self, ClapError> {
        let unknown = || ClapError::UnknownPlugin(id.to_owned());
        let descriptor = library.descriptor(id).cloned().ok_or_else(unknown)?;
        let c_id = CString::new(id).map_err(|_| unknown())?;

        let shared = Shared::new(info, library.clone());
        // SAFETY: the host pointer is inside `shared`, which outlives the
        // plugin: it is what destroys it.
        let plugin = unsafe { library.create_raw(shared.host_ptr(), &c_id) };
        if plugin.is_null() || !plugin.is_aligned() {
            return Err(ClapError::CreateFailed(id.to_owned()));
        }
        // SAFETY: not null, aligned, and returned by the factory as a
        // plugin, which stays valid until it is destroyed.
        let table = unsafe { *plugin };
        if table.destroy.is_none() {
            // It cannot even be destroyed, so it is left where it is, and
            // so is the host it was given a pointer to.
            std::mem::forget(shared);
            return Err(ClapError::MalformedPlugin("destroy"));
        }
        // From here on the plugin is destroyed again if anything is wrong
        // with it, which is also what CLAP asks for when `init` fails.
        shared.set_plugin(plugin);
        let checked = (|| {
            let init = table.init.ok_or(ClapError::MalformedPlugin("init"))?;
            let activate = table
                .activate
                .ok_or(ClapError::MalformedPlugin("activate"))?;
            let deactivate = table
                .deactivate
                .ok_or(ClapError::MalformedPlugin("deactivate"))?;
            let process = table.process.ok_or(ClapError::MalformedPlugin("process"))?;
            // SAFETY: the plugin was just created, and this thread is from
            // now on its main thread.
            if !unsafe { init(plugin) } {
                return Err(ClapError::PluginInitFailed);
            }
            Ok((activate, deactivate, process))
        })();
        let (activate, deactivate, process) = match checked {
            Ok(calls) => calls,
            Err(error) => {
                shared.close();
                return Err(error);
            }
        };

        let mut this = Self {
            shared,
            plugin,
            table,
            activate,
            deactivate,
            process,
            descriptor,
            extensions: Extensions::default(),
            params: Vec::new(),
            slots: Vec::new(),
            audio_inputs: Vec::new(),
            audio_outputs: Vec::new(),
            note_inputs: Vec::new(),
            note_outputs: Vec::new(),
            layout_problem: None,
            link: None,
            waiting: Vec::new(),
            stream_chunk: usize::MAX,
            editor: None,
            due_timers: Vec::new(),
        };
        this.extensions = Extensions {
            params: this.extension(CLAP_EXT_PARAMS),
            state: this.extension(CLAP_EXT_STATE),
            audio_ports: this.extension(CLAP_EXT_AUDIO_PORTS),
            note_ports: this.extension(CLAP_EXT_NOTE_PORTS),
            latency: this.extension(CLAP_EXT_LATENCY),
            tail: this.extension(CLAP_EXT_TAIL),
            ports_activation: this.extension(CLAP_EXT_AUDIO_PORTS_ACTIVATION),
            gui: this.extension(CLAP_EXT_GUI).filter(editor::is_complete),
            timer: this.extension(CLAP_EXT_TIMER_SUPPORT),
            fd: this.extension(CLAP_EXT_POSIX_FD_SUPPORT),
        };
        this.read_params();
        this.read_audio_ports();
        this.read_note_ports();
        Ok(this)
    }

    /// Asks the plugin for one of its extensions and copies the table.
    fn extension<T: Copy>(&self, id: &CStr) -> Option<T> {
        let get = self.table.get_extension?;
        // SAFETY: the plugin is initialised, `id` is a C string, and CLAP
        // allows this function on any thread.
        let table = unsafe { get(self.plugin, id.as_ptr()) }.cast::<T>();
        if table.is_null() || !table.is_aligned() {
            return None;
        }
        // SAFETY: not null, aligned, and returned by the plugin as the
        // table of the extension `id` names, which is a `T`. Extension
        // tables stay valid for the plugin's lifetime.
        Some(unsafe { *table })
    }

    /// Leaves a note about the plugin for the next `pump`.
    fn warn(&mut self, message: String) {
        self.waiting.push(HostEvent::Log {
            severity: LogSeverity::PluginMisbehaving,
            message,
        });
    }

    // --- What the plugin is ----------------------------------------------

    /// What the plugin said about itself before it was created.
    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.descriptor
    }

    /// The plugin's audio inputs, in the plugin's order. A main input, if
    /// there is one, is first.
    pub fn audio_inputs(&self) -> &[AudioPortInfo] {
        &self.audio_inputs
    }

    /// The plugin's audio outputs, in the plugin's order.
    pub fn audio_outputs(&self) -> &[AudioPortInfo] {
        &self.audio_outputs
    }

    /// The plugin's note inputs. Empty for a plugin that takes no notes.
    pub fn note_inputs(&self) -> &[NotePortInfo] {
        &self.note_inputs
    }

    /// The plugin's note outputs.
    pub fn note_outputs(&self) -> &[NotePortInfo] {
        &self.note_outputs
    }

    fn read_audio_ports(&mut self) {
        self.layout_problem = None;
        self.audio_inputs = self.audio_port_list(true);
        self.audio_outputs = self.audio_port_list(false);
    }

    fn audio_port_list(&mut self, is_input: bool) -> Vec<AudioPortInfo> {
        let mut ports = Vec::new();
        let Some((count, get)) = self
            .extensions
            .audio_ports
            .and_then(|ext| ext.count.zip(ext.get))
        else {
            return ports;
        };
        // SAFETY: the plugin is initialised and this is its main thread.
        let declared = unsafe { count(self.plugin, is_input) } as usize;
        if declared > MAX_PORTS {
            self.layout_problem = Some("more audio ports than this host serves");
        }
        for index in 0..declared.min(MAX_PORTS) {
            // SAFETY: zero is a valid value for every field: numbers, an
            // array of characters and a null pointer.
            let mut raw: clap_audio_port_info = unsafe { std::mem::zeroed() };
            // SAFETY: as above, with a struct this function owns.
            if !unsafe { get(self.plugin, index as u32, is_input, &raw mut raw) } {
                // The plugin counted a port it will not describe. Its
                // process call would still expect a buffer for it.
                self.layout_problem = Some("an audio port the plugin counts but does not describe");
                break;
            }
            if raw.channel_count as usize > MAX_CHANNELS_PER_PORT {
                self.layout_problem =
                    Some("an audio port with more channels than this host serves");
            }
            // SAFETY: a port type is null or one of CLAP's NUL-terminated
            // constants.
            let port_type = unsafe { bounded_text(raw.port_type, PORT_TYPE_BYTES) };
            ports.push(AudioPortInfo {
                id: raw.id,
                name: field_text(&raw.name),
                channels: raw.channel_count,
                // CLAP has the main port first, so a flag anywhere else
                // means nothing.
                is_main: index == 0 && raw.flags & CLAP_AUDIO_PORT_IS_MAIN != 0,
                port_type: (!port_type.is_empty()).then_some(port_type),
                in_place_pair: (raw.in_place_pair != CLAP_INVALID_ID).then_some(raw.in_place_pair),
                supports_64_bit: raw.flags & CLAP_AUDIO_PORT_SUPPORTS_64BITS != 0,
            });
        }
        ports
    }

    fn read_note_ports(&mut self) {
        self.note_inputs = self.note_port_list(true);
        self.note_outputs = self.note_port_list(false);
    }

    fn note_port_list(&self, is_input: bool) -> Vec<NotePortInfo> {
        let mut ports = Vec::new();
        let Some((count, get)) = self
            .extensions
            .note_ports
            .and_then(|ext| ext.count.zip(ext.get))
        else {
            return ports;
        };
        // SAFETY: the plugin is initialised and this is its main thread.
        let declared = (unsafe { count(self.plugin, is_input) } as usize).min(MAX_PORTS);
        for index in 0..declared {
            // SAFETY: zero is a valid value for every field.
            let mut raw: clap_note_port_info = unsafe { std::mem::zeroed() };
            // SAFETY: as above, with a struct this function owns.
            if !unsafe { get(self.plugin, index as u32, is_input, &raw mut raw) } {
                break;
            }
            let has = |dialect: clap_note_dialect| raw.supported_dialects & dialect != 0;
            ports.push(NotePortInfo {
                id: raw.id,
                name: field_text(&raw.name),
                clap: has(CLAP_NOTE_DIALECT_CLAP),
                midi: has(CLAP_NOTE_DIALECT_MIDI),
                midi_mpe: has(CLAP_NOTE_DIALECT_MIDI_MPE),
                midi2: has(CLAP_NOTE_DIALECT_MIDI2),
            });
        }
        ports
    }

    // --- Parameters ------------------------------------------------------

    /// The plugin's parameters, in the plugin's order.
    ///
    /// At most [`MAX_PARAMS`] are listed. A parameter whose range is not a
    /// pair of numbers with the lower first, or whose id repeats an earlier
    /// one, is left out and reported through [`pump`](Self::pump).
    pub fn params(&self) -> &[ParamInfo] {
        &self.params
    }

    /// The parameter with this id, if the plugin has one.
    pub fn param(&self, id: ParamId) -> Option<&ParamInfo> {
        let slot = find_param(&self.slots, id.0)?;
        self.params.get(slot.index as usize)
    }

    fn read_params(&mut self) {
        self.params.clear();
        self.slots.clear();
        let Some((count, get)) = self
            .extensions
            .params
            .and_then(|ext| ext.count.zip(ext.get_info))
        else {
            return;
        };
        // SAFETY: the plugin is initialised and this is its main thread.
        let declared = unsafe { count(self.plugin) } as usize;
        if declared > MAX_PARAMS {
            self.warn(format!(
                "the plugin declares {declared} parameters; the first {MAX_PARAMS} are used"
            ));
        }
        let mut refused = 0usize;
        for index in 0..declared.min(MAX_PARAMS) {
            // SAFETY: zero is a valid value for every field: numbers,
            // arrays of characters and a null pointer.
            let mut raw: clap_param_info = unsafe { std::mem::zeroed() };
            // SAFETY: as above, with a struct this function owns.
            if !unsafe { get(self.plugin, index as u32, &raw mut raw) } {
                refused += 1;
                continue;
            }
            let sound = raw.id != CLAP_INVALID_ID
                && raw.min_value.is_finite()
                && raw.max_value.is_finite()
                && raw.min_value <= raw.max_value;
            if !sound {
                refused += 1;
                continue;
            }
            self.slots.push(ParamSlot {
                id: raw.id,
                cookie: raw.cookie,
                min: raw.min_value,
                max: raw.max_value,
                index: self.params.len() as u32,
            });
            self.params.push(ParamInfo {
                id: ParamId(raw.id),
                name: field_text(&raw.name),
                module: field_text(&raw.module),
                min: raw.min_value,
                max: raw.max_value,
                default: if raw.default_value.is_nan() {
                    raw.min_value
                } else {
                    raw.default_value.clamp(raw.min_value, raw.max_value)
                },
                flags: ParamFlags::from_bits(raw.flags),
            });
        }

        // The audio thread finds parameters by id in a sorted table. Two
        // parameters with one id cannot be told apart, so only the first
        // is kept.
        self.slots.sort_by_key(|slot| (slot.id, slot.index));
        let before = self.slots.len();
        self.slots.dedup_by_key(|slot| slot.id);
        if self.slots.len() != before {
            refused += before - self.slots.len();
            let mut kept = vec![false; self.params.len()];
            for slot in &self.slots {
                kept[slot.index as usize] = true;
            }
            let mut position = 0;
            self.params.retain(|_| {
                position += 1;
                kept[position - 1]
            });
            // The list got shorter, so each slot is told where its
            // parameter now is.
            let mut order: Vec<usize> = (0..self.slots.len()).collect();
            order.sort_by_key(|&slot| self.slots[slot].index);
            for (place, slot) in order.into_iter().enumerate() {
                self.slots[slot].index = place as u32;
            }
        }
        if refused > 0 {
            self.warn(format!(
                "{refused} parameter(s) were left out: no description, a range that is not \
                 two numbers in order, or an id already taken"
            ));
        }
    }

    /// The current value of a parameter, in the plugin's own scale, as the
    /// plugin reports it, held to the range the plugin declared for the
    /// parameter.
    ///
    /// A value set with [`set_param`](Self::set_param) while the plugin is
    /// active shows up here once the audio thread has processed a block.
    ///
    /// Returns `None` if the plugin has no such parameter, will not say, or
    /// answers with something that is not a finite number.
    pub fn param_value(&self, id: ParamId) -> Option<f64> {
        let slot = find_param(&self.slots, id.0)?;
        let get = self.extensions.params?.get_value?;
        let mut value = f64::NAN;
        // SAFETY: the plugin is initialised, this is its main thread, and
        // `value` is a number this function owns.
        if !unsafe { get(self.plugin, id.0, &raw mut value) } {
            return None;
        }
        value.is_finite().then(|| slot.hold(value))
    }

    /// Sets a parameter, in the plugin's own scale. The value is held to
    /// the parameter's range.
    ///
    /// While an audio half exists, the value travels to it through a
    /// wait-free queue and reaches the plugin as an event on the first
    /// frame of the next block, ahead of that block's own events. Otherwise
    /// it is handed to the plugin before this returns.
    ///
    /// # Errors
    ///
    /// [`ClapError::UnknownParam`] for an id the plugin does not have,
    /// [`ClapError::InvalidArgument`] for a value that is not a number,
    /// [`ClapError::QueueFull`] if 1,024 changes are already waiting for
    /// the audio thread, and [`ClapError::Unsupported`] if the plugin
    /// cannot take a value outside processing.
    pub fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), ClapError> {
        let slot = find_param(&self.slots, id.0).ok_or(ClapError::UnknownParam(id))?;
        if value.is_nan() {
            return Err(ClapError::InvalidArgument(
                "a parameter value has to be a number",
            ));
        }
        let value = slot.hold(value);
        if self.shared.processor_alive() {
            if let Some(link) = &mut self.link {
                let message = ToAudio::ParamValue { id: id.0, value };
                return link
                    .to_audio
                    .push(message)
                    .map_err(|_| ClapError::QueueFull);
            }
        }
        // No audio half: anything it left unread goes first, then this.
        let mut values = self.retire_link();
        values.push((id, value));
        self.deliver(&values)
    }

    /// The text a person should see for `value` of a parameter, as the
    /// plugin words it, units included. At most 255 bytes.
    ///
    /// The plugin is asked about `value` held to the range it declared for
    /// the parameter, as with every value this host sends it.
    ///
    /// Returns `None` if the plugin has no such parameter or cannot say, or
    /// if `value` is not a number.
    pub fn param_to_text(&self, id: ParamId, value: f64) -> Option<String> {
        let slot = find_param(&self.slots, id.0)?;
        if value.is_nan() {
            return None;
        }
        let value = slot.hold(value);
        let to_text = self.extensions.params?.value_to_text?;
        let mut text = [0 as c_char; VALUE_TEXT_BYTES];
        // The last byte is kept back, so the text ends even if the plugin
        // fills everything it is offered.
        let room = (VALUE_TEXT_BYTES - 1) as u32;
        // SAFETY: the plugin is initialised, this is its main thread, and
        // `text` has room for the size passed.
        if !unsafe { to_text(self.plugin, id.0, value, text.as_mut_ptr(), room) } {
            return None;
        }
        Some(field_text(&text))
    }

    /// The value a piece of text stands for, as the plugin reads it: the
    /// reverse of [`param_to_text`](Self::param_to_text). The value is held
    /// to the range the plugin declared for the parameter, so text that
    /// names a value beyond the range gives the nearer end of it.
    ///
    /// Returns `None` if the plugin has no such parameter, cannot make
    /// sense of the text, or answers with something that is not a finite
    /// number.
    pub fn text_to_param(&self, id: ParamId, text: &str) -> Option<f64> {
        let slot = find_param(&self.slots, id.0)?;
        let to_value = self.extensions.params?.text_to_value?;
        let text = CString::new(text).ok()?;
        let mut value = f64::NAN;
        // SAFETY: the plugin is initialised, this is its main thread,
        // `text` is a C string and `value` a number this function owns.
        if !unsafe { to_value(self.plugin, id.0, text.as_ptr(), &raw mut value) } {
            return None;
        }
        value.is_finite().then(|| slot.hold(value))
    }

    /// Exchanges parameter changes with the plugin now, on this thread.
    ///
    /// Only possible while no audio half exists; with one, process a block
    /// or call [`ClapProcessor::flush`] there. What the plugin reports in
    /// return comes out of the next [`pump`](Self::pump).
    ///
    /// # Errors
    ///
    /// [`ClapError::ProcessorAlive`] while an audio half exists, and
    /// [`ClapError::Unsupported`] if the plugin has no parameters to flush.
    pub fn flush(&mut self) -> Result<(), ClapError> {
        if self.shared.processor_alive() {
            return Err(ClapError::ProcessorAlive);
        }
        let values = self.retire_link();
        self.deliver(&values)
    }

    /// Lets go of the queues to an audio half that no longer exists, and
    /// returns the parameter changes it never read.
    fn retire_link(&mut self) -> Vec<(ParamId, f64)> {
        let mut unread = Vec::new();
        if self.shared.processor_alive() {
            return unread;
        }
        let Some(mut link) = self.link.take() else {
            return unread;
        };
        while let Some(report) = link.from_audio.pop() {
            self.waiting.push(report.into());
        }
        if let Some(mut queue) = self.shared.take_leftover() {
            while let Some(ToAudio::ParamValue { id, value }) = queue.pop() {
                unread.push((ParamId(id), value));
            }
        }
        unread
    }

    /// Hands parameter values to the plugin on this thread, through its
    /// flush function, and keeps what it reports for the next `pump`.
    ///
    /// Callers make sure no audio half exists.
    fn deliver(&mut self, values: &[(ParamId, f64)]) -> Result<(), ClapError> {
        let Some(flush) = self.extensions.params.and_then(|ext| ext.flush) else {
            return Err(ClapError::Unsupported(
                "the plugin has no parameters to flush",
            ));
        };
        let mut input: Vec<InEvent> = values
            .iter()
            .filter_map(|&(param, value)| {
                let kind = EventKind::ParamValue { param, value };
                to_clap(&kind, 0, &self.slots, NoteRoute::default())
            })
            .collect();
        let mut reports = Reports {
            events: Vec::new(),
            params: &self.slots,
            unlisted: 0,
        };
        let in_events = input_events(&mut input);
        let out_events = clap_output_events {
            ctx: (&raw mut reports).cast(),
            try_push: Some(report_push),
        };
        // An active plugin's flush belongs to the audio thread. With no
        // audio half, nobody holds that role, and this thread takes it for
        // the length of the call.
        let role = self.shared.active().then(|| self.shared.take_audio_role());
        // SAFETY: the plugin is initialised. It is either inactive, where
        // CLAP has flush called on the main thread, or active with nothing
        // processing it, where this thread holds the audio role. Both
        // lists point at locals that outlive the call.
        unsafe { flush(self.plugin, &raw const in_events, &raw const out_events) };
        if let Some(previous) = role {
            self.shared.give_audio_role(previous);
        }
        let Reports {
            mut events,
            unlisted,
            ..
        } = reports;
        self.waiting.append(&mut events);
        // Remarked on by the next `pump`, together with the ones the audio
        // half refused.
        self.shared
            .unlisted_reports
            .fetch_add(unlisted, Ordering::Relaxed);
        Ok(())
    }

    // --- State -----------------------------------------------------------

    /// Asks the plugin for everything it would need to come back as it is
    /// now. The bytes mean something to the plugin alone.
    ///
    /// # Errors
    ///
    /// [`ClapError::Unsupported`] if the plugin keeps no state,
    /// [`ClapError::StateSaveFailed`] if it reports failure, and
    /// [`ClapError::StateTooLarge`] if it writes more than
    /// [`MAX_STATE_BYTES`].
    pub fn save_state(&mut self) -> Result<Vec<u8>, ClapError> {
        let save = self
            .extensions
            .state
            .and_then(|ext| ext.save)
            .ok_or(ClapError::Unsupported("the plugin keeps no state"))?;
        let mut writer = Writer {
            bytes: Vec::new(),
            per_call: self.stream_chunk,
            too_large: false,
        };
        let stream = clap_ostream {
            ctx: (&raw mut writer).cast(),
            write: Some(stream_write),
        };
        // SAFETY: the plugin is initialised and this is its main thread.
        // The stream and the writer behind it outlive the call.
        let saved = unsafe { save(self.plugin, &raw const stream) };
        if writer.too_large {
            Err(ClapError::StateTooLarge)
        } else if !saved {
            Err(ClapError::StateSaveFailed)
        } else {
            Ok(writer.bytes)
        }
    }

    /// Gives the plugin back a state it saved earlier.
    ///
    /// A plugin usually answers by asking for its parameter values to be
    /// read again, which comes out of the next [`pump`](Self::pump) as
    /// [`HostEvent::ParamsRescanned`].
    ///
    /// # Errors
    ///
    /// [`ClapError::Unsupported`] if the plugin keeps no state, and
    /// [`ClapError::StateLoadFailed`] if it refuses these bytes. What a
    /// plugin has changed by the time it refuses is up to the plugin.
    pub fn load_state(&mut self, state: &[u8]) -> Result<(), ClapError> {
        let load = self
            .extensions
            .state
            .and_then(|ext| ext.load)
            .ok_or(ClapError::Unsupported("the plugin keeps no state"))?;
        let mut reader = Reader {
            bytes: state,
            position: 0,
            per_call: self.stream_chunk,
        };
        let stream = clap_istream {
            ctx: (&raw mut reader).cast(),
            read: Some(stream_read),
        };
        // SAFETY: the plugin is initialised and this is its main thread.
        // The stream and the reader behind it outlive the call.
        if unsafe { load(self.plugin, &raw const stream) } {
            Ok(())
        } else {
            Err(ClapError::StateLoadFailed)
        }
    }

    /// Limits how many bytes the state streams move in one call.
    ///
    /// CLAP lets a host take or give fewer bytes than a plugin asks for,
    /// and a plugin has to cope by asking again. Hosts differ in how they
    /// use that freedom, so a plugin that only works when every call moves
    /// everything will fail somewhere. Setting a small limit here shows
    /// whether a plugin copes. Zero means no limit, which is the default.
    pub fn set_stream_chunk(&mut self, bytes: usize) {
        self.stream_chunk = if bytes == 0 { usize::MAX } else { bytes };
    }

    // --- Switching ports ---------------------------------------------------

    /// Switches audio port `index` of one direction on or off through the
    /// plugin's `audio-ports-activation`, as a host does with a port
    /// nothing is routed to or from. Every port starts on. A port that is
    /// off is still given memory, which the plugin is expected to leave
    /// or fill with silence; it may skip the work of filling it.
    ///
    /// # Errors
    ///
    /// - [`ClapError::AlreadyActive`] while the plugin is active: this host
    ///   switches ports only while it is not.
    /// - [`ClapError::Unsupported`] if the plugin has no
    ///   `audio-ports-activation`.
    /// - [`ClapError::InvalidArgument`] for a port the plugin lacks, or one
    ///   it refuses to switch.
    pub fn set_audio_port_active(
        &mut self,
        is_input: bool,
        index: usize,
        active: bool,
    ) -> Result<(), ClapError> {
        if self.shared.active() {
            return Err(ClapError::AlreadyActive);
        }
        let Some(set_active) = self
            .extensions
            .ports_activation
            .and_then(|ext| ext.set_active)
        else {
            return Err(ClapError::Unsupported("audio-ports-activation"));
        };
        let ports = if is_input {
            &self.audio_inputs
        } else {
            &self.audio_outputs
        };
        if index >= ports.len() {
            return Err(ClapError::InvalidArgument("no such audio port"));
        }
        // SAFETY: the plugin is initialised and inactive, and this is its
        // main thread, where the extension allows the call. The sample size
        // is the one this host processes in.
        if unsafe { set_active(self.plugin, is_input, index as u32, active, 32) } {
            Ok(())
        } else {
            Err(ClapError::InvalidArgument(
                "the plugin refused to switch the port",
            ))
        }
    }

    // --- Latency and tail ------------------------------------------------

    /// How many frames late the plugin's output is, as it reported when it
    /// was last activated. Zero before the first activation and for a
    /// plugin that does not say. At most [`MAX_LATENCY_FRAMES`].
    pub fn latency(&self) -> u32 {
        self.shared.latency()
    }

    /// How long the plugin goes on sounding after its input stops, or
    /// `None` if it does not say.
    pub fn tail(&self) -> Option<Tail> {
        let get = self.extensions.tail?.get?;
        // SAFETY: the plugin is initialised, and CLAP allows this function
        // on the main thread.
        Some(Tail::from_frames(unsafe { get(self.plugin) }))
    }

    /// Reads the plugin's latency, which CLAP allows while it is active.
    fn read_latency(&mut self) {
        let Some(get) = self.extensions.latency.and_then(|ext| ext.get) else {
            return;
        };
        // SAFETY: the plugin is active and this is its main thread.
        let reported = unsafe { get(self.plugin) };
        if reported > MAX_LATENCY_FRAMES {
            self.warn(format!(
                "the plugin reports a latency of {reported} frames; {MAX_LATENCY_FRAMES} is used"
            ));
        }
        let frames = reported.min(MAX_LATENCY_FRAMES);
        if frames != self.shared.latency() {
            self.shared.set_latency(frames);
            self.waiting.push(HostEvent::LatencyChanged(frames));
        }
    }

    // --- Activation ------------------------------------------------------

    /// Whether the plugin is active.
    pub fn is_active(&self) -> bool {
        self.shared.active()
    }

    /// Whether the audio half made by the last activation still exists.
    pub fn has_processor(&self) -> bool {
        self.shared.processor_alive()
    }

    /// Gets the plugin ready to process at `sample_rate`, in blocks of up
    /// to `max_block_size` frames, and returns its audio half.
    ///
    /// Everything processing will need is allocated here: a buffer for
    /// every port, the pointer tables CLAP wants them described by, room
    /// for the events of a block in both directions, and the two queues
    /// between the halves.
    ///
    /// # Errors
    ///
    /// - [`ClapError::AlreadyActive`] if the plugin is active. Deactivate
    ///   it first.
    /// - [`ClapError::InvalidArgument`] for a sample rate that is not a
    ///   positive number, or a block size of zero or above
    ///   [`MAX_BLOCK_SIZE`].
    /// - [`ClapError::Unsupported`] if the plugin's ports are beyond what
    ///   this host serves: more than [`MAX_PORTS`] in a direction, more
    ///   than [`MAX_CHANNELS_PER_PORT`] channels in one, or more than
    ///   [`MAX_SCRATCH_SAMPLES`] of memory in all.
    /// - [`ClapError::ActivationFailed`] if the plugin refuses.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<ClapProcessor, ClapError> {
        if self.shared.active() {
            return Err(ClapError::AlreadyActive);
        }
        if !(sample_rate.is_finite() && sample_rate > 0.0) {
            return Err(ClapError::InvalidArgument(
                "the sample rate has to be a positive number",
            ));
        }
        if max_block_size == 0 || max_block_size > MAX_BLOCK_SIZE {
            return Err(ClapError::InvalidArgument(
                "the block size has to be between 1 and MAX_BLOCK_SIZE",
            ));
        }
        // Lists the plugin asked to have read again while it was active
        // are read now, before anything is sized by them.
        self.rescan_lists();
        if let Some(problem) = self.layout_problem {
            return Err(ClapError::Unsupported(problem));
        }
        let channels: usize = self
            .audio_inputs
            .iter()
            .chain(&self.audio_outputs)
            .map(|port| port.channels as usize)
            .sum();
        if channels.saturating_mul(max_block_size) > MAX_SCRATCH_SAMPLES {
            return Err(ClapError::Unsupported(
                "more channels at this block size than this host allocates for",
            ));
        }

        // SAFETY: the plugin is initialised and inactive, and this is its
        // main thread. The block size was checked to fit in 32 bits.
        if !unsafe { (self.activate)(self.plugin, sample_rate, 1, max_block_size as u32) } {
            return Err(ClapError::ActivationFailed);
        }
        self.shared.set_active(true);
        self.read_latency();

        let (to_audio, from_main) = spsc::channel(QUEUE_CAPACITY);
        let (to_main, from_audio) = spsc::channel(QUEUE_CAPACITY);
        self.link = Some(Link {
            to_audio,
            from_audio,
        });
        let first_note_input = self.note_inputs.first();
        Ok(ClapProcessor::new(Setup {
            shared: Arc::clone(&self.shared),
            calls: Calls {
                plugin: self.plugin,
                table: self.table,
                process: self.process,
                flush: self.extensions.params.and_then(|ext| ext.flush),
                tail: self.extensions.tail.and_then(|ext| ext.get),
            },
            sample_rate,
            max_frames: max_block_size,
            inputs: &self.audio_inputs,
            outputs: &self.audio_outputs,
            params: self.slots.clone(),
            route: NoteRoute {
                clap: first_note_input.is_some_and(|port| port.clap),
                // A port that reads MIDI as MPE is sent the same events as
                // one that reads it plain; see `NotePortInfo::midi_mpe`.
                midi: first_note_input.is_some_and(|port| port.midi || port.midi_mpe),
            },
            from_main,
            to_main,
        }))
    }

    /// Deactivates the plugin, once its audio half has been dropped.
    ///
    /// Parameter changes the audio half had not yet read are handed to the
    /// plugin here, so none is lost. Does nothing if the plugin is not
    /// active.
    ///
    /// # Errors
    ///
    /// [`ClapError::ProcessorAlive`] if the [`ClapProcessor`] still exists:
    /// the plugin may be processing, and CLAP forbids deactivating it then.
    pub fn deactivate(&mut self) -> Result<(), ClapError> {
        if !self.shared.active() {
            return Ok(());
        }
        if self.shared.processor_alive() {
            return Err(ClapError::ProcessorAlive);
        }
        let unread = self.retire_link();
        // SAFETY: the plugin is active, this is its main thread, and the
        // audio half told it to stop processing when it was dropped.
        unsafe { (self.deactivate)(self.plugin) };
        self.shared.set_active(false);
        if !unread.is_empty() {
            // A plugin without a flush function had no way to take them.
            let _ = self.deliver(&unread);
        }
        self.rescan_lists();
        Ok(())
    }

    // --- The plugin's requests -------------------------------------------

    /// How many blocks have come out silent because the plugin failed,
    /// since it was created.
    pub fn failed_blocks(&self) -> u64 {
        self.shared.failed_blocks.load(Ordering::Relaxed)
    }

    /// How many events have been dropped for want of room, since the
    /// plugin was created:
    ///
    /// - events for the plugin beyond [`EVENTS_PER_BLOCK`] in one call of
    ///   its process function;
    /// - events from the plugin beyond [`EVENTS_PER_BLOCK`] in one block of
    ///   the caller's;
    /// - events waiting for the next block beyond the 1,024 that can wait:
    ///   the ones placed past the end of a block, and the ones of blocks
    ///   the plugin could not be called for;
    /// - reports for a main thread that had stopped calling
    ///   [`pump`](Self::pump).
    ///
    /// Anything but zero means something was not heard or not seen. Events
    /// that [`ClapProcessor::reset`](manguebyte_dsp::Processor::reset)
    /// forgets on purpose are not counted.
    pub fn lost_events(&self) -> u64 {
        self.shared.lost_events.load(Ordering::Relaxed)
    }

    /// How many events have not been sent because the plugin has no way to
    /// receive them, since it was created: notes for a plugin with no note
    /// input, values for parameters it does not have, numbers that are not
    /// numbers.
    ///
    /// This is not a fault by itself. In a graph every node is shown every
    /// event of the block, so an effect is shown the notes meant for the
    /// instrument beside it, and they are counted here.
    pub fn undeliverable_events(&self) -> u64 {
        self.shared.undeliverable_events.load(Ordering::Relaxed)
    }

    /// Answers what the plugin has asked for since the last call, and
    /// returns what there is to know.
    ///
    /// A plugin talks to its host from any thread and at any moment. This
    /// is the one place where the host answers, on the main thread and
    /// outside every other call into the plugin:
    ///
    /// - a requested main-thread callback is made. A plugin that asks for
    ///   another one from inside the callback gets it from the next call;
    /// - a requested parameter flush is done, if no audio half exists;
    /// - lists the plugin says have changed are read again, where CLAP
    ///   allows it at this point. A change that needs the plugin inactive
    ///   waits for [`deactivate`](Self::deactivate);
    /// - parameter changes, gestures and failures from the audio thread,
    ///   log messages, and requests only the application can act on
    ///   (a restart, more processing, a new size for the editor's window)
    ///   are returned;
    /// - first of all, the plugin's timers that are due fire. While the
    ///   editor is open, call `pump` again by
    ///   [`next_editor_timer`](Self::next_editor_timer) at the latest;
    /// - an editor that closed by itself is destroyed, and
    ///   [`EditorRequest::Closed`] returned.
    ///
    /// Call it regularly: a few times a second is enough for most plugins,
    /// and a user interface wants it on every frame.
    pub fn pump(&mut self) -> Vec<HostEvent> {
        self.fire_timers();
        let mut requests = self.shared.take_requests();
        if requests & request::CALLBACK != 0 {
            if let Some(callback) = self.table.on_main_thread {
                // SAFETY: the plugin is initialised, this is its main
                // thread, and it asked for this call.
                unsafe { callback(self.plugin) };
            }
            // Whatever the callback asked for in turn is answered now too,
            // with one exception: another callback. That request is left
            // where it is and answered by the next call, so that a plugin
            // which spreads its work over several callbacks gets every one
            // of them, and one that asks without end cannot keep this
            // function from returning.
            requests |= self.shared.take_requests_except(request::CALLBACK);
        }
        self.answer_editor();

        if let Some(link) = &mut self.link {
            while let Some(report) = link.from_audio.pop() {
                self.waiting.push(report.into());
            }
        }
        if requests & request::RESTART != 0 {
            self.waiting.push(HostEvent::RestartRequested);
        }
        if requests & request::PROCESS != 0 {
            self.waiting.push(HostEvent::ProcessRequested);
        }
        if requests & request::FLUSH != 0 {
            if self.shared.processor_alive() {
                self.waiting.push(HostEvent::FlushRequested);
            } else {
                let values = self.retire_link();
                // A plugin that asks for a flush has a flush function.
                let _ = self.deliver(&values);
            }
        }
        if requests & request::STATE_DIRTY != 0 {
            self.waiting.push(HostEvent::StateDirty);
        }
        if requests & request::LATENCY != 0 && self.shared.active() {
            self.read_latency();
        }
        if requests & request::TAIL != 0 {
            self.waiting.push(HostEvent::TailChanged);
        }
        if requests & request::PARAM_CLEAR != 0 {
            let clears = std::mem::take(
                &mut *self
                    .shared
                    .clears
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()),
            );
            for (id, flags) in clears {
                self.waiting.push(HostEvent::ParamClearRequested {
                    param: ParamId(id),
                    all: flags & CLAP_PARAM_CLEAR_ALL != 0,
                    automation: flags & CLAP_PARAM_CLEAR_AUTOMATIONS != 0,
                    modulation: flags & CLAP_PARAM_CLEAR_MODULATIONS != 0,
                });
            }
        }
        self.rescan_lists();

        for failure in ProcessFailure::from_bits(self.shared.take_failures()) {
            self.waiting.push(HostEvent::ProcessFailed(failure));
        }
        let wrong = self.shared.wrong_thread_calls.swap(0, Ordering::Relaxed);
        if wrong > 0 {
            self.warn(format!(
                "the plugin called a main-thread function of the host from another thread \
                 {wrong} time(s); the calls were ignored"
            ));
        }
        let unlisted = self.shared.unlisted_reports.swap(0, Ordering::Relaxed);
        if unlisted > 0 {
            self.warn(format!(
                "the plugin reported a value or a gesture for a parameter that is not in its \
                 list {unlisted} time(s); the reports were dropped"
            ));
        }
        let (messages, lost) = self.shared.log.drain();
        for (severity, message) in messages {
            self.waiting.push(HostEvent::Log {
                severity: LogSeverity::from_clap(severity),
                message,
            });
        }
        if lost > 0 {
            self.warn(format!(
                "{lost} log message(s) from the plugin were dropped: it logs faster than it is read"
            ));
        }
        std::mem::take(&mut self.waiting)
    }

    /// Reads again the lists the plugin says have changed, as far as CLAP
    /// allows in the plugin's current state.
    fn rescan_lists(&mut self) {
        let active = self.shared.active();

        let mut flags = self.shared.params_rescan.swap(0, Ordering::Relaxed);
        if active && flags & CLAP_PARAM_RESCAN_ALL != 0 {
            // CLAP allows replacing the whole list only while the plugin
            // is inactive, so that part waits for deactivation.
            self.shared
                .params_rescan
                .fetch_or(CLAP_PARAM_RESCAN_ALL, Ordering::Relaxed);
            flags &= !CLAP_PARAM_RESCAN_ALL;
        }
        if flags != 0 {
            let all = flags & CLAP_PARAM_RESCAN_ALL != 0;
            let info = all || flags & CLAP_PARAM_RESCAN_INFO != 0;
            if info {
                self.read_params();
            }
            self.waiting.push(HostEvent::ParamsRescanned(ParamsRescan {
                values: all || flags & CLAP_PARAM_RESCAN_VALUES != 0,
                text: all || flags & CLAP_PARAM_RESCAN_TEXT != 0,
                info,
                all,
            }));
        }

        // Of the ports, only names may change while the plugin is active.
        let flags = self.shared.audio_ports_rescan.swap(0, Ordering::Relaxed);
        if active && flags & !CLAP_AUDIO_PORTS_RESCAN_NAMES != 0 {
            self.shared
                .audio_ports_rescan
                .fetch_or(flags, Ordering::Relaxed);
        } else if flags != 0 {
            self.read_audio_ports();
            self.waiting.push(HostEvent::AudioPortsChanged);
        }

        let flags = self.shared.note_ports_rescan.swap(0, Ordering::Relaxed);
        if active && flags & !CLAP_NOTE_PORTS_RESCAN_NAMES != 0 {
            self.shared
                .note_ports_rescan
                .fetch_or(flags, Ordering::Relaxed);
        } else if flags != 0 {
            self.read_note_ports();
            self.waiting.push(HostEvent::NotePortsChanged);
        }
    }
}

impl Drop for ClapPlugin {
    fn drop(&mut self) {
        // The editor goes first, while this is still the main thread and
        // the plugin certainly exists: an audio half that outlives this
        // value may destroy the plugin later, on its own thread, and an
        // editor cannot be destroyed there.
        self.close_editor();
        // Only `pump` fires timers and only this value tells the plugin of
        // descriptors, so whatever the plugin still has registered is
        // forgotten with it.
        self.shared.editor_loop().clear();
        // Destroys the plugin now if its audio half is gone, and leaves
        // that to the audio half if it is not.
        self.shared.main_handle_dropped();
    }
}

impl std::fmt::Debug for ClapPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapPlugin")
            .field("id", &self.descriptor.id)
            .field("active", &self.is_active())
            .field("params", &self.params.len())
            .finish_non_exhaustive()
    }
}

// --- Callbacks handed to the plugin on the main thread -------------------

/// Where what a plugin reports during a flush on the main thread goes.
struct Reports<'a> {
    events: Vec<HostEvent>,
    /// The plugin's parameters, sorted by id, which every report is held
    /// against.
    params: &'a [ParamSlot],
    /// Reports refused because they were about a parameter that is not in
    /// the list.
    unlisted: u64,
}

/// Collects what a plugin reports during a flush on the main thread.
unsafe extern "C" fn report_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    guard(false, || {
        if list.is_null() {
            return false;
        }
        // SAFETY: the plugin passes the list it was given, whose `ctx` is
        // the address of the `Reports` in `deliver`, which nothing else
        // touches during the call and which outlives it.
        let Some(reports) = (unsafe { (*list).ctx.cast::<Reports<'_>>().as_mut() }) else {
            return false;
        };
        // SAFETY: the plugin passes an event that is valid for this call
        // and as large as its header says, or null.
        let Some((_, kind)) = (unsafe { from_clap(event) }) else {
            // Nothing this host has a use for.
            return true;
        };
        let report = match held_to_declared(kind, reports.params) {
            Some(OutputKind::Event(EventKind::ParamValue { param, value })) => {
                HostEvent::ParamValue { param, value }
            }
            Some(OutputKind::GestureBegin { param }) => HostEvent::GestureBegin { param },
            Some(OutputKind::GestureEnd { param }) => HostEvent::GestureEnd { param },
            // A flush is about parameters. Anything else has no meaning
            // here and is taken without being kept.
            Some(_) => return true,
            // About a parameter the plugin does not list: counted, and
            // taken without being kept.
            None => {
                reports.unlisted += 1;
                return true;
            }
        };
        if reports.events.len() >= EVENTS_PER_BLOCK {
            return false;
        }
        reports.events.push(report);
        true
    })
}

/// Where a plugin's state goes while it is being saved.
struct Writer {
    bytes: Vec<u8>,
    /// Most bytes taken per call.
    per_call: usize,
    /// Set when the plugin tried to write past [`MAX_STATE_BYTES`].
    too_large: bool,
}

unsafe extern "C" fn stream_write(
    stream: *const clap_ostream,
    data: *const c_void,
    size: u64,
) -> i64 {
    guard(-1, || {
        if stream.is_null() {
            return -1;
        }
        // SAFETY: the plugin passes the stream it was given, whose `ctx`
        // is the address of the `Writer` in `save_state`, which nothing
        // else touches during the call.
        let Some(writer) = (unsafe { (*stream).ctx.cast::<Writer>().as_mut() }) else {
            return -1;
        };
        if size == 0 {
            return 0;
        }
        if data.is_null() {
            return -1;
        }
        let room = MAX_STATE_BYTES.saturating_sub(writer.bytes.len());
        if room == 0 {
            writer.too_large = true;
            return -1;
        }
        // Fewer bytes than offered is allowed: the plugin offers the rest
        // again.
        let count = usize::try_from(size)
            .unwrap_or(usize::MAX)
            .min(writer.per_call)
            .min(room);
        // SAFETY: the plugin offers `size` readable bytes at `data`, and
        // `count` is no more than that.
        writer
            .bytes
            .extend_from_slice(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), count) });
        count as i64
    })
}

/// Where a plugin's state comes from while it is being loaded.
struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    /// Most bytes given per call.
    per_call: usize,
}

unsafe extern "C" fn stream_read(
    stream: *const clap_istream,
    buffer: *mut c_void,
    size: u64,
) -> i64 {
    guard(-1, || {
        if stream.is_null() {
            return -1;
        }
        // SAFETY: the plugin passes the stream it was given, whose `ctx`
        // is the address of the `Reader` in `load_state`, which nothing
        // else touches during the call.
        let Some(reader) = (unsafe { (*stream).ctx.cast::<Reader<'_>>().as_mut() }) else {
            return -1;
        };
        let rest = reader.bytes.get(reader.position..).unwrap_or_default();
        // Fewer bytes than asked for is allowed: the plugin asks again.
        // Zero means the end.
        let count = usize::try_from(size)
            .unwrap_or(usize::MAX)
            .min(reader.per_call)
            .min(rest.len());
        if count == 0 {
            return 0;
        }
        if buffer.is_null() {
            return -1;
        }
        // SAFETY: the plugin offers `size` writable bytes at `buffer`,
        // `count` is no more than that and no more than `rest` holds, and
        // the plugin's buffer cannot overlap bytes this host owns.
        unsafe { ptr::copy_nonoverlapping(rest.as_ptr(), buffer.cast::<u8>(), count) };
        reader.position += count;
        count as i64
    })
}
