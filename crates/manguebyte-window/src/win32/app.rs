//! An application's side of the Win32 backend: OLE for its thread, and the
//! thread's message loop.

use std::ptr;

use windows_sys::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

use super::last_error;
use crate::window::WindowError;

/// OLE on the application's thread, and its loop.
pub(crate) struct Win32App {
    /// Whether this application initialised OLE, and so uninitialises it.
    ole: bool,
}

impl Win32App {
    /// Initialises OLE for this thread, which drag and drop needs and the
    /// file dialogs want: a single-threaded apartment. A thread that is
    /// already a multithreaded apartment keeps it; the application then
    /// takes no drops.
    pub(crate) fn new() -> Win32App {
        // SAFETY: the reserved argument is null, as documented. Every
        // success, `S_FALSE` (already initialised) included, is balanced by
        // the `OleUninitialize` in `drop`, on this thread, since the value
        // cannot leave it (an `App` is not `Send`).
        let result = unsafe { OleInitialize(ptr::null()) };
        Win32App { ole: result >= 0 }
    }

    /// Whether OLE is initialised for the thread by this application.
    pub(crate) fn has_ole(&self) -> bool {
        self.ole
    }

    /// Waits for the thread's next message and dispatches it. Returns the
    /// exit code when the message is `WM_QUIT`.
    pub(crate) fn next_message(&self) -> Result<Option<i32>, WindowError> {
        let mut message = MSG::default();
        // SAFETY: `message` is a valid place for a message of this thread.
        let got = unsafe { GetMessageW(&mut message, ptr::null_mut(), 0, 0) };
        match got {
            0 => Ok(Some(message.wParam as i32)),
            -1 => Err(last_error("GetMessageW")),
            _ => {
                // SAFETY: a message just taken from this thread's queue.
                unsafe {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
                Ok(None)
            }
        }
    }
}

impl Drop for Win32App {
    fn drop(&mut self) {
        if self.ole {
            // SAFETY: balances the successful `OleInitialize` of `new`, on
            // the same thread. The application's windows hold it alive, so
            // none of them is still registered for drops.
            unsafe {
                OleUninitialize();
            }
        }
    }
}
