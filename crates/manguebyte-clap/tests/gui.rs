//! Drives the CLAP `gui` extension, and an editor's way back to the host,
//! through the C interface, the way a host does.
//!
//! The editor is a fake that records every call made to it and lets a test
//! act as the person using it: drag a control, ask for a size, register a
//! timer. The host is a fake that records every call made to it and can
//! answer the way real hosts do, including by calling the plugin back from
//! inside its own functions.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_ulong, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use manguebyte_clap::clap_sys::audio_buffer::clap_audio_buffer;
use manguebyte_clap::clap_sys::entry::clap_plugin_entry;
use manguebyte_clap::clap_sys::events::*;
use manguebyte_clap::clap_sys::ext::gui::*;
use manguebyte_clap::clap_sys::ext::params::*;
use manguebyte_clap::clap_sys::ext::posix_fd_support::*;
use manguebyte_clap::clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use manguebyte_clap::clap_sys::ext::timer_support::*;
use manguebyte_clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte_clap::clap_sys::host::clap_host;
use manguebyte_clap::clap_sys::id::clap_id;
use manguebyte_clap::clap_sys::plugin::clap_plugin;
use manguebyte_clap::clap_sys::process::*;
use manguebyte_clap::clap_sys::stream::clap_istream;
use manguebyte_clap::clap_sys::version::CLAP_VERSION;
use manguebyte_clap::plugin_entry;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::EventKind;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::{ChoiceParam, FloatParam, FloatRange, Param, ParamInfo, param_set};
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorSpec, Gesture, HostSize, LoopHandle, MainThreadWaker,
    OpenEditor, ParentWindow, Resize, WindowApi, checked_scale,
};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, save_state};

// --- Watching the heap ---------------------------------------------------
//
// `process` runs on the host's audio thread, so it must not allocate or
// free, editor or no editor. The allocator counts heap calls made while a
// thread is inside `process`.

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

// --- The plugins under test ------------------------------------------------

thread_local! {
    /// Runs once, right after the next value is stored into `cutoff` on
    /// this thread: the moment another thread could store a value of its
    /// own, or the host could run a block.
    static HOOK: RefCell<Option<Box<dyn FnOnce()>>> = const { RefCell::new(None) };
    /// The last value of `cutoff` the plugin was told about in an event,
    /// on the thread that processes.
    static PLUGIN_SAW: Cell<Option<f64>> = const { Cell::new(None) };
}

/// Runs `then` right after the next value is stored into `cutoff` on this
/// thread.
fn after_next_store(then: impl FnOnce() + 'static) {
    HOOK.with(|hook| *hook.borrow_mut() = Some(Box::new(then)));
}

/// A float parameter that runs `HOOK` right after a value is stored. It
/// reads as the `FloatParam` inside.
struct Hooked(FloatParam);

impl std::ops::Deref for Hooked {
    type Target = FloatParam;
    fn deref(&self) -> &FloatParam {
        &self.0
    }
}

impl Param for Hooked {
    fn info(&self) -> &ParamInfo {
        self.0.info()
    }
    fn plain(&self) -> f64 {
        self.0.plain()
    }
    fn set_plain(&self, value: f64) {
        self.0.set_plain(value);
        let hook = HOOK
            .try_with(|hook| hook.borrow_mut().take())
            .ok()
            .flatten();
        if let Some(hook) = hook {
            hook();
        }
    }
    fn stored_plain(&self, value: f64) -> Option<f64> {
        self.0.stored_plain(value)
    }
    fn default_plain(&self) -> f64 {
        self.0.default_plain()
    }
    fn normalized_to_plain(&self, normalized: f64) -> f64 {
        self.0.normalized_to_plain(normalized)
    }
    fn plain_to_normalized(&self, plain: f64) -> f64 {
        self.0.plain_to_normalized(plain)
    }
    fn step_count(&self) -> Option<u32> {
        self.0.step_count()
    }
    fn plain_to_text(&self, plain: f64) -> String {
        self.0.plain_to_text(plain)
    }
    fn text_to_plain(&self, text: &str) -> Option<f64> {
        self.0.text_to_plain(text)
    }
}

struct PanelParams {
    cutoff: Hooked,
    mode: ChoiceParam,
    gain: FloatParam,
}
param_set!(PanelParams { cutoff, mode, gain });

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const MODE: ParamId = ParamId::from_name("mode");
const GAIN: ParamId = ParamId::from_name("gain");

fn panel_params() -> Arc<PanelParams> {
    Arc::new(PanelParams {
        cutoff: Hooked(FloatParam::new(
            "cutoff",
            "Cutoff",
            FloatRange::linear(0.0, 100.0),
            25.0,
        )),
        mode: ChoiceParam::new("mode", "Mode", &["Low", "Band", "High"], 0),
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
    })
}

/// Notes the last value of `cutoff` the plugin is told about.
fn note_what_the_plugin_saw(context: &ProcessContext<'_, f32>) {
    for event in context.events {
        if let EventKind::ParamValue { param, value } = event.kind {
            if param == CUTOFF {
                let _ = PLUGIN_SAW.try_with(|saw| saw.set(Some(value)));
            }
        }
    }
}

thread_local! {
    /// The parameters of the plugin most recently created on this thread,
    /// for a test to read as an interface would.
    static PARAMS: RefCell<Option<Arc<PanelParams>>> = const { RefCell::new(None) };
}

fn params() -> Arc<PanelParams> {
    PARAMS.with(|params| params.borrow().clone().expect("a plugin was created"))
}

/// A plugin with an editor, the fake one below, set up by `SETUP`.
struct Panel {
    params: Arc<PanelParams>,
}

impl Plugin for Panel {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.panel",
        name: "Panel",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Has an editor",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 0,
        outputs: 1,
        sidechain: 0,
    };
    type Params = PanelParams;

    fn new() -> Self {
        let params = panel_params();
        PARAMS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&params)));
        Self { params }
    }
    fn params(&self) -> Arc<PanelParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        note_what_the_plugin_saw(context);
    }
    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        Some(Box::new(Spec(SETUP.with(|setup| setup.borrow().clone()))))
    }
}

/// The same plugin without an editor.
struct Bare {
    params: Arc<PanelParams>,
}

impl Plugin for Bare {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.bare",
        name: "Bare",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "Has no editor",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = Panel::LAYOUT;
    type Params = PanelParams;

    fn new() -> Self {
        let params = panel_params();
        PARAMS.with(|slot| *slot.borrow_mut() = Some(Arc::clone(&params)));
        Self { params }
    }
    fn params(&self) -> Arc<PanelParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

// --- The fake editor ---------------------------------------------------------

/// How the next `Panel` created on this thread sets up its editor.
#[derive(Clone)]
struct Setup {
    /// The size at scale 1.
    base: HostSize,
    resize: Resize,
    /// Win32, Cocoa and X11, in that order.
    supports: [bool; 3],
    fail_open: bool,
    accept_scale: bool,
    /// Registers a 16 ms timer from inside `open`.
    timer_on_open: bool,
    /// Asks for this size from inside `set_visible(true)`.
    resize_on_show: Option<HostSize>,
    /// Asks for this size from inside `open`, as an editor given no scale
    /// does once it has found the system's.
    resize_on_open: Option<HostSize>,
    /// Refuses every size the host sets, as an editor whose rules differ
    /// from its spec's would.
    refuse_sizes: bool,
    panic_on_show: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            base: HostSize::new(300, 200),
            resize: Resize::Free {
                min: HostSize::new(150, 100),
                max: HostSize::new(1200, 800),
            },
            supports: [true; 3],
            fail_open: false,
            accept_scale: true,
            timer_on_open: false,
            resize_on_show: None,
            resize_on_open: None,
            refuse_sizes: false,
            panic_on_show: false,
        }
    }
}

thread_local! {
    static SETUP: RefCell<Setup> = RefCell::new(Setup::default());
    static LOG: RefCell<Vec<EditorCall>> = const { RefCell::new(Vec::new()) };
    static PROBE: RefCell<Weak<Probe>> = const { RefCell::new(Weak::new()) };
}

fn set_up(setup: Setup) {
    SETUP.with(|slot| *slot.borrow_mut() = setup);
}

/// Every call made to an editor on this thread, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EditorCall {
    Open(ParentWindow, Option<f64>),
    Size,
    Fit(HostSize),
    SetSize(HostSize, bool),
    SetScale(f64),
    SetVisible(bool),
    OnMainThread,
    Dropped,
    /// An editor method was called while the editor was calling the host.
    Reentered,
}

fn log(call: EditorCall) {
    LOG.with(|log| log.borrow_mut().push(call));
}

fn take_log() -> Vec<EditorCall> {
    LOG.with(|log| log.take())
}

fn api_index(api: WindowApi) -> Option<usize> {
    match api {
        WindowApi::Win32 => Some(0),
        WindowApi::Cocoa => Some(1),
        WindowApi::X11 => Some(2),
        _ => None,
    }
}

struct Spec(Setup);

impl EditorSpec for Spec {
    fn size(&self, scale: f64) -> HostSize {
        self.0.base.scale_by(scale)
    }

    fn resize(&self) -> Resize {
        self.0.resize
    }

    fn supports(&self, api: WindowApi) -> bool {
        api_index(api).is_some_and(|index| self.0.supports[index])
    }

    fn open(
        &self,
        parent: ParentWindow,
        scale: Option<f64>,
        context: EditorContext,
    ) -> Result<Box<dyn OpenEditor>, EditorError> {
        log(EditorCall::Open(parent, scale));
        assert!(scale.is_none_or(|scale| checked_scale(scale).is_some()));
        let probe = Rc::new(Probe {
            context,
            setup: self.0.clone(),
            size: Cell::new(self.size(scale.unwrap_or(1.0))),
            scale: Cell::new(scale),
            calling_host: Cell::new(false),
            gestures: RefCell::new(Vec::new()),
            handles: RefCell::new(Vec::new()),
            ticks: Rc::new(Cell::new(0)),
        });
        if self.0.timer_on_open {
            assert!(probe.every(16));
        }
        if let Some(size) = self.0.resize_on_open {
            probe.request_resize(size);
        }
        if self.0.fail_open {
            return Err(EditorError::Platform("no window for you".into()));
        }
        PROBE.with(|slot| *slot.borrow_mut() = Rc::downgrade(&probe));
        Ok(Box::new(FakeEditor { probe }))
    }
}

/// The editor's insides, which a test drives as a person would.
struct Probe {
    context: EditorContext,
    setup: Setup,
    size: Cell<HostSize>,
    scale: Cell<Option<f64>>,
    /// Set while the editor is calling the host.
    calling_host: Cell<bool>,
    gestures: RefCell<Vec<Gesture>>,
    handles: RefCell<Vec<LoopHandle>>,
    ticks: Rc<Cell<u32>>,
}

impl Probe {
    /// Calls the host, noting that the editor is doing so.
    fn out<R>(&self, call: impl FnOnce(&EditorContext) -> R) -> R {
        let outer = self.calling_host.replace(true);
        let result = call(&self.context);
        self.calling_host.set(outer);
        result
    }

    fn rules(&self) -> Resize {
        self.setup.resize.scaled(self.scale.get().unwrap_or(1.0))
    }

    /// The pointer goes down on the control for `param`, which shows `start`.
    fn drag(&self, param: ParamId, start: f64) {
        let gesture = self.out(|context| Gesture::begin(context, param, start));
        self.gestures.borrow_mut().push(gesture);
    }

    /// The pointer moves the last control taken hold of to `value`.
    fn move_to(&self, value: f64) {
        let gesture = self.gestures.borrow_mut().pop();
        if let Some(mut gesture) = gesture {
            self.out(|_| gesture.set(value));
            self.gestures.borrow_mut().push(gesture);
        }
    }

    /// The pointer lets go of the last control taken hold of.
    fn release(&self) {
        let gesture = self.gestures.borrow_mut().pop();
        self.out(|_| drop(gesture));
    }

    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        let answer = self.out(|context| context.request_resize(size));
        if let Some(size) = answer {
            self.size.set(size);
        }
        answer
    }

    fn every(&self, period_ms: u32) -> bool {
        let ticks = Rc::clone(&self.ticks);
        let handle =
            self.out(|context| context.every(period_ms, move || ticks.set(ticks.get() + 1)));
        handle
            .map(|handle| self.handles.borrow_mut().push(handle))
            .is_some()
    }

    fn watch(&self, fd: i32) -> bool {
        let ticks = Rc::clone(&self.ticks);
        let handle = self.out(|context| context.watch_fd(fd, move || ticks.set(ticks.get() + 100)));
        handle
            .map(|handle| self.handles.borrow_mut().push(handle))
            .is_some()
    }

    fn drop_handle(&self, index: usize) {
        let handle = self.handles.borrow_mut().remove(index);
        self.out(|_| drop(handle));
    }
}

/// Runs `work` on the open editor's probe. The probe is not kept, so that
/// closing the editor drops it, and with it whatever gestures it holds.
fn with_probe<R>(work: impl FnOnce(&Probe) -> R) -> R {
    let probe = PROBE
        .with(|slot| slot.borrow().upgrade())
        .expect("an editor is open");
    work(&probe)
}

fn probe_alive() -> bool {
    PROBE.with(|slot| slot.borrow().strong_count() > 0)
}

struct FakeEditor {
    probe: Rc<Probe>,
}

impl FakeEditor {
    fn enter(&self, call: EditorCall) {
        if self.probe.calling_host.get() {
            log(EditorCall::Reentered);
        }
        log(call);
    }
}

impl OpenEditor for FakeEditor {
    fn size(&self) -> HostSize {
        self.enter(EditorCall::Size);
        self.probe.size.get()
    }

    fn fit(&self, wanted: HostSize) -> HostSize {
        self.enter(EditorCall::Fit(wanted));
        self.probe.rules().fit(self.probe.size.get(), wanted)
    }

    fn set_size(&mut self, size: HostSize) -> bool {
        let accepted = !self.probe.setup.refuse_sizes
            && self.probe.rules().fit(self.probe.size.get(), size) == size;
        self.enter(EditorCall::SetSize(size, accepted));
        if accepted {
            self.probe.size.set(size);
        }
        accepted
    }

    fn set_scale(&mut self, scale: f64) -> bool {
        self.enter(EditorCall::SetScale(scale));
        assert!(checked_scale(scale).is_some());
        if !self.probe.setup.accept_scale {
            return false;
        }
        self.probe.scale.set(Some(scale));
        self.probe.size.set(self.probe.setup.base.scale_by(scale));
        true
    }

    fn set_visible(&mut self, visible: bool) {
        self.enter(EditorCall::SetVisible(visible));
        if visible && self.probe.setup.panic_on_show {
            panic!("the editor failed to show itself");
        }
        if let Some(size) = self.probe.setup.resize_on_show.filter(|_| visible) {
            self.probe.request_resize(size);
        }
    }

