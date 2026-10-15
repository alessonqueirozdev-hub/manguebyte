//! What the end-to-end tests share: a counting allocator, a test plugin
//! with a knob and a meter, and a fake CLAP host that drives the plugin
//! through the C interface, the way a host does.
//!
//! Every `unsafe` block here is a call across that interface with pointers
//! the test owns and keeps alive for the duration of the call, so the
//! blocks are not commented one by one.

#![allow(dead_code)]
#![allow(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Mutex};

use manguebyte_clap::clap_sys::audio_buffer::clap_audio_buffer;
use manguebyte_clap::clap_sys::entry::clap_plugin_entry;
use manguebyte_clap::clap_sys::events::*;
use manguebyte_clap::clap_sys::ext::gui::*;
use manguebyte_clap::clap_sys::ext::params::*;
use manguebyte_clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte_clap::clap_sys::host::clap_host;
use manguebyte_clap::clap_sys::id::clap_id;
use manguebyte_clap::clap_sys::plugin::clap_plugin;
use manguebyte_clap::clap_sys::process::*;
use manguebyte_clap::clap_sys::version::CLAP_VERSION;
use manguebyte_clap::plugin_entry;
use manguebyte_core::{AtomicF32, AudioBuffer, ParamId};
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_gui::{Editor, HeadlessEditor};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::editor::{EditorSpec, HostSize, Resize};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_ui::{
    AccessEntry, AtomicSource, IntoView, ParamHandle, Role, column, knob, label, meter, row,
};

// --- Watching the heap ---------------------------------------------------
//
// `process` and `flush` run on the host's audio thread, so they must not
// allocate or free, whatever the editor does meanwhile. The allocator
// counts heap calls made while a thread is inside one of them.

thread_local! {
    static IN_PROCESS: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

pub struct CountingAllocator;

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

/// Runs `work` with the heap watched on this thread, and returns how many
/// heap calls it made with what it returned.
pub fn watched<R>(work: impl FnOnce() -> R) -> (usize, R) {
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_PROCESS.with(|watching| watching.set(true));
    let result = work();
    IN_PROCESS.with(|watching| watching.set(false));
    (HEAP_CALLS.with(Cell::get), result)
}

// --- The plugin under test -------------------------------------------------

pub struct TestParams {
    pub gain: FloatParam,
}
param_set!(TestParams { gain });

pub const GAIN: ParamId = ParamId::from_name("gain");

/// The editor's size at scale 1.
pub const BASE: HostSize = HostSize::new(240, 180);

/// How the editor may be resized, at scale 1.
pub const RULES: Resize = Resize::Free {
    min: HostSize::new(200, 150),
    max: HostSize::new(480, 360),
};

thread_local! {
    /// The editors opened on this thread, the latest last; and the
    /// parameters and level of the plugin most recently created.
    static OPENED: RefCell<Vec<HeadlessEditor>> = const { RefCell::new(Vec::new()) };
    static OPENS: Cell<usize> = const { Cell::new(0) };
    static SHARED: RefCell<Option<(Arc<TestParams>, Arc<AtomicF32>)>> = const { RefCell::new(None) };
}

/// The editor opened last on this thread, if it is still open.
pub fn editor() -> HeadlessEditor {
    OPENED.with(|opened| {
        opened
            .borrow_mut()
            .pop()
            .expect("an editor was opened on this thread")
    })
}

/// Forgets the editors opened so far, and returns how many there were.
pub fn forget_editors() -> usize {
    OPENED.with(|opened| opened.borrow_mut().drain(..).count())
}

/// How many editors have been opened on this thread.
pub fn opens() -> usize {
    OPENS.with(Cell::get)
}

/// The parameters of the plugin most recently created on this thread.
pub fn params() -> Arc<TestParams> {
    SHARED.with(|shared| shared.borrow().as_ref().expect("a plugin").0.clone())
}

/// The level the plugin most recently created on this thread publishes.
pub fn level() -> Arc<AtomicF32> {
    SHARED.with(|shared| shared.borrow().as_ref().expect("a plugin").1.clone())
}

/// The plugin's editor: a title, a knob for the gain and a meter for the
/// level the processor publishes.
pub fn editor_spec(params: &Arc<TestParams>, level: &Arc<AtomicF32>) -> Editor {
    let params = Arc::clone(params);
    let level = Arc::clone(level);
    Editor::new(BASE, move || {
        column((
            label("Gain test").title(),
            row((
                knob(ParamHandle::new(Arc::clone(&params), |p| &p.gain)),
                meter("Output", AtomicSource::new(Arc::clone(&level)))
                    .range(-60.0, 0.0)
                    .unit("dB")
                    .precision(1),
            ))
            .spacing(12.0),
        ))
        .spacing(8.0)
        .padding(12.0)
    })
    .with_resize(RULES)
    .on_headless_open(|editor| {
        OPENS.with(|opens| opens.set(opens.get() + 1));
        OPENED.with(|opened| opened.borrow_mut().push(editor));
    })
}

/// A plugin that outputs its gain as a constant signal and publishes the
/// level of what it outputs, in decibels, for its editor's meter.
pub struct GainTest {
    params: Arc<TestParams>,
    level: Arc<AtomicF32>,
}

impl Plugin for GainTest {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.gui",
        name: "Gui test",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "A knob and a meter",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 0,
    };
    type Params = TestParams;

    fn new() -> Self {
        let params = Arc::new(TestParams {
            gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5)
                .with_precision(2),
        });
        let level = Arc::new(AtomicF32::new(-60.0));
        SHARED.with(|shared| *shared.borrow_mut() = Some((params.clone(), level.clone())));
        Self { params, level }
    }

    fn params(&self) -> Arc<TestParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, buffer: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {
        let gain = self.params.gain.value();
        for channel in buffer.iter_channels_mut() {
            channel.fill(gain);
        }
        let peak = buffer.peak();
        let db = if peak > 0.0 {
            (20.0 * peak.log10()).max(-60.0)
        } else {
            -60.0
        };
        self.level.store(db, Ordering::Relaxed);
    }

    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        Some(Box::new(editor_spec(&self.params, &self.level)))
    }
}

