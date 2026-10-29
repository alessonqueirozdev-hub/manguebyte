//! The glue between the CLAP C ABI and a [`Plugin`].
//!
//! # Threads
//!
//! CLAP says which thread may call each function, and this file leans on
//! those rules instead of on locks:
//!
//! - `process`, `reset`, `start_processing` and `stop_processing` are called
//!   on the audio thread.
//! - `activate` and `deactivate` are called on the main thread, and only
//!   while the plugin is not processing.
//! - `params.flush` is called on the audio thread while the plugin is
//!   active, in place of `process`, and on the main thread otherwise.
//! - Everything else is called on the main thread, the editor's extensions
//!   (`gui`, `timer-support`, `posix-fd-support`) and `on_main_thread`
//!   included.
//!
//! So the audio-side state ([`Audio`]) is only ever touched by one thread at
//! a time, and lives in an `UnsafeCell`. Everything the main thread needs
//! while audio runs goes through the parameters, which are atomics, or
//! through the few atomics on [`Instance`]. The editor's state
//! ([`Gui`](crate::gui::Gui)) is the main thread's alone.
//!
//! # Telling the host what changed
//!
//! Before each block, and in `flush`, the host is sent what it did not do
//! itself: for each parameter, the start of a gesture made in the editor,
//! the parameter's value if it is not the one the host knows, and the end of
//! the gesture, in that order. Gestures arrive as counts
//! ([`GestureCounters`]), so the editor never waits and nothing between the
//! editor and the audio side can fill up.
//!
//! The host's output list can: CLAP lets `try_push` refuse an event. What
//! the host has been sent is recorded only once it took it, so a refused
//! begin, value or end is sent again on the next block or flush, in the
//! same order, and the host never sees an end without its begin or a
//! gesture's value outside it. Until then, gestures on that parameter merge
//! into one, and only the latest value is sent.
//!
//! A value the host set, by automation, `flush` or loading state, is never
//! sent back to it: what a parameter holds after storing a host's value is
//! worked out with [`Param::stored_plain`] rather than read back, so an edit
//! the editor stores at that moment still counts as news; and after the
//! host was asked to read every value again, the values it was asked to
//! read count as known, gestures or not.

use std::any::TypeId;
use std::cell::UnsafeCell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
    clap_plugin_audio_ports_activation,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config,
};
use clap_sys::ext::gui::CLAP_EXT_GUI;
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_note_name, clap_plugin_note_name};
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;
use clap_sys::ext::voice_info::{
    CLAP_EXT_VOICE_INFO, CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES, clap_host_voice_info,
    clap_plugin_voice_info, clap_voice_info,
};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, CLAP_SECTIME_FACTOR};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::plugin_features::*;
use clap_sys::process::*;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::{CLAP_VERSION, clap_version_is_compatible};
use manguebyte_core::{AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::note_input::NoteInput;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte_midi::MidiMessage;
use manguebyte_midi::ump::Packet;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::editor::{GestureCounters, GestureReader};
use manguebyte_plugin::output::{OutputBuffer, OutputEvent};
use manguebyte_plugin::{
    AudioLayout, ParamWatcher, Plugin, PluginKind, Tail, VoiceInfo, load_state, offered_layouts,
    save_state,
};

use crate::gui::{self, Gui, GuiVtables, HostRef, SharedFlags};
use crate::output::NoteSender;

/// Most events delivered to the plugin in one block. Extra events are
/// dropped rather than allocated for.
const EVENTS_PER_BLOCK: usize = 4_096;

/// Most transport events a block is cut at. Later ones in the same block
/// are ignored rather than allocated for.
const TRANSPORT_CHANGES: usize = 64;

/// Largest state a host may hand back, so a corrupt stream cannot make the
/// plugin allocate without bound.
const MAX_STATE_BYTES: usize = 64 << 20;

/// CLAP reads any tail at or above `INT32_MAX` as infinite (`ext/tail.h`).
const INFINITE_TAIL: u32 = i32::MAX as u32;

/// Runs `work`, turning a panic into `fallback`.
///
/// A panic must never cross into the host: unwinding through C is not
/// defined, and aborting would take the user's session with it.
pub(crate) fn guard<R>(fallback: R, work: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or(fallback)
}

// --- Parameter values as CLAP sees them ---------------------------------
//
// A continuous parameter is exposed as its position in 0..=1, so a host's
// automation lane has the same feel as the plugin's own control. A stepped
// parameter is exposed as a whole number of steps, which is what CLAP's
// "stepped" flag means.

fn step_count(param: &dyn Param) -> Option<f64> {
    param.step_count().filter(|&steps| steps > 0).map(f64::from)
}

fn clap_tail(tail: manguebyte_plugin::Tail) -> u32 {
    match tail {
        Tail::None => 0,
        Tail::Samples(samples) => u32::try_from(samples)
            .ok()
            .filter(|&n| n < INFINITE_TAIL)
            // Values above INFINITE_TAIL are clamped to INFINITE_TAIL - 1
            // Which is i32::MAX - 1
            .unwrap_or(INFINITE_TAIL - 1),
        Tail::Infinite => INFINITE_TAIL,
    }
}

fn to_clap(param: &dyn Param, plain: f64) -> f64 {
    let position = param.plain_to_normalized(plain);
    match step_count(param) {
        Some(steps) => (position * steps).round(),
        None => position,
    }
}

fn from_clap(param: &dyn Param, value: f64) -> f64 {
    let position = match step_count(param) {
        Some(steps) => value / steps,
        None => value,
    };
    param.normalized_to_plain(position)
}

/// Copies `text` into a fixed C buffer, cutting it at a character boundary
/// if it does not fit, and always ending it with a NUL.
fn copy_text(text: &str, out: &mut [c_char]) {
    let Some(room) = out.len().checked_sub(1) else {
        return;
    };
    let mut len = text.len().min(room);
    while !text.is_char_boundary(len) {
        len -= 1;
    }
    for (slot, &byte) in out.iter_mut().zip(&text.as_bytes()[..len]) {
        *slot = byte as c_char;
    }
    out[len] = 0;
}

// --- Descriptor ---------------------------------------------------------

static DESCRIPTORS: Mutex<Vec<(TypeId, &'static clap_plugin_descriptor)>> = Mutex::new(Vec::new());

/// A C string that lives for the rest of the program.
fn leak_c_string(text: &str) -> *const c_char {
    let cleaned: String = text.chars().filter(|&c| c != '\0').collect();
    CString::new(cleaned)
        .expect("interior NULs were removed")
        .into_raw()
}

/// The CLAP descriptor of `P`, built once and kept for the rest of the
/// program, as CLAP requires descriptors to outlive every plugin instance.
fn descriptor<P: Plugin>() -> &'static clap_plugin_descriptor {
    let mut all = DESCRIPTORS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let key = TypeId::of::<P>();
    if let Some(&(_, found)) = all.iter().find(|(id, _)| *id == key) {
        return found;
    }

    let info = P::INFO;
    let mut features: Vec<*const c_char> = match info.kind {
        PluginKind::Instrument => vec![
            CLAP_PLUGIN_FEATURE_INSTRUMENT.as_ptr(),
            CLAP_PLUGIN_FEATURE_SYNTHESIZER.as_ptr(),
        ],
        PluginKind::Effect => vec![CLAP_PLUGIN_FEATURE_AUDIO_EFFECT.as_ptr()],
        PluginKind::NoteEffect => vec![CLAP_PLUGIN_FEATURE_NOTE_EFFECT.as_ptr()],
        PluginKind::Analyzer => vec![CLAP_PLUGIN_FEATURE_ANALYZER.as_ptr()],
    };
    features.push(ptr::null());

    let empty = leak_c_string("");
    let built: &'static clap_plugin_descriptor = Box::leak(Box::new(clap_plugin_descriptor {
        clap_version: CLAP_VERSION,
        id: leak_c_string(info.id),
        name: leak_c_string(info.name),
        vendor: leak_c_string(info.vendor),
        url: empty,
        manual_url: empty,
        support_url: empty,
        version: leak_c_string(info.version),
        description: leak_c_string(info.description),
        features: Box::leak(features.into_boxed_slice()).as_ptr(),
    }));
    all.push((key, built));
    built
}

// --- Entry point and factory --------------------------------------------

/// The CLAP entry point for a library that exports the single plugin `P`.
///
/// [`export_clap!`](crate::export_clap) puts the result in the `clap_entry`
/// symbol a host looks for. Tests can call it directly to drive a plugin
/// through the real C interface.
pub const fn plugin_entry<P: Plugin>() -> clap_plugin_entry {
    clap_plugin_entry {
        clap_version: CLAP_VERSION,
        init: Some(entry_init),
        deinit: Some(entry_deinit),
        get_factory: Some(entry_get_factory::<P>),
    }
}

unsafe extern "C" fn entry_init(_plugin_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory<P: Plugin>(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated string.
    let id = unsafe { CStr::from_ptr(factory_id) };
    if id == CLAP_PLUGIN_FACTORY_ID {
        ptr::from_ref(Vtables::<P>::FACTORY).cast()
    } else {
        ptr::null()
    }
}

/// The tables of function pointers handed to the host, one set per plugin
/// type. Associated constants of reference type live for the whole program.
struct Vtables<P>(PhantomData<P>);

impl<P: Plugin> Vtables<P> {
    const FACTORY: &'static clap_plugin_factory = &clap_plugin_factory {
        get_plugin_count: Some(factory_count),
        get_plugin_descriptor: Some(factory_descriptor::<P>),
        create_plugin: Some(factory_create::<P>),
    };
    const PARAMS: &'static clap_plugin_params = &clap_plugin_params {
        count: Some(params_count::<P>),
        get_info: Some(params_get_info::<P>),
        get_value: Some(params_get_value::<P>),
        value_to_text: Some(params_value_to_text::<P>),
        text_to_value: Some(params_text_to_value::<P>),
        flush: Some(params_flush::<P>),
    };
    const STATE: &'static clap_plugin_state = &clap_plugin_state {
        save: Some(state_save::<P>),
        load: Some(state_load::<P>),
    };
    const AUDIO_PORTS: &'static clap_plugin_audio_ports = &clap_plugin_audio_ports {
        count: Some(audio_ports_count::<P>),
        get: Some(audio_ports_get::<P>),
    };
    const NOTE_PORTS: &'static clap_plugin_note_ports = &clap_plugin_note_ports {
        count: Some(note_ports_count::<P>),
        get: Some(note_ports_get::<P>),
    };
    const LATENCY: &'static clap_plugin_latency = &clap_plugin_latency {
        get: Some(latency_get::<P>),
    };
    const TAIL: &'static clap_plugin_tail = &clap_plugin_tail {
        get: Some(tail_get::<P>),
    };
    const PORTS_CONFIG: &'static clap_plugin_audio_ports_config = &clap_plugin_audio_ports_config {
        count: Some(ports_config_count::<P>),
        get: Some(ports_config_get::<P>),
        select: Some(ports_config_select::<P>),
    };
    const PORTS_ACTIVATION: &'static clap_plugin_audio_ports_activation =
        &clap_plugin_audio_ports_activation {
            can_activate_while_processing: Some(ports_can_activate_while_processing),
            set_active: Some(ports_set_active::<P>),
        };
    const VOICE_INFO: &'static clap_plugin_voice_info = &clap_plugin_voice_info {
        get: Some(voice_info_get::<P>),
    };
    const NOTE_NAME: &'static clap_plugin_note_name = &clap_plugin_note_name {
        count: Some(note_name_count::<P>),
        get: Some(note_name_get::<P>),
    };
}

