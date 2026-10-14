//! Drives a plugin's editor through VST 3's binary interface, the way a host
//! does: the component handler, `IPlugView` with its content scale, and the
//! host's `IPlugFrame` with the Linux `IRunLoop` behind it.
//!
//! The editor is a fake that records every call made to it and lets a test
//! act as the person using it: drag a control, ask for a size, register a
//! timer. The host's objects are fakes that record every call made to them
//! and can answer the way real hosts do, including by calling the plugin
//! back from inside their own functions.
//!
//! Every `unsafe` block in this file is a call across that interface with
//! pointers the test owns and keeps alive for the duration of the call, so
//! the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]
#![allow(non_snake_case)]
// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast needed on one platform is redundant on the other.
#![allow(clippy::unnecessary_cast)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::EventKind;
use manguebyte_dsp::{ProcessContext, ProcessSpec, split_at_events};
use manguebyte_params::{ChoiceParam, FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorSpec, Gesture, HostSize, Key, KeyEvent, LoopHandle,
    MainThreadWaker, Modifiers, OpenEditor, ParentWindow, Resize, WindowApi, checked_scale,
};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
use manguebyte_vst3::vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler, ITimerHandlerTrait,
};
use manguebyte_vst3::vst3::Steinberg::Vst::{
    self as vst, IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentHandler,
    IComponentHandlerTrait, IComponentTrait, IEditController, IEditControllerTrait,
    IParamValueQueue, IParamValueQueueTrait, IParameterChanges, IParameterChangesTrait,
};
use manguebyte_vst3::vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, IPluginBaseTrait, IPluginFactory,
    IPluginFactoryTrait, ViewRect, kInvalidArgument, kResultFalse, kResultOk, kResultTrue, tresult,
};
use manguebyte_vst3::vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};
use manguebyte_vst3::{class_id, param_id, plugin_factory};

// --- Watching the heap -------------------------------------------------------
//
// `process` runs on the host's audio thread, so it must not allocate or free,
// whatever the editor does meanwhile. The allocator counts heap calls made
// while a thread is inside `process`.

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

// --- The plugins under test --------------------------------------------------

struct PanelParams {
    cutoff: FloatParam,
    mode: ChoiceParam,
    gain: FloatParam,
}
param_set!(PanelParams { cutoff, mode, gain });

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const MODE: ParamId = ParamId::from_name("mode");
const GAIN: ParamId = ParamId::from_name("gain");

fn panel_params() -> Arc<PanelParams> {
    Arc::new(PanelParams {
        cutoff: FloatParam::new("cutoff", "Cutoff", FloatRange::linear(0.0, 100.0), 25.0),
        mode: ChoiceParam::new("mode", "Mode", &["Low", "Band", "High"], 0),
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
    })
}

thread_local! {
    /// The parameters of the plugin most recently created on this thread,
    /// for a test to read as an interface would.
    static PARAMS: RefCell<Option<Arc<PanelParams>>> = const { RefCell::new(None) };
}

fn params() -> Arc<PanelParams> {
    PARAMS.with(|params| params.borrow().clone().expect("a plugin was created"))
}

/// A plugin with the fake editor below, set up by `SETUP`. It outputs the
/// cutoff it has been told about, following events alone.
struct Panel {
    params: Arc<PanelParams>,
    current: f32,
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
        Self {
            current: params.cutoff.value() as f32,
            params,
        }
    }
    fn params(&self) -> Arc<PanelParams> {
        Arc::clone(&self.params)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        split_at_events(
            &mut self.current,
            buffer,
            context.events,
            |current, event| {
                if let EventKind::ParamValue {
                    param: CUTOFF,
                    value,
                } = event.kind
                {
                    *current = value as f32;
                }
            },
            |current, piece| piece.channel_mut(0).fill(*current),
        );
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
        Self {
            params: panel_params(),
        }
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
    /// Asks for this size from inside `open`.
    resize_on_open: Option<HostSize>,
    /// The keys the editor uses; it leaves the rest to the host.
    used_keys: Vec<Key>,
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
            resize_on_open: None,
            used_keys: vec![Key::Up],
        }
    }
}

thread_local! {
    static SETUP: RefCell<Setup> = RefCell::new(Setup::default());
    static LOG: RefCell<Vec<EditorCall>> = const { RefCell::new(Vec::new()) };
    /// Every editor opened on this thread, in order.
    static PROBES: RefCell<Vec<Weak<Probe>>> = const { RefCell::new(Vec::new()) };
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
    Key(KeyEvent),
    OnMainThread,
    Dropped,
    /// An editor method was called, or the editor dropped, while it was
    /// calling the host.
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
        PROBES.with(|probes| probes.borrow_mut().push(Rc::downgrade(&probe)));
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

    fn waker(&self) -> Option<MainThreadWaker> {
        self.context.main_thread_waker()
    }
}

/// Runs `work` on the probe of the editor opened `back` editors ago on this
/// thread (0 for the latest). The probe is not kept, so that closing the
/// editor drops it, and with it whatever gestures it holds.
fn with_probe_back<R>(back: usize, work: impl FnOnce(&Probe) -> R) -> R {
    let probe = PROBES
        .with(|probes| {
            let probes = probes.borrow();
            probes.iter().rev().nth(back).and_then(Weak::upgrade)
        })
        .expect("that editor is open");
    work(&probe)
}

fn with_probe<R>(work: impl FnOnce(&Probe) -> R) -> R {
    with_probe_back(0, work)
}

/// Runs `work` on the probe of the latest editor still open on this
/// thread, if any.
fn with_any_probe<R>(work: impl FnOnce(&Probe) -> R) -> Option<R> {
    let probe = PROBES.with(|probes| probes.borrow().iter().rev().find_map(Weak::upgrade))?;
    Some(work(&probe))
}