/// The widget with `role` and `name` in `editor`.
pub fn get(editor: &HeadlessEditor, role: Role, name: &str) -> AccessEntry {
    editor.find(role, name).unwrap_or_else(|| {
        panic!(
            "no {role:?} named {name:?} in {:?}",
            editor
                .access_tree()
                .iter()
                .map(|entry| (entry.node.role, entry.node.name.clone()))
                .collect::<Vec<_>>()
        )
    })
}

/// The knob's value as text.
pub fn knob_text(editor: &HeadlessEditor) -> Option<String> {
    get(editor, Role::Slider, "Gain").node.value
}

// --- The fake host -----------------------------------------------------------

/// Every call the plugin made to the host, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCall {
    RequestCallback,
    RequestFlush,
    Rescan(u32),
    RequestResize(u32, u32),
    Closed(bool),
}

/// How the host answers `request_resize`: grants it, or not.
pub struct HostState {
    calls: Mutex<Vec<HostCall>>,
    plugin: AtomicPtr<clap_plugin>,
    grant_resizes: std::sync::atomic::AtomicBool,
}

impl HostState {
    fn record(&self, call: HostCall) {
        self.calls.lock().unwrap().push(call);
    }
}

unsafe fn state<'a>(host: *const clap_host) -> &'a HostState {
    unsafe { &*(*host).host_data.cast::<HostState>() }
}

unsafe extern "C" fn host_get_extension(
    _host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_PARAMS {
        ptr::from_ref(&HOST_PARAMS).cast()
    } else if id == CLAP_EXT_GUI {
        ptr::from_ref(&HOST_GUI).cast()
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn host_nothing(_: *const clap_host) {}

unsafe extern "C" fn host_request_callback(host: *const clap_host) {
    unsafe { state(host) }.record(HostCall::RequestCallback);
}

unsafe extern "C" fn host_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    unsafe { state(host) }.record(HostCall::Rescan(flags));
}

unsafe extern "C" fn host_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {}

unsafe extern "C" fn host_request_flush(host: *const clap_host) {
    unsafe { state(host) }.record(HostCall::RequestFlush);
}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_rescan),
    clear: Some(host_clear),
    request_flush: Some(host_request_flush),
};

unsafe extern "C" fn host_resize_hints_changed(_: *const clap_host) {}