    fn on_main_thread(&mut self) {
        self.enter(EditorCall::OnMainThread);
    }
}

impl Drop for FakeEditor {
    fn drop(&mut self) {
        if self.probe.calling_host.get() {
            // Dropped under its own feet.
            log(EditorCall::Reentered);
        }
        log(EditorCall::Dropped);
    }
}

// --- The fake host -----------------------------------------------------------

/// Every call the plugin made to the host, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostCall {
    RequestCallback,
    RequestFlush,
    Rescan(u32),
    RequestResize(u32, u32),
    Closed(bool),
    RegisterTimer(u32, clap_id),
    UnregisterTimer(clap_id),
    RegisterFd(i32, u32),
    UnregisterFd(i32),
}

/// What the host offers.
#[derive(Debug, Clone, Copy)]
struct Offers {
    params: bool,
    gui: bool,
    timers: bool,
    fds: bool,
    callbacks: bool,
}

const EVERYTHING: Offers = Offers {
    params: true,
    gui: true,
    timers: true,
    fds: true,
    callbacks: true,
};

/// The host's answer to `request_resize`, which may call the plugin back.
type ResizeReply = Box<dyn FnMut(Ui, u32, u32) -> bool + Send>;
/// What the host does when told the editor closed.
type ClosedReply = Box<dyn FnMut(Ui) + Send>;

struct HostState {
    offers: Offers,
    calls: Mutex<Vec<HostCall>>,
    plugin: AtomicPtr<clap_plugin>,
    on_resize: Mutex<Option<ResizeReply>>,
    on_closed: Mutex<Option<ClosedReply>>,
    refuse_registrations: AtomicBool,
    next_timer: AtomicU32,
    /// Assertions that failed inside the host's functions. A panic may not
    /// leave a C function, so they are kept here and checked by the test.
    failures: Mutex<Vec<String>>,
}

impl HostState {
    fn record(&self, call: HostCall) {
        self.calls.lock().unwrap().push(call);
    }

    fn ui(&self) -> Ui {
        Ui::of(self.plugin.load(Ordering::Acquire))
    }

    /// Runs a test's reply, keeping a failed assertion for later.
    fn run<R>(&self, fallback: R, reply: impl FnOnce() -> R) -> R {
        match catch_unwind(AssertUnwindSafe(reply)) {
            Ok(result) => result,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| {
                        payload
                            .downcast_ref::<&str>()
                            .map(|text| (*text).to_owned())
                    })
                    .unwrap_or_default();
                self.failures.lock().unwrap().push(message);
                fallback
            }
        }
    }
}

unsafe fn state<'a>(host: *const clap_host) -> &'a HostState {
    unsafe { &*(*host).host_data.cast::<HostState>() }
}

unsafe extern "C" fn host_get_extension(
    host: *const clap_host,
    id: *const c_char,
) -> *const c_void {
    let (offers, id) = unsafe { (state(host).offers, CStr::from_ptr(id)) };
    if offers.params && id == CLAP_EXT_PARAMS {
        ptr::from_ref(&HOST_PARAMS).cast()
    } else if offers.gui && id == CLAP_EXT_GUI {
        ptr::from_ref(&HOST_GUI).cast()
    } else if offers.timers && id == CLAP_EXT_TIMER_SUPPORT {
        ptr::from_ref(&HOST_TIMERS).cast()
    } else if offers.fds && id == CLAP_EXT_POSIX_FD_SUPPORT {
        ptr::from_ref(&HOST_FDS).cast()
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
    // Taken out while it runs, so that it may call the plugin, which may
    // call the host again.
    let reply = state.on_resize.lock().unwrap().take();
    let Some(mut reply) = reply else {
        return true;
    };
    let granted = state.run(false, || reply(state.ui(), width, height));
    *state.on_resize.lock().unwrap() = Some(reply);
    granted
}

unsafe extern "C" fn host_request_show_or_hide(_: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn host_closed(host: *const clap_host, was_destroyed: bool) {
    let state = unsafe { state(host) };
    state.record(HostCall::Closed(was_destroyed));
    let reply = state.on_closed.lock().unwrap().take();
    if let Some(mut reply) = reply {
        state.run((), || reply(state.ui()));
        *state.on_closed.lock().unwrap() = Some(reply);
    }
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_resize_hints_changed),
    request_resize: Some(host_request_resize),
    request_show: Some(host_request_show_or_hide),
    request_hide: Some(host_request_show_or_hide),
    closed: Some(host_closed),
};

unsafe extern "C" fn host_register_timer(
    host: *const clap_host,
    period_ms: u32,
    timer_id: *mut clap_id,
) -> bool {
    let state = unsafe { state(host) };
    if state.refuse_registrations.load(Ordering::Relaxed) {
        return false;
    }
    let id = state.next_timer.fetch_add(1, Ordering::Relaxed);
    unsafe { *timer_id = id };
    state.record(HostCall::RegisterTimer(period_ms, id));
    true
}

unsafe extern "C" fn host_unregister_timer(host: *const clap_host, timer_id: clap_id) -> bool {
    unsafe { state(host) }.record(HostCall::UnregisterTimer(timer_id));
    true
}

static HOST_TIMERS: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_register_timer),
    unregister_timer: Some(host_unregister_timer),
};

unsafe extern "C" fn host_register_fd(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    let state = unsafe { state(host) };
    if state.refuse_registrations.load(Ordering::Relaxed) {
        return false;
    }
    state.record(HostCall::RegisterFd(fd, flags));
    true
}

unsafe extern "C" fn host_modify_fd(_: *const clap_host, _: i32, _: clap_posix_fd_flags) -> bool {
    true
}

unsafe extern "C" fn host_unregister_fd(host: *const clap_host, fd: i32) -> bool {
    unsafe { state(host) }.record(HostCall::UnregisterFd(fd));
    true
}

static HOST_FDS: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_register_fd),
    modify_fd: Some(host_modify_fd),
    unregister_fd: Some(host_unregister_fd),
};

/// An event the plugin sent the host.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Out {
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
        assert_eq!(head.time, 0);
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

/// Collects what the plugin sends into the `Vec<Out>` behind the list.
unsafe extern "C" fn record_event(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        let sent = &mut *(*list).ctx.cast::<Vec<Out>>();
        sent.push(decode(event));
    }
    true
}

/// A host whose output list, the first time it is handed an event, stores a
/// value into a parameter: an edit that lands while the plugin is in the
/// middle of preparing a block.
struct Meddling {
    sent: Vec<Out>,
    params: Arc<PanelParams>,
    cutoff: Option<f32>,
}

unsafe extern "C" fn meddle(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        let meddling = &mut *(*list).ctx.cast::<Meddling>();
        if let Some(cutoff) = meddling.cutoff.take() {
            meddling.params.cutoff.set(cutoff);
        }
        meddling.sent.push(decode(event));
    }
    true
}

/// An output list with room for `room` more events, as a host's bounded
/// list is; once it is full, it refuses.
struct Room {
    sent: Vec<Out>,
    room: usize,
}