unsafe extern "C" fn factory_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_descriptor<P: Plugin>(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    guard(ptr::null(), || match index {
        0 => ptr::from_ref(descriptor::<P>()),
        _ => ptr::null(),
    })
}

unsafe extern "C" fn factory_create<P: Plugin>(
    _factory: *const clap_plugin_factory,
    host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    guard(ptr::null(), || {
        if host.is_null() || plugin_id.is_null() {
            return ptr::null();
        }
        // SAFETY: the host passes a valid `clap_host` and a NUL-terminated id.
        let (host_version, wanted) = unsafe { ((*host).clap_version, CStr::from_ptr(plugin_id)) };
        if !clap_version_is_compatible(host_version) || wanted.to_bytes() != P::INFO.id.as_bytes() {
            return ptr::null();
        }

        let plugin = P::new();
        let params = plugin.params();
        if params.validate().is_err() {
            return ptr::null();
        }
        // Asked once, right after the parameters, as the contract says.
        let editor = plugin.editor();
        let mut ids = Vec::with_capacity(params.len());
        params.visit(&mut |param| ids.push(param.param_id()));
        // Asked now so that `get_extension` knows whether to offer
        // `voice-info`, and again on each activation.
        let voices = plugin.voice_info();

        // Gestures are counted only for a plugin with an editor; without
        // one, nothing here differs from a plugin that never had one.
        let (gui, gestures) = match editor {
            Some(spec) => {
                let counters = Arc::new(GestureCounters::new(ids.len()));
                let gestures = Gestures::new(GestureReader::new(Arc::clone(&counters)));
                (Some(Gui::new(spec, counters)), Some(gestures))
            }
            None => (None, None),
        };

        let instance = Box::into_raw(Box::new(Instance::<P> {
            clap: clap_plugin {
                desc: descriptor::<P>(),
                plugin_data: ptr::null_mut(),
                init: Some(plugin_init::<P>),
                destroy: Some(plugin_destroy::<P>),
                activate: Some(plugin_activate::<P>),
                deactivate: Some(plugin_deactivate::<P>),
                start_processing: Some(plugin_start_processing::<P>),
                stop_processing: Some(plugin_stop_processing::<P>),
                reset: Some(plugin_reset::<P>),
                process: Some(plugin_process::<P>),
                get_extension: Some(plugin_get_extension::<P>),
                on_main_thread: Some(plugin_on_main_thread::<P>),
            },
            // SAFETY: checked non-null above; CLAP keeps the host valid
            // until the plugin is destroyed, and `plugin_destroy` closes the
            // editor, which drops every copy, before that.
            host: unsafe { HostRef::new(host) },
            latency: AtomicU32::new(0),
            tail: AtomicU32::new(INFINITE_TAIL),
            layouts: offered_layouts::<P>(),
            config: AtomicUsize::new(0),
            sidechain_on: AtomicBool::new(true),
            active: AtomicBool::new(false),
            has_voice_info: voices.is_some(),
            voices: Mutex::new(voices),
            flags: Arc::new(SharedFlags::new(&*params)),
            has_editor: gui.is_some(),
            gui,
            audio: UnsafeCell::new(Audio {
                watcher: ParamWatcher::new(&*params),
                known_to_host: current_values(&*params),
                current: vec![0.0; ids.len()],
                gestures,
                plugin,
                sample_rate: 0.0,
                layout: P::LAYOUT,
                buffer: AudioBuffer::new(P::LAYOUT.outputs, 0),
                sidechain: AudioBuffer::new(P::LAYOUT.sidechain, 0),
                events: EventBuffer::new(),
                piece: EventBuffer::new(),
                changes: Vec::with_capacity(TRANSPORT_CHANGES),
                input: NoteInput::new(P::MPE),
                notes: NoteSender::new(if P::NOTE_OUTPUT { EVENTS_PER_BLOCK } else { 0 }),
                piece_output: if P::NOTE_OUTPUT {
                    OutputBuffer::with_capacity(EVENTS_PER_BLOCK)
                } else {
                    OutputBuffer::new()
                },
                resend_values: false,
            }),
            ids,
            params,
        }));
        // SAFETY: `instance` was just produced by `Box::into_raw`, so it is
        // valid and nothing else refers to it yet.
        unsafe {
            (*instance).clap.plugin_data = instance.cast();
            &raw const (*instance).clap
        }
    })
}

// --- Instance -----------------------------------------------------------

/// One plugin as the host sees it.
pub(crate) struct Instance<P: Plugin> {
    /// What the host holds a pointer to. Its `plugin_data` points back here.
    clap: clap_plugin,
    /// The host that created this plugin. CLAP keeps it valid for as long
    /// as the plugin exists.
    host: HostRef,
    params: Arc<P::Params>,
    /// The id of each parameter, in the order the host indexes them.
    ids: Vec<ParamId>,
    /// Latency reported by the plugin when it was last activated.
    latency: AtomicU32,
    /// Tail reported by the plugin when it was last activated.
    tail: AtomicU32,
    /// The layouts offered as `audio-ports-config` configurations, the
    /// plugin's own first; a configuration's id is its index.
    layouts: Vec<AudioLayout>,
    /// The configuration the host selected, by index into `layouts`. Only
    /// changed while the plugin is not active.
    config: AtomicUsize,
    /// Whether the sidechain port is active (`audio-ports-activation`).
    /// Only changed while the plugin is not active.
    sidechain_on: AtomicBool,
    /// Whether the plugin is active, so that ports are not changed under
    /// it.
    active: AtomicBool,
    /// Whether the plugin said anything of its voices when it was created,
    /// which decides whether `voice-info` is offered. Never changes.
    has_voice_info: bool,
    /// What the plugin said of its voices when it was last activated.
    /// Taken on the main thread only.
    voices: Mutex<Option<VoiceInfo>>,
    /// What the main and audio threads share besides parameters.
    flags: Arc<SharedFlags>,
    /// Whether `gui` is there. Set once, when the plugin is created, so that
    /// `get_extension`, which any thread may call, need not look at `gui`.
    has_editor: bool,
    /// The editor, for a plugin that has one. Main thread only.
    pub(crate) gui: Option<Gui>,
    /// See the module documentation for who may touch this and when.
    audio: UnsafeCell<Audio<P>>,
}

/// The state used while processing.
struct Audio<P> {
    plugin: P,
    sample_rate: f64,
    /// The layout the plugin was prepared for.
    layout: AudioLayout,
    buffer: AudioBuffer<f32>,
    sidechain: AudioBuffer<f32>,
    events: EventBuffer,
    /// The events of one piece of a block cut at a transport event,
    /// counted from the piece's start.
    piece: EventBuffer,
    /// The transport events inside the block, with their frames, in time
    /// order.
    changes: Vec<(u32, Transport)>,
    /// What the plugin sends during one piece of a cut block, before it
    /// joins the block's output on the piece's frames.
    piece_output: OutputBuffer,
    /// Each parameter's value as of the event being read, in index order:
    /// what a modulation amount is converted around, so that automation
    /// earlier in the same block counts, wherever the host cut its blocks.
    current: Vec<f64>,
    /// Reads the host's MIDI 1.0, MPE and MIDI 2.0 events as per-note
    /// events, as every wrapper does. Notes in CLAP's own dialect, and
    /// its note expressions, need no reading and do not go through it.
    input: NoteInput,
    /// What the plugin sends out, and how it reaches the host.
    notes: NoteSender,
    /// Parameter values as the plugin last saw them.
    watcher: ParamWatcher,
    /// Parameter values as the host last knew them, in index order.
    known_to_host: Vec<f64>,
    /// The editor's gestures on their way to the host, for a plugin with an
    /// editor.
    gestures: Option<Gestures>,
    /// Whether the plugin panicked in the last block, so that the next one
    /// starts by announcing every parameter's value: the changes of the
    /// block it did not finish were stored and remembered as seen, but may
    /// never have reached it.
    resend_values: bool,
}

/// Turns the editor's gesture counts into events, and remembers what the
/// host has been sent of them.
struct Gestures {
    reader: GestureReader,
    /// Per parameter: the host has been sent a begin and not yet its end.
    open: Box<[bool]>,
    /// Per parameter: a gesture began that the host has not been sent a
    /// begin for, because its list refused it.
    owed: Box<[bool]>,
}

impl Gestures {
    /// Allocates.
    fn new(reader: GestureReader) -> Self {
        let len = reader.counters().len();
        Self {
            reader,
            open: vec![false; len].into_boxed_slice(),
            owed: vec![false; len].into_boxed_slice(),
        }
    }
}

fn current_values(params: &dyn ParamSet) -> Vec<f64> {
    let mut values = Vec::with_capacity(params.len());
    params.visit(&mut |param| values.push(param.plain()));
    values
}

