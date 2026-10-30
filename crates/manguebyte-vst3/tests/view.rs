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
    /// What the editors on this thread were told by `set_visible`, in
    /// order, with `None` for an editor dropped. Kept apart from `LOG`, so
    /// that the other tests' logs need not list the calls.
    static VISIBILITY: RefCell<Vec<Option<bool>>> = const { RefCell::new(Vec::new()) };
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
            // Only a host with a loop has timers to give; the tests that
            // need this one check that the host saw it registered.
            probe.every(16);
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

    fn set_visible(&mut self, visible: bool) {
        VISIBILITY.with(|log| log.borrow_mut().push(Some(visible)));
    }

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
        VISIBILITY.with(|log| log.borrow_mut().push(None));
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

/// How the host answers `resizeView` with the view, the width and the
/// height asked for.
type ResizeReply = Box<dyn FnMut(&Reentry, i32, i32) -> tresult>;

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
            assert_eq!(
                ComRef::from_raw(self.view).unwrap().getSize(&mut rect),
                kResultOk
            );
        }
        (rect.right - rect.left, rect.bottom - rect.top)
    }

    fn removed(&self) -> tresult {
        unsafe { ComRef::from_raw(self.view).unwrap().removed() }
    }
}

/// What the host's objects share: the calls made to them, their replies,
/// and the run loop's registrations.
#[derive(Default)]
struct HostState {
    calls: RefCell<Vec<HostCall>>,
    /// The calls made to component handlers, each with the number of the
    /// handler that received it.
    handled: RefCell<Vec<(usize, HostCall)>>,
    /// Run once inside `performEdit`, if set.
    on_perform: RefCell<Option<Reply>>,
    /// Run once inside `beginEdit`, if set.
    on_begin: RefCell<Option<Reply>>,
    /// Run once inside `endEdit`, if set.
    on_end: RefCell<Option<Reply>>,
    /// Run once inside `restartComponent`, if set.
    on_restart: RefCell<Option<Reply>>,
    /// Run once inside any of the run loop's functions, if set.
    on_loop: RefCell<Option<Reply>>,
    /// Replies that have run, kept until the test lets go of them, so that
    /// one holding the last reference to a view does not release it from
    /// inside the host's function.
    spent: RefCell<Vec<Reply>>,
    /// The frame's answer to `resizeView`: `None` grants the size asked for
    /// with `onSize`, as hosts do.
    on_resize: RefCell<Option<ResizeReply>>,
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

    /// Records a call the handler numbered `handler` received.
    fn record_handled(&self, handler: usize, call: HostCall) {
        self.record(call);
        self.handled.borrow_mut().push((handler, call));
    }

    fn take_calls(&self) -> Vec<HostCall> {
        self.calls.take()
    }

