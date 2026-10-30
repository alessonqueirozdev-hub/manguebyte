//! Drives the CLAP wrapper through the C interface, the way a host does.
//!
//! Nothing here reaches into the wrapper's internals. The test plays the
//! part of a host: it asks the entry point for the factory, creates a
//! plugin, queries its extensions and calls `process` with raw buffers and
//! event lists. If these pass, a real host sees the same behaviour.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use manguebyte_clap::clap_sys::audio_buffer::clap_audio_buffer;
use manguebyte_clap::clap_sys::entry::clap_plugin_entry;
use manguebyte_clap::clap_sys::events::*;
use manguebyte_clap::clap_sys::ext::audio_ports::*;
use manguebyte_clap::clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use manguebyte_clap::clap_sys::ext::note_ports::*;
use manguebyte_clap::clap_sys::ext::params::*;
use manguebyte_clap::clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use manguebyte_clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte_clap::clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use manguebyte_clap::clap_sys::host::clap_host;
use manguebyte_clap::clap_sys::id::CLAP_INVALID_ID;
use manguebyte_clap::clap_sys::plugin::clap_plugin;
use manguebyte_clap::clap_sys::process::*;
use manguebyte_clap::clap_sys::stream::{clap_istream, clap_ostream};
use manguebyte_clap::clap_sys::version::CLAP_VERSION;
use manguebyte_clap::plugin_entry;
use manguebyte_core::{AtomicF64, AudioBuffer, NoDenormals, ParamId};
use manguebyte_dsp::event::{EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice, split_at_events};
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

// --- Watching the heap ---------------------------------------------------
//
// The wrapper's `process` runs on the host's audio thread, so it must not
// allocate or free. The allocator below counts heap calls made while a
// thread is inside `process`, and the tests require that count to be zero.

thread_local! {
    static IN_PROCESS: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        let _ = IN_PROCESS.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

// --- The plugins under test ---------------------------------------------

struct LevelParams {
    level: FloatParam,
    shape: ChoiceParam,
    explode: BoolParam,
}
param_set!(LevelParams {
    level,
    shape,
    explode
});

const LEVEL: ParamId = ParamId::from_name("level");
const SHAPE: ParamId = ParamId::from_name("shape");

/// Where the test, playing the part of a user interface, finds the
/// parameters of the most recently created `Level`.
static LEVEL_PARAMS: Mutex<Option<Arc<LevelParams>>> = Mutex::new(None);

/// Half of the smallest normal number, worked out by the processor and not
/// by the compiler. It is subnormal, unless such numbers are being flushed.
fn half_of_the_smallest() -> f32 {
    use std::hint::black_box;
    black_box(black_box(f32::MIN_POSITIVE) * black_box(0.5))
}

/// What `half_of_the_smallest` came to inside the last `Level::process`.
static HALF_INSIDE_PROCESS: AtomicU32 = AtomicU32::new(u32::MAX);

fn level_params() -> Arc<LevelParams> {
    LEVEL_PARAMS.lock().unwrap().clone().unwrap()
}

/// Scales its input by `level`, following events alone and never polling.
struct Level {
    params: Arc<LevelParams>,
    current: f32,
    /// The transport tempo seen in the last block, for the test to read, or
    /// a negative number when there was none.
    ///
    /// This is an atomic and not a mutex on purpose. On macOS a standard
    /// mutex allocates the first time it is locked, and the allocation guard
    /// caught an earlier version of this plugin doing exactly that inside
    /// `process`.
    tempo_seen: Arc<AtomicF64>,
}

static TEMPO_SEEN: Mutex<Option<Arc<AtomicF64>>> = Mutex::new(None);

impl Plugin for Level {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.level",
        name: "Level",
        vendor: "Manguebyte",
        version: "1.2.3",
        description: "Scales the input",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = LevelParams;

    fn new() -> Self {
        let params = Arc::new(LevelParams {
            level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 4.0), 1.0)
                .with_unit("x"),
            shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 1),
            explode: BoolParam::new("explode", "Explode", false).hidden(),
        });
        *LEVEL_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
        let tempo_seen = Arc::new(AtomicF64::new(-1.0));
        *TEMPO_SEEN.lock().unwrap() = Some(Arc::clone(&tempo_seen));
        Self {
            current: params.level.value(),
            params,
            tempo_seen,
        }
    }

    fn params(&self) -> Arc<LevelParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        assert!(!self.params.explode.value(), "the plugin was told to fail");
        HALF_INSIDE_PROCESS.store(half_of_the_smallest().to_bits(), Ordering::Relaxed);
        self.tempo_seen.store(
            context.transport.tempo_bpm.unwrap_or(-1.0),
            Ordering::Relaxed,
        );
        split_at_events(
            &mut self.current,
            buffer,
            context.events,
            |current, event| {
                if let EventKind::ParamValue {
                    param: LEVEL,
                    value,
                } = event.kind
                {
                    *current = value as f32;
                }
            },
            |current, piece| piece.apply_gain(*current),
        );
    }

    fn latency_samples(&self) -> usize {
        7
    }
}

// The macro is what a real plugin crate uses, so the effect goes through it.
manguebyte_clap::export_clap!(Level);

const BRIGHTNESS: ParamId = ParamId::from_name("brightness");

/// Outputs its key number while held, plus whatever it was modulated by
/// and ten times its pan, scaled by pressure.
#[derive(Default)]
struct KeyVoice {
    level: f32,
    pressure: f32,
    modulation: f32,
    pan: f32,
}

impl Voice for KeyVoice {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, note: NoteRef, _: f32) {
        self.level = f32::from(note.key);
    }
    fn note_off(&mut self, _: f32) {
        self.level = 0.0;
    }
    fn choke(&mut self) {
        self.level = 0.0;
        self.pressure = 1.0;
        self.modulation = 0.0;
        self.pan = 0.0;
    }
    fn is_active(&self) -> bool {
        self.level > 0.0
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        if self.level == 0.0 {
            // Released: a finished voice is still rendered to the end of
            // the piece of the block it finished in, and must stay silent.
            return;
        }
        let sample = (self.level + self.modulation + 10.0 * self.pan) * self.pressure;
        for channel in buffer.iter_channels_mut() {
            channel.iter_mut().for_each(|out| *out += sample);
        }
    }
    fn expression(&mut self, expression: Expression, value: f64) {
        match expression {
            Expression::Pressure => self.pressure = value as f32,
            Expression::Pan => self.pan = value as f32,
            _ => {}
        }
    }
    fn modulate(&mut self, param: ParamId, amount: f64) {
        if param == BRIGHTNESS {
            self.modulation = amount as f32;
        }
    }
}

