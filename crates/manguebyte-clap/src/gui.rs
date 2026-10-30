//! The CLAP `gui` extension, and the host's side of the editor contract.
//!
//! A plugin whose [`Plugin::editor`] returns a spec offers three extensions:
//! `gui`, and `timer-support` and `posix-fd-support` for the callbacks of the
//! host's loop. [`Gui`] keeps the spec and, while the host shows it, the open
//! editor. [`ClapEditorHost`] is what the editor reaches the host through:
//! its gestures become counts that the audio side turns into events (see
//! [`GestureCounters`]), its resize requests go to the host's `gui`
//! extension, and its timers and descriptors to the host's loop
//! ([`ClapLoop`]).
//!
//! The wrapper embeds nothing itself. It passes the window handle the host
//! gives to `set_parent` on to the editor, as a [`ParentWindow`], and what
//! the editor does with it is the editor's business.
//!
//! # Threads
//!
//! Everything here runs on the host's main thread. The exceptions are
//! [`SharedFlags`], which the audio thread reads, and the function
//! [`ClapEditorHost::main_thread_callback`] hands out, which other threads
//! call and which only calls the host's thread-safe `request_callback`.
//!
//! # Calls that come back
//!
//! Hosts answer some calls by calling the plugin before they return: a host
//! may call `set_size` from inside `request_resize`, `destroy` from inside
//! `closed`, or run its own loop, and with it `on_main_thread` or
//! `on_timer`. The editor contract forbids calling into an editor, or
//! dropping it, while the editor is calling out, so [`Busy`] notes every
//! call out in progress, and while one is:
//!
//! - `set_size` from inside `request_resize` is that request's answer;
//! - `get_size` is answered without the editor: with that answer, or the
//!   size being asked for, inside `request_resize`, and otherwise with the
//!   size last learned, if there is one;
//! - `adjust_size` is answered without the editor, from the spec's
//!   [`Resize`](manguebyte_plugin::editor::Resize) rules at the host's
//!   scale;
//! - `on_main_thread` is put off: when the call out returns, the plugin asks
//!   the host for another;
//! - timers and descriptors do not fire, and any other call that would
//!   reach the editor returns false;
//! - `destroy` is carried out as soon as nothing is in progress: at the
//!   start or the end of the next call from the host into the plugin, or,
//!   if the call came from the editor's own window, when the host calls the
//!   plugin's `on_main_thread`, which the plugin asks for.
//!
//! The same holds while the editor is being opened or closed, and while one
//! of its methods runs.
//!
//! A host that destroys the whole plugin from inside such a call breaks
//! CLAP's rules. The plugin then cuts the editor off at once, while the host
//! is still there to be called: its timers and descriptors are
//! unregistered, its gestures ended, its waker silenced, and nothing calls
//! the host again. The plugin itself is freed when the outermost call from
//! the host into it returns. If none is on the stack, because the editor
//! called the host from its own window, the plugin is freed at once except
//! for the editor, which is leaked: it cannot be dropped under its own
//! frame, and nothing of the plugin's runs after that frame returns.

use std::cell::{Cell, OnceCell, RefCell};
use std::ffi::{CStr, c_char};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering, fence};

use clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_COCOA, CLAP_WINDOW_API_WIN32, CLAP_WINDOW_API_X11,
    clap_gui_resize_hints, clap_host_gui, clap_plugin_gui, clap_window,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_VALUES, clap_host_params,
};
use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, clap_host_posix_fd_support,
    clap_plugin_posix_fd_support, clap_posix_fd_flags,
};
use clap_sys::ext::timer_support::{
    CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support, clap_plugin_timer_support,
};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use manguebyte_core::ParamId;
use manguebyte_params::ParamSet;
use manguebyte_plugin::Plugin;
use manguebyte_plugin::editor::{
    EditorContext, EditorHost, EditorSpec, GestureCounters, HostLink, HostLoop, HostSize,
    LoopToken, OpenEditor, ParentWindow, WindowApi, checked_scale,
};

use crate::wrapper::{free, guard, instance};

// --- The host --------------------------------------------------------------

/// The host that created a plugin.
///
/// CLAP keeps the `clap_host` valid until the plugin is destroyed. A
/// `HostRef` is used only until then: the wrapper drops everything that
/// holds one when the plugin is destroyed, and what it cannot drop yet (see
/// the module documentation) is made quiet first and never calls the host
/// again.
#[derive(Clone, Copy)]
pub(crate) struct HostRef(*const clap_host);

// SAFETY: the pointer is only used to call the host's functions. On threads
// other than the main thread a `HostRef` is used only to call
// `request_callback`, which CLAP marks thread-safe, and only while the
// plugin exists: the main-thread waker that carries one there is silenced
// when the editor's link is dropped, before the plugin is destroyed.
unsafe impl Send for HostRef {}

// SAFETY: as for `Send`; shared use calls only thread-safe host functions.
unsafe impl Sync for HostRef {}

impl HostRef {
    /// Wraps the host a plugin was created with.
    ///
    /// # Safety
    ///
    /// `host` must be null, or the host that created the plugin, which CLAP
    /// keeps valid until the plugin is destroyed; and no `HostRef` made
    /// from it may be used after that.
    pub(crate) unsafe fn new(host: *const clap_host) -> Self {
        Self(host)
    }

    /// The host as CLAP passes it to its own functions.
    pub(crate) fn as_ptr(self) -> *const clap_host {
        self.0
    }

    /// Whether the host can be asked for a call on its main thread.
    fn can_request_callback(self) -> bool {
        // SAFETY: valid or null, per `new`.
        unsafe { self.0.as_ref() }.is_some_and(|host| host.request_callback.is_some())
    }

    /// Asks the host to call the plugin's `on_main_thread` soon. Safe on any
    /// thread, the audio thread included.
    pub(crate) fn request_callback(self) {
        // SAFETY: valid or null, per `new`.
        let Some(request) = (unsafe { self.0.as_ref() }).and_then(|host| host.request_callback)
        else {
            return;
        };
        // SAFETY: the host's own function with its own pointer, which CLAP
        // marks thread-safe.
        unsafe { request(self.0) };
    }

    /// A copy of the host's extension `id`, or `None` if it offers none.
    ///
    /// # Safety
    ///
    /// `T` must be the type CLAP gives extension `id`, and this must be
    /// called on the main thread, after the plugin's `init` has begun.
    pub(crate) unsafe fn extension<T: Copy>(self, id: &CStr) -> Option<T> {
        // SAFETY: valid or null, per `new`.
        let get = unsafe { self.0.as_ref() }?.get_extension?;
        // SAFETY: the host's own function with its own pointer. A non-null
        // result points to the host's `T`, per this function's contract,
        // which lives as long as the host; it is copied out at once.
        unsafe { get(self.0, id.as_ptr()).cast::<T>().as_ref().copied() }
    }
}

/// The host extensions an editor uses, copied out once, in `init`.
#[derive(Clone, Copy)]
struct HostExtensions {
    params: Option<clap_host_params>,
    gui: Option<clap_host_gui>,
    timer: Option<clap_host_timer_support>,
    fd: Option<clap_host_posix_fd_support>,
}

