//! The host as a plugin sees it.
//!
//! Every plugin instance is given a `clap_host`: a table of functions it
//! may call back, some of them from any thread at any moment. [`Shared`]
//! is what stands behind that table. Both halves of a hosted plugin hold
//! it in an [`Arc`]. Everything in it that a callback changes is an atomic,
//! or is touched on the main thread only, so a callback never waits for
//! anything. The one lock the two halves have in common guards the queue
//! end the audio half hands back as it is dropped, which is not something
//! that happens while audio is processed.
//!
//! The callbacks do not act. They record what the plugin asked for, and
//! [`ClapPlugin::pump`](super::ClapPlugin::pump) acts on it later, on the
//! main thread, outside any call into the plugin. That keeps the plugin
//! from re-entering the host in the middle of something else.
//!
//! An editor's timers and file descriptors are the one thing the callbacks
//! keep rather than flag, in [`EditorLoop`]. CLAP has them registered on
//! the main thread only, so the mutex around them is never contended: it
//! is there because [`Shared`] has to be `Sync`, and it is held for the
//! length of one registration or one look, never across a call into the
//! plugin. A plugin may register or unregister from inside any call the
//! host makes to it, `on_timer` included, and finds the lock free.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::{Duration, Instant};

use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::{CLAP_EXT_LOG, clap_host_log, clap_log_severity};
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::posix_fd_support::{
    CLAP_EXT_POSIX_FD_SUPPORT, CLAP_POSIX_FD_ERROR, CLAP_POSIX_FD_READ, CLAP_POSIX_FD_WRITE,
    clap_host_posix_fd_support, clap_posix_fd_flags,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};
use clap_sys::host::clap_host;
use clap_sys::id::{CLAP_INVALID_ID, clap_id};
use clap_sys::plugin::clap_plugin;
use clap_sys::version::CLAP_VERSION;
use manguebyte_core::spsc::Consumer;

use super::library::ClapLibrary;
use super::mailbox::{MESSAGE_BYTES, Mailbox};
use super::text::{copy_bounded, text_is};
use super::{HostInfo, ProcessFailure, ToAudio};
use crate::editor::{EditorSize, FdInterest, FdWatches, Timers};

/// Things a plugin has asked for and the main thread has not handled yet,
/// as bits of [`Shared::requests`]. Requests to read a list again are kept
/// apart, with their flags, in the `*_rescan` fields.
pub(crate) mod request {
    pub(crate) const RESTART: u32 = 1 << 0;
    pub(crate) const PROCESS: u32 = 1 << 1;
    pub(crate) const CALLBACK: u32 = 1 << 2;
    pub(crate) const FLUSH: u32 = 1 << 3;
    pub(crate) const STATE_DIRTY: u32 = 1 << 4;
    pub(crate) const LATENCY: u32 = 1 << 5;
    pub(crate) const TAIL: u32 = 1 << 6;
    pub(crate) const PARAM_CLEAR: u32 = 1 << 7;
}

/// What a plugin's editor has asked the application for and the main
/// thread has not handled yet, as bits of [`Shared::editor_requests`]. A
/// later request of the same kind replaces an earlier one: the size asked
/// for last, shown or hidden as asked last.
pub(crate) mod editor_request {
    pub(crate) const RESIZE: u32 = 1 << 0;
    pub(crate) const SHOW: u32 = 1 << 1;
    pub(crate) const HIDE: u32 = 1 << 2;
    pub(crate) const CLOSED: u32 = 1 << 3;
}

/// Most requests to clear a parameter's automation that are kept between
/// two calls to `pump`.
const MAX_CLEARS: usize = 1_024;

/// The audio port properties this host can read again when a plugin says
/// they changed: all the ones CLAP defines.
const AUDIO_PORT_RESCANS: u32 = CLAP_AUDIO_PORTS_RESCAN_NAMES
    | CLAP_AUDIO_PORTS_RESCAN_FLAGS
    | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT
    | CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE
    | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR
    | CLAP_AUDIO_PORTS_RESCAN_LIST;

/// The kinds of note event this host converts to and from, which is what a
/// plugin is told when it asks. MPE is not among them: a port that reads
/// MIDI as MPE is sent MIDI all the same, but this host does not arrange
/// notes into MPE zones, and so does not claim to speak the dialect.
pub(crate) const NOTE_DIALECTS: clap_note_dialect = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;

// How threads are told apart and how a panic is kept from the plugin are
// the same for every format, and live with the rest of what the formats
// share.
pub(crate) use crate::threads::{guard, thread_token};

/// The bytes of a C string holding `text`, without any NUL bytes it
/// contained, and with the terminator.
///
/// A `Vec` rather than a `CString`: a `CString` is a `Box`, and moving a
/// `Box` asserts that nothing else points into it, which would make the
/// pointers `clap_host` holds invalid the moment the text is moved into
/// [`Shared`].
fn c_string(text: &str) -> Vec<u8> {
    let mut bytes: Vec<u8> = text.bytes().filter(|&byte| byte != 0).collect();
    bytes.push(0);
    bytes
}

