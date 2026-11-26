//! Drives the CLAP extensions beyond the basic ones through the C
//! interface, the way a host does: per-note automation and modulation and
//! their flags, `voice-info`, `note-name`, `audio-ports-config`,
//! `audio-ports-activation`, `render`, 64-bit audio and transport events
//! inside a block.
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
use std::sync::{Arc, Mutex, MutexGuard};

use manguebyte_clap::clap_sys::audio_buffer::clap_audio_buffer;
use manguebyte_clap::clap_sys::entry::clap_plugin_entry;
use manguebyte_clap::clap_sys::events::*;
use manguebyte_clap::clap_sys::ext::audio_ports::*;
use manguebyte_clap::clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, clap_plugin_audio_ports_activation,
};
use manguebyte_clap::clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_audio_ports_config, clap_plugin_audio_ports_config,
};
use manguebyte_clap::clap_sys::ext::note_name::{
    CLAP_EXT_NOTE_NAME, clap_note_name, clap_plugin_note_name,
};
use manguebyte_clap::clap_sys::ext::params::*;
use manguebyte_clap::clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
};
use manguebyte_clap::clap_sys::ext::voice_info::{
    CLAP_EXT_VOICE_INFO, CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES, clap_host_voice_info,
    clap_plugin_voice_info, clap_voice_info,
};
use manguebyte_clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte_clap::clap_sys::host::clap_host;
use manguebyte_clap::clap_sys::plugin::clap_plugin;
use manguebyte_clap::clap_sys::process::*;
use manguebyte_clap::clap_sys::version::CLAP_VERSION;
use manguebyte_clap::plugin_entry;
use manguebyte_core::governor::{Governor, GovernorConfig};
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_dsp::instruments::{OrganParams, OrganVoice};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Voice};
use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::output::OutputBuffer;
use manguebyte_plugin::{AudioLayout, NoteName, Plugin, PluginInfo, PluginKind, VoiceInfo};

// --- Watching the heap ---------------------------------------------------

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

/// The plugins share globals with the tests, so tests run one at a time.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn one_at_a_time() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

// --- An instrument with a per-note parameter ------------------------------

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const LEVEL: ParamId = ParamId::from_name("level");

struct PolyParams {
    cutoff: FloatParam,
    level: FloatParam,
}
param_set!(PolyParams { cutoff, level });

/// The parameters of the most recent `Poly`, for working out values.
static POLY_PARAMS: Mutex<Option<Arc<PolyParams>>> = Mutex::new(None);
/// How many voices `Poly` says it plays.
static VOICES: AtomicU32 = AtomicU32::new(4);
/// How many times the host was told the voices changed.
static VOICE_CHANGES: AtomicU32 = AtomicU32::new(0);

/// Outputs its cutoff, its own value for its note or the parameter's,
/// plus its modulation, while held.
struct CutoffVoice {
    held: bool,
    cutoff: f64,
    own: Option<f64>,
    modulation: f64,
}

impl Voice for CutoffVoice {
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn note_on(&mut self, _: NoteRef, _: f32) {
        self.held = true;
    }
    fn note_off(&mut self, _: f32) {
        self.held = false;
    }
    fn choke(&mut self) {
        self.held = false;
        self.own = None;
        self.modulation = 0.0;
    }
    fn is_active(&self) -> bool {
        self.held
    }
    fn render(&mut self, buffer: &mut AudioBuffer<f32>) {
        if !self.held {
            return;
        }
        let sample = (self.own.unwrap_or(self.cutoff) + self.modulation) as f32;
        for channel in buffer.iter_channels_mut() {
            channel.iter_mut().for_each(|out| *out += sample);
        }
    }
    fn set_param(&mut self, param: ParamId, value: f64) {
        if param == CUTOFF {
            self.cutoff = value;
            self.own = None;
        }
    }
    fn set_note_param(&mut self, param: ParamId, value: f64) {
        if param == CUTOFF {
            self.own = Some(value);
        }
    }
    fn modulate(&mut self, param: ParamId, amount: f64) {
        if param == CUTOFF {
            self.modulation = amount;
        }
    }
}

struct Poly {
    synth: Synth<CutoffVoice>,
    params: Arc<PolyParams>,
}

impl Plugin for Poly {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.poly",
        name: "Poly",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays its cutoff",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_INSTRUMENT;
    const NOTE_NAMES: &'static [NoteName] = &[
        NoteName {
            name: "Kick",
            key: 36,
            channel: None,
        },
        NoteName {
            name: "Snare",
            key: 38,
            channel: Some(9),
        },
    ];
    type Params = PolyParams;

    fn new() -> Self {
        let params = Arc::new(PolyParams {
            cutoff: FloatParam::new(
                "cutoff",
                "Cutoff",
                FloatRange::logarithmic(20.0, 20_000.0),
                1_000.0,
            )
            .per_note(),
            level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 1.0), 1.0),
        });
        *POLY_PARAMS.lock().unwrap() = Some(Arc::clone(&params));
        Self {
            synth: Synth::with_voices(8, || CutoffVoice {
                held: false,
                cutoff: 1_000.0,
                own: None,
                modulation: 0.0,
            }),
            params,
        }
    }
    fn params(&self) -> Arc<PolyParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.synth.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.synth.process_with(buffer, context);
    }
    fn reset(&mut self) {
        self.synth.reset();
    }
    fn voice_info(&self) -> Option<VoiceInfo> {
        Some(VoiceInfo {
            voices: VOICES.load(Ordering::Relaxed),
            capacity: 8,
            overlapping_notes: true,
        })
    }
}

