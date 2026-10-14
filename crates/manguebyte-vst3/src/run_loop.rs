//! The host's own loop, VST 3's `IRunLoop`, as the editor contract's
//! [`HostLoop`].
//!
//! A host on Linux runs the plugin's editor from its own loop and offers it
//! through the `IPlugFrame` it gives the view: the frame also answers for
//! `IRunLoop`. The wrapper asks on every platform, so a host elsewhere that
//! offers one is used too.
//!
//! Each timer is registered with a handler object of its own, because
//! `IRunLoop` names timers by their handler. Descriptors are registered once
//! each, however many watches share one. A callback runs out of the table,
//! so it may register and cancel, itself included; one cancelled while it
//! runs is dropped as soon as it returns. Nothing fires while the editor
//! cannot be called (see [`Busy`]).
//!
//! [`Wake`] is the editor's way back to the main thread from other threads:
//! a Unix socket pair whose reading end is registered with the loop, and to
//! which a waker writes a byte. Where there are no Unix sockets there is no
//! wake, and the editor checks for work on its own timer, as the contract
//! allows.

// See the note at the top of `wrapper.rs`.
#![allow(clippy::unnecessary_cast)]

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use manguebyte_plugin::editor::{HostLoop, LoopToken};
use vst3::Steinberg::Linux::{
    IEventHandler, IEventHandlerTrait, IRunLoop, IRunLoopTrait, ITimerHandler, ITimerHandlerTrait,
};
use vst3::Steinberg::kResultOk;
use vst3::{Class, ComPtr, ComWrapper};

use crate::view::Busy;
use crate::wrapper::guard;

/// Timers and descriptors on the host's `IRunLoop`.
pub(crate) struct RunLoop {
    host: ComPtr<IRunLoop>,
    table: Rc<Table>,
}

/// What the loop's handler objects share with it.
struct Table {
    busy: Rc<Busy>,
    next_token: Cell<u64>,
    entries: RefCell<Vec<Entry>>,
    /// One registration with the host per descriptor watched.
    fds: RefCell<Vec<(i32, ComWrapper<FdHandler>)>>,
}

struct Entry {
    token: LoopToken,
    source: Source,
    /// `None` while the callback runs.
    callback: Option<Box<dyn FnMut()>>,
}

/// What a callback waits for.
enum Source {
    /// A timer, with the handler it was registered with.
    Timer(ComWrapper<TimerHandler>),
    /// A descriptor becoming readable.
    Fd(i32),
}

impl Source {
    fn is_fd(&self, fd: i32) -> bool {
        matches!(*self, Self::Fd(watched) if watched == fd)
    }
}