/// What both halves of a hosted plugin, and the plugin itself through its
/// callbacks, have in common.
pub(crate) struct Shared {
    /// What the plugin holds a pointer to. Its `host_data` points back at
    /// this struct.
    host: clap_host,
    /// Owns the text `host` points to, each NUL-terminated.
    _text: [Vec<u8>; 4],
    /// The plugin, once the factory has made it.
    plugin: AtomicPtr<clap_plugin>,
    /// The thread the plugin was created on, as its [`thread_token`].
    main_thread: u64,
    /// The thread that is inside one of the plugin's audio-thread
    /// functions right now, as its [`thread_token`], or zero.
    audio_thread: AtomicU64,
    /// [`request`] bits.
    requests: AtomicU32,
    /// `CLAP_PARAM_RESCAN_*` flags gathered since they were last handled.
    pub(crate) params_rescan: AtomicU32,
    /// `CLAP_AUDIO_PORTS_RESCAN_*` flags gathered likewise.
    pub(crate) audio_ports_rescan: AtomicU32,
    /// `CLAP_NOTE_PORTS_RESCAN_*` flags gathered likewise.
    pub(crate) note_ports_rescan: AtomicU32,
    /// Requests to forget automation, as `(parameter, flags)`. Only the
    /// main thread touches this.
    pub(crate) clears: Mutex<Vec<(clap_id, u32)>>,
    /// Kinds of processing failure seen since the main thread last asked.
    failures: AtomicU32,
    /// Blocks that ended in silence because the plugin failed.
    pub(crate) failed_blocks: AtomicU64,
    /// Events dropped because a list or a queue was full.
    pub(crate) lost_events: AtomicU64,
    /// Events not sent because the plugin has no way to receive them.
    pub(crate) undeliverable_events: AtomicU64,
    /// Calls the plugin made on a thread CLAP does not allow them on.
    pub(crate) wrong_thread_calls: AtomicU32,
    /// Values and gestures the plugin reported while processing for
    /// parameters that are not in its list, and that were dropped.
    pub(crate) unlisted_reports: AtomicU64,
    pub(crate) log: Mailbox,
    active: AtomicBool,
    processor_alive: AtomicBool,
    latency: AtomicU32,
    /// The receiving end of the main thread's queue, handed back by the
    /// audio half when it is dropped, with whatever it had not read.
    leftover: Mutex<Option<Consumer<ToAudio>>>,
    /// Set when the main handle has been dropped.
    main_gone: AtomicBool,
    /// Set when the plugin has been destroyed, or is about to be.
    closed: AtomicBool,
    /// The timers and descriptors the plugin registered. Only the main
    /// thread touches this, and never while it calls into the plugin.
    editor_loop: Mutex<EditorLoop>,
    /// Set from just before the plugin's `gui.create` until its editor is
    /// destroyed: the time in which its requests mean something.
    editor_created: AtomicBool,
    /// [`editor_request`] bits.
    editor_requests: AtomicU32,
    /// The size of the latest resize request, the width in the high half.
    requested_size: AtomicU64,
    /// Keeps the plugin's code loaded for as long as this value exists,
    /// which is at least as long as the plugin does.
    _library: ClapLibrary,
}

// Both halves hold a `Shared`, on different threads, so it has to be safe
// to share. Nothing here needs to promise that by hand: every field that
// changes is an atomic or behind a mutex. This fails to compile if a field
// is ever added for which that is not true.
const _: () = {
    const fn shareable<T: Send + Sync>() {}
    shareable::<Shared>();
};