/// What the cutoff holds at `normalized`, as the plugin works it out.
/// That a parameter's value read back is the one read before. The host
/// reads it through the parameter's skewed range, so the same stored value
/// gives the same bits; Miri adds noise to `ln` and `exp` on purpose, so
/// under Miri the two reads may differ in the last bits.
#[track_caller]
fn assert_unmoved(now: f64, before: f64) {
    if cfg!(miri) {
        assert!(
            (now - before).abs() <= 1e-12 * before.abs().max(1.0),
            "{now} != {before}"
        );
    } else {
        assert_eq!(now, before);
    }
}

fn cutoff_at(normalized: f64) -> f64 {
    let params = POLY_PARAMS.lock().unwrap().clone().unwrap();
    params.cutoff.normalized_to_plain(normalized)
}

// --- An effect with several layouts ---------------------------------------

struct ShapeParams {
    level: FloatParam,
}
param_set!(ShapeParams { level });

/// The layouts `Shape` has been given, in order.
static SHAPE_LAYOUTS: Mutex<Vec<AudioLayout>> = Mutex::new(Vec::new());

/// Scales its input by `level`, and adds 1000 when it has a sidechain.
struct Shape {
    params: Arc<ShapeParams>,
}

impl Plugin for Shape {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.shape",
        name: "Shape",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Runs in several shapes",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT.with_sidechain(2);
    const LAYOUTS: &'static [AudioLayout] = &[
        AudioLayout::STEREO_EFFECT.with_sidechain(2),
        AudioLayout::STEREO_EFFECT,
        AudioLayout::MONO_EFFECT.with_sidechain(1),
        AudioLayout::MONO_EFFECT,
    ];
    type Params = ShapeParams;

    fn new() -> Self {
        Self {
            params: Arc::new(ShapeParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.5),
            }),
        }
    }
    fn params(&self) -> Arc<ShapeParams> {
        Arc::clone(&self.params)
    }
    fn set_layout(&mut self, layout: AudioLayout) {
        SHAPE_LAYOUTS.lock().unwrap().push(layout);
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        let keyed = if context.sidechain.is_some() {
            1_000.0
        } else {
            0.0
        };
        let level = self.params.level.value();
        for channel in buffer.iter_channels_mut() {
            channel
                .iter_mut()
                .for_each(|sample| *sample = *sample * level + keyed);
        }
    }
}

// --- An instrument that plays the tempo ------------------------------------

/// Writes the tempo into every frame, and 1000 more on the frame of each
/// note-on, which it also sends out.
struct Clock {
    params: Arc<ShapeParams>,
}

impl Plugin for Clock {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.clock",
        name: "Clock",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays the tempo",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_INSTRUMENT;
    const NOTE_OUTPUT: bool = true;
    type Params = ShapeParams;

    fn new() -> Self {
        Self {
            params: Arc::new(ShapeParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.0),
            }),
        }
    }
    fn params(&self) -> Arc<ShapeParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.process_with_output(buffer, context, &mut OutputBuffer::new());
    }
    fn process_with_output(
        &mut self,
        buffer: &mut AudioBuffer<f32>,
        context: &ProcessContext<'_, f32>,
        output: &mut OutputBuffer,
    ) {
        let tempo = context.transport.tempo_bpm.unwrap_or(0.0) as f32;
        buffer.channel_mut(0).fill(tempo);
        for event in context.events {
            if let EventKind::NoteOn { note, velocity } = event.kind {
                buffer.channel_mut(0)[event.offset as usize] += 1_000.0;
                output.note_on(event.offset, note, velocity);
            }
        }
    }
}

// --- A minimal host ------------------------------------------------------

unsafe extern "C" fn host_voices_changed(_: *const clap_host) {
    VOICE_CHANGES.fetch_add(1, Ordering::Relaxed);
}

static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(host_voices_changed),
};

unsafe extern "C" fn host_get_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_EXT_VOICE_INFO {
        ptr::from_ref(&HOST_VOICE_INFO).cast()
    } else {
        ptr::null()
    }
}
unsafe extern "C" fn host_request(_: *const clap_host) {}

/// One event the host sends, as its concrete CLAP struct.
#[derive(Clone, Copy)]
enum Sent {
    Note(clap_event_note),
    Value(clap_event_param_value),
    Modulation(clap_event_param_mod),
    Transport(clap_event_transport),
}

impl Sent {
    fn header(&self) -> *const clap_event_header {
        match self {
            Self::Note(event) => ptr::from_ref(event).cast(),
            Self::Value(event) => ptr::from_ref(event).cast(),
            Self::Modulation(event) => ptr::from_ref(event).cast(),
            Self::Transport(event) => ptr::from_ref(event).cast(),
        }
    }

    /// The same event on another frame.
    fn at(mut self, time: u32) -> Self {
        match &mut self {
            Self::Note(event) => event.header.time = time,
            Self::Value(event) => event.header.time = time,
            Self::Modulation(event) => event.header.time = time,
            Self::Transport(event) => event.header.time = time,
        }
        self
    }

