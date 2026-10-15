//! Real windows for tests and probes: a top-level parent, reading pixels
//! back, sending input, running the message loop, counting system objects.
//!
//! Compiled only with the crate's `testing` feature, which no plugin should
//! turn on: a plugin never makes top-level windows of its own or runs a
//! message loop inside a host. It exists so that tests and the probe
//! example can check the backend against the system without any `unsafe`
//! of their own.
//!
//! ```
//! use manguebyte_gfx::{Color, IRect, Pixmap};
//! use manguebyte_window::win32::testing::{self, ReadBack, TopLevel};
//! use manguebyte_window::{Handled, PhysicalSize, Window, WindowOptions};
//!
//! let parent = TopLevel::new(64, 48).expect("a desktop");
//! let window = Window::open(
//!     parent.parent(),
//!     WindowOptions::new(PhysicalSize::new(32, 24)),
//!     |_: &manguebyte_window::WindowCx<'_>, _| Handled::Yes,
//! )
//! .expect("a child window");
//! let mut frame = Pixmap::new(32, 24).expect("small");
//! frame.fill(Color::rgb(10, 200, 30));
//! window.present_all(&frame).expect("open");
//!
//! // What the window shows, read from the screen; without a desktop to
//! // read, this panics unless MANGUEBYTE_WINDOW_SKIP_READ_BACK is set.
//! if testing::require_read_back(&parent, IRect::new(40, 30, 20, 16)) {
//!     let shown = testing::read_back(&window.cx(), ReadBack::Screen).expect("our window");
//!     assert_eq!(shown.pixel(31, 23).expect("inside").to_array(), [10, 200, 30, 255]);
//! }
//! drop(window);
//! assert!(!testing::class_registered());
//! ```

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem;
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use manguebyte_gfx::{IRect, Pixmap};
use windows_sys::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BitBlt, ClientToScreen, CreateCompatibleDC,
    CreateDIBSection, CreateSolidBrush, DIB_RGB_COLORS, DeleteDC, DeleteObject, FillRect, GdiFlush,
    GetDC, GetStockObject, HDC, ReleaseDC, SRCCOPY, SelectObject, UpdateWindow,
};
use windows_sys::Win32::Storage::Xps::{PW_CLIENTONLY, PrintWindow};
use windows_sys::Win32::System::Threading::{
    GR_GDIOBJECTS, GR_USEROBJECTS, GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId,
    GetGuiResources,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GW_CHILD, GW_HWNDNEXT,
    GetClassInfoExW, GetClassNameW, GetClientRect, GetDesktopWindow, GetParent, GetWindow,
    GetWindowThreadProcessId, IsWindow, MSG, PM_REMOVE, PeekMessageW, SW_SHOWNOACTIVATE,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SendMessageW, SetWindowPos, ShowWindow,
    TranslateMessage, WM_CAPTURECHANGED, WM_CHAR, WM_DPICHANGED_AFTERPARENT, WM_GETDLGCODE,
    WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER, WNDCLASSEXW, WS_CLIPCHILDREN, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

#[cfg(not(target_pointer_width = "64"))]
use windows_sys::Win32::UI::WindowsAndMessaging::{GCL_HMODULE, GetClassLongW};
#[cfg(target_pointer_width = "64")]
use windows_sys::Win32::UI::WindowsAndMessaging::{GCLP_HMODULE, GetClassLongPtrW};

use super::{
    STATE_BYTES, WM_MOUSELEAVE, check_parent, class_name, find_get_dpi, last_error, register_class,
    set_state_slot, state_slot, this_module, unregister_class,
};
use crate::convert;
use crate::{Button, ParentWindow, PhysicalPoint, PhysicalSize, WindowCx, WindowError};

/// A plain top-level window owned by a test, to embed windows in as a host
/// would. It has no frame, does not take the keyboard when shown, paints
/// itself black, and is destroyed when dropped.
///
/// It keeps the keys, characters and wheel turns that reach it, as a host
/// would see what its editor passes on, and can send each of them back to
/// another window, as a host that returns keys to the focused child does.
#[derive(Debug)]
pub struct TopLevel {
    hwnd: Cell<HWND>,
    module: HMODULE,
    class: Vec<u16>,
    state: Rc<ParentState>,
}

/// What the test parent's window procedure keeps.
#[derive(Debug, Default)]
struct ParentState {
    received: RefCell<Vec<Received>>,
    send_back_to: Cell<Option<usize>>,
}

/// A key, character or wheel message that reached a [`TopLevel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Received {
    /// The message: `WM_KEYDOWN`, `WM_CHAR`, `WM_MOUSEWHEEL` and so on.
    pub message: u32,
    /// Its `WPARAM`.
    pub wparam: usize,
    /// Its `LPARAM`.
    pub lparam: isize,
}