impl Shared {
    /// Makes the host side of one plugin instance. The calling thread
    /// becomes that plugin's main thread.
    pub(crate) fn new(info: &HostInfo, library: ClapLibrary) -> Arc<Self> {
        let text = [
            c_string(&info.name),
            c_string(&info.vendor),
            c_string(&info.url),
            c_string(&info.version),
        ];
        Arc::new_cyclic(|this: &Weak<Self>| Self {
            host: clap_host {
                clap_version: CLAP_VERSION,
                // The address this value is about to be given. It is only
                // followed from the callbacks, which the plugin cannot
                // reach before this value exists.
                host_data: this.as_ptr().cast_mut().cast(),
                // A `Vec` keeps its bytes on the heap, so these stay valid
                // when the array is moved into the struct.
                name: text[0].as_ptr().cast(),
                vendor: text[1].as_ptr().cast(),
                url: text[2].as_ptr().cast(),
                version: text[3].as_ptr().cast(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
            _text: text,
            plugin: AtomicPtr::new(ptr::null_mut()),
            main_thread: thread_token(),
            audio_thread: AtomicU64::new(0),
            requests: AtomicU32::new(0),
            params_rescan: AtomicU32::new(0),
            audio_ports_rescan: AtomicU32::new(0),
            note_ports_rescan: AtomicU32::new(0),
            clears: Mutex::new(Vec::new()),
            failures: AtomicU32::new(0),
            failed_blocks: AtomicU64::new(0),
            lost_events: AtomicU64::new(0),
            undeliverable_events: AtomicU64::new(0),
            wrong_thread_calls: AtomicU32::new(0),
            unlisted_reports: AtomicU64::new(0),
            log: Mailbox::new(),
            active: AtomicBool::new(false),
            processor_alive: AtomicBool::new(false),
            latency: AtomicU32::new(0),
            leftover: Mutex::new(None),
            main_gone: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            editor_loop: Mutex::new(EditorLoop::default()),
            editor_created: AtomicBool::new(false),
            editor_requests: AtomicU32::new(0),
            requested_size: AtomicU64::new(0),
            _library: library,
        })
    }

    /// The pointer the plugin is given as its host.
    pub(crate) fn host_ptr(&self) -> *const clap_host {
        &raw const self.host
    }

    /// Records the plugin the factory made. From here on
    /// [`close`](Self::close) destroys it.
    pub(crate) fn set_plugin(&self, plugin: *const clap_plugin) {
        self.plugin.store(plugin.cast_mut(), Ordering::Release);
    }

    /// Whether the calling thread is the plugin's main thread.
    pub(crate) fn is_main(&self) -> bool {
        let token = thread_token();
        token != 0 && token == self.main_thread
    }

    /// Whether the calling thread is inside one of the plugin's
    /// audio-thread functions.
    fn is_audio(&self) -> bool {
        let token = thread_token();
        token != 0 && token == self.audio_thread.load(Ordering::Relaxed)
    }

    /// Marks the calling thread as the audio thread, for the length of one
    /// call into the plugin, and returns what to give back afterwards.
    ///
    /// CLAP's audio thread is a role, not a thread: whichever thread is
    /// calling an audio-thread function holds it. Real-time safe.
    pub(crate) fn take_audio_role(&self) -> u64 {
        self.audio_thread.swap(thread_token(), Ordering::Relaxed)
    }

    /// Ends what [`take_audio_role`](Self::take_audio_role) began.
    pub(crate) fn give_audio_role(&self, previous: u64) {
        self.audio_thread.store(previous, Ordering::Relaxed);
    }

    fn raise(&self, bits: u32) {
        self.requests.fetch_or(bits, Ordering::Relaxed);
    }

    /// The requests made since the last call, which are then forgotten.
    pub(crate) fn take_requests(&self) -> u32 {
        self.requests.swap(0, Ordering::Relaxed)
    }

    /// The requests made since the last call other than `kept`, which are
    /// then forgotten. A request among `kept` is neither returned nor
    /// forgotten: it stays for the next [`take_requests`](Self::take_requests).
    ///
    /// One atomic step, so a request that arrives from another thread at
    /// the same moment is either returned here or still there afterwards.
    pub(crate) fn take_requests_except(&self, kept: u32) -> u32 {
        self.requests.fetch_and(kept, Ordering::Relaxed) & !kept
    }

    /// Records a call made on a thread it is not allowed on.
    fn wrong_thread(&self) {
        self.wrong_thread_calls.fetch_add(1, Ordering::Relaxed);
    }

    /// Records that a block ended in silence, and why. Real-time safe.
    pub(crate) fn fail(&self, failure: ProcessFailure) {
        self.failures.fetch_or(failure.bit(), Ordering::Relaxed);
        self.failed_blocks.fetch_add(1, Ordering::Relaxed);
    }

    /// The kinds of failure seen since the last call, as
    /// [`ProcessFailure::bit`]s.
    pub(crate) fn take_failures(&self) -> u32 {
        self.failures.swap(0, Ordering::Relaxed)
    }

    pub(crate) fn active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::Release);
    }

    pub(crate) fn processor_alive(&self) -> bool {
        self.processor_alive.load(Ordering::Acquire)
    }

    /// Records that an audio half now exists.
    pub(crate) fn processor_created(&self) {
        self.processor_alive.store(true, Ordering::Release);
    }

    /// Called by the audio half as it is dropped: hands back its end of the
    /// main thread's queue, then lets the main thread know it is gone.
    ///
    /// The order matters. A value the main thread queues while it still
    /// believes the audio half is alive is then certain to be in the queue
    /// that was handed back.
    pub(crate) fn processor_dropped(&self, unread: Option<Consumer<ToAudio>>) {
        *self
            .leftover
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = unread;
        self.processor_alive.store(false, Ordering::SeqCst);
    }

    /// The queue end handed back by a dropped audio half, if any.
    pub(crate) fn take_leftover(&self) -> Option<Consumer<ToAudio>> {
        self.leftover
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }

