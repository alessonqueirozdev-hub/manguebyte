//! The editor's side of its link to a host.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, PoisonError};

use super::{EditorHost, HostLoop, HostSize, LoopToken, OpenGestures};

/// What an open editor can ask of whoever shows it.
///
/// A wrapper makes one with [`attach`](Self::attach) for each editor it
/// opens and passes it to [`EditorSpec::open`](super::EditorSpec::open).
/// Through it the editor asks for a new size, registers timers and file
/// descriptors on the host's loop, gets a [`MainThreadWaker`] for its other
/// threads, says that values changed or that it cannot go on, and, with a
/// [`Gesture`](super::Gesture), reports a person's edits.
///
/// That is all it can do. The [`EditorHost`] behind it stays out of reach,
/// so an edit cannot reach the host except through a `Gesture`, and nothing
/// the editor registers on the host's loop can outlive the editor.
///
/// A context is cheap to clone. Every clone is cut off from the host when
/// the wrapper drops the [`HostLink`] that came with it: from then on its
/// requests are refused, its gestures send nothing, and the host is never
/// called again through it, so an editor that keeps a clone after it is
/// closed cannot reach a host or a plugin instance that has gone away.
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
/// use manguebyte_plugin::editor::testing::{HostCall, ManualLoop, RecordingEditorHost};
/// use manguebyte_plugin::editor::{EditorContext, HostSize};
///
/// let host_loop = Rc::new(ManualLoop::new());
/// let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
/// let (context, link) = EditorContext::attach(recorder.clone());
///
/// // The host grants the size; the editor takes it as its own.
/// let size = HostSize::new(640, 480);
/// assert_eq!(context.request_resize(size), Some(size));
///
/// // A 16 ms timer on the host's loop, kept for as long as the editor
/// // keeps the handle.
/// let ticks = Rc::new(Cell::new(0));
/// let counter = Rc::clone(&ticks);
/// let timer = context.every(16, move || counter.set(counter.get() + 1));
/// assert!(timer.is_some());
/// assert_eq!(host_loop.fire_timers(), 1);
///
/// // The editor closes but leaks its timer handle (its destructor does not
/// // run); dropping the link cancels the timer anyway and cuts every clone
/// // of the context off.
/// let leaked = std::mem::ManuallyDrop::new(timer);
/// drop(link);
/// assert_eq!(host_loop.fire_timers(), 0);
/// assert_eq!(ticks.get(), 1);
/// assert_eq!(Rc::strong_count(&ticks), 1, "the callback was dropped");
/// assert!(!context.is_attached());
/// assert_eq!(context.request_resize(size), None);
/// assert_eq!(recorder.take_calls(), [HostCall::RequestResize(size)]);
///
/// // Should the handle be dropped after all, much later, the loop is not
/// // called again.
/// let cancels = host_loop.cancels();
/// drop(std::mem::ManuallyDrop::into_inner(leaked));
/// assert_eq!(host_loop.cancels(), cancels);
/// ```
///
/// An editor cannot report an edit except through a
/// [`Gesture`](super::Gesture):
///
/// ```compile_fail,E0599
/// use manguebyte_plugin::editor::EditorContext;
/// use manguebyte_params::ParamId;
///
/// fn stray_end(context: &EditorContext) {
///     context.end_gesture(ParamId::from_name("cutoff"));
/// }
/// ```
#[derive(Clone)]
pub struct EditorContext {
    shared: Rc<Shared>,
}

/// What every clone of a context and its link share.
struct Shared {
    /// The host, until the link is dropped.
    host: RefCell<Option<Rc<dyn EditorHost>>>,
    /// Set when the link starts to detach; from then on nothing new is
    /// accepted, though ends still reach the host until it is gone.
    detaching: Cell<bool>,
    gestures: OpenGestures,
    /// Timers and watches registered and not yet cancelled.
    registrations: RefCell<Vec<Registration>>,
    next_registration: Cell<u64>,
    /// Made once, when the context is attached, if the host offers it.
    waker: Option<MainThreadWaker>,
}

struct Registration {
    id: u64,
    host_loop: Rc<dyn HostLoop>,
    token: LoopToken,
}