    /// The calls each handler received, by handler number, in order.
    fn take_handled(&self) -> Vec<(usize, HostCall)> {
        self.handled.take()
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

    /// Runs the reply in `slot`, taken out while it runs, once.
    fn run_once(&self, slot: &RefCell<Option<Reply>>, reentry: &Reentry) {
        let reply = slot.borrow_mut().take();
        if let Some(mut reply) = reply {
            reply(reentry);
            self.spent.borrow_mut().push(reply);
        }
    }

    /// Runs the reply in `slot` once, from inside a function that has no
    /// view to offer it.
    fn run_inside(&self, slot: &RefCell<Option<Reply>>) {
        self.run_once(
            slot,
            &Reentry {
                view: ptr::null_mut(),
            },
        );
    }

    /// Lets go of every reply, run or not, and of what they hold.
    fn forget_replies(&self) {
        let pending = (
            self.on_perform.borrow_mut().take(),
            self.on_begin.borrow_mut().take(),
            self.on_end.borrow_mut().take(),
            self.on_restart.borrow_mut().take(),
            self.on_loop.borrow_mut().take(),
            self.on_resize.borrow_mut().take(),
        );
        let spent = self.spent.take();
        drop((pending, spent));
    }

    /// Fires every timer once, as the host's loop does, and returns how
    /// many it fired.
    fn fire_timers(&self) -> usize {
        let timers: Vec<ComPtr<ITimerHandler>> = self
            .timers
            .borrow()
            .iter()
            .map(|(h, _)| h.clone())
            .collect();
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

/// The host's component handler, numbered so that a test can tell which
/// handler received what.
struct Handler {
    host: Rc<HostState>,
    /// Set when the plugin has released its last reference.
    gone: Rc<Cell<bool>>,
    number: usize,
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
        self.host.record_handled(self.number, HostCall::Begin(id));
        self.host.run_inside(&self.host.on_begin);
        kResultOk
    }
    unsafe fn performEdit(&self, id: u32, value: f64) -> tresult {
        self.host
            .record_handled(self.number, HostCall::Perform(id, value));
        self.host.run_inside(&self.host.on_perform);
        kResultOk
    }
    unsafe fn endEdit(&self, id: u32) -> tresult {
        self.host.record_handled(self.number, HostCall::End(id));
        self.host.run_inside(&self.host.on_end);
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        self.host
            .record_handled(self.number, HostCall::Restart(flags));
        self.host.run_inside(&self.host.on_restart);
        kResultOk
    }
}

/// A new handler numbered `number` on `host`, and its drop flag.
fn new_handler(host: &Rc<HostState>, number: usize) -> (ComWrapper<Handler>, Rc<Cell<bool>>) {
    let gone = Rc::new(Cell::new(false));
    let handler = ComWrapper::new(Handler {
        host: Rc::clone(host),
        gone: Rc::clone(&gone),
        number,
    });
    (handler, gone)
}

fn handler_pointer(handler: &ComWrapper<Handler>) -> *mut IComponentHandler {
    handler.as_com_ref::<IComponentHandler>().unwrap().as_ptr()
}

/// The calls the handler numbered `number` received, in order.
fn calls_of(handled: &[(usize, HostCall)], number: usize) -> Vec<HostCall> {
    handled
        .iter()
        .filter(|&&(of, _)| of == number)
        .map(|&(_, call)| call)
        .collect()
}

/// Checks that the handler numbered `handler` saw, for each parameter,
/// begins and ends in pairs, values only between them and in range, and
/// nothing left open.
fn assert_well_formed(handler: usize, calls: &[HostCall]) {
    let mut open: Vec<u32> = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        match *call {
            HostCall::Begin(id) => {
                assert!(
                    !open.contains(&id),
                    "handler {handler}, call {index}: {call:?} twice: {calls:?}"
                );
                open.push(id);
            }
            HostCall::Perform(id, value) => {
                assert!(
                    open.contains(&id),
                    "handler {handler}, call {index}: {call:?} outside an edit: {calls:?}"
                );
                assert!(
                    (0.0..=1.0).contains(&value),
                    "handler {handler}, call {index}: {call:?}"
                );
            }
            HostCall::End(id) => {
                assert!(
                    open.contains(&id),
                    "handler {handler}, call {index}: {call:?} with no begin: {calls:?}"
                );
                open.retain(|&other| other != id);
            }
            _ => {}
        }
    }
    assert_eq!(
        open,
        [],
        "handler {handler} was left with edits open: {calls:?}"
    );
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

/// The run loop's functions run the host's reply, if one is set, after
/// doing their work and before returning, as a host that runs its loop
/// meanwhile would.
impl IRunLoopTrait for LoopFrame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: i32) -> tresult {
        self.host.record(HostCall::RegisterFd(fd));
        if self.host.refuse_registrations.get() {
            return kResultFalse;
        }
        let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
            return kInvalidArgument;
        };
        self.host.fds.borrow_mut().push((handler.to_com_ptr(), fd));
        self.host.run_inside(&self.host.on_loop);
        kResultOk
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        let removed: Vec<(ComPtr<IEventHandler>, i32)> = {
            let mut fds = self.host.fds.borrow_mut();
            let (gone, kept) = fds
                .drain(..)
                .partition(|(known, _)| known.as_ptr() == handler);
            *fds = kept;
            gone
        };
        if removed.is_empty() {
            self.host
                .fail("an unknown event handler was unregistered".into());
        }
        for &(_, fd) in &removed {
            self.host.record(HostCall::UnregisterFd(fd));
        }
        // Released after the table is put back, as a host would.
        drop(removed);
        self.host.run_inside(&self.host.on_loop);
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
        self.host.run_inside(&self.host.on_loop);
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
        self.host.run_inside(&self.host.on_loop);
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

/// One plugin instance, created through its factory, with a host's handler,
/// which is number 1.
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
        let component = create::<P>();
        let (handler, handler_gone) = new_handler(&host, 1);
        unsafe {
            let controller: ComPtr<IEditController> = component.cast().unwrap();
            assert_eq!(
                controller.setComponentHandler(handler_pointer(&handler)),
                kResultOk
            );
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
        // Replies may hold the host's state, a view or the component.
        self.host.forget_replies();
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

/// Whether the host's content scale is taken on this system: everywhere but
/// on macOS, where every parent is an `NSView`, in points.
const SCALES: bool = !cfg!(target_os = "macos");

/// What `setContentScaleFactor` answers for a scale the wrapper would take
/// on a system that takes scales.
const SCALE_TAKEN: tresult = if SCALES { kResultOk } else { kResultFalse };

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
    let view = session
        .create_view(Some(c"editor"))
        .expect("an editor view");
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
    let (second, gone_second) = new_handler(&host, 2);
    let pointer = handler_pointer(&second);
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
        let (handler, gone) = new_handler(&host, 3);
        let pointer = handler_pointer(&handler);
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
    assert_eq!(
        params().cutoff.value(),
        25.0,
        "the editor wrote the parameter"
    );

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

/// A host that goes on calling `process` with blocks of no frames while it
/// is stopped may echo the edit in one of those. The value is stored then,
/// and the plugin hears about it on the first frame of the next block that
/// has audio.
#[test]
fn an_edit_echoed_in_a_block_without_audio_reaches_the_plugin_on_the_next_block() {
    set_up(Setup::default());
    let session = Session::panel();
    session.activate();
    session.set_processing(true);
    assert_eq!(session.process(8, vec![]).0, [25.0; 8]);
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.75);
        probe.release();
    });
    assert_eq!(
        params().cutoff.value(),
        25.0,
        "the editor wrote the parameter"
    );

    let cutoff = param_id(CUTOFF);
    let (output, heap_calls) = session.process(0, vec![(cutoff, vec![(0, 0.75)])]);
    assert!(output.is_empty());
    assert_eq!(heap_calls, 0);
    assert_eq!(params().cutoff.value(), 75.0);
    let (output, heap_calls) = session.process(8, vec![]);
    assert_eq!(output, [75.0; 8], "the plugin was not told on frame 0");
    assert_eq!(heap_calls, 0);
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
    let (handler, gone) = new_handler(&newer, 2);
    let pointer = handler_pointer(&handler);
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
    assert_eq!(
        newer.take_edits(),
        [HostCall::Begin(gain), HostCall::End(gain)]
    );
    assert!(gone.get());
    with_probe(|probe| {
        probe.move_to(0.25);
        probe.release();
    });
    assert_eq!(params().gain.value(), 0.25, "the value was lost");
    assert_eq!(session.shown(GAIN), 0.25);
}

/// A host replaces its handler (1 by 2) while an edit is open and, from
/// inside the `endEdit` the old handler is sent for it, replaces it again
/// (by 3). Each handler sees well-formed edits: handler 2, replaced before
/// the edit was begun on it, is sent nothing at all.
#[test]
fn a_handler_replaced_from_inside_the_old_handlers_end_edit_sees_well_formed_edits() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| probe.drag(CUTOFF, 0.25));

    let host = &session.host;
    let (second, second_gone) = new_handler(host, 2);
    let (third, _) = new_handler(host, 3);
    let controller = session.controller.clone();
    *host.on_end.borrow_mut() = Some(Box::new(move |_| unsafe {
        controller.setComponentHandler(handler_pointer(&third));
    }));
    unsafe {
        assert_eq!(
            session
                .controller
                .setComponentHandler(handler_pointer(&second)),
            kResultOk
        );
    }
    drop(second);
    assert!(session.handler_gone.get(), "handler 1 was kept");
    assert!(second_gone.get(), "handler 2 was kept");
    with_probe(|probe| {
        probe.move_to(0.5);
        probe.release();
    });

    let cutoff = param_id(CUTOFF);
    let handled = host.take_handled();
    assert_eq!(
        calls_of(&handled, 1),
        [HostCall::Begin(cutoff), HostCall::End(cutoff)]
    );
    assert_eq!(calls_of(&handled, 2), []);
    assert_eq!(
        calls_of(&handled, 3),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
        ]
    );
    host.assert_no_failures();
}