/// How many editors opened on this thread are still alive.
fn editors_alive() -> usize {
    PROBES.with(|probes| {
        probes
            .borrow()
            .iter()
            .filter(|probe| probe.strong_count() > 0)
            .count()
    })
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
        let accepted = self.probe.rules().fit(self.probe.size.get(), size) == size;
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

    fn set_visible(&mut self, _visible: bool) {}

    fn key(&mut self, event: KeyEvent) -> bool {
        self.enter(EditorCall::Key(event));
        let used = self.probe.setup.used_keys.contains(&event.key);
        if used && event.pressed && event.key == Key::Up {
            // Arrow up nudges the cutoff, as one edit.
            let cutoff = params().cutoff.normalized();
            self.probe
                .out(|context| Gesture::set_once(context, CUTOFF, cutoff + 0.25));
        }
        used
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

/// Every call the plugin made to the host's objects, in order.
#[derive(Debug, Clone, Copy, PartialEq)]
enum HostCall {
    Begin(u32),
    Perform(u32, f64),
    End(u32),
    Restart(i32),
    ResizeView(i32, i32, i32, i32),
    RegisterFd(i32),
    UnregisterFd(i32),
    RegisterTimer(u64),
    UnregisterTimer(u64),
}

/// A reply the host makes from inside one of its functions.
type Reply = Box<dyn FnMut(&Reentry)>;

/// What a reply can reach: the plugin's view the host was called about,
/// if any.
struct Reentry {
    view: *mut IPlugView,
}

impl Reentry {
    fn on_size(&self, width: i32, height: i32) -> tresult {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe { ComRef::from_raw(self.view).unwrap().onSize(&mut rect) }
    }

    fn size(&self) -> (i32, i32) {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        unsafe {
            assert_eq!(ComRef::from_raw(self.view).unwrap().getSize(&mut rect), kResultOk);
        }
        (rect.right - rect.left, rect.bottom - rect.top)
    }
}

/// What the host's objects share: the calls made to them, their replies,
/// and the run loop's registrations.
#[derive(Default)]
struct HostState {
    calls: RefCell<Vec<HostCall>>,
    /// Run once inside `performEdit`, if set.
    on_perform: RefCell<Option<Reply>>,
    /// The frame's answer to `resizeView`: `None` grants the size asked for
    /// with `onSize`, as hosts do.
    on_resize: RefCell<Option<Box<dyn FnMut(&Reentry, i32, i32) -> tresult>>>,
    refuse_registrations: Cell<bool>,
    timers: RefCell<Vec<(ComPtr<ITimerHandler>, u64)>>,
    fds: RefCell<Vec<(ComPtr<IEventHandler>, i32)>>,
    /// Failures inside the host's functions, which may not panic across
    /// the interface; checked by the test.
    failures: RefCell<Vec<String>>,
}

impl HostState {
    fn record(&self, call: HostCall) {
        self.calls.borrow_mut().push(call);
    }

    fn take_calls(&self) -> Vec<HostCall> {
        self.calls.take()
    }

    /// The calls, leaving out the run loop's, which the wake of an editor
    /// makes on systems with Unix sockets.
    fn take_edits(&self) -> Vec<HostCall> {
        self.take_calls()
            .into_iter()
            .filter(|call| {
                !matches!(call, HostCall::RegisterFd(fd) | HostCall::UnregisterFd(fd) if *fd < 1000)
            })
            .collect()
    }

    fn fail(&self, message: String) {
        self.failures.borrow_mut().push(message);
    }

    fn assert_no_failures(&self) {
        assert_eq!(*self.failures.borrow(), Vec::<String>::new());
    }

    /// Runs `reply`, taken out of `slot` while it runs, once.
    fn run_once(slot: &RefCell<Option<Reply>>, reentry: &Reentry) {
        let reply = slot.borrow_mut().take();
        if let Some(mut reply) = reply {
            reply(reentry);
        }
    }

    /// Fires every timer once, as the host's loop does, and returns how
    /// many it fired.
    fn fire_timers(&self) -> usize {
        let timers: Vec<ComPtr<ITimerHandler>> =
            self.timers.borrow().iter().map(|(h, _)| h.clone()).collect();
        for timer in &timers {
            unsafe { timer.onTimer() };
        }
        timers.len()
    }

    /// Tells every handler of `fd` that it can be read.
    fn fire_fd(&self, fd: i32) -> usize {
        let handlers: Vec<ComPtr<IEventHandler>> = self
            .fds
            .borrow()
            .iter()
            .filter(|(_, known)| *known == fd)
            .map(|(h, _)| h.clone())
            .collect();
        for handler in &handlers {
            unsafe { handler.onFDIsSet(fd) };
        }
        handlers.len()
    }

    /// The descriptors registered, in order.
    fn watched(&self) -> Vec<i32> {
        self.fds.borrow().iter().map(|&(_, fd)| fd).collect()
    }

    fn timer_periods(&self) -> Vec<u64> {
        self.timers.borrow().iter().map(|&(_, ms)| ms).collect()
    }
}

/// The host's component handler.
struct Handler {
    host: Rc<HostState>,
    /// Set when the plugin has released its last reference.
    gone: Rc<Cell<bool>>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl Drop for Handler {
    fn drop(&mut self) {
        self.gone.set(true);
    }
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, id: u32) -> tresult {
        self.host.record(HostCall::Begin(id));
        kResultOk
    }
    unsafe fn performEdit(&self, id: u32, value: f64) -> tresult {
        self.host.record(HostCall::Perform(id, value));
        HostState::run_once(
            &self.host.on_perform,
            &Reentry {
                view: ptr::null_mut(),
            },
        );
        kResultOk
    }
    unsafe fn endEdit(&self, id: u32) -> tresult {
        self.host.record(HostCall::End(id));
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        self.host.record(HostCall::Restart(flags));
        kResultOk
    }
}

/// The host's frame, without a run loop, as on Windows and macOS.
struct Frame {
    host: Rc<HostState>,
    gone: Rc<Cell<bool>>,
}

/// The host's frame with a run loop, as on Linux.
struct LoopFrame {
    host: Rc<HostState>,
    gone: Rc<Cell<bool>>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl Class for LoopFrame {
    type Interfaces = (IPlugFrame, IRunLoop);
}

impl Drop for Frame {
    fn drop(&mut self) {
        self.gone.set(true);
    }
}

impl Drop for LoopFrame {
    fn drop(&mut self) {
        self.gone.set(true);
    }
}

fn resize_view(host: &HostState, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
    let rect = unsafe { *rect };
    host.record(HostCall::ResizeView(
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
    ));
    let reentry = Reentry { view };
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    let reply = host.on_resize.borrow_mut().take();
    match reply {
        Some(mut reply) => {
            let result = reply(&reentry, width, height);
            *host.on_resize.borrow_mut() = Some(reply);
            result
        }
        None => {
            // What hosts do: the window takes the size, and the view is
            // told before `resizeView` returns.
            if reentry.on_size(width, height) != kResultOk {
                host.fail("onSize refused the size asked for".into());
            }
            kResultOk
        }
    }
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        resize_view(&self.host, view, rect)
    }
}

impl IPlugFrameTrait for LoopFrame {
    unsafe fn resizeView(&self, view: *mut IPlugView, rect: *mut ViewRect) -> tresult {
        resize_view(&self.host, view, rect)
    }
}

impl IRunLoopTrait for LoopFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: i32) -> tresult {
        self.host.record(HostCall::RegisterFd(fd));
        if self.host.refuse_registrations.get() {
            return kResultFalse;
        }
        let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
            return kInvalidArgument;
        };
        self.host
            .fds
            .borrow_mut()
            .push((handler.to_com_ptr(), fd));
        kResultOk
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        let removed: Vec<(ComPtr<IEventHandler>, i32)> = {
            let mut fds = self.host.fds.borrow_mut();
            let (gone, kept) = fds.drain(..).partition(|(known, _)| known.as_ptr() == handler);
            *fds = kept;
            gone
        };
        if removed.is_empty() {
            self.host.fail("an unknown event handler was unregistered".into());
        }
        for &(_, fd) in &removed {
            self.host.record(HostCall::UnregisterFd(fd));
        }
        // Released after the table is put back, as a host would.
        drop(removed);
        kResultOk
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, milliseconds: u64) -> tresult {
        self.host.record(HostCall::RegisterTimer(milliseconds));
        if self.host.refuse_registrations.get() {
            return kResultFalse;
        }
        let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
            return kInvalidArgument;
        };
        self.host
            .timers
            .borrow_mut()
            .push((handler.to_com_ptr(), milliseconds));
        kResultOk
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        let removed: Vec<(ComPtr<ITimerHandler>, u64)> = {
            let mut timers = self.host.timers.borrow_mut();
            let (gone, kept) = timers
                .drain(..)
                .partition(|(known, _)| known.as_ptr() == handler);
            *timers = kept;
            gone
        };
        if removed.is_empty() {
            self.host.fail("an unknown timer was unregistered".into());
        }
        for &(_, ms) in &removed {
            self.host.record(HostCall::UnregisterTimer(ms));
        }
        drop(removed);
        kResultOk
    }
}

// --- A session with the plugin -----------------------------------------------

fn iid<I: Interface>() -> *const c_char {
    I::IID.as_ptr().cast()
}

/// A new instance of `P`, created through its factory and initialized.
fn create<P: Plugin>() -> ComPtr<IComponent> {
    unsafe {
        let factory = ComPtr::<IPluginFactory>::from_raw(plugin_factory::<P>().cast()).unwrap();
        let class = class_id(P::INFO.id);
        let mut object = ptr::null_mut();
        assert_eq!(
            factory.createInstance(class.as_ptr(), iid::<IComponent>(), &mut object),
            kResultOk
        );
        let component = ComPtr::<IComponent>::from_raw(object.cast()).unwrap();
        assert_eq!(component.initialize(ptr::null_mut()), kResultOk);
        component
    }
}

/// One plugin instance, created through its factory, with a host's handler.
struct Session {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ComPtr<IEditController>,
    host: Rc<HostState>,
    handler_gone: Rc<Cell<bool>>,
}

impl Session {
    fn new<P: Plugin>() -> Self {
        let host = Rc::new(HostState::default());
        let handler_gone = Rc::new(Cell::new(false));
        let component = create::<P>();
        unsafe {
            let controller: ComPtr<IEditController> = component.cast().unwrap();
            let handler = ComWrapper::new(Handler {
                host: Rc::clone(&host),
                gone: Rc::clone(&handler_gone),
            });
            let pointer = handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
            assert_eq!(controller.setComponentHandler(pointer), kResultOk);
            // From here on only the plugin holds the handler.
            drop(handler);
            Self {
                processor: component.cast().unwrap(),
                controller,
                component,
                host,
                handler_gone,
            }
        }
    }