impl EditorContext {
    /// A context for an editor about to be opened on `host`, and the link
    /// the wrapper keeps for as long as that editor is open.
    ///
    /// Asks `host` once for its
    /// [`main_thread_callback`](EditorHost::main_thread_callback).
    pub fn attach(host: Rc<dyn EditorHost>) -> (Self, HostLink) {
        let waker = host.main_thread_callback().map(MainThreadWaker::new);
        let shared = Rc::new(Shared {
            host: RefCell::new(Some(host)),
            detaching: Cell::new(false),
            gestures: OpenGestures::new(),
            registrations: RefCell::new(Vec::new()),
            next_registration: Cell::new(0),
            waker,
        });
        let link = HostLink {
            shared: Rc::clone(&shared),
        };
        (Self { shared }, link)
    }

    /// Whether the context still reaches the host: true until the wrapper
    /// drops the [`HostLink`].
    pub fn is_attached(&self) -> bool {
        !self.shared.detaching.get() && self.shared.host.borrow().is_some()
    }

    /// Asks the host to make the editor's window `size`.
    ///
    /// Returns the size the window has now, normally `size`, which the
    /// editor takes as its own without waiting for
    /// [`OpenEditor::set_size`](super::OpenEditor::set_size); or `None` if
    /// the host refused or the context is detached.
    ///
    /// The host never calls back into the editor from inside this call, so
    /// an editor may make it from anywhere, including from
    /// [`EditorSpec::open`](super::EditorSpec::open) and
    /// [`OpenEditor::set_scale`](super::OpenEditor::set_scale).
    pub fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        self.live_host()?.request_resize(size)
    }

    /// Tells the host that the editor changed many parameters without
    /// gestures, such as by loading a preset of its own, so that it reads
    /// every value again.
    pub fn values_changed(&self) {
        if let Some(host) = self.live_host() {
            host.values_changed();
        }
    }

    /// Tells the host the editor cannot go on: its window was destroyed
    /// under it, or it failed. The wrapper drops the editor later, never
    /// from inside this call.
    pub fn closed(&self) {
        if let Some(host) = self.live_host() {
            host.closed();
        }
    }

    /// A handle that other threads use to have
    /// [`OpenEditor::on_main_thread`](super::OpenEditor::on_main_thread)
    /// called on the host's main thread, or `None` if the host offers no
    /// way to do that or the context is detached. An editor without one
    /// checks for work from other threads on its own timer instead.
    pub fn main_thread_waker(&self) -> Option<MainThreadWaker> {
        if self.is_attached() {
            self.shared.waker.clone()
        } else {
            None
        }
    }

    /// Whether the host offers a loop for timers and file descriptors.
    ///
    /// On Linux an editor inside a host runs entirely from the host's loop
    /// (CLAP `timer-support` and `posix-fd-support`, VST 3 `IRunLoop`),
    /// never from a thread of its own. Hosts offer one there, and usually
    /// not elsewhere. An editor that needs a loop and finds none refuses to
    /// open with [`EditorError::NoHostLoop`](super::EditorError::NoHostLoop).
    pub fn has_host_loop(&self) -> bool {
        self.live_host()
            .is_some_and(|host| host.host_loop().is_some())
    }

    /// Calls `tick` about every `period_ms` milliseconds on the host's
    /// loop, until the returned handle is dropped or the context is
    /// detached. `None` if there is no loop, the host refused, or the
    /// context is detached. Hosts may tick more slowly than asked; CLAP
    /// promises only about 30 Hz.
    pub fn every(&self, period_ms: u32, tick: impl FnMut() + 'static) -> Option<LoopHandle> {
        self.register(|host_loop| host_loop.every(period_ms, Box::new(tick)))
    }

    /// Calls `ready` whenever `fd` can be read, until the returned handle
    /// is dropped or the context is detached. `None` if there is no loop,
    /// the host refused, or the context is detached.
    pub fn watch_fd(&self, fd: i32, ready: impl FnMut() + 'static) -> Option<LoopHandle> {
        self.register(|host_loop| host_loop.watch_fd(fd, Box::new(ready)))
    }

    /// The gestures open on each parameter through this context.
    pub fn open_gestures(&self) -> &OpenGestures {
        &self.shared.gestures
    }

    /// The host, while the context is attached and not detaching.
    pub(super) fn live_host(&self) -> Option<Rc<dyn EditorHost>> {
        if self.shared.detaching.get() {
            return None;
        }
        self.shared.host.borrow().clone()
    }

    /// The host, until the link is gone. Ends still reach it while the
    /// link detaches, so that a gesture dropped then is ended normally.
    pub(super) fn host_for_ends(&self) -> Option<Rc<dyn EditorHost>> {
        self.shared.host.borrow().clone()
    }

    fn register(&self, add: impl FnOnce(&dyn HostLoop) -> Option<LoopToken>) -> Option<LoopHandle> {
        let host_loop = self.live_host()?.host_loop()?;
        let token = add(&*host_loop)?;
        if !self.is_attached() {
            // Detached while the host was registering it.
            host_loop.cancel(token);
            return None;
        }
        let id = self.shared.next_registration.get();
        self.shared.next_registration.set(id.wrapping_add(1));
        self.shared.registrations.borrow_mut().push(Registration {
            id,
            host_loop,
            token,
        });
        Some(LoopHandle {
            shared: Rc::downgrade(&self.shared),
            id,
        })
    }
}