unsafe extern "C" fn record_while_room(
    list: *const clap_output_events,
    event: *const clap_event_header,
) -> bool {
    unsafe {
        let room = &mut *(*list).ctx.cast::<Room>();
        if room.room == 0 {
            return false;
        }
        room.room -= 1;
        room.sent.push(decode(event));
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
fn automation(param: ParamId, value: f64) -> clap_event_param_value {
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

const FRAMES: usize = 16;

/// Runs one block, as the audio thread. What the plugin sends is added to
/// `sent`, which must have room for it. Returns how many times the plugin
/// used the heap.
fn run_block(plugin: *const clap_plugin, sent: &mut Vec<Out>) -> usize {
    run_block_with(plugin, &[], sent)
}

/// Runs one block, as the audio thread, with `input` as the host's events.
/// What the plugin sends is added to `sent`, which must have room for it.
/// Returns how many times the plugin used the heap.
fn run_block_with(
    plugin: *const clap_plugin,
    input: &[clap_event_param_value],
    sent: &mut Vec<Out>,
) -> usize {
    let output = clap_output_events {
        ctx: ptr::from_mut(sent).cast(),
        try_push: Some(record_event),
    };
    run_block_raw(plugin, input, &raw const output)
}

/// Runs one block, as the audio thread, with `output` as the host's output
/// list. Returns how many times the plugin used the heap.
fn run_block_into(plugin: *const clap_plugin, output: &clap_output_events) -> usize {
    run_block_raw(plugin, &[], output)
}

/// Runs one block, as the audio thread, with `input` as the host's events
/// and `output` as its output list, which may be null. Returns how many
/// times the plugin used the heap.
fn run_block_raw(
    plugin: *const clap_plugin,
    input: &[clap_event_param_value],
    output: *const clap_output_events,
) -> usize {
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
    let process = clap_process {
        steady_time: -1,
        frames_count: FRAMES as u32,
        transport: ptr::null(),
        audio_inputs: ptr::null(),
        audio_outputs: outputs.as_mut_ptr(),
        audio_inputs_count: 0,
        audio_outputs_count: 1,
        in_events: &raw const input,
        out_events: output,
    };
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_PROCESS.with(|watching| watching.set(true));
    let status = unsafe { (*plugin).process.unwrap()(plugin, &raw const process) };
    IN_PROCESS.with(|watching| watching.set(false));
    assert_eq!(status, CLAP_PROCESS_CONTINUE);
    HEAP_CALLS.with(Cell::get)
}

/// What a host reads back from a state stream.
struct Reading {
    bytes: Vec<u8>,
    at: usize,
}

unsafe extern "C" fn stream_read(stream: *const clap_istream, data: *mut c_void, size: u64) -> i64 {
    unsafe {
        let reading = &mut *(*stream).ctx.cast::<Reading>();
        let rest = &reading.bytes[reading.at..];
        let count = rest.len().min(size as usize);
        ptr::copy_nonoverlapping(rest.as_ptr(), data.cast::<u8>(), count);
        reading.at += count;
        count as i64
    }
}

/// The state of a `Panel` whose parameters hold these values.
fn panel_state(cutoff: f32, mode: usize, gain: f32) -> String {
    let saved = panel_params();
    saved.cutoff.set(cutoff);
    saved.mode.set_index(mode);
    saved.gain.set(gain);
    save_state::<Panel>(&saved)
}

/// The plugin's `gui` extension, called as a host calls it.
#[derive(Clone, Copy)]
struct Ui {
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
}

impl Ui {
    fn of(plugin: *const clap_plugin) -> Self {
        let gui = unsafe {
            (*plugin).get_extension.unwrap()(plugin, CLAP_EXT_GUI.as_ptr())
                .cast::<clap_plugin_gui>()
        };
        Self { plugin, gui }
    }

    fn table(&self) -> &clap_plugin_gui {
        unsafe { self.gui.as_ref() }.expect("the plugin offers a gui")
    }

    fn is_api_supported(&self, api: *const c_char, is_floating: bool) -> bool {
        unsafe { self.table().is_api_supported.unwrap()(self.plugin, api, is_floating) }
    }

    fn create_raw(&self, api: *const c_char, is_floating: bool) -> bool {
        unsafe { self.table().create.unwrap()(self.plugin, api, is_floating) }
    }

    fn create(&self, api: &CStr) -> bool {
        self.create_raw(api.as_ptr(), false)
    }

    fn destroy(&self) {
        unsafe { self.table().destroy.unwrap()(self.plugin) }
    }

    fn set_scale(&self, scale: f64) -> bool {
        unsafe { self.table().set_scale.unwrap()(self.plugin, scale) }
    }

    fn size(&self) -> Option<HostSize> {
        let (mut width, mut height) = (0, 0);
        unsafe {
            self.table().get_size.unwrap()(self.plugin, &raw mut width, &raw mut height)
                .then_some(HostSize::new(width, height))
        }
    }

    fn can_resize(&self) -> bool {
        unsafe { self.table().can_resize.unwrap()(self.plugin) }
    }

    fn hints(&self) -> Option<clap_gui_resize_hints> {
        let mut hints = clap_gui_resize_hints {
            can_resize_horizontally: false,
            can_resize_vertically: false,
            preserve_aspect_ratio: false,
            aspect_ratio_width: 7,
            aspect_ratio_height: 7,
        };
        unsafe {
            self.table().get_resize_hints.unwrap()(self.plugin, &raw mut hints).then_some(hints)
        }
    }

    fn adjust(&self, size: HostSize) -> Option<HostSize> {
        let (mut width, mut height) = (size.width, size.height);
        unsafe {
            self.table().adjust_size.unwrap()(self.plugin, &raw mut width, &raw mut height)
                .then_some(HostSize::new(width, height))
        }
    }

    fn set_size(&self, size: HostSize) -> bool {
        unsafe { self.table().set_size.unwrap()(self.plugin, size.width, size.height) }
    }

    fn set_parent_raw(&self, window: *const clap_window) -> bool {
        unsafe { self.table().set_parent.unwrap()(self.plugin, window) }
    }

    /// Embeds the editor in the window `handle` of the API named `api`,
    /// writing the handle the way a host on that API does.
    fn set_parent(&self, api: *const c_char, handle: u64) -> bool {
        let named = (!api.is_null()).then(|| unsafe { CStr::from_ptr(api) });
        let specific = if named == Some(CLAP_WINDOW_API_X11) {
            clap_window_handle {
                x11: narrow(handle),
            }
        } else if named == Some(CLAP_WINDOW_API_COCOA) {
            clap_window_handle {
                cocoa: ptr::without_provenance_mut(handle as usize),
            }
        } else if named == Some(CLAP_WINDOW_API_WIN32) {
            clap_window_handle {
                win32: ptr::without_provenance_mut(handle as usize),
            }
        } else {
            clap_window_handle {
                ptr: ptr::without_provenance_mut(handle as usize),
            }
        };
        let window = clap_window { api, specific };
        self.set_parent_raw(&raw const window)
    }

    fn open(&self, api: &CStr, handle: u64) -> bool {
        self.set_parent(api.as_ptr(), handle)
    }

    fn show(&self) -> bool {
        unsafe { self.table().show.unwrap()(self.plugin) }
    }

    fn hide(&self) -> bool {
        unsafe { self.table().hide.unwrap()(self.plugin) }
    }

    /// The plugin's `on_main_thread`, as a host's loop calls it.
    fn on_main_thread(&self) {
        unsafe { (*self.plugin).on_main_thread.unwrap()(self.plugin) };
    }

    /// The plugin's `on_timer`, as a host's loop calls it.
    fn on_timer(&self, id: clap_id) {
        unsafe {
            let timers =
                (*self.plugin).get_extension.unwrap()(self.plugin, CLAP_EXT_TIMER_SUPPORT.as_ptr())
                    .cast::<clap_plugin_timer_support>();
            (*timers).on_timer.unwrap()(self.plugin, id);
        }
    }
}

/// A plugin created in a fake host, through the factory.
struct Session {
    entry: clap_plugin_entry,
    /// Owned through raw pointers: the plugin keeps pointers to both, and
    /// moving a `Box` would invalidate them (Miri says so).
    host: *mut clap_host,
    state: *mut HostState,
    plugin: *const clap_plugin,
    active: Cell<bool>,
    processing: Cell<bool>,
    /// The host destroyed the plugin itself, from inside a call.
    destroyed: Cell<bool>,
}

impl Session {
    fn new(entry: clap_plugin_entry, id: &CStr, offers: Offers) -> Self {
        Self::with_init(entry, id, offers, true)
    }

    fn panel() -> Self {
        Self::new(
            plugin_entry::<Panel>(),
            c"org.manguebyte.test.panel",
            EVERYTHING,
        )
    }

    fn with_init(entry: clap_plugin_entry, id: &CStr, offers: Offers, init: bool) -> Self {
        take_log();
        HOOK.with(|hook| hook.borrow_mut().take());
        PLUGIN_SAW.with(|saw| saw.set(None));
        let state = Box::into_raw(Box::new(HostState {
            offers,
            calls: Mutex::new(Vec::new()),
            plugin: AtomicPtr::new(ptr::null_mut()),
            on_resize: Mutex::new(None),
            on_closed: Mutex::new(None),
            refuse_registrations: AtomicBool::new(false),
            next_timer: AtomicU32::new(40),
            failures: Mutex::new(Vec::new()),
        }));
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: state.cast(),
            name: c"gui test host".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_nothing),
            request_process: Some(host_nothing),
            request_callback: if offers.callbacks {
                Some(host_request_callback)
            } else {
                None
            },
        }));
        unsafe {
            assert!(entry.init.unwrap()(c"/plugins/test.clap".as_ptr()));
            let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>();
            let plugin = (*factory).create_plugin.unwrap()(factory, host, id.as_ptr());
            assert!(!plugin.is_null());
            if init {
                assert!((*plugin).init.unwrap()(plugin));
            }
            (*state).plugin.store(plugin.cast_mut(), Ordering::Release);
            Self {
                entry,
                host,
                state,
                plugin,
                active: Cell::new(false),
                processing: Cell::new(false),
                destroyed: Cell::new(false),
            }
        }
    }

    fn state(&self) -> &HostState {
        unsafe { &*self.state }
    }

    fn ui(&self) -> Ui {
        Ui::of(self.plugin)
    }

    fn extension(&self, id: &CStr) -> *const c_void {
        unsafe { (*self.plugin).get_extension.unwrap()(self.plugin, id.as_ptr()) }
    }

    /// Every call the plugin made to the host since the last time, after
    /// checking that the test's replies inside the host did not fail.
    fn take_calls(&self) -> Vec<HostCall> {
        let failures = std::mem::take(&mut *self.state().failures.lock().unwrap());
        assert_eq!(failures, Vec::<String>::new(), "inside the host");
        std::mem::take(&mut *self.state().calls.lock().unwrap())
    }

    /// Destroys the plugin as a host does, and returns the calls the plugin
    /// made to the host while it was being destroyed.
    fn destroy(self) -> Vec<HostCall> {
        let session = std::mem::ManuallyDrop::new(self);
        // SAFETY: `session` is never used or dropped again.
        unsafe { session.tear_down() }
    }

    /// Stops, deactivates and destroys the plugin, then frees the host.
    ///
    /// # Safety
    ///
    /// Only once, after which the session must not be used.
    unsafe fn tear_down(&self) -> Vec<HostCall> {
        unsafe {
            let mut calls = Vec::new();
            // A plugin the host destroyed already is gone.
            if !self.destroyed.get() {
                if self.processing.get() {
                    (*self.plugin).stop_processing.unwrap()(self.plugin);
                }
                if self.active.get() {
                    (*self.plugin).deactivate.unwrap()(self.plugin);
                }
                let before = self.state().calls.lock().unwrap().len();
                (*self.plugin).destroy.unwrap()(self.plugin);
                calls = self.state().calls.lock().unwrap().split_off(before);
            }
            let failures = std::mem::take(&mut *self.state().failures.lock().unwrap());
            self.entry.deinit.unwrap()();
            drop(Box::from_raw(self.host));
            drop(Box::from_raw(self.state));
            if !std::thread::panicking() {
                assert_eq!(failures, Vec::<String>::new(), "inside the host");
            }
            calls
        }
    }

    fn activate(&self) {
        unsafe {
            assert!((*self.plugin).activate.unwrap()(
                self.plugin,
                48_000.0,
                1,
                64
            ))
        };
        self.active.set(true);
    }

    fn start(&self) {
        if !self.active.get() {
            self.activate();
        }
        unsafe { assert!((*self.plugin).start_processing.unwrap()(self.plugin)) };
        self.processing.set(true);
    }

    fn stop(&self) {
        unsafe { (*self.plugin).stop_processing.unwrap()(self.plugin) };
        self.processing.set(false);
    }

    fn process(&self) -> Vec<Out> {
        self.process_with(&[])
    }

    /// Runs one block with `input` as the host's events.
    fn process_with(&self, input: &[clap_event_param_value]) -> Vec<Out> {
        self.block(input, usize::MAX)
    }

    /// Runs one block whose output list takes at most `room` events.
    fn process_with_room(&self, room: usize) -> Vec<Out> {
        self.block(&[], room)
    }

    /// Runs one block with `input` as the host's events and an output list
    /// that takes at most `room` events.
    fn block(&self, input: &[clap_event_param_value], room: usize) -> Vec<Out> {
        let mut room = Room {
            sent: Vec::with_capacity(256),
            room,
        };
        let output = clap_output_events {
            ctx: (&raw mut room).cast(),
            try_push: Some(record_while_room),
        };
        assert_eq!(
            run_block_raw(self.plugin, input, &raw const output),
            0,
            "process used the heap"
        );
        room.sent
    }

    fn deactivate(&self) {
        unsafe { (*self.plugin).deactivate.unwrap()(self.plugin) };
        self.active.set(false);
    }

    /// Runs one block with no output list at all.
    fn process_without_a_list(&self) {
        assert_eq!(
            run_block_raw(self.plugin, &[], ptr::null()),
            0,
            "process used the heap"
        );
    }

    /// Calls `params.flush` with `input` as the host's events and an output
    /// list that takes at most `room` events, watching the heap as `process`
    /// is watched: `flush` runs on the audio thread while the plugin is
    /// active.
    fn flush_with(&self, input: &[clap_event_param_value], room: usize) -> Vec<Out> {
        let mut room = Room {
            sent: Vec::with_capacity(256),
            room,
        };
        let events: &[clap_event_param_value] = input;
        let input = clap_input_events {
            ctx: (&raw const events).cast_mut().cast(),
            size: Some(input_size),
            get: Some(input_get),
        };
        let output = clap_output_events {
            ctx: (&raw mut room).cast(),
            try_push: Some(record_while_room),
        };
        let params = unsafe {
            self.extension(CLAP_EXT_PARAMS)
                .cast::<clap_plugin_params>()
                .as_ref()
                .unwrap()
        };
        let flush = params.flush.unwrap();
        HEAP_CALLS.with(|calls| calls.set(0));
        IN_PROCESS.with(|watching| watching.set(true));
        unsafe { flush(self.plugin, &raw const input, &raw const output) };
        IN_PROCESS.with(|watching| watching.set(false));
        assert_eq!(HEAP_CALLS.with(Cell::get), 0, "flush used the heap");
        room.sent
    }

    fn flush(&self) -> Vec<Out> {
        self.flush_with(&[], usize::MAX)
    }

    /// The value the host reads for `param` through `params.get_value`.
    fn value(&self, param: ParamId) -> f64 {
        let mut value = f64::NAN;
        unsafe {
            let params = self
                .extension(CLAP_EXT_PARAMS)
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

    /// Loads `text` through the `state` extension, as a host restoring a
    /// session does.
    fn load_state(&self, text: &str) -> bool {
        let mut reading = Reading {
            bytes: text.as_bytes().to_vec(),
            at: 0,
        };
        let stream = clap_istream {
            ctx: (&raw mut reading).cast(),
            read: Some(stream_read),
        };
        unsafe {
            let state = self
                .extension(CLAP_EXT_STATE)
                .cast::<clap_plugin_state>()
                .as_ref()
                .unwrap();
            state.load.unwrap()(self.plugin, &raw const stream)
        }
    }

    fn on_main_thread(&self) {
        unsafe { (*self.plugin).on_main_thread.unwrap()(self.plugin) };
    }

    fn on_timer(&self, id: clap_id) {
        unsafe {
            let timers = self
                .extension(CLAP_EXT_TIMER_SUPPORT)
                .cast::<clap_plugin_timer_support>();
            (*timers).on_timer.unwrap()(self.plugin, id);
        }
    }

    fn on_fd(&self, fd: i32, flags: clap_posix_fd_flags) {
        unsafe {
            let fds = self
                .extension(CLAP_EXT_POSIX_FD_SUPPORT)
                .cast::<clap_plugin_posix_fd_support>();
            (*fds).on_fd.unwrap()(self.plugin, fd, flags);
        }
    }

    /// Answers the plugin's resize requests with `reply` from now on.
    fn on_resize(&self, reply: impl FnMut(Ui, u32, u32) -> bool + Send + 'static) {
        *self.state().on_resize.lock().unwrap() = Some(Box::new(reply));
    }

    fn on_closed(&self, reply: impl FnMut(Ui) + Send + 'static) {
        *self.state().on_closed.lock().unwrap() = Some(Box::new(reply));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: dropped once, and never used again.
        unsafe { self.tear_down() };
    }
}

const PARENT: u64 = 0x0004_05A2;

fn win32_parent() -> ParentWindow {
    ParentWindow::win32(PARENT as usize).unwrap()
}

/// Checks that the events alternate begin and end on each parameter and
/// that every value is sent inside a gesture, and returns how many gestures
/// were opened. `open` carries the state from one call to the next.
fn check_nesting(events: &[Out], open: &mut Vec<ParamId>) -> usize {
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
                    "event {index}: {event:?} outside a gesture, after {:?}",
                    &events[index.saturating_sub(12)..index]
                );
                assert!((0.0..=2.0).contains(&value), "event {index}: {event:?}");
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

// --- The tests -----------------------------------------------------------

#[test]
fn a_plugin_without_an_editor_offers_no_gui_and_reports_as_before() {
    let session = Session::new(
        plugin_entry::<Bare>(),
        c"org.manguebyte.test.bare",
        EVERYTHING,
    );
    for extension in [
        CLAP_EXT_GUI,
        CLAP_EXT_TIMER_SUPPORT,
        CLAP_EXT_POSIX_FD_SUPPORT,
    ] {
        assert!(
            session.extension(extension).is_null(),
            "{extension:?} offered"
        );
    }
    session.start();
    // A change from outside the host is still reported, as a value alone.
    params().cutoff.set(50.0);
    assert_eq!(session.process(), [Out::Value(CUTOFF, 0.5)]);
    assert_eq!(session.process(), []);
    session.stop();
    session.on_main_thread();
    // Flush now reports too: a change while not processing goes out there.
    params().mode.set_index(2);
    assert_eq!(session.flush(), [Out::Value(MODE, 2.0)]);
    assert_eq!(session.flush(), []);
    // And the host is never asked for anything.
    assert_eq!(session.take_calls(), []);
}

#[test]
fn a_plugin_with_an_editor_offers_the_gui_and_the_loop_callbacks() {
    set_up(Setup::default());
    let session = Session::panel();
    for extension in [
        CLAP_EXT_GUI,
        CLAP_EXT_TIMER_SUPPORT,
        CLAP_EXT_POSIX_FD_SUPPORT,
    ] {
        assert!(
            !session.extension(extension).is_null(),
            "{extension:?} missing"
        );
    }
    // Asking changes nothing: no host call, no editor call.
    assert_eq!(session.take_calls(), []);
    assert_eq!(take_log(), []);
}

#[test]
fn the_usual_call_order_opens_shows_hides_and_closes_the_editor() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    for _ in 0..2 {
        // The order CLAP's gui.h gives for an embedded window.
        assert!(ui.is_api_supported(CLAP_WINDOW_API_WIN32.as_ptr(), false));
        assert!(ui.create(CLAP_WINDOW_API_WIN32));
        assert!(ui.set_scale(1.5));
        assert!(ui.can_resize());
        assert_eq!(ui.size(), Some(HostSize::new(450, 300)));
        assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
        assert!(ui.show());
        assert_eq!(ui.size(), Some(HostSize::new(450, 300)));
        assert!(ui.hide());
        ui.destroy();
        assert_eq!(
            take_log(),
            [
                EditorCall::Open(win32_parent(), Some(1.5)),
                EditorCall::SetVisible(true),
                EditorCall::Size,
                EditorCall::SetVisible(false),
                EditorCall::Dropped,
            ]
        );
        assert!(!probe_alive(), "the editor outlived destroy");
    }
    // Showing an editor asks the host for nothing.
    assert_eq!(session.take_calls(), []);
}