/// The instance behind a pointer the host passed back.
///
/// # Safety
///
/// `plugin` must be null or a pointer returned by `factory_create::<P>` that
/// has not been destroyed.
pub(crate) unsafe fn instance<'a, P: Plugin>(
    plugin: *const clap_plugin,
) -> Option<&'a Instance<P>> {
    if plugin.is_null() {
        return None;
    }
    // SAFETY: per this function's contract `plugin` is live, and its
    // `plugin_data` was set by `factory_create` to the instance that owns it.
    unsafe { (*plugin).plugin_data.cast::<Instance<P>>().as_ref() }
}

impl<P: Plugin> Instance<P> {
    fn index_of(&self, id: clap_id) -> Option<usize> {
        self.ids.iter().position(|known| known.0 == id)
    }

    fn param(&self, id: clap_id) -> Option<&dyn Param> {
        self.index_of(id)?;
        self.params.find_param(ParamId(id))
    }

    /// The layout of the selected configuration, which the ports describe.
    fn selected(&self) -> AudioLayout {
        let index = self.config.load(Ordering::Relaxed);
        self.layouts.get(index).copied().unwrap_or(P::LAYOUT)
    }

    /// The layout the plugin runs with: the selected one, or, with its
    /// sidechain port switched off, the same without a sidechain if the
    /// plugin offers that. Otherwise the sidechain stays, silent.
    fn running_layout(&self) -> AudioLayout {
        let selected = self.selected();
        let without = selected.with_sidechain(0);
        if selected.sidechain > 0
            && !self.sidechain_on.load(Ordering::Relaxed)
            && self.layouts.contains(&without)
        {
            without
        } else {
            selected
        }
    }
}

unsafe extern "C" fn plugin_init<P: Plugin>(plugin: *const clap_plugin) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return false;
        };
        // The host's extensions may be looked up from here on, not before.
        if P::NOTE_OUTPUT {
            // SAFETY: the extension is asked for with its own type, on the
            // main thread, inside `init`.
            let ports = unsafe {
                instance
                    .host
                    .extension::<clap_host_note_ports>(CLAP_EXT_NOTE_PORTS)
            };
            if let Some(supported) = ports.and_then(|ports| ports.supported_dialects) {
                // SAFETY: the host's own function with its own pointer, on
                // the main thread; nothing is processing before `init`
                // returns, so the audio state is free.
                unsafe {
                    let dialects = supported(instance.host.as_ptr());
                    (*instance.audio.get()).notes.set_dialects(dialects);
                }
            }
        }
        if let Some(gui) = &instance.gui {
            let params: Arc<dyn ParamSet> = instance.params.clone();
            gui.init(
                instance.host,
                params,
                &instance.ids,
                Arc::clone(&instance.flags),
            );
        }
        true
    })
}

unsafe extern "C" fn plugin_destroy<P: Plugin>(plugin: *const clap_plugin) {
    guard((), || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return;
        };
        // The editor goes first, then its link, while the plugin and the
        // host they point at are still here. A host that never destroyed
        // its editor gets the same ending as one that did.
        if let Some(gui) = &instance.gui {
            if !gui.shutdown() {
                // Destroyed from inside one of the plugin's own calls, which
                // frees it when the outermost returns (see `Gui::shutdown`).
                return;
            }
        }
        // SAFETY: the host destroys each plugin once, and nothing refers to
        // it any more: no call into it is in progress.
        unsafe { free::<P>(plugin) };
    });
}

/// Frees a plugin instance.
///
/// # Safety
///
/// `plugin` must be a live plugin made by `factory_create::<P>`, and nothing
/// may use it, or any reference into it, afterwards.
pub(crate) unsafe fn free<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: per this function's contract; `plugin_data` is the pointer
    // `factory_create` got from `Box::into_raw`.
    drop(unsafe { Box::from_raw((*plugin).plugin_data.cast::<Instance<P>>()) });
}

unsafe extern "C" fn plugin_activate<P: Plugin>(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames: u32,
    max_frames: u32,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return false;
        };
        // SAFETY: `activate` runs on the main thread while the plugin is not
        // processing, so no other thread is using the audio state.
        let audio = unsafe { &mut *instance.audio.get() };
        let max_frames = max_frames.max(1) as usize;
        let layout = instance.running_layout();

        audio.sample_rate = sample_rate;
        audio.layout = layout;
        audio.buffer = AudioBuffer::new(layout.outputs, max_frames);
        audio.sidechain = AudioBuffer::new(layout.sidechain, max_frames);
        audio.events = EventBuffer::with_capacity(EVENTS_PER_BLOCK);
        audio.piece = EventBuffer::with_capacity(EVENTS_PER_BLOCK);
        audio.plugin.set_layout(layout);
        audio.plugin.prepare(&ProcessSpec {
            sample_rate,
            max_block_size: max_frames,
            num_channels: layout.outputs,
        });
        let latency = u32::try_from(audio.plugin.latency_samples()).unwrap_or(u32::MAX);
        instance.latency.store(latency, Ordering::Relaxed);
        let tail = clap_tail(audio.plugin.tail_samples());
        instance.tail.store(tail, Ordering::Relaxed);
        if instance.has_voice_info {
            let voices = audio.plugin.voice_info();
            let changed = {
                let mut known = instance
                    .voices
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                std::mem::replace(&mut *known, voices) != voices
            };
            if changed {
                // SAFETY: `activate` runs on the main thread, where the
                // host's `voice-info` may be called, and the host outlives
                // the plugin.
                unsafe { voices_changed(instance.host.as_ptr()) };
            }
        }
        instance.active.store(true, Ordering::Relaxed);
        true
    })
}

/// Tells the host that what the plugin says of its voices changed.
///
/// # Safety
///
/// `host` must be the live host that created the plugin, or null, and this
/// must be called on the main thread.
unsafe fn voices_changed(host: *const clap_host) {
    // SAFETY: guaranteed by this function's contract.
    let Some(get_extension) = (unsafe { host.as_ref() }).and_then(|host| host.get_extension) else {
        return;
    };
    // SAFETY: the host's own function with the host's own pointer. A
    // non-null result is the host's `clap_host_voice_info`, valid while the
    // host is.
    let voice_info = unsafe {
        get_extension(host, CLAP_EXT_VOICE_INFO.as_ptr())
            .cast::<clap_host_voice_info>()
            .as_ref()
    };
    if let Some(changed) = voice_info.and_then(|voice_info| voice_info.changed) {
        // SAFETY: as above, and we are on the main thread.
        unsafe { changed(host) };
    }
}

unsafe extern "C" fn plugin_deactivate<P: Plugin>(plugin: *const clap_plugin) {
    guard((), || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return;
        };
        // SAFETY: `deactivate` runs on the main thread after processing has
        // stopped, so no other thread is using the audio state.
        let audio = unsafe { &mut *instance.audio.get() };
        audio.plugin.reset();
        audio.input.reset();
        audio.notes.reset();
        instance.active.store(false, Ordering::Relaxed);
    });
}

unsafe extern "C" fn plugin_start_processing<P: Plugin>(plugin: *const clap_plugin) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return false;
        };
        instance.flags.processing.store(true, Ordering::SeqCst);
        true
    })
}

unsafe extern "C" fn plugin_stop_processing<P: Plugin>(plugin: *const clap_plugin) {
    guard((), || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return;
        };
        let flags = &instance.flags;
        flags.processing.store(false, Ordering::SeqCst);
        // An edit made just now may have seen the plugin still processing
        // and asked for no flush. This is the audio thread, which may not
        // ask for one, so it asks for a call on the main thread, which
        // does (see `ClapEditorHost::edited` for the other half).
        if instance.has_editor && flags.unsent.load(Ordering::SeqCst) {
            instance.host.request_callback();
        }
    });
}

unsafe extern "C" fn plugin_on_main_thread<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: the host passes a plugin it created through our factory, on
    // the main thread. A plugin without an editor has nothing to do here.
    unsafe { gui::on_main_thread::<P>(plugin) };
}

unsafe extern "C" fn plugin_reset<P: Plugin>(plugin: *const clap_plugin) {
    guard((), || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return;
        };
        // SAFETY: `reset` runs on the audio thread, which is the only thread
        // using the audio state while the plugin is active.
        let audio = unsafe { &mut *instance.audio.get() };
        audio.plugin.reset();
        // The voices are gone, so the notes the reader followed are too.
        audio.input.reset();
        audio.notes.reset();
    });
}

unsafe extern "C" fn plugin_get_extension<P: Plugin>(
    plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY: the host passes a valid NUL-terminated string.
    let id = unsafe { CStr::from_ptr(id) };
    // The editor's extensions are offered only by a plugin that has one.
    // SAFETY: the host passes a plugin it created through our factory.
    // `has_editor` never changes, so reading it from any thread is sound.
    // SAFETY: as above; `layouts` and `has_voice_info` never change either.
    let (has_editor, configs, has_voice_info) = match unsafe { instance::<P>(plugin) } {
        Some(instance) => (
            instance.has_editor,
            instance.layouts.len(),
            instance.has_voice_info,
        ),
        None => (false, 1, false),
    };
    if id == CLAP_EXT_AUDIO_PORTS_CONFIG && configs > 1 {
        ptr::from_ref(Vtables::<P>::PORTS_CONFIG).cast()
    } else if (id == CLAP_EXT_AUDIO_PORTS_ACTIVATION
        || id == CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT)
        && P::LAYOUT.sidechain > 0
    {
        ptr::from_ref(Vtables::<P>::PORTS_ACTIVATION).cast()
    } else if id == CLAP_EXT_VOICE_INFO && has_voice_info {
        ptr::from_ref(Vtables::<P>::VOICE_INFO).cast()
    } else if id == CLAP_EXT_NOTE_NAME && takes_notes::<P>() && !P::NOTE_NAMES.is_empty() {
        ptr::from_ref(Vtables::<P>::NOTE_NAME).cast()
    } else if has_editor && id == CLAP_EXT_GUI {
        ptr::from_ref(GuiVtables::<P>::GUI).cast()
    } else if has_editor && id == CLAP_EXT_TIMER_SUPPORT {
        ptr::from_ref(GuiVtables::<P>::TIMER).cast()
    } else if has_editor && id == CLAP_EXT_POSIX_FD_SUPPORT {
        ptr::from_ref(GuiVtables::<P>::FD).cast()
    } else if id == CLAP_EXT_PARAMS {
        ptr::from_ref(Vtables::<P>::PARAMS).cast()
    } else if id == CLAP_EXT_STATE {
        ptr::from_ref(Vtables::<P>::STATE).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        ptr::from_ref(Vtables::<P>::AUDIO_PORTS).cast()
    } else if id == CLAP_EXT_NOTE_PORTS && (takes_notes::<P>() || P::NOTE_OUTPUT) {
        ptr::from_ref(Vtables::<P>::NOTE_PORTS).cast()
    } else if id == CLAP_EXT_LATENCY {
        ptr::from_ref(Vtables::<P>::LATENCY).cast()
    } else if id == CLAP_EXT_TAIL {
        ptr::from_ref(Vtables::<P>::TAIL).cast()
    } else {
        ptr::null()
    }
}