    pub(crate) fn latency(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    pub(crate) fn set_latency(&self, frames: u32) {
        self.latency.store(frames, Ordering::Relaxed);
    }

    /// The editor's timers and descriptors, locked. Main thread only, and
    /// never to be held across a call into the plugin.
    pub(crate) fn editor_loop(&self) -> MutexGuard<'_, EditorLoop> {
        self.editor_loop
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Whether the plugin's editor exists, as far as its requests go.
    fn editor_created(&self) -> bool {
        self.editor_created.load(Ordering::Acquire)
    }

    /// Records that the editor is about to be created, or is gone. Either
    /// way, requests left from an earlier editor are forgotten.
    pub(crate) fn set_editor_created(&self, created: bool) {
        self.editor_created.store(created, Ordering::Release);
        self.editor_requests.store(0, Ordering::Release);
    }

    /// The editor's requests since the last call, as [`editor_request`]
    /// bits, and the size of the latest resize request. They are then
    /// forgotten.
    pub(crate) fn take_editor_requests(&self) -> (u32, EditorSize) {
        let bits = self.editor_requests.swap(0, Ordering::Acquire);
        let size = self.requested_size.load(Ordering::Acquire);
        // The two halves of one `u64`.
        let size = EditorSize::new((size >> 32) as u32, size as u32);
        (bits, size)
    }

    /// The editor asks to be `size`. True if the request is taken, which
    /// it is whenever the editor exists and the size is not empty: the
    /// application answers it later, from `pump`.
    fn request_editor_resize(&self, size: EditorSize) -> bool {
        if !self.editor_created() || size.width == 0 || size.height == 0 {
            return false;
        }
        let packed = (u64::from(size.width) << 32) | u64::from(size.height);
        // The size is in place before the bit that says it is there.
        self.requested_size.store(packed, Ordering::Release);
        self.editor_requests
            .fetch_or(editor_request::RESIZE, Ordering::Release);
        true
    }

    /// The editor asks to be shown or hidden; the later request replaces
    /// the earlier. True if the editor exists.
    fn request_editor_visible(&self, visible: bool) -> bool {
        if !self.editor_created() {
            return false;
        }
        let (wanted, replaced) = if visible {
            (editor_request::SHOW, editor_request::HIDE)
        } else {
            (editor_request::HIDE, editor_request::SHOW)
        };
        // One step, so that of two requests from two threads at once one
        // is the last, and not both nor neither.
        let mut bits = self.editor_requests.load(Ordering::Acquire);
        while let Err(now) = self.editor_requests.compare_exchange_weak(
            bits,
            bits & !replaced | wanted,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            bits = now;
        }
        true
    }

    /// The editor closed by itself.
    fn editor_closed(&self) {
        if self.editor_created() {
            self.editor_requests
                .fetch_or(editor_request::CLOSED, Ordering::Release);
        }
    }

    /// Deactivates the plugin if it is active, and destroys it. Does
    /// nothing the second time.
    ///
    /// This is a method on a shared reference and not a destructor, on
    /// purpose. A plugin may call back into its host while it is being
    /// destroyed, through the pointer it holds to this value, and a
    /// destructor's exclusive borrow of the value would make that call a
    /// breach of Rust's aliasing rules.
    ///
    /// To be called on the plugin's main thread only, with no audio half
    /// in existence: the two conditions CLAP sets for destroying a plugin.
    pub(crate) fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let plugin = self
            .plugin
            .swap(ptr::null_mut(), Ordering::AcqRel)
            .cast_const();
        if plugin.is_null() {
            return;
        }
        // SAFETY: `plugin` came from the plugin's factory, and the swap
        // above makes this the one place it is destroyed. Per this
        // function's contract this is the main thread and nothing is
        // processing, which is what CLAP asks for both calls.
        unsafe {
            let table = *plugin;
            if self.active.swap(false, Ordering::AcqRel) {
                if let Some(deactivate) = table.deactivate {
                    deactivate(plugin);
                }
            }
            if let Some(destroy) = table.destroy {
                destroy(plugin);
            }
        }
    }

    /// Called when the main handle is dropped, on the main thread.
    ///
    /// If the audio half is gone too, the plugin is destroyed now. If not,
    /// that falls to the audio half, in
    /// [`processor_gone`](Self::processor_gone).
    pub(crate) fn main_handle_dropped(&self) {
        // This store and load, and the mirrored pair in `processor_gone`
        // and `processor_dropped`, are sequentially consistent so that when
        // both halves go at the same moment on two threads, at least one
        // of them sees that the other has gone.
        self.main_gone.store(true, Ordering::SeqCst);
        if !self.processor_alive.load(Ordering::SeqCst) {
            self.close();
        }
    }

    /// Called by the audio half as the last thing it does, with its own
    /// reference to this value.
    ///
    /// CLAP allows a plugin to be destroyed on its main thread only. While
    /// the main handle exists, destroying is its job. If it is gone and
    /// this happens to be the main thread, the plugin is destroyed here.
    /// If it is gone and this is another thread, no thread is left on
    /// which the plugin may be destroyed. It is then kept, with this value
    /// and its code, until the process ends: a leak, chosen over
    /// destroying a plugin on a thread it does not expect.
    pub(crate) fn processor_gone(this: &Arc<Self>) {
        if !this.main_gone.load(Ordering::SeqCst) {
            return;
        }
        if this.is_main() {
            this.close();
        } else if !this.closed.load(Ordering::SeqCst) {
            std::mem::forget(Arc::clone(this));
        }
    }
}