impl HostExtensions {
    /// Asks `host` for each extension. Timers and descriptors count only if
    /// the host can both register and unregister them.
    fn query(host: HostRef) -> Self {
        // SAFETY: each extension is asked for with its own type, and this is
        // called from `init`, on the main thread.
        unsafe {
            Self {
                params: host.extension(CLAP_EXT_PARAMS),
                gui: host.extension(CLAP_EXT_GUI),
                timer: host
                    .extension::<clap_host_timer_support>(CLAP_EXT_TIMER_SUPPORT)
                    .filter(|timer| {
                        timer.register_timer.is_some() && timer.unregister_timer.is_some()
                    }),
                fd: host
                    .extension::<clap_host_posix_fd_support>(CLAP_EXT_POSIX_FD_SUPPORT)
                    .filter(|fd| fd.register_fd.is_some() && fd.unregister_fd.is_some()),
            }
        }
    }
}

/// What the main thread and the audio thread share, apart from the
/// parameters and the gesture counters.
pub(crate) struct SharedFlags {
    /// Cleared when the host was asked to read every value again (after a
    /// state load, or after the editor changed many values). The audio side
    /// then takes the values the host was asked to read, in `reread`, as
    /// the ones it knows.
    host_knows_values: AtomicBool,
    /// Each parameter's value, as the bits of an `f64` and in the order the
    /// host numbers them, when the host was last asked to read them again.
    reread: Box<[AtomicU64]>,
    /// State loads under way on the main thread. While one is, a value that
    /// differs from what the host knows may be one being restored, so none
    /// is reported.
    loading: AtomicU32,
    /// True between `start_processing` and `stop_processing`.
    pub(crate) processing: AtomicBool,
    /// Set by the editor's host after each edit, and cleared by `process` or
    /// `flush` before they report edits: while it is set, an edit may be
    /// waiting to be sent.
    pub(crate) unsent: AtomicBool,
    /// The parameters' [`labels_version`](ParamSet::labels_version) when
    /// the host last read their names, or was last asked to. Written on the
    /// main thread only; the audio side reads it to know whether a change
    /// it sees is news.
    labels_told: AtomicU64,
}

impl SharedFlags {
    /// The flags of a new plugin whose parameters are `params`. Allocates.
    pub(crate) fn new(params: &dyn ParamSet) -> Self {
        let mut reread = Vec::with_capacity(params.len());
        params.visit(&mut |param| reread.push(AtomicU64::new(param.plain().to_bits())));
        Self {
            host_knows_values: AtomicBool::new(true),
            reread: reread.into_boxed_slice(),
            loading: AtomicU32::new(0),
            processing: AtomicBool::new(false),
            unsent: AtomicBool::new(false),
            labels_told: AtomicU64::new(params.labels_version()),
        }
    }

    /// Main thread, before asking the host to read parameter information
    /// again: whether a parameter was given a new name, short name or unit
    /// since the host was last asked, and if so, notes that it is being
    /// asked now. However many were renamed, that is one answer of `true`.
    pub(crate) fn take_new_labels(&self, params: &dyn ParamSet) -> bool {
        let now = params.labels_version();
        self.labels_told.swap(now, Ordering::Relaxed) != now
    }

    /// Audio side: whether the host has yet to be asked about the names
    /// counted by `version`. Real-time safe.
    pub(crate) fn labels_untold(&self, version: u64) -> bool {
        self.labels_told.load(Ordering::Relaxed) != version
    }

    /// Main thread, right before the host is asked to read every value
    /// again: notes the values it will read, which it then knows, so that
    /// the audio side does not report them back to it as news.
    pub(crate) fn host_rereads(&self, params: &dyn ParamSet) {
        let mut slots = self.reread.iter();
        params.visit(&mut |param| {
            if let Some(slot) = slots.next() {
                slot.store(param.plain().to_bits(), Ordering::Relaxed);
            }
        });
        // Release: the audio side that sees the flag cleared sees the values.
        self.host_knows_values.store(false, Ordering::Release);
    }

    /// Main thread: notes a state load until the guard is dropped. Call
    /// [`host_rereads`](Self::host_rereads) before dropping it if the load
    /// changed anything.
    pub(crate) fn loading(&self) -> Loading<'_> {
        self.loading.fetch_add(1, Ordering::Relaxed);
        // Orders the count before the values the load stores; `settled`
        // pairs this with an acquire fence.
        fence(Ordering::Release);
        Loading(self)
    }

    /// Audio side, before reporting: if the host was asked to read every
    /// value again, takes what it was asked to read as what it knows.
    /// Real-time safe.
    pub(crate) fn take_reread(&self, known: &mut [f64]) {
        if !self.host_knows_values.swap(true, Ordering::AcqRel) {
            for (known, slot) in known.iter_mut().zip(self.reread.iter()) {
                *known = f64::from_bits(slot.load(Ordering::Relaxed));
            }
        }
    }

    /// Audio side, right after reading a parameter's value: whether a value
    /// read then may be reported to the host as news. It may not while a
    /// state load is under way, nor if the host has been asked to read the
    /// values again since [`take_reread`](Self::take_reread): it may be one
    /// the host is reading for itself, and the next report finds out.
    /// Real-time safe.
    pub(crate) fn settled(&self) -> bool {
        // Pairs with the release fence in `loading`: a value stored by a
        // load and read before this fence means the load is counted below,
        // or has ended and cleared `host_knows_values`.
        fence(Ordering::Acquire);
        self.loading.load(Ordering::Acquire) == 0 && self.host_knows_values.load(Ordering::Relaxed)
    }
}

/// A state load under way; see [`SharedFlags::loading`].
pub(crate) struct Loading<'a>(&'a SharedFlags);

impl Drop for Loading<'_> {
    fn drop(&mut self) {
        // Release: the audio side that sees the count fall sees the values
        // noted by `host_rereads` before it.
        self.0.loading.fetch_sub(1, Ordering::Release);
    }
}

// --- Calls in progress -----------------------------------------------------

/// What is in progress on the main thread, and what the wrapper knows
/// without asking the editor, shared by the [`Gui`], its [`ClapEditorHost`]
/// and its [`ClapLoop`].
#[derive(Default)]
struct Busy {
    /// Calls the editor made to the host that have not returned.
    calling_out: Cell<u32>,
    /// Calls from the host into the plugin's editor extensions, and into
    /// `on_main_thread`, that have not returned.
    entries: Cell<u32>,
    /// `request_resize` calls waiting for the host.
    resizing: Cell<u32>,
    /// The size the innermost `request_resize` asks for.
    requested: Cell<Option<HostSize>>,
    /// The size the host set from inside the innermost `request_resize`.
    resize_answer: Cell<Option<HostSize>>,
    /// How many resize requests the host has granted.
    granted: Cell<u64>,
    /// The editor's size as last learned: from the editor itself, a size it
    /// took, or a request the host granted. `None` when not known.
    size: Cell<Option<HostSize>>,
    /// The host asked for `destroy` when the editor could not be dropped.
    destroy_requested: Cell<bool>,
    /// The host called `on_main_thread` while the editor could not be
    /// reached; another call is asked for when the editor's call returns.
    callback_owed: Cell<bool>,
    /// The plugin is being destroyed: the host is not called again, except
    /// to unregister timers and descriptors while the editor is cut off.
    quiet: Cell<bool>,
    /// The host destroyed the plugin from inside one of the plugin's own
    /// calls; the outermost frees it as it returns.
    free_pending: Cell<bool>,
    /// The host, for asking for a call on its main thread. Set in `init`.
    host: Cell<Option<HostRef>>,
}