    fn time(&self) -> u32 {
        unsafe { (*self.header()).time }
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

fn note_on(time: u32, key: i16, note_id: i32) -> Sent {
    Sent::Note(clap_event_note {
        header: header::<clap_event_note>(time, CLAP_EVENT_NOTE_ON),
        note_id,
        port_index: 0,
        channel: 0,
        key,
        velocity: 1.0,
    })
}

/// A parameter value, for every note or, with a note id or a key, for one.
fn value(time: u32, param: ParamId, note_id: i32, key: i16, value: f64) -> Sent {
    Sent::Value(clap_event_param_value {
        header: header::<clap_event_param_value>(time, CLAP_EVENT_PARAM_VALUE),
        param_id: param.0,
        cookie: ptr::null_mut(),
        note_id,
        port_index: if note_id >= 0 || key >= 0 { 0 } else { -1 },
        channel: if key >= 0 { 0 } else { -1 },
        key,
        value,
    })
}

fn modulation(time: u32, param: ParamId, note_id: i32, amount: f64) -> Sent {
    Sent::Modulation(clap_event_param_mod {
        header: header::<clap_event_param_mod>(time, CLAP_EVENT_PARAM_MOD),
        param_id: param.0,
        cookie: ptr::null_mut(),
        note_id,
        port_index: -1,
        channel: -1,
        key: -1,
        amount,
    })
}

fn transport(time: u32, tempo: f64) -> clap_event_transport {
    clap_event_transport {
        header: header::<clap_event_transport>(time, CLAP_EVENT_TRANSPORT),
        flags: CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_IS_PLAYING,
        song_pos_beats: 0,
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
    }
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

/// Collects the notes the plugin sends, as `(type, time, key)`.
unsafe extern "C" fn events_push(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        let type_ = (*event).type_;
        if type_ == CLAP_EVENT_NOTE_ON || type_ == CLAP_EVENT_NOTE_OFF {
            let note = &*event.cast::<clap_event_note>();
            (*(*list).ctx.cast::<Vec<(u16, u32, i16)>>()).push((type_, note.header.time, note.key));
        }
    }
    true
}

/// A port's memory, in one sample size or the other.
#[derive(Clone, Debug, PartialEq)]
enum Samples {
    F32(Vec<Vec<f32>>),
    F64(Vec<Vec<f64>>),
}

impl Samples {
    fn frames(&self) -> usize {
        match self {
            Self::F32(channels) => channels[0].len(),
            Self::F64(channels) => channels[0].len(),
        }
    }

    fn f32(&self) -> &Vec<Vec<f32>> {
        match self {
            Self::F32(channels) => channels,
            Self::F64(_) => panic!("not 32-bit"),
        }
    }
}

/// The pointers of a port, kept alive while the plugin reads them.
struct Pointers {
    f32: Vec<*mut f32>,
    f64: Vec<*mut f64>,
}

impl Pointers {
    fn of(samples: &mut Samples) -> Self {
        match samples {
            Samples::F32(channels) => Self {
                f32: channels.iter_mut().map(|c| c.as_mut_ptr()).collect(),
                f64: Vec::new(),
            },
            Samples::F64(channels) => Self {
                f32: Vec::new(),
                f64: channels.iter_mut().map(|c| c.as_mut_ptr()).collect(),
            },
        }
    }

    fn port(&mut self) -> clap_audio_buffer {
        let (data32, data64, count) = if self.f64.is_empty() {
            (self.f32.as_mut_ptr(), ptr::null_mut(), self.f32.len())
        } else {
            (ptr::null_mut(), self.f64.as_mut_ptr(), self.f64.len())
        };
        clap_audio_buffer {
            data32,
            data64,
            channel_count: count as u32,
            latency: 0,
            constant_mask: 0,
        }
    }
}

/// What one call to `process` produced.
struct Run {
    status: clap_process_status,
    /// The notes the plugin sent, as `(type, time, key)`.
    sent: Vec<(u16, u32, i16)>,
    heap_calls: usize,
}

struct Host {
    entry: clap_plugin_entry,
    plugin: *const clap_plugin,
    host: *mut clap_host,
    active: Cell<bool>,
}

impl Host {
    fn new<P: Plugin>() -> Self {
        let entry = plugin_entry::<P>();
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: ptr::null_mut(),
            name: c"test host".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_request),
            request_process: Some(host_request),
            request_callback: Some(host_request),
        }));
        let id = CString::new(P::INFO.id).unwrap();
        unsafe {
            assert!(entry.init.unwrap()(c"/plugins/test.clap".as_ptr()));
            let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>();
            let plugin = (*factory).create_plugin.unwrap()(factory, host, id.as_ptr());
            assert!(!plugin.is_null());
            assert!((*plugin).init.unwrap()(plugin));
            Self {
                entry,
                plugin,
                host,
                active: Cell::new(false),
            }
        }
    }

    fn extension<T>(&self, id: &CStr) -> Option<&T> {
        unsafe {
            (*self.plugin).get_extension.unwrap()(self.plugin, id.as_ptr())
                .cast::<T>()
                .as_ref()
        }
    }

    fn activate(&self, max_frames: u32) {
        unsafe {
            assert!((*self.plugin).activate.unwrap()(
                self.plugin,
                48_000.0,
                1,
                max_frames
            ));
            assert!((*self.plugin).start_processing.unwrap()(self.plugin));
        }
        self.active.set(true);
    }

    fn deactivate(&self) {
        unsafe {
            (*self.plugin).stop_processing.unwrap()(self.plugin);
            (*self.plugin).deactivate.unwrap()(self.plugin);
        }
        self.active.set(false);
    }

    fn params(&self) -> &clap_plugin_params {
        self.extension(CLAP_EXT_PARAMS).unwrap()
    }

    fn param_flags(&self, index: u32) -> u32 {
        unsafe {
            let mut info: clap_param_info = std::mem::zeroed();
            assert!(self.params().get_info.unwrap()(
                self.plugin,
                index,
                &raw mut info
            ));
            info.flags
        }
    }

    fn param_value(&self, id: ParamId) -> f64 {
        let mut value = f64::NAN;
        unsafe {
            assert!(self.params().get_value.unwrap()(
                self.plugin,
                id.0,
                &raw mut value
            ))
        };
        value
    }

    fn flush(&self, events: Vec<Sent>) {
        let mut events = events;
        let input = clap_input_events {
            ctx: (&raw mut events).cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let mut sent: Vec<(u16, u32, i16)> = Vec::new();
        let output = clap_output_events {
            ctx: (&raw mut sent).cast(),
            try_push: Some(events_push),
        };
        unsafe { self.params().flush.unwrap()(self.plugin, &raw const input, &raw const output) };
    }

    fn port_count(&self, is_input: bool) -> u32 {
        let ports: &clap_plugin_audio_ports = self.extension(CLAP_EXT_AUDIO_PORTS).unwrap();
        unsafe { ports.count.unwrap()(self.plugin, is_input) }
    }

    fn port(&self, index: u32, is_input: bool) -> Option<clap_audio_port_info> {
        let ports: &clap_plugin_audio_ports = self.extension(CLAP_EXT_AUDIO_PORTS).unwrap();
        unsafe {
            let mut info: clap_audio_port_info = std::mem::zeroed();
            ports.get.unwrap()(self.plugin, index, is_input, &raw mut info).then_some(info)
        }
    }

    /// Processes one block: `inputs` and `outputs` are the ports, in the
    /// order the plugin declares them, and `tempo` the block's transport.
    fn process(
        &self,
        inputs: &mut [Samples],
        outputs: &mut [Samples],
        events: &[Sent],
        tempo: Option<f64>,
    ) -> Run {
        let frames = outputs[0].frames();
        let mut input_pointers: Vec<Pointers> = inputs.iter_mut().map(Pointers::of).collect();
        let mut output_pointers: Vec<Pointers> = outputs.iter_mut().map(Pointers::of).collect();
        let input_ports: Vec<clap_audio_buffer> =
            input_pointers.iter_mut().map(Pointers::port).collect();
        let mut output_ports: Vec<clap_audio_buffer> =
            output_pointers.iter_mut().map(Pointers::port).collect();

        let mut events = events.to_vec();
        let input_events = clap_input_events {
            ctx: (&raw mut events).cast(),
            size: Some(events_size),
            get: Some(events_get),
        };
        let mut sent: Vec<(u16, u32, i16)> = Vec::with_capacity(64);
        let output_events = clap_output_events {
            ctx: (&raw mut sent).cast(),
            try_push: Some(events_push),
        };
        let transport = tempo.map(|tempo| transport(0, tempo));
        let process = clap_process {
            steady_time: -1,
            frames_count: frames as u32,
            transport: transport.as_ref().map_or(ptr::null(), ptr::from_ref),
            audio_inputs: input_ports.as_ptr(),
            audio_outputs: output_ports.as_mut_ptr(),
            audio_inputs_count: input_ports.len() as u32,
            audio_outputs_count: output_ports.len() as u32,
            in_events: &raw const input_events,
            out_events: &raw const output_events,
        };
        HEAP_CALLS.with(|calls| calls.set(0));
        IN_PROCESS.with(|watching| watching.set(true));
        let status = unsafe { (*self.plugin).process.unwrap()(self.plugin, &raw const process) };
        IN_PROCESS.with(|watching| watching.set(false));
        Run {
            status,
            sent,
            heap_calls: HEAP_CALLS.with(Cell::get),
        }
    }

    /// Plays a mono instrument over `frames` frames with `events` placed on
    /// the timeline, in blocks cut as `pattern` says, and returns what it
    /// played. Every block is checked to have used no heap.
    fn play(&self, frames: usize, pattern: &[usize], events: &[Sent]) -> Vec<f32> {
        let mut played = Vec::with_capacity(frames);
        let mut start = 0;
        let mut next = 0;
        while start < frames {
            let length = pattern[next % pattern.len()].min(frames - start);
            next += 1;
            let end = start + length;
            let block: Vec<Sent> = events
                .iter()
                .filter(|event| (start..end).contains(&(event.time() as usize)))
                .map(|event| event.at(event.time() - start as u32))
                .collect();
            let mut outputs = [Samples::F32(vec![vec![0.0; length]])];
            let run = self.process(&mut [], &mut outputs, &block, None);
            assert_eq!(run.status, CLAP_PROCESS_CONTINUE);
            assert_eq!(run.heap_calls, 0, "the plugin used the heap in process");
            played.extend_from_slice(&outputs[0].f32()[0]);
            start = end;
        }
        played
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        unsafe {
            if self.active.get() {
                self.deactivate();
            }
            (*self.plugin).destroy.unwrap()(self.plugin);
            self.entry.deinit.unwrap()();
            drop(Box::from_raw(self.host));
        }
    }
}