struct KeysParams {
    brightness: FloatParam,
}
param_set!(KeysParams { brightness });

struct Keys {
    synth: Synth<KeyVoice>,
    params: Arc<KeysParams>,
}

impl Plugin for Keys {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.keys",
        name: "Keys",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays key numbers",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 2,
    };
    type Params = KeysParams;

    fn new() -> Self {
        Self {
            synth: Synth::with_voices(4, KeyVoice::default),
            params: Arc::new(KeysParams {
                brightness: FloatParam::new(
                    "brightness",
                    "Brightness",
                    FloatRange::linear(0.0, 100.0),
                    50.0,
                ),
            }),
        }
    }
    fn params(&self) -> Arc<KeysParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
        // Make the sidechain audible so the test can see it arrived.
        if let Some(sidechain) = context.sidechain {
            let level = sidechain.peak();
            buffer.channel_mut(0).iter_mut().for_each(|s| *s += level);
        }
    }
    fn reset(&mut self) {
        self.synth.reset();
    }
}

// --- A minimal host ------------------------------------------------------

/// Rescans the plugin has asked for, as the flags it passed.
static RESCANS: Mutex<Vec<u32>> = Mutex::new(Vec::new());

unsafe extern "C" fn host_rescan(_: *const clap_host, flags: clap_param_rescan_flags) {
    RESCANS.lock().unwrap().push(flags);
}
unsafe extern "C" fn host_clear(_: *const clap_host, _: u32, _: clap_param_clear_flags) {}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_rescan),
    clear: Some(host_clear),
    request_flush: Some(host_request),
};

unsafe extern "C" fn host_get_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
        ptr::from_ref(&HOST_PARAMS).cast()
    } else {
        ptr::null()
    }
}
unsafe extern "C" fn host_request(_: *const clap_host) {}

/// One event the host sends, stored as its concrete CLAP struct.
enum Sent {
    Note(clap_event_note),
    Expression(clap_event_note_expression),
    Value(clap_event_param_value),
    Modulation(clap_event_param_mod),
    Midi(clap_event_midi),
    Midi2(clap_event_midi2),
}

impl Sent {
    /// A pointer to the whole event, viewed as its header, which is how a
    /// host hands events over. It must be derived from the whole struct:
    /// the plugin reads past the header through it.
    fn header(&self) -> *const clap_event_header {
        match self {
            Self::Note(event) => ptr::from_ref(event).cast(),
            Self::Expression(event) => ptr::from_ref(event).cast(),
            Self::Value(event) => ptr::from_ref(event).cast(),
            Self::Modulation(event) => ptr::from_ref(event).cast(),
            Self::Midi(event) => ptr::from_ref(event).cast(),
            Self::Midi2(event) => ptr::from_ref(event).cast(),
        }
    }
}

fn header<T>(time: u32, type_: clap_event_type) -> clap_event_header {
    clap_event_header {
        size: size_of::<T>() as u32,
        time,
        space_id: CLAP_CORE_EVENT_SPACE_ID,
        type_,
        flags: 0,
    }
}

fn note(time: u32, type_: clap_event_type, key: i16, note_id: i32) -> Sent {
    Sent::Note(clap_event_note {
        header: header::<clap_event_note>(time, type_),
        note_id,
        port_index: 0,
        channel: 0,
        key,
        velocity: 1.0,
    })
}

fn value(time: u32, param: ParamId, value: f64) -> Sent {
    Sent::Value(clap_event_param_value {
        header: header::<clap_event_param_value>(time, CLAP_EVENT_PARAM_VALUE),
        param_id: param.0,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    })
}

fn midi(time: u32, data: [u8; 3]) -> Sent {
    Sent::Midi(clap_event_midi {
        header: header::<clap_event_midi>(time, CLAP_EVENT_MIDI),
        port_index: 0,
        data,
    })
}

unsafe extern "C" fn events_size(list: *const clap_input_events) -> u32 {
    unsafe { (*(*list).ctx.cast::<Vec<Sent>>()).len() as u32 }
}

unsafe extern "C" fn events_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    unsafe { (&(*(*list).ctx.cast::<Vec<Sent>>()))[index as usize].header() }
}

/// Collects the parameter values the plugin reports back.
unsafe extern "C" fn events_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        if (*event).type_ == CLAP_EVENT_PARAM_VALUE {
            let event = &*event.cast::<clap_event_param_value>();
            (*(*list).ctx.cast::<Vec<(u32, f64)>>()).push((event.param_id, event.value));
        }
    }
    true
}

/// Accepts a few bytes at a time, to exercise the plugin's write loop.
unsafe extern "C" fn stream_write(
    stream: *const clap_ostream,
    data: *const c_void,
    size: u64,
) -> i64 {
    let count = size.min(7) as usize;
    unsafe {
        let bytes = std::slice::from_raw_parts(data.cast::<u8>(), count);
        (*(*stream).ctx.cast::<Vec<u8>>()).extend_from_slice(bytes);
    }
    count as i64
}

/// Hands out a few bytes at a time, to exercise the plugin's read loop.
unsafe extern "C" fn stream_read(stream: *const clap_istream, data: *mut c_void, size: u64) -> i64 {
    unsafe {
        let (bytes, position) = &mut *(*stream).ctx.cast::<(Vec<u8>, usize)>();
        let count = (size.min(5) as usize).min(bytes.len() - *position);
        ptr::copy_nonoverlapping(bytes[*position..].as_ptr(), data.cast::<u8>(), count);
        *position += count;
        count as i64
    }
}

/// What one call to `process` produced.
struct Processed {
    status: clap_process_status,
    /// One vector per output channel.
    output: Vec<Vec<f32>>,
    /// Parameter values the plugin reported, as `(id, value)`.
    reported: Vec<(u32, f64)>,
    /// How many times the plugin used the heap during the call.
    heap_calls: usize,
}

struct Host {
    entry: clap_plugin_entry,
    factory: *const clap_plugin_factory,
    plugin: *const clap_plugin,
    /// The `clap_host` the plugin was given, owned through a raw pointer.
    ///
    /// The plugin keeps a pointer to it. Holding it in a `Box` instead would
    /// be a mistake Miri catches: moving a `Box` asserts exclusive access to
    /// what it points to, which invalidates the pointer the plugin holds.
    host: *mut clap_host,
    /// What this host alone records, for a host made with
    /// [`recording`](Self::recording). The `clap_host` points at it.
    recorder: Option<Arc<Recorder>>,
}