    fn panel() -> Self {
        Self::new::<Panel>()
    }

    /// Prepares the plugin, without blocks running yet.
    fn activate(&self) {
        let mut setup = vst::ProcessSetup {
            processMode: vst::ProcessModes_::kRealtime as i32,
            symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
            maxSamplesPerBlock: 64,
            sampleRate: 48_000.0,
        };
        unsafe {
            assert_eq!(self.processor.setupProcessing(&mut setup), kResultOk);
            assert_eq!(self.component.setActive(1), kResultOk);
        }
    }

    fn set_processing(&self, on: bool) {
        unsafe { assert_eq!(self.processor.setProcessing(u8::from(on)), kResultOk) };
    }

    fn create_view(&self, name: Option<&CStr>) -> Option<ComPtr<IPlugView>> {
        let name = name.map_or(ptr::null(), CStr::as_ptr);
        unsafe { ComPtr::from_raw(self.controller.createView(name)) }
    }

    fn view(&self) -> Ui {
        Ui {
            view: self.create_view(Some(c"editor")).expect("a view"),
        }
    }

    /// The controller's value of `param`.
    fn shown(&self, param: ParamId) -> f64 {
        unsafe { self.controller.getParamNormalized(param_id(param)) }
    }

    fn terminate(&self) {
        unsafe { assert_eq!(self.component.terminate(), kResultOk) };
    }

    /// Processes one block of `frames` with these automation points, and
    /// returns the output and how many heap calls `process` made.
    fn process(&self, frames: usize, changes: Vec<(u32, Vec<(i32, f64)>)>) -> (Vec<f32>, usize) {
        process_block(&self.processor, frames, changes)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        unsafe {
            self.processor.setProcessing(0);
            self.component.setActive(0);
            self.component.terminate();
        }
    }
}

/// The automation points of one parameter in one block.
struct Queue {
    id: u32,
    points: Vec<(i32, f64)>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> u32 {
        self.id
    }
    unsafe fn getPointCount(&self) -> i32 {
        self.points.len() as i32
    }
    unsafe fn getPoint(&self, index: i32, offset: *mut i32, value: *mut f64) -> tresult {
        let Some(&(at, to)) = self.points.get(index as usize) else {
            return kResultFalse;
        };
        unsafe {
            *offset = at;
            *value = to;
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _offset: i32, _value: f64, _index: *mut i32) -> tresult {
        kResultFalse
    }
}

struct Changes {
    queues: Vec<ComWrapper<Queue>>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> i32 {
        self.queues.len() as i32
    }
    unsafe fn getParameterData(&self, index: i32) -> *mut IParamValueQueue {
        self.queues
            .get(index as usize)
            .and_then(|queue| queue.as_com_ref::<IParamValueQueue>())
            .map_or(ptr::null_mut(), |queue| queue.as_ptr())
    }
    unsafe fn addParameterData(&self, _id: *const u32, _index: *mut i32) -> *mut IParamValueQueue {
        ptr::null_mut()
    }
}

/// One block through `processor`, on this thread: the output's one channel
/// and the heap calls `process` made.
fn process_block(
    processor: &ComPtr<IAudioProcessor>,
    frames: usize,
    changes: Vec<(u32, Vec<(i32, f64)>)>,
) -> (Vec<f32>, usize) {
    let mut output = vec![-1.0f32; frames];
    let mut channels = [output.as_mut_ptr()];
    let mut outputs = [vst::AudioBusBuffers {
        numChannels: 1,
        silenceFlags: 0,
        __field0: vst::AudioBusBuffers__type0 {
            channelBuffers32: channels.as_mut_ptr(),
        },
    }];
    let changes = ComWrapper::new(Changes {
        queues: changes
            .into_iter()
            .map(|(id, points)| ComWrapper::new(Queue { id, points }))
            .collect(),
    });
    let mut data = vst::ProcessData {
        processMode: vst::ProcessModes_::kRealtime as i32,
        symbolicSampleSize: vst::SymbolicSampleSizes_::kSample32 as i32,
        numSamples: frames as i32,
        numInputs: 0,
        numOutputs: 1,
        inputs: ptr::null_mut(),
        outputs: outputs.as_mut_ptr(),
        inputParameterChanges: changes.as_com_ref::<IParameterChanges>().unwrap().as_ptr(),
        outputParameterChanges: ptr::null_mut(),
        inputEvents: ptr::null_mut(),
        outputEvents: ptr::null_mut(),
        processContext: ptr::null_mut(),
    };
    HEAP_CALLS.with(|calls| calls.set(0));
    IN_PROCESS.with(|watching| watching.set(true));
    let status = unsafe { processor.process(&mut data) };
    IN_PROCESS.with(|watching| watching.set(false));
    assert_eq!(status, kResultOk);
    (output, HEAP_CALLS.with(Cell::get))
}

/// A view of the plugin's editor, as the host holds it.
struct Ui {
    view: ComPtr<IPlugView>,
}

const PARENT: usize = 0x0004_05A2;

impl Ui {
    fn supports(&self, kind: Option<&CStr>) -> tresult {
        unsafe {
            self.view
                .isPlatformTypeSupported(kind.map_or(ptr::null(), CStr::as_ptr))
        }
    }

    fn attach(&self, kind: &CStr, parent: usize) -> tresult {
        unsafe {
            self.view
                .attached(ptr::without_provenance_mut::<c_void>(parent), kind.as_ptr())
        }
    }

    fn attach_hwnd(&self) -> tresult {
        self.attach(c"HWND", PARENT)
    }

    fn remove(&self) -> tresult {
        unsafe { self.view.removed() }
    }

    fn size(&self) -> HostSize {
        let mut rect = ViewRect {
            left: 7,
            top: 7,
            right: 0,
            bottom: 0,
        };
        unsafe { assert_eq!(self.view.getSize(&mut rect), kResultOk) };
        assert_eq!((rect.left, rect.top), (0, 0));
        HostSize::new(rect.right as u32, rect.bottom as u32)
    }

    fn on_size(&self, width: i32, height: i32) -> tresult {
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        };
        unsafe { self.view.onSize(&mut rect) }
    }

    /// `checkSizeConstraint` on a rectangle at 10, 20.
    fn check(&self, width: i32, height: i32) -> (tresult, ViewRect) {
        let mut rect = ViewRect {
            left: 10,
            top: 20,
            right: 10 + width,
            bottom: 20 + height,
        };
        let result = unsafe { self.view.checkSizeConstraint(&mut rect) };
        (result, rect)
    }

    fn can_resize(&self) -> tresult {
        unsafe { self.view.canResize() }
    }

    fn scale(&self, factor: f32) -> tresult {
        let scaling: ComPtr<IPlugViewContentScaleSupport> = self.view.cast().unwrap();
        unsafe { scaling.setContentScaleFactor(factor) }
    }

    fn key_down(&self, character: u16, code: u32, modifiers: u32) -> tresult {
        unsafe {
            self.view
                .onKeyDown(character, code as i16, modifiers as i16)
        }
    }

    fn key_up(&self, character: u16, code: u32, modifiers: u32) -> tresult {
        unsafe { self.view.onKeyUp(character, code as i16, modifiers as i16) }
    }

    fn set_frame(&self, frame: *mut IPlugFrame) -> tresult {
        unsafe { self.view.setFrame(frame) }
    }

    /// Gives the view a frame without a run loop; returns its drop flag.
    fn give_frame(&self, host: &Rc<HostState>) -> Rc<Cell<bool>> {
        let gone = Rc::new(Cell::new(false));
        let frame = ComWrapper::new(Frame {
            host: Rc::clone(host),
            gone: Rc::clone(&gone),
        });
        let pointer = frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        assert_eq!(self.set_frame(pointer), kResultOk);
        gone
    }