#[test]
fn calls_out_of_order_or_with_nonsense_return_false() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();

    // Nothing created yet.
    assert!(!ui.set_scale(1.0));
    assert_eq!(ui.size(), None);
    assert!(!ui.can_resize());
    assert!(ui.hints().is_none());
    assert_eq!(ui.adjust(HostSize::new(400, 300)), None);
    assert!(!ui.set_size(HostSize::new(300, 200)));
    assert!(!ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(!ui.show());
    assert!(!ui.hide());
    ui.destroy();
    ui.destroy();

    // Floating windows, unknown and missing APIs.
    assert!(!ui.is_api_supported(CLAP_WINDOW_API_WIN32.as_ptr(), true));
    assert!(!ui.create_raw(CLAP_WINDOW_API_WIN32.as_ptr(), true));
    for api in [CLAP_WINDOW_API_WAYLAND, c"", c"win32 ", c"WIN32"] {
        assert!(!ui.is_api_supported(api.as_ptr(), false), "{api:?}");
        assert!(!ui.create(api), "{api:?}");
    }
    assert!(!ui.is_api_supported(ptr::null(), false));
    assert!(!ui.create_raw(ptr::null(), false));
    unsafe {
        let table = ui.table();
        assert!(!table.set_transient.unwrap()(ui.plugin, ptr::null()));
        table.suggest_title.unwrap()(ui.plugin, c"Panel".as_ptr());
        // A null plugin.
        assert!(!table.create.unwrap()(
            ptr::null(),
            CLAP_WINDOW_API_WIN32.as_ptr(),
            false
        ));
        assert!(!table.show.unwrap()(ptr::null()));
        table.destroy.unwrap()(ptr::null());
        // Null places to write to.
        assert!(ui.create(CLAP_WINDOW_API_WIN32));
        assert!(!table.get_size.unwrap()(
            ui.plugin,
            ptr::null_mut(),
            ptr::null_mut()
        ));
        assert!(!table.adjust_size.unwrap()(
            ui.plugin,
            ptr::null_mut(),
            ptr::null_mut()
        ));
        assert!(!table.get_resize_hints.unwrap()(ui.plugin, ptr::null_mut()));
        assert!(!table.get_preferred_api.unwrap()(
            ui.plugin,
            ptr::null_mut(),
            ptr::null_mut()
        ));
    }

    // Created once only.
    assert!(!ui.create(CLAP_WINDOW_API_WIN32));
    // Hostile scales.
    for scale in [
        0.0,
        -1.0,
        0.1,
        8.5,
        100.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        assert!(!ui.set_scale(scale), "{scale}");
    }
    // Hostile parents: none, another API, a null handle, a nameless API.
    assert!(!ui.set_parent_raw(ptr::null()));
    assert!(!ui.open(CLAP_WINDOW_API_X11, PARENT));
    assert!(!ui.open(CLAP_WINDOW_API_WIN32, 0));
    assert!(!ui.set_parent(ptr::null(), PARENT));
    assert!(!ui.set_parent(CLAP_WINDOW_API_WAYLAND.as_ptr(), PARENT));
    assert_eq!(take_log(), [], "the editor was opened by a bad call");

    // Opened once only.
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(!ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(!ui.create(CLAP_WINDOW_API_WIN32));
    ui.destroy();
    assert_eq!(
        take_log(),
        [EditorCall::Open(win32_parent(), None), EditorCall::Dropped]
    );
    // Still usable after all that.
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, 0x0060_0001));
    assert_eq!(
        take_log(),
        [EditorCall::Open(
            ParentWindow::x11(0x0060_0001).unwrap(),
            None
        )]
    );
}

/// `get_size` and `adjust_size` take two places to write to, and C lets a
/// host pass the same place for both. The wrapper writes through the
/// pointers, one after the other, and never makes two `&mut` of one place,
/// which would be undefined behaviour; Miri, which CI runs on this crate,
/// checks that.
#[test]
fn one_place_for_both_sides_is_written_without_undefined_behaviour() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    let table = ui.table();
    let mut side = 0_u32;
    let place = &raw mut side;
    unsafe {
        assert!(table.get_size.unwrap()(ui.plugin, place, place));
        // The width is written first, then the height over it.
        assert_eq!(place.read(), 200);
        // Both sides are read before either is written: 500 by 500 fits.
        place.write(500);
        assert!(table.adjust_size.unwrap()(ui.plugin, place, place));
        assert_eq!(place.read(), 500);
        // 5000 by 5000 becomes 1200 by 800; the height is written last.
        place.write(5_000);
        assert!(table.adjust_size.unwrap()(ui.plugin, place, place));
        assert_eq!(place.read(), 800);
    }
    assert_eq!(side, 800);
}

#[test]
fn the_gui_needs_init_first() {
    set_up(Setup::default());
    let session = Session::with_init(
        plugin_entry::<Panel>(),
        c"org.manguebyte.test.panel",
        EVERYTHING,
        false,
    );
    let ui = session.ui();
    assert!(!ui.create(CLAP_WINDOW_API_WIN32));
    assert!(!ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    unsafe { assert!((*session.plugin).init.unwrap()(session.plugin)) };
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
}

#[test]
fn an_editor_that_does_not_support_an_api_is_not_offered_it() {
    set_up(Setup {
        supports: [true, false, false],
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.is_api_supported(CLAP_WINDOW_API_WIN32.as_ptr(), false));
    assert!(!ui.is_api_supported(CLAP_WINDOW_API_COCOA.as_ptr(), false));
    assert!(!ui.is_api_supported(CLAP_WINDOW_API_X11.as_ptr(), false));
    assert!(!ui.create(CLAP_WINDOW_API_X11));

    let (mut api, mut floating): (*const c_char, bool) = (ptr::null(), true);
    let table = ui.table();
    let preferred =
        unsafe { table.get_preferred_api.unwrap()(ui.plugin, &raw mut api, &raw mut floating) };
    // Only Windows is supported, so only a Windows build prefers an API.
    assert_eq!(preferred, cfg!(target_os = "windows"));
    if preferred {
        assert_eq!(unsafe { CStr::from_ptr(api) }, CLAP_WINDOW_API_WIN32);
        assert!(!floating);
    }
}

#[test]
fn the_preferred_api_is_the_one_of_this_platform() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    let (mut api, mut floating): (*const c_char, bool) = (ptr::null(), true);
    let table = ui.table();
    assert!(unsafe {
        table.get_preferred_api.unwrap()(ui.plugin, &raw mut api, &raw mut floating)
    });
    let expected = if cfg!(target_os = "windows") {
        CLAP_WINDOW_API_WIN32
    } else if cfg!(target_os = "macos") {
        CLAP_WINDOW_API_COCOA
    } else {
        CLAP_WINDOW_API_X11
    };
    // CLAP wants one of its own constants, not a copy, so that the pointer
    // stays valid for as long as the plugin is loaded: it is `'static`.
    assert_eq!(unsafe { CStr::from_ptr(api) }, expected);
    assert!(!floating);
}

#[test]
fn sizes_are_in_the_hosts_pixels_and_follow_its_scale() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();

    // Win32: physical pixels, scaled by what the host says.
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    assert!(ui.set_scale(2.0));
    assert_eq!(ui.size(), Some(HostSize::new(600, 400)));
    // Limits are scaled too: 150x100 to 1200x800 at 1, doubled at 2.
    assert_eq!(
        ui.adjust(HostSize::new(10, 10)),
        Some(HostSize::new(300, 200))
    );
    assert_eq!(
        ui.adjust(HostSize::new(9_999, 9_999)),
        Some(HostSize::new(2400, 1600))
    );
    assert_eq!(
        ui.hints().map(|hints| (
            hints.can_resize_horizontally,
            hints.can_resize_vertically,
            hints.preserve_aspect_ratio,
            hints.aspect_ratio_width,
            hints.aspect_ratio_height
        )),
        Some((true, true, false, 0, 0))
    );
    // A size set before the window exists is kept for it, if it fits.
    assert!(
        !ui.set_size(HostSize::new(250, 200)),
        "below the minimum at 2x"
    );
    assert!(ui.set_size(HostSize::new(700, 500)));
    assert_eq!(ui.size(), Some(HostSize::new(700, 500)));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(ui.size(), Some(HostSize::new(700, 500)));
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32_parent(), Some(2.0)),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(700, 500), true),
            EditorCall::Size,
        ]
    );
    // Once open, the editor answers; a new scale goes to it.
    assert_eq!(
        ui.adjust(HostSize::new(5_000, 1)),
        Some(HostSize::new(2400, 200))
    );
    assert!(ui.set_scale(1.25));
    assert_eq!(ui.size(), Some(HostSize::new(375, 250)));
    assert!(!ui.set_scale(0.0));
    assert_eq!(
        take_log(),
        [
            EditorCall::Fit(HostSize::new(5_000, 1)),
            EditorCall::SetScale(1.25),
            EditorCall::Size,
        ]
    );
    ui.destroy();
    take_log();

    // Cocoa: points, which the system scales; the host's scale is refused
    // and the editor opens without one.
    assert!(ui.create(CLAP_WINDOW_API_COCOA));
    assert!(!ui.set_scale(2.0));
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    assert!(ui.open(CLAP_WINDOW_API_COCOA, PARENT));
    assert_eq!(
        take_log(),
        [EditorCall::Open(
            ParentWindow::cocoa(PARENT as usize).unwrap(),
            None
        )]
    );
    ui.destroy();
    take_log();
    drop(session);

    // An editor that cannot take a new scale says so, and the old one
    // stays.
    set_up(Setup {
        accept_scale: false,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    assert!(!ui.set_scale(2.0));
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    ui.destroy();
}

/// CLAP's order is create, set_scale, get_size or set_size, set_parent,
/// show. An editor given no scale may find the system's and ask for its
/// size from inside `open`, as the editor contract allows. When the host
/// grants it, that is the size of the host's window: a size the host set
/// before the window existed is out of date and is not forced on the editor
/// afterwards.
#[test]
fn a_size_the_editor_asked_for_while_opening_is_not_undone() {
    set_up(Setup {
        resize_on_open: Some(HostSize::new(450, 300)),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    // The host has no scale to give and sets the size it read.
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    assert!(ui.set_size(HostSize::new(300, 200)));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(session.take_calls(), [HostCall::RequestResize(450, 300)]);
    assert_eq!(
        ui.size(),
        Some(HostSize::new(450, 300)),
        "the editor no longer has the size the host granted it"
    );
    assert_eq!(
        take_log(),
        [EditorCall::Open(win32_parent(), None), EditorCall::Size]
    );
    ui.destroy();
    take_log();

    // A host that refuses keeps its window at the size it set, and the
    // editor is given that size once it is open.
    session.on_resize(|_, _, _| false);
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.set_size(HostSize::new(600, 400)));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(session.take_calls(), [HostCall::RequestResize(450, 300)]);
    assert_eq!(ui.size(), Some(HostSize::new(600, 400)));
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32_parent(), None),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(600, 400), true),
            EditorCall::Size,
        ]
    );
}

/// A size the host set before the window existed, which the open editor
/// refuses: the host is asked to make its window the editor's size, so
/// that the two agree.
#[test]
fn a_preset_size_the_editor_refuses_gives_way_to_the_editors_own() {
    set_up(Setup {
        refuse_sizes: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    // The spec's rules accept it, so the wrapper keeps it.
    assert!(ui.set_size(HostSize::new(700, 500)));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32_parent(), None),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(700, 500), false),
        ]
    );
    assert_eq!(session.take_calls(), [HostCall::RequestResize(300, 200)]);
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    ui.destroy();
    take_log();

    // A host that answers with yet another size: that size is offered to
    // the editor, once.
    session.on_resize(|ui, _, _| ui.set_size(HostSize::new(320, 240)));
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.set_size(HostSize::new(700, 500)));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32_parent(), None),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(700, 500), false),
            EditorCall::SetSize(HostSize::new(320, 240), false),
        ]
    );
    assert_eq!(session.take_calls(), [HostCall::RequestResize(300, 200)]);
}

#[test]
fn fixed_and_shaped_editors_tell_the_host_how_they_resize() {
    set_up(Setup {
        resize: Resize::Fixed,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(!ui.can_resize());
    assert_eq!(ui.adjust(HostSize::new(400, 400)), None);
    assert!(!ui.set_size(HostSize::new(301, 200)));
    assert!(ui.set_size(HostSize::new(300, 200)));
    let hints = ui.hints().unwrap();
    assert!(!hints.can_resize_horizontally && !hints.can_resize_vertically);
    drop(session);

    set_up(Setup {
        base: HostSize::new(320, 180),
        resize: Resize::Ratio {
            min: HostSize::new(160, 90),
            max: HostSize::new(1600, 900),
            width: 16,
            height: 9,
        },
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    let hints = ui.hints().unwrap();
    assert!(hints.preserve_aspect_ratio);
    assert_eq!(
        (hints.aspect_ratio_width, hints.aspect_ratio_height),
        (16, 9)
    );
    assert_eq!(
        ui.adjust(HostSize::new(1000, 1000)),
        Some(HostSize::new(1000, 563))
    );
    assert!(ui.set_size(HostSize::new(1000, 563)));
    assert!(!ui.set_size(HostSize::new(1000, 1000)));
}

#[test]
fn gestures_and_values_reach_the_host_in_order_from_process() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    session.take_calls();

    // A drag across three blocks.
    with_probe(|probe| probe.drag(CUTOFF, 0.25));
    with_probe(|probe| probe.move_to(0.5));
    // The interface sees the value at once.
    assert_eq!(params().cutoff.value(), 50.0);
    assert_eq!(
        session.process(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.5)]
    );
    with_probe(|probe| probe.move_to(0.6));
    with_probe(|probe| probe.move_to(0.75));
    assert_eq!(session.process(), [Out::Value(CUTOFF, 0.75)]);
    assert_eq!(session.process(), []);
    with_probe(|probe| probe.release());
    assert_eq!(session.process(), [Out::End(CUTOFF)]);

    // A whole drag between two blocks, and two controls at once: each
    // parameter in the host's order, begin, value, end.
    with_probe(|probe| {
        probe.drag(GAIN, 0.5);
        probe.move_to(0.25);
        probe.release();
        probe.drag(CUTOFF, 0.75);
        probe.move_to(1.0);
        probe.drag(MODE, 0.0);
        probe.move_to(1.0);
    });
    assert_eq!(
        session.process(),
        [
            Out::Begin(CUTOFF),
            Out::Value(CUTOFF, 1.0),
            Out::Begin(MODE),
            Out::Value(MODE, 2.0),
            Out::Begin(GAIN),
            Out::Value(GAIN, 0.25),
            Out::End(GAIN),
        ]
    );
    with_probe(|probe| {
        probe.release();
        probe.release();
    });
    assert_eq!(session.process(), [Out::End(CUTOFF), Out::End(MODE)]);

    // A click on a switch: one edit in one call.
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, MODE, 0.5)));
    assert_eq!(
        session.process(),
        [Out::Begin(MODE), Out::Value(MODE, 1.0), Out::End(MODE)]
    );
    // A change from elsewhere is still a value alone.
    params().gain.set(0.75);
    assert_eq!(session.process(), [Out::Value(GAIN, 0.75)]);

    // Escape during a drag puts the starting value back. Between two blocks
    // the host sees a gesture that changed nothing...
    with_probe(|probe| {
        probe.drag(CUTOFF, 1.0);
        probe.move_to(0.0);
        let gesture = probe.gestures.borrow_mut().pop().unwrap();
        probe.out(|_| gesture.cancel());
    });
    assert_eq!(session.process(), [Out::Begin(CUTOFF), Out::End(CUTOFF)]);
    // ...and across blocks, the value going and coming back.
    with_probe(|probe| {
        probe.drag(CUTOFF, 1.0);
        probe.move_to(0.0);
    });
    assert_eq!(
        session.process(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.0)]
    );
    with_probe(|probe| {
        let gesture = probe.gestures.borrow_mut().pop().unwrap();
        probe.out(|_| gesture.cancel());
    });
    assert_eq!(
        session.process(),
        [Out::Value(CUTOFF, 1.0), Out::End(CUTOFF)]
    );

    // While the host processes, it is never asked for a flush.
    assert_eq!(session.take_calls(), []);
    ui.destroy();
}