impl RunLoop {
    /// The loop `host` offers, for an editor whose calls are noted in
    /// `busy`.
    pub(crate) fn new(host: ComPtr<IRunLoop>, busy: Rc<Busy>) -> Self {
        Self {
            host,
            table: Rc::new(Table {
                busy,
                next_token: Cell::new(1),
                entries: RefCell::new(Vec::new()),
                fds: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Registers `handler` for `fd` with the host. True if the host took it.
    fn register_fd(&self, handler: *mut IEventHandler, fd: i32) -> bool {
        let _out = self.table.busy.calling_out();
        // SAFETY: the host's own loop, kept alive by our reference, on the
        // main thread, and a handler object of ours that the caller keeps
        // alive while it is registered.
        unsafe { self.host.registerEventHandler(handler, fd) == kResultOk }
    }

    /// Unregisters `handler` and every descriptor it watches.
    fn unregister_fd(&self, handler: *mut IEventHandler) {
        let _out = self.table.busy.calling_out();
        // SAFETY: as in `register_fd`.
        unsafe { self.host.unregisterEventHandler(handler) };
    }

    /// Cancels every timer and watch still registered. The editor's link
    /// cancels everything the editor registered; this is for what it could
    /// not, when the link is gone.
    pub(crate) fn cancel_all(&self) {
        let tokens: Vec<LoopToken> = self
            .table
            .entries
            .borrow()
            .iter()
            .map(|entry| entry.token)
            .collect();
        for token in tokens {
            self.cancel(token);
        }
    }
}

impl Table {
    fn next_token(&self) -> LoopToken {
        let token = LoopToken::new(self.next_token.get());
        self.next_token.set(token.get().wrapping_add(1));
        token
    }

    /// Runs every callback `wanted` picks, once, unless the editor cannot
    /// be called now.
    fn fire(&self, wanted: impl Fn(&Entry) -> bool) {
        if self.busy.is_busy() {
            return;
        }
        let due: Vec<LoopToken> = self
            .entries
            .borrow()
            .iter()
            .filter(|entry| wanted(entry))
            .map(|entry| entry.token)
            .collect();
        for token in due {
            if self.busy.is_busy() {
                // A callback before this one left the editor busy.
                return;
            }
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
            let _in = self.busy.in_editor();
            let mut running = Running {
                table: self,
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
    table: &'a Table,
    token: LoopToken,
    callback: Option<Box<dyn FnMut()>>,
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let callback = self.callback.take();
        let cancelled = match self.table.entries.try_borrow_mut() {
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

impl HostLoop for RunLoop {
    fn watch_fd(&self, fd: i32, ready: Box<dyn FnMut()>) -> Option<LoopToken> {
        let watched = self.table.fds.borrow().iter().any(|&(known, _)| known == fd);
        if !watched {
            let handler = ComWrapper::new(FdHandler {
                table: Rc::downgrade(&self.table),
                fd,
            });
            let pointer = handler.as_com_ref::<IEventHandler>()?.as_ptr();
            if !self.register_fd(pointer, fd) {
                return None;
            }
            self.table.fds.borrow_mut().push((fd, handler));
        }
        let token = self.table.next_token();
        self.table.entries.borrow_mut().push(Entry {
            token,
            source: Source::Fd(fd),
            callback: Some(ready),
        });
        Some(token)
    }

    fn every(&self, period_ms: u32, tick: Box<dyn FnMut()>) -> Option<LoopToken> {
        let token = self.table.next_token();
        let handler = ComWrapper::new(TimerHandler {
            table: Rc::downgrade(&self.table),
            token,
        });
        let pointer = handler.as_com_ref::<ITimerHandler>()?.as_ptr();
        let registered = {
            let _out = self.table.busy.calling_out();
            // SAFETY: as in `register_fd`. A period of zero would never
            // fire on some hosts, so the shortest asked for is 1 ms.
            unsafe { self.host.registerTimer(pointer, u64::from(period_ms.max(1))) == kResultOk }
        };
        if !registered {
            return None;
        }
        self.table.entries.borrow_mut().push(Entry {
            token,
            source: Source::Timer(handler),
            callback: Some(tick),
        });
        Some(token)
    }

    fn cancel(&self, token: LoopToken) {
        let removed = {
            let mut entries = self.table.entries.borrow_mut();
            entries
                .iter()
                .position(|entry| entry.token == token)
                .map(|index| entries.remove(index))
        };
        let Some(removed) = removed else {
            return;
        };
        match &removed.source {
            Source::Timer(handler) => {
                if let Some(handler) = handler.as_com_ref::<ITimerHandler>() {
                    let _out = self.table.busy.calling_out();
                    // SAFETY: as in `register_fd`; the handler is the one
                    // registered, alive until `removed` drops.
                    unsafe { self.host.unregisterTimer(handler.as_ptr()) };
                }
            }
            &Source::Fd(fd) => {
                // The registration goes with the last watch that uses it.
                let still_watched = self
                    .table
                    .entries
                    .borrow()
                    .iter()
                    .any(|entry| entry.source.is_fd(fd));
                if !still_watched {
                    let registration = {
                        let mut fds = self.table.fds.borrow_mut();
                        fds.iter()
                            .position(|&(known, _)| known == fd)
                            .map(|index| fds.remove(index))
                    };
                    if let Some((_, handler)) = registration {
                        if let Some(pointer) = handler.as_com_ref::<IEventHandler>() {
                            self.unregister_fd(pointer.as_ptr());
                        }
                    }
                }
            }
        }
        // The callback is dropped last, outside the table, unless it is
        // running, in which case it is dropped as soon as it returns.
        drop(removed);
    }
}

/// What a timer is registered with: the host calls it, and it runs the
/// callback its token names.
struct TimerHandler {
    table: Weak<Table>,
    token: LoopToken,
}

impl Class for TimerHandler {
    type Interfaces = (ITimerHandler,);
}

impl ITimerHandlerTrait for TimerHandler {
    unsafe fn onTimer(&self) {
        guard((), || {
            if let Some(table) = self.table.upgrade() {
                table.fire(|entry| entry.token == self.token);
            }
        });
    }
}

/// What a descriptor is registered with: the host calls it when the
/// descriptor can be read, and it runs every watch on it.
struct FdHandler {
    table: Weak<Table>,
    fd: i32,
}

impl Class for FdHandler {
    type Interfaces = (IEventHandler,);
}

impl IEventHandlerTrait for FdHandler {
    unsafe fn onFDIsSet(&self, _fd: i32) {
        // The descriptor registered, whatever the host passes.
        guard((), || {
            if let Some(table) = self.table.upgrade() {
                table.fire(|entry| entry.source.is_fd(self.fd));
            }
        });
    }
}

pub(crate) use wake::Wake;

#[cfg(unix)]
mod wake {
    use std::cell::Cell;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::rc::{Rc, Weak};
    use std::sync::Arc;

    use vst3::Steinberg::Linux::{IEventHandler, IEventHandlerTrait};
    use vst3::{Class, ComWrapper};

    use super::RunLoop;
    use crate::view::ViewState;
    use crate::wrapper::guard;

    /// A way back to the main thread for an open editor: a socket pair
    /// whose reading end is registered with the host's loop.
    pub(crate) struct Wake {
        /// Kept so that the socket can be unregistered.
        run_loop: Rc<RunLoop>,
        handler: ComWrapper<WakeHandler>,
        writer: Arc<UnixStream>,
        registered: Cell<bool>,
    }

    impl Wake {
        /// Registers a new socket pair with `run_loop`, so that a byte
        /// written to it makes the host call `view` back. `None` if the
        /// system or the host refused.
        pub(crate) fn new(run_loop: &Rc<RunLoop>, view: Weak<ViewState>) -> Option<Self> {
            let (reader, writer) = UnixStream::pair().ok()?;
            reader.set_nonblocking(true).ok()?;
            writer.set_nonblocking(true).ok()?;
            let fd = reader.as_raw_fd();
            let handler = ComWrapper::new(WakeHandler { reader, view });
            let pointer = handler.as_com_ref::<IEventHandler>()?.as_ptr();
            if !run_loop.register_fd(pointer, fd) {
                return None;
            }
            Some(Self {
                run_loop: Rc::clone(run_loop),
                handler,
                writer: Arc::new(writer),
                registered: Cell::new(true),
            })
        }

        /// The function that asks for a call back: any thread may call it.
        /// A full socket means a call back is already on its way.
        pub(crate) fn signal(&self) -> Arc<dyn Fn() + Send + Sync> {
            let writer = Arc::clone(&self.writer);
            Arc::new(move || {
                let _ = (&*writer).write(&[1]);
            })
        }

        /// Unregisters the socket from the host's loop. Later calls do
        /// nothing.
        pub(crate) fn release(&self) {
            if self.registered.replace(false) {
                if let Some(pointer) = self.handler.as_com_ref::<IEventHandler>() {
                    self.run_loop.unregister_fd(pointer.as_ptr());
                }
            }
        }
    }

    /// What the socket is registered with.
    struct WakeHandler {
        reader: UnixStream,
        view: Weak<ViewState>,
    }

    impl Class for WakeHandler {
        type Interfaces = (IEventHandler,);
    }

    impl IEventHandlerTrait for WakeHandler {
        unsafe fn onFDIsSet(&self, _fd: i32) {
            guard((), || {
                // Every byte written so far is answered by this one call.
                let mut bytes = [0u8; 64];
                while matches!((&self.reader).read(&mut bytes), Ok(count) if count > 0) {}
                if let Some(view) = self.view.upgrade() {
                    view.on_wake();
                }
            });
        }
    }
}

#[cfg(not(unix))]
mod wake {
    use std::rc::{Rc, Weak};
    use std::sync::Arc;

    use super::RunLoop;
    use crate::view::ViewState;

    /// No way back to the main thread on this system: there is never one.
    pub(crate) enum Wake {}

    impl Wake {
        /// Always `None`: without Unix sockets there is nothing to register
        /// with the host's loop.
        pub(crate) fn new(_run_loop: &Rc<RunLoop>, _view: Weak<ViewState>) -> Option<Self> {
            None
        }

        pub(crate) fn signal(&self) -> Arc<dyn Fn() + Send + Sync> {
            match *self {}
        }

        pub(crate) fn release(&self) {
            match *self {}
        }
    }
}