    /// Gives the view a frame with a run loop; returns its drop flag.
    fn give_loop_frame(&self, host: &Rc<HostState>) -> Rc<Cell<bool>> {
        let gone = Rc::new(Cell::new(false));
        let frame = ComWrapper::new(LoopFrame {
            host: Rc::clone(host),
            gone: Rc::clone(&gone),
        });
        let pointer = frame.as_com_ref::<IPlugFrame>().unwrap().as_ptr();
        assert_eq!(self.set_frame(pointer), kResultOk);
        gone
    }
}

fn win32() -> ParentWindow {
    ParentWindow::win32(PARENT).unwrap()
}

// --- The component handler ---------------------------------------------------

#[test]
fn a_plugin_without_an_editor_offers_no_view_and_one_with_offers_only_the_editor() {
    let bare = Session::new::<Bare>();
    assert!(bare.create_view(Some(c"editor")).is_none());
    drop(bare);

    let session = Session::panel();
    assert!(session.create_view(None).is_none());
    assert!(session.create_view(Some(c"Editor")).is_none());
    assert!(session.create_view(Some(c"")).is_none());
    let view = session.create_view(Some(c"editor")).expect("an editor view");
    // A view answers for its two interfaces and nothing else.
    assert!(view.cast::<IPlugViewContentScaleSupport>().is_some());
    assert!(view.cast::<IComponent>().is_none());
    assert_eq!(editors_alive(), 0, "creating a view opens nothing");
}

#[test]
fn the_component_handler_is_kept_until_the_host_takes_it_away() {
    let session = Session::panel();
    // The test gave its handler and kept no reference: the plugin's keeps
    // it alive.
    assert!(!session.handler_gone.get());

    // Setting the same handler again changes nothing.
    let host = Rc::clone(&session.host);
    let gone_second = Rc::new(Cell::new(false));
    let second = ComWrapper::new(Handler {
        host: Rc::clone(&host),
        gone: Rc::clone(&gone_second),
    });
    let pointer = second.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
    unsafe {
        assert_eq!(session.controller.setComponentHandler(pointer), kResultOk);
        assert_eq!(session.controller.setComponentHandler(pointer), kResultOk);
    }
    drop(second);
    // Replaced: the first is released, the second kept.
    assert!(session.handler_gone.get());
    assert!(!gone_second.get());
    // Taken away: released.
    unsafe {
        assert_eq!(
            session.controller.setComponentHandler(ptr::null_mut()),
            kResultOk
        );
    }
    assert!(gone_second.get());

    // `terminate` releases it too, and so does releasing the component.
    for terminate in [true, false] {
        let gone = Rc::new(Cell::new(false));
        let handler = ComWrapper::new(Handler {
            host: Rc::clone(&host),
            gone: Rc::clone(&gone),
        });
        let pointer = handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
        let component = create::<Panel>();
        let controller: ComPtr<IEditController> = component.cast().unwrap();
        unsafe { assert_eq!(controller.setComponentHandler(pointer), kResultOk) };
        drop(handler);
        assert!(!gone.get());
        if terminate {
            unsafe { assert_eq!(component.terminate(), kResultOk) };
            assert!(gone.get(), "terminate kept the handler");
        }
        drop(controller);
        drop(component);
        assert!(gone.get(), "the component kept the handler");
    }
    assert_eq!(host.take_calls(), []);
}

/// An edit made while no blocks run: the host hears begin, every value the
/// control set, and end; the controller reports the value at once; and the
/// value is stored at once, so the plugin hears about it on the first frame
/// of the next block.
#[test]
fn an_edit_reaches_the_host_as_begin_perform_and_end() {
    let session = Session::panel();
    session.activate();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    let cutoff = param_id(CUTOFF);

    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
        probe.move_to(0.75);
        // Held against the same value: not sent again.
        probe.move_to(0.75);
    });
    assert_eq!(session.shown(CUTOFF), 0.75);
    assert_eq!(params().cutoff.value(), 75.0);
    with_probe(Probe::release);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::Perform(cutoff, 0.75),
            HostCall::End(cutoff),
        ]
    );

    // A whole edit in one go, on another parameter.
    with_probe(|probe| {
        probe.out(|context| Gesture::set_once(context, MODE, 1.0));
    });
    let mode = param_id(MODE);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(mode),
            HostCall::Perform(mode, 1.0),
            HostCall::End(mode),
        ]
    );
    assert_eq!(params().mode.index(), 2);

    session.set_processing(true);
    let (output, heap_calls) = session.process(8, vec![]);
    assert_eq!(output, [75.0; 8], "the plugin was not told on frame 0");
    assert_eq!(heap_calls, 0);
    session.host.assert_no_failures();
}

/// While blocks run, the host's points stay the only source of a value: an
/// edit changes what the controller reports, and reaches the plugin when the
/// host sends it back as automation, on the frame the host chooses.
#[test]
fn while_blocks_run_an_edit_reaches_the_plugin_only_through_the_host() {
    let session = Session::panel();
    session.activate();
    session.set_processing(true);
    let (output, _) = session.process(8, vec![]);
    assert_eq!(output, [25.0; 8]);

    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.75);
        probe.release();
    });
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.75),
            HostCall::End(cutoff),
        ]
    );
    assert_eq!(session.shown(CUTOFF), 0.75);
    assert_eq!(params().cutoff.value(), 25.0, "the editor wrote the parameter");

    // A block before the host's echo: nothing has changed for the plugin.
    let (output, heap_calls) = session.process(8, vec![]);
    assert_eq!(output, [25.0; 8]);
    assert_eq!(heap_calls, 0);
    // The echo, as a point on frame 4.
    let (output, heap_calls) = session.process(8, vec![(cutoff, vec![(4, 0.75)])]);
    assert_eq!(output, [25.0, 25.0, 25.0, 25.0, 75.0, 75.0, 75.0, 75.0]);
    assert_eq!(heap_calls, 0);
    assert_eq!(params().cutoff.value(), 75.0);

    // Once the host stops processing, edits are stored at once again.
    session.set_processing(false);
    with_probe(|probe| {
        probe.out(|context| Gesture::set_once(context, CUTOFF, 0.5));
    });
    assert_eq!(params().cutoff.value(), 50.0);
}

#[test]
fn edits_from_two_views_on_one_parameter_reach_the_host_as_one() {
    let session = Session::panel();
    let (first, second) = (session.view(), session.view());
    assert_eq!(first.attach_hwnd(), kResultOk);
    assert_eq!(second.attach(c"HWND", PARENT + 8), kResultOk);
    assert_eq!(editors_alive(), 2);
    let cutoff = param_id(CUTOFF);

    with_probe_back(1, |probe| probe.drag(CUTOFF, 0.25));
    with_probe_back(0, |probe| probe.drag(CUTOFF, 0.25));
    with_probe_back(1, |probe| probe.move_to(0.5));
    with_probe_back(0, |probe| probe.move_to(0.75));
    with_probe_back(1, Probe::release);
    with_probe_back(0, |probe| probe.move_to(0.875));
    with_probe_back(0, Probe::release);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::Perform(cutoff, 0.75),
            HostCall::Perform(cutoff, 0.875),
            HostCall::End(cutoff),
        ]
    );
}

#[test]
fn a_handler_replaced_during_an_edit_sees_it_end_and_the_new_one_sees_it_begin() {
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    let cutoff = param_id(CUTOFF);
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });

    let newer = Rc::new(HostState::default());
    let gone = Rc::new(Cell::new(false));
    let handler = ComWrapper::new(Handler {
        host: Rc::clone(&newer),
        gone: Rc::clone(&gone),
    });
    let pointer = handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr();
    unsafe { assert_eq!(session.controller.setComponentHandler(pointer), kResultOk) };
    drop(handler);
    assert!(session.handler_gone.get(), "the old handler was kept");

    with_probe(|probe| {
        probe.move_to(0.75);
        probe.release();
    });
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
        ]
    );
    assert_eq!(
        newer.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.75),
            HostCall::End(cutoff),
        ]
    );

    // Taken away in the middle of an edit: ended on the way out, and what
    // follows reaches nobody.
    with_probe(|probe| probe.drag(GAIN, 0.5));
    unsafe {
        assert_eq!(
            session.controller.setComponentHandler(ptr::null_mut()),
            kResultOk
        );
    }
    let gain = param_id(GAIN);
    assert_eq!(newer.take_edits(), [HostCall::Begin(gain), HostCall::End(gain)]);
    assert!(gone.get());
    with_probe(|probe| {
        probe.move_to(0.25);
        probe.release();
    });
    assert_eq!(params().gain.value(), 0.25, "the value was lost");
    assert_eq!(session.shown(GAIN), 0.25);
}