/// CLAP lets a host's output list refuse an event when it is full. What it
/// refuses is sent again on the next block or flush, in order, so the host
/// never gets an end without its begin or a value outside its gesture, and
/// in the end has every gesture whole and the last value.
#[test]
fn events_the_hosts_list_refuses_are_sent_again_in_order() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    // A block whose list refuses everything, and one with no list at all.
    assert_eq!(session.process_with_room(0), []);
    session.process_without_a_list();
    // Then blocks with room.
    let mut received = session.process();
    with_probe(|probe| probe.release());
    received.extend(session.process());
    received.extend(session.process());
    assert_eq!(
        received,
        [
            Out::Begin(CUTOFF),
            Out::Value(CUTOFF, 0.5),
            Out::End(CUTOFF)
        ],
        "the host did not receive the gesture whole"
    );

    // Room for one event per block: each block sends the next one.
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.5);
        probe.move_to(0.75);
        probe.drag(GAIN, 0.5);
        probe.move_to(0.25);
        probe.release();
    });
    let blocks: Vec<Vec<Out>> = (0..6).map(|_| session.process_with_room(1)).collect();
    assert_eq!(
        blocks,
        [
            vec![Out::Begin(CUTOFF)],
            vec![Out::Value(CUTOFF, 0.75)],
            vec![Out::Begin(GAIN)],
            vec![Out::Value(GAIN, 0.25)],
            vec![Out::End(GAIN)],
            vec![],
        ]
    );
    // An end that is refused waits too, in `flush` as in `process`.
    with_probe(|probe| probe.release());
    session.stop();
    assert_eq!(session.flush_with(&[], 0), []);
    assert_eq!(session.flush(), [Out::End(CUTOFF)]);
    assert_eq!(session.flush(), []);
    assert_eq!(
        [CUTOFF, MODE, GAIN].map(|param| session.value(param)),
        [0.75, 0.0, 0.25]
    );
    ui.destroy();
    drop(session);

    // A plugin without an editor: a value the list refuses is sent later.
    let session = Session::new(
        plugin_entry::<Bare>(),
        c"org.manguebyte.test.bare",
        EVERYTHING,
    );
    session.start();
    params().gain.set(0.75);
    assert_eq!(session.process_with_room(0), []);
    assert_eq!(session.process(), [Out::Value(GAIN, 0.75)]);
    assert_eq!(session.process(), []);
}

/// The plugin reads the parameters at the start of a block to announce
/// changes to itself, and stores the host's values a little later. An edit
/// stored in between must not be undone by writing the announced value
/// back, and must be announced in the next block.
#[test]
fn an_edit_stored_while_a_block_is_prepared_is_kept_and_sent_on() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    // The host's list is handed the gesture's begin after the plugin has
    // read the parameters for this block and before it stores the host's
    // values: the next move of the drag lands right there.
    let mut meddling = Meddling {
        sent: Vec::with_capacity(16),
        params: params(),
        cutoff: Some(75.0),
    };
    let output = clap_output_events {
        ctx: (&raw mut meddling).cast(),
        try_push: Some(meddle),
    };
    assert_eq!(run_block_into(session.plugin, &output), 0);
    assert_eq!(meddling.sent, [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.5)]);
    assert_eq!(params().cutoff.value(), 75.0, "the edit was undone");
    // The next block sends it, inside the gesture.
    assert_eq!(session.process(), [Out::Value(CUTOFF, 0.75)]);
    with_probe(|probe| probe.release());
    assert_eq!(session.process(), [Out::End(CUTOFF)]);
    ui.destroy();
}

/// Runs `work` as another thread would, unseen by the heap watcher: the
/// work of the main thread that lands while the audio side is busy.
fn elsewhere(work: impl FnOnce()) {
    let watching = IN_PROCESS.with(|watching| watching.replace(false));
    work();
    IN_PROCESS.with(|watched| watched.set(watching));
}

/// The plugin stores the host's value for a parameter and records what the
/// host knows and what the plugin has seen without reading the parameter
/// back. An edit the editor stores right after, from the main thread while
/// the block is prepared, stays news to both, and both are told on the
/// next block.
#[test]
fn an_edit_landing_as_the_hosts_value_is_stored_reaches_the_host_and_the_plugin() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    assert_eq!(
        session.process(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.5)]
    );
    assert_eq!(PLUGIN_SAW.with(Cell::get), Some(50.0));

    // The host plays an automation point at 0.25; the person's next move
    // lands right after the plugin stores it.
    after_next_store(|| elsewhere(|| with_probe(|probe| probe.move_to(0.75))));
    assert_eq!(session.process_with(&[automation(CUTOFF, 0.25)]), []);
    assert!(HOOK.with(|hook| hook.borrow().is_none()), "the hook ran");
    assert_eq!(params().cutoff.value(), 75.0, "the edit was stored last");
    assert_eq!(PLUGIN_SAW.with(Cell::get), Some(25.0));

    // The next block tells both.
    assert_eq!(session.process(), [Out::Value(CUTOFF, 0.75)]);
    assert_eq!(PLUGIN_SAW.with(Cell::get), Some(75.0));
    with_probe(|probe| probe.release());
    assert_eq!(session.process(), [Out::End(CUTOFF)]);
    assert_eq!(session.value(CUTOFF), 0.75);
}

/// The same through `flush`, which stores the host's values too.
#[test]
fn an_edit_landing_as_a_flushed_value_is_stored_reaches_the_host() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    assert_eq!(
        session.flush(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.5)]
    );
    after_next_store(|| elsewhere(|| with_probe(|probe| probe.move_to(0.75))));
    assert_eq!(
        session.flush_with(&[automation(CUTOFF, 0.25)], usize::MAX),
        []
    );
    assert_eq!(params().cutoff.value(), 75.0, "the edit was stored last");
    assert_eq!(session.flush(), [Out::Value(CUTOFF, 0.75)]);
    with_probe(|probe| probe.release());
    assert_eq!(session.flush(), [Out::End(CUTOFF)]);
    assert_eq!(session.value(CUTOFF), 0.75);
}

#[test]
fn gestures_and_values_reach_the_host_from_flush_when_it_is_not_processing() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));

    // Not even activated: each edit asks for a flush.
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestFlush, HostCall::RequestFlush]
    );
    assert_eq!(
        session.flush(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.5)]
    );
    assert_eq!(session.flush(), []);
    with_probe(|probe| probe.release());
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_eq!(session.flush(), [Out::End(CUTOFF)]);

    // Activated but not processing: the same.
    session.activate();
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 1.0)));
    assert_eq!(session.take_calls(), [HostCall::RequestFlush; 3]);
    assert_eq!(
        session.flush(),
        [Out::Begin(GAIN), Out::Value(GAIN, 1.0), Out::End(GAIN)]
    );

    // And when processing starts, a gesture still open there carries on.
    with_probe(|probe| probe.drag(MODE, 0.0));
    assert_eq!(session.flush(), [Out::Begin(MODE)]);
    session.start();
    with_probe(|probe| probe.move_to(1.0));
    assert_eq!(session.process(), [Out::Value(MODE, 2.0)]);
    with_probe(|probe| probe.release());
    assert_eq!(session.process(), [Out::End(MODE)]);
    ui.destroy();
}

#[test]
fn an_edit_as_processing_stops_is_flushed_through_the_main_thread() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));

    // The edit happens while the host processes, so no flush is asked for,
    // and processing stops before another block.
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, CUTOFF, 0.5)));
    assert_eq!(session.take_calls(), []);
    session.stop();
    // The audio thread may not ask for a flush, so it asks for the main
    // thread, which does.
    assert_eq!(session.take_calls(), [HostCall::RequestCallback]);
    session.on_main_thread();
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_eq!(
        session.flush(),
        [
            Out::Begin(CUTOFF),
            Out::Value(CUTOFF, 0.5),
            Out::End(CUTOFF)
        ]
    );
    // With nothing waiting, neither is asked for.
    session.on_main_thread();
    session.start();
    session.stop();
    assert_eq!(session.take_calls(), []);
    assert_eq!(take_log().last(), Some(&EditorCall::OnMainThread));
    ui.destroy();
}

/// The plugin asks for `on_main_thread` when it needs it: for an edit left
/// to flush when processing stopped, or for the editor's waker. A host may
/// make that call from a loop it runs while the editor is calling it. The
/// call is then put off, not used up: once the editor is back, the plugin
/// asks for another, which does the work.
#[test]
fn a_call_back_that_comes_while_the_editor_is_calling_out_is_not_used_up() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    // An edit while the host processes asks for nothing; then processing
    // stops, and the plugin asks for a call on the main thread.
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 0.75)));
    session.stop();
    assert_eq!(session.take_calls(), [HostCall::RequestCallback]);
    // The host makes that call while the editor asks it for a size.
    session.on_resize(|ui, _, _| {
        ui.on_main_thread();
        true
    });
    take_log();
    with_probe(|probe| probe.request_resize(HostSize::new(400, 300)));
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestResize(400, 300), HostCall::RequestCallback],
        "the edit is stuck until something else happens"
    );
    assert_eq!(take_log(), [], "the editor was called while calling out");
    // The call that comes next does the work: a flush, and the editor.
    session.on_main_thread();
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_eq!(take_log(), [EditorCall::OnMainThread]);
    assert_eq!(
        session.flush(),
        [Out::Begin(GAIN), Out::Value(GAIN, 0.75), Out::End(GAIN)]
    );

    // The editor's own wake-up, answered the same way.
    let waker = with_probe(|probe| probe.context.main_thread_waker()).expect("a waker");
    assert!(waker.wake());
    assert_eq!(session.take_calls(), [HostCall::RequestCallback]);
    with_probe(|probe| probe.request_resize(HostSize::new(500, 300)));
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestResize(500, 300), HostCall::RequestCallback],
        "the editor's wake-up was used up"
    );
    assert_eq!(take_log(), []);
    session.on_main_thread();
    assert_eq!(take_log(), [EditorCall::OnMainThread]);
    assert_eq!(session.take_calls(), []);
    ui.destroy();
}

#[test]
fn a_gesture_open_when_the_editor_is_destroyed_is_ended() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.75);
    });
    assert_eq!(
        session.process(),
        [Out::Begin(CUTOFF), Out::Value(CUTOFF, 0.75)]
    );
    // Closed in the middle of the drag: the editor drops its gesture.
    ui.destroy();
    assert_eq!(session.process(), [Out::End(CUTOFF)]);

    // A gesture the editor leaked, so that its destructor never runs: the
    // link ends it when the editor closes.
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    let leaked = with_probe(|probe| {
        probe.drag(GAIN, 0.5);
        probe.move_to(0.25);
        std::mem::ManuallyDrop::new(probe.gestures.borrow_mut().pop().unwrap())
    });
    ui.destroy();
    assert_eq!(
        session.process(),
        [Out::Begin(GAIN), Out::Value(GAIN, 0.25), Out::End(GAIN)]
    );
    // Dropped after all, much later, it sends nothing.
    drop(std::mem::ManuallyDrop::into_inner(leaked));
    assert_eq!(session.process(), []);

    // A host that destroys the plugin with the editor open, a drag in
    // progress, a timer and a watch: the editor is dropped first, its
    // gesture ended, its timer and watch unregistered, and the host is not
    // asked for a flush while the plugin goes away.
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.75);
        assert!(probe.every(16));
        assert!(probe.watch(9));
    });
    session.stop();
    session.take_calls();
    take_log();
    assert_eq!(
        session.destroy(),
        [HostCall::UnregisterTimer(40), HostCall::UnregisterFd(9)]
    );
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert!(!probe_alive());
}

/// A host must not destroy a plugin from inside one of the plugin's own
/// calls. One that does gets no use after free: the editor is cut off at
/// once, while the host is still there (its timer is unregistered, its
/// gesture ended, and the host is called for nothing else), and the plugin
/// is freed when the outermost call into it returns, the editor with it.
/// Miri checks that nothing touches the plugin after that.
#[test]
fn a_plugin_destroyed_from_inside_its_own_call_is_freed_when_the_call_returns() {
    set_up(Setup {
        resize_on_show: Some(HostSize::new(500, 400)),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    let ticks = with_probe(|probe| {
        assert!(probe.every(16));
        probe.drag(CUTOFF, 0.25);
        Rc::clone(&probe.ticks)
    });
    session.take_calls();
    take_log();
    // `show` makes the editor ask for a size; the host destroys the whole
    // plugin from inside that request.
    session.on_resize(|ui, _, _| {
        unsafe { (*ui.plugin).destroy.unwrap()(ui.plugin) };
        true
    });
    session.destroyed.set(true);
    ui.show();
    assert_eq!(
        take_log(),
        [EditorCall::SetVisible(true), EditorCall::Dropped]
    );
    assert!(!probe_alive());
    assert_eq!(
        session.take_calls(),
        [
            HostCall::RequestResize(500, 400),
            HostCall::UnregisterTimer(40)
        ]
    );
    assert_eq!(
        Rc::strong_count(&ticks),
        1,
        "a callback outlived the plugin"
    );
}

/// The same while the editor opens: it is cut off in the middle of `open`
/// (the timer it registered there is unregistered at once), dropped as soon
/// as `open` returns, and the plugin is freed when `set_parent` returns.
#[test]
fn a_plugin_destroyed_while_its_editor_opens_is_freed_when_the_call_returns() {
    set_up(Setup {
        timer_on_open: true,
        resize_on_open: Some(HostSize::new(450, 300)),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    session.on_resize(|ui, _, _| {
        unsafe { (*ui.plugin).destroy.unwrap()(ui.plugin) };
        true
    });
    session.destroyed.set(true);
    assert!(!ui.open(CLAP_WINDOW_API_X11, PARENT));
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(ParentWindow::x11(PARENT).unwrap(), None),
            EditorCall::Dropped
        ]
    );
    assert!(!probe_alive());
    assert_eq!(
        session.take_calls(),
        [
            HostCall::RegisterTimer(16, 40),
            HostCall::RequestResize(450, 300),
            HostCall::UnregisterTimer(40)
        ]
    );
}

/// The same from inside a call the editor makes from its own window, with
/// nothing of the plugin's on the stack to finish the job. The plugin is
/// freed at once, except for the editor, which cannot be dropped under its
/// own frame and is leaked; it is cut off, and nothing it does afterwards
/// reaches the host.
#[test]
#[cfg_attr(miri, ignore = "leaks the editor on purpose, which Miri reports")]
fn a_plugin_destroyed_from_inside_the_editors_own_call_leaks_only_the_editor() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    let ticks = with_probe(|probe| {
        assert!(probe.every(16));
        Rc::clone(&probe.ticks)
    });
    session.take_calls();
    take_log();
    session.on_resize(|ui, _, _| {
        unsafe { (*ui.plugin).destroy.unwrap()(ui.plugin) };
        true
    });
    session.destroyed.set(true);
    with_probe(|probe| probe.request_resize(HostSize::new(500, 400)));
    assert_eq!(
        session.take_calls(),
        [
            HostCall::RequestResize(500, 400),
            HostCall::UnregisterTimer(40)
        ]
    );
    assert_eq!(take_log(), [], "the editor was dropped under its own frame");
    assert!(probe_alive());
    // Cut off: nothing it does reaches the host.
    with_probe(|probe| {
        probe.drag(GAIN, 0.5);
        probe.move_to(1.0);
        probe.release();
        assert_eq!(probe.request_resize(HostSize::new(600, 400)), None);
        assert!(!probe.every(16));
        assert!(probe.context.main_thread_waker().is_none());
        assert!(!probe.context.is_attached());
    });
    assert_eq!(session.take_calls(), []);
    // The timer's callback was dropped; the editor and the test hold the
    // rest.
    assert_eq!(Rc::strong_count(&ticks), 2);
}

#[test]
fn the_host_loop_carries_timers_and_descriptors() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    let ticks = with_probe(|probe| {
        assert!(probe.context.has_host_loop());
        assert!(probe.every(16));
        assert!(probe.watch(7));
        assert!(probe.watch(7));
        Rc::clone(&probe.ticks)
    });
    // One timer, and one registration for two watches on one descriptor.
    assert_eq!(
        session.take_calls(),
        [
            HostCall::RegisterTimer(16, 40),
            HostCall::RegisterFd(7, CLAP_POSIX_FD_READ)
        ]
    );
    session.on_timer(40);
    assert_eq!(ticks.get(), 1);
    session.on_timer(41);
    session.on_fd(8, CLAP_POSIX_FD_READ);
    session.on_fd(7, CLAP_POSIX_FD_WRITE);
    assert_eq!(
        ticks.get(),
        1,
        "a callback ran for something it did not wait for"
    );
    session.on_fd(7, CLAP_POSIX_FD_READ);
    assert_eq!(ticks.get(), 201);
    session.on_fd(7, CLAP_POSIX_FD_ERROR);
    assert_eq!(ticks.get(), 401);

    // The host registration goes with the last watch on the descriptor.
    with_probe(|probe| probe.drop_handle(1));
    assert_eq!(session.take_calls(), []);
    with_probe(|probe| probe.drop_handle(1));
    assert_eq!(session.take_calls(), [HostCall::UnregisterFd(7)]);
    session.on_fd(7, CLAP_POSIX_FD_READ);
    assert_eq!(ticks.get(), 401);

    // A host that refuses gives the editor nothing to cancel.
    session
        .state()
        .refuse_registrations
        .store(true, Ordering::Relaxed);
    with_probe(|probe| {
        assert!(!probe.every(16));
        assert!(!probe.watch(3));
    });
    assert_eq!(session.take_calls(), []);

    // Closing the editor unregisters its timer.
    ui.destroy();
    assert_eq!(session.take_calls(), [HostCall::UnregisterTimer(40)]);
    session.on_timer(40);
    assert_eq!(ticks.get(), 401, "a timer ran after its editor closed");
    assert_eq!(
        Rc::strong_count(&ticks),
        1,
        "a callback outlived its editor"
    );
}