/// A host replaces its handler while an edit is open, and terminates the
/// component from inside the `endEdit` the old handler is sent for it. The
/// editor is closed and the new handler released by `terminate`, and once
/// it has returned no handler is called again: the new handler, on which
/// the edit had not been begun yet, is sent nothing.
#[test]
fn no_handler_is_called_after_terminate_even_from_inside_a_handler_swap() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| probe.drag(CUTOFF, 0.25));
    take_log();

    let host = Rc::clone(&session.host);
    let (second, second_gone) = new_handler(&host, 2);
    let component = session.component.clone();
    // How many handler calls there had been when `terminate` returned,
    // and what it answered.
    let after_terminate = Rc::new(Cell::new((usize::MAX, kInvalidArgument)));
    let (marks, noted) = (Rc::clone(&host), Rc::clone(&after_terminate));
    *host.on_end.borrow_mut() = Some(Box::new(move |_| {
        let result = unsafe { component.terminate() };
        noted.set((marks.handled.borrow().len(), result));
    }));
    unsafe {
        assert_eq!(
            session
                .controller
                .setComponentHandler(handler_pointer(&second)),
            kResultOk
        );
    }
    drop(second);
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert!(session.handler_gone.get() && second_gone.get());

    let cutoff = param_id(CUTOFF);
    let handled = host.take_handled();
    assert_eq!(
        handled,
        [(1, HostCall::Begin(cutoff)), (1, HostCall::End(cutoff))]
    );
    assert_eq!(after_terminate.get(), (handled.len(), kResultOk));
    host.assert_no_failures();
}

/// A host replaces its handler while two edits are open and, from inside
/// the `endEdit` the old handler is sent for the first, the editor lets go
/// of the second, as from its own window. That end goes to the old handler,
/// where the edit was begun, and only the edit still open is begun on the
/// new one.
#[test]
fn an_edit_ended_while_its_handler_is_replaced_is_ended_where_it_began() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.drag(GAIN, 0.5);
    });

    let host = &session.host;
    let (second, _) = new_handler(host, 2);
    *host.on_end.borrow_mut() = Some(Box::new(|_| with_probe(Probe::release)));
    unsafe {
        assert_eq!(
            session
                .controller
                .setComponentHandler(handler_pointer(&second)),
            kResultOk
        );
    }
    drop(second);
    with_probe(Probe::release);

    let (cutoff, gain) = (param_id(CUTOFF), param_id(GAIN));
    let handled = host.take_handled();
    assert_eq!(
        calls_of(&handled, 1),
        [
            HostCall::Begin(cutoff),
            HostCall::Begin(gain),
            HostCall::End(cutoff),
            HostCall::End(gain),
        ]
    );
    assert_eq!(
        calls_of(&handled, 2),
        [HostCall::Begin(cutoff), HostCall::End(cutoff)]
    );
    host.assert_no_failures();
}

/// A host that replaces its handler from inside `beginEdit` has the edit
/// ended on the old handler, which had been told it began, and begun on
/// the new one, which gets the values that follow.
#[test]
fn a_handler_replaced_from_inside_begin_edit_has_the_edit_moved_to_the_new_one() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);

    let host = &session.host;
    let (second, _) = new_handler(host, 2);
    let controller = session.controller.clone();
    *host.on_begin.borrow_mut() = Some(Box::new(move |_| unsafe {
        controller.setComponentHandler(handler_pointer(&second));
    }));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
        probe.release();
    });

    let cutoff = param_id(CUTOFF);
    assert_eq!(
        host.take_handled(),
        [
            (1, HostCall::Begin(cutoff)),
            (1, HostCall::End(cutoff)),
            (2, HostCall::Begin(cutoff)),
            (2, HostCall::Perform(cutoff, 0.5)),
            (2, HostCall::End(cutoff)),
        ]
    );
    assert!(session.handler_gone.get());
    host.assert_no_failures();
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

/// The title, short title and units the controller gives for `param`.
fn titles(session: &Session, param: ParamId) -> (String, String, String) {
    let wanted = param_id(param);
    let count = unsafe { session.controller.getParameterCount() };
    for index in 0..count {
        let mut info: vst::ParameterInfo = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { session.controller.getParameterInfo(index, &mut info) },
            kResultOk
        );
        if info.id == wanted {
            let text = |field: &[u16]| {
                let end = field.iter().position(|&c| c == 0).unwrap_or(field.len());
                String::from_utf16_lossy(&field[..end])
            };
            return (text(&info.title), text(&info.shortTitle), text(&info.units));
        }
    }
    panic!("no parameter {param:?}");
}

/// The parameters renamed on another thread, all at once, as a plugin
/// that loads a whole new instrument does.
fn load_instrument(prefix: &'static str) {
    let params = params();
    std::thread::spawn(move || {
        for param in [&params.cutoff as &dyn Param, &params.mode, &params.gain] {
            let name = format!("{prefix} {}", param.name());
            param.info().set_display_name(&name);
            param.info().set_short_name(&name[..4]);
        }
        params.gain.info().set_display_unit("dB");
    })
    .join()
    .unwrap();
}

/// Three parameters renamed, and their unit changed, are one
/// `restartComponent(kParamTitlesChanged)` at the next call the host makes
/// into the controller that may carry it, and `getParameterInfo` gives the
/// new titles. Calls with nothing renamed in between ask for nothing.
#[test]
fn a_batch_of_renames_is_one_title_restart_and_the_info_gives_the_new_titles() {
    let session = Session::panel();
    session.host.take_edits();
    assert_eq!(
        titles(&session, GAIN),
        ("Gain".into(), "Gain".into(), String::new())
    );

    load_instrument("Swell");
    session.activate();
    let titles_changed = vst::RestartFlags_::kParamTitlesChanged as i32;
    assert_eq!(
        session.host.take_edits(),
        [HostCall::Restart(titles_changed)]
    );
    assert_eq!(
        titles(&session, GAIN),
        ("Swell Gain".into(), "Swel".into(), "dB".into())
    );
    assert_eq!(titles(&session, CUTOFF).0, "Swell Cutoff");
    unsafe {
        assert_eq!(
            session.controller.setComponentState(ptr::null_mut()),
            kResultOk
        );
        assert_eq!(session.component.setActive(0), kResultOk);
    }
    assert_eq!(session.host.take_edits(), []);

    // A state load that comes with a new instrument.
    load_instrument("Choir");
    unsafe {
        assert_eq!(
            session.controller.setComponentState(ptr::null_mut()),
            kResultOk
        );
    }
    assert_eq!(
        session.host.take_edits(),
        [HostCall::Restart(titles_changed)]
    );
    assert_eq!(titles(&session, MODE).0, "Choir Mode");
    // The id is the same, so automation and state still find the
    // parameter.
    assert_eq!(params().mode.param_id(), MODE);
    session.host.assert_no_failures();
}