#[test]
fn values_the_editor_changed_without_gestures_are_read_again() {
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    // A preset of the editor's own, written straight into the parameters.
    params().gain.set(0.125);
    params().cutoff.set(80.0);
    assert_eq!(session.shown(GAIN), 0.5);
    with_probe(|probe| probe.out(EditorContext::values_changed));
    assert_eq!(
        session.host.take_edits(),
        [HostCall::Restart(
            vst::RestartFlags_::kParamValuesChanged as i32
        )]
    );
    assert_eq!(session.shown(GAIN), 0.125);
    assert_eq!(session.shown(CUTOFF), 0.8);
}

// --- The view's lifecycle ----------------------------------------------------

#[test]
fn a_view_refuses_to_be_removed_unattached_or_attached_twice() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.remove(), kResultFalse);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(ui.attach_hwnd(), kResultFalse);
    assert_eq!(ui.attach(c"X11EmbedWindowID", 0x1234), kResultFalse);
    assert_eq!(take_log(), [EditorCall::Open(win32(), None)]);
    assert_eq!(ui.remove(), kResultOk);
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert_eq!(ui.remove(), kResultFalse);

    // And again, any number of times, with another kind of window.
    for _ in 0..3 {
        assert_eq!(ui.attach(c"X11EmbedWindowID", 0x1234), kResultOk);
        assert_eq!(ui.remove(), kResultOk);
    }
    let x11 = ParentWindow::x11(0x1234).unwrap();
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(x11, None),
            EditorCall::Dropped,
            EditorCall::Open(x11, None),
            EditorCall::Dropped,
            EditorCall::Open(x11, None),
            EditorCall::Dropped,
        ]
    );
    assert_eq!(editors_alive(), 0);
}

#[test]
fn platform_types_and_parents_are_checked() {
    set_up(Setup {
        supports: [true, true, false],
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.supports(Some(c"HWND")), kResultTrue);
    assert_eq!(ui.supports(Some(c"NSView")), kResultTrue);
    assert_eq!(ui.supports(Some(c"X11EmbedWindowID")), kResultFalse);
    assert_eq!(ui.supports(Some(c"WaylandSurfaceID")), kResultFalse);
    assert_eq!(ui.supports(Some(c"HIView")), kResultFalse);
    assert_eq!(ui.supports(Some(c"")), kResultFalse);
    assert_eq!(ui.supports(None), kResultFalse);

    assert_eq!(ui.attach(c"X11EmbedWindowID", 0x1234), kResultFalse);
    assert_eq!(ui.attach(c"UIView", PARENT), kResultFalse);
    let parent = ptr::without_provenance_mut::<c_void>(PARENT);
    unsafe { assert_eq!(ui.view.attached(parent, ptr::null()), kResultFalse) };
    assert_eq!(ui.attach(c"HWND", 0), kInvalidArgument);
    assert_eq!(take_log(), []);
    assert_eq!(ui.attach(c"NSView", 0x7000), kResultOk);
    assert_eq!(
        take_log(),
        [EditorCall::Open(ParentWindow::cocoa(0x7000).unwrap(), None)]
    );
}

#[test]
fn a_view_outliving_its_plugin_is_left_inert() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let frame_gone = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        assert!(probe.every(20));
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    take_log();
    let host = Rc::clone(&session.host);
    let handler_gone = Rc::clone(&session.handler_gone);
    // The host lets go of the plugin while its editor is open.
    drop(session);

    // The editor was closed first, its edit ended on the handler, its timer
    // unregistered, and only then the handler released.
    assert_eq!(take_log(), [EditorCall::Dropped]);
    let cutoff = param_id(CUTOFF);
    let calls = host.take_edits();
    assert_eq!(
        calls,
        [
            HostCall::RegisterTimer(20),
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
            HostCall::UnregisterTimer(20),
        ]
    );
    assert!(handler_gone.get());
    assert_eq!(editors_alive(), 0);

    // The view answers without an editor, and opens none.
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(ui.on_size(400, 300), kResultOk);
    assert_eq!(ui.check(9_999, 9_999).0, kResultTrue);
    assert_eq!(ui.can_resize(), kResultTrue);
    assert_eq!(ui.scale(2.0), kResultOk);
    assert_eq!(ui.key_down(0, 12, 0), kResultFalse);
    assert_eq!(ui.attach_hwnd(), kResultFalse);
    // It counts as attached until the host removes it.
    assert_eq!(ui.remove(), kResultOk);
    assert_eq!(ui.remove(), kResultFalse);
    assert_eq!(ui.attach_hwnd(), kResultFalse);
    assert_eq!(take_log(), []);
    assert!(!frame_gone.get());
    drop(ui);
    assert!(frame_gone.get(), "the view kept the frame");
    assert!(host.timers.borrow().is_empty() && host.fds.borrow().is_empty());
    host.assert_no_failures();
}

#[test]
fn terminate_closes_the_editors_and_refuses_new_views_until_initialized() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    session.terminate();
    assert_eq!(take_log(), [EditorCall::Open(win32(), None), EditorCall::Dropped]);
    assert!(session.create_view(Some(c"editor")).is_none());
    assert_eq!(ui.remove(), kResultOk);
    assert_eq!(ui.attach_hwnd(), kResultFalse);

    unsafe { assert_eq!(session.component.initialize(ptr::null_mut()), kResultOk) };
    let again = session.view();
    assert_eq!(again.attach_hwnd(), kResultOk);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(editors_alive(), 2);
}

#[test]
fn a_view_released_while_attached_closes_its_editor_and_lets_go_of_the_frame() {
    set_up(Setup {
        timer_on_open: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    let frame_gone = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    // A watch the editor leaks: the link takes care of it.
    let leaked = with_probe(|probe| {
        assert!(probe.watch(1005));
        std::mem::ManuallyDrop::new(probe.handles.borrow_mut().pop())
    });
    assert_eq!(session.host.timer_periods(), [16]);
    drop(ui);
    assert_eq!(take_log(), [EditorCall::Open(win32(), None), EditorCall::Dropped]);
    assert!(session.host.timers.borrow().is_empty());
    assert!(session.host.fds.borrow().is_empty());
    assert!(frame_gone.get());
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::RegisterTimer(16),
            HostCall::RegisterFd(1005),
            HostCall::UnregisterTimer(16),
            HostCall::UnregisterFd(1005),
        ]
    );
    // Dropped after all, much later, the handle calls nothing.
    drop(std::mem::ManuallyDrop::into_inner(leaked));
    assert_eq!(session.host.take_calls(), []);
    session.host.assert_no_failures();
}

#[test]
fn an_editor_that_fails_to_open_leaves_nothing_behind() {
    set_up(Setup {
        fail_open: true,
        timer_on_open: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    let _frame = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultFalse);
    assert_eq!(take_log(), [EditorCall::Open(win32(), None)]);
    assert_eq!(
        session.host.take_edits(),
        [HostCall::RegisterTimer(16), HostCall::UnregisterTimer(16)]
    );
    assert!(session.host.fds.borrow().is_empty(), "the wake was left behind");
    // Not attached, so not removable.
    assert_eq!(ui.remove(), kResultFalse);
    session.host.assert_no_failures();
}

#[test]
fn an_editor_that_says_it_cannot_go_on_is_dropped_at_the_next_call() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| probe.out(EditorContext::closed));
    // VST 3 has no way to tell the host; the next call drops the editor.
    assert_eq!(editors_alive(), 1);
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(editors_alive(), 0);
    assert_eq!(take_log(), [EditorCall::Open(win32(), None), EditorCall::Dropped]);
    assert_eq!(ui.remove(), kResultOk);
    assert_eq!(ui.attach_hwnd(), kResultOk);
}

// --- Sizes and scale ---------------------------------------------------------

