//! The macOS view in a binary of its own, as a plugin has it.
//!
//! This builds a library. The macOS test (`tests/macos.rs`) loads two copies
//! of it into one process and opens a view with each, to check that two
//! binaries do not share the view's Objective-C class. On other systems it
//! is empty.

#[cfg(target_os = "macos")]
mod exports {
    use std::cell::Cell;
    use std::ffi::{CString, c_char, c_void};
    use std::ptr;
    use std::rc::Rc;

    use manguebyte_window::macos::testing;
    use manguebyte_window::{
        Event, Handled, ParentWindow, PhysicalSize, Window, WindowCx, WindowOptions,
    };

    struct Open {
        window: Window,
        presses: Rc<Cell<u32>>,
        class: CString,
    }

    /// Opens a view of `width` by `height` device pixels inside the view at
    /// `parent`, from the main thread. Returns null if it cannot.
    #[unsafe(no_mangle)]
    pub extern "C" fn mangue_view_open(parent: usize, width: u32, height: u32) -> *mut c_void {
        let presses = Rc::new(Cell::new(0));
        let seen = Rc::clone(&presses);
        let Some(parent) = ParentWindow::cocoa(parent) else {
            return ptr::null_mut();
        };
        let options = WindowOptions::new(PhysicalSize::new(width, height));
        let handler = move |_: &WindowCx<'_>, event: Event| {
            if matches!(event, Event::PointerDown { .. }) {
                seen.set(seen.get() + 1);
            }
            Handled::Yes
        };
        let Ok(window) = Window::open(parent, options, handler) else {
            return ptr::null_mut();
        };
        let class = testing::view_class_name(&window).and_then(|name| CString::new(name).ok());
        let Some(class) = class else {
            return ptr::null_mut();
        };
        Box::into_raw(Box::new(Open {
            window,
            presses,
            class,
        }))
        .cast()
    }

    /// The name of the view's class, for as long as the view is open.
    ///
    /// # Safety
    ///
    /// `open` must come from [`mangue_view_open`] of this library and not
    /// have been closed.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mangue_view_class(open: *const c_void) -> *const c_char {
        // SAFETY: the caller passes a live value this library made.
        let open = unsafe { &*open.cast::<Open>() };
        open.class.as_ptr()
    }

    /// How many presses the view's handler has seen, or `u32::MAX` if the
    /// window is closed.
    ///
    /// # Safety
    ///
    /// As for [`mangue_view_class`].
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mangue_view_presses(open: *const c_void) -> u32 {
        // SAFETY: the caller passes a live value this library made.
        let open = unsafe { &*open.cast::<Open>() };
        if open.window.is_open() {
            open.presses.get()
        } else {
            u32::MAX
        }
    }

    /// Closes the view and frees `open`.
    ///
    /// # Safety
    ///
    /// As for [`mangue_view_class`]; `open` is not used again.
    #[unsafe(no_mangle)]
    pub unsafe extern "C" fn mangue_view_close(open: *mut c_void) {
        // SAFETY: the caller passes a live value this library made and
        // gives it up.
        drop(unsafe { Box::from_raw(open.cast::<Open>()) });
    }
}