/// What one host was asked for, kept apart from [`RESCANS`], which every
/// host made with `Host::new` shares, so that tests running at the same
/// time cannot see each other's calls.
#[derive(Default)]
struct Recorder {
    rescans: Mutex<Vec<u32>>,
    callbacks: AtomicU32,
}

impl Recorder {
    fn take_rescans(&self) -> Vec<u32> {
        std::mem::take(&mut *self.rescans.lock().unwrap())
    }

    fn callbacks(&self) -> u32 {
        self.callbacks.load(Ordering::SeqCst)
    }
}

unsafe fn recorder<'a>(host: *const clap_host) -> &'a Recorder {
    unsafe { &*(*host).host_data.cast::<Recorder>() }
}

unsafe extern "C" fn recording_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    unsafe { recorder(host) }
        .rescans
        .lock()
        .unwrap()
        .push(flags);
}

unsafe extern "C" fn recording_request_callback(host: *const clap_host) {
    unsafe { recorder(host) }
        .callbacks
        .fetch_add(1, Ordering::SeqCst);
}

static RECORDING_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(recording_rescan),
    clear: Some(host_clear),
    request_flush: Some(host_request),
};

unsafe extern "C" fn recording_get_extension(
    _: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_PARAMS {
        ptr::from_ref(&RECORDING_PARAMS).cast()
    } else {
        ptr::null()
    }
}

impl Host {
    fn new(entry: clap_plugin_entry, id: &str) -> Self {
        Self::create(entry, id, None)
    }

    /// A host that records its own rescans and requests for a call on the
    /// main thread, which `Recorder` gives back.
    fn recording(entry: clap_plugin_entry, id: &str) -> (Self, Arc<Recorder>) {
        let recorder = Arc::new(Recorder::default());
        (
            Self::create(entry, id, Some(Arc::clone(&recorder))),
            recorder,
        )
    }

