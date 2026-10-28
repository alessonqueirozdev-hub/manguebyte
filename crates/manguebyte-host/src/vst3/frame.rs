//! The host as a hosted plugin's editor sees it: the frame its view is put
//! in, and the run loop its timers and file descriptors are registered
//! with.
//!
//! VST 3 gives a view one host object, an `IPlugFrame`, through
//! `setFrame`. The view asks it for a new size with `resizeView`. On Linux
//! the same object also answers for `IRunLoop`, because an editor there has
//! no event loop of its own and runs from its host's: it registers timers
//! and file descriptors, and the host calls them back on the main thread.
//!
//! As in the rest of this host, the callbacks do not act. A resize request
//! is recorded for [`Vst3Plugin::pump`](super::Vst3Plugin::pump) to hand
//! the application, which owns the window and answers with
//! [`Vst3Plugin::set_editor_size`](super::Vst3Plugin::set_editor_size).
//! Timers are kept in the shared [`Timers`] registry and fired by `pump`;
//! descriptors are kept in [`FdWatches`] for the application to watch.
//!
//! Every frame belongs to one opening of the editor. A frame from an
//! earlier opening that the plugin still holds refuses everything, and so
//! does any frame once the editor is closed, so nothing the plugin
//! registers outlives the view it registered it for.
//!
//! The registries hold numbers only. The plugin's handler objects are kept
//! beside them, with a reference of the host's own, from registration until
//! they are unregistered or the editor closes. A handler is only ever
//! called with no lock held, through a reference taken for the call, so it
//! may register and unregister anything, itself included, while it runs.
//!
//! All of this is the main thread's. A call from another thread is refused
//! and counted, and [`Vst3Plugin::pump`](super::Vst3Plugin::pump) remarks
//! on it.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoopTrait, ITimerHandler, ITimerHandlerTrait,
};
use vst3::Steinberg::{
    IPlugFrame, IPlugFrameTrait, IPlugView, ViewRect, kInvalidArgument, kResultFalse, kResultTrue,
    tresult,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use super::objects::lock;
use crate::editor::{EditorSize, FdInterest, FdWatch, FdWatches, Timers};
use crate::threads::{guard, thread_token};

/// The size of a rectangle a plugin passes, or `None` if it is null or
/// inside out.
///
/// # Safety
///
/// `rect` must be null or valid to read.
pub(crate) unsafe fn rect_size(rect: *const ViewRect) -> Option<EditorSize> {
    // SAFETY: per this function's contract.
    let rect = unsafe { rect.as_ref() }?;
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    Some(EditorSize::new(
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// A rectangle of `size` at the origin. A side that does not fit in VST 3's
/// coordinates ends at the largest coordinate.
pub(crate) fn rect_of(size: EditorSize) -> ViewRect {
    let end = |length: u32| i32::try_from(length).unwrap_or(i32::MAX);
    ViewRect {
        left: 0,
        top: 0,
        right: end(size.width),
        bottom: end(size.height),
    }
}

/// What the editor's frame and run loop record, for the main half of the
/// plugin to act on.
///
/// The handlers are the plugin's objects, each with a reference the host
/// holds. They are taken, called and released on the main thread only:
/// every function that adds one, or takes one out to call it, checks the
/// thread first, and the lists are emptied on the main thread when the
/// editor closes, which the main half of the plugin does before it goes.
#[derive(Default)]
struct State {
    /// The serial of the frame of the open editor, or zero when no editor
    /// is open. A frame of another serial refuses everything.
    current: u64,
    /// The serial of the last frame made.
    last_serial: u64,
    /// The latest size the editor asked for and the application has not
    /// been told of yet. Only the latest counts: it replaces the others.
    resize: Option<EditorSize>,
    timers: Timers,
    /// The handler of each timer, by the timer's id.
    timer_handlers: Vec<(u32, ComPtr<ITimerHandler>)>,
    fds: FdWatches,
    /// The handler of each watched descriptor.
    fd_handlers: Vec<(i32, ComPtr<IEventHandler>)>,
}

/// What every frame of one plugin shares with its main half.
pub(crate) struct EditorLoop {
    /// The thread the plugin was created on, as its [`thread_token`].
    main_thread: u64,
    /// Calls made on a thread other than the main one, which were refused.
    off_thread: AtomicU32,
    state: Mutex<State>,
}

// The frame may be released by the plugin on any thread, so what it points
// at has to be safe to share. This fails to compile if a field is ever
// added for which that is not true.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<EditorLoop>();
};

impl EditorLoop {
    /// An editor loop with nothing open. The calling thread is the plugin's
    /// main thread.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            main_thread: thread_token(),
            off_thread: AtomicU32::new(0),
            state: Mutex::new(State::default()),
        })
    }

    /// The state, for a call from the plugin: `None`, and the call counted,
    /// on a thread other than the main one, and `None` too if the state is
    /// in use further up the main thread's stack, which no call of this
    /// host's into the plugin allows but a refusal is better than a
    /// deadlock.
    fn for_plugin(&self) -> Option<MutexGuard<'_, State>> {
        let token = thread_token();
        if token == 0 || token != self.main_thread {
            self.off_thread.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        match self.state.try_lock() {
            Ok(state) => Some(state),
            Err(TryLockError::Poisoned(poison)) => Some(poison.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    /// Starts a new opening of the editor and returns the frame its view is
    /// given. Main thread.
    pub(crate) fn begin(self: &Arc<Self>) -> ComWrapper<Frame> {
        let serial = {
            let mut state = lock(&self.state);
            state.last_serial += 1;
            state.current = state.last_serial;
            state.resize = None;
            state.current
        };
        ComWrapper::new(Frame {
            shared: Arc::clone(self),
            serial,
        })
    }

    /// Ends the opening of the editor: every frame refuses from now on, and
    /// every timer, descriptor and request is forgotten. Main thread.
    ///
    /// The handlers are released after the lock, since releasing one runs
    /// the plugin's code, which may call a frame.
    pub(crate) fn end(&self) {
        let (timers, fds) = {
            let mut state = lock(&self.state);
            state.current = 0;
            state.resize = None;
            state.timers.clear();
            state.fds.clear();
            (
                std::mem::take(&mut state.timer_handlers),
                std::mem::take(&mut state.fd_handlers),
            )
        };
        drop(timers);
        drop(fds);
    }

    /// The size the editor asked for since the last call, if it did.
    pub(crate) fn take_resize(&self) -> Option<EditorSize> {
        lock(&self.state).resize.take()
    }

    /// How many calls came on another thread since the last call.
    pub(crate) fn take_off_thread(&self) -> u32 {
        self.off_thread.swap(0, Ordering::Relaxed)
    }

    /// When the editor's next timer is due.
    pub(crate) fn next_timer(&self) -> Option<Instant> {
        lock(&self.state).timers.next_due()
    }

    /// The descriptors the editor asked to be watched.
    pub(crate) fn fds(&self) -> Vec<FdWatch> {
        lock(&self.state).fds.list().to_vec()
    }

    /// Calls every timer due at `now` once, in the order they were
    /// registered. `due` is room for their ids, kept by the caller between
    /// calls. Main thread.
    ///
    /// The ids are taken first and the lock let go. Each handler is then
    /// looked up again just before it is called, so that a timer an earlier
    /// one unregistered does not fire, and called through a reference of
    /// its own, so that it may unregister itself while it runs.
    pub(crate) fn fire_timers(&self, now: Instant, due: &mut Vec<u32>) {
        {
            let mut state = lock(&self.state);
            if state.timers.is_empty() {
                return;
            }
            state.timers.take_due(now, due);
        }
        for &id in due.iter() {
            let handler = lock(&self.state)
                .timer_handlers
                .iter()
                .find(|(timer, _)| *timer == id)
                .map(|(_, handler)| handler.clone());
            if let Some(handler) = handler {
                // SAFETY: a handler the plugin registered and has not
                // unregistered, kept alive by the reference taken above, on
                // the main thread, with no lock of this host's held.
                unsafe { handler.onTimer() };
            }
        }
        due.clear();
    }

    /// Tells the handler of `fd` that the descriptor is ready, if the
    /// editor watches it. Main thread.
    pub(crate) fn fd_ready(&self, fd: i32, ready: FdInterest) {
        if ready.is_empty() {
            return;
        }
        let handler = {
            let state = lock(&self.state);
            if state.fds.get(fd).is_none() {
                return;
            }
            state
                .fd_handlers
                .iter()
                .find(|(watched, _)| *watched == fd)
                .map(|(_, handler)| handler.clone())
        };
        if let Some(handler) = handler {
            // SAFETY: as for a timer in `fire_timers`.
            unsafe { handler.onFDIsSet(fd) };
        }
    }
}

/// The frame an editor's view is put in, for one opening of the editor.
pub(crate) struct Frame {
    shared: Arc<EditorLoop>,
    serial: u64,
}

impl Frame {
    /// The state, if the call may go ahead: on the main thread, and for the
    /// editor that is open now.
    fn enter(&self) -> Option<MutexGuard<'_, State>> {
        let state = self.shared.for_plugin()?;
        (state.current == self.serial && self.serial != 0).then_some(state)
    }
}

/// The interfaces a frame answers for. `IRunLoop` is where VST 3 has a host
/// offer one: on Linux and the other systems whose windows are X11's. A
/// plugin elsewhere runs its editor from the system's own loop.
#[cfg(all(unix, not(target_os = "macos")))]
type FrameInterfaces = (IPlugFrame, vst3::Steinberg::Linux::IRunLoop);
#[cfg(not(all(unix, not(target_os = "macos"))))]
type FrameInterfaces = (IPlugFrame,);

impl Class for Frame {
    type Interfaces = FrameInterfaces;
}

/// The pointer a view is given as its frame.
pub(crate) fn frame_ptr(frame: &ComWrapper<Frame>) -> *mut IPlugFrame {
    frame
        .as_com_ref::<IPlugFrame>()
        .map_or(std::ptr::null_mut(), |frame| frame.as_ptr())
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, _view: *mut IPlugView, new_size: *mut ViewRect) -> tresult {
        // The view is not needed to know which one is meant: a frame
        // belongs to one view.
        guard(kResultFalse, || {
            let Some(mut state) = self.enter() else {
                return kResultFalse;
            };
            // SAFETY: a rectangle the plugin owns, or null.
            let Some(size) = (unsafe { rect_size(new_size) }) else {
                return kInvalidArgument;
            };
            // Granted, for the application to carry out: it makes its
            // window this size and then gives the view its `onSize`.
            state.resize = Some(size);
            kResultTrue
        })
    }
}

impl IRunLoopTrait for Frame {
    unsafe fn registerEventHandler(&self, handler: *mut IEventHandler, fd: i32) -> tresult {
        guard(kResultFalse, || {
            let Some(mut state) = self.enter() else {
                return kResultFalse;
            };
            // SAFETY: the plugin's handler, valid for this call, or null.
            let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
                return kInvalidArgument;
            };
            // VST 3 has a handler told when its descriptor can be read.
            let read = FdInterest {
                read: true,
                ..FdInterest::default()
            };
            if !state.fds.add(fd, read) {
                return kResultFalse;
            }
            state.fd_handlers.push((fd, handler.to_com_ptr()));
            kResultTrue
        })
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        guard(kResultFalse, || {
            let Some(mut state) = self.enter() else {
                return kResultFalse;
            };
            if handler.is_null() {
                return kInvalidArgument;
            }
            let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.fd_handlers)
                .into_iter()
                .partition(|(_, held)| held.as_ptr() == handler);
            state.fd_handlers = kept;
            for (fd, _) in &gone {
                state.fds.remove(*fd);
            }
            drop(state);
            // Released with no lock held.
            if gone.is_empty() {
                kResultFalse
            } else {
                kResultTrue
            }
        })
    }

    unsafe fn registerTimer(&self, handler: *mut ITimerHandler, milliseconds: u64) -> tresult {
        guard(kResultFalse, || {
            let Some(mut state) = self.enter() else {
                return kResultFalse;
            };
            // SAFETY: the plugin's handler, valid for this call, or null.
            let Some(handler) = (unsafe { ComRef::from_raw(handler) }) else {
                return kInvalidArgument;
            };
            let Some(id) = state
                .timers
                .add(Duration::from_millis(milliseconds), Instant::now())
            else {
                return kResultFalse;
            };
            state.timer_handlers.push((id, handler.to_com_ptr()));
            kResultTrue
        })
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        guard(kResultFalse, || {
            let Some(mut state) = self.enter() else {
                return kResultFalse;
            };
            if handler.is_null() {
                return kInvalidArgument;
            }
            // Every timer of the handler: VST 3 names timers by handler.
            let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut state.timer_handlers)
                .into_iter()
                .partition(|(_, held)| held.as_ptr() == handler);
            state.timer_handlers = kept;
            for (id, _) in &gone {
                state.timers.remove(*id);
            }
            drop(state);
            // Released with no lock held.
            if gone.is_empty() {
                kResultFalse
            } else {
                kResultTrue
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use super::*;

    /// A plugin's timer, which counts its calls and may act on the frame
    /// while it runs.
    struct Ticker {
        calls: Rc<Cell<u32>>,
        /// Run on every call, with the ticker's own pointer.
        act: RefCell<Box<dyn FnMut(*mut ITimerHandler)>>,
        this: Cell<*mut ITimerHandler>,
    }

    impl Class for Ticker {
        type Interfaces = (ITimerHandler,);
    }

    impl ITimerHandlerTrait for Ticker {
        unsafe fn onTimer(&self) {
            self.calls.set(self.calls.get() + 1);
            (self.act.borrow_mut())(self.this.get());
        }
    }

    fn ticker(
        act: impl FnMut(*mut ITimerHandler) + 'static,
    ) -> (ComPtr<ITimerHandler>, Rc<Cell<u32>>) {
        let calls = Rc::new(Cell::new(0));
        let wrapper = ComWrapper::new(Ticker {
            calls: Rc::clone(&calls),
            act: RefCell::new(Box::new(act)),
            this: Cell::new(std::ptr::null_mut()),
        });
        let pointer = wrapper.to_com_ptr::<ITimerHandler>().unwrap();
        wrapper.this.set(pointer.as_ptr());
        (pointer, calls)
    }

    /// A plugin's descriptor handler, which records what it was told.
    struct Watcher {
        told: Rc<RefCell<Vec<i32>>>,
    }

    impl Class for Watcher {
        type Interfaces = (IEventHandler,);
    }

    impl IEventHandlerTrait for Watcher {
        unsafe fn onFDIsSet(&self, fd: i32) {
            self.told.borrow_mut().push(fd);
        }
    }

    fn watcher() -> (ComPtr<IEventHandler>, Rc<RefCell<Vec<i32>>>) {
        let told = Rc::new(RefCell::new(Vec::new()));
        let wrapper = ComWrapper::new(Watcher {
            told: Rc::clone(&told),
        });
        (wrapper.to_com_ptr::<IEventHandler>().unwrap(), told)
    }

    fn resize(frame: &Frame, size: EditorSize) -> tresult {
        let mut rect = rect_of(size);
        // SAFETY: a rectangle on the stack; the view is not read.
        unsafe { frame.resizeView(std::ptr::null_mut(), &raw mut rect) }
    }

    #[test]
    fn resize_requests_wait_for_the_application_and_only_the_latest_counts() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        assert_eq!(resize(&frame, EditorSize::new(300, 200)), kResultTrue);
        assert_eq!(resize(&frame, EditorSize::new(320, 240)), kResultTrue);
        assert_eq!(editors.take_resize(), Some(EditorSize::new(320, 240)));
        assert_eq!(editors.take_resize(), None);

        // Null and inside-out rectangles are refused.
        // SAFETY: null is allowed; a rectangle on the stack.
        unsafe {
            assert_eq!(
                frame.resizeView(std::ptr::null_mut(), std::ptr::null_mut()),
                kInvalidArgument
            );
            let mut inside_out = ViewRect {
                left: 10,
                top: 0,
                right: 0,
                bottom: 10,
            };
            assert_eq!(
                frame.resizeView(std::ptr::null_mut(), &raw mut inside_out),
                kInvalidArgument
            );
        }
        assert_eq!(editors.take_resize(), None);

        // Once the editor is closed, and for a frame of an earlier
        // opening, nothing is taken.
        editors.end();
        assert_eq!(resize(&frame, EditorSize::new(10, 10)), kResultFalse);
        let second = editors.begin();
        assert_eq!(resize(&frame, EditorSize::new(10, 10)), kResultFalse);
        assert_eq!(resize(&second, EditorSize::new(20, 20)), kResultTrue);
        assert_eq!(editors.take_resize(), Some(EditorSize::new(20, 20)));
        // A request not yet taken goes with the editor.
        assert_eq!(resize(&second, EditorSize::new(30, 30)), kResultTrue);
        editors.end();
        assert_eq!(editors.take_resize(), None);
    }

    #[test]
    fn timers_are_registered_fired_when_due_and_unregistered() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        let (fast, fast_calls) = ticker(|_| {});
        let (slow, slow_calls) = ticker(|_| {});
        let start = Instant::now();
        // SAFETY: the handlers are alive; null is allowed.
        unsafe {
            assert_eq!(frame.registerTimer(fast.as_ptr(), 10), kResultTrue);
            assert_eq!(frame.registerTimer(slow.as_ptr(), 1_000), kResultTrue);
            assert_eq!(
                frame.registerTimer(std::ptr::null_mut(), 10),
                kInvalidArgument
            );
        }
        let next = editors.next_timer().expect("a timer is due");
        assert!(next >= start + Duration::from_millis(10));
        assert!(next < start + Duration::from_millis(1_000));

        let mut due = Vec::new();
        editors.fire_timers(start, &mut due);
        assert_eq!(
            (fast_calls.get(), slow_calls.get()),
            (0, 0),
            "nothing is due yet"
        );
        editors.fire_timers(next, &mut due);
        assert_eq!((fast_calls.get(), slow_calls.get()), (1, 0));
        editors.fire_timers(start + Duration::from_secs(5), &mut due);
        assert_eq!((fast_calls.get(), slow_calls.get()), (2, 1));

        // SAFETY: as above.
        unsafe {
            assert_eq!(frame.unregisterTimer(fast.as_ptr()), kResultTrue);
            assert_eq!(frame.unregisterTimer(fast.as_ptr()), kResultFalse);
        }
        editors.fire_timers(start + Duration::from_secs(60), &mut due);
        assert_eq!((fast_calls.get(), slow_calls.get()), (2, 2));

        // Closing the editor forgets the rest, and lets go of the handler.
        editors.end();
        assert_eq!(editors.next_timer(), None);
        editors.fire_timers(start + Duration::from_secs(120), &mut due);
        assert_eq!(slow_calls.get(), 2);
        // SAFETY: the handler is alive.
        unsafe { assert_eq!(frame.registerTimer(slow.as_ptr(), 10), kResultFalse) };
        drop(slow);
    }

    #[test]
    fn a_timer_may_unregister_itself_and_register_another_while_it_runs() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        let (other, other_calls) = ticker(|_| {});
        let other_pointer = other.as_ptr();
        let inner = frame.clone();
        let (once, once_calls) = ticker(move |this| {
            // SAFETY: the ticker's own pointer, alive while it runs, and
            // the other ticker, which the test keeps alive.
            unsafe {
                assert_eq!(inner.unregisterTimer(this), kResultTrue);
                assert_eq!(inner.registerTimer(other_pointer, 1), kResultTrue);
            }
        });
        // SAFETY: the handler is alive.
        unsafe { assert_eq!(frame.registerTimer(once.as_ptr(), 1), kResultTrue) };
        // Only the host's reference keeps the ticker alive now: it must
        // survive unregistering itself from inside its own call.
        drop(once);

        let later = Instant::now() + Duration::from_secs(1);
        let mut due = Vec::new();
        editors.fire_timers(later, &mut due);
        assert_eq!(once_calls.get(), 1);
        // The timer registered from inside was not due in the same round.
        assert_eq!(other_calls.get(), 0);
        editors.fire_timers(later + Duration::from_secs(1), &mut due);
        assert_eq!((once_calls.get(), other_calls.get()), (1, 1));
        editors.end();
        drop(other);
    }

    #[test]
    fn a_timer_unregistered_by_an_earlier_one_in_the_same_round_does_not_fire() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        let (second, second_calls) = ticker(|_| {});
        let second_pointer = second.as_ptr();
        let inner = frame.clone();
        let (first, _) = ticker(move |_| {
            // SAFETY: the second ticker, which the test keeps alive.
            unsafe { inner.unregisterTimer(second_pointer) };
        });
        // SAFETY: the handlers are alive.
        unsafe {
            frame.registerTimer(first.as_ptr(), 5);
            frame.registerTimer(second.as_ptr(), 5);
        }
        let mut due = Vec::new();
        editors.fire_timers(Instant::now() + Duration::from_secs(1), &mut due);
        assert_eq!(second_calls.get(), 0);
        editors.end();
    }

    #[test]
    fn descriptors_are_watched_reported_and_unregistered_by_handler() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        let (handler, told) = watcher();
        let (other, other_told) = watcher();
        // SAFETY: the handlers are alive; null is allowed.
        unsafe {
            assert_eq!(frame.registerEventHandler(handler.as_ptr(), 7), kResultTrue);
            assert_eq!(frame.registerEventHandler(handler.as_ptr(), 9), kResultTrue);
            // A descriptor is watched once, and a negative one is none.
            assert_eq!(frame.registerEventHandler(other.as_ptr(), 7), kResultFalse);
            assert_eq!(frame.registerEventHandler(other.as_ptr(), -1), kResultFalse);
            assert_eq!(frame.registerEventHandler(other.as_ptr(), 11), kResultTrue);
            assert_eq!(
                frame.registerEventHandler(std::ptr::null_mut(), 12),
                kInvalidArgument
            );
        }
        let read = FdInterest {
            read: true,
            ..FdInterest::default()
        };
        let fds: Vec<i32> = editors.fds().iter().map(|watch| watch.fd).collect();
        assert_eq!(fds, [7, 9, 11]);
        assert!(editors.fds().iter().all(|watch| watch.interest == read));

        editors.fd_ready(9, read);
        editors.fd_ready(
            11,
            FdInterest {
                error: true,
                ..FdInterest::default()
            },
        );
        // Nothing ready, and a descriptor nobody watches, call no one.
        editors.fd_ready(7, FdInterest::default());
        editors.fd_ready(8, read);
        assert_eq!(*told.borrow(), [9]);
        assert_eq!(*other_told.borrow(), [11]);

        // Unregistering a handler drops every descriptor it watched.
        // SAFETY: the handler is alive.
        unsafe {
            assert_eq!(frame.unregisterEventHandler(handler.as_ptr()), kResultTrue);
            assert_eq!(frame.unregisterEventHandler(handler.as_ptr()), kResultFalse);
        }
        let fds: Vec<i32> = editors.fds().iter().map(|watch| watch.fd).collect();
        assert_eq!(fds, [11]);
        editors.fd_ready(7, read);
        assert_eq!(*told.borrow(), [9]);

        editors.end();
        assert!(editors.fds().is_empty());
        editors.fd_ready(11, read);
        assert_eq!(*other_told.borrow(), [11]);
    }

    #[test]
    fn calls_from_another_thread_are_refused_and_counted() {
        let editors = EditorLoop::new();
        let frame = editors.begin();
        let (handler, calls) = ticker(|_| {});
        // Only the addresses cross: the other thread never reaches either
        // object, since it is turned away first.
        let address = handler.as_ptr().addr();
        let elsewhere = frame.clone();
        std::thread::spawn(move || {
            let pointer: *mut ITimerHandler = std::ptr::with_exposed_provenance_mut(address);
            let mut rect = rect_of(EditorSize::new(5, 5));
            // SAFETY: the calls are refused before the pointers are used.
            unsafe {
                assert_eq!(elsewhere.registerTimer(pointer, 1), kResultFalse);
                assert_eq!(
                    elsewhere.resizeView(std::ptr::null_mut(), &raw mut rect),
                    kResultFalse
                );
                assert_eq!(elsewhere.unregisterTimer(pointer), kResultFalse);
            }
        })
        .join()
        .unwrap();
        assert_eq!(editors.take_off_thread(), 3);
        assert_eq!(editors.take_off_thread(), 0);
        assert_eq!(editors.next_timer(), None);
        assert_eq!(editors.take_resize(), None);
        assert_eq!(calls.get(), 0);
        editors.end();
    }

    #[test]
    fn rectangles_convert_both_ways() {
        let rect = rect_of(EditorSize::new(300, 200));
        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (0, 0, 300, 200)
        );
        // SAFETY: a rectangle on the stack.
        assert_eq!(unsafe { rect_size(&rect) }, Some(EditorSize::new(300, 200)));
        let huge = rect_of(EditorSize::new(u32::MAX, 1));
        assert_eq!((huge.right, huge.bottom), (i32::MAX, 1));
        // SAFETY: null is allowed.
        assert_eq!(unsafe { rect_size(std::ptr::null()) }, None);
    }
}