unsafe extern "C" fn host_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    let state = unsafe { state(host) };
    state.record(HostCall::RequestResize(width, height));
    state.grant_resizes.load(Ordering::Relaxed)
}

unsafe extern "C" fn host_request_show_or_hide(_: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn host_closed(host: *const clap_host, was_destroyed: bool) {
    unsafe { state(host) }.record(HostCall::Closed(was_destroyed));
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_resize_hints_changed),
    request_resize: Some(host_request_resize),
    request_show: Some(host_request_show_or_hide),
    request_hide: Some(host_request_show_or_hide),
    closed: Some(host_closed),
};

/// An event the plugin sent the host.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Out {
    Begin(ParamId),
    Value(ParamId, f64),
    End(ParamId),
}

/// An event the plugin handed to the host, read as a host reads it.
///
/// # Safety
///
/// `event` must point to a whole CLAP event.
unsafe fn decode(event: *const clap_event_header) -> Out {
    unsafe {
        let head = &*event;
        assert_eq!(head.space_id, CLAP_CORE_EVENT_SPACE_ID);
        match head.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                assert_eq!(head.size as usize, size_of::<clap_event_param_value>());
                let value = &*event.cast::<clap_event_param_value>();
                Out::Value(ParamId(value.param_id), value.value)
            }
            CLAP_EVENT_PARAM_GESTURE_BEGIN | CLAP_EVENT_PARAM_GESTURE_END => {
                assert_eq!(head.size as usize, size_of::<clap_event_param_gesture>());
                let gesture = &*event.cast::<clap_event_param_gesture>();
                let param = ParamId(gesture.param_id);
                if head.type_ == CLAP_EVENT_PARAM_GESTURE_BEGIN {
                    Out::Begin(param)
                } else {
                    Out::End(param)
                }
            }
            other => panic!("the plugin sent an event of type {other}"),
        }
    }
}

/// An output list with room for a fixed number of events and no heap: what
/// it is handed is kept in place, and what does not fit is counted.
pub struct Outbox {
    events: [Option<Out>; 64],
    len: usize,
    overflow: usize,
}

impl Outbox {
    pub const fn new() -> Self {
        Self {
            events: [None; 64],
            len: 0,
            overflow: 0,
        }
    }

    pub fn to_vec(&self) -> Vec<Out> {
        self.events[..self.len].iter().flatten().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.len + self.overflow
    }
}

unsafe extern "C" fn keep_event(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        let outbox = &mut *(*list).ctx.cast::<Outbox>();
        let decoded = decode(event);
        if outbox.len < outbox.events.len() {
            outbox.events[outbox.len] = Some(decoded);
            outbox.len += 1;
        } else {
            outbox.overflow += 1;
        }
    }
    true
}

/// The host's events for a block: parameter values, as a slice.
unsafe extern "C" fn input_size(list: *const clap_input_events) -> u32 {
    let events = unsafe { *(*list).ctx.cast::<&[clap_event_param_value]>() };
    events.len() as u32
}

unsafe extern "C" fn input_get(
    list: *const clap_input_events,
    index: u32,
) -> *const clap_event_header {
    unsafe {
        let events = *(*list).ctx.cast::<&[clap_event_param_value]>();
        events
            .get(index as usize)
            .map_or(ptr::null(), |event| ptr::from_ref(event).cast())
    }
}

/// A host automation point at frame 0, in CLAP's scale for the parameter.
pub fn automation(param: ParamId, value: f64) -> clap_event_param_value {
    clap_event_param_value {
        header: clap_event_header {
            size: size_of::<clap_event_param_value>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_PARAM_VALUE,
            flags: 0,
        },
        param_id: param.0,
        cookie: ptr::null_mut(),
        note_id: -1,
        port_index: -1,
        channel: -1,
        key: -1,
        value,
    }
}

pub const FRAMES: usize = 32;

/// A plugin pointer that may be handed to the audio thread: CLAP lets the
/// host call `process` there while the main thread calls everything else.
#[derive(Clone, Copy)]
pub struct AudioSide(pub *const clap_plugin);

// SAFETY: the plugin is used from the audio thread only through `process`,
// and only between `start_processing` and `stop_processing`, which CLAP
// allows alongside the main thread's calls.
unsafe impl Send for AudioSide {}