impl Busy {
    /// Notes a call out until the returned guard is dropped.
    fn calling_out(&self) -> CallingOut<'_> {
        self.calling_out
            .set(self.calling_out.get().saturating_add(1));
        CallingOut(self)
    }

    /// Asks the host for a call to `on_main_thread`, unless the plugin is
    /// being destroyed.
    fn request_callback(&self) {
        if self.quiet.get() {
            return;
        }
        if let Some(host) = self.host.get() {
            host.request_callback();
        }
    }
}

struct CallingOut<'a>(&'a Busy);

impl Drop for CallingOut<'_> {
    fn drop(&mut self) {
        let busy = self.0;
        // The host called `on_main_thread` while the editor was out: ask for
        // another call, now that it is coming back. Still counted as out, so
        // a host that called back from inside `request_callback` would find
        // the editor busy, not reenter it.
        if busy.calling_out.get() == 1 && busy.callback_owed.replace(false) {
            busy.request_callback();
        }
        busy.calling_out
            .set(busy.calling_out.get().saturating_sub(1));
    }
}

/// A call from the host into the plugin's editor extensions, until dropped.
struct Entered<'a>(&'a Busy);

impl<'a> Entered<'a> {
    fn new(busy: &'a Busy) -> Self {
        busy.entries.set(busy.entries.get().saturating_add(1));
        Self(busy)
    }
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        let entries = &self.0.entries;
        entries.set(entries.get().saturating_sub(1));
    }
}

/// A `request_resize` waiting for the host, with room for the size the host
/// sets before it returns.
struct Resizing<'a> {
    busy: &'a Busy,
    /// What a request this one is nested in asked for and was answered, put
    /// back afterwards.
    outer_requested: Option<HostSize>,
    outer_answer: Option<HostSize>,
}

impl<'a> Resizing<'a> {
    fn new(busy: &'a Busy, size: HostSize) -> Self {
        busy.resizing.set(busy.resizing.get().saturating_add(1));
        let outer_requested = busy.requested.replace(Some(size));
        let outer_answer = busy.resize_answer.take();
        Self {
            busy,
            outer_requested,
            outer_answer,
        }
    }

    fn answer(&self) -> Option<HostSize> {
        self.busy.resize_answer.get()
    }
}

impl Drop for Resizing<'_> {
    fn drop(&mut self) {
        let busy = self.busy;
        busy.resizing.set(busy.resizing.get().saturating_sub(1));
        busy.requested.set(self.outer_requested);
        busy.resize_answer.set(self.outer_answer);
    }
}

/// Sets a flag until dropped.
struct Raised<'a>(&'a Cell<bool>);

impl<'a> Raised<'a> {
    fn new(flag: &'a Cell<bool>) -> Self {
        flag.set(true);
        Self(flag)
    }
}

impl Drop for Raised<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

// --- The editor's host -----------------------------------------------------

/// What an editor reaches a CLAP host through.
///
/// A gesture's begin or end is counted in [`GestureCounters`], a value is
/// stored straight into the parameter, so the interface sees it at once,
/// and `process` or `flush` sends the host the begin, the value and the end
/// as events, in that order. When the host is not processing, each edit
/// asks it for a flush (`request_flush`), the only call to the host a
/// gesture makes.
pub(crate) struct ClapEditorHost {
    host: HostRef,
    extensions: HostExtensions,
    params: Arc<dyn ParamSet>,
    /// The id of each parameter, in the order the counters index them.
    ids: Box<[ParamId]>,
    counters: Arc<GestureCounters>,
    flags: Arc<SharedFlags>,
    host_loop: Option<Rc<ClapLoop>>,
    busy: Rc<Busy>,
}

impl ClapEditorHost {
    fn index(&self, param: ParamId) -> Option<usize> {
        self.ids.iter().position(|&id| id == param)
    }

    /// After an edit: tells the audio side there is something to send and,
    /// if the host is not processing, asks the host for a flush.
    fn edited(&self) {
        // Sequentially consistent, as is the load of `processing`, pairing
        // with `stop_processing`, which stores `processing` and then loads
        // this: at least one side sees the other, so an edit made as
        // processing stops is either flushed now or called back for.
        self.flags.unsent.store(true, Ordering::SeqCst);
        if !self.flags.processing.load(Ordering::SeqCst) {
            self.request_flush();
        }
    }

    /// Asks the host to call `process` or `flush` soon, unless the plugin is
    /// being destroyed.
    fn request_flush(&self) {
        if self.busy.quiet.get() {
            return;
        }
        let Some(request) = self
            .extensions
            .params
            .and_then(|params| params.request_flush)
        else {
            return;
        };
        let _out = self.busy.calling_out();
        // SAFETY: the host's own function with its own pointer, valid while
        // the plugin exists. CLAP allows it on any thread but the audio
        // thread; this is the main thread.
        unsafe { request(self.host.as_ptr()) };
    }

    /// If an edit may be waiting and the host is not processing, asks for a
    /// flush. For `on_main_thread`, which `stop_processing` asks for when it
    /// finds an edit waiting.
    fn flush_if_waiting(&self) {
        if self.flags.unsent.load(Ordering::SeqCst) && !self.flags.processing.load(Ordering::SeqCst)
        {
            self.request_flush();
        }
    }
}

impl EditorHost for ClapEditorHost {
    fn begin_gesture(&self, param: ParamId) {
        let _out = self.busy.calling_out();
        if let Some(index) = self.index(param) {
            // Counted before any value of the gesture is stored, so that the
            // audio side never sends such a value outside the gesture.
            self.counters.begin(index);
            self.edited();
        }
    }

    fn perform(&self, param: ParamId, normalized: f64) {
        let _out = self.busy.calling_out();
        let Some(target) = self
            .index(param)
            .and_then(|_| self.params.find_param(param))
        else {
            return;
        };
        target.set_normalized(normalized);
        self.edited();
    }

    fn end_gesture(&self, param: ParamId) {
        let _out = self.busy.calling_out();
        if let Some(index) = self.index(param) {
            self.counters.end(index);
            self.edited();
        }
    }