#[test]
fn a_timer_may_cancel_itself_and_register_another_while_it_runs() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    let fired = Rc::new(Cell::new(0));
    // Where the second timer's handle ends up: outside the editor, as a
    // handle an editor forgot about would be.
    let kept: Rc<RefCell<Vec<LoopHandle>>> = Rc::default();
    with_probe(|probe| {
        let context = probe.context.clone();
        let slot: Rc<RefCell<Option<LoopHandle>>> = Rc::default();
        let (inner, count, keep) = (Rc::clone(&slot), Rc::clone(&fired), Rc::clone(&kept));
        let handle = probe.out(|now| {
            now.every(16, move || {
                count.set(count.get() + 1);
                // Cancels itself, and starts a slower one in its place.
                drop(inner.borrow_mut().take());
                let count = Rc::clone(&count);
                if let Some(next) = context.every(100, move || count.set(count.get() + 10)) {
                    keep.borrow_mut().push(next);
                }
            })
        });
        *slot.borrow_mut() = handle;
    });
    session.take_calls();
    session.on_timer(40);
    assert_eq!(fired.get(), 1);
    assert_eq!(
        session.take_calls(),
        [
            HostCall::UnregisterTimer(40),
            HostCall::RegisterTimer(100, 41)
        ]
    );
    session.on_timer(40);
    session.on_timer(41);
    assert_eq!(fired.get(), 11);
    // The handle left outside is still alive, yet its timer is unregistered
    // when the editor closes, and dropping it later does nothing.
    ui.destroy();
    assert_eq!(session.take_calls(), [HostCall::UnregisterTimer(41)]);
    assert_eq!(kept.borrow().len(), 1);
    kept.borrow_mut().clear();
    assert_eq!(session.take_calls(), []);
    assert_eq!(
        Rc::strong_count(&fired),
        1,
        "a callback outlived its editor"
    );
}

#[test]
fn a_host_without_a_loop_offers_none() {
    set_up(Setup::default());
    let offers = Offers {
        timers: false,
        fds: false,
        ..EVERYTHING
    };
    let session = Session::new(
        plugin_entry::<Panel>(),
        c"org.manguebyte.test.panel",
        offers,
    );
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    with_probe(|probe| {
        assert!(!probe.context.has_host_loop());
        assert!(!probe.every(16));
        assert!(!probe.watch(4));
    });
    assert_eq!(session.take_calls(), []);
    drop(session);

    // Timers alone are a loop too, without descriptors.
    let offers = Offers {
        fds: false,
        ..EVERYTHING
    };
    let session = Session::new(
        plugin_entry::<Panel>(),
        c"org.manguebyte.test.panel",
        offers,
    );
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    with_probe(|probe| {
        assert!(probe.context.has_host_loop());
        assert!(probe.every(16));
        assert!(!probe.watch(4));
    });
}

#[test]
fn resize_requests_reach_the_host_and_take_its_answer() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();

    // The host agrees.
    let wanted = HostSize::new(640, 480);
    assert_eq!(
        with_probe(|probe| probe.request_resize(wanted)),
        Some(wanted)
    );
    // The host refuses.
    session.on_resize(|_, _, _| false);
    assert_eq!(with_probe(|probe| probe.request_resize(wanted)), None);
    // The host sets another size before it answers: that is the answer,
    // whatever it returns, and the editor is not called for it.
    for granted in [true, false] {
        session.on_resize(move |ui, width, height| {
            assert!(ui.set_size(HostSize::new(width / 2, height / 2)));
            granted
        });
        assert_eq!(
            with_probe(|probe| probe.request_resize(wanted)),
            Some(HostSize::new(320, 240))
        );
    }
    // A host that calls anything else back is answered without the editor,
    // which is busy asking: the size is the one asked for, and a size is
    // checked against the spec's rules. Anything that needs the editor gets
    // false. Nothing reaches the editor.
    session.on_resize(|ui, _, _| {
        assert_eq!(ui.size(), Some(HostSize::new(640, 480)));
        assert_eq!(
            ui.adjust(HostSize::new(5_000, 50)),
            Some(HostSize::new(1200, 100))
        );
        assert!(!ui.show());
        assert!(!ui.hide());
        assert!(!ui.set_scale(2.0));
        assert!(!ui.create(CLAP_WINDOW_API_WIN32));
        assert!(!ui.open(CLAP_WINDOW_API_WIN32, PARENT));
        true
    });
    assert_eq!(
        with_probe(|probe| probe.request_resize(wanted)),
        Some(wanted)
    );
    assert_eq!(take_log(), []);
    assert_eq!(session.take_calls(), [HostCall::RequestResize(640, 480); 5]);
    // After the request, everything works again.
    assert_eq!(ui.size(), Some(wanted));
    ui.destroy();
}

/// Hosts often check a size with `adjust_size` before they resize their
/// window, and may do so from inside `request_resize`. The answer does not
/// need the editor, so the host gets it while the editor is calling out,
/// and `get_size` gives the size being asked for.
#[test]
fn the_host_can_check_a_size_while_the_editor_asks_for_it() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();
    session.on_resize(|ui, width, height| {
        let wanted = HostSize::new(width, height);
        assert_eq!(ui.size(), Some(wanted));
        ui.adjust(wanted) == Some(wanted) && ui.set_size(wanted)
    });
    let wanted = HostSize::new(640, 480);
    assert_eq!(
        with_probe(|probe| probe.request_resize(wanted)),
        Some(wanted),
        "the host checked the size and was refused"
    );
    // Beyond the editor's limits the host is told what fits, and refuses.
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(5_000, 480))),
        None
    );
    assert_eq!(take_log(), [], "the editor was called while calling out");
    assert_eq!(ui.size(), Some(wanted));
    session.take_calls();
}

#[test]
fn a_destroy_from_inside_a_request_waits_until_the_editor_is_out_of_it() {
    // From inside the editor's own window: nothing of the plugin's is on
    // the stack, so the plugin asks the host to call back.
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();
    session.on_resize(|ui, _, _| {
        ui.destroy();
        // Gone, as far as the host can tell.
        assert_eq!(ui.size(), None);
        assert!(!ui.create(CLAP_WINDOW_API_WIN32));
        false
    });
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(500, 400))),
        None
    );
    assert!(
        probe_alive(),
        "the editor was dropped while it was calling out"
    );
    assert_eq!(take_log(), []);
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestResize(500, 400), HostCall::RequestCallback]
    );
    session.on_main_thread();
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert!(!probe_alive());
    // And the next editor opens normally.
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    drop(session);

    // From inside one of the plugin's own functions: done as it returns.
    set_up(Setup {
        resize_on_show: Some(HostSize::new(500, 400)),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    session.on_resize(|ui, _, _| {
        ui.destroy();
        true
    });
    take_log();
    assert!(ui.show());
    assert_eq!(
        take_log(),
        [EditorCall::SetVisible(true), EditorCall::Dropped]
    );
    assert!(!probe_alive());
}

/// A `destroy` that arrives while the editor is calling the host from its
/// own window has to wait. The host considers the editor gone at once and
/// may create a new one straight away, before it gets round to the plugin's
/// `on_main_thread`: the old editor is dropped first, and `create` works.
#[test]
fn create_right_after_a_destroy_that_had_to_wait_succeeds() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();
    session.on_resize(|ui, _, _| {
        ui.destroy();
        false
    });
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(500, 400))),
        None
    );
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestResize(500, 400), HostCall::RequestCallback]
    );
    assert!(probe_alive());
    assert!(
        ui.create(CLAP_WINDOW_API_WIN32),
        "create refused right after the host destroyed the editor"
    );
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert_eq!(take_log(), [EditorCall::Open(win32_parent(), None)]);
    // The call back asked for finds the new editor, which checks for work.
    session.on_main_thread();
    assert_eq!(take_log(), [EditorCall::OnMainThread]);
    assert_eq!(session.take_calls(), []);
}

/// The same destroy that had to wait: until the host calls back, its loop
/// may still deliver a tick it had queued for the editor's timer. The host
/// destroyed the editor, and maybe the window it lived in, so the editor is
/// dropped first, its timer unregistered, and the tick reaches nothing.
#[test]
fn a_timer_does_not_tick_in_an_editor_the_host_has_destroyed() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(ui.open(CLAP_WINDOW_API_X11, PARENT));
    let ticks = with_probe(|probe| {
        assert!(probe.every(16));
        Rc::clone(&probe.ticks)
    });
    assert_eq!(session.take_calls(), [HostCall::RegisterTimer(16, 40)]);
    session.on_timer(40);
    assert_eq!(ticks.get(), 1, "the timer works");

    session.on_resize(|ui, _, _| {
        ui.destroy();
        true
    });
    with_probe(|probe| probe.request_resize(HostSize::new(500, 400)));
    assert_eq!(
        session.take_calls(),
        [HostCall::RequestResize(500, 400), HostCall::RequestCallback]
    );
    take_log();
    session.on_timer(40);
    assert_eq!(
        ticks.get(),
        1,
        "the editor's timer ran after the host destroyed the editor"
    );
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert_eq!(session.take_calls(), [HostCall::UnregisterTimer(40)]);
    assert_eq!(
        Rc::strong_count(&ticks),
        1,
        "a callback outlived its editor"
    );
}

#[test]
fn an_editor_that_cannot_go_on_is_destroyed_by_the_host() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();
    // A host that destroys the editor as soon as it hears.
    session.on_closed(|ui| ui.destroy());
    with_probe(|probe| probe.out(|context| context.closed()));
    assert_eq!(
        session.take_calls(),
        [HostCall::Closed(true), HostCall::RequestCallback]
    );
    assert!(probe_alive());
    session.on_main_thread();
    assert_eq!(take_log(), [EditorCall::Dropped]);
}

#[test]
fn waking_from_another_thread_reaches_the_editor_on_the_main_thread() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    take_log();
    let waker: MainThreadWaker = with_probe(|probe| probe.context.main_thread_waker()).unwrap();
    let worker = waker.clone();
    std::thread::spawn(move || assert!(worker.wake()))
        .join()
        .unwrap();
    assert_eq!(session.take_calls(), [HostCall::RequestCallback]);
    session.on_main_thread();
    assert_eq!(take_log(), [EditorCall::OnMainThread]);
    // Once the editor is closed, waking does nothing.
    ui.destroy();
    assert!(!waker.wake());
    assert_eq!(session.take_calls(), []);
    drop(session);

    // A host that cannot call back offers no waker.
    let offers = Offers {
        callbacks: false,
        ..EVERYTHING
    };
    let session = Session::new(
        plugin_entry::<Panel>(),
        c"org.manguebyte.test.panel",
        offers,
    );
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(with_probe(|probe| probe.context.main_thread_waker()).is_none());
}

#[test]
fn values_changed_asks_the_host_to_read_them_and_does_not_send_them_back() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    // The editor loads a preset of its own.
    params().cutoff.set(80.0);
    params().mode.set_index(1);
    with_probe(|probe| probe.out(|context| context.values_changed()));
    assert_eq!(
        session.take_calls(),
        [HostCall::Rescan(CLAP_PARAM_RESCAN_VALUES)]
    );
    assert_eq!(session.process(), []);

    // A gesture made before the next block is still sent whole.
    with_probe(|probe| probe.out(|context| context.values_changed()));
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 0.75)));
    assert_eq!(
        session.process(),
        [Out::Begin(GAIN), Out::Value(GAIN, 0.75), Out::End(GAIN)]
    );
    ui.destroy();
}

/// An editor that loads a whole new instrument renames parameters as well
/// as setting them: the one rescan it asks for reads the names too, and the
/// audio side, which then finds the host told, asks for nothing more.
#[test]
fn values_changed_after_renames_has_the_host_read_the_names_in_the_same_rescan() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    params().cutoff.info().set_display_name("Brightness");
    params().gain.info().set_display_name("Level");
    params().gain.set(0.25);
    with_probe(|probe| probe.out(|context| context.values_changed()));
    assert_eq!(
        session.take_calls(),
        [HostCall::Rescan(
            CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_INFO
        )]
    );
    assert_eq!(session.process(), []);
    assert_eq!(session.take_calls(), []);
    // Values alone, later, are values alone.
    params().gain.set(0.5);
    with_probe(|probe| probe.out(|context| context.values_changed()));
    assert_eq!(
        session.take_calls(),
        [HostCall::Rescan(CLAP_PARAM_RESCAN_VALUES)]
    );
    ui.destroy();
}