#[test]
fn values_changed_after_renames_has_the_host_read_the_titles_in_the_same_restart() {
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.attach_hwnd(), kResultOk);
    session.host.take_edits();
    load_instrument("Great");
    params().gain.set(0.25);
    with_probe(|probe| probe.out(EditorContext::values_changed));
    assert_eq!(
        session.host.take_edits(),
        [HostCall::Restart(
            (vst::RestartFlags_::kParamValuesChanged | vst::RestartFlags_::kParamTitlesChanged)
                as i32
        )]
    );
    assert_eq!(session.shown(GAIN), 0.25);
    assert_eq!(titles(&session, GAIN).0, "Great Gain");
    // Values alone, later, are values alone.
    with_probe(|probe| probe.out(EditorContext::values_changed));
    assert_eq!(
        session.host.take_edits(),
        [HostCall::Restart(
            vst::RestartFlags_::kParamValuesChanged as i32
        )]
    );
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

/// VST 3 has no call that shows a view, so the wrapper shows the editor
/// once `attached` has succeeded, and hides it at the start of `removed`,
/// before it is dropped.
#[test]
fn an_attached_editor_is_shown_and_hidden_before_it_is_dropped() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    VISIBILITY.with(|log| log.borrow_mut().clear());
    let visibility = || VISIBILITY.with(|log| log.borrow().clone());
    for _ in 0..2 {
        assert_eq!(ui.attach_hwnd(), kResultOk);
        assert_eq!(visibility().last(), Some(&Some(true)));
        // Shown once, and hidden only when removed.
        assert_eq!(ui.attach_hwnd(), kResultFalse);
        assert_eq!(visibility().last(), Some(&Some(true)));
        assert_eq!(editors_alive(), 1);
        assert_eq!(ui.remove(), kResultOk);
        // Hidden, then dropped.
        assert_eq!(visibility()[visibility().len() - 2..], [Some(false), None]);
        assert_eq!(editors_alive(), 0);
    }
    assert_eq!(
        visibility(),
        [Some(true), Some(false), None, Some(true), Some(false), None]
    );
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(win32(), None),
            EditorCall::Dropped,
            EditorCall::Open(win32(), None),
            EditorCall::Dropped,
        ]
    );

    // An editor that fails to open is never shown.
    set_up(Setup {
        fail_open: true,
        ..Setup::default()
    });
    let session = Session::panel();
    let ui = session.view();
    VISIBILITY.with(|log| log.borrow_mut().clear());
    assert_eq!(ui.attach_hwnd(), kResultFalse);
    assert_eq!(visibility(), []);
    take_log();
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
    assert_eq!(ui.scale(2.0), SCALE_TAKEN);
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
    assert_eq!(
        take_log(),
        [EditorCall::Open(win32(), None), EditorCall::Dropped]
    );
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
    assert_eq!(
        take_log(),
        [EditorCall::Open(win32(), None), EditorCall::Dropped]
    );
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
    assert!(
        session.host.fds.borrow().is_empty(),
        "the wake was left behind"
    );
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
    assert_eq!(
        take_log(),
        [EditorCall::Open(win32(), None), EditorCall::Dropped]
    );
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
        assert_eq!(
            ui.view.checkSizeConstraint(ptr::null_mut()),
            kInvalidArgument
        );
    }
    assert_eq!(ui.on_size(-5, 10), kInvalidArgument);
    let mut inside_out = ViewRect {
        left: 100,
        top: 0,
        right: 0,
        bottom: 100,
    };
    unsafe {
        assert_eq!(
            ui.view.checkSizeConstraint(&mut inside_out),
            kInvalidArgument
        )
    };
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
    assert_eq!(
        (rect.right - rect.left, rect.bottom - rect.top),
        (1000, 563)
    );
    assert_eq!(ui.attach_hwnd(), kResultOk);
    let (_, rect) = ui.check(10, 10);
    assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (160, 90));
}

/// The size `getSize` reports before the view is attached is the size the
/// editor opens at, whatever content scale the host set first, so that the
/// host's window and the editor agree. On macOS the parent is an `NSView`,
/// in points, and the scale is refused; elsewhere it is taken.
#[test]
fn the_size_reported_before_attaching_is_the_size_the_editor_opens_at() {
    set_up(Setup::default());
    let (kind, parent) = if SCALES {
        (c"HWND", win32())
    } else {
        (c"NSView", ParentWindow::cocoa(PARENT).unwrap())
    };
    for factor in [1.0f32, 1.25, 2.0, 3.0] {
        let session = Session::panel();
        let ui = session.view();
        assert_eq!(ui.scale(factor), SCALE_TAKEN, "at {factor}");
        let before = ui.size();
        assert_eq!(ui.attach(kind, PARENT), kResultOk);
        let scale = SCALES.then_some(f64::from(factor));
        assert_eq!(take_log(), [EditorCall::Open(parent, scale)]);
        assert_eq!(ui.size(), before, "at {factor}");
        assert_eq!(
            before,
            HostSize::new(300, 200).scale_by(scale.unwrap_or(1.0)),
            "at {factor}"
        );
        drop((ui, session));
        take_log();
    }
}