impl fmt::Debug for EditorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EditorContext")
            .field("attached", &self.is_attached())
            .field("open_gestures", &self.shared.gestures)
            .field("registrations", &self.shared.registrations.borrow().len())
            .field("waker", &self.shared.waker.is_some())
            .finish()
    }
}

impl Shared {
    /// Ends what the editor left behind and cuts every context off.
    fn detach(&self) {
        if self.detaching.replace(true) {
            return;
        }
        let Some(host) = self.host.borrow().clone() else {
            return;
        };
        // Whatever happens below, the host and the waker are let go of.
        let _forget = ForgetHost(self);

        // Timers and watches the editor did not cancel. Cancelling drops
        // their callbacks, which may end gestures they held; those ends
        // still reach the host.
        let registrations = self.registrations.take();
        for registration in registrations {
            registration.host_loop.cancel(registration.token);
        }
        // Gestures the editor did not end: forgotten, or held by something
        // that outlives it.
        for param in self.gestures.close_all() {
            host.end_gesture(param);
        }
    }
}

/// Lets go of the host and the waker when dropped, even while unwinding.
struct ForgetHost<'a>(&'a Shared);

impl Drop for ForgetHost<'_> {
    fn drop(&mut self) {
        if let Some(waker) = &self.0.waker {
            waker.detach();
        }
        let host = self.0.host.replace(None);
        drop(host);
    }
}

/// The wrapper's end of an [`EditorContext`]: kept for as long as the
/// editor is open, and dropped right after the editor is.
///
/// Dropping it ends every gesture the editor left open, cancels every
/// timer and watch it left on the host's loop, makes its
/// [`MainThreadWaker`]s do nothing, and cuts every clone of the context
/// off, so that the [`EditorHost`] is never called again. A wrapper drops
/// it before anything its `EditorHost` points at goes away.
///
/// ```
/// use std::rc::Rc;
/// use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
/// use manguebyte_plugin::editor::{EditorContext, Gesture};
/// use manguebyte_params::ParamId;
///
/// let recorder = Rc::new(RecordingEditorHost::new());
/// let (context, link) = EditorContext::attach(recorder.clone());
/// let cutoff = ParamId::from_name("cutoff");
///
/// // An editor that leaks a gesture in the middle of a drag: its
/// // destructor does not run.
/// let mut drag = Gesture::begin(&context, cutoff, 0.5);
/// drag.set(0.75);
/// let leaked = std::mem::ManuallyDrop::new(drag);
///
/// // The host still sees the end.
/// drop(link);
/// assert_eq!(recorder.take_calls(), [
///     HostCall::BeginGesture(cutoff),
///     HostCall::Perform(cutoff, 0.75),
///     HostCall::EndGesture(cutoff),
/// ]);
///
/// // Dropped after all, much later, the gesture sends nothing more.
/// drop(std::mem::ManuallyDrop::into_inner(leaked));
/// assert!(recorder.take_calls().is_empty());
/// ```
#[must_use = "the editor is cut off from the host as soon as this is dropped"]
pub struct HostLink {
    shared: Rc<Shared>,
}

impl HostLink {
    /// Cuts the editor off. The same as dropping the link, written out.
    pub fn detach(self) {}
}

impl Drop for HostLink {
    fn drop(&mut self) {
        self.shared.detach();
    }
}

impl fmt::Debug for HostLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostLink").finish_non_exhaustive()
    }
}

