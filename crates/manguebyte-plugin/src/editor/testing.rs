//! Test doubles for the editor contract: a host that records what an editor
//! asks of it, and a loop whose timers and descriptors a test fires by
//! hand.
//!
//! They are public so that wrappers, toolkits and plugin authors can test
//! editors without a host, a window or a screen.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use manguebyte_core::ParamId;
use manguebyte_params::ParamSet;

use super::{EditorHost, HostLoop, HostSize, LoopToken};

/// One call an editor made on a [`RecordingEditorHost`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostCall {
    /// [`EditorHost::begin_gesture`].
    BeginGesture(ParamId),
    /// [`EditorHost::perform`], with the normalized value.
    Perform(ParamId, f64),
    /// [`EditorHost::end_gesture`].
    EndGesture(ParamId),
    /// [`EditorHost::values_changed`].
    ValuesChanged,
    /// [`EditorHost::request_resize`], with the size asked for.
    RequestResize(HostSize),
    /// [`EditorHost::closed`].
    Closed,
    /// A [`MainThreadWaker`](super::MainThreadWaker) asked for a call on
    /// the main thread.
    Wake,
}

/// How a [`RecordingEditorHost`] answers resize requests.
type ResizeAnswer = Rc<dyn Fn(HostSize) -> Option<HostSize>>;

/// An [`EditorHost`] that records every call, in order.
///
/// Questions ([`host_loop`](EditorHost::host_loop) and
/// [`main_thread_callback`](EditorHost::main_thread_callback)) are answered
/// but not recorded. By default every resize request is granted as asked,
/// main-thread callbacks are offered, there is no host loop and values are
/// not stored anywhere; the `with_` and `set_` methods change that.
///
/// Wakes come from any thread. Each is recorded as [`HostCall::Wake`] when
/// the host is next called or read on its own thread, so a wake from the
/// thread that reads the calls is recorded in its place, and a wake from
/// another thread no later than the first call or read after it.
///
/// ```
/// use std::rc::Rc;
/// use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
/// use manguebyte_plugin::editor::{EditorContext, Gesture, HostSize};
/// use manguebyte_params::ParamId;
///
/// let recorder = Rc::new(RecordingEditorHost::new());
/// let (context, link) = EditorContext::attach(recorder.clone());
///
/// let size = HostSize::new(640, 480);
/// assert_eq!(context.request_resize(size), Some(size));
/// // From now on the host keeps windows within 800 by 600.
/// recorder.set_resize_answer(|wanted| {
///     Some(HostSize::new(wanted.width.min(800), wanted.height.min(600)))
/// });
/// assert_eq!(context.request_resize(HostSize::new(9999, 9999)), Some(HostSize::new(800, 600)));
///
/// let level = ParamId::from_name("level");
/// let gesture = Gesture::begin(&context, level, 0.0);
/// assert_eq!(recorder.open_at_host(), [level]);
/// drop(gesture);
/// drop(link);
///
/// assert_eq!(recorder.take_calls(), [
///     HostCall::RequestResize(size),
///     HostCall::RequestResize(HostSize::new(9999, 9999)),
///     HostCall::BeginGesture(level),
///     HostCall::EndGesture(level),
/// ]);
/// assert_eq!(recorder.well_formed(), Ok(()));
/// ```
pub struct RecordingEditorHost {
    calls: RefCell<Vec<HostCall>>,
    /// Wakes not yet recorded, counted from any thread.
    wakes: Arc<AtomicUsize>,
    offers_callbacks: bool,
    resize_answer: RefCell<ResizeAnswer>,
    host_loop: Option<Rc<dyn HostLoop>>,
    params: Option<Arc<dyn ParamSet>>,
}

impl RecordingEditorHost {
    /// A host that has recorded nothing.
    pub fn new() -> Self {
        let grant_as_asked: ResizeAnswer = Rc::new(Some::<HostSize>);
        Self {
            calls: RefCell::new(Vec::new()),
            wakes: Arc::new(AtomicUsize::new(0)),
            offers_callbacks: true,
            resize_answer: RefCell::new(grant_as_asked),
            host_loop: None,
            params: None,
        }
    }

    /// Also stores every performed value into the matching parameter of
    /// `params`, as a standalone program does, so that an editor under test
    /// reads back what it set.
    #[must_use]
    pub fn with_params(mut self, params: Arc<dyn ParamSet>) -> Self {
        self.params = Some(params);
        self
    }

    /// Offers `host_loop` from [`EditorHost::host_loop`], as a host on
    /// Linux does.
    #[must_use]
    pub fn with_loop(mut self, host_loop: Rc<dyn HostLoop>) -> Self {
        self.host_loop = Some(host_loop);
        self
    }

    /// Offers no main-thread callbacks, as a host with no way to ask for
    /// them does.
    #[must_use]
    pub fn without_main_thread_callback(mut self) -> Self {
        self.offers_callbacks = false;
        self
    }

    /// Sets how [`EditorHost::request_resize`] answers from now on: with
    /// the size `answer` gives for the size asked for, or `None` to refuse.
    pub fn set_resize_answer(&self, answer: impl Fn(HostSize) -> Option<HostSize> + 'static) {
        *self.resize_answer.borrow_mut() = Rc::new(answer);
    }