    fn create(entry: clap_plugin_entry, id: &str, recorder: Option<Arc<Recorder>>) -> Self {
        let (host_data, get_extension, request_callback): (
            *mut c_void,
            unsafe extern "C" fn(*const clap_host, *const c_char) -> *const c_void,
            unsafe extern "C" fn(*const clap_host),
        ) = match &recorder {
            Some(recorder) => (
                Arc::as_ptr(recorder).cast_mut().cast(),
                recording_get_extension,
                recording_request_callback,
            ),
            None => (ptr::null_mut(), host_get_extension, host_request),
        };
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data,
            name: c"test host".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(get_extension),
            request_restart: Some(host_request),
            request_process: Some(host_request),
            request_callback: Some(request_callback),
        }));
        let id = CString::new(id).unwrap();
        unsafe {
            assert!(entry.init.unwrap()(c"/plugins/test.clap".as_ptr()));
            let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>();
            assert!(!factory.is_null());
            let plugin = (*factory).create_plugin.unwrap()(factory, host, id.as_ptr());
            assert!(!plugin.is_null(), "the factory refused to create {id:?}");
            assert!((*plugin).init.unwrap()(plugin));
            Self {
                entry,
                factory,
                plugin,
                host,
                recorder,
            }
        }
    }

    /// Calls the plugin's `on_main_thread`, as a host does when asked.
    fn on_main_thread(&self) {
        unsafe { (*self.plugin).on_main_thread.unwrap()(self.plugin) };
    }

    fn extension<T>(&self, id: &CStr) -> Option<&T> {
        unsafe {
            (*self.plugin).get_extension.unwrap()(self.plugin, id.as_ptr())
                .cast::<T>()
                .as_ref()
        }
    }

    fn params(&self) -> &clap_plugin_params {
        self.extension(CLAP_EXT_PARAMS).unwrap()
    }

    fn activate(&self, sample_rate: f64, max_frames: u32) {
        unsafe {
            assert!((*self.plugin).activate.unwrap()(
                self.plugin,
                sample_rate,
                1,
                max_frames
            ));
            assert!((*self.plugin).start_processing.unwrap()(self.plugin));
        }
    }

    fn param_info(&self, index: u32) -> Option<clap_param_info> {
        unsafe {
            let mut info: clap_param_info = std::mem::zeroed();
            self.params().get_info.unwrap()(self.plugin, index, &raw mut info).then_some(info)
        }
    }

    fn param_value(&self, id: ParamId) -> Option<f64> {
        let mut value = f64::NAN;
        unsafe {
            self.params().get_value.unwrap()(self.plugin, id.0, &raw mut value).then_some(value)
        }
    }

    fn param_text(&self, id: ParamId, value: f64) -> Option<String> {
        let mut buffer = [0 as c_char; 64];
        unsafe {
            self.params().value_to_text.unwrap()(
                self.plugin,
                id.0,
                value,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
            )
            .then(|| CStr::from_ptr(buffer.as_ptr()).to_str().unwrap().to_owned())
        }
    }

    fn param_from_text(&self, id: ParamId, text: &str) -> Option<f64> {
        let text = CString::new(text).unwrap();
        let mut value = f64::NAN;
        unsafe {
            self.params().text_to_value.unwrap()(self.plugin, id.0, text.as_ptr(), &raw mut value)
                .then_some(value)
        }
    }

    fn flush(&self, events: Vec<Sent>) {
        let mut events = events;
        let input = clap_input_events {
            ctx: (&raw mut events).cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let mut reported: Vec<(u32, f64)> = Vec::new();
        let output = clap_output_events {
            ctx: (&raw mut reported).cast(),
            try_push: Some(events_push),
        };
        unsafe { self.params().flush.unwrap()(self.plugin, &raw const input, &raw const output) };
    }

    fn save(&self) -> Option<Vec<u8>> {
        let state: &clap_plugin_state = self.extension(CLAP_EXT_STATE).unwrap();
        let mut bytes: Vec<u8> = Vec::new();
        let stream = clap_ostream {
            ctx: (&raw mut bytes).cast(),
            write: Some(stream_write),
        };
        unsafe { state.save.unwrap()(self.plugin, &raw const stream).then_some(bytes) }
    }

    fn load(&self, bytes: &[u8]) -> bool {
        let state: &clap_plugin_state = self.extension(CLAP_EXT_STATE).unwrap();
        let mut source = (bytes.to_vec(), 0usize);
        let stream = clap_istream {
            ctx: (&raw mut source).cast(),
            read: Some(stream_read),
        };
        unsafe { state.load.unwrap()(self.plugin, &raw const stream) }
    }

    /// Processes one block.
    ///
    /// `main` is the main port's memory, used for input and output alike,
    /// as a host does when processing in place. `sidechain`, if given, is a
    /// second input port.
    fn process(
        &self,
        main: &mut [Vec<f32>],
        sidechain: Option<&mut [Vec<f32>]>,
        events: Vec<Sent>,
        tempo: Option<f64>,
    ) -> Processed {
        let frames = main[0].len();
        let mut main_channels: Vec<*mut f32> = main.iter_mut().map(|c| c.as_mut_ptr()).collect();
        let main_port = clap_audio_buffer {
            data32: main_channels.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: main_channels.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        let mut side_channels: Vec<*mut f32> = sidechain
            .map(|channels| channels.iter_mut().map(|c| c.as_mut_ptr()).collect())
            .unwrap_or_default();
        let side_port = clap_audio_buffer {
            data32: side_channels.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: side_channels.len() as u32,
            latency: 0,
            constant_mask: 0,
        };
        // Hand over the ports the plugin declared, in its order. An
        // instrument has no main input, so its sidechain comes first.
        let has_main_input = self
            .audio_port(0, true)
            .is_some_and(|port| port.flags & CLAP_AUDIO_PORT_IS_MAIN != 0);
        let inputs: Vec<clap_audio_buffer> = match (has_main_input, side_channels.is_empty()) {
            (true, true) => vec![main_port],
            (true, false) => vec![main_port, side_port],
            (false, false) => vec![side_port],
            (false, true) => vec![],
        };
        let mut outputs = [main_port];

        let mut events = events;
        let input_events = clap_input_events {
            ctx: (&raw mut events).cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        // Room for what the plugin reports, so that collecting it is not
        // itself counted as heap use.
        let mut reported: Vec<(u32, f64)> = Vec::with_capacity(64);
        let output_events = clap_output_events {
            ctx: (&raw mut reported).cast(),
            try_push: Some(events_push),
        };
        let transport = tempo.map(|tempo| clap_event_transport {
            header: header::<clap_event_transport>(0, CLAP_EVENT_TRANSPORT),
            flags: CLAP_TRANSPORT_HAS_TEMPO
                | CLAP_TRANSPORT_IS_PLAYING
                | CLAP_TRANSPORT_HAS_BEATS_TIMELINE,
            song_pos_beats: 3 * CLAP_BEATTIME_FACTOR,
            song_pos_seconds: 0,
            tempo,
            tempo_inc: 0.0,
            loop_start_beats: 0,
            loop_end_beats: 0,
            loop_start_seconds: 0,
            loop_end_seconds: 0,
            bar_start: 0,
            bar_number: 0,
            tsig_num: 4,
            tsig_denom: 4,
        });

        let process = clap_process {
            steady_time: -1,
            frames_count: frames as u32,
            transport: transport.as_ref().map_or(ptr::null(), ptr::from_ref),
            audio_inputs: inputs.as_ptr(),
            audio_outputs: outputs.as_mut_ptr(),
            audio_inputs_count: inputs.len() as u32,
            audio_outputs_count: 1,
            in_events: &raw const input_events,
            out_events: &raw const output_events,
        };
        HEAP_CALLS.with(|calls| calls.set(0));
        IN_PROCESS.with(|watching| watching.set(true));
        let status = unsafe { (*self.plugin).process.unwrap()(self.plugin, &raw const process) };
        IN_PROCESS.with(|watching| watching.set(false));
        Processed {
            status,
            output: main.to_vec(),
            reported,
            heap_calls: HEAP_CALLS.with(Cell::get),
        }
    }

    fn input_ports(&self) -> u32 {
        let ports: &clap_plugin_audio_ports = self.extension(CLAP_EXT_AUDIO_PORTS).unwrap();
        unsafe { ports.count.unwrap()(self.plugin, true) }
    }

    fn audio_port(&self, index: u32, is_input: bool) -> Option<clap_audio_port_info> {
        let ports: &clap_plugin_audio_ports = self.extension(CLAP_EXT_AUDIO_PORTS).unwrap();
        unsafe {
            let mut info: clap_audio_port_info = std::mem::zeroed();
            ports.get.unwrap()(self.plugin, index, is_input, &raw mut info).then_some(info)
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            (*self.plugin).stop_processing.unwrap()(self.plugin);
            (*self.plugin).deactivate.unwrap()(self.plugin);
            (*self.plugin).destroy.unwrap()(self.plugin);
            self.entry.deinit.unwrap()();
            // The plugin is gone, so nothing points at the host any more.
            drop(Box::from_raw(self.host));
        }
        // And nothing at the recorder.
        drop(self.recorder.take());
    }
}

fn text(pointer: *const c_char) -> String {
    unsafe { CStr::from_ptr(pointer).to_str().unwrap().to_owned() }
}

fn name(field: &[c_char]) -> String {
    text(field.as_ptr())
}

/// The plugins under test share globals, so the tests that use `Level` must
/// not run at the same time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn level_host() -> (std::sync::MutexGuard<'static, ()>, Host) {
    let guard = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let host = Host::new(clap_entry, Level::INFO.id);
    host.activate(48_000.0, 64);
    (guard, host)
}

fn stereo(value: f32, frames: usize) -> Vec<Vec<f32>> {
    vec![vec![value; frames]; 2]
}

// --- The tests -----------------------------------------------------------

#[test]
fn the_heap_watcher_itself_works() {
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_PROCESS.with(|watching| watching.set(true));
    drop(std::hint::black_box(vec![0u8; 64]));
    IN_PROCESS.with(|watching| watching.set(false));
    assert!(
        HEAP_CALLS.with(Cell::get) >= 2,
        "an allocation went unnoticed"
    );
}

#[test]
fn the_factory_describes_and_creates_the_plugin() {
    let (_guard, host) = level_host();
    unsafe {
        let factory = host.factory;
        assert_eq!((*factory).get_plugin_count.unwrap()(factory), 1);
        let descriptor = &*(*factory).get_plugin_descriptor.unwrap()(factory, 0);
        assert_eq!(text(descriptor.id), "org.manguebyte.test.level");
        assert_eq!(text(descriptor.name), "Level");
        assert_eq!(text(descriptor.vendor), "Manguebyte");
        assert_eq!(text(descriptor.version), "1.2.3");
        assert_eq!(text(descriptor.description), "Scales the input");
        assert_eq!(text(*descriptor.features), "audio-effect");
        assert!((*descriptor.features.add(1)).is_null());
        assert!(ptr::eq((*host.plugin).desc, descriptor));

        assert!((*factory).get_plugin_descriptor.unwrap()(factory, 1).is_null());
        let other =
            (*factory).create_plugin.unwrap()(factory, host.host, c"org.example.other".as_ptr());
        assert!(other.is_null(), "created a plugin with the wrong id");
        assert!(host.entry.get_factory.unwrap()(c"clap.unknown-factory".as_ptr()).is_null());
    }
}

#[test]
fn parameters_are_listed_with_values_and_text() {
    let (_guard, host) = level_host();
    unsafe { assert_eq!(host.params().count.unwrap()(host.plugin), 3) };

    // A continuous parameter is a position in 0..=1.
    let level = host.param_info(0).unwrap();
    assert_eq!(level.id, LEVEL.0);
    assert_eq!(name(&level.name), "Level");
    assert_eq!((level.min_value, level.max_value), (0.0, 1.0));
    assert_eq!(level.default_value, 0.25);
    assert_eq!(
        level.flags,
        CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_MODULATABLE
    );
    assert_eq!(host.param_value(LEVEL), Some(0.25));
    assert_eq!(host.param_text(LEVEL, 0.5).unwrap(), "2.00 x");
    assert_eq!(host.param_from_text(LEVEL, "3 x"), Some(0.75));
    assert_eq!(host.param_from_text(LEVEL, "loud"), None);

    // A stepped parameter is a whole number of steps.
    let shape = host.param_info(1).unwrap();
    assert_eq!((shape.min_value, shape.max_value), (0.0, 2.0));
    assert_eq!(shape.default_value, 1.0);
    assert_eq!(
        shape.flags,
        CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED
    );
    assert_eq!(host.param_text(SHAPE, 2.0).unwrap(), "Square");
    assert_eq!(host.param_from_text(SHAPE, "sine"), Some(0.0));

    let explode = host.param_info(2).unwrap();
    assert_ne!(explode.flags & CLAP_PARAM_IS_HIDDEN, 0);

    assert!(host.param_info(3).is_none());
    assert_eq!(host.param_value(ParamId::from_name("nope")), None);
    assert!(host.param_text(ParamId::from_name("nope"), 0.0).is_none());
}

#[test]
fn ports_follow_the_layout() {
    let (_guard, host) = level_host();
    assert_eq!(host.input_ports(), 1);
    for is_input in [true, false] {
        let port = host.audio_port(0, is_input).unwrap();
        assert_eq!(name(&port.name), "Main");
        assert_eq!(port.channel_count, 2);
        assert_eq!(
            port.flags,
            CLAP_AUDIO_PORT_IS_MAIN | CLAP_AUDIO_PORT_SUPPORTS_64BITS
        );
        assert_eq!(text(port.port_type), "stereo");
        assert_eq!(port.in_place_pair, 0, "in-place processing is allowed");
        assert!(host.audio_port(1, is_input).is_none());
    }
    // An effect takes no notes.
    assert!(
        host.extension::<clap_plugin_note_ports>(CLAP_EXT_NOTE_PORTS)
            .is_none()
    );
    assert!(
        host.extension::<c_void>(c"clap.no-such-extension")
            .is_none()
    );

    let latency: &clap_plugin_latency = host.extension(CLAP_EXT_LATENCY).unwrap();
    unsafe { assert_eq!(latency.get.unwrap()(host.plugin), 7) };
}

#[test]
fn automation_lands_on_its_frame_when_processing_in_place() {
    let (_guard, host) = level_host();
    let mut audio = stereo(1.0, 64);
    let events = vec![value(10, LEVEL, 0.5), value(40, LEVEL, 1.0)];
    let processed = host.process(&mut audio, None, events, Some(97.0));

    assert_eq!(processed.status, CLAP_PROCESS_CONTINUE);
    assert_eq!(processed.heap_calls, 0, "process used the heap");
    for channel in &processed.output {
        assert_eq!(&channel[..10], &[1.0; 10]);
        assert_eq!(&channel[10..40], &[2.0; 30]);
        assert_eq!(&channel[40..], &[4.0; 24]);
    }
    // The host made these changes, so they are not reported back to it.
    assert!(processed.reported.is_empty());
    assert_eq!(host.param_value(LEVEL), Some(1.0));
    assert_eq!(level_params().level.value(), 4.0);

    let tempo = TEMPO_SEEN.lock().unwrap().clone().unwrap();
    assert_eq!(tempo.load(Ordering::Relaxed), 97.0);
}

/// Host automation is never reported back to the host, also at values that
/// binary floating point cannot hold exactly: what the parameter holds
/// after storing the host's value is what the host knows.
#[test]
fn automation_at_inexact_values_is_not_reported_back() {
    let (_guard, host) = level_host();
    // 0.3 of the way up 0..4 is 1.2, which an `f32` cannot hold exactly.
    let processed = host.process(&mut stereo(1.0, 16), None, vec![value(4, LEVEL, 0.3)], None);
    assert!(processed.reported.is_empty());
    for _ in 0..3 {
        let processed = host.process(&mut stereo(1.0, 16), None, vec![], None);
        assert!(
            processed.reported.is_empty(),
            "sent back {:?}",
            processed.reported
        );
    }
    assert_eq!(level_params().level.value(), 1.2_f32);
}

#[test]
fn a_change_from_outside_reaches_the_plugin_and_is_reported_to_the_host_once() {
    let (_guard, host) = level_host();
    // A user interface writes straight to the parameter.
    level_params().level.set(3.0);

    let processed = host.process(&mut stereo(1.0, 16), None, vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 16]);
    assert_eq!(processed.reported, vec![(LEVEL.0, 0.75)]);
    assert_eq!(processed.heap_calls, 0, "reporting a change used the heap");

    let processed = host.process(&mut stereo(1.0, 16), None, vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 16]);
    assert!(
        processed.reported.is_empty(),
        "reported the same change twice"
    );

    // A stepped parameter is reported in steps.
    level_params().shape.set_index(2);
    let processed = host.process(&mut stereo(1.0, 16), None, vec![], None);
    assert_eq!(processed.reported, vec![(SHAPE.0, 2.0)]);
}