/// On macOS every parent is an `NSView`, whose sizes are points that the
/// system scales: the host's content scale is refused before the view is
/// attached and while it is, and no size is ever scaled.
#[cfg(target_os = "macos")]
#[test]
fn on_macos_the_hosts_content_scale_is_refused() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    assert_eq!(ui.scale(2.0), kResultFalse);
    assert_eq!(ui.size(), HostSize::new(300, 200));
    let (_, rect) = ui.check(1, 1);
    assert_eq!((rect.right - rect.left, rect.bottom - rect.top), (150, 100));
    for kind in [c"NSView", c"HWND"] {
        assert_eq!(ui.attach(kind, 0x7000), kResultOk);
        assert_eq!(ui.scale(2.0), kResultFalse);
        assert_eq!(ui.size(), HostSize::new(300, 200));
        assert_eq!(ui.remove(), kResultOk);
    }
    assert_eq!(
        take_log(),
        [
            EditorCall::Open(ParentWindow::cocoa(0x7000).unwrap(), None),
            EditorCall::Size,
            EditorCall::Dropped,
            EditorCall::Open(ParentWindow::win32(0x7000).unwrap(), None),
            EditorCall::Size,
            EditorCall::Dropped,
        ]
    );
}

/// The host's content scale reaches the editor, on the systems that take
/// one: all but macOS (see above).
#[cfg(not(target_os = "macos"))]
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
    assert_eq!(
        with_probe(|probe| probe.request_resize(HostSize::new(400, 300))),
        None
    );

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

/// A size and a scale the host sets while the editor is calling out cannot
/// reach the editor then. The host is answered from the spec's rules, and
/// the editor gets both, the scale first, as soon as nothing is in progress.
#[test]
fn a_size_and_a_scale_set_while_the_editor_calls_out_reach_it_afterwards() {
    use manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_ as codes;

    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let _frame = ui.give_frame(&session.host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();

    // What the host's calls from inside `performEdit` answered, with the
    // width and height they gave back, checked once the call is over:
    // nothing may panic inside the host's functions.
    let answers: Rc<RefCell<Vec<(tresult, i32, i32)>>> = Rc::default();

    // The host sets two sizes, the first too small, and a scale, sends a
    // key, and asks about sizes.
    let view = ui.view.clone();
    let seen = Rc::clone(&answers);
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        let ui = Ui { view: view.clone() };
        let mut seen = seen.borrow_mut();
        seen.push((ui.on_size(100, 50), 0, 0));
        seen.push((ui.on_size(500, 400), 0, 0));
        seen.push((ui.scale(2.0), 0, 0));
        seen.push((ui.key_down(0, codes::KEY_UP as u32, 0), 0, 0));
        let (result, rect) = ui.check(5_000, 50);
        seen.push((result, rect.right - rect.left, rect.bottom - rect.top));
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        let result = unsafe { ui.view.getSize(&mut rect) };
        seen.push((result, rect.right, rect.bottom));
    }));
    with_probe(|probe| {
        probe.drag(CUTOFF, 0.25);
        probe.move_to(0.5);
    });
    assert_eq!(
        answers.take(),
        [
            (kResultFalse, 0, 0),
            (kResultOk, 0, 0),
            (SCALE_TAKEN, 0, 0),
            (kResultFalse, 0, 0),
            // At the scale the editor has until it is told.
            (kResultTrue, 1200, 100),
            (kResultOk, 500, 400),
        ]
    );
    assert_eq!(take_log(), [], "the editor was called while calling out");

    // The next call from the host gives them to the editor.
    assert_eq!(ui.size(), HostSize::new(500, 400));
    let given = [
        EditorCall::Size,
        EditorCall::SetSize(HostSize::new(500, 400), true),
        EditorCall::Size,
    ];
    if SCALES {
        let mut expected = vec![EditorCall::SetScale(2.0)];
        expected.extend(given);
        assert_eq!(take_log(), expected);
    } else {
        assert_eq!(take_log(), given);
    }
    with_probe(Probe::release);
    let cutoff = param_id(CUTOFF);
    assert_eq!(
        session.host.take_calls(),
        [
            HostCall::Begin(cutoff),
            HostCall::Perform(cutoff, 0.5),
            HostCall::End(cutoff),
        ]
    );
    if !SCALES {
        // What follows is about scales, which macOS refuses.
        session.host.assert_no_failures();
        return;
    }

    // A size held at one scale that the editor refuses at the next: the
    // host is asked to make its window the editor's size.
    let view = ui.view.clone();
    let seen = Rc::clone(&answers);
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        let ui = Ui { view: view.clone() };
        let mut seen = seen.borrow_mut();
        seen.push((ui.on_size(400, 250), 0, 0));
        seen.push((ui.scale(4.0), 0, 0));
    }));
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 1.0)));
    assert_eq!(answers.take(), [(kResultOk, 0, 0), (kResultOk, 0, 0)]);
    assert_eq!(take_log(), []);
    assert_eq!(ui.size(), HostSize::new(1200, 800));
    assert_eq!(
        take_log(),
        [
            EditorCall::SetScale(4.0),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(400, 250), false),
            EditorCall::Size,
        ]
    );
    let gain = param_id(GAIN);
    assert_eq!(
        session.host.take_calls(),
        [
            HostCall::Begin(gain),
            HostCall::Perform(gain, 1.0),
            HostCall::End(gain),
            HostCall::ResizeView(0, 0, 1200, 800),
        ]
    );

    // A scale held when the view is removed meanwhile: the editor is
    // dropped, and the next one opens with the scale the host set.
    let view = ui.view.clone();
    let seen = Rc::clone(&answers);
    *session.host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        let ui = Ui { view: view.clone() };
        let mut seen = seen.borrow_mut();
        seen.push((ui.scale(1.5), 0, 0));
        seen.push((ui.remove(), 0, 0));
    }));
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 0.0)));
    assert_eq!(answers.take(), [(kResultOk, 0, 0), (kResultOk, 0, 0)]);
    assert_eq!(take_log(), []);
    assert_eq!(ui.size(), HostSize::new(450, 300));
    assert_eq!(take_log(), [EditorCall::Dropped]);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    assert_eq!(take_log(), [EditorCall::Open(win32(), Some(1.5))]);
    session.host.assert_no_failures();
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

#[cfg(all(unix, not(miri)))]
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

/// Without Unix sockets, and under Miri, which cannot make a socket
/// non-blocking, there is no wake.
#[cfg(any(not(unix), miri))]
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
        ui.key_down(
            0,
            manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_::KEY_UP as u32,
            0
        ),
        kResultTrue
    );
    let log = take_log();
    assert_eq!(log.last(), Some(&EditorCall::Dropped));
    assert!(!log.contains(&EditorCall::Reentered), "{log:?}");
    assert_eq!(editors_alive(), 0);
}