/// The timers and descriptors a plugin registered through
/// `timer-support` and `posix-fd-support`, and which of them go with its
/// editor.
///
/// A plugin may register a timer at any time, not only while its editor is
/// open. The ones it registers while the editor exists are noted, so that
/// closing the editor takes away whatever it left behind without stopping
/// the others.
#[derive(Debug, Default)]
pub(crate) struct EditorLoop {
    pub(crate) timers: Timers,
    /// Every registered timer's id, and whether the editor existed when it
    /// was registered. No longer than `timers`, which is bounded.
    timer_ids: Vec<(u32, bool)>,
    pub(crate) fds: FdWatches,
    /// The descriptors watched since the editor was created.
    editor_fds: Vec<i32>,
}

impl EditorLoop {
    fn register_timer(&mut self, period: Duration, editor: bool) -> Option<u32> {
        let id = self.timers.add(period, Instant::now())?;
        self.timer_ids.push((id, editor));
        Some(id)
    }

    fn unregister_timer(&mut self, id: u32) -> bool {
        self.timer_ids.retain(|&(known, _)| known != id);
        self.timers.remove(id)
    }

    /// Whether the timer `id` is registered.
    pub(crate) fn has_timer(&self, id: u32) -> bool {
        self.timer_ids.iter().any(|&(known, _)| known == id)
    }

    fn register_fd(&mut self, fd: i32, interest: FdInterest, editor: bool) -> bool {
        let added = self.fds.add(fd, interest);
        if added && editor {
            self.editor_fds.push(fd);
        }
        added
    }

    fn unregister_fd(&mut self, fd: i32) -> bool {
        self.editor_fds.retain(|&known| known != fd);
        self.fds.remove(fd)
    }

    /// Forgets the timers and descriptors registered while the editor
    /// existed, as it goes.
    pub(crate) fn forget_editor(&mut self) {
        let timers = &mut self.timers;
        self.timer_ids.retain(|&(id, editor)| {
            if editor {
                timers.remove(id);
            }
            !editor
        });
        for fd in std::mem::take(&mut self.editor_fds) {
            self.fds.remove(fd);
        }
    }

    /// Forgets every timer and descriptor, as the main half goes: nothing
    /// could fire them any more.
    pub(crate) fn clear(&mut self) {
        self.timers.clear();
        self.timer_ids.clear();
        self.fds.clear();
        self.editor_fds.clear();
    }
}

/// What a plugin waits for on a descriptor, from CLAP's flags. Flags CLAP
/// does not define are ignored.
pub(crate) fn fd_interest(flags: clap_posix_fd_flags) -> FdInterest {
    FdInterest {
        read: flags & CLAP_POSIX_FD_READ != 0,
        write: flags & CLAP_POSIX_FD_WRITE != 0,
        error: flags & CLAP_POSIX_FD_ERROR != 0,
    }
}

/// CLAP's flags for what happened on a descriptor.
pub(crate) fn fd_flags(interest: FdInterest) -> clap_posix_fd_flags {
    let flag = |set: bool, flag: clap_posix_fd_flags| if set { flag } else { 0 };
    flag(interest.read, CLAP_POSIX_FD_READ)
        | flag(interest.write, CLAP_POSIX_FD_WRITE)
        | flag(interest.error, CLAP_POSIX_FD_ERROR)
}

/// The `Shared` behind a host pointer a plugin passed back.
///
/// # Safety
///
/// `host` must be null or the pointer [`Shared::host_ptr`] gave the plugin,
/// and the plugin must not have been destroyed.
unsafe fn shared<'a>(host: *const clap_host) -> Option<&'a Shared> {
    if host.is_null() {
        return None;
    }
    // SAFETY: per the contract `host` is the `clap_host` inside a live
    // `Shared`, whose `host_data` is that value's own address. A `Shared`
    // outlives its plugin: it is what destroys it.
    unsafe { (*host).host_data.cast::<Shared>().as_ref() }
}

/// Does `work` for a callback that CLAP allows on the main thread only.
/// From any other thread the call is counted and otherwise ignored.
///
/// # Safety
///
/// As for [`shared`].
unsafe fn on_main_thread(host: *const clap_host, work: impl FnOnce(&Shared)) {
    // SAFETY: passed on from the caller.
    unsafe { main_thread_answer(host, (), work) }
}

/// Answers a callback that CLAP allows on the main thread only with what
/// `work` returns. From any other thread the call is counted and answered
/// with `refused`, as is a call for a host that is not there.
///
/// # Safety
///
/// As for [`shared`].
unsafe fn main_thread_answer<R: Copy>(
    host: *const clap_host,
    refused: R,
    work: impl FnOnce(&Shared) -> R,
) -> R {
    guard(refused, || {
        // SAFETY: passed on from the caller.
        let Some(shared) = (unsafe { shared(host) }) else {
            return refused;
        };
        if shared.is_main() {
            work(shared)
        } else {
            shared.wrong_thread();
            refused
        }
    })
}