#[test]
fn sizes_follow_the_editors_rules_before_and_after_it_opens() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(ui.can_resize(), kResultTrue);
    // Before the editor is open the spec's rules answer, and the rectangle
    // keeps its corner.
    let (result, rect) = ui.check(9_999, 1);
    assert_eq!(result, kResultTrue);
    assert_eq!(
        (rect.left, rect.top, rect.right, rect.bottom),
        (10, 20, 1210, 120)
    );
    // A size set before the editor is open is kept for it, if the rules
    // accept it.
    assert_eq!(ui.on_size(10, 10), kResultFalse);
    assert_eq!(ui.on_size(500, 400), kResultOk);
    assert_eq!(ui.size(), HostSize::new(500, 400));
    assert_eq!(take_log(), []);

    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32(), None),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(500, 400), true),
        ]
    );
    // Now the editor answers.
    assert_eq!(ui.size(), HostSize::new(500, 400));
    let (result, rect) = ui.check(5_000, 50);
    assert_eq!(result, kResultTrue);
    assert_eq!(
        (rect.left, rect.top, rect.right, rect.bottom),
        (10, 20, 1210, 120)
    );
    assert_eq!(ui.on_size(600, 500), kResultOk);
    assert_eq!(ui.on_size(5, 5), kResultFalse);
    assert_eq!(
        take_log(),
        [
            EditorCall::Size,
            EditorCall::Fit(HostSize::new(5_000, 50)),
            EditorCall::SetSize(HostSize::new(600, 500), true),
            EditorCall::SetSize(HostSize::new(5, 5), false),
        ]
    );

    // Hostile rectangles.
    unsafe {
        assert_eq!(ui.view.getSize(ptr::null_mut()), kInvalidArgument);
        assert_eq!(ui.view.onSize(ptr::null_mut()), kInvalidArgument);
        assert_eq!(ui.view.checkSizeConstraint(ptr::null_mut()), kInvalidArgument);
    }
    assert_eq!(ui.on_size(-5, 10), kInvalidArgument);
    let mut inside_out = ViewRect {
        left: 100,
        top: 0,
        right: 0,
        bottom: 100,
    };
    unsafe { assert_eq!(ui.view.checkSizeConstraint(&mut inside_out), kInvalidArgument) };
    assert_eq!(take_log(), []);
}

#[test]
fn a_fixed_editor_and_a_shaped_one_keep_to_their_rules() {
    set_up(Setup {
        resize: Resize::Fixed,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.can_resize(), kResultFalse);
    let (_, rect) = ui.check(9_999, 9_999);
    assert_eq!((rect.right, rect.bottom), (310, 220));
    // A host that sets the editor's own size is answered yes.
    assert_eq!(ui.on_size(300, 200), kResultOk);
    assert_eq!(ui.on_size(301, 200), kResultFalse);
    drop((ui, session));

    set_up(Setup {
        resize: Resize::Ratio {
            min: HostSize::new(160, 90),
            max: HostSize::new(1600, 900),
            width: 16,
            height: 9,
        },
        base: HostSize::new(320, 180),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.can_resize(), kResultTrue);
    let (_, rect) = ui.check(1000, 1000);
    assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (1000, 563));
    assert_eq!(ui.attach_hwnd(), kResultOk);
    let (_, rect) = ui.check(10, 10);
    assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (160, 90));
}

#[test]
fn the_hosts_content_scale_reaches_the_editor() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    for hostile in [0.0, -1.0, 0.1, 100.0, f32::NAN, f32::INFINITY] {
        assert_eq!(ui.scale(hostile), kResultFalse, "{hostile}");
    }
    assert_eq!(ui.scale(1.5), kResultOk);
    assert_eq!(ui.size(), HostSize::new(450, 300));
    // The limits grow with the scale.
    let (_, rect) = ui.check(1, 1);
    assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (225, 150));
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(take_log(), [EditorCall::Open(win32(), Some(1.5))]);
    assert_eq!(ui.size(), HostSize::new(450, 300));

    assert_eq!(ui.scale(2.0), kResultOk);
    assert_eq!(ui.size(), HostSize::new(600, 400));
    assert_eq!(ui.scale(f32::NAN), kResultFalse);
    assert_eq!(
        take_log(),
        [
            EditorCall::Size,
            EditorCall::SetScale(2.0),
            EditorCall::Size,
        ]
    );
    assert_eq!(ui.remove(), kResultOk);
    take_log();

    // Cocoa: points, which the system scales; the editor opens without the
    // host's scale and refuses it.
    assert_eq!(ui.attach(c"NSView", 0x7000), kResultOk);
    assert_eq!(
        take_log(),
        [EditorCall::Open(ParentWindow::cocoa(0x7000).unwrap(), None)]
    );
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(ui.scale(2.0), kResultFalse);
    assert_eq!(ui.remove(), kResultOk);
    drop((ui, session));

    // An editor that cannot take a new scale says so.
    set_up(Setup {
        accept_scale: false,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(ui.scale(2.0), kResultFalse);
    assert_eq!(ui.size(), HostSize::new(300, 200));
}

#[test]
fn the_editors_resize_requests_go_through_the_frame() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();
    // No frame: no way to ask.
    assert_eq!(with_probe(|probe| probe.request_resize(HostSize::new(400, 300))), None);

    // A host that sets the size from inside `resizeView`, as hosts do: that
    // is the answer, and the editor is not called for it.
    let _frame = ui.give_frame(&session.host);
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(400, 300))),
        Some(HostSize::new(400, 300))
    );
    assert_eq!(
        session.host.take_calls(),
        [HostCall::ResizeView(0, 0, 400, 300)]
    );
    assert_eq!(take_log(), []);
    assert_eq!(ui.size(), HostSize::new(400, 300));
    take_log();

    // The host gives another size, and asks the size meanwhile, which is
    // answered without the editor.
    let sizes_seen = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&sizes_seen);
    *session.host.on_resize.borrow_mut() = Some(Box::new(move |reentry, _, _| {
        seen.borrow_mut().push(reentry.size());
        assert_eq!(reentry.on_size(500, 350), kResultOk);
        seen.borrow_mut().push(reentry.size());
        kResultFalse
    }));
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(800, 600))),
        Some(HostSize::new(500, 350))
    );
    assert_eq!(*sizes_seen.borrow(), [(800, 600), (500, 350)]);
    assert_eq!(take_log(), []);

    // A host that refuses.
    *session.host.on_resize.borrow_mut() = Some(Box::new(|_, _, _| kResultFalse));
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(700, 700))),
        None
    );
    // The frame taken away.
    assert_eq!(ui.set_frame(ptr::null_mut()), kResultOk);
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(700, 700))),
        None
    );
    session.host.assert_no_failures();
}

#[test]
fn an_editor_may_ask_for_its_size_while_it_opens() {
    set_up(Setup {
        resize_on_open: Some(HostSize::new(640, 480)),
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    let _frame = ui.give_frame(&session.host);
    // A size the host set earlier is out of date once it granted another.
    assert_eq!(ui.on_size(500, 400), kResultOk);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(
        session.host.take_calls(),
        [HostCall::ResizeView(0, 0, 640, 480)]
    );
    assert_eq!(take_log(), [EditorCall::Open(win32(), None)]);
    assert_eq!(ui.size(), HostSize::new(640, 480));
}

// --- Keys --------------------------------------------------------------------

#[test]
fn keys_reach_the_editor_and_the_ones_it_does_not_use_go_back() {
    use manguebyte_vst3::vst3::Steinberg::KeyModifier_ as bits;
    use manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_ as codes;

    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    // No editor: nothing to give a key to.
    assert_eq!(ui.key_down(0, codes::KEY_UP as u32, 0), kResultFalse);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();

    let shift = bits::kShiftKey as u32;
    assert_eq!(ui.key_down(0, codes::KEY_UP as u32, shift), kResultTrue);
    assert_eq!(ui.key_up(0, codes::KEY_UP as u32, 0), kResultTrue);
    assert_eq!(ui.key_down(u16::from(b'q'), 0, 0), kResultFalse);
    let with_shift = Modifiers {
        shift: true,
        ..Modifiers::NONE
    };
    assert_eq!(
        take_log(),
        [
            EditorCall::Key(KeyEvent {
                key: Key::Up,
                modifiers: with_shift,
                pressed: true
            }),
            EditorCall::Key(KeyEvent {
                key: Key::Up,
                modifiers: Modifiers::NONE,
                pressed: false
            }),
            EditorCall::Key(KeyEvent {
                key: Key::Character('q'),
                modifiers: Modifiers::NONE,
                pressed: true
            }),
        ]
    );
    // The key moved the cutoff, as one edit.
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
        ]
    );
    unsafe {
        assert_eq!(ui.view.onWheel(1.0), kResultFalse);
        assert_eq!(ui.view.onFocus(1), kResultOk);
    }
    assert_eq!(ui.remove(), kResultOk);
    unsafe { assert_eq!(ui.view.onFocus(1), kResultFalse) };
}