impl AudioSide {
    /// Runs one block, as the audio thread, with `input` as the host's
    /// events, putting what the plugin sends into `outbox`. Returns how many
    /// heap calls the plugin made.
    pub fn block(self, input: &[clap_event_param_value], outbox: &mut Outbox) -> usize {
        let plugin = self.0;
        let mut samples = [0.0_f32; FRAMES];
        let mut channels = [samples.as_mut_ptr()];
        let mut outputs = [clap_audio_buffer {
            data32: channels.as_mut_ptr(),
            data64: ptr::null_mut(),
            channel_count: 1,
            latency: 0,
            constant_mask: 0,
        }];
        let events: &[clap_event_param_value] = input;
        let input = clap_input_events {
            ctx: (&raw const events).cast_mut().cast(),
            size: Some(input_size),
            get: Some(input_get),
        };
        let output = clap_output_events {
            ctx: ptr::from_mut(outbox).cast(),
            try_push: Some(keep_event),
        };
        let process = clap_process {
            steady_time: -1,
            frames_count: FRAMES as u32,
            transport: ptr::null(),
            audio_inputs: ptr::null(),
            audio_outputs: outputs.as_mut_ptr(),
            audio_inputs_count: 0,
            audio_outputs_count: 1,
            in_events: &raw const input,
            out_events: &raw const output,
        };
        let (heap, status) =
            watched(|| unsafe { (*plugin).process.unwrap()(plugin, &raw const process) });
        assert_eq!(status, CLAP_PROCESS_CONTINUE);
        heap
    }

    /// Calls `params.flush`, as the audio thread or as the main thread when
    /// the plugin is not active. Returns how many heap calls it made.
    pub fn flush(self, input: &[clap_event_param_value], outbox: &mut Outbox) -> usize {
        let plugin = self.0;
        let events: &[clap_event_param_value] = input;
        let input = clap_input_events {
            ctx: (&raw const events).cast_mut().cast(),
            size: Some(input_size),
            get: Some(input_get),
        };
        let output = clap_output_events {
            ctx: ptr::from_mut(outbox).cast(),
            try_push: Some(keep_event),
        };
        let params = unsafe {
            (*plugin).get_extension.unwrap()(plugin, CLAP_EXT_PARAMS.as_ptr())
                .cast::<clap_plugin_params>()
                .as_ref()
                .unwrap()
        };
        let flush = params.flush.unwrap();
        let (heap, ()) = watched(|| unsafe { flush(plugin, &raw const input, &raw const output) });
        heap
    }

    pub fn start(self) -> bool {
        unsafe { (*self.0).start_processing.unwrap()(self.0) }
    }

    pub fn stop(self) {
        unsafe { (*self.0).stop_processing.unwrap()(self.0) }
    }
}

/// The plugin's `gui` extension, called as a host calls it.
#[derive(Clone, Copy)]
pub struct Ui {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
}

impl Ui {
    fn of(plugin: *const clap_plugin) -> Self {
        let gui = unsafe {
            (*plugin).get_extension.unwrap()(plugin, CLAP_EXT_GUI.as_ptr())
                .cast::<clap_plugin_gui>()
        };
        assert!(!gui.is_null(), "the plugin offers a gui");
        Self { plugin, gui }
    }

    fn table(&self) -> &clap_plugin_gui {
        unsafe { &*self.gui }
    }

    pub fn is_api_supported(&self, api: &CStr) -> bool {
        unsafe { self.table().is_api_supported.unwrap()(self.plugin, api.as_ptr(), false) }
    }

    pub fn create(&self) -> bool {
        unsafe { self.table().create.unwrap()(self.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) }
    }

    pub fn destroy(&self) {
        unsafe { self.table().destroy.unwrap()(self.plugin) }
    }

    pub fn set_scale(&self, scale: f64) -> bool {
        unsafe { self.table().set_scale.unwrap()(self.plugin, scale) }
    }

    pub fn size(&self) -> Option<HostSize> {
        let (mut width, mut height) = (0, 0);
        unsafe {
            self.table().get_size.unwrap()(self.plugin, &raw mut width, &raw mut height)
                .then_some(HostSize::new(width, height))
        }
    }

    pub fn can_resize(&self) -> bool {
        unsafe { self.table().can_resize.unwrap()(self.plugin) }
    }