/// A host removes the view from inside an edit the editor made from its own
/// window, so the editor cannot be dropped then, and its loop calls one of
/// the editor's timers, or descriptors, before the host calls the view
/// again. The removal is carried out first: the editor is not run for a
/// view the host has removed, and is dropped.
#[test]
fn a_view_removed_from_inside_an_edit_is_closed_when_the_hosts_loop_calls_back() {
    set_up(Setup::default());
    for descriptor in [false, true] {
        let session = Session::panel();
        let ui = session.view();
        let host = Rc::clone(&session.host);
        let _frame = ui.give_loop_frame(&host);
        assert_eq!(ui.attach_hwnd(), kResultOk);
        let ticks = with_probe(|probe| {
            assert!(if descriptor {
                probe.watch(1005)
            } else {
                probe.every(16)
            });
            Rc::clone(&probe.ticks)
        });
        take_log();
        let view = ui.view.clone();
        let answer = Rc::new(Cell::new(kInvalidArgument));
        let seen = Rc::clone(&answer);
        *host.on_perform.borrow_mut() = Some(Box::new(move |_| {
            seen.set(unsafe { view.removed() });
        }));
        with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 1.0)));
        assert_eq!(answer.get(), kResultOk, "the host removed the view");
        assert_eq!(take_log(), [], "the editor was dropped under its own feet");

        let fired = if descriptor {
            host.fire_fd(1005)
        } else {
            host.fire_timers()
        };
        assert_eq!(fired, 1);
        assert_eq!(
            ticks.get(),
            0,
            "the editor ran for a view the host had removed"
        );
        assert_eq!(take_log(), [EditorCall::Dropped]);
        assert!(host.timers.borrow().is_empty() && host.fds.borrow().is_empty());
        assert_eq!(ui.remove(), kResultFalse);
        host.assert_no_failures();
    }
}

/// One of the editor's timers makes an edit, and the host removes the view
/// from inside it. The removal is carried out as soon as the timer's
/// callback returns, not at some later call that may never come.
#[test]
fn a_view_removed_from_inside_a_timer_callback_is_closed_when_it_returns() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_loop_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    with_probe(|probe| {
        let context = probe.context.clone();
        let handle = probe
            .context
            .every(16, move || Gesture::set_once(&context, GAIN, 1.0))
            .expect("the host has a loop");
        probe.handles.borrow_mut().push(handle);
    });
    take_log();
    host.take_edits();
    let view = ui.view.clone();
    *host.on_perform.borrow_mut() = Some(Box::new(move |_| unsafe {
        view.removed();
    }));

    assert_eq!(host.fire_timers(), 1);
    assert_eq!(take_log(), [EditorCall::Dropped]);
    let gain = param_id(GAIN);
    assert_eq!(
        host.take_edits(),
        [
            HostCall::Begin(gain),
            HostCall::Perform(gain, 1.0),
            HostCall::End(gain),
            HostCall::UnregisterTimer(16),
        ]
    );
    assert!(host.timers.borrow().is_empty() && host.fds.borrow().is_empty());
    host.assert_no_failures();
}

/// A host removes the view from inside an edit; the editor, still in the
/// code that made the edit, asks for a new size and for a timer. The host
/// is asked for neither, since it has removed the view, and the editor is
/// told no.
#[test]
fn an_editor_whose_view_was_removed_cannot_resize_it_or_use_the_hosts_loop() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_loop_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();
    host.take_edits();
    let view = ui.view.clone();
    *host.on_perform.borrow_mut() = Some(Box::new(move |_| unsafe {
        view.removed();
    }));
    with_probe(|probe| {
        probe.out(|context| Gesture::set_once(context, GAIN, 1.0));
        assert_eq!(probe.request_resize(HostSize::new(400, 300)), None);
        assert!(!probe.every(16));
        assert!(!probe.context.has_host_loop());
    });
    let gain = param_id(GAIN);
    assert_eq!(
        host.take_edits(),
        [
            HostCall::Begin(gain),
            HostCall::Perform(gain, 1.0),
            HostCall::End(gain),
        ]
    );
    assert_eq!(take_log(), []);
    // Dropped at the host's next call into the view.
    assert_eq!(ui.size(), HostSize::new(300, 200));
    assert_eq!(take_log(), [EditorCall::Dropped]);
    host.assert_no_failures();
}

/// While the editor calls out, the host sets a size and a scale at which
/// the editor refuses that size. Once nothing is in progress the editor is
/// given both, refuses the size, and the host is asked to make its window
/// the editor's size. A host that, from inside that `resizeView`, gives yet
/// another size and removes the view: the editor is not offered the other
/// size, and is dropped when the call is over.
#[cfg(not(target_os = "macos"))]
#[test]
fn a_view_removed_from_inside_the_resize_the_wrapper_asked_for_gets_no_more_sizes() {
    set_up(Setup::default());
    let session = Session::panel();
    let ui = session.view();
    let host = Rc::clone(&session.host);
    let _frame = ui.give_frame(&host);
    assert_eq!(ui.attach_hwnd(), kResultOk);
    take_log();
    let view = ui.view.clone();
    *host.on_perform.borrow_mut() = Some(Box::new(move |_| {
        let ui = Ui { view: view.clone() };
        ui.on_size(400, 250);
        ui.scale(4.0);
    }));
    with_probe(|probe| probe.out(|context| Gesture::set_once(context, GAIN, 1.0)));
    assert_eq!(take_log(), []);

    let removed = Rc::new(Cell::new(kInvalidArgument));
    let seen = Rc::clone(&removed);
    *host.on_resize.borrow_mut() = Some(Box::new(move |reentry, _, _| {
        reentry.on_size(700, 500);
        seen.set(reentry.removed());
        kResultTrue
    }));
    // The window's size as the host last set it.
    assert_eq!(ui.size(), HostSize::new(700, 500));
    assert_eq!(removed.get(), kResultOk);
    assert_eq!(
        take_log(),
        [
            EditorCall::SetScale(4.0),
            EditorCall::Size,
            EditorCall::SetSize(HostSize::new(400, 250), false),
            EditorCall::Dropped,
        ]
    );
    let gain = param_id(GAIN);
    assert_eq!(
        host.take_calls(),
        [
            HostCall::Begin(gain),
            HostCall::Perform(gain, 1.0),
            HostCall::End(gain),
            HostCall::ResizeView(0, 0, 1200, 800),
        ]
    );
    assert_eq!(ui.remove(), kResultFalse);
    host.assert_no_failures();
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

/// Where the host runs a reply from: inside `performEdit`, `beginEdit`,
/// `endEdit`, `restartComponent`, or one of the run loop's functions.
const INSIDE: [&str; 5] = ["performEdit", "beginEdit", "endEdit", "restart", "run loop"];

/// What a reply does.
const REPLIES: [&str; 10] = [
    "remove the view",
    "set a size",
    "set a scale",
    "ask about sizes",
    "send a key",
    "run the loop's timers",
    "replace the handler",
    "terminate the component",
    "let go of a control",
    "take the handler away",
];

/// What the replies of one run share: the session, the view the reply is
/// for, the drop flags of every handler given, the number the next handler
/// gets, and whether the component has been terminated.
struct Stage<'a> {
    session: &'a Session,
    view: &'a ComPtr<IPlugView>,
    handlers: &'a Rc<RefCell<Vec<Rc<Cell<bool>>>>>,
    next_number: &'a Rc<Cell<usize>>,
    terminated: &'a Rc<Cell<bool>>,
}