    /// Every call recorded so far.
    pub fn calls(&self) -> Vec<HostCall> {
        self.record_wakes();
        self.calls.borrow().clone()
    }

    /// Every call recorded so far, forgetting them.
    pub fn take_calls(&self) -> Vec<HostCall> {
        self.record_wakes();
        self.calls.take()
    }

    /// The parameters on which, going by the calls recorded, a gesture has
    /// begun and not ended, in the order they began.
    pub fn open_at_host(&self) -> Vec<ParamId> {
        let mut open = Vec::new();
        for call in self.calls() {
            match call {
                HostCall::BeginGesture(param) => open.push(param),
                HostCall::EndGesture(param) => open.retain(|&id| id != param),
                _ => {}
            }
        }
        open
    }

    /// Checks that the calls recorded so far are what a plugin format
    /// accepts: on each parameter, begins and ends alternate starting with
    /// a begin, and values are performed only while a gesture is open and
    /// only if they are finite and within `0..=1`. Gestures still open are
    /// not an error. The error names the first call that breaks a rule,
    /// counting from 0.
    pub fn well_formed(&self) -> Result<(), String> {
        let mut open: Vec<ParamId> = Vec::new();
        for (index, call) in self.calls().iter().enumerate() {
            match *call {
                HostCall::BeginGesture(param) if open.contains(&param) => {
                    return Err(format!(
                        "call {index}: {call:?} while a gesture on that parameter is open"
                    ));
                }
                HostCall::BeginGesture(param) => open.push(param),
                HostCall::EndGesture(param) | HostCall::Perform(param, _)
                    if !open.contains(&param) =>
                {
                    return Err(format!(
                        "call {index}: {call:?} with no gesture open on that parameter"
                    ));
                }
                HostCall::Perform(_, value) if !(0.0..=1.0).contains(&value) => {
                    return Err(format!("call {index}: {call:?} with a value outside 0..=1"));
                }
                HostCall::EndGesture(param) => open.retain(|&id| id != param),
                _ => {}
            }
        }
        Ok(())
    }

    fn record(&self, call: HostCall) {
        self.record_wakes();
        self.calls.borrow_mut().push(call);
    }

    /// Records the wakes counted since the last call or read.
    fn record_wakes(&self) {
        let wakes = self.wakes.swap(0, Ordering::AcqRel);
        if wakes > 0 {
            self.calls
                .borrow_mut()
                .extend(std::iter::repeat_n(HostCall::Wake, wakes));
        }
    }
}

impl Default for RecordingEditorHost {
    fn default() -> Self {
        Self::new()
    }
}

impl EditorHost for RecordingEditorHost {
    fn begin_gesture(&self, param: ParamId) {
        self.record(HostCall::BeginGesture(param));
    }

    fn perform(&self, param: ParamId, normalized: f64) {
        self.record(HostCall::Perform(param, normalized));
        if let Some(target) = self
            .params
            .as_ref()
            .and_then(|params| params.find_param(param))
        {
            target.set_normalized(normalized);
        }
    }

    fn end_gesture(&self, param: ParamId) {
        self.record(HostCall::EndGesture(param));
    }

    fn values_changed(&self) {
        self.record(HostCall::ValuesChanged);
    }

    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        self.record(HostCall::RequestResize(size));
        // Cloned out, so that the answer may call the host.
        let answer = Rc::clone(&self.resize_answer.borrow());
        answer(size)
    }

    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        if !self.offers_callbacks {
            return None;
        }
        let wakes = Arc::clone(&self.wakes);
        Some(Box::new(move || {
            wakes.fetch_add(1, Ordering::AcqRel);
        }))
    }

    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        self.host_loop.clone()
    }

    fn closed(&self) {
        self.record(HostCall::Closed);
    }
}

impl fmt::Debug for RecordingEditorHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordingEditorHost")
            .field("calls", &self.calls.borrow())
            .field("pending_wakes", &self.wakes.load(Ordering::Acquire))
            .field("offers_callbacks", &self.offers_callbacks)
            .field("host_loop", &self.host_loop.is_some())
            .field("params", &self.params.is_some())
            .finish_non_exhaustive()
    }
}

/// A [`HostLoop`] that does nothing until a test fires its timers or
/// descriptors.
///
/// It keeps the promises the contract makes: a cancelled callback never
/// runs again and is dropped before [`cancel`](HostLoop::cancel) returns
/// (or, if it is the one running, as soon as it returns); a callback may
/// cancel itself or register others while it runs; and tokens are never
/// reused. Editors reach it through
/// [`EditorContext::every`](super::EditorContext::every) and
/// [`EditorContext::watch_fd`](super::EditorContext::watch_fd); a test of a
/// wrapper may also call it directly, as here.
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
/// use manguebyte_plugin::editor::HostLoop;
/// use manguebyte_plugin::editor::testing::ManualLoop;
///
/// let host_loop = ManualLoop::new();
/// let ticks = Rc::new(Cell::new(0));
/// let counter = Rc::clone(&ticks);
/// let token = host_loop
///     .every(16, Box::new(move || counter.set(counter.get() + 1)))
///     .expect("the loop accepts timers");
///
/// assert_eq!(host_loop.fire_timers(), 1);
/// host_loop.cancel(token);
/// assert_eq!(host_loop.fire_timers(), 0);
/// assert_eq!(ticks.get(), 1);
/// assert_eq!(Rc::strong_count(&ticks), 1, "the callback was dropped");
/// ```
pub struct ManualLoop {
    next_token: Cell<u64>,
    refusing: Cell<bool>,
    cancels: Cell<usize>,
    entries: RefCell<Vec<Entry>>,
}