    fn values_changed(&self) {
        if self.busy.quiet.get() {
            return;
        }
        let _out = self.busy.calling_out();
        match self.extensions.params.and_then(|params| params.rescan) {
            Some(rescan) => {
                // The host reads every value again, so it knows them as they
                // are now; the audio side must not report them back as news.
                self.flags.host_rereads(&*self.params);
                // An editor that loaded something of its own, such as a
                // whole new instrument, may have renamed parameters too:
                // the host reads their names in the same rescan.
                let mut flags = CLAP_PARAM_RESCAN_VALUES;
                if self.flags.take_new_labels(&*self.params) {
                    flags |= CLAP_PARAM_RESCAN_INFO;
                }
                // SAFETY: the host's own function with its own pointer, on
                // the main thread, where CLAP wants it.
                unsafe { rescan(self.host.as_ptr(), flags) };
            }
            // A host that cannot read them again is sent them as values.
            None => self.edited(),
        }
    }

    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        if self.busy.quiet.get() {
            return None;
        }
        let request = self.extensions.gui.and_then(|gui| gui.request_resize)?;
        let _out = self.busy.calling_out();
        let resizing = Resizing::new(&self.busy, size);
        // SAFETY: the host's own function with its own pointer, on the main
        // thread.
        let granted = unsafe { request(self.host.as_ptr(), size.width, size.height) };
        // A size the host set before returning is what the window has,
        // whatever it returned.
        let answer = resizing.answer().or(granted.then_some(size));
        drop(resizing);
        if answer.is_some() {
            self.busy.size.set(answer);
            self.busy
                .granted
                .set(self.busy.granted.get().wrapping_add(1));
        }
        answer
    }

    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        let host = self.host;
        host.can_request_callback()
            .then(|| Box::new(move || host.request_callback()) as Box<dyn Fn() + Send + Sync>)
    }

    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        self.host_loop
            .clone()
            .map(|host_loop| host_loop as Rc<dyn HostLoop>)
    }

    fn closed(&self) {
        if self.busy.quiet.get() {
            return;
        }
        let _out = self.busy.calling_out();
        if let Some(closed) = self.extensions.gui.and_then(|gui| gui.closed) {
            // SAFETY: the host's own function with its own pointer; CLAP
            // marks it thread-safe. With `was_destroyed` true the host must
            // answer with `destroy`, which may come from inside this call
            // and then waits until it has returned.
            unsafe { closed(self.host.as_ptr(), true) };
        }
    }
}

// --- The host's loop ---------------------------------------------------------

/// Timers and descriptors on the host's own loop, through CLAP's
/// `timer-support` and `posix-fd-support`.
///
/// The host calls the plugin's `on_timer` and `on_fd`, and the wrapper finds
/// the callbacks here. A callback runs out of the table, so it may register
/// and cancel, itself included; one cancelled while it runs is dropped as
/// soon as it returns. Several watches on one descriptor share one
/// registration with the host.
pub(crate) struct ClapLoop {
    host: HostRef,
    timer: Option<clap_host_timer_support>,
    fd: Option<clap_host_posix_fd_support>,
    busy: Rc<Busy>,
    next_token: Cell<u64>,
    entries: RefCell<Vec<LoopEntry>>,
}

struct LoopEntry {
    token: LoopToken,
    source: Source,
    /// `None` while the callback runs.
    callback: Option<Box<dyn FnMut()>>,
}

/// What a callback waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Timer(clap_id),
    Fd(i32),
}

impl ClapLoop {
    fn add(&self, source: Source, callback: Box<dyn FnMut()>) -> LoopToken {
        let token = LoopToken::new(self.next_token.get());
        self.next_token.set(token.get().wrapping_add(1));
        self.entries.borrow_mut().push(LoopEntry {
            token,
            source,
            callback: Some(callback),
        });
        token
    }

    fn watched(&self, source: Source) -> bool {
        self.entries
            .borrow()
            .iter()
            .any(|entry| entry.source == source)
    }

    /// Runs every callback waiting for `source`, once.
    fn fire(&self, source: Source) {
        let due: Vec<LoopToken> = self
            .entries
            .borrow()
            .iter()
            .filter(|entry| entry.source == source)
            .map(|entry| entry.token)
            .collect();
        for token in due {
            let callback = self
                .entries
                .borrow_mut()
                .iter_mut()
                .find(|entry| entry.token == token)
                .and_then(|entry| entry.callback.take());
            let Some(callback) = callback else {
                // Cancelled by an earlier callback, or running already.
                continue;
            };
            let mut running = Running {
                owner: self,
                token,
                callback: Some(callback),
            };
            if let Some(callback) = running.callback.as_mut() {
                callback();
            }
        }
    }
}

/// A callback taken out of the table to run. Dropping it puts it back, or
/// drops it if it was cancelled meanwhile, even while unwinding.
struct Running<'a> {
    owner: &'a ClapLoop,
    token: LoopToken,
    callback: Option<Box<dyn FnMut()>>,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let callback = self.callback.take();
        let cancelled = match self.owner.entries.try_borrow_mut() {
            Ok(mut entries) => match entries.iter_mut().find(|entry| entry.token == self.token) {
                Some(entry) => {
                    entry.callback = callback;
                    None
                }
                None => callback,
            },
            Err(_) => callback,
        };
        // Dropped after the table is released, in case dropping it calls
        // the loop.
        drop(cancelled);
    }
}

impl HostLoop for ClapLoop {
    fn watch_fd(&self, fd: i32, ready: Box<dyn FnMut()>) -> Option<LoopToken> {
        let support = self.fd.filter(|_| !self.busy.quiet.get())?;
        if !self.watched(Source::Fd(fd)) {
            let register = support.register_fd?;
            let _out = self.busy.calling_out();
            // SAFETY: the host's own function with its own pointer, on the
            // main thread.
            if !unsafe { register(self.host.as_ptr(), fd, CLAP_POSIX_FD_READ) } {
                return None;
            }
        }
        Some(self.add(Source::Fd(fd), ready))
    }

    fn every(&self, period_ms: u32, tick: Box<dyn FnMut()>) -> Option<LoopToken> {
        let register = self
            .timer
            .filter(|_| !self.busy.quiet.get())?
            .register_timer?;
        let mut id = CLAP_INVALID_ID;
        let registered = {
            let _out = self.busy.calling_out();
            // SAFETY: the host's own function with its own pointer, on the
            // main thread, and a place for the id that lives for the call.
            unsafe { register(self.host.as_ptr(), period_ms, &raw mut id) }
        };
        (registered && id != CLAP_INVALID_ID).then(|| self.add(Source::Timer(id), tick))
    }