/// The messages a [`TopLevel`] keeps.
fn kept_by_parent(message: u32) -> bool {
    matches!(
        message,
        WM_KEYDOWN
            | WM_KEYUP
            | WM_SYSKEYDOWN
            | WM_SYSKEYUP
            | WM_CHAR
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    )
}

/// The test parent's window procedure.
unsafe extern "system" fn parent_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let data = state_slot(hwnd);
    if data != 0 && kept_by_parent(message) {
        let raw: *const ParentState = ptr::with_exposed_provenance(data as usize);
        // SAFETY: the slot holds `Rc::as_ptr` of the state the TopLevel
        // owns, set after creation and cleared before the window is
        // destroyed, so the state is alive; only shared references to it
        // are ever made.
        let state = unsafe { &*raw };
        state.received.borrow_mut().push(Received {
            message,
            wparam,
            lparam,
        });
        if let Some(to) = state.send_back_to.get() {
            let to: HWND = ptr::with_exposed_provenance_mut(to);
            // SAFETY: the same plain-number message, to a window the test
            // named; a window that is gone only fails.
            unsafe {
                SendMessageW(to, message, wparam, lparam);
            }
        }
    }
    // SAFETY: the system's own message, passed on unchanged.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

impl TopLevel {
    /// A shown window of `width` by `height` pixels at the top-left corner
    /// of the primary screen.
    ///
    /// # Errors
    ///
    /// [`WindowError::Platform`] if the system refuses, as it may without a
    /// desktop.
    pub fn new(width: u32, height: u32) -> Result<TopLevel, WindowError> {
        let module = this_module()?;
        let class = class_name("TestParent", module);
        // SAFETY: BLACK_BRUSH is a stock object; it needs no release.
        let black = unsafe { GetStockObject(BLACK_BRUSH) };
        register_class(module, &class, Some(parent_procedure), black, STATE_BYTES)?;
        // SAFETY: the class is registered for `module`; a top-level window
        // with no parent, menu or creation parameter.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                ptr::null(),
                WS_POPUP | WS_CLIPCHILDREN,
                0,
                0,
                width.min(16_384) as i32,
                height.min(16_384) as i32,
                ptr::null_mut(),
                ptr::null_mut(),
                module,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            let error = last_error("CreateWindowExW");
            unregister_class(module, &class);
            return Err(error);
        }
        let state = Rc::new(ParentState::default());
        set_state_slot(hwnd, Rc::as_ptr(&state).expose_provenance() as isize);
        // SAFETY: our own window, shown without activating it, then painted.
        unsafe {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            UpdateWindow(hwnd);
        }
        Ok(TopLevel {
            hwnd: Cell::new(hwnd),
            module,
            class,
            state,
        })
    }

    /// The keys, characters and wheel turns that reached the window since
    /// the last call, in order.
    pub fn take_received(&self) -> Vec<Received> {
        self.state.received.take()
    }

    /// Sends every key, character and wheel turn that reaches the window
    /// on to `handle` as well (or stops, with `None`), as a host that
    /// returns keys to its focused child does.
    pub fn send_back_to(&self, handle: Option<usize>) {
        self.state.send_back_to.set(handle);
    }

    /// The window as a parent for [`Window::open`](crate::Window::open).
    ///
    /// # Panics
    ///
    /// If the window was destroyed.
    pub fn parent(&self) -> ParentWindow {
        ParentWindow::win32(self.hwnd.get().addr()).expect("the top-level window was destroyed")
    }

    /// The `HWND`, or `None` once destroyed.
    pub fn handle(&self) -> Option<usize> {
        let hwnd = self.hwnd.get();
        (!hwnd.is_null()).then(|| hwnd.addr())
    }

    /// Whether the window still exists.
    pub fn is_open(&self) -> bool {
        let hwnd = self.hwnd.get();
        // SAFETY: IsWindow accepts any value.
        !hwnd.is_null() && unsafe { IsWindow(hwnd) } != 0
    }

    /// Paints `rect` of the window's client area in a solid colour with
    /// GDI, outside any child. Returns whether GDI accepted it.
    pub fn fill(&self, rect: IRect, rgb: [u8; 3]) -> bool {
        self.is_open() && fill_window(self.hwnd.get(), rect, rgb)
    }

    /// Reads `rect` of the window's client area through its device
    /// context, as premultiplied RGBA with alpha 255.
    ///
    /// # Errors
    ///
    /// [`WindowError::Closed`] once destroyed, or [`WindowError::Platform`].
    pub fn read_back(&self, rect: IRect) -> Result<Pixmap, WindowError> {
        if !self.is_open() {
            return Err(WindowError::Closed);
        }
        read_window_dc(self.hwnd.get(), rect)
    }

    /// Destroys the window, and with it every window inside it.
    pub fn destroy(&self) {
        let hwnd = self.hwnd.replace(ptr::null_mut());
        if !hwnd.is_null() {
            set_state_slot(hwnd, 0);
            // SAFETY: our own window, on the thread that made it (the value
            // is not `Send`, since it holds a raw handle).
            unsafe {
                DestroyWindow(hwnd);
            }
        }
        unregister_class(self.module, &self.class);
    }
}