// --- Processing ---------------------------------------------------------

unsafe extern "C" fn plugin_process<P: Plugin>(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    guard(CLAP_PROCESS_ERROR, || {
        // Numbers too small to hear are zero for as long as the plugin
        // runs, and the host's own setting is back when this returns.
        let _quiet = NoDenormals::new();
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return CLAP_PROCESS_ERROR;
        };
        // SAFETY: the host passes a `clap_process` that is valid for the
        // duration of this call, or null.
        let Some(process) = (unsafe { process.as_ref() }) else {
            return CLAP_PROCESS_ERROR;
        };
        // SAFETY: `process` runs on the audio thread, which is the only
        // thread using the audio state while the plugin is active.
        let audio = unsafe { &mut *instance.audio.get() };

        let frames = process.frames_count as usize;
        if frames > audio.buffer.capacity() {
            // More than the host promised when activating.
            return CLAP_PROCESS_ERROR;
        }
        audio.buffer.set_num_frames(frames);
        audio.buffer.clear();
        let layout = audio.layout;

        // SAFETY: the pointers in `process` are valid for this call.
        unsafe {
            instance.gather_events(audio, process.in_events, process.out_events);
            let inputs = ports(process.audio_inputs, process.audio_inputs_count);
            if layout.inputs > 0 {
                if let Some(main) = inputs.first() {
                    read_port(main, &mut audio.buffer, layout.inputs);
                }
            }
            if layout.sidechain > 0 {
                audio.sidechain.set_num_frames(frames);
                audio.sidechain.clear();
                let index = usize::from(layout.inputs > 0);
                if let Some(port) = inputs.get(index) {
                    read_port(port, &mut audio.sidechain, layout.sidechain);
                }
            }
        }

        // SAFETY: the transport pointer is valid for this call, or null.
        let transport =
            unsafe { process.transport.as_ref() }.map_or(Transport::STOPPED, convert_transport);
        audio.notes.output.clear();
        if !audio.render(frames, transport) {
            // The plugin panicked. The host is told the block failed, and
            // the next block starts with every parameter's value, since the
            // changes of this one may not have reached the plugin.
            audio.resend_values = true;
            return CLAP_PROCESS_ERROR;
        }

        // SAFETY: the output pointers in `process` are valid for this call.
        // Everything was read from the inputs before this point, so writing
        // is sound even when the host processes in place.
        unsafe {
            let outputs = ports(process.audio_outputs, process.audio_outputs_count);
            for (index, port) in outputs.iter().enumerate() {
                write_port(port, (index == 0).then_some(&audio.buffer), frames);
            }
            // What the plugin sent goes after the parameter values that
            // went before the block, all of which are on frame 0.
            audio.notes.send(process.frames_count, process.out_events);
        }
        CLAP_PROCESS_CONTINUE
    })
}

impl<P: Plugin> Audio<P> {
    /// Runs the plugin over the block of `frames` frames gathered in
    /// `events`, `buffer` and `sidechain`, starting with `transport`, and
    /// collects what it sends in `notes.output`. Returns whether it
    /// finished, rather than panicking. Real-time safe.
    ///
    /// A block with transport events inside it (`changes`) is cut at each,
    /// so that the plugin sees the transport the host gave from the frame
    /// it gave it on, as the LV2 wrapper does with `time:Position`. Each
    /// piece is a block of its own to the plugin: its events, its output
    /// and its sidechain counted from the piece's start, its transport the
    /// one in force there. A transport event on frame 0 replaces the
    /// block's own.
    fn render(&mut self, frames: usize, mut transport: Transport) -> bool {
        let Self {
            plugin,
            layout,
            buffer,
            sidechain,
            events,
            piece,
            changes,
            notes,
            piece_output,
            ..
        } = self;
        let keyed = layout.sidechain > 0;

        // Nearly every block: no transport event inside it.
        if changes.iter().all(|&(at, _)| at == 0) {
            if let Some(&(_, change)) = changes.last() {
                transport = change;
            }
            let mut context = ProcessContext::new(events, &transport);
            if keyed {
                context = context.with_sidechain(sidechain);
            }
            let output = &mut notes.output;
            return catch_unwind(AssertUnwindSafe(|| {
                plugin.process_with_output(buffer, &context, output);
            }))
            .is_ok();
        }

        let mut start = 0;
        let mut next_change = 0;
        let mut delivered = 0;
        loop {
            // The transport in force from here on.
            while let Some(&(at, change)) = changes.get(next_change) {
                if at as usize > start {
                    break;
                }
                transport = change;
                next_change += 1;
            }
            if start >= frames {
                return true;
            }
            let end = changes
                .get(next_change)
                .map_or(frames, |&(at, _)| (at as usize).min(frames));

            // The events of this piece, counted from its start. The last
            // piece also takes any past the end of the block.
            piece.clear();
            while let Some(&event) = events.as_slice().get(delivered) {
                if end < frames && event.offset as usize >= end {
                    break;
                }
                let offset = event.offset.saturating_sub(start as u32);
                let _ = piece.push(Event { offset, ..event });
                delivered += 1;
            }

            piece_output.clear();
            let piece_events: &EventBuffer = piece;
            let output: &mut OutputBuffer = piece_output;
            let finished = buffer.windowed(start..end, |window| {
                let mut run = |context: &ProcessContext<'_, f32>| {
                    catch_unwind(AssertUnwindSafe(|| {
                        plugin.process_with_output(window, context, output);
                    }))
                    .is_ok()
                };
                if keyed {
                    sidechain.windowed(start..end, |side| {
                        run(&ProcessContext::new(piece_events, &transport).with_sidechain(side))
                    })
                } else {
                    run(&ProcessContext::new(piece_events, &transport))
                }
            });
            if !finished {
                return false;
            }
            // What the piece sent, on the block's frames; past the piece's
            // end is its last frame, as past a block's end is the block's.
            let last = (end - start - 1) as u32;
            for event in piece_output.iter() {
                let offset = start as u32 + event.offset.min(last);
                let _ = notes.output.push(OutputEvent { offset, ..*event });
            }
            start = end;
        }
    }
}

/// The host's audio ports as a slice.
///
/// # Safety
///
/// `first` must point to `count` valid ports, or be null.
unsafe fn ports<'a>(first: *const clap_audio_buffer, count: u32) -> &'a [clap_audio_buffer] {
    if first.is_null() || count == 0 {
        return &[];
    }
    // SAFETY: guaranteed by this function's contract.
    unsafe { std::slice::from_raw_parts(first, count as usize) }
}

/// Copies up to `channels` channels of a host port into `buffer`.
///
/// The host gives each port in 32 bits (`data32`) or, since every port
/// declares `CLAP_AUDIO_PORT_SUPPORTS_64BITS`, in 64 bits (`data64`). The
/// plugin processes in 32 bits, so 64-bit samples are rounded to the
/// nearest `f32`: the plugin hears exactly what it would have been given in
/// 32 bits.
///
/// # Safety
///
/// `port` must describe valid channel pointers of the sample size it sets,
/// each with at least `buffer.num_frames()` frames, or have both null.
unsafe fn read_port(port: &clap_audio_buffer, buffer: &mut AudioBuffer<f32>, channels: usize) {
    let frames = buffer.num_frames();
    let count = channels
        .min(port.channel_count as usize)
        .min(buffer.num_channels());
    for index in 0..count {
        let target = buffer.channel_mut(index);
        if !port.data32.is_null() {
            // SAFETY: `index` is below the port's channel count.
            let channel = unsafe { *port.data32.add(index) };
            if !channel.is_null() {
                // SAFETY: the host guarantees `frames` readable samples.
                target.copy_from_slice(unsafe { std::slice::from_raw_parts(channel, frames) });
            }
        } else if !port.data64.is_null() {
            // SAFETY: as above, for the 64-bit channels.
            let channel = unsafe { *port.data64.add(index) };
            if !channel.is_null() {
                // SAFETY: as above.
                let samples = unsafe { std::slice::from_raw_parts(channel, frames) };
                for (out, &sample) in target.iter_mut().zip(samples) {
                    *out = sample as f32;
                }
            }
        }
    }
}