    fn cancel(&self, token: LoopToken) {
        let removed = {
            let mut entries = self.entries.borrow_mut();
            entries
                .iter()
                .position(|entry| entry.token == token)
                .map(|index| entries.remove(index))
        };
        let Some(removed) = removed else {
            return;
        };
        // The host registration goes with the last callback that uses it.
        // This is the one call made while the plugin is being destroyed:
        // the host must not be left calling a plugin that is gone.
        if !self.watched(removed.source) {
            let _out = self.busy.calling_out();
            match removed.source {
                Source::Timer(id) => {
                    if let Some(unregister) = self.timer.and_then(|timer| timer.unregister_timer) {
                        // SAFETY: the host's own function with its own
                        // pointer, on the main thread.
                        unsafe { unregister(self.host.as_ptr(), id) };
                    }
                }
                Source::Fd(fd) => {
                    if let Some(unregister) = self.fd.and_then(|fd| fd.unregister_fd) {
                        // SAFETY: as above.
                        unsafe { unregister(self.host.as_ptr(), fd) };
                    }
                }
            }
        }
        // The callback is dropped last, outside the table, unless it is
        // running, in which case it is dropped as soon as it returns.
        drop(removed);
    }
}

// --- The editor --------------------------------------------------------------

/// The CLAP name of a window API.
fn api_name(api: WindowApi) -> Option<&'static CStr> {
    match api {
        WindowApi::Win32 => Some(CLAP_WINDOW_API_WIN32),
        WindowApi::Cocoa => Some(CLAP_WINDOW_API_COCOA),
        WindowApi::X11 => Some(CLAP_WINDOW_API_X11),
        _ => None,
    }
}

/// The window API a CLAP name stands for. Wayland is not one: CLAP does not
/// embed into Wayland windows.
fn api_from_name(name: &CStr) -> Option<WindowApi> {
    [WindowApi::Win32, WindowApi::Cocoa, WindowApi::X11]
        .into_iter()
        .find(|&api| api_name(api) == Some(name))
}

/// The window API of this platform.
fn platform_api() -> WindowApi {
    if cfg!(target_os = "windows") {
        WindowApi::Win32
    } else if cfg!(target_os = "macos") {
        WindowApi::Cocoa
    } else {
        WindowApi::X11
    }
}

/// A window id as `ParentWindow::x11` takes it. `c_ulong` is 32 bits wide
/// on some platforms and 64 on others.
fn x11_id(window: impl Into<u64>) -> u64 {
    window.into()
}

/// The parent window a host passes to `set_parent`.
///
/// # Safety
///
/// `window` must be null or point to a `clap_window` whose `api` is null or
/// a NUL-terminated string, and whose handle is the one that names.
unsafe fn parent_window(window: *const clap_window) -> Option<ParentWindow> {
    // SAFETY: per this function's contract.
    let window = unsafe { window.as_ref() }?;
    if window.api.is_null() {
        return None;
    }
    // SAFETY: per this function's contract, a NUL-terminated string.
    let api = api_from_name(unsafe { CStr::from_ptr(window.api) })?;
    // SAFETY: the field read is the one the API names, which is the one the
    // host wrote.
    unsafe {
        match api {
            WindowApi::Win32 => ParentWindow::win32(window.specific.win32.addr()),
            WindowApi::Cocoa => ParentWindow::cocoa(window.specific.cocoa.addr()),
            WindowApi::X11 => ParentWindow::x11(x11_id(window.specific.x11)),
            _ => None,
        }
    }
}

/// A plugin's editor as the host sees it through `gui`.
///
/// Main thread only. The editor is embedded, never floating. Every call
/// made out of order, with arguments that make no sense, or while the
/// editor cannot be reached (see the module documentation) returns false
/// and changes nothing, unless the module documentation says how it is
/// answered without the editor; `destroy` is always safe.
pub(crate) struct Gui {
    spec: Box<dyn EditorSpec>,
    counters: Arc<GestureCounters>,
    /// The editor's host, made in `init`.
    host: OnceCell<Rc<ClapEditorHost>>,
    busy: Rc<Busy>,
    /// The API passed to `create`, until `destroy`.
    created: Cell<Option<WindowApi>>,
    /// The host's scale, if it gave one that was accepted.
    scale: Cell<Option<f64>>,
    /// A size the host set before the editor was open.
    preset: Cell<Option<HostSize>>,
    opening: Cell<bool>,
    closing: Cell<bool>,
    /// The open editor. Borrowed only while one of its methods runs.
    editor: RefCell<Option<Box<dyn OpenEditor>>>,
    /// The wrapper's end of the open editor's context.
    link: Cell<Option<HostLink>>,
}

impl Gui {
    /// The editor state of a new plugin instance, before `init`.
    pub(crate) fn new(spec: Box<dyn EditorSpec>, counters: Arc<GestureCounters>) -> Self {
        Self {
            spec,
            counters,
            host: OnceCell::new(),
            busy: Rc::default(),
            created: Cell::new(None),
            scale: Cell::new(None),
            preset: Cell::new(None),
            opening: Cell::new(false),
            closing: Cell::new(false),
            editor: RefCell::new(None),
            link: Cell::new(None),
        }
    }

    /// Looks up the host's extensions and makes the editor's host. Called
    /// from the plugin's `init`; later calls change nothing.
    pub(crate) fn init(
        &self,
        host: HostRef,
        params: Arc<dyn ParamSet>,
        ids: &[ParamId],
        flags: Arc<SharedFlags>,
    ) {
        if self.host.get().is_some() {
            return;
        }
        let extensions = HostExtensions::query(host);
        let host_loop = (extensions.timer.is_some() || extensions.fd.is_some()).then(|| {
            Rc::new(ClapLoop {
                host,
                timer: extensions.timer,
                fd: extensions.fd,
                busy: Rc::clone(&self.busy),
                next_token: Cell::new(1),
                entries: RefCell::new(Vec::new()),
            })
        });
        let editor_host = Rc::new(ClapEditorHost {
            host,
            extensions,
            params,
            ids: ids.into(),
            counters: Arc::clone(&self.counters),
            flags,
            host_loop,
            busy: Rc::clone(&self.busy),
        });
        self.busy.host.set(Some(host));
        // Cannot fail: checked empty above, and nothing in between sets it.
        let _ = self.host.set(editor_host);
    }

    /// Whether the editor cannot be called, opened, closed or dropped now.
    fn is_busy(&self) -> bool {
        self.busy.calling_out.get() > 0
            || self.opening.get()
            || self.closing.get()
            || self.editor.try_borrow_mut().is_err()
    }

    /// Runs `work` on the open editor, if there is one and it can be called
    /// now.
    fn with_editor<R>(&self, work: impl FnOnce(&mut dyn OpenEditor) -> R) -> Option<R> {
        if self.busy.calling_out.get() > 0 || self.closing.get() {
            return None;
        }
        let mut slot = self.editor.try_borrow_mut().ok()?;
        let editor = slot.as_deref_mut()?;
        Some(work(editor))
    }

    fn is_open(&self) -> bool {
        self.editor
            .try_borrow()
            .map_or(true, |editor| editor.is_some())
    }

    /// The scale sizes are worked out at.
    fn scale_or_one(&self) -> f64 {
        self.scale.get().unwrap_or(1.0)
    }

    /// The size before the editor is open.
    fn size_before_open(&self) -> HostSize {
        self.preset
            .get()
            .unwrap_or_else(|| self.spec.size(self.scale_or_one()))
    }