struct Entry {
    token: LoopToken,
    source: Source,
    /// `None` while the callback is running.
    callback: Option<Box<dyn FnMut()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Fd(i32),
    Timer(u32),
}

impl ManualLoop {
    /// A loop with nothing registered.
    pub fn new() -> Self {
        Self {
            next_token: Cell::new(1),
            refusing: Cell::new(false),
            cancels: Cell::new(0),
            entries: RefCell::new(Vec::new()),
        }
    }

    /// Makes the loop refuse new timers and watches from now on, as a host
    /// that has run out of them does.
    pub fn set_refusing(&self, refusing: bool) {
        self.refusing.set(refusing);
    }

    /// Runs every timer once, in the order they were registered, and
    /// returns how many ran. Timers registered while this runs wait for the
    /// next call.
    pub fn fire_timers(&self) -> usize {
        self.fire(|source| matches!(source, Source::Timer(_)))
    }

    /// Runs every watch on `fd` once, as when it becomes readable, and
    /// returns how many ran.
    pub fn fire_fd(&self, fd: i32) -> usize {
        self.fire(|source| source == Source::Fd(fd))
    }

    /// The periods of the timers registered and not cancelled, in
    /// milliseconds.
    pub fn timers(&self) -> Vec<u32> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry.source {
                Source::Timer(period) => Some(period),
                Source::Fd(_) => None,
            })
            .collect()
    }

    /// The descriptors watched and not cancelled.
    pub fn watched_fds(&self) -> Vec<i32> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry.source {
                Source::Fd(fd) => Some(fd),
                Source::Timer(_) => None,
            })
            .collect()
    }

    /// How many times [`cancel`](HostLoop::cancel) has been called, with
    /// tokens known or not.
    pub fn cancels(&self) -> usize {
        self.cancels.get()
    }

    fn register(&self, source: Source, callback: Box<dyn FnMut()>) -> Option<LoopToken> {
        if self.refusing.get() {
            return None;
        }
        let token = LoopToken::new(self.next_token.get());
        self.next_token.set(token.get() + 1);
        self.entries.borrow_mut().push(Entry {
            token,
            source,
            callback: Some(callback),
        });
        Some(token)
    }

    fn fire(&self, wanted: impl Fn(Source) -> bool) -> usize {
        let due: Vec<LoopToken> = self
            .entries
            .borrow()
            .iter()
            .filter(|entry| wanted(entry.source))
            .map(|entry| entry.token)
            .collect();
        let mut ran = 0;
        for token in due {
            // Taken out while it runs, so that it may call the loop.
            let callback = self
                .entries
                .borrow_mut()
                .iter_mut()
                .find(|entry| entry.token == token)
                .and_then(|entry| entry.callback.take());
            let Some(mut callback) = callback else {
                // Cancelled by an earlier callback, or already running.
                continue;
            };
            callback();
            ran += 1;
            let cancelled = {
                let mut entries = self.entries.borrow_mut();
                match entries.iter_mut().find(|entry| entry.token == token) {
                    Some(entry) => {
                        entry.callback = Some(callback);
                        None
                    }
                    None => Some(callback),
                }
            };
            // A callback that cancelled itself is dropped here, after the
            // table is released, in case dropping it calls the loop.
            drop(cancelled);
        }
        ran
    }
}

impl Default for ManualLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl HostLoop for ManualLoop {
    fn watch_fd(&self, fd: i32, ready: Box<dyn FnMut()>) -> Option<LoopToken> {
        self.register(Source::Fd(fd), ready)
    }

    fn every(&self, period_ms: u32, tick: Box<dyn FnMut()>) -> Option<LoopToken> {
        self.register(Source::Timer(period_ms), tick)
    }

    fn cancel(&self, token: LoopToken) {
        self.cancels.set(self.cancels.get() + 1);
        let removed = {
            let mut entries = self.entries.borrow_mut();
            entries
                .iter()
                .position(|entry| entry.token == token)
                .map(|index| entries.remove(index))
        };
        // Dropped after the table is released, in case dropping the
        // callback calls the loop.
        drop(removed);
    }
}

impl fmt::Debug for ManualLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManualLoop")
            .field("timers", &self.timers())
            .field("watched_fds", &self.watched_fds())
            .field("refusing", &self.refusing.get())
            .field("cancels", &self.cancels.get())
            .finish()
    }
}