    pub fn hints(&self) -> Option<clap_gui_resize_hints> {
        let mut hints = clap_gui_resize_hints {
            can_resize_horizontally: false,
            can_resize_vertically: false,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 0,
            aspect_ratio_height: 0,
        };
        unsafe {
            self.table().get_resize_hints.unwrap()(self.plugin, &raw mut hints).then_some(hints)
        }
    }

    pub fn adjust(&self, size: HostSize) -> Option<HostSize> {
        let (mut width, mut height) = (size.width, size.height);
        unsafe {
            self.table().adjust_size.unwrap()(self.plugin, &raw mut width, &raw mut height)
                .then_some(HostSize::new(width, height))
        }
    }

    pub fn set_size(&self, size: HostSize) -> bool {
        unsafe { self.table().set_size.unwrap()(self.plugin, size.width, size.height) }
    }

    /// Embeds the editor in the Win32 window `handle`, which, in headless
    /// mode, need not exist.
    pub fn set_parent(&self, handle: usize) -> bool {
        let window = clap_window {
            api: CLAP_WINDOW_API_WIN32.as_ptr(),
            specific: clap_window_handle {
                win32: ptr::without_provenance_mut(handle),
            },
        };
        unsafe { self.table().set_parent.unwrap()(self.plugin, &raw const window) }
    }

    pub fn show(&self) -> bool {
        unsafe { self.table().show.unwrap()(self.plugin) }
    }

    pub fn hide(&self) -> bool {
        unsafe { self.table().hide.unwrap()(self.plugin) }
    }

    pub fn on_main_thread(&self) {
        unsafe { (*self.plugin).on_main_thread.unwrap()(self.plugin) };
    }

    /// Creates, embeds and shows the editor, as a host opening it does, at
    /// `scale` if given.
    pub fn open(&self, scale: Option<f64>) -> HeadlessEditor {
        assert!(self.create());
        if let Some(scale) = scale {
            assert!(self.set_scale(scale));
        }
        assert!(self.set_parent(PARENT));
        assert!(self.show());
        editor()
    }
}

/// The window handle the fake host gives. Headless windows never look at
/// it.
pub const PARENT: usize = 0x0004_05A2;

/// A plugin created in the fake host, through the factory.
pub struct Session {
    entry: clap_plugin_entry,
    /// Owned through raw pointers: the plugin keeps pointers to both.
    host: *mut clap_host,
    state: *mut HostState,
    pub plugin: *const clap_plugin,
    active: Cell<bool>,
    processing: Cell<bool>,
}