    /// The editor's size while it cannot be asked: inside its own resize
    /// request, the size the host set or else the one asked for; otherwise
    /// the size last learned, if any.
    fn size_unasked(&self) -> Option<HostSize> {
        let busy = &self.busy;
        if busy.resizing.get() > 0 {
            if let Some(size) = busy.resize_answer.get().or(busy.requested.get()) {
                return Some(size);
            }
        }
        busy.size.get()
    }

    /// Carries out a `destroy` that had to wait, once nothing is in
    /// progress. Every entry point calls this when it starts and before it
    /// returns to the host.
    pub(crate) fn settle(&self) {
        if self.busy.destroy_requested.get() && !self.is_busy() {
            self.close();
        }
    }

    /// Drops the editor, then its link, and forgets what the host said.
    fn close(&self) {
        if self.closing.get() {
            return;
        }
        let _closing = Raised::new(&self.closing);
        // Whatever destroy was waiting is this one.
        self.busy.destroy_requested.set(false);
        self.created.set(None);
        self.scale.set(None);
        self.preset.set(None);
        self.busy.size.set(None);
        // Taken out first, so that nothing is borrowed while they drop. The
        // link is dropped after the editor, even if dropping the editor
        // panics: it ends whatever gestures, timers and watches the editor
        // left behind.
        let link = self.link.take();
        let editor = self
            .editor
            .try_borrow_mut()
            .ok()
            .and_then(|mut editor| editor.take());
        drop(editor);
        drop(link);
    }

    /// Closes the editor for a plugin that is going away, or, if the editor
    /// is still calling out from its own window, cuts it off and leaks it:
    /// it cannot be dropped under its own frame, and nothing of the
    /// plugin's runs again after that frame returns.
    fn close_for_good(&self) {
        if self.busy.calling_out.get() == 0 {
            self.close();
            return;
        }
        self.created.set(None);
        drop(self.link.take());
        if let Ok(mut slot) = self.editor.try_borrow_mut() {
            std::mem::forget(slot.take());
        }
    }

    /// Closes everything for good, for `plugin.destroy`. Returns whether
    /// the plugin may be freed now; if not, the outermost call into it
    /// frees it as it returns (see [`finish_destroy`](Self::finish_destroy)).
    pub(crate) fn shutdown(&self) -> bool {
        if self.busy.quiet.replace(true) {
            // Destroyed again while being destroyed: the first destroy is
            // still on the stack and frees the plugin.
            return false;
        }
        if self.busy.entries.get() == 0 {
            self.close_for_good();
            return true;
        }
        // From inside one of the plugin's own calls, which CLAP does not
        // allow. The host may be gone once this returns, so the editor is
        // cut off now: its link ends its gestures, unregisters its timers
        // and descriptors and silences its waker. The editor itself, which
        // may be on the stack, goes with the plugin.
        self.created.set(None);
        self.busy.destroy_requested.set(false);
        drop(self.link.take());
        self.busy.free_pending.set(true);
        false
    }

    /// At the end of a call from the host: whether the host destroyed the
    /// plugin from inside it and this was the outermost call, in which case
    /// the editor is closed and the plugin must be freed now.
    pub(crate) fn finish_destroy(&self) -> bool {
        if !self.busy.free_pending.get() || self.busy.entries.get() > 0 {
            return false;
        }
        self.busy.free_pending.set(false);
        self.close_for_good();
        true
    }

    pub(crate) fn is_api_supported(&self, api: Option<WindowApi>, is_floating: bool) -> bool {
        !is_floating && api.is_some_and(|api| self.spec.supports(api))
    }

    pub(crate) fn preferred_api(
        &self,
        api: Option<&mut *const c_char>,
        is_floating: Option<&mut bool>,
    ) -> bool {
        let (Some(api), Some(is_floating)) = (api, is_floating) else {
            return false;
        };
        let platform = platform_api();
        let Some(name) = api_name(platform).filter(|_| self.spec.supports(platform)) else {
            return false;
        };
        *api = name.as_ptr();
        *is_floating = false;
        true
    }

    pub(crate) fn create(&self, api: Option<WindowApi>, is_floating: bool) -> bool {
        let Some(api) = api else {
            return false;
        };
        if is_floating
            || !self.spec.supports(api)
            || self.host.get().is_none()
            || self.busy.quiet.get()
            || self.created.get().is_some()
            || self.busy.destroy_requested.get()
            || self.is_busy()
            || self.is_open()
        {
            return false;
        }
        self.created.set(Some(api));
        self.scale.set(None);
        self.preset.set(None);
        true
    }

    pub(crate) fn destroy(&self) {
        if self.closing.get() {
            // Already on its way out.
            return;
        }
        if self.is_busy() {
            // Not now: from inside a call out of the editor, or while it is
            // opened, closed or called. The host considers it gone.
            self.created.set(None);
            if !self.busy.destroy_requested.replace(true) {
                // In case nothing of ours is on the stack to finish it.
                self.busy.request_callback();
            }
            return;
        }
        self.close();
    }

    pub(crate) fn set_scale(&self, scale: f64) -> bool {
        // Cocoa sizes are in points: the system scales them, and CLAP says
        // not to use this there.
        let (Some(api), Some(scale)) = (self.created.get(), checked_scale(scale)) else {
            return false;
        };
        if api == WindowApi::Cocoa || self.is_busy() {
            return false;
        }
        if !self.is_open() {
            self.scale.set(Some(scale));
            // A size set at another scale no longer applies.
            self.preset.set(None);
            return true;
        }
        let granted = self.busy.granted.get();
        let taken = self
            .with_editor(|editor| editor.set_scale(scale))
            .unwrap_or(false);
        if taken {
            self.scale.set(Some(scale));
            if self.busy.granted.get() == granted {
                // The size may have changed with the scale, without a
                // request the host granted.
                self.busy.size.set(None);
            }
        }
        taken
    }

    pub(crate) fn size(&self) -> Option<HostSize> {
        self.created.get()?;
        if !self.is_open() && !self.is_busy() {
            return Some(self.size_before_open());
        }
        match self.with_editor(|editor| editor.size()) {
            Some(size) => {
                self.busy.size.set(Some(size));
                Some(size)
            }
            None => self.size_unasked(),
        }
    }

    pub(crate) fn can_resize(&self) -> bool {
        self.created.get().is_some() && self.spec.resize().scaled(self.scale_or_one()).can_resize()
    }

    pub(crate) fn resize_hints(&self, hints: Option<&mut clap_gui_resize_hints>) -> bool {
        let Some(hints) = hints.filter(|_| self.created.get().is_some()) else {
            return false;
        };
        let rules = self.spec.resize().scaled(self.scale_or_one()).hints();
        let (ratio_width, ratio_height) = rules.ratio.unwrap_or((0, 0));
        *hints = clap_gui_resize_hints {
            can_resize_horizontally: rules.horizontal,
            can_resize_vertically: rules.vertical,
            preserve_aspect_ratio: rules.ratio.is_some(),
            aspect_ratio_width: ratio_width,
            aspect_ratio_height: ratio_height,
        };
        true
    }