fn name(field: &[c_char]) -> String {
    unsafe { CStr::from_ptr(field.as_ptr()).to_str().unwrap().to_owned() }
}

// --- Per-note automation and modulation ------------------------------------

#[test]
fn a_per_note_parameter_says_so_and_the_others_do_not() {
    let _guard = one_at_a_time();
    let host = Host::new::<Poly>();
    let per_note = CLAP_PARAM_IS_AUTOMATABLE_PER_NOTE_ID
        | CLAP_PARAM_IS_AUTOMATABLE_PER_KEY
        | CLAP_PARAM_IS_MODULATABLE_PER_NOTE_ID
        | CLAP_PARAM_IS_MODULATABLE_PER_KEY;
    let cutoff = host.param_flags(0);
    assert_eq!(cutoff & per_note, per_note);
    // Not per channel or per port.
    assert_eq!(
        cutoff & (CLAP_PARAM_IS_AUTOMATABLE_PER_CHANNEL | CLAP_PARAM_IS_MODULATABLE_PER_PORT),
        0
    );
    assert_eq!(host.param_flags(1) & per_note, 0);
}

#[test]
fn per_note_automation_lands_on_its_note_and_frame() {
    let _guard = one_at_a_time();
    let host = Host::new::<Poly>();
    host.activate(64);
    let before = host.param_value(CUTOFF);
    let own = cutoff_at(0.5);

    // Two notes; at frame 10 the first, by its id, takes its own cutoff.
    let mut outputs = [Samples::F32(vec![vec![0.0; 32]])];
    let events = [
        note_on(0, 60, 1),
        note_on(0, 64, 2),
        value(10, CUTOFF, 1, -1, 0.5),
    ];
    let run = host.process(&mut [], &mut outputs, &events, None);
    assert_eq!((run.status, run.heap_calls), (CLAP_PROCESS_CONTINUE, 0));
    let played = &outputs[0].f32()[0];
    assert!(played[..10].iter().all(|&sample| sample == 2_000.0));
    let expected = (own as f32) + 1_000.0;
    assert!(
        played[10..].iter().all(|&sample| sample == expected),
        "{played:?}"
    );
    // The parameter itself did not move.
    assert_unmoved(host.param_value(CUTOFF), before);

    // By key: the second note, on frame 3.
    let mut outputs = [Samples::F32(vec![vec![0.0; 8]])];
    let events = [value(3, CUTOFF, -1, 64, 0.25)];
    let run = host.process(&mut [], &mut outputs, &events, None);
    assert_eq!(run.heap_calls, 0);
    let played = &outputs[0].f32()[0];
    let second = cutoff_at(0.25) as f32;
    assert_eq!(played[2], expected);
    assert_eq!(played[3], own as f32 + second);

    // A value for every note puts both back on the parameter.
    let mut outputs = [Samples::F32(vec![vec![0.0; 8]])];
    let events = [value(4, CUTOFF, -1, -1, 0.75)];
    let run = host.process(&mut [], &mut outputs, &events, None);
    assert_eq!(run.heap_calls, 0);
    let both = 2.0 * cutoff_at(0.75) as f32;
    assert_eq!(outputs[0].f32()[0][4..], [both; 4]);
}