/// A reply that calls the plugin back from inside one of the host's
/// functions, picked at random from [`REPLIES`], with its index there.
/// Nothing in it asserts, since a panic may not cross the interface.
fn random_reply(random: &mut Random, stage: &Stage<'_>) -> (usize, Reply) {
    use manguebyte_vst3::vst3::Steinberg::VirtualKeyCodes_ as codes;

    let ui = Ui {
        view: stage.view.clone(),
    };
    let (width, height) = (random.below(1500) as i32, random.below(1000) as i32);
    let factor = [1.0, 1.5, 2.0, f32::NAN][random.below(4) as usize];
    let kind = random.below(REPLIES.len() as u64) as usize;
    let reply: Reply = match kind {
        0 => Box::new(move |_: &Reentry| {
            ui.remove();
        }),
        1 => Box::new(move |_: &Reentry| {
            ui.on_size(width, height);
        }),
        2 => Box::new(move |_: &Reentry| {
            ui.scale(factor);
        }),
        3 => Box::new(move |_: &Reentry| unsafe {
            let mut rect = ViewRect {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            ui.view.getSize(&mut rect);
            ui.view.checkSizeConstraint(&mut rect);
            ui.view.canResize();
        }),
        4 => Box::new(move |_: &Reentry| {
            ui.key_down(0, codes::KEY_UP as u32, 0);
        }),
        5 => {
            let host = Rc::clone(&stage.session.host);
            Box::new(move |_: &Reentry| {
                host.fire_timers();
            })
        }
        6 => {
            let host = Rc::clone(&stage.session.host);
            let controller = stage.session.controller.clone();
            let handlers = Rc::clone(stage.handlers);
            let next_number = Rc::clone(stage.next_number);
            Box::new(move |_: &Reentry| {
                let number = next_number.get();
                next_number.set(number + 1);
                let (handler, gone) = new_handler(&host, number);
                unsafe { controller.setComponentHandler(handler_pointer(&handler)) };
                handlers.borrow_mut().push(gone);
            })
        }
        7 => {
            let component = stage.session.component.clone();
            let terminated = Rc::clone(stage.terminated);
            Box::new(move |_: &Reentry| {
                if !terminated.replace(true) {
                    unsafe { component.terminate() };
                }
            })
        }
        // The editor, from its own window, as a nested loop of the host's
        // would let it.
        8 => Box::new(|_: &Reentry| {
            with_any_probe(Probe::release);
        }),
        _ => {
            let controller = stage.session.controller.clone();
            Box::new(move |_: &Reentry| unsafe {
                controller.setComponentHandler(ptr::null_mut());
            })
        }
    };
    (kind, reply)
}

/// How the host answers `resizeView`, picked at random: with the size asked
/// for, as hosts do; with another size; with a refusal; or by removing the
/// view first.
fn random_resize_reply(random: &mut Random) -> Option<ResizeReply> {
    let (width, height) = (random.below(1500) as i32, random.below(1000) as i32);
    match random.below(4) {
        0 => None,
        1 => Some(Box::new(move |reentry: &Reentry, _: i32, _: i32| {
            reentry.on_size(width, height);
            kResultTrue
        })),
        2 => Some(Box::new(|_: &Reentry, _: i32, _: i32| kResultFalse)),
        _ => Some(Box::new(|reentry: &Reentry, _: i32, _: i32| {
            reentry.removed();
            kResultFalse
        })),
    }
}

/// Random host calls on two views, the editors' gestures, timers and
/// requests, frames and handlers coming and going, a host that calls back
/// from inside any of its handler's and its run loop's functions, and the
/// component terminated or released before or after its views, in every
/// order: nothing crashes, `process` never uses the heap, every handler sees
/// well-formed edits, the editor is never called while it calls out, and
/// once everything is released no editor, registration, frame or handler is
/// left.
#[test]
fn views_survive_every_order_of_host_calls() {
    let seeds = if cfg!(miri) { 8 } else { 1000 };
    let steps = if cfg!(miri) { 125 } else { 250 };
    // How much of the interesting ground the runs covered, all seeds
    // together: the editors opened, the handlers that were called, and
    // every reply that ran, with where it ran from.
    let (mut opened, mut handlers_called) = (0, 0);
    let ran: Rc<RefCell<Vec<(usize, usize)>>> = Rc::default();
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
        // The drop flags of every frame and handler the host gave.
        let mut frames: Vec<Rc<Cell<bool>>> = Vec::new();
        let handlers = Rc::new(RefCell::new(vec![Rc::clone(&session.handler_gone)]));
        let next_number = Rc::new(Cell::new(2));
        let terminated = Rc::new(Cell::new(false));
        for _ in 0..steps {
            // Replies that have run let go of what they hold here, where
            // nothing of the plugin is on the stack.
            drop(host.spent.take());
            let which = random.below(2) as usize;
            let action = random.below(18);
            if action == 10 {
                // The host releases the view, attached or not.
                views[which] = None;
                continue;
            }
            let Some(ui) = views[which].as_ref() else {
                if !terminated.get() && random.below(4) == 0 {
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
                    let choice = random.below(8);
                    with_any_probe(|probe| match choice {
                        0 => probe.drag(param, value),
                        1 => probe.move_to(value),
                        2 => probe.release(),
                        3 => {
                            probe.request_resize(HostSize::new(400, 300));
                        }
                        4 => probe.out(EditorContext::values_changed),
                        5 => probe.out(EditorContext::closed),
                        6 => {
                            probe.every(5);
                        }
                        _ => {
                            if !probe.handles.borrow().is_empty() {
                                probe.drop_handle(0);
                            }
                        }
                    });
                }
                13 => {
                    host.fire_timers();
                }
                14 if !terminated.get() && random.below(8) == 0 => {
                    session.terminate();
                    terminated.set(true);
                }
                16 => {
                    let inside = random.below(INSIDE.len() as u64) as usize;
                    let stage = Stage {
                        session: &session,
                        view: &ui.view,
                        handlers: &handlers,
                        next_number: &next_number,
                        terminated: &terminated,
                    };
                    let (does, mut reply) = random_reply(&mut random, &stage);
                    let noted = Rc::clone(&ran);
                    let counted: Reply = Box::new(move |reentry: &Reentry| {
                        noted.borrow_mut().push((inside, does));
                        reply(reentry);
                    });
                    let slot = [
                        &host.on_perform,
                        &host.on_begin,
                        &host.on_end,
                        &host.on_restart,
                        &host.on_loop,
                    ][inside];
                    // The reply it replaces is dropped after the slot is
                    // put back: it may hold the last reference to a view,
                    // whose release calls the host's loop.
                    let replaced = slot.replace(Some(counted));
                    drop(replaced);
                }
                17 => {
                    let replaced = host.on_resize.replace(random_resize_reply(&mut random));
                    drop(replaced);
                }
                _ => {
                    session.set_processing(random.below(2) == 0);
                    let (_, heap_calls) = session.process(16, vec![]);
                    assert_eq!(heap_calls, 0, "seed {seed}: process used the heap");
                }
            }
        }
        host.forget_replies();
        if seed % 2 == 0 {
            drop(views);
            drop(session);
        } else {
            // The component goes first, with its views still there.
            drop(session);
            drop(views);
        }
        assert_eq!(editors_alive(), 0, "seed {seed}");
        assert!(
            handlers.borrow().iter().all(|gone| gone.get()),
            "seed {seed}"
        );
        assert!(frames.iter().all(|gone| gone.get()), "seed {seed}");
        assert!(host.timers.borrow().is_empty(), "seed {seed}");
        assert!(host.fds.borrow().is_empty(), "seed {seed}");
        // Each handler on its own: one that is replaced must not leave
        // edits open, or be sent ends for edits begun on another.
        let handled = host.take_handled();
        for number in 1..next_number.get() {
            let calls = calls_of(&handled, number);
            if !calls.is_empty() {
                handlers_called += 1;
            }
            assert_well_formed(number, &calls);
        }
        host.take_calls();
        host.assert_no_failures();
        let log = take_log();
        assert!(
            !log.contains(&EditorCall::Reentered),
            "seed {seed}: the editor was called while calling out"
        );
        // Every editor opened was dropped, none leaked.
        let opens = log
            .iter()
            .filter(|call| matches!(call, EditorCall::Open(..)))
            .count();
        let drops = log
            .iter()
            .filter(|&&call| call == EditorCall::Dropped)
            .count();
        assert_eq!(opens, drops, "seed {seed}");
        opened += opens;
        PROBES.with(|probes| probes.borrow_mut().clear());
    }

    // The runs are deterministic. The floors sit about a fifth under what
    // the runs measured on Windows, natively and at the size Miri runs, so
    // that a change that makes them shallow is noticed. On other systems
    // the host's loop also carries the wake, or the scale is refused, and
    // the numbers differ a little.
    let ran = ran.take();
    let floors = if cfg!(miri) { FLOORS_MIRI } else { FLOORS };
    let count = |wanted: &dyn Fn(usize, usize) -> bool| {
        ran.iter()
            .filter(|&&(inside, does)| wanted(inside, does))
            .count()
    };
    assert!(
        opened >= floors.opened,
        "only {opened} editors opened ({floors:?})"
    );
    assert!(
        handlers_called >= floors.handlers_called,
        "only {handlers_called} handlers were called ({floors:?})"
    );
    assert!(
        ran.len() >= floors.replies_ran,
        "only {} replies ran ({floors:?})",
        ran.len()
    );
    for (at, name) in INSIDE.iter().enumerate() {
        let ran_there = count(&|inside, _| inside == at);
        assert!(
            ran_there >= floors.inside[at],
            "only {ran_there} replies ran inside {name} ({floors:?})"
        );
        for (kind, reply) in REPLIES.iter().enumerate() {
            let both = count(&|inside, does| inside == at && does == kind);
            assert!(
                both >= floors.each_pair,
                "only {both} replies did {reply:?} inside {name} ({floors:?})"
            );
        }
    }
    for (kind, name) in REPLIES.iter().enumerate() {
        let did = count(&|_, does| does == kind);
        assert!(
            did >= floors.replies[kind],
            "only {did} replies did {name:?} ({floors:?})"
        );
    }
}