impl Drop for TopLevel {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Paints `rect` of `hwnd`'s client area in a solid colour with GDI.
fn fill_window(hwnd: HWND, rect: IRect, rgb: [u8; 3]) -> bool {
    let r = to_rect(rect);
    let colour = u32::from(rgb[0]) | (u32::from(rgb[1]) << 8) | (u32::from(rgb[2]) << 16);
    // SAFETY: a DC of a live window and a brush of our own, both released
    // before returning.
    unsafe {
        let hdc = GetDC(hwnd);
        if hdc.is_null() {
            return false;
        }
        let brush = CreateSolidBrush(colour);
        let filled = !brush.is_null() && FillRect(hdc, &r, brush) != 0;
        if !brush.is_null() {
            DeleteObject(brush);
        }
        ReleaseDC(hwnd, hdc);
        filled
    }
}

/// Paints `rect` of one of this crate's windows in a solid colour with
/// GDI, behind the window's back, as another program drawing on its
/// surface would. Returns whether `handle` is such a window and GDI
/// accepted it. Read-back tests use it to show that a read-back reads the
/// surface and not the window's own back buffer.
pub fn draw_over(handle: usize, rect: IRect, rgb: [u8; 3]) -> bool {
    ours(handle).is_some_and(|hwnd| fill_window(hwnd, rect, rgb))
}

fn to_rect(rect: IRect) -> RECT {
    RECT {
        left: rect.x,
        top: rect.y,
        right: rect.right().clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        bottom: rect
            .bottom()
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
    }
}

/// The variable that lets a test skip the read-back checks in a session
/// that cannot read windows back. CI never sets it.
pub const SKIP_READ_BACK: &str = "MANGUEBYTE_WINDOW_SKIP_READ_BACK";

/// How to read a window's pixels.
///
/// Only [`Screen`](Self::Screen) reads what the window shows. In this
/// crate's tests on Windows 11, `PrintWindow` gave the window's back
/// buffer rather than the pixels on its surface, both with `PW_CLIENTONLY`
/// on the window and with `PW_RENDERFULLCONTENT` on its top-level window
/// (which also had some channels off by one), so it is offered only as a
/// check of the `WM_PRINTCLIENT` answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReadBack {
    /// Copy them from the window's device context with `BitBlt`: what is
    /// drawn on the window's surface, whoever drew it. Needs a desktop.
    Screen,
    /// Ask the window to draw itself with `PrintWindow` and
    /// `PW_CLIENTONLY`, which it answers with `WM_PRINTCLIENT` from its
    /// back buffer. This reads what the window means to show, not what it
    /// shows: it checks the `WM_PRINTCLIENT` answer, not presenting.
    PrintClient,
}

/// Whether `hwnd` is a live window of this crate's window class, made by
/// this thread.
fn ours(handle: usize) -> Option<HWND> {
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    if hwnd.is_null() || check_parent(hwnd).is_err() {
        return None;
    }
    let mut process = 0;
    // SAFETY: `hwnd` is a window; `process` is a valid place for its id.
    let thread = unsafe { GetWindowThreadProcessId(hwnd, &mut process) };
    // SAFETY: no arguments.
    if thread != unsafe { GetCurrentThreadId() } || process != unsafe { GetCurrentProcessId() } {
        return None;
    }
    let expected = class_name("Window", this_module().ok()?);
    let mut name = [0u16; 260];
    // SAFETY: `name` holds 260 units, which is what the call is told.
    let len = unsafe { GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32) };
    let len = usize::try_from(len).ok()?;
    (name[..len] == expected[..expected.len() - 1]).then_some(hwnd)
}

/// Reads the pixels of one of this crate's windows, by `method`, as
/// premultiplied RGBA with alpha 255 (GDI keeps no alpha).
///
/// # Errors
///
/// [`WindowError::Closed`] if the window is not open or not a window of
/// this crate on this thread, or [`WindowError::Platform`].
pub fn read_back(window: &WindowCx<'_>, method: ReadBack) -> Result<Pixmap, WindowError> {
    let hwnd = window
        .raw_handle()
        .and_then(ours)
        .ok_or(WindowError::Closed)?;
    let size = client_size(hwnd);
    match method {
        ReadBack::Screen => read_window_dc(hwnd, size.bounds()),
        ReadBack::PrintClient => with_bitmap(size.width, size.height, |memory| {
            // SAFETY: `hwnd` is our window and `memory` a memory DC with a
            // bitmap of the window's size selected.
            let ok = unsafe { PrintWindow(hwnd, memory, PW_CLIENTONLY) };
            if ok == 0 {
                Err(last_error("PrintWindow"))
            } else {
                Ok(())
            }
        }),
    }
}