    pub(crate) fn adjust_size(&self, wanted: HostSize) -> Option<HostSize> {
        if !self.can_resize() {
            return None;
        }
        let rules = self.spec.resize().scaled(self.scale_or_one());
        if !self.is_open() && !self.is_busy() {
            return Some(rules.fit(self.size_before_open(), wanted));
        }
        if let Some(size) = self.with_editor(|editor| editor.fit(wanted)) {
            return Some(size);
        }
        // The editor cannot be asked now, so the spec's rules answer. They
        // use the current size only for a fixed editor, which cannot get
        // here, so a size that is not known does not matter.
        Some(rules.fit(self.size_unasked().unwrap_or(wanted), wanted))
    }

    pub(crate) fn set_size(&self, size: HostSize) -> bool {
        if self.created.get().is_none() {
            return false;
        }
        if self.busy.resizing.get() > 0 {
            // The host answering a resize request: the editor takes this
            // size as the request's result.
            self.busy.resize_answer.set(Some(size));
            return true;
        }
        if self.is_open() {
            let taken = self
                .with_editor(|editor| editor.set_size(size))
                .unwrap_or(false);
            if taken {
                self.busy.size.set(Some(size));
            }
            return taken;
        }
        if self.is_busy() {
            return false;
        }
        // No window yet: kept for when there is one, if it is a size the
        // editor accepts.
        let rules = self.spec.resize().scaled(self.scale_or_one());
        if rules.fit(self.size_before_open(), size) != size {
            return false;
        }
        self.preset.set(Some(size));
        true
    }

    pub(crate) fn set_parent(&self, parent: Option<ParentWindow>) -> bool {
        let (Some(api), Some(parent), Some(host)) = (self.created.get(), parent, self.host.get())
        else {
            return false;
        };
        if parent.api() != api || self.is_busy() || self.is_open() {
            return false;
        }
        let host = Rc::clone(host);
        let (context, link) = EditorContext::attach(Rc::clone(&host) as Rc<dyn EditorHost>);
        // Kept from now on, where `shutdown` can cut the editor off even
        // while it opens.
        self.link.set(Some(link));
        let granted = self.busy.granted.get();
        // The editor opens at the size the spec gives for its scale.
        let opening_size = self.spec.size(self.scale_or_one());
        self.busy.size.set(Some(opening_size));
        let opened = {
            let _opening = Raised::new(&self.opening);
            self.spec.open(parent, self.scale.get(), context)
        };
        let editor = match opened {
            // The host destroyed the plugin while the editor opened; the
            // editor is out of `open`, so it can be dropped.
            Ok(editor) if self.busy.quiet.get() => {
                drop(editor);
                drop(self.link.take());
                return false;
            }
            Ok(editor) => editor,
            Err(_) => {
                // Ends whatever the editor registered before it gave up.
                drop(self.link.take());
                self.busy.size.set(None);
                return false;
            }
        };
        *self.editor.borrow_mut() = Some(editor);
        // A size the host set before there was a window, unless the host
        // granted the editor another while it opened: the host's latest
        // answer is the size of its window.
        let preset = self.preset.take();
        if let Some(preset) = preset.filter(|_| self.busy.granted.get() == granted) {
            self.apply_preset(preset, &host);
        }
        true
    }

    /// Gives a just-opened editor the size the host set before its window
    /// existed. If the editor refuses it, the host is asked to make its
    /// window the editor's size instead, so that the two agree.
    fn apply_preset(&self, preset: HostSize, host: &ClapEditorHost) {
        let outcome = self.with_editor(|editor| {
            let size = editor.size();
            if size == preset || editor.set_size(preset) {
                Ok(preset)
            } else {
                Err(size)
            }
        });
        match outcome {
            Some(Ok(size)) => self.busy.size.set(Some(size)),
            Some(Err(size)) => {
                self.busy.size.set(Some(size));
                let answer = host
                    .request_resize(size)
                    .filter(|_| !self.busy.destroy_requested.get());
                if let Some(other) = answer.filter(|&answer| answer != size) {
                    // The host made its window yet another size: offered to
                    // the editor once.
                    if self.with_editor(|editor| editor.set_size(other)) == Some(true) {
                        self.busy.size.set(Some(other));
                    }
                }
            }
            None => {}
        }
    }

    pub(crate) fn set_visible(&self, visible: bool) -> bool {
        self.created.get().is_some()
            && self
                .with_editor(|editor| editor.set_visible(visible))
                .is_some()
    }

    pub(crate) fn on_timer(&self, timer_id: clap_id) {
        if let Some(host_loop) = self.host_loop().filter(|_| !self.is_busy()) {
            host_loop.fire(Source::Timer(timer_id));
        }
    }

    pub(crate) fn on_fd(&self, fd: i32, flags: clap_posix_fd_flags) {
        if flags & (CLAP_POSIX_FD_READ | CLAP_POSIX_FD_ERROR) == 0 {
            return;
        }
        if let Some(host_loop) = self.host_loop().filter(|_| !self.is_busy()) {
            host_loop.fire(Source::Fd(fd));
        }
    }

    pub(crate) fn on_main_thread(&self) {
        let Some(host) = self.host.get() else {
            return;
        };
        if self.is_busy() {
            // From a loop the host runs while the editor is calling it.
            // Neither the flush nor the editor's wake-up can be done now;
            // the host is asked for another call when the editor is back.
            self.busy.callback_owed.set(true);
            return;
        }
        host.flush_if_waiting();
        self.with_editor(|editor| editor.on_main_thread());
    }

    fn host_loop(&self) -> Option<Rc<ClapLoop>> {
        self.host.get()?.host_loop.clone()
    }
}

// --- The C interface -------------------------------------------------------

/// The extension tables of a plugin with an editor.
pub(crate) struct GuiVtables<P>(PhantomData<P>);

impl<P: Plugin> GuiVtables<P> {
    pub(crate) const GUI: &'static clap_plugin_gui = &clap_plugin_gui {
        is_api_supported: Some(gui_is_api_supported::<P>),
        get_preferred_api: Some(gui_get_preferred_api::<P>),
        create: Some(gui_create::<P>),
        destroy: Some(gui_destroy::<P>),
        set_scale: Some(gui_set_scale::<P>),
        get_size: Some(gui_get_size::<P>),
        can_resize: Some(gui_can_resize::<P>),
        get_resize_hints: Some(gui_get_resize_hints::<P>),
        adjust_size: Some(gui_adjust_size::<P>),
        set_size: Some(gui_set_size::<P>),
        set_parent: Some(gui_set_parent::<P>),
        set_transient: Some(gui_set_transient),
        suggest_title: Some(gui_suggest_title),
        show: Some(gui_show::<P>),
        hide: Some(gui_hide::<P>),
    };
    pub(crate) const TIMER: &'static clap_plugin_timer_support = &clap_plugin_timer_support {
        on_timer: Some(timer_on_timer::<P>),
    };
    pub(crate) const FD: &'static clap_plugin_posix_fd_support = &clap_plugin_posix_fd_support {
        on_fd: Some(fd_on_fd::<P>),
    };
}