// --- The host's loop ---------------------------------------------------------

#[test]
fn timers_and_descriptors_go_to_the_hosts_run_loop() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_loop_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    host.take_edits();

    with_probe(|probe| {
        assert!(probe.context.has_host_loop());
        assert!(probe.every(16));
        assert!(probe.every(0));
        assert!(probe.watch(1005));
        assert!(probe.watch(1005));
        assert!(probe.watch(1006));
    });
    // One registration per descriptor; a period of 0 is asked for as 1 ms.
    assert_eq!(
        host.take_edits(),
        [
            HostCall::RegisterTimer(16),
            HostCall::RegisterTimer(1),
            HostCall::RegisterFd(1005),
            HostCall::RegisterFd(1006),
        ]
    );
    assert_eq!(host.fire_timers(), 2);
    assert_eq!(host.fire_fd(1005), 1);
    assert_eq!(with_probe(|probe| probe.ticks.get()), 202);

    // The registration goes with the last watch on its descriptor.
    with_probe(|probe| probe.drop_handle(2));
    assert_eq!(host.take_edits(), []);
    with_probe(|probe| probe.drop_handle(2));
    assert_eq!(host.take_edits(), [HostCall::UnregisterFd(1005)]);
    with_probe(|probe| probe.drop_handle(0));
    assert_eq!(host.take_edits(), [HostCall::UnregisterTimer(16)]);

    // A host that has run out refuses, and nothing is kept.
    host.refuse_registrations.set(true);
    with_probe(|probe| {
        assert!(!probe.every(10));
        assert!(!probe.watch(1007));
    });
    host.refuse_registrations.set(false);
    assert_eq!(
        host.take_edits(),
        [HostCall::RegisterTimer(10), HostCall::RegisterFd(1007)]
    );

    // Closing unregisters what is left, the wake included.
    assert_eq!(ui.remove(), kResultOk);
    assert!(host.timers.borrow().is_empty());
    assert!(host.fds.borrow().is_empty());
    assert_eq!(
        host.take_edits(),
        [HostCall::UnregisterTimer(1), HostCall::UnregisterFd(1006)]
    );
    host.assert_no_failures();

    // Without a run loop there is none to offer.
    let other = session.view();
    let _plain = other.give_frame(&host);
    assert_eq!(other.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        assert!(!probe.context.has_host_loop());
        assert!(!probe.every(16));
        assert!(probe.waker().is_none());
    });
}

#[test]
fn a_timer_may_cancel_itself_and_nothing_fires_while_the_editor_calls_out() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_loop_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);

    // A timer that drops its own handle on its second tick.
    let slot: Rc<RefCell<Option<LoopHandle>>> = Rc::new(RefCell::new(None));
    let ticks = Rc::new(Cell::new(0));
    let (inside, counted) = (Rc::clone(&slot), Rc::clone(&ticks));
    let handle = with_probe(|probe| {
        probe.context.every(5, move || {
            counted.set(counted.get() + 1);
            if counted.get() == 2 {
                drop(inside.borrow_mut().take());
            }
        })
    });
    *slot.borrow_mut() = handle;
    host.fire_timers();
    host.fire_timers();
    host.fire_timers();
    assert_eq!(ticks.get(), 2);
    assert!(host.timer_periods().is_empty());

    // Inside `performEdit` the host runs its loop: the editor's timer does
    // not fire under its feet.
    with_probe(|probe| assert!(probe.every(16)));
    let fired = Rc::new(Cell::new(usize::MAX));
    let host_inside = Rc::clone(&host);
    let fired_inside = Rc::clone(&fired);
    *host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        fired_inside.set(host_inside.fire_timers());
    }));
    with_probe(|probe| {
        probe.out(|context| Gesture::set_once(context, GAIN, 1.0));
        assert_eq!(probe.ticks.get(), 0);
    });
    assert_eq!(fired.get(), 1, "the host did not run its loop");
    host.fire_timers();
    assert_eq!(with_probe(|probe| probe.ticks.get()), 1);
    host.assert_no_failures();
}

#[cfg(unix)]
#[test]
fn a_wake_from_another_thread_calls_the_editor_back_on_the_main_thread() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_loop_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    // The socket the wake listens on is registered with the loop.
    let watched = host.watched();
    assert_eq!(watched.len(), 1);
    let fd = watched[0];
    take_log();

    let waker = with_probe(|probe| probe.waker()).expect("the host has a loop");
    let worker = waker.clone();
    std::thread::spawn(move || {
        for _ in 0..3 {
            assert!(worker.wake());
        }
    })
    .join()
    .unwrap();
    // The host's loop finds the socket readable and calls back, once for
    // all three wakes.
    assert_eq!(host.fire_fd(fd), 1);
    assert_eq!(take_log(), [EditorCall::OnMainThread]);

    // A call back that comes while the editor is calling out is put off,
    // and asked for again when the call returns.
    let host_inside = Rc::clone(&host);
    *host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        host_inside.fire_fd(fd);
    }));
    with_probe(|probe| {
        assert!(probe.waker().unwrap().wake());
        probe.out(|context| Gesture::set_once(context, GAIN, 1.0));
    });
    assert_eq!(take_log(), []);
    assert_eq!(host.fire_fd(fd), 1);
    assert_eq!(take_log(), [EditorCall::OnMainThread]);

    // Once the editor is closed, waking does nothing and the socket is
    // unregistered.
    assert_eq!(ui.remove(), kResultOk);
    assert!(!waker.wake());
    assert!(host.watched().is_empty());
    host.assert_no_failures();
}

#[cfg(not(unix))]
#[test]
fn without_unix_sockets_there_is_no_wake_even_with_a_run_loop() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let _frame = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        assert!(probe.context.has_host_loop());
        assert!(probe.waker().is_none());
    });
    assert!(session.host.watched().is_empty());
}

// --- Calls that come back ----------------------------------------------------

#[test]
fn a_view_removed_from_inside_an_edit_is_closed_once_the_edit_returns() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();
    let view = ui.view.clone();
    let result = Rc::new(Cell::new(kInvalidArgument));
    let seen = Rc::clone(&result);
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        seen.set(unsafe { view.removed() });
    }));
    // From the editor's own window: nothing of the view is on the stack.
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    assert_eq!(result.get(), kResultOk);
    assert_eq!(take_log(), [], "the editor was dropped under its own feet");
    // The next call from the host finishes the job; the edit is ended.
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(take_log(), [EditorCall::Dropped]);
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
        ]
    );
    assert_eq!(ui.remove(), kResultFalse);

    // From inside one of the view's own calls, the end of that call does.
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();
    let view = ui.view.clone();
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        assert_eq!(unsafe { view.removed() }, kResultOk);
    }));
    assert_eq!(
        ui.key_down(0, manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_::KEY_UP as u32, 0),
        kResultTrue
    );
    let log = take_log();
    assert_eq!(log.last(), Some(&EditorCall::Dropped));
    assert!(!log.contains(&EditorCall::Reentered), "{log:?}");
    assert_eq!(editors_alive(), 0);
}