#[test]
fn state_survives_a_round_trip_through_host_streams() {
    let (_guard, host) = level_host();
    level_params().level.set(2.5);
    level_params().shape.set_index(0);
    let saved = host.save().unwrap();
    let as_text = String::from_utf8(saved.clone()).unwrap();
    assert!(as_text.starts_with("manguebyte-plugin 1 org.manguebyte.test.level\n"));
    // Let the plugin and the host both see that change.
    host.process(&mut stereo(1.0, 8), None, vec![], None);

    level_params().level.set(0.5);
    host.process(&mut stereo(1.0, 8), None, vec![], None);

    RESCANS.lock().unwrap().clear();
    assert!(host.load(&saved));
    // Values changed without an event, so the host is asked to reread them.
    assert_eq!(*RESCANS.lock().unwrap(), [CLAP_PARAM_RESCAN_VALUES]);
    assert_eq!(host.param_value(LEVEL), Some(0.625));
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    // The plugin heard about the restored value as an event...
    assert_eq!(processed.output[0], vec![2.5; 8]);
    // ...and the host, which did the restoring, was not told what it knows.
    assert!(processed.reported.is_empty());
    assert_eq!(
        processed.heap_calls, 0,
        "the block after a restore used the heap"
    );

    RESCANS.lock().unwrap().clear();
    assert!(!host.load(b"not a state"));
    assert!(!host.load(&[0xFF, 0xFE, 0x00]));
    let foreign = as_text.replace("test.level", "test.other");
    assert!(!host.load(foreign.as_bytes()));
    assert_eq!(
        level_params().level.value(),
        2.5,
        "a refused load changed a value"
    );
    assert!(
        RESCANS.lock().unwrap().is_empty(),
        "a refused load asked for a rescan"
    );
}