/// Answers a callback that CLAP allows on any thread with what `work`
/// returns, or with `refused` for a host that is not there.
///
/// # Safety
///
/// As for [`shared`].
unsafe fn any_thread_answer<R: Copy>(
    host: *const clap_host,
    refused: R,
    work: impl FnOnce(&Shared) -> R,
) -> R {
    guard(refused, || {
        // SAFETY: passed on from the caller.
        unsafe { shared(host) }.map_or(refused, work)
    })
}

/// Records a request that may come from any thread.
///
/// # Safety
///
/// As for [`shared`].
unsafe fn note_request(host: *const clap_host, bits: u32) {
    guard((), || {
        // SAFETY: passed on from the caller.
        if let Some(shared) = unsafe { shared(host) } {
            shared.raise(bits);
        }
    });
}

// --- The host itself -----------------------------------------------------

unsafe extern "C" fn get_extension(host: *const clap_host, id: *const c_char) -> *const c_void {
    guard(ptr::null(), || {
        // SAFETY: the plugin passes the host it was created with.
        if unsafe { shared(host) }.is_none() {
            return ptr::null();
        }
        // SAFETY: the plugin passes a NUL-terminated extension id, or null.
        let is = |known: &CStr| unsafe { text_is(id, known) };
        if is(CLAP_EXT_THREAD_CHECK) {
            ptr::from_ref(&THREAD_CHECK).cast()
        } else if is(CLAP_EXT_LOG) {
            ptr::from_ref(&LOG).cast()
        } else if is(CLAP_EXT_PARAMS) {
            ptr::from_ref(&PARAMS).cast()
        } else if is(CLAP_EXT_STATE) {
            ptr::from_ref(&STATE).cast()
        } else if is(CLAP_EXT_LATENCY) {
            ptr::from_ref(&LATENCY).cast()
        } else if is(CLAP_EXT_TAIL) {
            ptr::from_ref(&TAIL).cast()
        } else if is(CLAP_EXT_AUDIO_PORTS) {
            ptr::from_ref(&AUDIO_PORTS).cast()
        } else if is(CLAP_EXT_NOTE_PORTS) {
            ptr::from_ref(&NOTE_PORTS).cast()
        } else if is(CLAP_EXT_GUI) {
            ptr::from_ref(&GUI).cast()
        } else if is(CLAP_EXT_TIMER_SUPPORT) {
            ptr::from_ref(&TIMER_SUPPORT).cast()
        } else if is(CLAP_EXT_POSIX_FD_SUPPORT) {
            // Offered on every system: a descriptor is only a number, and
            // where the application has no use for one, no plugin there
            // registers any.
            ptr::from_ref(&POSIX_FD_SUPPORT).cast()
        } else {
            ptr::null()
        }
    })
}

unsafe extern "C" fn request_restart(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { note_request(host, request::RESTART) };
}

unsafe extern "C" fn request_process(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { note_request(host, request::PROCESS) };
}

unsafe extern "C" fn request_callback(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { note_request(host, request::CALLBACK) };
}

// --- Thread check --------------------------------------------------------

static THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(is_main_thread),
    is_audio_thread: Some(is_audio_thread),
};

unsafe extern "C" fn is_main_thread(host: *const clap_host) -> bool {
    guard(false, || {
        // SAFETY: the plugin passes the host it was created with.
        unsafe { shared(host) }.is_some_and(Shared::is_main)
    })
}

unsafe extern "C" fn is_audio_thread(host: *const clap_host) -> bool {
    guard(false, || {
        // SAFETY: the plugin passes the host it was created with.
        unsafe { shared(host) }.is_some_and(Shared::is_audio)
    })
}

// --- Log -----------------------------------------------------------------

static LOG: clap_host_log = clap_host_log { log: Some(log) };

unsafe extern "C" fn log(host: *const clap_host, severity: clap_log_severity, text: *const c_char) {
    guard((), || {
        // SAFETY: the plugin passes the host it was created with.
        let Some(shared) = (unsafe { shared(host) }) else {
            return;
        };
        // This may be the audio thread, so the text is copied onto the
        // stack and from there into the mailbox, with no allocation.
        let mut bytes = [0u8; MESSAGE_BYTES];
        // SAFETY: the plugin passes a NUL-terminated message, or null.
        let length = unsafe { copy_bounded(text, &mut bytes) };
        shared.log.post(severity, &bytes[..length]);
    });
}

// --- Parameters ----------------------------------------------------------

static PARAMS: clap_host_params = clap_host_params {
    rescan: Some(params_rescan),
    clear: Some(params_clear),
    request_flush: Some(params_request_flush),
};

unsafe extern "C" fn params_rescan(host: *const clap_host, flags: clap_param_rescan_flags) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        on_main_thread(host, |shared| {
            shared.params_rescan.fetch_or(flags, Ordering::Relaxed);
        });
    }
}

unsafe extern "C" fn params_clear(
    host: *const clap_host,
    param_id: clap_id,
    flags: clap_param_clear_flags,
) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        on_main_thread(host, |shared| {
            let mut clears = shared
                .clears
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if clears.len() < MAX_CLEARS {
                clears.push((param_id, flags));
                shared.raise(request::PARAM_CLEAR);
            }
        });
    }
}