#[test]
fn per_note_automation_for_a_parameter_without_it_is_ignored() {
    let _guard = one_at_a_time();
    let host = Host::new::<Poly>();
    host.activate(64);
    let level = host.param_value(LEVEL);
    let mut outputs = [Samples::F32(vec![vec![0.0; 8]])];
    let events = [note_on(0, 60, 1), value(2, LEVEL, 1, -1, 0.1)];
    let run = host.process(&mut [], &mut outputs, &events, None);
    assert_eq!(run.status, CLAP_PROCESS_CONTINUE);
    assert_eq!(host.param_value(LEVEL), level);
}

#[test]
fn a_flush_does_not_store_a_value_for_one_note() {
    let _guard = one_at_a_time();
    let host = Host::new::<Poly>();
    let before = host.param_value(CUTOFF);
    host.flush(vec![value(0, CUTOFF, 3, -1, 0.9)]);
    assert_unmoved(host.param_value(CUTOFF), before);
    // A value for every note still is.
    host.flush(vec![value(0, CUTOFF, -1, -1, 0.9)]);
    assert!((host.param_value(CUTOFF) - 0.9).abs() < 1e-6);
}

#[test]
fn per_note_modulation_does_not_depend_on_where_blocks_are_cut() {
    let _guard = one_at_a_time();
    // A logarithmic parameter automated and modulated in the same stretch:
    // the modulation's amount is converted around the value automation
    // gave it on its frame, wherever the blocks are cut.
    let timeline = [
        note_on(0, 60, 1),
        note_on(0, 67, 2),
        value(8, CUTOFF, -1, -1, 0.25),
        modulation(12, CUTOFF, 1, 0.1),
        value(20, CUTOFF, -1, -1, 0.75),
        modulation(30, CUTOFF, 2, -0.2),
        modulation(31, CUTOFF, -1, 0.05),
    ];
    let render = |pattern: &[usize]| {
        let host = Host::new::<Poly>();
        host.activate(64);
        host.play(48, pattern, &timeline)
    };
    let whole = render(&[48]);
    // The modulation at 12 moved one note a tenth of the way up from
    // where automation had put the parameter at frame 8.
    let base = cutoff_at(0.25);
    let moved = cutoff_at(0.35);
    let expected = (base + (moved - base)) as f32 + base as f32;
    assert!((whole[12] - expected).abs() <= expected * 1e-6, "{whole:?}");
    assert_ne!(whole[11], whole[12]);
    for pattern in [&[8, 8, 32][..], &[5, 7, 11, 25], &[1], &[3, 13]] {
        assert_eq!(render(pattern), whole, "cut as {pattern:?}");
    }
}