#[test]
fn flush_updates_values_without_processing() {
    let (_guard, host) = level_host();
    host.flush(vec![value(0, LEVEL, 0.75), value(0, SHAPE, 2.0)]);
    assert_eq!(host.param_value(LEVEL), Some(0.75));
    assert_eq!(level_params().level.value(), 3.0);
    assert_eq!(level_params().shape.index(), 2);

    // The plugin still hears about it on the next block, and the host is
    // not told about its own change.
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.output[0], vec![3.0; 8]);
    assert!(processed.reported.is_empty());
}

#[test]
fn numbers_too_small_to_hear_are_zero_while_the_plugin_runs_and_only_then() {
    let (_guard, host) = level_host();
    assert!(half_of_the_smallest() > 0.0, "the test thread flushes them");
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.status, CLAP_PROCESS_CONTINUE);

    let inside = f32::from_bits(HALF_INSIDE_PROCESS.load(Ordering::Relaxed));
    if NoDenormals::SUPPORTED {
        assert_eq!(inside, 0.0, "the plugin ran with subnormal numbers on");
    }
    // The host gets its own setting back.
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked into the host"
    );

    // Also when the plugin fails half-way.
    level_params().explode.set(true);
    host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert!(
        half_of_the_smallest() > 0.0,
        "the setting leaked after a panic"
    );
}

#[test]
fn a_block_larger_than_promised_and_a_panic_are_errors_not_crashes() {
    let (_guard, host) = level_host();
    let processed = host.process(&mut stereo(1.0, 65), None, vec![], None);
    assert_eq!(processed.status, CLAP_PROCESS_ERROR);
    assert_eq!(processed.heap_calls, 0, "refusing a block used the heap");

    level_params().explode.set(true);
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.status, CLAP_PROCESS_ERROR);

    // The plugin is still usable afterwards.
    level_params().explode.set(false);
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.status, CLAP_PROCESS_CONTINUE);
}

/// A value the host sent in a block the plugin panicked in was stored, but
/// never reached the plugin, which follows events alone. The next block
/// gives it every value again, so it does not keep playing the old one.
#[test]
fn after_a_panic_the_next_block_gives_the_plugin_every_value() {
    let (_guard, host) = level_host();
    level_params().explode.set(true);
    let processed = host.process(&mut stereo(1.0, 8), None, vec![value(0, LEVEL, 0.5)], None);
    assert_eq!(processed.status, CLAP_PROCESS_ERROR);
    assert_eq!(level_params().level.value(), 2.0, "the value was stored");

    level_params().explode.set(false);
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.status, CLAP_PROCESS_CONTINUE);
    assert_eq!(
        processed.output[0],
        vec![2.0; 8],
        "the plugin kept the value from before the panic"
    );
    // Only once: the block after that announces nothing new.
    let processed = host.process(&mut stereo(1.0, 8), None, vec![], None);
    assert_eq!(processed.output[0], vec![2.0; 8]);
    assert_eq!(processed.heap_calls, 0);
}

#[test]
fn note_pan_arrives_in_the_framework_unit() {
    // CLAP's pan runs from 0 (left) through 0.5 (centre) to 1 (right); the
    // framework's from -1 to 1.
    let host = Host::new(plugin_entry::<Keys>(), Keys::INFO.id);
    host.activate(48_000.0, 32);
    let pan = |time: u32, value: f64| {
        Sent::Expression(clap_event_note_expression {
            header: header::<clap_event_note_expression>(time, CLAP_EVENT_NOTE_EXPRESSION),
            expression_id: CLAP_NOTE_EXPRESSION_PAN,
            note_id: 7,
            port_index: 0,
            channel: 0,
            key: -1,
            value,
        })
    };
    let events = vec![
        note(0, CLAP_EVENT_NOTE_ON, 60, 7),
        pan(4, 0.25),
        pan(8, 0.5),
        pan(12, 1.0),
        pan(16, 0.0),
    ];
    let processed = host.process(&mut [vec![0.0f32; 20]], None, events, None);
    let out = &processed.output[0];
    assert_eq!(&out[..4], &[60.0; 4]);
    assert_eq!(&out[4..8], &[55.0; 4], "a quarter of the way is -0.5");
    assert_eq!(&out[8..12], &[60.0; 4], "the middle is the centre");
    assert_eq!(&out[12..16], &[70.0; 4], "the top is fully right");
    assert_eq!(&out[16..], &[50.0; 4], "the bottom is fully left");
}

fn midi2(time: u32, words: &[u32]) -> Sent {
    let mut data = [0; 4];
    data[..words.len()].copy_from_slice(words);
    Sent::Midi2(clap_event_midi2 {
        header: header::<clap_event_midi2>(time, CLAP_EVENT_MIDI2),
        port_index: 0,
        data,
    })
}