unsafe extern "C" fn params_request_flush(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { note_request(host, request::FLUSH) };
}

// --- State, latency and tail ---------------------------------------------

static STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(state_mark_dirty),
};

unsafe extern "C" fn state_mark_dirty(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { on_main_thread(host, |shared| shared.raise(request::STATE_DIRTY)) };
}

static LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(latency_changed),
};

unsafe extern "C" fn latency_changed(host: *const clap_host) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { on_main_thread(host, |shared| shared.raise(request::LATENCY)) };
}

static TAIL: clap_host_tail = clap_host_tail {
    changed: Some(tail_changed),
};

unsafe extern "C" fn tail_changed(host: *const clap_host) {
    // CLAP has the plugin call this on the audio thread.
    // SAFETY: the plugin passes the host it was created with.
    unsafe { note_request(host, request::TAIL) };
}

// --- Ports ---------------------------------------------------------------

static AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(audio_ports_is_rescan_flag_supported),
    rescan: Some(audio_ports_rescan),
};

unsafe extern "C" fn audio_ports_is_rescan_flag_supported(_: *const clap_host, flag: u32) -> bool {
    // Every list is read again whole, so every property CLAP defines can
    // be picked up. Nothing is promised about flags it does not define.
    flag != 0 && flag & !AUDIO_PORT_RESCANS == 0
}

unsafe extern "C" fn audio_ports_rescan(host: *const clap_host, flags: u32) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        on_main_thread(host, |shared| {
            shared.audio_ports_rescan.fetch_or(flags, Ordering::Relaxed);
        });
    }
}

static NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(note_ports_supported_dialects),
    rescan: Some(note_ports_rescan),
};

unsafe extern "C" fn note_ports_supported_dialects(_: *const clap_host) -> clap_note_dialect {
    NOTE_DIALECTS
}

unsafe extern "C" fn note_ports_rescan(host: *const clap_host, flags: u32) {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        on_main_thread(host, |shared| {
            shared.note_ports_rescan.fetch_or(flags, Ordering::Relaxed);
        });
    }
}

// --- Editors -------------------------------------------------------------

static GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_resize_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};

// CLAP lets a plugin make these requests from any thread. They are recorded
// in atomics, which any thread may touch without waiting, and answered by
// `pump` on the main thread. None of them calls the plugin back, so a
// plugin may make them from inside any call the host makes to it.

unsafe extern "C" fn gui_resize_hints_changed(_: *const clap_host) {
    // Nothing is kept of the hints: whether the editor can be resized, and
    // to what, is asked anew each time the application resizes it.
}

unsafe extern "C" fn gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    // The answer is "taken": the application resizes its window when it
    // reads the request, and then tells the plugin with `set_size`. That is
    // never called from in here, where the plugin is still in the middle of
    // asking.
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        any_thread_answer(host, false, |shared| {
            shared.request_editor_resize(EditorSize::new(width, height))
        })
    }
}

unsafe extern "C" fn gui_request_show(host: *const clap_host) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { any_thread_answer(host, false, |shared| shared.request_editor_visible(true)) }
}

unsafe extern "C" fn gui_request_hide(host: *const clap_host) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { any_thread_answer(host, false, |shared| shared.request_editor_visible(false)) }
}

unsafe extern "C" fn gui_closed(host: *const clap_host, _was_destroyed: bool) {
    // Whether the plugin says its editor was destroyed or only that the
    // connection to it was lost, `pump` destroys the editor: CLAP asks for
    // that as the acknowledgement in the first case, and in the second the
    // application is about to let go of the window the editor was in.
    // SAFETY: the plugin passes the host it was created with.
    unsafe { any_thread_answer(host, (), Shared::editor_closed) }
}

static TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(timer_register),
    unregister_timer: Some(timer_unregister),
};

unsafe extern "C" fn timer_register(
    host: *const clap_host,
    period_ms: u32,
    timer_id: *mut clap_id,
) -> bool {
    if timer_id.is_null() {
        return false;
    }
    // SAFETY: the plugin passes the host it was created with.
    let id = unsafe {
        main_thread_answer(host, None, |shared| {
            let editor = shared.editor_created();
            shared
                .editor_loop()
                .register_timer(Duration::from_millis(period_ms.into()), editor)
        })
    };
    // SAFETY: the plugin passes a place for the id, checked non-null above.
    unsafe { timer_id.write(id.unwrap_or(CLAP_INVALID_ID)) };
    id.is_some()
}

unsafe extern "C" fn timer_unregister(host: *const clap_host, timer_id: clap_id) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        main_thread_answer(host, false, |shared| {
            shared.editor_loop().unregister_timer(timer_id)
        })
    }
}

static POSIX_FD_SUPPORT: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(fd_register),
    modify_fd: Some(fd_modify),
    unregister_fd: Some(fd_unregister),
};

unsafe extern "C" fn fd_register(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        main_thread_answer(host, false, |shared| {
            let editor = shared.editor_created();
            shared
                .editor_loop()
                .register_fd(fd, fd_interest(flags), editor)
        })
    }
}