// --- Voices and note names ---------------------------------------------------

#[test]
fn voice_info_is_reported_and_its_changes_announced() {
    let _guard = one_at_a_time();
    VOICES.store(4, Ordering::Relaxed);
    VOICE_CHANGES.store(0, Ordering::Relaxed);
    let host = Host::new::<Poly>();
    let voice_info: &clap_plugin_voice_info = host.extension(CLAP_EXT_VOICE_INFO).unwrap();
    let get = || unsafe {
        let mut info: clap_voice_info = std::mem::zeroed();
        assert!(voice_info.get.unwrap()(host.plugin, &raw mut info));
        (info.voice_count, info.voice_capacity, info.flags)
    };
    assert_eq!(get(), (4, 8, CLAP_VOICE_INFO_SUPPORTS_OVERLAPPING_NOTES));

    // The same on activation: nothing to announce.
    host.activate(64);
    assert_eq!(VOICE_CHANGES.load(Ordering::Relaxed), 0);
    host.deactivate();
    VOICES.store(6, Ordering::Relaxed);
    host.activate(64);
    assert_eq!(VOICE_CHANGES.load(Ordering::Relaxed), 1);
    assert_eq!(get().0, 6);
    VOICES.store(4, Ordering::Relaxed);

    // A plugin that says nothing of its voices offers no `voice-info`.
    let effect = Host::new::<Shape>();
    assert!(
        effect
            .extension::<clap_plugin_voice_info>(CLAP_EXT_VOICE_INFO)
            .is_none()
    );
}

#[test]
fn note_names_are_listed() {
    let _guard = one_at_a_time();
    let host = Host::new::<Poly>();
    let names: &clap_plugin_note_name = host.extension(CLAP_EXT_NOTE_NAME).unwrap();
    unsafe {
        assert_eq!(names.count.unwrap()(host.plugin), 2);
        let mut name_of: clap_note_name = std::mem::zeroed();
        assert!(names.get.unwrap()(host.plugin, 0, &raw mut name_of));
        assert_eq!(
            (name(&name_of.name), name_of.key, name_of.channel),
            ("Kick".to_owned(), 36, -1)
        );
        assert!(names.get.unwrap()(host.plugin, 1, &raw mut name_of));
        assert_eq!(
            (name(&name_of.name), name_of.key, name_of.channel),
            ("Snare".to_owned(), 38, 9)
        );
        assert!(!names.get.unwrap()(host.plugin, 2, &raw mut name_of));
    }
    let effect = Host::new::<Shape>();
    assert!(
        effect
            .extension::<clap_plugin_note_name>(CLAP_EXT_NOTE_NAME)
            .is_none()
    );
}

// --- Port configurations and activation ---------------------------------------

fn stereo(value: f32, frames: usize) -> Samples {
    Samples::F32(vec![vec![value; frames]; 2])
}

#[test]
fn port_configurations_are_the_layouts_offered() {
    let _guard = one_at_a_time();
    SHAPE_LAYOUTS.lock().unwrap().clear();
    let host = Host::new::<Shape>();
    let configs: &clap_plugin_audio_ports_config =
        host.extension(CLAP_EXT_AUDIO_PORTS_CONFIG).unwrap();
    let config = |index| unsafe {
        let mut config: clap_audio_ports_config = std::mem::zeroed();
        configs.get.unwrap()(host.plugin, index, &raw mut config).then_some(config)
    };
    unsafe { assert_eq!(configs.count.unwrap()(host.plugin), 4) };
    let names: Vec<String> = (0..4)
        .map(|index| name(&config(index).unwrap().name))
        .collect();
    assert_eq!(
        names,
        [
            "Stereo, stereo sidechain",
            "Stereo",
            "Mono, mono sidechain",
            "Mono"
        ]
    );
    let mono = config(2).unwrap();
    assert_eq!(
        (
            mono.id,
            mono.input_port_count,
            mono.output_port_count,
            mono.main_input_channel_count
        ),
        (2, 2, 1, 1)
    );
    assert!(mono.has_main_input && mono.has_main_output);
    assert!(config(4).is_none());

    // Not while active.
    host.activate(64);
    unsafe { assert!(!configs.select.unwrap()(host.plugin, 3)) };
    host.deactivate();
    unsafe { assert!(configs.select.unwrap()(host.plugin, 3)) };
    assert_eq!((host.port_count(true), host.port_count(false)), (1, 1));
    assert_eq!(host.port(0, true).unwrap().channel_count, 1);
    host.activate(64);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::MONO_EFFECT)
    );
    let mut inputs = [Samples::F32(vec![vec![0.5; 16]])];
    let mut outputs = [Samples::F32(vec![vec![0.0; 16]])];
    let run = host.process(&mut inputs, &mut outputs, &[], None);
    assert_eq!((run.status, run.heap_calls), (CLAP_PROCESS_CONTINUE, 0));
    assert_eq!(outputs[0].f32()[0], [0.75; 16]);

    // A plugin with one layout has no configurations to offer.
    let poly = Host::new::<Poly>();
    assert!(
        poly.extension::<clap_plugin_audio_ports_config>(CLAP_EXT_AUDIO_PORTS_CONFIG)
            .is_none()
    );
}