/// ADR 0007, section 5: restored state is never reported back to the host
/// as a person's edit. A host that loads state reads every value again and
/// asks for no events. A gesture that happens to be open must not carry the
/// restored value to the host as if the person had set it: a host writing
/// automation in touch mode would record it.
#[test]
fn restored_state_is_not_sent_back_inside_an_open_gesture() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    // The person holds the gain control without moving it.
    with_probe(|probe| probe.drag(GAIN, 0.5));
    assert_eq!(session.process(), [Out::Begin(GAIN)]);
    session.take_calls();
    assert!(session.load_state(&panel_state(25.0, 0, 0.9)));
    assert_eq!(
        session.take_calls(),
        [HostCall::Rescan(CLAP_PARAM_RESCAN_VALUES)]
    );
    assert_eq!(params().gain.value(), 0.9);
    assert_eq!(
        session.process(),
        [],
        "the restored gain went back to the host as the person's edit"
    );
    with_probe(|probe| probe.release());
    assert_eq!(session.process(), [Out::End(GAIN)]);
}

/// The host knows the values it was asked to read again, as they were when
/// it was asked. An edit made after that is news, even the first one after
/// a state load while the host is not processing, which reaches the host
/// through `flush`.
#[test]
fn an_edit_made_after_a_state_load_still_reaches_the_host() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(session.load_state(&panel_state(80.0, 2, 0.9)));
    session.take_calls();
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 0.25)));
    assert_eq!(session.take_calls(), [HostCall::RequestFlush; 3]);
    assert_eq!(
        session.flush(),
        [Out::Begin(GAIN), Out::Value(GAIN, 0.25), Out::End(GAIN)]
    );
    assert_eq!(session.flush(), []);

    // While processing, the next block sends the edit and not the values
    // just restored.
    session.start();
    assert!(session.load_state(&panel_state(10.0, 1, 0.5)));
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, CUTOFF, 0.75)));
    assert_eq!(
        session.process(),
        [
            Out::Begin(CUTOFF),
            Out::Value(CUTOFF, 0.75),
            Out::End(CUTOFF)
        ]
    );
    assert_eq!(session.process(), []);
    assert_eq!(
        [CUTOFF, MODE, GAIN].map(|param| session.value(param)),
        [0.75, 1.0, 0.5]
    );
}

/// A block can run while the main thread restores state, here right after
/// the first restored value is stored. It reports none of the restored
/// values, and neither does any block after.
#[test]
fn a_block_that_runs_while_state_loads_sends_none_of_it() {
    set_up(Setup::default());
    let session = Session::panel();
    session.start();
    let plugin = session.plugin;
    let during: Rc<RefCell<Vec<Out>>> = Rc::default();
    let seen = Rc::clone(&during);
    after_next_store(move || {
        let mut sent = Vec::with_capacity(16);
        assert_eq!(run_block(plugin, &mut sent), 0);
        seen.borrow_mut().extend(sent);
    });
    assert!(session.load_state(&panel_state(80.0, 2, 0.9)));
    assert!(HOOK.with(|hook| hook.borrow().is_none()), "no block ran");
    assert_eq!(params().cutoff.value(), 80.0);
    assert_eq!(*during.borrow(), [], "a block sent restored values back");
    assert_eq!(session.process(), []);
    assert_eq!(session.process(), []);
}

#[test]
fn an_editor_that_fails_to_open_leaves_nothing_behind() {
    set_up(Setup {
        fail_open: true,
        timer_on_open: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_X11));
    assert!(!ui.open(CLAP_WINDOW_API_X11, PARENT));
    assert_eq!(
        session.take_calls(),
        [
            HostCall::RegisterTimer(16, 40),
            HostCall::UnregisterTimer(40)
        ]
    );
    assert!(!ui.show());
    // Still created: the host may try again, or destroy.
    assert!(ui.size().is_some());
    ui.destroy();
    assert!(ui.create(CLAP_WINDOW_API_X11));
}

#[test]
fn a_panicking_editor_does_not_take_the_host_down() {
    set_up(Setup {
        panic_on_show: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.ui();
    assert!(ui.create(CLAP_WINDOW_API_WIN32));
    assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
    assert!(!ui.show());
    // The editor is still there and still answers.
    assert!(ui.hide());
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    ui.destroy();
    assert_eq!(take_log().last(), Some(&EditorCall::Dropped));
}

/// A pointer the test hands to the thread that plays the audio thread.
struct AudioSide(*const clap_plugin);

// SAFETY: CLAP lets a host call `process` on another thread while the main
// thread uses the plugin's other functions.
unsafe impl Send for AudioSide {}

/// The host's automation in the test below, in CLAP's scale: values that
/// each parameter holds exactly.
const AUTOMATION: [(ParamId, f64); 6] = [
    (CUTOFF, 0.25),
    (MODE, 1.0),
    (GAIN, 0.75),
    (CUTOFF, 0.5),
    (MODE, 2.0),
    (GAIN, 0.25),
];

/// Thousands of editors opened, dragged and closed while the audio thread
/// runs blocks with host automation on the very parameters being dragged.
/// Gestures stay well formed, `process` never touches the heap, and the
/// host ends up knowing each parameter's value: the last thing it heard,
/// its own automation or the plugin's report, is what the parameter holds.
#[test]
fn the_editor_opens_and_closes_thousands_of_times_while_audio_runs() {
    let cycles = if cfg!(miri) { 8 } else { 10_000 };
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.ui();
    session.start();

    let controls = [CUTOFF, MODE, GAIN];
    let index_of = move |param: ParamId| controls.iter().position(|&id| id == param).unwrap();
    let start = controls.map(|param| session.value(param));
    let done = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicUsize::new(0));
    let audio = {
        let plugin = AudioSide(session.plugin);
        let (done, blocks) = (Arc::clone(&done), Arc::clone(&blocks));
        std::thread::spawn(move || {
            let plugin = plugin;
            let mut everything = Vec::new();
            let mut sent = Vec::with_capacity(1024);
            let mut heap_calls = 0;
            // What the host knows of each parameter.
            let mut host_view = start;
            let mut block = 0_usize;
            loop {
                // Read before the block, so that the last block runs after
                // the last edit.
                let finished = done.load(Ordering::Acquire);
                // An automation point every third block.
                let point = (block % 3 == 0).then(|| AUTOMATION[(block / 3) % AUTOMATION.len()]);
                let events = point.map(|(param, value)| [automation(param, value)]);
                let input = events.as_ref().map_or(&[][..], |events| &events[..]);
                sent.clear();
                heap_calls += run_block_with(plugin.0, input, &mut sent);
                // In a block the plugin reports first, then stores the
                // host's values.
                for event in &sent {
                    if let Out::Value(param, value) = *event {
                        host_view[index_of(param)] = value;
                    }
                }
                if let Some((param, value)) = point {
                    host_view[index_of(param)] = value;
                }
                everything.extend_from_slice(&sent);
                blocks.fetch_add(1, Ordering::Release);
                block += 1;
                if finished {
                    return (everything, heap_calls, host_view);
                }
            }
        })
    };
    // Audio is running before the first editor opens.
    while blocks.load(Ordering::Acquire) == 0 {
        std::thread::yield_now();
    }

    let every = if cfg!(miri) { 2 } else { 64 };
    for cycle in 0..cycles {
        if cycle % every == 0 {
            // Let audio catch up now and then, so that blocks are known to
            // run in between, however the threads are scheduled.
            let seen = blocks.load(Ordering::Acquire);
            while blocks.load(Ordering::Acquire) == seen {
                std::thread::yield_now();
            }
        }
        assert!(ui.create(CLAP_WINDOW_API_WIN32));
        assert!(ui.set_scale(1.0 + (cycle % 4) as f64 * 0.25));
        assert!(ui.size().is_some());
        assert!(ui.open(CLAP_WINDOW_API_WIN32, PARENT));
        assert!(ui.show());
        let control = controls[cycle % 3];
        with_probe(|probe| {
            probe.drag(control, 0.0);
            probe.move_to(((cycle % 5) as f64 + 1.0) / 5.0);
            if cycle % 2 == 0 {
                // Half the time the drag is still on when the editor closes.
                probe.release();
            }
        });
        assert!(ui.hide());
        ui.destroy();
        assert!(!probe_alive());
    }
    done.store(true, Ordering::Release);
    let (events, heap_calls, host_view) = audio.join().unwrap();

    assert_eq!(heap_calls, 0, "process used the heap");
    assert_eq!(
        host_view,
        controls.map(|param| session.value(param)),
        "the host does not know what the parameters hold"
    );
    assert!(blocks.load(Ordering::Acquire) > cycles / every);
    let mut open = Vec::new();
    let begins = check_nesting(&events, &mut open);
    assert!(open.is_empty(), "gestures left open at the host: {open:?}");
    assert!(
        (1..=cycles).contains(&begins),
        "{begins} gestures for {cycles}"
    );
    let log = take_log();
    let opened = log
        .iter()
        .filter(|call| matches!(call, EditorCall::Open(..)))
        .count();
    let dropped = log
        .iter()
        .filter(|&&call| call == EditorCall::Dropped)
        .count();
    assert_eq!((opened, dropped), (cycles, cycles));
    assert!(!log.contains(&EditorCall::Reentered));
    // While the host processes it is never asked for a flush.
    assert!(!session.take_calls().contains(&HostCall::RequestFlush));
}

// --- Fuzzing -------------------------------------------------------------

/// A small deterministic generator (xorshift64*), so that failures repeat.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len())]
    }
}

/// What the wrapper should answer, worked out from the CLAP rules and the
/// editor contract alone.
#[derive(Debug, Default)]
struct Model {
    created: Option<WindowApi>,
    open: bool,
    scale: Option<f64>,
    preset: Option<HostSize>,
    /// The open editor's size and scale.
    size: HostSize,
    editor_scale: Option<f64>,
}

impl Model {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn spec_rules(&self, setup: &Setup) -> Resize {
        setup.resize.scaled(self.scale.unwrap_or(1.0))
    }

    fn editor_rules(&self, setup: &Setup) -> Resize {
        setup.resize.scaled(self.editor_scale.unwrap_or(1.0))
    }

    fn before_open(&self, setup: &Setup) -> HostSize {
        self.preset
            .unwrap_or_else(|| setup.base.scale_by(self.scale.unwrap_or(1.0)))
    }
}

const API_NAMES: [Option<&CStr>; 6] = [
    Some(CLAP_WINDOW_API_WIN32),
    Some(CLAP_WINDOW_API_COCOA),
    Some(CLAP_WINDOW_API_X11),
    Some(CLAP_WINDOW_API_WAYLAND),
    Some(c""),
    None,
];

fn api_of(name: Option<&CStr>) -> Option<WindowApi> {
    match name? {
        name if name == CLAP_WINDOW_API_WIN32 => Some(WindowApi::Win32),
        name if name == CLAP_WINDOW_API_COCOA => Some(WindowApi::Cocoa),
        name if name == CLAP_WINDOW_API_X11 => Some(WindowApi::X11),
        _ => None,
    }
}

/// The parent a host's window comes to, as the test writes it.
fn expected_parent(api: WindowApi, handle: u64) -> Option<ParentWindow> {
    match api {
        WindowApi::Win32 => ParentWindow::win32(handle as usize),
        WindowApi::Cocoa => ParentWindow::cocoa(handle as usize),
        WindowApi::X11 => ParentWindow::x11(widen(narrow::<c_ulong>(handle))),
        _ => None,
    }
}

/// `value` as a `c_ulong`, which is 32 bits wide on some platforms and 64
/// on others; a value that does not fit becomes 0, a null window.
fn narrow<T: TryFrom<u64> + Default>(value: u64) -> T {
    T::try_from(value).unwrap_or_default()
}

/// A `c_ulong` as a `u64`, on any platform.
fn widen(value: impl Into<u64>) -> u64 {
    value.into()
}