fn client_size(hwnd: HWND) -> PhysicalSize {
    let mut r = RECT::default();
    // SAFETY: `r` is a valid place for the rectangle.
    unsafe {
        GetClientRect(hwnd, &mut r);
    }
    PhysicalSize::new(
        (i64::from(r.right) - i64::from(r.left)).max(0) as u32,
        (i64::from(r.bottom) - i64::from(r.top)).max(0) as u32,
    )
}

/// Copies `rect` of `hwnd`'s client area from its DC.
fn read_window_dc(hwnd: HWND, rect: IRect) -> Result<Pixmap, WindowError> {
    // SAFETY: a DC of a live window, released below.
    let source = unsafe { GetDC(hwnd) };
    if source.is_null() {
        return Err(last_error("GetDC"));
    }
    let result = with_bitmap(rect.width, rect.height, |memory| {
        // SAFETY: both DCs are valid; BitBlt clips to them.
        let ok = unsafe {
            BitBlt(
                memory,
                0,
                0,
                rect.width as i32,
                rect.height as i32,
                source,
                rect.x,
                rect.y,
                SRCCOPY,
            )
        };
        if ok == 0 {
            Err(last_error("BitBlt"))
        } else {
            Ok(())
        }
    });
    // SAFETY: releases the DC taken above.
    unsafe {
        ReleaseDC(hwnd, source);
    }
    result
}

/// Gives `draw` a memory DC with a `width` by `height` top-down 32-bit
/// bitmap selected, then reads the bitmap as opaque RGBA.
fn with_bitmap(
    width: u32,
    height: u32,
    draw: impl FnOnce(HDC) -> Result<(), WindowError>,
) -> Result<Pixmap, WindowError> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Pixmap::new(width.min(16_384), height.min(16_384)).ok_or(WindowError::Closed);
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width as i32,
            biHeight: -(height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    // SAFETY: a memory DC compatible with the screen, deleted below.
    let memory = unsafe { CreateCompatibleDC(ptr::null_mut()) };
    if memory.is_null() {
        return Err(last_error("CreateCompatibleDC"));
    }
    let mut bits: *mut c_void = ptr::null_mut();
    // SAFETY: `info` describes a 32-bit top-down bitmap; `bits` receives
    // its memory, owned by the bitmap until it is deleted below.
    let bitmap =
        unsafe { CreateDIBSection(memory, &info, DIB_RGB_COLORS, &mut bits, ptr::null_mut(), 0) };
    if bitmap.is_null() || bits.is_null() {
        let error = last_error("CreateDIBSection");
        // SAFETY: deletes the DC made above.
        unsafe {
            DeleteDC(memory);
        }
        return Err(error);
    }
    // SAFETY: selects our bitmap into our DC; the old object is put back
    // before either is deleted.
    let old = unsafe { SelectObject(memory, bitmap) };
    let drawn = draw(memory);
    // SAFETY: GDI may batch calls; this makes the bitmap's memory current.
    unsafe {
        GdiFlush();
    }
    let len = width as usize * height as usize * 4;
    // SAFETY: the DIB section holds `width * height` 32-bit pixels at
    // `bits`, alive until the bitmap is deleted below, and nothing writes
    // to it while this slice exists.
    let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), len) };
    let mut rgba = vec![0u8; len];
    for (from, to) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        to.copy_from_slice(&[from[2], from[1], from[0], 255]);
    }
    // SAFETY: puts the old object back, then deletes what we made.
    unsafe {
        SelectObject(memory, old);
        DeleteObject(bitmap);
        DeleteDC(memory);
    }
    drawn?;
    Pixmap::from_premultiplied(width, height, rgba).map_err(|_| WindowError::Closed)
}