#[test]
fn switching_the_sidechain_off_picks_the_layout_without_it() {
    let _guard = one_at_a_time();
    SHAPE_LAYOUTS.lock().unwrap().clear();
    let host = Host::new::<Shape>();
    let activation: &clap_plugin_audio_ports_activation =
        host.extension(CLAP_EXT_AUDIO_PORTS_ACTIVATION).unwrap();
    let set = |is_input, index, active| unsafe {
        activation.set_active.unwrap()(host.plugin, is_input, index, active, 32)
    };
    unsafe {
        assert!(!activation.can_activate_while_processing.unwrap()(
            host.plugin
        ))
    };
    // The main ports stay on; there is no third input.
    assert!(!set(true, 0, false));
    assert!(!set(false, 0, false));
    assert!(!set(true, 2, false));
    assert!(set(true, 1, false));
    // The ports are still there, only switched off.
    assert_eq!(host.port_count(true), 2);

    host.activate(64);
    assert_eq!(
        SHAPE_LAYOUTS.lock().unwrap().last(),
        Some(&AudioLayout::STEREO_EFFECT)
    );
    assert!(!set(true, 1, true), "not while active");
    let mut inputs = [stereo(1.0, 8), stereo(0.0, 8)];
    let mut outputs = [stereo(0.0, 8)];
    let run = host.process(&mut inputs, &mut outputs, &[], None);
    assert_eq!(run.heap_calls, 0);
    assert_eq!(outputs[0], stereo(1.5, 8));

    host.deactivate();
    assert!(set(true, 1, true));
    host.activate(64);
    assert_eq!(SHAPE_LAYOUTS.lock().unwrap().last(), Some(&Shape::LAYOUT));
    let run = host.process(&mut inputs, &mut outputs, &[], None);
    assert_eq!(run.heap_calls, 0);
    assert_eq!(outputs[0], stereo(1_001.5, 8));

    // A plugin without a sidechain has nothing to switch.
    let poly = Host::new::<Poly>();
    assert!(
        poly.extension::<clap_plugin_audio_ports_activation>(CLAP_EXT_AUDIO_PORTS_ACTIVATION)
            .is_none()
    );
}

// --- 64-bit audio ----------------------------------------------------------------

/// Samples that a 32-bit float does not hold exactly.
fn signal(frames: usize) -> Vec<f64> {
    (0..frames)
        .map(|frame| (frame as f64 * 0.013).sin() * 0.7 + 1e-9)
        .collect()
}

/// Runs `Shape` over `frames` frames in blocks of `pattern`, with the main
/// input and output 64-bit when `wide`, and returns the output.
fn shape(frames: usize, pattern: &[usize], wide_in: bool, wide_out: bool) -> Vec<Vec<f64>> {
    let input = signal(frames);
    let host = Host::new::<Shape>();
    host.activate(128);
    let mut played = vec![Vec::new(), Vec::new()];
    let mut start = 0;
    let mut next = 0;
    while start < frames {
        let length = pattern[next % pattern.len()].min(frames - start);
        next += 1;
        let piece = &input[start..start + length];
        let main = if wide_in {
            Samples::F64(vec![piece.to_vec(); 2])
        } else {
            Samples::F32(vec![piece.iter().map(|&s| s as f32).collect(); 2])
        };
        let side = Samples::F64(vec![vec![0.0; length]; 2]);
        let mut inputs = [main, side];
        let mut outputs = [if wide_out {
            Samples::F64(vec![vec![0.0; length]; 2])
        } else {
            stereo(0.0, length)
        }];
        let run = host.process(&mut inputs, &mut outputs, &[], None);
        assert_eq!((run.status, run.heap_calls), (CLAP_PROCESS_CONTINUE, 0));
        for (channel, out) in played.iter_mut().enumerate() {
            match &outputs[0] {
                Samples::F64(channels) => out.extend_from_slice(&channels[channel]),
                Samples::F32(channels) => {
                    out.extend(channels[channel].iter().map(|&s| f64::from(s)))
                }
            }
        }
        start += length;
    }
    played
}

#[test]
fn sixty_four_bit_audio_is_the_32_bit_result_widened() {
    let _guard = one_at_a_time();
    let narrow = shape(100, &[100], false, false);
    // 64 bits in and out: the plugin hears the input rounded to 32 bits,
    // and its output is widened exactly.
    let wide = shape(100, &[100], true, true);
    assert_eq!(wide, narrow);
    assert!(
        wide[0]
            .iter()
            .all(|&sample| f64::from(sample as f32) == sample)
    );
    // Either side alone, and whatever the blocks.
    assert_eq!(shape(100, &[100], true, false), narrow);
    assert_eq!(shape(100, &[100], false, true), narrow);
    for pattern in [&[7, 25, 32][..], &[1], &[64, 36]] {
        assert_eq!(
            shape(100, pattern, true, true),
            narrow,
            "cut as {pattern:?}"
        );
    }
    let host = Host::new::<Shape>();
    for is_input in [true, false] {
        let port = host.port(0, is_input).unwrap();
        assert_ne!(port.flags & CLAP_AUDIO_PORT_SUPPORTS_64BITS, 0);
    }
}

// --- Transport events inside a block ------------------------------------------------