/// Fills a host port from `source`, or with silence where `source` has no
/// channel for it, in whichever sample size the host gave it in. A 64-bit
/// port gets the plugin's 32-bit samples widened, which is exact.
///
/// # Safety
///
/// `port` must describe valid channel pointers of the sample size it sets,
/// each with at least `frames` writable frames, or have both null.
unsafe fn write_port(port: &clap_audio_buffer, source: Option<&AudioBuffer<f32>>, frames: usize) {
    for index in 0..port.channel_count as usize {
        let from = source
            .filter(|buffer| index < buffer.num_channels())
            .map(|buffer| buffer.channel(index));
        if !port.data32.is_null() {
            // SAFETY: `index` is below the port's channel count.
            let channel = unsafe { *port.data32.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: the host guarantees `frames` writable samples per
            // channel, and no reference to the host's input memory is alive
            // here.
            let samples = unsafe { std::slice::from_raw_parts_mut(channel, frames) };
            match from {
                Some(from) => samples.copy_from_slice(from),
                None => samples.fill(0.0),
            }
        } else if !port.data64.is_null() {
            // SAFETY: as above, for the 64-bit channels.
            let channel = unsafe { *port.data64.add(index) };
            if channel.is_null() {
                continue;
            }
            // SAFETY: as above.
            let samples = unsafe { std::slice::from_raw_parts_mut(channel, frames) };
            match from {
                Some(from) => {
                    for (out, &sample) in samples.iter_mut().zip(from) {
                        *out = f64::from(sample);
                    }
                }
                None => samples.fill(0.0),
            }
        }
    }
}

fn convert_transport(clap: &clap_event_transport) -> Transport {
    let has = |flag: u32| clap.flags & flag != 0;
    let beats = |fixed: i64| fixed as f64 / CLAP_BEATTIME_FACTOR as f64;
    let timeline = has(CLAP_TRANSPORT_HAS_BEATS_TIMELINE);
    Transport {
        playing: has(CLAP_TRANSPORT_IS_PLAYING),
        recording: has(CLAP_TRANSPORT_IS_RECORDING),
        tempo_bpm: has(CLAP_TRANSPORT_HAS_TEMPO).then_some(clap.tempo),
        time_signature: has(CLAP_TRANSPORT_HAS_TIME_SIGNATURE)
            .then_some((clap.tsig_num, clap.tsig_denom)),
        position_samples: None,
        position_beats: timeline.then(|| beats(clap.song_pos_beats)),
        bar_start_beats: timeline.then(|| beats(clap.bar_start)),
        loop_beats: (timeline && has(CLAP_TRANSPORT_IS_LOOP_ACTIVE))
            .then(|| (beats(clap.loop_start_beats), beats(clap.loop_end_beats))),
        position_seconds: has(CLAP_TRANSPORT_HAS_SECONDS_TIMELINE)
            .then(|| clap.song_pos_seconds as f64 / CLAP_SECTIME_FACTOR as f64),
        bar_number: timeline.then_some(i64::from(clap.bar_number)),
        tempo_increment: has(CLAP_TRANSPORT_HAS_TEMPO)
            .then_some(clap.tempo_inc)
            .filter(|increment| increment.is_finite()),
    }
}

fn note_ref(note_id: i32, channel: i16, key: i16) -> Option<NoteRef> {
    let id = u32::try_from(note_id).ok();
    let channel = u8::try_from(channel).unwrap_or(0) & 0x0F;
    match (u8::try_from(key), id) {
        (Ok(key), Some(id)) => Some(NoteRef::with_id(channel, key & 0x7F, id)),
        (Ok(key), None) => Some(NoteRef::new(channel, key & 0x7F)),
        // The key is a wildcard but the id is not: the id alone decides.
        (Err(_), Some(id)) => Some(NoteRef::with_id(channel, 0, id)),
        (Err(_), None) => None,
    }
}

fn expression(id: clap_note_expression) -> Option<Expression> {
    Some(match id {
        CLAP_NOTE_EXPRESSION_VOLUME => Expression::Volume,
        CLAP_NOTE_EXPRESSION_PAN => Expression::Pan,
        CLAP_NOTE_EXPRESSION_TUNING => Expression::Tuning,
        CLAP_NOTE_EXPRESSION_VIBRATO => Expression::Vibrato,
        CLAP_NOTE_EXPRESSION_BRIGHTNESS => Expression::Brightness,
        CLAP_NOTE_EXPRESSION_PRESSURE => Expression::Pressure,
        _ => return None,
    })
}

impl<P: Plugin> Instance<P> {
    /// Fills `audio.events` for the coming block and keeps host and plugin
    /// in agreement about parameter values.
    ///
    /// # Safety
    ///
    /// `input` and `output` must be valid event lists for the duration of
    /// the call, or null.
    unsafe fn gather_events(
        &self,
        audio: &mut Audio<P>,
        input: *const clap_input_events,
        output: *const clap_output_events,
    ) {
        let params: &dyn ParamSet = &*self.params;
        audio.events.clear();
        audio.changes.clear();

        // Changes the plugin has not seen yet go first, on frame 0. They
        // stay first: later events on frame 0 go after them. After a block
        // the plugin did not finish, that is every value.
        if audio.resend_values {
            let events = &mut audio.events;
            params.visit(&mut |param| {
                let kind = EventKind::ParamValue {
                    param: param.param_id(),
                    value: param.plain(),
                };
                let _ = events.push(Event { offset: 0, kind });
            });
            audio.watcher.remember(params);
            audio.resend_values = false;
        } else {
            audio.watcher.emit_changes(params, &mut audio.events);
        }
        let announced = audio.events.len();

        // Changes the host has not seen yet are reported to it.
        // SAFETY: `output` is valid for this call, per our contract.
        unsafe { self.report_to_host(audio, output) };

        // Then what the host sends, read in time order from each
        // parameter's value as it stands now.
        let mut values = audio.current.iter_mut();
        params.visit(&mut |param| {
            if let Some(value) = values.next() {
                *value = param.plain();
            }
        });
        // SAFETY: `input` is valid for this call, per our contract.
        unsafe { for_each_event(input, |header| self.convert(audio, header)) };

        // Only the host's values are stored. The announced ones were read
        // from the parameters, and the editor may have stored a newer value
        // since; writing the old one back would undo the edit, and then
        // report the old value to the host as a change of its own.
        let from_host = &audio.events.as_slice()[announced..];
        audio.watcher.apply_and_remember(params, from_host);
        // The host knows what the parameters made of its values, rounded as
        // they hold them, so the rounding does not go back to it as news.
        // Worked out, not read back: an edit stored since must stay news.
        for event in from_host {
            if let EventKind::ParamValue { param, value } = event.kind {
                if let (Some(index), Some(target)) =
                    (self.index_of(param.0), self.params.find_param(param))
                {
                    if let Some(stored) = target.stored_plain(value) {
                        audio.known_to_host[index] = stored;
                    }
                }
            }
        }
    }

    /// Sends the host what changed without it, parameter by parameter: the
    /// start of an editor's gesture, the value if the host does not know it,
    /// and the end of the gesture, in that order. Real-time safe.
    ///
    /// What the host's list refuses is sent again next time (see the module
    /// documentation): for a parameter, nothing after a refused event is
    /// sent, and nothing is recorded as sent until the host took it.
    ///
    /// A value the host set itself is never sent back: automation and
    /// `flush` values are recorded as known once they are stored, and after
    /// the host was asked to read every value again (state loaded, or many
    /// values changed by the editor) the values it was asked to read are
    /// taken as known, inside a gesture or not. While a state load is under
    /// way, no value is sent at all.
    ///
    /// # Safety
    ///
    /// `output` must be a valid output event list for the duration of the
    /// call, or null; and this must be called where `process` or `flush`
    /// may run.
    unsafe fn report_to_host(&self, audio: &mut Audio<P>, output: *const clap_output_events) {
        let flags = &*self.flags;
        // A swap and not a load: it pairs with the editor's store, so every
        // edit made before the flag was set is read below.
        flags.unsent.swap(false, Ordering::SeqCst);
        let Audio {
            known_to_host,
            gestures,
            ..
        } = audio;
        flags.take_reread(known_to_host);
        let mut index = 0;
        self.params.visit(&mut |param| {
            let at = index;
            index += 1;
            let Some(known) = known_to_host.get_mut(at) else {
                return;
            };
            // SAFETY: `output` is valid for this call, per our contract.
            unsafe { report_param(output, param, at, known, gestures.as_mut(), flags) };
        });
    }

    /// Turns one host event into ours and queues it.
    ///
    /// # Safety
    ///
    /// `header` must point to a valid CLAP event.
    unsafe fn convert(&self, audio: &mut Audio<P>, header: *const clap_event_header) {
        // SAFETY: guaranteed by this function's contract.
        let head = unsafe { &*header };
        if head.space_id != CLAP_CORE_EVENT_SPACE_ID {
            return;
        }
        /// Reads the event as its concrete type, if it is large enough.
        ///
        /// # Safety
        ///
        /// `header` must be the start of a valid event whose type is `T`.
        unsafe fn read<'a, T>(header: *const clap_event_header) -> Option<&'a T> {
            // SAFETY: the caller guarantees the event's type; its size field
            // is checked before the larger struct is read.
            unsafe { ((*header).size as usize >= size_of::<T>()).then(|| &*header.cast::<T>()) }
        }

        let kind = match head.type_ {
            CLAP_EVENT_NOTE_ON | CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE => {
                // SAFETY: these three types all carry a `clap_event_note`.
                let Some(event) = (unsafe { read::<clap_event_note>(header) }) else {
                    return;
                };
                let Some(note) = note_ref(event.note_id, event.channel, event.key) else {
                    return;
                };
                let velocity = event.velocity as f32;
                match head.type_ {
                    CLAP_EVENT_NOTE_ON => EventKind::NoteOn { note, velocity },
                    CLAP_EVENT_NOTE_OFF => EventKind::NoteOff { note, velocity },
                    _ => EventKind::NoteChoke { note },
                }
            }
            CLAP_EVENT_NOTE_EXPRESSION => {
                // SAFETY: this type carries a `clap_event_note_expression`.
                let Some(event) = (unsafe { read::<clap_event_note_expression>(header) }) else {
                    return;
                };
                let (Some(note), Some(expression)) = (
                    note_ref(event.note_id, event.channel, event.key),
                    expression(event.expression_id),
                ) else {
                    return;
                };
                // CLAP's pan runs from 0 (left) to 1 (right); the
                // framework's from -1 to 1. The other expressions share
                // their units.
                let value = if expression == Expression::Pan {
                    2.0 * event.value - 1.0
                } else {
                    event.value
                };
                EventKind::NoteExpression {
                    note,
                    expression,
                    value,
                }
            }
            CLAP_EVENT_PARAM_VALUE => {
                // SAFETY: this type carries a `clap_event_param_value`.
                let Some(event) = (unsafe { read::<clap_event_param_value>(header) }) else {
                    return;
                };
                let (Some(index), Some(param)) =
                    (self.index_of(event.param_id), self.param(event.param_id))
                else {
                    return;
                };
                let value = from_clap(param, event.value);
                if event.note_id >= 0 || event.key >= 0 {
                    // Per-note automation, for a parameter that offers it.
                    // The parameter itself keeps its value.
                    let note = note_ref(event.note_id, event.channel, event.key);
                    let (true, Some(note)) = (param.info().flags().per_note, note) else {
                        return;
                    };
                    EventKind::NoteParamValue {
                        param: ParamId(event.param_id),
                        value,
                        note,
                    }
                } else {
                    // What modulation later in the block is converted
                    // around.
                    if let (Some(current), Some(stored)) =
                        (audio.current.get_mut(index), param.stored_plain(value))
                    {
                        *current = stored;
                    }
                    // The host knows this value; `gather_events` records it
                    // as known once it is stored.
                    EventKind::ParamValue {
                        param: ParamId(event.param_id),
                        value,
                    }
                }
            }
            CLAP_EVENT_PARAM_MOD => {
                // SAFETY: this type carries a `clap_event_param_mod`.
                let Some(event) = (unsafe { read::<clap_event_param_mod>(header) }) else {
                    return;
                };
                let (Some(index), Some(param)) =
                    (self.index_of(event.param_id), self.param(event.param_id))
                else {
                    return;
                };
                // The host gives the offset in its own scale. Convert it to
                // the parameter's unit around the value the parameter has on
                // the event's frame: automation earlier in the block counts,
                // so the amount does not depend on where blocks are cut.
                let plain = audio.current.get(index).copied().unwrap_or(param.plain());
                let moved = from_clap(param, to_clap(param, plain) + event.amount);
                EventKind::ParamModulation {
                    param: ParamId(event.param_id),
                    amount: moved - plain,
                    note: note_ref(event.note_id, event.channel, event.key),
                }
            }
            CLAP_EVENT_TRANSPORT => {
                // SAFETY: this type carries a `clap_event_transport`.
                let Some(event) = (unsafe { read::<clap_event_transport>(header) }) else {
                    return;
                };
                // The block is cut here (see `Audio::render`). Past the
                // room set aside, a change is ignored.
                // A host must send events in time order; one that does not
                // has a change taken as no earlier than the one before.
                let time = audio
                    .changes
                    .last()
                    .map_or(head.time, |&(last, _)| head.time.max(last));
                if audio.changes.len() < audio.changes.capacity() {
                    audio.changes.push((time, convert_transport(event)));
                }
                return;
            }
            CLAP_EVENT_MIDI => {
                // SAFETY: this type carries a `clap_event_midi`.
                let Some(event) = (unsafe { read::<clap_event_midi>(header) }) else {
                    return;
                };
                let Some(message) = MidiMessage::from_bytes(&event.data) else {
                    return;
                };
                let Audio { events, input, .. } = audio;
                // A full buffer drops the event; allocating here is not
                // an option.
                input.midi1(head.time, message, |event| {
                    let _ = events.push(event);
                });
                return;
            }
            CLAP_EVENT_MIDI2 => {
                // SAFETY: this type carries a `clap_event_midi2`.
                let Some(event) = (unsafe { read::<clap_event_midi2>(header) }) else {
                    return;
                };
                // One Universal MIDI Packet, in as many of the four words
                // as its type takes.
                let Ok(packet) = Packet::from_words(&event.data) else {
                    return;
                };
                let Audio { events, input, .. } = audio;
                input.packet(head.time, &packet, |event| {
                    let _ = events.push(event);
                });
                return;
            }
            _ => return,
        };
        // A full buffer drops the event; allocating here is not an option.
        let _ = audio.events.push(Event {
            offset: head.time,
            kind,
        });
    }
}

/// Sends the host what it does not know about one parameter, the one at
/// index `at`, whose value the host last knew as `known`. See
/// [`Instance::report_to_host`].
///
/// # Safety
///
/// `output` must be a valid output event list for the duration of the call,
/// or null.
unsafe fn report_param(
    output: *const clap_output_events,
    param: &dyn Param,
    at: usize,
    known: &mut f64,
    mut gestures: Option<&mut Gestures>,
    flags: &SharedFlags,
) {
    let (plain, open_now) = match gestures.as_deref_mut() {
        // The value is read with the counts, so that a value the editor set
        // inside a gesture is never sent outside it.
        Some(gestures) => {
            let (edges, plain) = gestures.reader.read_with(at, || param.plain());
            if edges.begin && !gestures.open[at] {
                // A gesture the host has not been told of; if the host still
                // has one open, because its end was refused, the two merge.
                gestures.owed[at] = true;
            }
            (plain, gestures.reader.is_open(at))
        }
        None => (param.plain(), false),
    };
    let settled = flags.settled();
    let id = param.param_id().0;

    if let Some(gestures) = gestures.as_deref_mut() {
        if !gestures.open[at] && (gestures.owed[at] || open_now) {
            // SAFETY: `output` is valid for this call, per our contract.
            if !unsafe { send_gesture(output, id, CLAP_EVENT_PARAM_GESTURE_BEGIN) } {
                // Refused: nothing more for this parameter until it is sent.
                gestures.owed[at] = true;
                return;
            }
            gestures.open[at] = true;
            gestures.owed[at] = false;
        }
    }
    if plain != *known {
        // Not while state is being restored: the value may be the host's.
        // And not if refused. Either way it waits for next time, and so
        // does the end, so that the value still lands inside the gesture.
        // SAFETY: `output` is valid for this call, per our contract.
        if !settled || !unsafe { send_param_value(output, id, to_clap(param, plain)) } {
            return;
        }
        *known = plain;
    }
    if let Some(gestures) = gestures.filter(|gestures| gestures.open[at] && !open_now) {
        // SAFETY: `output` is valid for this call, per our contract.
        let sent = unsafe { send_gesture(output, id, CLAP_EVENT_PARAM_GESTURE_END) };
        if sent {
            gestures.open[at] = false;
        }
    }
}

/// Calls `handle` for every event in a host's input list.
///
/// # Safety
///
/// `list` must be a valid event list for the duration of the call, or null.
unsafe fn for_each_event(
    list: *const clap_input_events,
    mut handle: impl FnMut(*const clap_event_header),
) {
    // SAFETY: guaranteed by this function's contract.
    let Some(events) = (unsafe { list.as_ref() }) else {
        return;
    };
    let (Some(size), Some(get)) = (events.size, events.get) else {
        return;
    };
    // SAFETY: the host's own functions, called with the host's own list.
    for index in 0..unsafe { size(list) } {
        // SAFETY: as above; `index` is below the size just reported.
        let header = unsafe { get(list, index) };
        if !header.is_null() {
            handle(header);
        }
    }
}

/// Hands `event`, a CLAP event struct whose first field is its header, to a
/// host's output list, which copies it. Returns whether the host took it:
/// a full list, or none at all, refuses it.
///
/// # Safety
///
/// `list` must be a valid output event list for the duration of the call,
/// or null, and `T` a CLAP event type whose header's size is that of `T`.
pub(crate) unsafe fn push_event<T>(list: *const clap_output_events, event: &T) -> bool {
    // SAFETY: guaranteed by this function's contract.
    let Some(push) = (unsafe { list.as_ref() }).and_then(|events| events.try_push) else {
        return false;
    };
    // The pointer is taken to the whole event and then viewed as its header,
    // which is its first field. A pointer taken to the header field alone
    // would only be allowed to reach the header, and the host reads the
    // rest of the event through it. Miri caught exactly that.
    let header = ptr::from_ref(event).cast::<clap_event_header>();
    // SAFETY: the host's own function with the host's own list; the event
    // is valid for the call and the host copies it. Nothing here waits or
    // allocates: what the host refuses, the caller sends again later.
    unsafe { push(list, header) }
}

/// Tells the host that a gesture on a parameter began or ended. Returns
/// whether the host took the event.
///
/// # Safety
///
/// As for [`push_event`].
unsafe fn send_gesture(
    list: *const clap_output_events,
    param_id: clap_id,
    type_: clap_event_type,
) -> bool {
    let event = clap_event_param_gesture {
        header: clap_event_header {
            size: size_of::<clap_event_param_gesture>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_,
            flags: 0,
        },
        param_id,
    };
    // SAFETY: guaranteed by this function's contract, and the header gives
    // the event's own size.
    unsafe { push_event(list, &event) }
}

/// Tells the host that a parameter changed without it. Returns whether the
/// host took the event.
///
/// # Safety
///
/// As for [`push_event`].
unsafe fn send_param_value(list: *const clap_output_events, param_id: clap_id, value: f64) -> bool {
    let event = clap_event_param_value {
        header: clap_event_header {
            size: size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    };
    // SAFETY: guaranteed by this function's contract, and the header gives
    // the event's own size.
    unsafe { push_event(list, &event) }
}

// --- Parameters extension -----------------------------------------------

unsafe extern "C" fn params_count<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    guard(0, || {
        // SAFETY: the host passes a plugin it created through our factory.
        unsafe { instance::<P>(plugin) }.map_or(0, |instance| instance.ids.len() as u32)
    })
}

unsafe extern "C" fn params_get_info<P: Plugin>(
    plugin: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory,
        // and a `clap_param_info` it owns, or null.
        let (Some(instance), Some(info)) =
            (unsafe { instance::<P>(plugin) }, unsafe { info.as_mut() })
        else {
            return false;
        };
        let Some(param) = instance
            .ids
            .get(index as usize)
            .and_then(|&id| instance.params.find_param(id))
        else {
            return false;
        };

        let flags = param.info().flags();
        let stepped = step_count(param);
        let mut clap_flags = 0;
        if flags.automatable {
            clap_flags |= CLAP_PARAM_IS_AUTOMATABLE;
        }
        if flags.hidden {
            clap_flags |= CLAP_PARAM_IS_HIDDEN;
        }
        if flags.bypass {
            clap_flags |= CLAP_PARAM_IS_BYPASS;
        }
        match stepped {
            Some(_) => clap_flags |= CLAP_PARAM_IS_STEPPED,
            None => clap_flags |= CLAP_PARAM_IS_MODULATABLE,
        }
        // Per note: by note id and by key. Not per channel or per port
        // alone, which address every note of a channel or port and have no
        // form in the framework's events.
        if flags.per_note {
            if flags.automatable {
                clap_flags |=
                    CLAP_PARAM_IS_AUTOMATABLE_PER_NOTE_ID | CLAP_PARAM_IS_AUTOMATABLE_PER_KEY;
            }
            if stepped.is_none() {
                clap_flags |=
                    CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID | CLAP_PARAM_IS_MODULATABLE_PER_KEY;
            }
        }

        info.id = param.param_id().0;
        info.flags = clap_flags;
        info.cookie = ptr::null_mut();
        copy_text(param.name(), &mut info.name);
        copy_text("", &mut info.module);
        info.min_value = 0.0;
        info.max_value = stepped.unwrap_or(1.0);
        info.default_value = to_clap(param, param.default_plain());
        true
    })
}