#[test]
fn a_component_terminated_from_inside_an_edit_cuts_the_editor_off_at_once() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let _frame = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| assert!(probe.every(16)));
    take_log();
    session.host.take_edits();
    let component = session.component.clone();
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        assert_eq!(unsafe { component.terminate() }, kResultOk);
    }));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
        // Cut off: the gesture was ended for it, and nothing more is sent.
        assert!(!probe.context.is_attached());
        probe.move_to(0.75);
        probe.release();
    });
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            // The link cancels the editor's registrations, then ends its
            // gestures, both before the handler is released.
            HostCall::UnregisterTimer(16),
            HostCall::End(cutoff),
        ]
    );
    assert!(session.handler_gone.get());
    assert_eq!(take_log(), []);
    // Dropped at the next call.
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(take_log(), [EditorCall::Dropped]);
    session.host.assert_no_failures();
}

/// A host that releases its last reference to the view from inside a call
/// the editor makes from its own window: the editor is cut off at once and,
/// since it cannot be dropped under its own frame, leaked.
#[test]
#[cfg_attr(miri, ignore)]
fn a_view_released_from_inside_an_edit_cuts_the_editor_off_and_leaks_only_it() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let frame_gone = ui.give_loop_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| assert!(probe.every(16)));
    take_log();
    session.host.take_edits();
    let held = Rc::new(RefCell::new(Some(ui)));
    let inside = Rc::clone(&held);
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        drop(inside.borrow_mut().take());
    }));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
        assert!(!probe.context.is_attached());
    });
    assert!(held.borrow().is_none());
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_edits(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::UnregisterTimer(16),
            HostCall::End(cutoff),
        ]
    );
    assert_eq!(take_log(), [], "the editor was dropped under its own feet");
    assert!(frame_gone.get());
    assert!(session.host.fds.borrow().is_empty());
    session.host.assert_no_failures();
}

// --- The audio thread --------------------------------------------------------

/// `process` makes no heap calls while, on another thread, views are made,
/// opened, used and closed, and edits are made and echoed.
#[test]
fn process_makes_no_heap_calls_while_views_open_and_close() {
    set_up(Setup {
        timer_on_open: true,
        ..Setup::default()
    });
    let session = Session::panel();
    session.activate();
    session.set_processing(true);
    let stop = Arc::new(AtomicBool::new(false));
    let processor = session.processor.clone();
    let worker_stop = Arc::clone(&stop);
    let audio = std::thread::spawn(move || {
        let mut blocks = 0usize;
        let mut heap_calls = 0usize;
        while !worker_stop.load(Ordering::Relaxed) || blocks < 4 {
            let (_, calls) = process_block(&processor, 32, vec![]);
            heap_calls += calls;
            blocks += 1;
        }
        drop(processor);
        (blocks, heap_calls)
    });

    let rounds = if cfg!(miri) { 4 } else { 300 };
    let cutoff = param_id(CUTOFF);
    for round in 0..rounds {
        let ui = session.view();
        let _frame = ui.give_loop_frame(&session.host);
        assert_eq!(ui.attach_hwnd(), kResultOk);
        with_probe(|probe| {
            probe.drag(CUTOFF, 0.25);
            probe.move_to(f64::from(round % 100) / 100.0);
            probe.release();
        });
        session.host.fire_timers();
        unsafe {
            session
                .controller
                .setParamNormalized(cutoff, f64::from(round % 7) / 7.0);
        }
        if round % 2 == 0 {
            assert_eq!(ui.remove(), kResultOk);
        }
        drop(ui);
        session.host.take_calls();
    }
    stop.store(true, Ordering::Relaxed);
    let (blocks, heap_calls) = audio.join().unwrap();
    assert!(blocks >= 4);
    assert_eq!(heap_calls, 0, "process used the heap in {blocks} blocks");
    assert_eq!(editors_alive(), 0);
    session.host.assert_no_failures();
}

// --- Every order -------------------------------------------------------------

/// A small deterministic generator (xorshift64*), so that failures repeat.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// Checks that, for each parameter, the handler saw begins and ends in
/// pairs, values only between them, and nothing left open.
fn assert_well_formed(calls: &[HostCall]) {
    let mut open: Vec<u32> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        match *call {
            HostCall::Begin(id) => {
                assert!(!open.contains(&id), "call {index}: {call:?} twice");
                open.push(id);
            }
            HostCall::Perform(id, value) => {
                assert!(open.contains(&id), "call {index}: {call:?} outside an edit");
                assert!((0.0..=1.0).contains(&value), "call {index}: {call:?}");
            }
            HostCall::End(id) => {
                assert!(open.contains(&id), "call {index}: {call:?} with no begin");
                open.retain(|&other| other != id);
            }
            _ => {}
        }
    }
    assert_eq!(open, [], "edits left open");
}

/// Random host calls on two views, the editors' gestures and timers, frames
/// coming and going, and the component terminated or released, in every
/// order: nothing crashes, edits stay well formed, and once everything is
/// released no editor, registration, frame or handler is left.
#[test]
fn views_survive_every_order_of_host_calls() {
    let seeds = if cfg!(miri) { 2 } else { 60 };
    let steps = if cfg!(miri) { 40 } else { 200 };
    for seed in 0..seeds {
        let mut random = Random(0x9E37_79B9_7F4A_7C15 ^ (seed + 1));
        set_up(Setup {
            timer_on_open: random.below(2) == 0,
            ..Setup::default()
        });
        let session = Session::panel();
        session.activate();
        let host = Rc::clone(&session.host);
        let mut views: Vec<Option<Ui>> = vec![Some(session.view()), Some(session.view())];
        let mut frames: Vec<Rc<Cell<bool>>> = Vec::new();
        let mut terminated = false;
        for _ in 0..steps {
            let which = random.below(2) as usize;
            let action = random.below(16);
            if action == 10 {
                // The host releases the view, attached or not.
                views[which] = None;
                continue;
            }
            let Some(ui) = views[which].as_ref() else {
                if !terminated && random.below(4) == 0 {
                    views[which] = Some(session.view());
                }
                continue;
            };
            let kind = [c"HWND", c"NSView", c"X11EmbedWindowID"][random.below(3) as usize];
            match action {
                0 | 1 => {
                    ui.attach(kind, PARENT + random.below(4) as usize);
                }
                2 => {
                    ui.remove();
                }
                3 => {
                    ui.size();
                }
                4 => {
                    ui.on_size(random.below(1500) as i32, random.below(1000) as i32);
                }
                5 => {
                    ui.check(random.below(2000) as i32, random.below(2000) as i32);
                }
                6 => {
                    ui.scale([0.0, 1.0, 1.5, 2.0, f32::NAN][random.below(5) as usize]);
                }
                7 => {
                    ui.key_down(u16::from(b'a'), 12, 0);
                }
                8 => frames.push(if random.below(2) == 0 {
                    ui.give_loop_frame(&host)
                } else {
                    ui.give_frame(&host)
                }),
                9 => {
                    ui.set_frame(ptr::null_mut());
                }
                11 | 12 => {
                    let param = [CUTOFF, MODE, GAIN][random.below(3) as usize];
                    let value = random.below(5) as f64 / 4.0;
                    let choice = random.below(4);
                    with_any_probe(|probe| match choice {
                        0 => probe.drag(param, value),
                        1 => probe.move_to(value),
                        2 => probe.release(),
                        _ => {
                            probe.request_resize(HostSize::new(400, 300));
                        }
                    });
                }
                13 => {
                    host.fire_timers();
                }
                14 if !terminated && random.below(8) == 0 => {
                    session.terminate();
                    terminated = true;
                }
                _ => {
                    session.set_processing(random.below(2) == 0);
                    let _ = session.process(16, vec![]);
                }
            }
        }
        drop(views);
        let handler_gone = Rc::clone(&session.handler_gone);
        drop(session);
        assert_eq!(editors_alive(), 0, "seed {seed}");
        assert!(handler_gone.get(), "seed {seed}");
        assert!(frames.iter().all(|gone| gone.get()), "seed {seed}");
        assert!(host.timers.borrow().is_empty(), "seed {seed}");
        assert!(host.fds.borrow().is_empty(), "seed {seed}");
        assert_well_formed(&host.take_calls());
        host.assert_no_failures();
        assert!(
            !take_log().contains(&EditorCall::Reentered),
            "seed {seed}: the editor was called while calling out"
        );
        PROBES.with(|probes| probes.borrow_mut().clear());
    }
}