#[test]
fn clap_notes_mpe_and_midi_2_end_in_the_same_events() {
    let host = Host::new(plugin_entry::<Keys>(), Keys::INFO.id);
    host.activate(48_000.0, 32);
    let notes: &clap_plugin_note_ports = host.extension(CLAP_EXT_NOTE_PORTS).unwrap();
    unsafe {
        let mut info: clap_note_port_info = std::mem::zeroed();
        assert!(notes.get.unwrap()(host.plugin, 0, true, &raw mut info));
        let all = CLAP_NOTE_DIALECT_CLAP
            | CLAP_NOTE_DIALECT_MIDI
            | CLAP_NOTE_DIALECT_MIDI_MPE
            | CLAP_NOTE_DIALECT_MIDI2;
        assert_eq!(info.supported_dialects, all);
    }

    // Two notes on key 60 at once, the second halved by pressure while
    // the first is not, then released one after the other: first in
    // CLAP's own dialect, with note identifiers and an expression...
    let pressure = Sent::Expression(clap_event_note_expression {
        header: header::<clap_event_note_expression>(8, CLAP_EVENT_NOTE_EXPRESSION),
        expression_id: CLAP_NOTE_EXPRESSION_PRESSURE,
        note_id: 2,
        port_index: 0,
        channel: 0,
        key: -1,
        value: 0.5,
    });
    let clap = vec![
        note(0, CLAP_EVENT_NOTE_ON, 60, 1),
        note(4, CLAP_EVENT_NOTE_ON, 60, 2),
        pressure,
        note(12, CLAP_EVENT_NOTE_OFF, 60, 1),
        note(16, CLAP_EVENT_NOTE_OFF, 60, 2),
    ];
    // ...then as MPE, on two member channels of a lower zone the stream
    // configures...
    let mpe = vec![
        midi(0, [0xB0, 100, 6]),
        midi(0, [0xB0, 101, 0]),
        midi(0, [0xB0, 6, 15]),
        midi(0, [0x91, 60, 100]),
        midi(4, [0x92, 60, 100]),
        midi(8, [0xD2, 64, 0]),
        midi(12, [0x81, 60, 0]),
        midi(16, [0x82, 60, 0]),
    ];
    // ...and as MIDI 2.0 packets on one channel, the first note detached
    // from its number before the second starts, so that the pressure
    // reaches the second alone.
    let midi_2 = vec![
        midi2(0, &[0x4090_3C00, 0xFFFF_0000]),
        // Per-note management: detach the first note.
        midi2(4, &[0x40F0_3C02, 0]),
        midi2(4, &[0x4090_3C00, 0xFFFF_0000]),
        midi2(8, &[0x40A0_3C00, 0x8000_0000]),
        midi2(12, &[0x4080_3C00, 0]),
        midi2(16, &[0x4080_3C00, 0]),
    ];
    let mut outputs = Vec::new();
    for events in [clap, mpe, midi_2] {
        let processed = host.process(&mut [vec![0.0f32; 24]], None, events, None);
        assert_eq!(processed.heap_calls, 0, "reading notes used the heap");
        outputs.push(processed.output[0].clone());
    }
    let expected = [
        [60.0; 4].as_slice(),
        &[120.0; 4],
        &[90.0; 4],
        &[30.0; 4],
        &[0.0; 8],
    ]
    .concat();
    assert_eq!(outputs[0], expected);
    assert_eq!(outputs[1], expected, "MPE");
    assert_eq!(outputs[2], expected, "MIDI 2.0");
}

#[test]
fn an_instrument_takes_notes_expression_modulation_and_a_sidechain() {
    let host = Host::new(plugin_entry::<Keys>(), Keys::INFO.id);
    host.activate(48_000.0, 32);

    // No main input: the only input port is the stereo sidechain.
    assert_eq!(host.input_ports(), 1);
    let sidechain = host.audio_port(0, true).unwrap();
    assert_eq!(name(&sidechain.name), "Sidechain");
    assert_eq!(
        (sidechain.channel_count, sidechain.flags),
        (2, CLAP_AUDIO_PORT_SUPPORTS_64BITS)
    );
    assert_eq!(sidechain.in_place_pair, CLAP_INVALID_ID);
    let output = host.audio_port(0, false).unwrap();
    assert_eq!(
        (output.channel_count, output.flags),
        (1, CLAP_AUDIO_PORT_IS_MAIN | CLAP_AUDIO_PORT_SUPPORTS_64BITS)
    );
    assert_eq!(text(output.port_type), "mono");

    let notes: &clap_plugin_note_ports = host.extension(CLAP_EXT_NOTE_PORTS).unwrap();
    unsafe {
        assert_eq!(notes.count.unwrap()(host.plugin, true), 1);
        assert_eq!(notes.count.unwrap()(host.plugin, false), 0);
        let mut info: clap_note_port_info = std::mem::zeroed();
        assert!(notes.get.unwrap()(host.plugin, 0, true, &raw mut info));
        assert_eq!(info.preferred_dialect, CLAP_NOTE_DIALECT_CLAP);
        assert_ne!(info.supported_dialects & CLAP_NOTE_DIALECT_MIDI, 0);
    }

    let pressure = Sent::Expression(clap_event_note_expression {
        header: header::<clap_event_note_expression>(12, CLAP_EVENT_NOTE_EXPRESSION),
        expression_id: CLAP_NOTE_EXPRESSION_PRESSURE,
        note_id: 5,
        port_index: 0,
        channel: 0,
        key: -1,
        value: 0.5,
    });
    // +0.1 of the 0..=1 scale on a 0..100 parameter is +10 in its own unit.
    let modulation = Sent::Modulation(clap_event_param_mod {
        header: header::<clap_event_param_mod>(16, CLAP_EVENT_PARAM_MOD),
        param_id: BRIGHTNESS.0,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: 0,
        channel: 0,
        key: 72,
        amount: 0.1,
    });
    let events = vec![
        note(4, CLAP_EVENT_NOTE_ON, 60, 5),
        midi(8, [0x90, 72, 100]),
        pressure,
        modulation,
        note(20, CLAP_EVENT_NOTE_OFF, 60, 5),
        midi(24, [0x80, 72, 0]),
    ];
    let mut audio = vec![vec![9.0f32; 32]];
    let processed = host.process(&mut audio, None, events, None);
    assert_eq!(processed.heap_calls, 0, "playing notes used the heap");
    let out = &processed.output[0];
    assert_eq!(&out[..4], &[0.0; 4]);
    assert_eq!(&out[4..8], &[60.0; 4]);
    assert_eq!(&out[8..12], &[132.0; 4]);
    // Pressure halves the note with id 5 only.
    assert_eq!(&out[12..16], &[102.0; 4]);
    // Modulation lifts key 72 only.
    assert_eq!(&out[16..20], &[112.0; 4]);
    assert_eq!(&out[20..24], &[82.0; 4]);
    assert_eq!(&out[24..], &[0.0; 8]);

    // A choke with a wildcard key and no id addresses nothing.
    let events = vec![
        note(0, CLAP_EVENT_NOTE_ON, 50, -1),
        note(2, CLAP_EVENT_NOTE_CHOKE, -1, -1),
        note(4, CLAP_EVENT_NOTE_CHOKE, 50, -1),
    ];
    let mut side = stereo(0.25, 8);
    let processed = host.process(&mut [vec![0.0f32; 8]], Some(&mut side), events, None);
    assert_eq!(
        processed.output[0],
        [50.25, 50.25, 50.25, 50.25, 0.25, 0.25, 0.25, 0.25]
    );
}