unsafe extern "C" fn params_get_value<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    out: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(param) = (unsafe { instance::<P>(plugin) }).and_then(|i| i.param(id)) else {
            return false;
        };
        if out.is_null() {
            return false;
        }
        // SAFETY: `out` is a non-null pointer to an `f64` the host owns.
        unsafe { *out = to_clap(param, param.plain()) };
        true
    })
}

unsafe extern "C" fn params_value_to_text<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    value: f64,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(param) = (unsafe { instance::<P>(plugin) }).and_then(|i| i.param(id)) else {
            return false;
        };
        if out.is_null() || capacity == 0 {
            return false;
        }
        let mut text = param.plain_to_text(from_clap(param, value));
        let unit = param.info().unit();
        if !unit.is_empty() {
            text.push(' ');
            text.push_str(unit);
        }
        // SAFETY: the host provides `capacity` writable characters at `out`.
        copy_text(&text, unsafe {
            std::slice::from_raw_parts_mut(out, capacity as usize)
        });
        true
    })
}

unsafe extern "C" fn params_text_to_value<P: Plugin>(
    plugin: *const clap_plugin,
    id: clap_id,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(param) = (unsafe { instance::<P>(plugin) }).and_then(|i| i.param(id)) else {
            return false;
        };
        if text.is_null() || out.is_null() {
            return false;
        }
        // SAFETY: the host passes a valid NUL-terminated string.
        let Ok(text) = unsafe { CStr::from_ptr(text) }.to_str() else {
            return false;
        };
        let Some(plain) = param.text_to_plain(text) else {
            return false;
        };
        // SAFETY: `out` is a non-null pointer to an `f64` the host owns.
        unsafe { *out = to_clap(param, plain) };
        true
    })
}