unsafe extern "C" fn fd_modify(
    host: *const clap_host,
    fd: i32,
    flags: clap_posix_fd_flags,
) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe {
        main_thread_answer(host, false, |shared| {
            shared.editor_loop().fds.modify(fd, fd_interest(flags))
        })
    }
}

unsafe extern "C" fn fd_unregister(host: *const clap_host, fd: i32) -> bool {
    // SAFETY: the plugin passes the host it was created with.
    unsafe { main_thread_answer(host, false, |shared| shared.editor_loop().unregister_fd(fd)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thread_keeps_its_token_and_no_later_thread_is_given_it() {
        let here = thread_token();
        assert_ne!(here, 0);
        assert_eq!(thread_token(), here, "the token changed within a thread");
        let mut given = vec![here];

        // Threads that run one after another, each gone before the next
        // starts. This is where the address of a thread-local comes round
        // again, and where a token must not.
        let rounds = if cfg!(miri) { 8 } else { 256 };
        for _ in 0..rounds {
            let token = std::thread::spawn(|| {
                let token = thread_token();
                assert_eq!(thread_token(), token);
                token
            })
            .join()
            .unwrap();
            assert_ne!(token, 0);
            assert!(!given.contains(&token), "the token {token} was given twice");
            given.push(token);
        }

        // Threads that are alive at the same time.
        let together: Vec<u64> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8).map(|_| scope.spawn(thread_token)).collect();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });
        for token in together {
            assert_ne!(token, 0);
            assert!(!given.contains(&token), "the token {token} was given twice");
            given.push(token);
        }
        assert_eq!(thread_token(), here);
    }

    /// A library with no plugins in it, for a `Shared` to belong to.
    fn empty_library() -> ClapLibrary {
        use clap_sys::entry::clap_plugin_entry;
        use clap_sys::factory::plugin_factory::clap_plugin_factory;

        unsafe extern "C" fn init(_path: *const c_char) -> bool {
            true
        }
        unsafe extern "C" fn deinit() {}
        unsafe extern "C" fn count(_factory: *const clap_plugin_factory) -> u32 {
            0
        }
        static FACTORY: clap_plugin_factory = clap_plugin_factory {
            get_plugin_count: Some(count),
            get_plugin_descriptor: None,
            create_plugin: None,
        };
        unsafe extern "C" fn factory(_id: *const c_char) -> *const c_void {
            (&raw const FACTORY).cast()
        }
        static ENTRY: clap_plugin_entry = clap_plugin_entry {
            clap_version: CLAP_VERSION,
            init: Some(init),
            deinit: Some(deinit),
            get_factory: Some(factory),
        };
        // SAFETY: the entry point is the static above, whose functions do
        // what CLAP asks of them.
        unsafe { ClapLibrary::from_entry(&raw const ENTRY, "empty") }.unwrap()
    }

    #[test]
    fn the_main_thread_is_the_one_that_made_the_host_and_no_later_one() {
        // Made on a thread that then ends, as a plugin's main thread may.
        let shared = std::thread::spawn(|| {
            let shared = Shared::new(&HostInfo::default(), empty_library());
            assert!(shared.is_main());
            assert!(!shared.is_audio());
            shared
        })
        .join()
        .unwrap();
        assert!(!shared.is_main());

        // No thread started afterwards passes for it, however many come
        // and go, and whichever of them holds the audio role meanwhile.
        let rounds = if cfg!(miri) { 8 } else { 256 };
        for _ in 0..rounds {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || {
                assert!(
                    !shared.is_main(),
                    "a later thread passed for the main thread"
                );
                assert!(!shared.is_audio());
                let previous = shared.take_audio_role();
                assert!(shared.is_audio() && !shared.is_main());
                shared.give_audio_role(previous);
                assert!(!shared.is_audio(), "the audio role outlived the call");
            })
            .join()
            .unwrap();
        }
        assert!(!shared.is_audio());
    }

    #[test]
    fn requests_are_taken_once_and_the_ones_held_back_stay() {
        let shared = Shared::new(&HostInfo::default(), empty_library());

        shared.raise(request::RESTART | request::CALLBACK);
        assert_eq!(shared.take_requests(), request::RESTART | request::CALLBACK);
        assert_eq!(shared.take_requests(), 0, "a request was answered twice");

        // Everything but a callback is taken; the callback stays for the
        // next round, where it is taken with whatever else has come.
        shared.raise(request::FLUSH | request::CALLBACK | request::TAIL);
        assert_eq!(
            shared.take_requests_except(request::CALLBACK),
            request::FLUSH | request::TAIL
        );
        assert_eq!(shared.take_requests_except(request::CALLBACK), 0);
        shared.raise(request::PROCESS);
        assert_eq!(shared.take_requests(), request::CALLBACK | request::PROCESS);
        assert_eq!(shared.take_requests(), 0);

        // Nothing held back is nothing kept.
        shared.raise(request::LATENCY);
        assert_eq!(shared.take_requests_except(0), request::LATENCY);
        assert_eq!(shared.take_requests(), 0);
    }
}