// --- Parameters renamed while the plugin runs -------------------------------

mod renamed {
    use super::*;
    use manguebyte_params::{Param, ParamSet};

    /// A fixed pool of stops driven by number, as an organ that swaps whole
    /// instruments has: after a swap the same parameters drive the new
    /// organ's stops, under its names.
    pub(super) struct Stops(Vec<BoolParam>);

    impl ParamSet for Stops {
        fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param)) {
            for stop in &self.0 {
                visitor(stop);
            }
        }
    }

    impl Stops {
        /// Gives every stop the name of `organ`'s rank, from this thread.
        fn load_organ(&self, organ: &str) {
            for (n, stop) in self.0.iter().enumerate() {
                stop.info()
                    .set_display_name(&format!("{organ} rank {}", n + 1));
            }
        }
    }

    const STOPS: usize = 60;

    static ORGAN_PARAMS: Mutex<Option<Arc<Stops>>> = Mutex::new(None);

    /// The organ tests share `ORGAN_PARAMS`, so they take turns.
    static ORGAN_ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn organ_params() -> Arc<Stops> {
        ORGAN_PARAMS.lock().unwrap().clone().unwrap()
    }

    struct Organ {
        params: Arc<Stops>,
    }

    impl Plugin for Organ {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.organ",
            name: "Organ",
            vendor: "Manguebyte",
            version: "0.1.0",
            description: "Renames its stops",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
        type Params = Stops;

        fn new() -> Self {
            let params = Arc::new(Stops(
                (1..=STOPS)
                    .map(|n| BoolParam::new(&format!("stop-{n:02}"), &format!("Stop {n}"), false))
                    .collect(),
            ));
            *ORGAN_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
            Self { params }
        }

        fn params(&self) -> Arc<Stops> {
            Arc::clone(&self.params)
        }

        fn prepare(&mut self, _: &ProcessSpec) {}

        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    }

    fn organ_host() -> (std::sync::MutexGuard<'static, ()>, Host, Arc<Recorder>) {
        let guard = ORGAN_ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (host, recorder) = Host::recording(plugin_entry::<Organ>(), Organ::INFO.id);
        (guard, host, recorder)
    }

    fn names(host: &Host) -> Vec<String> {
        (0..STOPS as u32)
            .map(|index| name(&host.param_info(index).unwrap().name))
            .collect()
    }

    /// Sixty stops renamed on another thread while blocks run: no block
    /// uses the heap, the audio side asks for a call on the main thread,
    /// and the host is asked once to read the parameters' information
    /// again, after which `get_info` gives the new names. A second organ
    /// loaded between two blocks is one request and one rescan more.
    #[test]
    fn a_batch_of_renames_is_one_info_rescan_and_get_info_gives_the_new_names() {
        let (_turn, host, recorder) = organ_host();
        host.activate(48_000.0, 64);
        assert_eq!(recorder.take_rescans(), Vec::<u32>::new());
        assert_eq!(name(&host.param_info(0).unwrap().name), "Stop 1");

        let stops = organ_params();
        let organist = {
            let stops = Arc::clone(&stops);
            std::thread::spawn(move || stops.load_organ("Haarlem"))
        };
        let mut blocks = 0;
        loop {
            let done = organist.is_finished();
            let processed = host.process(&mut stereo(0.5, 64), None, vec![], None);
            assert_eq!(
                processed.heap_calls, 0,
                "a block during renames used the heap"
            );
            blocks += 1;
            if done {
                break;
            }
        }
        organist.join().unwrap();
        assert!(blocks > 0);
        assert!(
            recorder.callbacks() >= 1,
            "no call on the main thread was asked for"
        );
        // Nothing is told to the host from the audio thread.
        assert_eq!(recorder.take_rescans(), Vec::<u32>::new());

        host.on_main_thread();
        assert_eq!(recorder.take_rescans(), [CLAP_PARAM_RESCAN_INFO]);
        host.on_main_thread();
        assert_eq!(recorder.take_rescans(), Vec::<u32>::new());
        let expected: Vec<String> = (1..=STOPS).map(|n| format!("Haarlem rank {n}")).collect();
        assert_eq!(names(&host), expected);

        // The next organ, loaded whole before the next block.
        let asked = recorder.callbacks();
        let other = Arc::clone(&stops);
        std::thread::spawn(move || other.load_organ("Alkmaar"))
            .join()
            .unwrap();
        for _ in 0..3 {
            let processed = host.process(&mut stereo(0.5, 64), None, vec![], None);
            assert_eq!(processed.heap_calls, 0);
        }
        assert_eq!(recorder.callbacks(), asked + 1, "one request per batch");
        host.on_main_thread();
        assert_eq!(recorder.take_rescans(), [CLAP_PARAM_RESCAN_INFO]);
        assert_eq!(name(&host.param_info(59).unwrap().name), "Alkmaar rank 60");
        // The id never changed, so automation and state still find it.
        assert_eq!(
            host.param_info(59).unwrap().id,
            ParamId::from_name("stop-60").0
        );
    }

    /// Renames made while nothing runs reach the host when it activates the
    /// plugin, and renames made as state loads are read with the values.
    #[test]
    fn renames_reach_the_host_at_activation_and_with_a_state_load() {
        let (_turn, host, recorder) = organ_host();
        let stops = organ_params();
        stops.load_organ("Freiberg");
        host.activate(48_000.0, 64);
        assert_eq!(recorder.take_rescans(), [CLAP_PARAM_RESCAN_INFO]);
        assert_eq!(name(&host.param_info(0).unwrap().name), "Freiberg rank 1");

        let saved = host.save().unwrap();
        assert!(host.load(&saved));
        assert_eq!(recorder.take_rescans(), [CLAP_PARAM_RESCAN_VALUES]);
        stops.load_organ("Weingarten");
        assert!(host.load(&saved));
        assert_eq!(
            recorder.take_rescans(),
            [CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_INFO]
        );
        // Already told: the audio side does not ask again.
        let asked = recorder.callbacks();
        host.process(&mut stereo(0.5, 64), None, vec![], None);
        assert_eq!(recorder.callbacks(), asked);
    }
}