unsafe extern "C" fn params_flush<P: Plugin>(
    plugin: *const clap_plugin,
    input: *const clap_input_events,
    output: *const clap_output_events,
) {
    guard((), || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return;
        };
        // SAFETY: `flush` is called instead of `process`: on the audio
        // thread while active, on the main thread while not. Either way no
        // other thread is using the audio state.
        let audio = unsafe { &mut *instance.audio.get() };

        // First what the host does not know, as `process` does: the
        // editor's gestures and values, and values changed without the
        // host. A flush is how they reach a host that is not processing.
        // SAFETY: `output` is valid for this call, or null, and this runs
        // where `process` would.
        unsafe { instance.report_to_host(audio, output) };

        // Then the host's values. They are stored straight into
        // the parameters; the plugin's watcher is left alone, so the next
        // block still tells the plugin about them as events.
        // SAFETY: `input` is valid for this call, or null.
        unsafe {
            for_each_event(input, |header| {
                let head = &*header;
                if head.space_id != CLAP_CORE_EVENT_SPACE_ID
                    || head.type_ != CLAP_EVENT_PARAM_VALUE
                    || (head.size as usize) < size_of::<clap_event_param_value>()
                {
                    return;
                }
                let event = &*header.cast::<clap_event_param_value>();
                // A value for one note is not the parameter's value, and
                // with no block there is no note for it to reach.
                if event.note_id >= 0 || event.key >= 0 {
                    return;
                }
                let (Some(index), Some(param)) = (
                    instance.index_of(event.param_id),
                    instance.param(event.param_id),
                ) else {
                    return;
                };
                let value = from_clap(param, event.value);
                param.set_plain(value);
                // Worked out rather than read back, as in `gather_events`.
                if let Some(stored) = param.stored_plain(value) {
                    audio.known_to_host[index] = stored;
                }
            });
        }
    });
}

// --- State extension ----------------------------------------------------

unsafe extern "C" fn state_save<P: Plugin>(
    plugin: *const clap_plugin,
    stream: *const clap_ostream,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory,
        // and a stream that is valid for this call, or null.
        let (Some(instance), Some(write)) = (
            unsafe { instance::<P>(plugin) },
            unsafe { stream.as_ref() }.and_then(|stream| stream.write),
        ) else {
            return false;
        };
        let text = save_state::<P>(&instance.params);
        let mut rest = text.as_bytes();
        while !rest.is_empty() {
            // SAFETY: the host's own function with the host's own stream,
            // and a buffer of exactly the length passed.
            let written = unsafe { write(stream, rest.as_ptr().cast(), rest.len() as u64) };
            match usize::try_from(written) {
                Ok(count) if count > 0 && count <= rest.len() => rest = &rest[count..],
                _ => return false,
            }
        }
        true
    })
}

unsafe extern "C" fn state_load<P: Plugin>(
    plugin: *const clap_plugin,
    stream: *const clap_istream,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory,
        // and a stream that is valid for this call, or null.
        let (Some(instance), Some(read)) = (
            unsafe { instance::<P>(plugin) },
            unsafe { stream.as_ref() }.and_then(|stream| stream.read),
        ) else {
            return false;
        };
        let mut bytes = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            // SAFETY: the host's own function with the host's own stream,
            // and a buffer of exactly the length passed.
            let count = unsafe { read(stream, chunk.as_mut_ptr().cast(), chunk.len() as u64) };
            match usize::try_from(count) {
                Ok(0) => break,
                Ok(count) if count <= chunk.len() => bytes.extend_from_slice(&chunk[..count]),
                _ => return false,
            }
            if bytes.len() > MAX_STATE_BYTES {
                return false;
            }
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            return false;
        };
        let flags = &instance.flags;
        {
            // While the values change, the audio side reports none of them.
            let _loading = flags.loading();
            if load_state::<P>(&instance.params, text).is_err() {
                return false;
            }
            // Every value may have changed without an event, so the host is
            // asked to read them all again. It then knows them, and the
            // audio side must not report them back as if they were news.
            flags.host_rereads(&*instance.params);
        }
        // SAFETY: `load` runs on the main thread, where the host's parameter
        // extension may be called, and the host outlives the plugin.
        unsafe { rescan_values(instance.host.as_ptr()) };
        true
    })
}

/// Asks the host to read every parameter value again.
///
/// # Safety
///
/// `host` must be the live host that created the plugin, or null, and this
/// must be called on the main thread.
unsafe fn rescan_values(host: *const clap_host) {
    // SAFETY: guaranteed by this function's contract.
    let Some(get_extension) = (unsafe { host.as_ref() }).and_then(|host| host.get_extension) else {
        return;
    };
    // SAFETY: the host's own function with the host's own pointer. A
    // non-null result is the host's `clap_host_params`, valid while the
    // host is.
    let params = unsafe {
        get_extension(host, CLAP_EXT_PARAMS.as_ptr())
            .cast::<clap_host_params>()
            .as_ref()
    };
    if let Some(rescan) = params.and_then(|params| params.rescan) {
        // SAFETY: as above, and we are on the main thread.
        unsafe { rescan(host, CLAP_PARAM_RESCAN_VALUES) };
    }
}

// --- Ports and latency --------------------------------------------------

fn takes_notes<P: Plugin>() -> bool {
    matches!(
        P::INFO.kind,
        PluginKind::Instrument | PluginKind::NoteEffect
    )
}

/// The channel counts of the ports of `layout` in one direction, main port
/// first.
fn port_channels(layout: AudioLayout, is_input: bool) -> ([usize; 2], usize) {
    let candidates = if is_input {
        [layout.inputs, layout.sidechain]
    } else {
        [layout.outputs, 0]
    };
    let mut present = [0; 2];
    let mut count = 0;
    for channels in candidates {
        if channels > 0 {
            present[count] = channels;
            count += 1;
        }
    }
    (present, count)
}

/// The port type of a port of `channels` channels.
fn port_type(channels: usize) -> *const c_char {
    match channels {
        1 => CLAP_PORT_MONO.as_ptr(),
        2 => CLAP_PORT_STEREO.as_ptr(),
        _ => ptr::null(),
    }
}

/// The selected configuration's layout, or the plugin's own for a pointer
/// that is not a plugin.
///
/// # Safety
///
/// As for [`instance`].
unsafe fn selected<P: Plugin>(plugin: *const clap_plugin) -> AudioLayout {
    // SAFETY: per this function's contract.
    unsafe { instance::<P>(plugin) }.map_or(P::LAYOUT, Instance::selected)
}

unsafe extern "C" fn audio_ports_count<P: Plugin>(
    plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    guard(0, || {
        // SAFETY: the host passes a plugin it created through our factory.
        port_channels(unsafe { selected::<P>(plugin) }, is_input).1 as u32
    })
}

unsafe extern "C" fn audio_ports_get<P: Plugin>(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let layout = unsafe { selected::<P>(plugin) };
        let (channels, count) = port_channels(layout, is_input);
        let index = index as usize;
        // SAFETY: `info` is a struct the host owns, or null.
        let Some(info) = (unsafe { info.as_mut() }).filter(|_| index < count) else {
            return false;
        };
        let channels = channels[index];
        // The main port is the first one, except that an instrument's only
        // input port is its sidechain.
        let is_main = index == 0 && !(is_input && layout.inputs == 0);

        info.id = index as clap_id;
        copy_text(if is_main { "Main" } else { "Sidechain" }, &mut info.name);
        // Every port takes 64-bit samples as well, which the plugin hears
        // rounded to 32 bits (see `read_port`).
        info.flags = CLAP_AUDIO_PORT_SUPPORTS_64BITS;
        if is_main {
            info.flags |= CLAP_AUDIO_PORT_IS_MAIN;
        }
        info.channel_count = channels as u32;
        info.port_type = port_type(channels);
        // The main input and output can share memory when they are the
        // same shape.
        info.in_place_pair = if is_main && layout.inputs == layout.outputs {
            0
        } else {
            CLAP_INVALID_ID
        };
        true
    })
}

// --- Port configurations and activation ----------------------------------
//
// `audio-ports-config` offers each of the layouts `offered_layouts` lists,
// with its index as its id; `audio-ports-activation` lets the host switch
// the sidechain off. Both are main-thread calls made while the plugin is
// not active, and what they choose is read when it is next activated.