/// A timer or a watch on the host's loop, registered through
/// [`EditorContext::every`] or [`EditorContext::watch_fd`]. Dropping it
/// cancels it.
///
/// Its callback is dropped before the drop returns, or, if the handle is
/// dropped from inside that very callback, as soon as the callback returns.
/// After the [`HostLink`] is dropped the handle does nothing: the link has
/// cancelled the registration already.
#[must_use = "the timer or watch is cancelled as soon as this is dropped"]
pub struct LoopHandle {
    shared: Weak<Shared>,
    id: u64,
}

impl LoopHandle {
    /// Cancels the timer or watch. The same as dropping the handle,
    /// written out.
    pub fn cancel(self) {}
}

impl Drop for LoopHandle {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let registration = {
            let mut registrations = shared.registrations.borrow_mut();
            registrations
                .iter()
                .position(|registration| registration.id == self.id)
                .map(|index| registrations.remove(index))
        };
        // Cancelled after the table is released, in case dropping the
        // callback registers or cancels something else.
        if let Some(registration) = registration {
            registration.host_loop.cancel(registration.token);
        }
    }
}

impl fmt::Debug for LoopHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LoopHandle")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Wakes an open editor on the host's main thread, from any other thread.
///
/// An editor that does work on a thread of its own, such as loading a
/// file or scanning presets, gives that thread a waker. The thread hands
/// its result over through a channel or an atomic and calls
/// [`wake`](Self::wake); the host then calls the wrapper on its main
/// thread, and the wrapper calls
/// [`OpenEditor::on_main_thread`](super::OpenEditor::on_main_thread), where
/// the editor picks the result up. Several wakes may lead to one call.
///
/// A waker is `Send + Sync` and cheap to clone. Once the editor's
/// [`HostLink`] is dropped it does nothing, so a thread that outlives the
/// editor cannot reach a host or plugin that has gone away. It is not for
/// the audio thread: [`wake`](Self::wake) takes a lock.
///
/// ```
/// use std::rc::Rc;
/// use std::sync::mpsc;
/// use manguebyte_plugin::editor::EditorContext;
/// use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
///
/// let recorder = Rc::new(RecordingEditorHost::new());
/// let (context, link) = EditorContext::attach(recorder.clone());
/// let waker = context.main_thread_waker().expect("this host calls back");
///
/// let (results, inbox) = mpsc::channel();
/// let worker = waker.clone();
/// std::thread::spawn(move || {
///     results.send("12 presets").expect("the editor is listening");
///     assert!(worker.wake());
/// })
/// .join()
/// .expect("the worker finished");
///
/// // The host was asked for a call on its main thread. When it comes, the
/// // wrapper calls the editor's `on_main_thread`, which reads the inbox.
/// assert_eq!(recorder.take_calls(), [HostCall::Wake]);
/// assert_eq!(inbox.try_recv(), Ok("12 presets"));
///
/// // Once the editor is closed, waking does nothing.
/// drop(link);
/// assert!(!waker.wake());
/// assert!(recorder.take_calls().is_empty());
/// ```
#[derive(Clone)]
pub struct MainThreadWaker {
    /// The wrapper's function, until the link is dropped.
    slot: Arc<Mutex<Option<WakeFn>>>,
}

/// What a wrapper gives for waking, as
/// [`EditorHost::main_thread_callback`] returns it.
type WakeFn = Box<dyn Fn() + Send + Sync>;

impl MainThreadWaker {
    fn new(wake: WakeFn) -> Self {
        Self {
            slot: Arc::new(Mutex::new(Some(wake))),
        }
    }

    /// Asks the host to call the editor on its main thread soon. Returns
    /// false, and does nothing, once the editor has been closed.
    ///
    /// The wrapper's function runs while a lock is held, so that closing
    /// the editor waits for a wake in progress and nothing runs after it.
    pub fn wake(&self) -> bool {
        let slot = self.slot.lock().unwrap_or_else(PoisonError::into_inner);
        match slot.as_ref() {
            Some(wake) => {
                wake();
                true
            }
            None => false,
        }
    }

    /// Lets go of the wrapper's function, after any wake in progress.
    fn detach(&self) {
        let wake = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        // Dropped after the lock is released.
        drop(wake);
    }
}

impl fmt::Debug for MainThreadWaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let attached = self
            .slot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some();
        f.debug_struct("MainThreadWaker")
            .field("attached", &attached)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_waker_can_be_sent_and_shared() {
        fn send_and_sync<T: Send + Sync + Clone + 'static>() {}
        send_and_sync::<MainThreadWaker>();
    }
}