#[test]
fn a_transport_event_inside_a_block_takes_effect_on_its_frame() {
    let _guard = one_at_a_time();
    let host = Host::new::<Clock>();
    host.activate(64);
    let mut outputs = [Samples::F32(vec![vec![0.0; 32]])];
    let events = [
        Sent::Transport(transport(10, 90.0)),
        note_on(12, 60, 1),
        Sent::Transport(transport(20, 60.0)),
        note_on(20, 64, 2),
        note_on(31, 67, 3),
    ];
    let run = host.process(&mut [], &mut outputs, &events, Some(120.0));
    assert_eq!((run.status, run.heap_calls), (CLAP_PROCESS_CONTINUE, 0));
    let mut expected = vec![120.0; 10];
    expected.extend([90.0; 10]);
    expected.extend([60.0; 12]);
    expected[12] += 1_000.0;
    expected[20] += 1_000.0;
    expected[31] += 1_000.0;
    assert_eq!(outputs[0].f32()[0], expected);
    // The notes sent from each piece are on the block's frames.
    assert_eq!(
        run.sent,
        [
            (CLAP_EVENT_NOTE_ON, 12, 60),
            (CLAP_EVENT_NOTE_ON, 20, 64),
            (CLAP_EVENT_NOTE_ON, 31, 67)
        ]
    );

    // One on frame 0 replaces the block's transport.
    let mut outputs = [Samples::F32(vec![vec![0.0; 8]])];
    let events = [Sent::Transport(transport(0, 100.0))];
    let run = host.process(&mut [], &mut outputs, &events, Some(120.0));
    assert_eq!(run.heap_calls, 0);
    assert_eq!(outputs[0].f32()[0], [100.0; 8]);
}

// --- Offline render -------------------------------------------------------

/// An organ that renders within a governor whose maximum is nothing: live it
/// is silent, so anything it plays it was told to render offline. It does
/// nothing about offline renders itself.
struct Thrifty {
    organ: Synth<OrganVoice>,
    governor: Governor,
    params: Arc<ShapeParams>,
}

impl Plugin for Thrifty {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.thrifty",
        name: "Thrifty",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Plays only offline",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_INSTRUMENT;
    type Params = ShapeParams;

    fn new() -> Self {
        let mut governor = Governor::new(GovernorConfig::default());
        governor.set_max_budget(0.0);
        Self {
            organ: Synth::new(OrganVoice::voices(4, OrganParams::jazz())),
            governor,
            params: Arc::new(ShapeParams {
                level: FloatParam::new("level", "Level", FloatRange::linear(0.0, 2.0), 1.0),
            }),
        }
    }
    fn params(&self) -> Arc<ShapeParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.organ.prepare(spec);
    }
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        self.organ
            .process_governed(buffer, context, &mut self.governor);
    }
    fn reset(&mut self) {
        self.organ.reset();
    }
}

/// What the organ plays with every voice it is asked for.
fn whole_organ(frames: usize) -> Vec<f32> {
    let mut organ = Synth::new(OrganVoice::voices(4, OrganParams::jazz()));
    organ.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: frames,
        num_channels: 1,
    });
    organ.note_on(NoteRef::with_id(0, 48, 1), 1.0);
    organ.note_on(NoteRef::with_id(0, 55, 2), 1.0);
    let mut block = AudioBuffer::<f32>::new(1, frames);
    organ.process(&mut block);
    block.channel(0).to_vec()
}

#[test]
fn an_offline_render_plays_everything_whatever_the_budget() {
    let _guard = one_at_a_time();
    let host = Host::new::<Thrifty>();
    let render: &clap_plugin_render = host.extension(CLAP_EXT_RENDER).unwrap();
    let set = |mode| unsafe { render.set.unwrap()(host.plugin, mode) };
    assert!(!unsafe { render.has_hard_realtime_requirement.unwrap()(host.plugin) });
    host.activate(64);

    let notes = [note_on(0, 48, 1), note_on(0, 55, 2)];
    // Live, which is what a plugin starts as: not a sound.
    assert_eq!(host.play(256, &[64], &notes), [0.0; 256]);

    unsafe { (*host.plugin).reset.unwrap()(host.plugin) };
    assert!(set(CLAP_RENDER_OFFLINE));
    // In blocks of any size, and in the pieces a transport event cuts one
    // into.
    let expected = whole_organ(256);
    assert!(expected.iter().any(|&sample| sample != 0.0));
    assert_eq!(host.play(256, &[64, 7, 1], &notes), expected);

    unsafe { (*host.plugin).reset.unwrap()(host.plugin) };
    let mut outputs = [Samples::F32(vec![vec![0.0; 64]])];
    let events = [notes[0], notes[1], Sent::Transport(transport(30, 90.0))];
    let run = host.process(&mut [], &mut outputs, &events, Some(120.0));
    assert_eq!((run.status, run.heap_calls), (CLAP_PROCESS_CONTINUE, 0));
    assert_eq!(outputs[0].f32()[0], expected[..64]);

    // A mode that is neither is refused and changes nothing.
    assert!(!set(7));
    unsafe { (*host.plugin).reset.unwrap()(host.plugin) };
    assert_eq!(host.play(64, &[64], &notes), expected[..64]);

    unsafe { (*host.plugin).reset.unwrap()(host.plugin) };
    assert!(set(CLAP_RENDER_REALTIME));
    assert_eq!(host.play(64, &[64], &notes), [0.0; 64]);
}