impl Session {
    pub fn new() -> Self {
        forget_editors();
        let entry = plugin_entry::<GainTest>();
        let state = Box::into_raw(Box::new(HostState {
            calls: Mutex::new(Vec::new()),
            plugin: AtomicPtr::new(ptr::null_mut()),
            grant_resizes: std::sync::atomic::AtomicBool::new(true),
        }));
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: state.cast(),
            name: c"gui end-to-end host".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_nothing),
            request_process: Some(host_nothing),
            request_callback: Some(host_request_callback),
        }));
        unsafe {
            assert!(entry.init.unwrap()(c"/plugins/gui-test.clap".as_ptr()));
            let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>();
            let plugin = (*factory).create_plugin.unwrap()(
                factory,
                host,
                c"org.manguebyte.test.gui".as_ptr(),
            );
            assert!(!plugin.is_null());
            assert!((*plugin).init.unwrap()(plugin));
            (*state).plugin.store(plugin.cast_mut(), Ordering::Release);
            Self {
                entry,
                host,
                state,
                plugin,
                active: Cell::new(false),
                processing: Cell::new(false),
            }
        }
    }

    fn state(&self) -> &HostState {
        unsafe { &*self.state }
    }

    pub fn ui(&self) -> Ui {
        Ui::of(self.plugin)
    }

    pub fn audio(&self) -> AudioSide {
        AudioSide(self.plugin)
    }

    /// Every call the plugin made to the host since the last time.
    pub fn take_calls(&self) -> Vec<HostCall> {
        std::mem::take(&mut *self.state().calls.lock().unwrap())
    }

    /// Makes the host grant resize requests, or refuse them.
    pub fn grant_resizes(&self, grant: bool) {
        self.state().grant_resizes.store(grant, Ordering::Relaxed);
    }

    pub fn activate(&self) {
        unsafe {
            assert!((*self.plugin).activate.unwrap()(
                self.plugin,
                48_000.0,
                1,
                FRAMES as u32
            ))
        };
        self.active.set(true);
    }

    pub fn start(&self) {
        if !self.active.get() {
            self.activate();
        }
        assert!(self.audio().start());
        self.processing.set(true);
    }

    pub fn stop(&self) {
        self.audio().stop();
        self.processing.set(false);
    }

    /// Runs one block with `input` as the host's events and returns what the
    /// plugin sent, after checking that it did not use the heap.
    pub fn process_with(&self, input: &[clap_event_param_value]) -> Vec<Out> {
        let mut outbox = Outbox::new();
        assert_eq!(
            self.audio().block(input, &mut outbox),
            0,
            "process used the heap"
        );
        outbox.to_vec()
    }

    pub fn process(&self) -> Vec<Out> {
        self.process_with(&[])
    }

    /// Calls `params.flush` and returns what the plugin sent, after checking
    /// that it did not use the heap.
    pub fn flush(&self) -> Vec<Out> {
        let mut outbox = Outbox::new();
        assert_eq!(
            self.audio().flush(&[], &mut outbox),
            0,
            "flush used the heap"
        );
        outbox.to_vec()
    }

    /// The value the host reads for `param` through `params.get_value`.
    pub fn value(&self, param: ParamId) -> f64 {
        let mut value = f64::NAN;
        unsafe {
            let params =
                (*self.plugin).get_extension.unwrap()(self.plugin, CLAP_EXT_PARAMS.as_ptr())
                    .cast::<clap_plugin_params>()
                    .as_ref()
                    .unwrap();
            assert!(params.get_value.unwrap()(
                self.plugin,
                param.0,
                &raw mut value
            ));
        }
        value
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            if self.processing.get() {
                (*self.plugin).stop_processing.unwrap()(self.plugin);
            }
            if self.active.get() {
                (*self.plugin).deactivate.unwrap()(self.plugin);
            }
            (*self.plugin).destroy.unwrap()(self.plugin);
            self.entry.deinit.unwrap()();
            drop(Box::from_raw(self.host));
            drop(Box::from_raw(self.state));
        }
        forget_editors();
    }
}

/// Checks that the events alternate begin and end on each parameter and
/// that every value is sent inside a gesture. `open` carries the state from
/// one call to the next. Returns how many gestures began.
pub fn check_nesting(events: &[Out], open: &mut Vec<ParamId>) -> usize {
    let mut begins = 0;
    for (index, event) in events.iter().enumerate() {
        match *event {
            Out::Begin(param) => {
                assert!(
                    !open.contains(&param),
                    "event {index}: {event:?} while open"
                );
                open.push(param);
                begins += 1;
            }
            Out::Value(param, value) => {
                assert!(
                    open.contains(&param),
                    "event {index}: {event:?} outside a gesture"
                );
                assert!((0.0..=1.0).contains(&value), "event {index}: {event:?}");
            }
            Out::End(param) => {
                assert!(
                    open.contains(&param),
                    "event {index}: {event:?} while closed"
                );
                open.retain(|&id| id != param);
            }
        }
    }
    begins
}

thread_local! {
    static PANICS: Cell<usize> = const { Cell::new(0) };
}

/// Panics are caught at the C interface (the wrapper's guard) and at the
/// window (which poisons itself), so a test could not see them otherwise.
/// From the first call on, every panic is counted on the thread it
/// happens on, and still reported as usual.
pub fn watch_panics() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        let report = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = PANICS.try_with(|panics| panics.set(panics.get() + 1));
            report(info);
        }));
    });
}

/// How many panics happened on this thread since [`watch_panics`] was
/// first called.
pub fn panics() -> usize {
    PANICS.with(Cell::get)
}

/// Asserts that `got` is `expected` but for rounding: parameters store
/// their values as `f32`.
pub fn assert_events(got: &[Out], expected: &[Out]) {
    let close = |a: &Out, b: &Out| match (a, b) {
        (Out::Value(p, x), Out::Value(q, y)) => p == q && (x - y).abs() < 1e-6,
        _ => a == b,
    };
    assert!(
        got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| close(a, b)),
        "the host received {got:?}, not {expected:?}"
    );
}