#[test]
fn fuzzed_gui_calls_get_the_answers_the_rules_give() {
    let mut random = Random(0x5EED_CAFE_F00D_0001);
    let rounds = if cfg!(miri) { 6 } else { 300 };
    let steps = if cfg!(miri) { 30 } else { 120 };
    let scales = [
        1.0,
        1.5,
        2.0,
        0.25,
        8.0,
        0.0,
        -1.0,
        9.0,
        f64::NAN,
        f64::INFINITY,
    ];
    let lengths = [0, 1, 99, 150, 300, 450, 600, 1200, 2400, 5000, u32::MAX];
    let handles = [0, 1, 0x00AB_CDEF, 1 << 40];
    let resizes = [
        Resize::Fixed,
        Resize::Free {
            min: HostSize::new(150, 100),
            max: HostSize::new(1200, 800),
        },
        Resize::Ratio {
            min: HostSize::new(150, 100),
            max: HostSize::new(1500, 1000),
            width: 3,
            height: 2,
        },
    ];

    for round in 0..rounds {
        let setup = Setup {
            resize: random.pick(&resizes),
            supports: [
                random.next() % 4 != 0,
                random.next() % 4 != 0,
                random.next() % 4 != 0,
            ],
            accept_scale: random.next() % 3 != 0,
            fail_open: random.next() % 8 == 0,
            ..Setup::default()
        };
        set_up(setup.clone());
        let session = Session::panel();
        let ui = session.ui();
        let supported =
            |api: Option<WindowApi>| api.and_then(api_index).is_some_and(|i| setup.supports[i]);
        let mut model = Model::default();

        for step in 0..steps {
            let at = format!("round {round}, step {step}");
            match random.below(12) {
                0 => {
                    let name = random.pick(&API_NAMES);
                    let floating = random.next() % 5 == 0;
                    let raw = name.map_or(ptr::null(), CStr::as_ptr);
                    let api = api_of(name);
                    let expected = !floating && supported(api);
                    assert_eq!(ui.is_api_supported(raw, floating), expected, "{at}");
                }
                1 | 2 => {
                    let name = random.pick(&API_NAMES);
                    let floating = random.next() % 5 == 0;
                    let api = api_of(name);
                    let expected = !floating && supported(api) && model.created.is_none();
                    let got = ui.create_raw(name.map_or(ptr::null(), CStr::as_ptr), floating);
                    assert_eq!(got, expected, "{at}: create {name:?}");
                    if expected {
                        model.reset();
                        model.created = api;
                    }
                }
                3 => {
                    ui.destroy();
                    model.reset();
                }
                4 => {
                    let scale = random.pick(&scales);
                    let usable = model.created.is_some_and(|api| api != WindowApi::Cocoa)
                        && checked_scale(scale).is_some();
                    let expected = usable && (!model.open || setup.accept_scale);
                    assert_eq!(ui.set_scale(scale), expected, "{at}: scale {scale}");
                    if expected {
                        model.scale = Some(scale);
                        if model.open {
                            model.editor_scale = Some(scale);
                            model.size = setup.base.scale_by(scale);
                        } else {
                            model.preset = None;
                        }
                    }
                }
                5 => {
                    let expected = model.created.map(|_| {
                        if model.open {
                            model.size
                        } else {
                            model.before_open(&setup)
                        }
                    });
                    assert_eq!(ui.size(), expected, "{at}");
                }
                6 => {
                    let can = model.created.is_some() && model.spec_rules(&setup).can_resize();
                    assert_eq!(ui.can_resize(), can, "{at}");
                    let hints = ui.hints();
                    assert_eq!(hints.is_some(), model.created.is_some(), "{at}");
                    if let Some(hints) = hints {
                        let rules = model.spec_rules(&setup).hints();
                        assert_eq!(hints.can_resize_horizontally, rules.horizontal, "{at}");
                        assert_eq!(hints.can_resize_vertically, rules.vertical, "{at}");
                        assert_eq!(hints.preserve_aspect_ratio, rules.ratio.is_some(), "{at}");
                    }
                }
                7 => {
                    let wanted = HostSize::new(random.pick(&lengths), random.pick(&lengths));
                    let can = model.created.is_some() && model.spec_rules(&setup).can_resize();
                    let expected = can.then(|| {
                        if model.open {
                            model.editor_rules(&setup).fit(model.size, wanted)
                        } else {
                            model
                                .spec_rules(&setup)
                                .fit(model.before_open(&setup), wanted)
                        }
                    });
                    assert_eq!(ui.adjust(wanted), expected, "{at}: adjust {wanted:?}");
                }
                8 => {
                    let size = if random.next() % 2 == 0 {
                        HostSize::new(random.pick(&lengths), random.pick(&lengths))
                    } else {
                        // Often a size the rules accept.
                        let wanted = HostSize::new(random.pick(&lengths), random.pick(&lengths));
                        model
                            .spec_rules(&setup)
                            .fit(model.before_open(&setup), wanted)
                    };
                    let expected = match (model.created, model.open) {
                        (None, _) => false,
                        (Some(_), true) => model.editor_rules(&setup).fit(model.size, size) == size,
                        (Some(_), false) => {
                            model
                                .spec_rules(&setup)
                                .fit(model.before_open(&setup), size)
                                == size
                        }
                    };
                    assert_eq!(ui.set_size(size), expected, "{at}: set_size {size:?}");
                    if expected {
                        if model.open {
                            model.size = size;
                        } else {
                            model.preset = Some(size);
                        }
                    }
                }
                9 => {
                    let name = random.pick(&API_NAMES);
                    let handle = random.pick(&handles);
                    let api = api_of(name);
                    let parent = api.and_then(|api| expected_parent(api, handle));
                    let reaches_open = model.created.is_some()
                        && !model.open
                        && api == model.created
                        && parent.is_some();
                    let expected = reaches_open && !setup.fail_open;
                    take_log();
                    let got = ui.set_parent(name.map_or(ptr::null(), CStr::as_ptr), handle);
                    assert_eq!(got, expected, "{at}: set_parent {name:?} {handle:#x}");
                    let log = take_log();
                    if reaches_open {
                        assert_eq!(
                            log.first(),
                            Some(&EditorCall::Open(parent.unwrap(), model.scale)),
                            "{at}"
                        );
                    } else {
                        assert_eq!(log, [], "{at}");
                    }
                    if expected {
                        model.open = true;
                        model.editor_scale = model.scale;
                        model.size = setup.base.scale_by(model.scale.unwrap_or(1.0));
                        if let Some(preset) = model.preset.take() {
                            if preset != model.size
                                && model.editor_rules(&setup).fit(model.size, preset) == preset
                            {
                                model.size = preset;
                            }
                        }
                    }
                }
                10 => {
                    let expected = model.created.is_some() && model.open;
                    assert_eq!(ui.show(), expected, "{at}");
                }
                _ => {
                    let expected = model.created.is_some() && model.open;
                    assert_eq!(ui.hide(), expected, "{at}");
                }
            }
            assert_eq!(probe_alive(), model.open, "{at}");
        }
        ui.destroy();
        assert!(!probe_alive());
        let log = take_log();
        assert!(!log.contains(&EditorCall::Reentered));
        drop(session);
    }
}

/// Sizes a fuzzed host asks for.
const HOST_SIZES: [HostSize; 5] = [
    HostSize::new(0, 0),
    HostSize::new(150, 100),
    HostSize::new(400, 300),
    HostSize::new(640, 480),
    HostSize::new(5_000, 5_000),
];

/// What a fuzzed host does when the plugin calls it and it may call back:
/// from inside `request_resize` (`asked` is the size asked for) or
/// `closed`. Anything a host may do, calling the plugin back included.
/// Returns whether a resize request is granted.
fn reply_at_random(
    random: &mut Random,
    created: &AtomicBool,
    ui: Ui,
    asked: Option<HostSize>,
) -> bool {
    match random.below(13) {
        0 => {}
        1 => return false,
        2 => {
            if let Some(asked) = asked {
                // Another size than the one asked for: the answer.
                let other = HostSize::new(asked.width / 2 + 1, asked.height / 2 + 1);
                return ui.set_size(other);
            }
        }
        3 => {
            ui.destroy();
            created.store(false, Ordering::Relaxed);
        }
        4 => ui.on_main_thread(),
        5 => {
            let _ = ui.size();
        }
        6 => {
            let _ = ui.adjust(HostSize::new(640, 480));
        }
        7 => ui.on_timer(40 + random.below(4) as clap_id),
        8 => {
            if ui.create(CLAP_WINDOW_API_X11) {
                created.store(true, Ordering::Relaxed);
            }
        }
        9 => {
            let _ = ui.show();
        }
        10 => {
            let _ = ui.hide();
        }
        11 => {
            let _ = ui.set_scale(2.0);
        }
        _ => {
            let _ = ui.set_size(random.pick(&HOST_SIZES));
        }
    }
    random.next() % 4 != 0
}

/// A host that does anything in any order: `gui` calls, the person working
/// the editor, `process` with automation and short lists, `flush`,
/// `on_main_thread`, timers, state loads, and replies that call the plugin
/// back from inside its own calls. After every step: the editor is never
/// called or dropped while it is calling out, the host's events are well
/// formed with every value inside a gesture, neither `process` nor `flush`
/// touches the heap, `create` works whenever the host has no editor, and an
/// editor the host destroyed is gone once a call into the plugin returns.
/// At the end, every gesture is closed at the host, and the host knows each
/// parameter's value.
#[test]
fn fuzzed_hosts_that_call_back_keep_every_promise() {
    let rounds = if cfg!(miri) { 2 } else { 80 };
    let steps = if cfg!(miri) { 40 } else { 250 };
    let mut random = Random(0x0BAD_C0DE_1234_5678);
    let resizes = [
        Resize::Fixed,
        Resize::Free {
            min: HostSize::new(150, 100),
            max: HostSize::new(1200, 800),
        },
        Resize::Ratio {
            min: HostSize::new(150, 100),
            max: HostSize::new(1500, 1000),
            width: 3,
            height: 2,
        },
    ];
    let apis = [CLAP_WINDOW_API_WIN32, CLAP_WINDOW_API_X11];
    let controls = [CUTOFF, MODE, GAIN];
    let index_of = |param: ParamId| controls.iter().position(|&id| id == param).unwrap();
    let heard = |sent: &[Out], open_at_host: &mut Vec<ParamId>, view: &mut [f64; 3]| {
        check_nesting(sent, open_at_host);
        for event in sent {
            if let Out::Value(param, value) = *event {
                view[index_of(param)] = value;
            }
        }
    };

    for round in 0..rounds {
        set_up(Setup {
            resize: random.pick(&resizes),
            accept_scale: random.next() % 3 != 0,
            timer_on_open: random.next() % 3 == 0,
            resize_on_open: (random.next() % 4 == 0).then_some(HostSize::new(450, 300)),
            resize_on_show: (random.next() % 3 == 0).then_some(HostSize::new(400, 300)),
            ..Setup::default()
        });
        let session = Session::panel();
        let ui = session.ui();
        // Whether the host has an editor created, as the host sees it.
        let created = Arc::new(AtomicBool::new(false));
        let replies = Arc::new(Mutex::new(Random(random.next() | 1)));
        {
            let (replies, created) = (Arc::clone(&replies), Arc::clone(&created));
            session.on_resize(move |ui, width, height| {
                let Ok(mut random) = replies.try_lock() else {
                    return true;
                };
                let asked = Some(HostSize::new(width, height));
                reply_at_random(&mut random, &created, ui, asked)
            });
        }
        {
            let (replies, created) = (Arc::clone(&replies), Arc::clone(&created));
            session.on_closed(move |ui| {
                if let Ok(mut random) = replies.try_lock() {
                    reply_at_random(&mut random, &created, ui, None);
                }
            });
        }
        let mut created_api: Option<&CStr> = None;
        let mut open_at_host = Vec::new();
        let mut host_view = controls.map(|param| session.value(param));

        for step in 0..steps {
            let at = format!("round {round}, step {step}");
            let choice = random.below(16);
            // Every step but the person's (9 to 11), the audio side's (14)
            // and the transport's (15) is a call into the plugin's editor
            // extensions or `on_main_thread`.
            let a_call = choice <= 8 || choice == 12 || choice == 13;
            match choice {
                0 => {
                    let api = random.pick(&apis);
                    let expected = !created.load(Ordering::Relaxed);
                    assert_eq!(ui.create(api), expected, "{at}: create");
                    if expected {
                        created.store(true, Ordering::Relaxed);
                        created_api = Some(api);
                    }
                }
                1 => {
                    ui.destroy();
                    created.store(false, Ordering::Relaxed);
                }
                2 => {
                    let _ = ui.set_scale(random.pick(&[1.0, 1.5, 2.0, 0.0]));
                }
                3 => {
                    let _ = ui.set_size(random.pick(&HOST_SIZES));
                }
                4 => {
                    let expected = created.load(Ordering::Relaxed);
                    assert_eq!(ui.size().is_some(), expected, "{at}: size");
                }
                5 => {
                    let _ = ui.adjust(random.pick(&HOST_SIZES));
                }
                6 => {
                    let api = random.pick(&apis);
                    let expected = created.load(Ordering::Relaxed)
                        && !probe_alive()
                        && created_api == Some(api);
                    assert_eq!(ui.open(api, PARENT), expected, "{at}: set_parent");
                }
                7 => {
                    let expected = created.load(Ordering::Relaxed) && probe_alive();
                    assert_eq!(ui.show(), expected, "{at}: show");
                }
                8 => {
                    let expected = created.load(Ordering::Relaxed) && probe_alive();
                    assert_eq!(ui.hide(), expected, "{at}: hide");
                }
                9..=11 if probe_alive() => match random.below(9) {
                    // The person working the editor.
                    0 => {
                        let param = random.pick(&controls);
                        with_probe(|probe| probe.drag(param, 0.5));
                    }
                    1 | 2 => {
                        let value = random.pick(&[0.0, 0.25, 0.5, 1.0]);
                        with_probe(|probe| probe.move_to(value));
                    }
                    3 => with_probe(Probe::release),
                    4 => {
                        let size = random.pick(&HOST_SIZES);
                        with_probe(|probe| probe.request_resize(size));
                    }
                    5 => {
                        with_probe(|probe| probe.every(16));
                    }
                    6 => with_probe(|probe| {
                        if !probe.handles.borrow().is_empty() {
                            probe.drop_handle(0);
                        }
                    }),
                    7 => with_probe(|probe| probe.out(EditorContext::closed)),
                    _ => {
                        with_probe(|probe| probe.out(EditorContext::values_changed));
                        // The host read every value again.
                        host_view = controls.map(|param| session.value(param));
                    }
                },
                9..=11 => {}
                12 => ui.on_main_thread(),
                13 => ui.on_timer(40 + random.below(4) as clap_id),
                14 => {
                    // The audio side, with a list that may be short of room.
                    let room = random.pick(&[0, 1, 2, usize::MAX]);
                    if session.processing.get() {
                        let point = (random.next() % 3 == 0)
                            .then(|| (random.pick(&controls), random.pick(&[0.0, 1.0])));
                        let events = point.map(|(param, value)| [automation(param, value)]);
                        let input = events.as_ref().map_or(&[][..], |events| &events[..]);
                        let sent = session.block(input, room);
                        heard(&sent, &mut open_at_host, &mut host_view);
                        if let Some((param, value)) = point {
                            host_view[index_of(param)] = value;
                        }
                    } else {
                        let sent = session.flush_with(&[], room);
                        heard(&sent, &mut open_at_host, &mut host_view);
                    }
                }
                _ => {
                    if random.next() % 3 == 0 {
                        let state = panel_state(
                            random.pick(&[10.0, 50.0, 90.0]),
                            random.below(3),
                            random.pick(&[0.0, 0.5, 1.0]),
                        );
                        assert!(session.load_state(&state), "{at}: load");
                        host_view = controls.map(|param| session.value(param));
                    } else if session.processing.get() {
                        session.stop();
                    } else if session.active.get() && random.next() % 2 == 0 {
                        session.deactivate();
                    } else {
                        session.start();
                    }
                }
            }
            let log = take_log();
            assert!(
                !log.contains(&EditorCall::Reentered),
                "{at}: the editor was called or dropped while calling out: {log:?}"
            );
            if a_call && !created.load(Ordering::Relaxed) {
                assert!(
                    !probe_alive(),
                    "{at}: an editor the host destroyed outlived a call into the plugin"
                );
            }
            // Also checks that nothing failed inside the host.
            session.take_calls();
        }

        // The person lets go, and the host closes the editor.
        while probe_alive() && with_probe(|probe| !probe.gestures.borrow().is_empty()) {
            with_probe(Probe::release);
        }
        ui.destroy();
        ui.on_main_thread();
        assert!(!probe_alive(), "round {round}");
        for _ in 0..3 {
            let sent = if session.processing.get() {
                session.process()
            } else {
                session.flush()
            };
            heard(&sent, &mut open_at_host, &mut host_view);
        }
        assert_eq!(
            open_at_host,
            [],
            "round {round}: gestures left open at the host"
        );
        assert_eq!(
            host_view,
            controls.map(|param| session.value(param)),
            "round {round}: the host does not know what the parameters hold"
        );
        assert!(
            !take_log().contains(&EditorCall::Reentered),
            "round {round}"
        );
        session.take_calls();
    }
}