/// A message a test sends to a window, carrying only numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// `WM_MOUSEMOVE` at a client position.
    MouseMove {
        /// Device position in the window.
        at: PhysicalPoint,
        /// `MK_SHIFT`.
        shift: bool,
        /// `MK_CONTROL`.
        control: bool,
    },
    /// `WM_LBUTTONDOWN`, `WM_RBUTTONDOWN` or `WM_MBUTTONDOWN`.
    ButtonDown {
        /// Which button.
        button: Button,
        /// Device position in the window.
        at: PhysicalPoint,
        /// `MK_SHIFT`.
        shift: bool,
        /// `MK_CONTROL`.
        control: bool,
    },
    /// `WM_LBUTTONUP`, `WM_RBUTTONUP` or `WM_MBUTTONUP`.
    ButtonUp {
        /// Which button.
        button: Button,
        /// Device position in the window.
        at: PhysicalPoint,
    },
    /// `WM_MOUSELEAVE`.
    MouseLeave,
    /// `WM_CAPTURECHANGED`, to no window.
    CaptureChanged,
    /// `WM_MOUSEWHEEL` or `WM_MOUSEHWHEEL`, with the pointer at a client
    /// position (sent in screen coordinates, as the system does).
    Wheel {
        /// Device position in the window.
        at: PhysicalPoint,
        /// The turn in `WHEEL_DELTA` units.
        delta: i16,
        /// `WM_MOUSEHWHEEL` instead of `WM_MOUSEWHEEL`.
        horizontal: bool,
    },
    /// `WM_KEYDOWN`, or `WM_SYSKEYDOWN` when `system`.
    KeyDown {
        /// The virtual-key code.
        vk: u16,
        /// The key was already down (bit 30 of `LPARAM`).
        repeat: bool,
        /// `WM_SYSKEYDOWN`.
        system: bool,
    },
    /// `WM_KEYUP`.
    KeyUp {
        /// The virtual-key code.
        vk: u16,
    },
    /// `WM_CHAR` with one UTF-16 unit.
    Char(u16),
    /// `WM_SETFOCUS`.
    SetFocus,
    /// `WM_KILLFOCUS`.
    KillFocus,
    /// `WM_TIMER` for a timer id.
    Timer(u32),
    /// `WM_DPICHANGED_AFTERPARENT`.
    DpiChangedAfterParent,
    /// `WM_GETDLGCODE` with no message, as a dialog asks.
    GetDlgCode,
    /// `WM_SYSKEYUP`.
    SysKeyUp {
        /// The virtual-key code.
        vk: u16,
    },
}

/// Sends `message` to the window `handle` with `SendMessageW` and returns
/// the window procedure's answer, or `None` if `handle` is not a live
/// window of this crate made by this thread.
///
/// It may be called from inside the window's own handler, which is how
/// tests show that such messages wait for the handler to return.
pub fn send(handle: usize, message: Message) -> Option<isize> {
    let hwnd = ours(handle)?;
    let flags = |shift: bool, control: bool| {
        (if shift { convert::MK_SHIFT } else { 0 })
            | (if control { convert::MK_CONTROL } else { 0 })
    };
    let (msg, wparam, lparam) = match message {
        Message::MouseMove { at, shift, control } => (
            WM_MOUSEMOVE,
            flags(shift, control),
            convert::lparam_from_point(at),
        ),
        Message::ButtonDown {
            button,
            at,
            shift,
            control,
        } => {
            let msg = match button {
                Button::Primary => WM_LBUTTONDOWN,
                Button::Secondary => WM_RBUTTONDOWN,
                Button::Middle => WM_MBUTTONDOWN,
            };
            let held = match button {
                Button::Primary => 0x0001,
                Button::Secondary => 0x0002,
                Button::Middle => 0x0010,
            };
            (
                msg,
                flags(shift, control) | held,
                convert::lparam_from_point(at),
            )
        }
        Message::ButtonUp { button, at } => {
            let msg = match button {
                Button::Primary => WM_LBUTTONUP,
                Button::Secondary => WM_RBUTTONUP,
                Button::Middle => WM_MBUTTONUP,
            };
            (msg, 0, convert::lparam_from_point(at))
        }
        Message::MouseLeave => (WM_MOUSELEAVE, 0, 0),
        Message::CaptureChanged => (WM_CAPTURECHANGED, 0, 0),
        Message::Wheel {
            at,
            delta,
            horizontal,
        } => {
            let mut point = POINT { x: at.x, y: at.y };
            // SAFETY: `hwnd` is our window; `point` a valid place.
            unsafe {
                ClientToScreen(hwnd, &mut point);
            }
            let msg = if horizontal {
                WM_MOUSEHWHEEL
            } else {
                WM_MOUSEWHEEL
            };
            let wparam = usize::from(delta as u16) << 16;
            (
                msg,
                wparam,
                convert::lparam_from_point(PhysicalPoint::new(point.x, point.y)),
            )
        }
        Message::KeyDown { vk, repeat, system } => {
            let lparam = 1 | if repeat { 1 << 30 } else { 0 };
            (
                if system { WM_SYSKEYDOWN } else { WM_KEYDOWN },
                usize::from(vk),
                lparam,
            )
        }
        Message::KeyUp { vk } => (
            WM_KEYUP,
            usize::from(vk),
            (1usize | (3usize << 30)) as isize,
        ),
        Message::Char(unit) => (WM_CHAR, usize::from(unit), 1),
        Message::SetFocus => (WM_SETFOCUS, 0, 0),
        Message::KillFocus => (WM_KILLFOCUS, 0, 0),
        Message::Timer(id) => (WM_TIMER, id as usize, 0),
        Message::DpiChangedAfterParent => (WM_DPICHANGED_AFTERPARENT, 0, 0),
        Message::GetDlgCode => (WM_GETDLGCODE, 0, 0),
        Message::SysKeyUp { vk } => (
            WM_SYSKEYUP,
            usize::from(vk),
            (1usize | (3usize << 30)) as isize,
        ),
    };
    // SAFETY: every message here carries plain numbers (no pointers), and
    // goes to a window of our own class whose procedure reads them as such.
    Some(unsafe { SendMessageW(hwnd, msg, wparam, lparam) })
}