/// A name for a configuration: its main buses, then its sidechain.
fn config_name(layout: AudioLayout) -> String {
    let width = |channels: usize| match channels {
        1 => "Mono".to_owned(),
        2 => "Stereo".to_owned(),
        more => format!("{more} channels"),
    };
    let main = match (layout.inputs, layout.outputs) {
        (0, outputs) => width(outputs),
        (inputs, outputs) if inputs == outputs => width(inputs),
        (inputs, outputs) => format!("{} to {}", width(inputs), width(outputs).to_lowercase()),
    };
    match layout.sidechain {
        0 => main,
        channels => format!("{main}, {} sidechain", width(channels).to_lowercase()),
    }
}

unsafe extern "C" fn ports_config_count<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    guard(0, || {
        // SAFETY: the host passes a plugin it created through our factory.
        unsafe { instance::<P>(plugin) }.map_or(0, |instance| instance.layouts.len() as u32)
    })
}

unsafe extern "C" fn ports_config_get<P: Plugin>(
    plugin: *const clap_plugin,
    index: u32,
    config: *mut clap_audio_ports_config,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory,
        // and a struct it owns, or null.
        let (Some(instance), Some(config)) =
            (unsafe { instance::<P>(plugin) }, unsafe { config.as_mut() })
        else {
            return false;
        };
        let Some(&layout) = instance.layouts.get(index as usize) else {
            return false;
        };
        config.id = index;
        copy_text(&config_name(layout), &mut config.name);
        config.input_port_count = port_channels(layout, true).1 as u32;
        config.output_port_count = port_channels(layout, false).1 as u32;
        config.has_main_input = layout.inputs > 0;
        config.main_input_channel_count = layout.inputs as u32;
        config.main_input_port_type = port_type(layout.inputs);
        config.has_main_output = layout.outputs > 0;
        config.main_output_channel_count = layout.outputs as u32;
        config.main_output_port_type = port_type(layout.outputs);
        true
    })
}

unsafe extern "C" fn ports_config_select<P: Plugin>(
    plugin: *const clap_plugin,
    config_id: clap_id,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return false;
        };
        let index = config_id as usize;
        if instance.active.load(Ordering::Relaxed) || index >= instance.layouts.len() {
            return false;
        }
        instance.config.store(index, Ordering::Relaxed);
        // A new configuration has its ports active again, as
        // `audio-ports-activation` says of a selection.
        instance.sidechain_on.store(true, Ordering::Relaxed);
        true
    })
}

unsafe extern "C" fn ports_can_activate_while_processing(_plugin: *const clap_plugin) -> bool {
    false
}

unsafe extern "C" fn ports_set_active<P: Plugin>(
    plugin: *const clap_plugin,
    is_input: bool,
    port_index: u32,
    is_active: bool,
    _sample_size: u32,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory.
        let Some(instance) = (unsafe { instance::<P>(plugin) }) else {
            return false;
        };
        if instance.active.load(Ordering::Relaxed) {
            return false;
        }
        let layout = instance.selected();
        let index = port_index as usize;
        if index >= port_channels(layout, is_input).1 {
            return false;
        }
        let is_sidechain =
            is_input && layout.sidechain > 0 && index == usize::from(layout.inputs > 0);
        if is_sidechain {
            instance.sidechain_on.store(is_active, Ordering::Relaxed);
            return true;
        }
        // The main ports carry what the plugin is; they stay on.
        is_active
    })
}

// --- Voices and note names --------------------------------------------------

unsafe extern "C" fn voice_info_get<P: Plugin>(
    plugin: *const clap_plugin,
    info: *mut clap_voice_info,
) -> bool {
    guard(false, || {
        // SAFETY: the host passes a plugin it created through our factory,
        // and a struct it owns, or null.
        let (Some(instance), Some(info)) =
            (unsafe { instance::<P>(plugin) }, unsafe { info.as_mut() })
        else {
            return false;
        };
        let voices = *instance
            .voices
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let Some(voices) = voices else {
            return false;
        };
        info.voice_count = voices.voices;
        info.voice_capacity = voices.capacity.max(voices.voices);
        info.flags = if voices.overlapping_notes {
            CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES
        } else {
            0
        };
        true
    })
}

unsafe extern "C" fn note_name_count<P: Plugin>(_plugin: *const clap_plugin) -> u32 {
    P::NOTE_NAMES.len() as u32
}

unsafe extern "C" fn note_name_get<P: Plugin>(
    _plugin: *const clap_plugin,
    index: u32,
    note_name: *mut clap_note_name,
) -> bool {
    guard(false, || {
        // SAFETY: a struct the host owns, or null.
        let (Some(out), Some(name)) = (
            unsafe { note_name.as_mut() },
            P::NOTE_NAMES.get(index as usize),
        ) else {
            return false;
        };
        copy_text(name.name, &mut out.name);
        // The plugin's one note port.
        out.port = 0;
        out.key = i16::from(name.key & 0x7F);
        out.channel = name.channel.map_or(-1, |channel| i16::from(channel & 0x0F));
        true
    })
}

unsafe extern "C" fn note_ports_count<P: Plugin>(
    _plugin: *const clap_plugin,
    is_input: bool,
) -> u32 {
    u32::from(if is_input {
        takes_notes::<P>()
    } else {
        P::NOTE_OUTPUT
    })
}

unsafe extern "C" fn note_ports_get<P: Plugin>(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    guard(false, || {
        // SAFETY: `info` is a struct the host owns, or null.
        let Some(info) = (unsafe { info.as_mut() }) else {
            return false;
        };
        let present = if is_input {
            takes_notes::<P>()
        } else {
            P::NOTE_OUTPUT
        };
        if index != 0 || !present {
            return false;
        }
        info.id = 0;
        // MIDI of every kind ends in the same events as CLAP's own notes
        // and expressions, through the plugin's `NoteInput`.
        info.supported_dialects = CLAP_NOTE_DIALECT_CLAP
            | CLAP_NOTE_DIALECT_MIDI
            | CLAP_NOTE_DIALECT_MIDI_MPE
            | CLAP_NOTE_DIALECT_MIDI2;
        // What goes out is written in whichever of these the host takes;
        // see the `output` module.
        info.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
        copy_text(if is_input { "Notes" } else { "Notes out" }, &mut info.name);
        true
    })
}

unsafe extern "C" fn latency_get<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    guard(0, || {
        // SAFETY: the host passes a plugin it created through our factory.
        unsafe { instance::<P>(plugin) }
            .map_or(0, |instance| instance.latency.load(Ordering::Relaxed))
    })
}

unsafe extern "C" fn tail_get<P: Plugin>(plugin: *const clap_plugin) -> u32 {
    guard(INFINITE_TAIL, || {
        // SAFETY: the host passes a plugin it created through our factory.
        unsafe { instance::<P>(plugin) }.map_or(INFINITE_TAIL, |instance| {
            instance.tail.load(Ordering::Relaxed)
        })
    })
}

#[cfg(test)]
mod tests {
    use std::assert_eq;

    use super::*;

    fn clap_transport(flags: u32) -> clap_event_transport {
        clap_event_transport {
            header: clap_event_header {
                size: size_of::<clap_event_transport>() as u32,
                time: 0,
                space_id: CLAP_CORE_EVENT_SPACE_ID,
                type_: CLAP_EVENT_TRANSPORT,
                flags: 0,
            },
            flags,
            song_pos_beats: 21 * CLAP_BEATTIME_FACTOR / 2,
            song_pos_seconds: 5 * CLAP_SECTIME_FACTOR,
            tempo: 126.0,
            tempo_inc: 0.001,
            loop_start_beats: 8 * CLAP_BEATTIME_FACTOR,
            loop_end_beats: 16 * CLAP_BEATTIME_FACTOR,
            loop_start_seconds: 0,
            loop_end_seconds: 0,
            bar_start: 9 * CLAP_BEATTIME_FACTOR,
            bar_number: 3,
            tsig_num: 3,
            tsig_denom: 4,
        }
    }

    #[test]
    fn every_field_of_the_hosts_transport_is_read() {
        let all = CLAP_TRANSPORT_HAS_TEMPO
            | CLAP_TRANSPORT_HAS_BEATS_TIMELINE
            | CLAP_TRANSPORT_HAS_SECONDS_TIMELINE
            | CLAP_TRANSPORT_HAS_TIME_SIGNATURE
            | CLAP_TRANSPORT_IS_PLAYING
            | CLAP_TRANSPORT_IS_RECORDING
            | CLAP_TRANSPORT_IS_LOOP_ACTIVE;
        let transport = convert_transport(&clap_transport(all));
        assert_eq!(
            transport,
            Transport {
                playing: true,
                recording: true,
                tempo_bpm: Some(126.0),
                time_signature: Some((3, 4)),
                position_samples: None,
                position_beats: Some(10.5),
                bar_start_beats: Some(9.0),
                loop_beats: Some((8.0, 16.0)),
                position_seconds: Some(5.0),
                bar_number: Some(3),
                tempo_increment: Some(0.001),
            }
        );

        // What the flags do not vouch for is unknown.
        assert_eq!(convert_transport(&clap_transport(0)), Transport::STOPPED);
    }

    #[test]
    fn all_tail_boundaries_return_correct_value() {
        assert_eq!(clap_tail(Tail::None), 0, "Tail::None is 0");
        assert_eq!(
            clap_tail(Tail::Samples(480)),
            480,
            "480 is a common tail length"
        );
        assert_eq!(
            clap_tail(Tail::Samples(i32::MAX as usize - 1)),
            i32::MAX as u32 - 1,
            "i32::MAX - 1 is the largest possible value"
        );
        assert_eq!(
            clap_tail(Tail::Samples(i32::MAX as usize)),
            i32::MAX as u32 - 1,
            "Values above i32::MAX are clamped to i32::MAX - 1"
        );
        assert_eq!(
            clap_tail(Tail::Samples(usize::MAX)),
            i32::MAX as u32 - 1,
            "Values above i32::MAX are clamped to i32::MAX - 1"
        );
        assert_eq!(
            clap_tail(Tail::Infinite),
            INFINITE_TAIL,
            "Tail::Infinite is already infinite"
        );
    }
}