/// The least coverage [`views_survive_every_order_of_host_calls`] accepts.
#[derive(Debug)]
struct Floors {
    opened: usize,
    handlers_called: usize,
    replies_ran: usize,
    /// By [`INSIDE`].
    inside: [usize; 5],
    /// By [`REPLIES`].
    replies: [usize; 10],
    /// For every reply from inside every function.
    each_pair: usize,
}

/// Measured: 5621 editors, 931 handlers, 1838 replies; by where they ran
/// 413, 477, 535, 220 and 193; by what they did 183, 205, 178, 174, 186,
/// 184, 161, 179, 181 and 207; at least 15 for every pair.
const FLOORS: Floors = Floors {
    opened: 4500,
    handlers_called: 745,
    replies_ran: 1470,
    inside: [330, 380, 430, 175, 155],
    replies: [145, 165, 140, 140, 150, 145, 130, 145, 145, 165],
    each_pair: 5,
};

/// Measured: 31 editors, 8 handlers, 11 replies; by where they ran 2, 2,
/// 4, 3 and 0.
const FLOORS_MIRI: Floors = Floors {
    opened: 25,
    handlers_called: 6,
    replies_ran: 8,
    inside: [1, 1, 2, 1, 0],
    replies: [0; 10],
    each_pair: 0,
};