/// Resizes one of this crate's windows from outside, as a host does with
/// `SetWindowPos`. Returns whether the system accepted it.
pub fn resize_as_host(handle: usize, size: PhysicalSize) -> bool {
    let Some(hwnd) = ours(handle) else {
        return false;
    };
    // SAFETY: our own window; the messages this sends go to its procedure.
    unsafe {
        SetWindowPos(
            hwnd,
            ptr::null_mut(),
            0,
            0,
            size.width.min(16_384) as i32,
            size.height.min(16_384) as i32,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        ) != 0
    }
}

/// Has the system paint the window now if part of it needs it
/// (`UpdateWindow`). Returns whether `handle` is one of this crate's
/// windows.
pub fn update(handle: usize) -> bool {
    let Some(hwnd) = ours(handle) else {
        return false;
    };
    // SAFETY: our own window; WM_PAINT goes to its procedure.
    unsafe {
        UpdateWindow(hwnd);
    }
    true
}

/// Gives the window the keyboard with `SetFocus`.
pub fn focus(handle: usize) -> bool {
    let Some(hwnd) = ours(handle) else {
        return false;
    };
    // SAFETY: our own window, on its own thread.
    unsafe {
        SetFocus(hwnd);
    }
    true
}

/// Whether the system says `handle` has the keyboard.
pub fn has_system_focus(handle: usize) -> bool {
    // SAFETY: no arguments.
    let focus = unsafe { GetFocus() };
    !focus.is_null() && focus.addr() == handle
}

/// Whether the system says `handle` holds the pointer.
pub fn has_system_capture(handle: usize) -> bool {
    // SAFETY: no arguments.
    let capture = unsafe { GetCapture() };
    !capture.is_null() && capture.addr() == handle
}

/// The parent the system has for `handle`, if any.
pub fn parent_of(handle: usize) -> Option<usize> {
    let hwnd = ours(handle)?;
    // SAFETY: our own window.
    let parent = unsafe { GetParent(hwnd) };
    (!parent.is_null()).then(|| parent.addr())
}

/// The desktop window: a window of another process, for tests that a
/// window refuses a parent it may not embed into.
pub fn desktop_window() -> usize {
    // SAFETY: no arguments.
    unsafe { GetDesktopWindow() }.addr()
}

/// Whether `handle` is a live window of any kind.
pub fn is_window(handle: usize) -> bool {
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    // SAFETY: IsWindow accepts any value.
    unsafe { IsWindow(hwnd) != 0 }
}

/// Runs this thread's message loop for `duration`.
pub fn pump(duration: Duration) {
    let end = Instant::now() + duration;
    pump_until(duration, || Instant::now() >= end);
}