/// Runs `work` on the editor state of the plugin behind `plugin`, with
/// anything that had to wait done before and after it. `fallback` is the
/// answer when there is no such state or `work` panics.
///
/// If the host destroyed the plugin from inside this call and this is the
/// outermost call into the plugin, the plugin is freed before this returns.
///
/// # Safety
///
/// `plugin` must be null or a live plugin made by the factory for `P`, and
/// this must be called on the main thread.
unsafe fn entry<P: Plugin, R: Copy>(
    plugin: *const clap_plugin,
    fallback: R,
    work: impl FnOnce(&Gui) -> R,
) -> R {
    // SAFETY: per this function's contract.
    let Some(gui) = (unsafe { instance::<P>(plugin) }).and_then(|i| i.gui.as_ref()) else {
        return fallback;
    };
    let result = guard(fallback, || {
        let _entered = Entered::new(&gui.busy);
        // A destroy that had to wait is done first, so that nothing the
        // host does next meets an editor it has destroyed: no timer of its
        // fires, and a new one can be created at once.
        gui.settle();
        let result = work(gui);
        gui.settle();
        result
    });
    if gui.finish_destroy() {
        // SAFETY: the host destroyed this plugin from inside this call, and
        // this was the outermost call into it; nothing refers to the plugin
        // any more, here or up the stack, and `gui` is not used again.
        guard((), || unsafe { free::<P>(plugin) });
    }
    result
}

/// The window API a host names, or `None`.
///
/// # Safety
///
/// `name` must be null or a NUL-terminated string.
unsafe fn named_api(name: *const c_char) -> Option<WindowApi> {
    if name.is_null() {
        return None;
    }
    // SAFETY: per this function's contract.
    api_from_name(unsafe { CStr::from_ptr(name) })
}

unsafe extern "C" fn gui_is_api_supported<P: Plugin>(
    plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    // SAFETY: the host passes a plugin it created through our factory and a
    // string or null, on the main thread.
    unsafe {
        entry::<P, _>(plugin, false, |gui| {
            gui.is_api_supported(named_api(api), is_floating)
        })
    }
}

unsafe extern "C" fn gui_get_preferred_api<P: Plugin>(
    plugin: *const clap_plugin,
    api: *mut *const c_char,
    is_floating: *mut bool,
) -> bool {
    // SAFETY: as above, and places to write to that the host owns, or null.
    // They have different types, so they cannot be one place.
    unsafe {
        entry::<P, _>(plugin, false, |gui| {
            gui.preferred_api(api.as_mut(), is_floating.as_mut())
        })
    }
}

unsafe extern "C" fn gui_create<P: Plugin>(
    plugin: *const clap_plugin,
    api: *const c_char,
    is_floating: bool,
) -> bool {
    // SAFETY: as for `gui_is_api_supported`.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.create(named_api(api), is_floating)) }
}

unsafe extern "C" fn gui_destroy<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: the host passes a plugin it created through our factory, on
    // the main thread.
    unsafe { entry::<P, _>(plugin, (), Gui::destroy) }
}

unsafe extern "C" fn gui_set_scale<P: Plugin>(plugin: *const clap_plugin, scale: f64) -> bool {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.set_scale(scale)) }
}

/// Writes `size` to the host's two places.
///
/// They are written through the pointers, one after the other, and never
/// turned into references: C lets a host pass the same place for both,
/// which then ends up holding the height.
///
/// # Safety
///
/// `width` and `height` must be valid for writes, possibly the same place.
unsafe fn write_size(width: *mut u32, height: *mut u32, size: HostSize) {
    // SAFETY: per this function's contract.
    unsafe {
        width.write(size.width);
        height.write(size.height);
    }
}

unsafe extern "C" fn gui_get_size<P: Plugin>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    // SAFETY: as for `gui_destroy`, and two places to write to that the
    // host owns, checked non-null above, which may be one place.
    unsafe {
        entry::<P, _>(plugin, false, |gui| {
            let Some(size) = gui.size() else {
                return false;
            };
            write_size(width, height, size);
            true
        })
    }
}

unsafe extern "C" fn gui_can_resize<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, false, Gui::can_resize) }
}

unsafe extern "C" fn gui_get_resize_hints<P: Plugin>(
    plugin: *const clap_plugin,
    hints: *mut clap_gui_resize_hints,
) -> bool {
    // SAFETY: as for `gui_destroy`, and a struct the host owns, or null.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.resize_hints(hints.as_mut())) }
}

unsafe extern "C" fn gui_adjust_size<P: Plugin>(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    if width.is_null() || height.is_null() {
        return false;
    }
    // SAFETY: as for `gui_get_size`; both places are read before either is
    // written.
    unsafe {
        entry::<P, _>(plugin, false, |gui| {
            let wanted = HostSize::new(width.read(), height.read());
            let Some(size) = gui.adjust_size(wanted) else {
                return false;
            };
            write_size(width, height, size);
            true
        })
    }
}

unsafe extern "C" fn gui_set_size<P: Plugin>(
    plugin: *const clap_plugin,
    width: u32,
    height: u32,
) -> bool {
    // SAFETY: as for `gui_destroy`.
    unsafe {
        entry::<P, _>(plugin, false, |gui| {
            gui.set_size(HostSize::new(width, height))
        })
    }
}

unsafe extern "C" fn gui_set_parent<P: Plugin>(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    // SAFETY: as for `gui_destroy`, and a window the host describes
    // truthfully, or null.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.set_parent(parent_window(window))) }
}

/// Floating windows are not offered, so there is nothing to keep above.
unsafe extern "C" fn gui_set_transient(
    _plugin: *const clap_plugin,
    _window: *const clap_window,
) -> bool {
    false
}

/// Floating windows are not offered, so there is no title to show.
unsafe extern "C" fn gui_suggest_title(_plugin: *const clap_plugin, _title: *const c_char) {}

unsafe extern "C" fn gui_show<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.set_visible(true)) }
}

unsafe extern "C" fn gui_hide<P: Plugin>(plugin: *const clap_plugin) -> bool {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, false, |gui| gui.set_visible(false)) }
}

unsafe extern "C" fn timer_on_timer<P: Plugin>(plugin: *const clap_plugin, timer_id: clap_id) {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, (), |gui| gui.on_timer(timer_id)) }
}

unsafe extern "C" fn fd_on_fd<P: Plugin>(
    plugin: *const clap_plugin,
    fd: i32,
    flags: clap_posix_fd_flags,
) {
    // SAFETY: as for `gui_destroy`.
    unsafe { entry::<P, _>(plugin, (), |gui| gui.on_fd(fd, flags)) }
}

/// The plugin's `on_main_thread`, for a plugin with an editor.
///
/// # Safety
///
/// As for [`entry`].
pub(crate) unsafe fn on_main_thread<P: Plugin>(plugin: *const clap_plugin) {
    // SAFETY: per this function's contract.
    unsafe { entry::<P, _>(plugin, (), Gui::on_main_thread) }
}