/// Runs this thread's message loop until `done` returns true or `timeout`
/// passes. Returns whether `done` did.
pub fn pump_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + timeout;
    loop {
        let mut message = MSG::default();
        // SAFETY: `message` is a valid place for a message of this thread.
        while unsafe { PeekMessageW(&mut message, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            // SAFETY: a message just taken from this thread's queue.
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if done() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The GDI and USER objects this process holds, from `GetGuiResources`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GuiResources {
    /// GDI objects: DCs, bitmaps, brushes, regions.
    pub gdi: u32,
    /// USER objects: windows, menus, cursors, hooks.
    pub user: u32,
}

/// What `GetGuiResources` says for this process now.
pub fn gui_resources() -> GuiResources {
    // SAFETY: the pseudo-handle of this process needs no closing.
    unsafe {
        let process = GetCurrentProcess();
        GuiResources {
            gdi: GetGuiResources(process, GR_GDIOBJECTS),
            user: GetGuiResources(process, GR_USEROBJECTS),
        }
    }
}

/// The name of this binary's window class.
pub fn window_class_name() -> String {
    let module = this_module().unwrap_or(ptr::null_mut());
    let name = class_name("Window", module);
    String::from_utf16_lossy(&name[..name.len() - 1])
}

/// Whether this binary's window class is registered now.
pub fn class_registered() -> bool {
    let Ok(module) = this_module() else {
        return false;
    };
    let name = class_name("Window", module);
    let mut info = WNDCLASSEXW {
        cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
        ..WNDCLASSEXW::default()
    };
    // SAFETY: the name is NUL-terminated and `info` is a valid place with
    // its size set.
    unsafe { GetClassInfoExW(module, name.as_ptr(), &mut info) != 0 }
}

/// Whether this session can read windows back at all: paints a pattern of
/// colours on `parent` outside its children and reads it through the same
/// path as [`ReadBack::Screen`]. `Err` says why not.
///
/// # Errors
///
/// A description of what came back instead, when reading back does not
/// work here (no desktop, a locked session, a colour depth below 24 bits).
pub fn can_read_back(parent: &TopLevel, area: IRect) -> Result<(), String> {
    let colours = [[17, 99, 201], [250, 3, 128], [0, 255, 7], [77, 77, 76]];
    let quarter = area.width / 2;
    let half = area.height / 2;
    let cells = [
        IRect::new(area.x, area.y, quarter, half),
        IRect::new(area.x + quarter as i32, area.y, area.width - quarter, half),
        IRect::new(area.x, area.y + half as i32, quarter, area.height - half),
        IRect::new(
            area.x + quarter as i32,
            area.y + half as i32,
            area.width - quarter,
            area.height - half,
        ),
    ];
    for (cell, colour) in cells.iter().zip(colours) {
        if !parent.fill(*cell, colour) {
            return Err("GDI refused to paint the parent".into());
        }
    }
    let back = parent
        .read_back(area)
        .map_err(|error| format!("reading the parent failed: {error}"))?;
    for (cell, colour) in cells.iter().zip(colours) {
        let x = (cell.x - area.x) as u32 + cell.width / 2;
        let y = (cell.y - area.y) as u32 + cell.height / 2;
        let got = back
            .pixel(x, y)
            .map(|pixel| pixel.to_array())
            .unwrap_or_default();
        let want = [colour[0], colour[1], colour[2], 255];
        if got != want {
            return Err(format!(
                "painted {want:?} at ({x}, {y}) of the parent and read back {got:?}"
            ));
        }
    }
    Ok(())
}

/// Whether a test may go on to read windows back: true when
/// [`can_read_back`] says it works.
///
/// When it does not, this panics with the reason, so that a read-back
/// test fails rather than passes without checking a pixel, unless the
/// variable [`SKIP_READ_BACK`] is set: then it says on stderr that the
/// test skipped its read-back checks, and returns false.
///
/// # Panics
///
/// When reading back does not work here and [`SKIP_READ_BACK`] is not set.
pub fn require_read_back(parent: &TopLevel, area: IRect) -> bool {
    match can_read_back(parent, area) {
        Ok(()) => true,
        Err(why) if std::env::var_os(SKIP_READ_BACK).is_some() => {
            eprintln!("SKIPPED the read-back checks ({SKIP_READ_BACK} is set): {why}");
            false
        }
        Err(why) => panic!(
            "this session cannot read windows back ({why}); \
             set {SKIP_READ_BACK}=1 to skip the read-back checks"
        ),
    }
}

/// `SetThreadDpiAwarenessContext`, found at run time.
type SetThreadDpiFn = unsafe extern "system" fn(isize) -> isize;

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`.
const PER_MONITOR_AWARE_V2: isize = -4;

/// Makes this thread, and only this thread, per-monitor DPI aware (v2)
/// until dropped, so that windows it makes see the monitor's real DPI.
/// The process's awareness does not change.
#[derive(Debug)]
pub struct PerMonitorDpi {
    previous: isize,
    set: SetThreadDpiFn,
}

impl PerMonitorDpi {
    /// Switches this thread, or gives `None` on a system without
    /// per-monitor awareness v2 (before Windows 10 1703).
    pub fn enter() -> Option<PerMonitorDpi> {
        let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
        // SAFETY: the name is NUL-terminated.
        let module =
            unsafe { windows_sys::Win32::System::LibraryLoader::GetModuleHandleW(user32.as_ptr()) };
        if module.is_null() {
            return None;
        }
        // SAFETY: the procedure name is a NUL-terminated ASCII string.
        let found = unsafe {
            windows_sys::Win32::System::LibraryLoader::GetProcAddress(
                module,
                c"SetThreadDpiAwarenessContext".as_ptr().cast(),
            )
        }?;
        // SAFETY: SetThreadDpiAwarenessContext takes and returns a
        // DPI_AWARENESS_CONTEXT, a pointer-sized handle, with the system
        // calling convention, which is this type.
        let set = unsafe {
            mem::transmute::<unsafe extern "system" fn() -> isize, SetThreadDpiFn>(found)
        };
        // SAFETY: a documented context value; it changes this thread only.
        let previous = unsafe { set(PER_MONITOR_AWARE_V2) };
        (previous != 0).then_some(PerMonitorDpi { previous, set })
    }
}

impl Drop for PerMonitorDpi {
    fn drop(&mut self) {
        // SAFETY: the context this thread had before.
        unsafe {
            (self.set)(self.previous);
        }
    }
}

/// The DPI the system gives the window `handle` (`GetDpiForWindow`), or
/// `None` where the system lacks the call or `handle` is no window.
pub fn window_dpi(handle: usize) -> Option<u32> {
    let get = find_get_dpi()?;
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    // SAFETY: the function takes any window handle; a stale one gives 0.
    let dpi = unsafe { get(hwnd) };
    (dpi != 0).then_some(dpi)
}

/// A window class as the system keys it: its name and the module that
/// registered it. Two binaries may register classes of the same name;
/// the module tells them apart.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WindowClass {
    /// The class name.
    pub name: String,
    /// The `HMODULE` that registered it, as a number. It stays meaningful
    /// as a key after the module is unloaded: a class still registered
    /// under it then is one that leaked.
    pub module: usize,
}

/// The windows directly inside `parent`, in the system's order, whoever
/// made them. Empty when `parent` is no window or has none.
pub fn children_of(parent: usize) -> Vec<usize> {
    let parent: HWND = ptr::with_exposed_provenance_mut(parent);
    let mut children = Vec::new();
    // SAFETY: IsWindow accepts any value.
    if parent.is_null() || unsafe { IsWindow(parent) } == 0 {
        return children;
    }
    // SAFETY: `parent` is a window; GetWindow returns null when there is
    // none to return.
    let mut child = unsafe { GetWindow(parent, GW_CHILD) };
    while !child.is_null() {
        children.push(child.addr());
        // SAFETY: `child` was just returned as a window.
        child = unsafe { GetWindow(child, GW_HWNDNEXT) };
    }
    children
}

/// The class of the window `handle`, whoever made it, or `None` when
/// `handle` is no window.
pub fn class_of(handle: usize) -> Option<WindowClass> {
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    // SAFETY: IsWindow accepts any value.
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        return None;
    }
    let mut name = [0u16; 260];
    // SAFETY: `name` holds 260 units, which is what the call is told.
    let len = unsafe { GetClassNameW(hwnd, name.as_mut_ptr(), name.len() as i32) };
    if len <= 0 {
        return None;
    }
    // SAFETY: `hwnd` is a window; the index is the class's module.
    #[cfg(target_pointer_width = "64")]
    let module = unsafe { GetClassLongPtrW(hwnd, GCLP_HMODULE) };
    // SAFETY: as above, on a system where the value fits 32 bits.
    #[cfg(not(target_pointer_width = "64"))]
    let module = unsafe { GetClassLongW(hwnd, GCL_HMODULE) } as usize;
    Some(WindowClass {
        name: String::from_utf16_lossy(&name[..len as usize]),
        module,
    })
}

/// Whether `class` is registered now.
pub fn class_exists(class: &WindowClass) -> bool {
    let mut name: Vec<u16> = class.name.encode_utf16().collect();
    name.push(0);
    let module: HMODULE = ptr::with_exposed_provenance_mut(class.module);
    let mut info = WNDCLASSEXW {
        cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
        ..WNDCLASSEXW::default()
    };
    // SAFETY: the name is NUL-terminated and `info` is a valid place with
    // its size set. GetClassInfoExW only compares `module` as a key; it
    // does not read through it, so a module unloaded since is fine.
    unsafe { GetClassInfoExW(module, name.as_ptr(), &mut info) != 0 }
}

/// The parent the system has for the window `handle`, whoever made it, or
/// `None` when it has none or is no window. [`parent_of`] is the same for
/// this crate's windows only.
pub fn parent_of_any(handle: usize) -> Option<usize> {
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    // SAFETY: IsWindow accepts any value.
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        return None;
    }
    // SAFETY: `hwnd` is a window.
    let parent = unsafe { GetParent(hwnd) };
    (!parent.is_null()).then(|| parent.addr())
}

/// The size of the client area of the window `handle`, whoever made it,
/// in pixels, or `None` when it is no window.
pub fn client_size_of(handle: usize) -> Option<(u32, u32)> {
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    // SAFETY: IsWindow accepts any value.
    if hwnd.is_null() || unsafe { IsWindow(hwnd) } == 0 {
        return None;
    }
    let mut rect = RECT::default();
    // SAFETY: `hwnd` is a window and `rect` a valid place for its client
    // rectangle.
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0 {
        return None;
    }
    Some((
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}

/// Where the client area of the child window `handle` starts, in its
/// parent's client coordinates, or `None` when it is no child window.
pub fn position_in_parent(handle: usize) -> Option<(i32, i32)> {
    let parent = parent_of_any(handle)?;
    let hwnd: HWND = ptr::with_exposed_provenance_mut(handle);
    let parent: HWND = ptr::with_exposed_provenance_mut(parent);
    let mut child = POINT { x: 0, y: 0 };
    let mut origin = POINT { x: 0, y: 0 };
    // SAFETY: both are windows; the points are valid places.
    let ok = unsafe {
        ClientToScreen(hwnd, &mut child) != 0 && ClientToScreen(parent, &mut origin) != 0
    };
    ok.then(|| (child.x - origin.x, child.y - origin.y))
}
