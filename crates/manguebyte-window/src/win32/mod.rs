//! The Win32 backend.
//!
//! A plugin's window here is a `WS_CHILD` window inside the host's `HWND`,
//! made on the host's main thread and driven by the host's message loop:
//! for it the backend never runs a loop of its own. An application's
//! window ([`App::open_window`](crate::App::open_window)) is an overlapped
//! top-level window of the same class, run by [`App::run`](crate::App::run),
//! which is the thread's `GetMessageW` loop. (The `testing` module,
//! compiled only with the crate's `testing` feature, runs loops for tests
//! and probes.)
//!
//! What a top-level window adds:
//!
//! - It is made hidden at a first guess of its size, then given the inside
//!   its logical size needs at the scale of the monitor it landed on
//!   (`AdjustWindowRectExForDpi`, looked up at run time, with
//!   `AdjustWindowRectEx` as the fallback), and only then shown; the
//!   handler hears nothing of the sizes it went through.
//! - `WM_CLOSE` becomes [`Event::CloseRequested`]
//!   and nothing else: the application closes the window, or not.
//! - `WM_GETMINMAXINFO` gives the system the frame's limits, in outer
//!   pixels at the window's DPI; `WM_DPICHANGED` changes the scale first
//!   and then takes the suggested rectangle, its inside kept within the
//!   limits; a minimised window keeps its size and back buffer.
//! - A key, character or wheel turn the handler does not use goes to the
//!   system's default handling (`DefWindowProcW`), not to a parent, so
//!   Alt+F4, Alt and F10 work as in any window.
//!
//! Drops ([`WindowOptions::with_file_drops`](crate::WindowOptions::with_file_drops)):
//! a window that asks registers an `IDropTarget` of its own with
//! `RegisterDragDrop` when it is made, only on a thread that is a
//! single-threaded COM apartment, and revokes it in `WM_DESTROY`. An
//! [`App`](crate::App) initialises OLE for its thread; a child window never
//! does, so in a host whose main thread has no OLE it takes no drops. The
//! file dialogs are `IFileOpenDialog` and `IFileSaveDialog`, shown modal to
//! the window's top-level window; on a thread without COM they initialise
//! it for the length of the call only.
//!
//! The parent must be a window of this process. A child of another
//! process's window shares that process's input queue, so either process
//! hanging hangs the other; the backend refuses such a parent with
//! [`WindowError::InvalidParent`] rather than embed across processes.
//!
//! What it does to the process, and what it does not:
//!
//! - Its window class is named after this binary's module handle and the
//!   address of a static of this copy of the crate, so two plugins, or two
//!   copies of the framework in one plugin, never share a class. The class
//!   is registered with the binary's own module handle when a window is
//!   made and unregistered when the binary's last window is destroyed; the
//!   system counts the windows, so the crate keeps no count of its own. A
//!   class of that name with another window procedure is refused. The
//!   window keeps its state in the class's own extra bytes, never in
//!   `GWLP_USERDATA`, which anything in the process may write.
//! - No hooks, no threads, no mutable statics, no change to the process's
//!   DPI awareness or any other process-wide setting. The scale comes from
//!   `GetDpiForWindow` when the system has it (looked up at run time) and
//!   from the window's device context otherwise.
//! - Presenting converts the damaged rectangles of the pixmap into a
//!   blue-green-red back buffer and copies them with `SetDIBitsToDevice`;
//!   `WM_PAINT` and `WM_PRINTCLIENT` are answered from the back buffer.
//!   `WM_PAINT` validates the update region before the handler is asked,
//!   so a rectangle the handler invalidates while it paints is painted
//!   again.
//! - The pointer is captured from the first button down to the last button
//!   up. Wheel turns arrive in lines. Keys carry their modifiers.
//! - A key (`WM_KEYDOWN`, `WM_KEYUP`, `WM_SYSKEYDOWN`, `WM_SYSKEYUP`),
//!   typed character (`WM_CHAR`) or wheel turn the handler does not use is
//!   sent on to the parent window with the same parameters, once the
//!   handler has answered it, even if it had to wait. A control character
//!   (`WM_CHAR` for Enter, Tab and the like) makes no text event and goes
//!   on when the key that typed it did. Any of these that comes back to
//!   the window while it is passing a message on is dropped, so a parent
//!   that returns it cannot start a loop.
//! - `WM_GETDLGCODE` asks for arrow keys and characters, and for Tab,
//!   Enter and Escape only while the window
//!   [wants all keys](crate::Window::set_wants_all_keys), so a host's
//!   dialog keeps them otherwise.
//! - Input methods are left to the system: `DefWindowProcW` shows the
//!   composition in the system's own window and, as the system documents,
//!   turns what it produces into `WM_CHAR`, which arrives as text. (No
//!   test has run a real input method yet.) While text is typed the window
//!   keeps a hidden system caret ([`set_text_caret`](crate::Window::set_text_caret))
//!   at the interface's caret, made when it has the keyboard and destroyed
//!   when it loses it, as the system's convention is (the caret belongs to
//!   the thread), and asks the input method (`ImmSetCompositionWindow`,
//!   `ImmSetCandidateWindow`) to compose at the caret and list its
//!   candidates below it.
//! - The clipboard ([`Clipboard`](crate::Clipboard)) is opened for each
//!   read or write with the window as its owner and closed before the call
//!   returns, in the Unicode text format. Opening is tried four times,
//!   2 ms apart, while another program holds it.
//! - Timers are `SetTimer` timers on the window's own `HWND`, so they end
//!   with it.
//! - Nothing unwinds into the host: a panic in the window procedure is
//!   caught there.
//!
//! All of the crate's `unsafe` code is in this module.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem;
use std::num::NonZeroUsize;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;

use manguebyte_gfx::IRect;
use windows_sys::Win32::Foundation::{
    ERROR_CANNOT_FIND_WND_CLASS, ERROR_CLASS_ALREADY_EXISTS, GetLastError, GlobalFree, HINSTANCE,
    HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, GetDC,
    GetDeviceCaps, HBRUSH, HDC, InvalidateRect, LOGPIXELSX, PAINTSTRUCT, ReleaseDC, ScreenToClient,
    SetDIBitsToDevice,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, ImmGetContext, ImmReleaseContext,
    ImmSetCandidateWindow, ImmSetCompositionWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetCapture, GetDoubleClickTime, GetKeyState, MAPVK_VK_TO_CHAR, MapVirtualKeyW, ReleaseCapture,
    SetCapture, SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_LWIN,
    VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, CREATESTRUCTW, CW_USEDEFAULT, CreateCaret, CreateWindowExW,
    DLGC_WANTALLKEYS, DLGC_WANTARROWS, DLGC_WANTCHARS, DLGC_WANTTAB, DefWindowProcW, DestroyCaret,
    DestroyWindow, GetClassInfoExW, GetClientRect, GetParent, GetSystemMetrics,
    GetWindowThreadProcessId, IDC_ARROW, IsWindow, KillTimer, LoadCursorW, MINMAXINFO,
    RegisterClassExW, SIZE_MINIMIZED, SM_CXDOUBLECLK, SM_CYDOUBLECLK, SPI_GETCLIENTAREAANIMATION,
    SW_HIDE, SW_SHOW, SW_SHOWNA, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER,
    SendMessageW, SetCaretPos, SetTimer, SetWindowPos, SetWindowTextW, ShowWindow,
    SystemParametersInfoW, USER_TIMER_MINIMUM, UnregisterClassW, WM_CAPTURECHANGED, WM_CHAR,
    WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_DPICHANGED_AFTERPARENT, WM_ERASEBKGND, WM_GETDLGCODE,
    WM_GETMINMAXINFO, WM_KEYDOWN, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_PRINTCLIENT, WM_RBUTTONDBLCLK,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETFOCUS, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_TIMER,
    WNDCLASSEXW, WNDPROC, WS_CAPTION, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_MINIMIZEBOX,
    WS_OVERLAPPED, WS_OVERLAPPEDWINDOW, WS_SYSMENU, WS_VISIBLE,
};

use crate::app::{AppInner, AppWindowOptions, Frame, device_size};
use crate::convert;
use crate::dialog::{DialogKind, FileDialog};
use crate::dispatch::{Origin, WindowHandler, drop_quietly};
use crate::event::{Button, Event, Handled, KeyEvent, Modifiers, PhysicalSize, RawInput, TimerId};
use crate::pixels::BackBuffer;
use crate::pointer::ClickSettings;
use crate::window::{Inner, Platform, WindowError, WindowOptions};

pub(crate) mod app;
mod com;
mod dialog;
mod drop;
#[cfg(feature = "testing")]
pub mod testing;

/// `WM_MOUSELEAVE`, which windows-sys keeps with the common controls.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// A static that never changes; its address names this copy of the crate
/// in window class names.
static ANCHOR: u8 = 0;

/// `GetDpiForWindow`, found at run time because older systems lack it.
type GetDpiForWindowFn = unsafe extern "system" fn(HWND) -> u32;

/// `AdjustWindowRectExForDpi`, found at run time for the same reason.
type AdjustForDpiFn = unsafe extern "system" fn(*mut RECT, u32, i32, u32, u32) -> i32;

/// What only a top-level window of an application has.
struct TopLevel {
    style: u32,
    ex_style: u32,
    adjust: Option<AdjustForDpiFn>,
    /// Whether showing the window activates it.
    activate: bool,
}

/// The Win32 side of a window.
pub(crate) struct Win32Platform {
    /// `Some` for an application's top-level window.
    top_level: Option<TopLevel>,
    /// The drop target the window registered, until `WM_DESTROY`.
    drops: RefCell<Option<drop::Registration>>,
    /// The window, until it is destroyed.
    hwnd: Cell<HWND>,
    module: HINSTANCE,
    /// The class name, NUL-terminated.
    class: Vec<u16>,
    get_dpi: Option<GetDpiForWindowFn>,
    /// Set when the window procedure takes its reference to the shared
    /// state, in `WM_NCCREATE`.
    adopted: Cell<bool>,
    /// Set when `CreateWindowExW` returned the window, so that a window
    /// that fails half-way through creation does not report `Closed`.
    created: Cell<bool>,
    /// Set while a message the handler declined is being sent to the
    /// parent.
    passing: Cell<bool>,
    /// Whether the last key down went on to the parent: the control
    /// character it typed, which makes no event, follows it.
    key_passed: Cell<bool>,
    /// The system caret this window made, and where it put it.
    caret: Cell<Option<IRect>>,
}

/// The clipboard's Unicode text format, `CF_UNICODETEXT`.
const CF_UNICODETEXT: u32 = 13;

/// How often the clipboard is tried, a couple of milliseconds apart, when
/// another program holds it open, as clipboard managers do for a moment
/// after every copy.
const CLIPBOARD_TRIES: u32 = 4;

/// The clipboard, opened by a window of ours, closed when dropped.
struct OpenClipboardGuard;

impl OpenClipboardGuard {
    fn open(hwnd: HWND) -> Result<OpenClipboardGuard, WindowError> {
        for attempt in 0..CLIPBOARD_TRIES {
            // SAFETY: our own window; opening only fails while another
            // window has the clipboard open.
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Ok(OpenClipboardGuard);
            }
            if attempt + 1 < CLIPBOARD_TRIES {
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
        Err(last_error("OpenClipboard"))
    }
}

impl Drop for OpenClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard and closes it once.
        unsafe {
            CloseClipboard();
        }
    }
}

/// The address of this copy of the crate's static.
fn anchor() -> usize {
    (&raw const ANCHOR).addr()
}

/// The module handle of the binary this code is in: the plugin's DLL in a
/// plugin, the program in a program.
pub(crate) fn this_module() -> Result<HMODULE, WindowError> {
    let mut module: HMODULE = ptr::null_mut();
    // SAFETY: FROM_ADDRESS takes any address inside a loaded module, and
    // ANCHOR is inside this one; UNCHANGED_REFCOUNT means there is nothing
    // to release. `module` is a valid place for the result.
    let ok = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            (&raw const ANCHOR).cast::<u16>(),
            &mut module,
        )
    };
    if ok == 0 {
        Err(last_error("GetModuleHandleExW"))
    } else {
        Ok(module)
    }
}

/// The class name for windows of `kind` made by this binary.
pub(crate) fn class_name(kind: &str, module: HMODULE) -> Vec<u16> {
    convert::class_name(kind, module.addr(), anchor())
}

/// The last system error, for `call`.
pub(crate) fn last_error(call: &'static str) -> WindowError {
    // SAFETY: reads the calling thread's last-error value; no arguments.
    let code = unsafe { GetLastError() };
    WindowError::Platform { call, code }
}

/// Registers the class `name` for `module` with `procedure` and
/// `extra` window bytes, unless it already is. A class of that name that
/// someone else registered with another procedure, or with fewer extra
/// bytes, is refused with `ERROR_CLASS_ALREADY_EXISTS`: its windows would
/// not run our code, or would have no room for the state slot.
pub(crate) fn register_class(
    module: HMODULE,
    name: &[u16],
    procedure: WNDPROC,
    background: HBRUSH,
    extra: i32,
) -> Result<(), WindowError> {
    debug_assert_eq!(name.last(), Some(&0));
    // SAFETY: IDC_ARROW is a system cursor, loaded with a null module.
    let cursor = unsafe { LoadCursorW(ptr::null_mut(), IDC_ARROW) };
    let class = WNDCLASSEXW {
        cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: procedure,
        cbClsExtra: 0,
        cbWndExtra: extra,
        hInstance: module,
        hIcon: ptr::null_mut(),
        hCursor: cursor,
        hbrBackground: background,
        lpszMenuName: ptr::null(),
        lpszClassName: name.as_ptr(),
        hIconSm: ptr::null_mut(),
    };
    // SAFETY: `class` is fully initialised and its name is NUL-terminated
    // and outlives the call (the system copies it).
    if unsafe { RegisterClassExW(&class) } != 0 {
        return Ok(());
    }
    let error = last_error("RegisterClassExW");
    let exists = WindowError::Platform {
        call: "RegisterClassExW",
        code: ERROR_CLASS_ALREADY_EXISTS,
    };
    if error != exists {
        return Err(error);
    }
    let mut info = WNDCLASSEXW {
        cbSize: mem::size_of::<WNDCLASSEXW>() as u32,
        ..WNDCLASSEXW::default()
    };
    // SAFETY: the name is NUL-terminated and `info` is a valid place with
    // its size set.
    if unsafe { GetClassInfoExW(module, name.as_ptr(), &mut info) } == 0 {
        // It went between the two calls: making the window then fails for
        // want of the class, and the caller registers again.
        return Ok(());
    }
    let address = |procedure: WNDPROC| procedure.map(|f| f as usize);
    if address(info.lpfnWndProc) == address(procedure) && info.cbWndExtra >= extra {
        Ok(())
    } else {
        Err(exists)
    }
}

/// Unregisters the class `name` of `module` if no window of it is left.
/// Fails quietly while windows remain: the system counts them.
pub(crate) fn unregister_class(module: HMODULE, name: &[u16]) {
    // SAFETY: the name is NUL-terminated; unregistering a class that has
    // windows, or none, only fails.
    unsafe {
        UnregisterClassW(name.as_ptr(), module);
    }
}

/// Whether `hwnd` is a window of this process.
pub(crate) fn check_parent(hwnd: HWND) -> Result<(), WindowError> {
    // SAFETY: IsWindow accepts any value.
    if unsafe { IsWindow(hwnd) } == 0 {
        return Err(WindowError::InvalidParent("not a window"));
    }
    let mut process = 0;
    // SAFETY: `hwnd` is a window, and `process` a valid place for its
    // process id.
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut process);
    }
    // SAFETY: no arguments.
    if process != unsafe { GetCurrentProcessId() } {
        return Err(WindowError::InvalidParent(
            "the window belongs to another process",
        ));
    }
    Ok(())
}

/// Whether the system lets programs animate their windows' contents: the
/// "Animation effects" setting (Settings, Accessibility, Visual effects),
/// read as `SPI_GETCLIENTAREAANIMATION`. True if it cannot be read.
pub(crate) fn client_area_animation() -> bool {
    let mut enabled: i32 = 1;
    // SAFETY: SPI_GETCLIENTAREAANIMATION writes one BOOL through the
    // pointer, which points at a live i32 for the length of the call; no
    // flag asks the system to write or broadcast the setting.
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            (&raw mut enabled).cast::<c_void>(),
            0,
        )
    };
    read == 0 || enabled != 0
}

/// Looks up `GetDpiForWindow` in user32, which is loaded in every process
/// that has windows.
pub(crate) fn find_get_dpi() -> Option<GetDpiForWindowFn> {
    let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
    // SAFETY: the name is NUL-terminated; a module that is not loaded
    // gives null.
    let module = unsafe { GetModuleHandleW(user32.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: the procedure name is a NUL-terminated ASCII string.
    let found = unsafe { GetProcAddress(module, c"GetDpiForWindow".as_ptr().cast()) }?;
    // SAFETY: GetDpiForWindow is documented as `UINT GetDpiForWindow(HWND)`
    // with the system calling convention, which is this type.
    Some(unsafe {
        mem::transmute::<unsafe extern "system" fn() -> isize, GetDpiForWindowFn>(found)
    })
}

/// The window extra bytes a class of this crate reserves: one
/// pointer-sized slot at offset 0 for the window's state. Unlike
/// `GWLP_USERDATA`, these belong to the class, so only code that knows the
/// class writes them.
pub(crate) const STATE_BYTES: i32 = mem::size_of::<isize>() as i32;

/// The offset of the state slot in the window extra bytes.
const STATE_SLOT: i32 = 0;

#[cfg(target_pointer_width = "64")]
pub(crate) fn state_slot(hwnd: HWND) -> isize {
    // SAFETY: the class of every window this is called with reserves
    // STATE_BYTES extra bytes, so offset 0 is inside them; a stale handle
    // only fails.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(hwnd, STATE_SLOT) }
}

#[cfg(target_pointer_width = "64")]
pub(crate) fn set_state_slot(hwnd: HWND, value: isize) {
    // SAFETY: as for `state_slot`.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(hwnd, STATE_SLOT, value);
    }
}

#[cfg(target_pointer_width = "32")]
pub(crate) fn state_slot(hwnd: HWND) -> isize {
    // SAFETY: as above; on 32-bit systems the slot is 32 bits wide.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongW(hwnd, STATE_SLOT) as isize
    }
}

#[cfg(target_pointer_width = "32")]
pub(crate) fn set_state_slot(hwnd: HWND, value: isize) {
    // SAFETY: as above.
    unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SetWindowLongW(hwnd, STATE_SLOT, value as i32);
    }
}

/// Looks up `AdjustWindowRectExForDpi` in user32 (Windows 10 1607 and
/// later).
fn find_adjust_for_dpi() -> Option<AdjustForDpiFn> {
    let user32: Vec<u16> = "user32.dll\0".encode_utf16().collect();
    // SAFETY: the name is NUL-terminated; a module that is not loaded
    // gives null.
    let module = unsafe { GetModuleHandleW(user32.as_ptr()) };
    if module.is_null() {
        return None;
    }
    // SAFETY: the procedure name is a NUL-terminated ASCII string.
    let found = unsafe { GetProcAddress(module, c"AdjustWindowRectExForDpi".as_ptr().cast()) }?;
    // SAFETY: AdjustWindowRectExForDpi is documented as
    // `BOOL (LPRECT, DWORD, BOOL, DWORD, UINT)` with the system calling
    // convention, which is this type.
    Some(unsafe { mem::transmute::<unsafe extern "system" fn() -> isize, AdjustForDpiFn>(found) })
}

/// A platform for a window of this binary's class, not yet made.
fn platform(module: HMODULE, top_level: Option<TopLevel>) -> Win32Platform {
    Win32Platform {
        top_level,
        drops: RefCell::new(None),
        hwnd: Cell::new(ptr::null_mut()),
        module,
        class: class_name("Window", module),
        get_dpi: find_get_dpi(),
        adopted: Cell::new(false),
        created: Cell::new(false),
        passing: Cell::new(false),
        key_passed: Cell::new(false),
        caret: Cell::new(None),
    }
}

/// Makes a window inside `parent` for the shared state built here.
pub(crate) fn open(
    parent: NonZeroUsize,
    options: &WindowOptions,
    handler: Box<dyn WindowHandler>,
) -> Result<Rc<Inner>, WindowError> {
    let parent: HWND = ptr::with_exposed_provenance_mut(parent.get());
    check_parent(parent)?;
    let module = this_module()?;
    let inner = Rc::new(Inner::new(
        Platform::Win32(platform(module, None)),
        options,
        handler,
    ));
    let style =
        WS_CHILD | WS_CLIPSIBLINGS | WS_CLIPCHILDREN | if options.visible { WS_VISIBLE } else { 0 };
    let Platform::Win32(platform) = &inner.platform else {
        unreachable!("made just above");
    };
    create(
        &inner,
        Creation {
            parent,
            style,
            ex_style: 0,
            at: (0, 0),
            size: options.size,
            title: &[0],
        },
    )?;
    if let Some(scale) = platform.query_scale() {
        inner.init_system_scale(scale);
    }
    inner.on_size(platform.client_size());
    if options.accepts_drops {
        platform.register_drops(&inner);
    }
    Ok(inner)
}

/// Makes an application's top-level window: hidden at first, sized for the
/// scale of the monitor it lands on, then shown if asked.
pub(crate) fn open_top_level(
    options: &AppWindowOptions,
    frame: Frame,
    ole: bool,
    app: Rc<AppInner>,
    handler: Box<dyn WindowHandler>,
) -> Result<Rc<Inner>, WindowError> {
    let module = this_module()?;
    let style = WS_CLIPCHILDREN
        | if options.resizable {
            WS_OVERLAPPEDWINDOW
        } else {
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX
        };
    let top_level = TopLevel {
        style,
        ex_style: 0,
        adjust: find_adjust_for_dpi(),
        activate: options.activate,
    };
    let title = com::wide(&frame.title.borrow());
    // A first guess at the size, at scale 1; the real one follows once the
    // window knows its monitor.
    let guess = frame.clamp(device_size(options.size, 1.0), 1.0);
    let window_options = WindowOptions {
        size: guess,
        scale: None,
        visible: options.visible,
        focus_on_click: true,
        accepts_drops: options.accepts_drops,
    };
    let inner = Rc::new(Inner::with_frame(
        Platform::Win32(platform(module, Some(top_level))),
        &window_options,
        handler,
        Some(frame),
        Some(app),
    ));
    let Platform::Win32(platform) = &inner.platform else {
        unreachable!("made just above");
    };
    // Nobody hears of the sizes and scales the window goes through while it
    // is made: what it ends with is its starting state.
    inner.quiet.set(true);
    let at = options
        .position
        .map_or((CW_USEDEFAULT, CW_USEDEFAULT), |at| (at.x, at.y));
    let outer = platform.outer_size(guess, 96);
    let made = create(
        &inner,
        Creation {
            parent: ptr::null_mut(),
            style,
            ex_style: 0,
            at,
            size: outer,
            title: &title,
        },
    );
    if let Err(error) = made {
        inner.quiet.set(false);
        return Err(error);
    }
    if let Some(scale) = platform.query_scale() {
        inner.init_system_scale(scale);
    }
    let size = inner.frame.as_ref().map_or(guess, |frame| {
        frame.clamp(device_size(options.size, inner.scale()), inner.scale())
    });
    let sized = platform.set_size(size);
    inner.on_size(platform.client_size());
    inner.quiet.set(false);
    if let Err(error) = sized {
        // Nobody had the window yet: its handler hears nothing of its end.
        if let Some(handler) = inner.dispatcher().retire() {
            drop_quietly(handler);
        }
        platform.destroy();
        return Err(error);
    }
    if options.accepts_drops && ole {
        platform.register_drops(&inner);
    }
    if options.visible {
        platform.set_visible(true);
    }
    Ok(inner)
}

/// How to make a window.
struct Creation<'a> {
    /// The parent, or null for a top-level window.
    parent: HWND,
    style: u32,
    ex_style: u32,
    /// Where, in the parent's client pixels or the screen's.
    at: (i32, i32),
    /// The size of the whole window, frame included.
    size: PhysicalSize,
    /// The NUL-terminated title.
    title: &'a [u16],
}

/// Makes the system window for `inner`, registering the class first.
fn create(inner: &Rc<Inner>, how: Creation<'_>) -> Result<HWND, WindowError> {
    let Platform::Win32(platform) = &inner.platform else {
        unreachable!("a Win32 window");
    };
    let module = platform.module;
    let mut attempts = 0;
    let hwnd = loop {
        register_class(
            module,
            &platform.class,
            Some(window_procedure),
            ptr::null_mut(),
            STATE_BYTES,
        )?;
        // The reference the window will hold, from WM_NCCREATE to
        // WM_NCDESTROY.
        let raw = Rc::into_raw(Rc::clone(inner));
        // SAFETY: the class is registered for `module` with a
        // NUL-terminated name; `parent` is null or a window of this
        // process; the title is NUL-terminated; the creation parameter is
        // the pointer the window procedure expects.
        let hwnd = unsafe {
            CreateWindowExW(
                how.ex_style,
                platform.class.as_ptr(),
                how.title.as_ptr(),
                how.style,
                how.at.0,
                how.at.1,
                how.size.width as i32,
                how.size.height as i32,
                how.parent,
                ptr::null_mut(),
                module,
                raw.cast::<c_void>(),
            )
        };
        let error = if hwnd.is_null() {
            last_error("CreateWindowExW")
        } else {
            WindowError::Closed
        };
        if !platform.adopted.get() {
            // SAFETY: the window procedure never took the reference, so it
            // is still ours to give back, once.
            drop(unsafe { Rc::from_raw(raw) });
        }
        if !hwnd.is_null() {
            break hwnd;
        }
        attempts += 1;
        let class_vanished = error
            == WindowError::Platform {
                call: "CreateWindowExW",
                code: ERROR_CANNOT_FIND_WND_CLASS,
            };
        // Another thread's last window took the class away between our
        // registration and our creation: register again.
        if class_vanished && attempts < 4 && !platform.adopted.get() {
            continue;
        }
        unregister_class(module, &platform.class);
        return Err(error);
    };
    debug_assert_eq!(platform.hwnd.get(), hwnd);
    platform.created.set(true);
    Ok(hwnd)
}

/// The window procedure of every window of the class.
unsafe extern "system" fn window_procedure(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the system calls this procedure with a window of the
        // class and the parameters of the message.
        unsafe { handle_message(hwnd, message, wparam, lparam) }
    }));
    match result {
        Ok(result) => result,
        Err(payload) => {
            drop_quietly(payload);
            // SAFETY: the default handling of the same message.
            unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
        }
    }
}

/// The modifiers held now, as the keyboard state of this thread says.
fn keyboard_modifiers() -> Modifiers {
    Modifiers {
        shift: key_down(VK_SHIFT),
        control: key_down(VK_CONTROL),
        alt: key_down(VK_MENU),
        meta: key_down(VK_LWIN) || key_down(VK_RWIN),
    }
}

fn key_down(vk: u16) -> bool {
    // SAFETY: GetKeyState accepts any virtual-key code.
    unsafe { GetKeyState(i32::from(vk)) < 0 }
}

/// The modifiers of a mouse message.
fn mouse_modifiers(wparam: WPARAM) -> Modifiers {
    convert::mouse_modifiers(
        wparam,
        key_down(VK_MENU),
        key_down(VK_LWIN) || key_down(VK_RWIN),
    )
}

fn message_time() -> u32 {
    // SAFETY: no arguments; reads the time of the current message.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetMessageTime() as u32 }
}

/// Handles one message.
///
/// # Safety
///
/// `hwnd` must be a window of the class, and `wparam` and `lparam` the
/// parameters the system gave with `message`.
unsafe fn handle_message(hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: the caller passes the system's own message.
    let default = || unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };

    if message == WM_NCCREATE {
        // SAFETY: for WM_NCCREATE, `lparam` points to the CREATESTRUCTW of
        // the CreateWindowExW call, alive for the length of the message.
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        let raw = create.lpCreateParams.cast_const().cast::<Inner>();
        if !raw.is_null() {
            // SAFETY: `open` passed a pointer from `Rc::into_raw`, and the
            // reference it carries is alive until we take it here.
            let inner = unsafe { &*raw };
            if let Platform::Win32(platform) = &inner.platform {
                platform.hwnd.set(hwnd);
                platform.adopted.set(true);
                set_state_slot(hwnd, raw.expose_provenance() as isize);
            }
        }
        return default();
    }

    let data = state_slot(hwnd);
    if data == 0 {
        return default();
    }
    let raw: *const Inner = ptr::with_exposed_provenance(data as usize);
    // SAFETY: the window holds a strong reference from WM_NCCREATE until
    // WM_NCDESTROY, and the slot is cleared before it is released. The
    // slot is in the class's extra bytes, which only this module writes
    // (the class is refused if it was registered with another procedure).
    // Taking
    // another one keeps the state alive for this message whatever the
    // handler does, including dropping the window.
    let inner = unsafe {
        Rc::increment_strong_count(raw);
        Rc::from_raw(raw)
    };
    let Platform::Win32(platform) = &inner.platform else {
        return default();
    };
    let origin = Origin {
        message,
        wparam,
        lparam,
    };

    match message {
        WM_NCDESTROY => {
            set_state_slot(hwnd, 0);
            platform.hwnd.set(ptr::null_mut());
            if platform.created.get() {
                inner.on_destroyed();
            }
            // SAFETY: the window's own reference from WM_NCCREATE, released
            // once: the slot is clear, so no later message finds it.
            drop(unsafe { Rc::from_raw(raw) });
            default()
        }
        WM_DESTROY => {
            // OLE wants the drop target revoked while the window exists.
            let drops = platform.drops.borrow_mut().take();
            if let Some(drops) = drops {
                drops.revoke();
            }
            default()
        }
        WM_CLOSE if platform.top_level.is_some() => {
            // The application decides; the window stays until it closes it.
            inner.close_requested();
            0
        }
        WM_GETMINMAXINFO if platform.top_level.is_some() => {
            let info = lparam as *mut MINMAXINFO;
            if let (Some(frame), false) = (inner.frame.as_ref(), info.is_null()) {
                // SAFETY: for WM_GETMINMAXINFO, `lparam` points to the
                // system's MINMAXINFO, alive and ours to fill for the
                // length of the message.
                let info = unsafe { &mut *info };
                platform.limit_tracking(frame, inner.scale(), info);
            }
            0
        }
        WM_DPICHANGED if platform.top_level.is_some() => {
            let dpi = (wparam & 0xFFFF) as u32;
            let suggested = lparam as *const RECT;
            if let (Some(scale), false) = (convert::scale_from_dpi(dpi), suggested.is_null()) {
                // SAFETY: for WM_DPICHANGED, `lparam` points to the
                // suggested window rectangle, alive for the message.
                let suggested = unsafe { *suggested };
                platform.move_to_dpi(&inner, dpi, scale, suggested);
            }
            0
        }
        WM_SIZE if platform.top_level.is_some() && wparam == SIZE_MINIMIZED as usize => {
            // A minimised window keeps its size and its back buffer.
            0
        }
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            // BeginPaint first: it validates the update region, and a
            // rectangle the handler invalidates while it paints must stay
            // invalid so that another WM_PAINT comes for it.
            let mut paint: PAINTSTRUCT = PAINTSTRUCT::default();
            // SAFETY: `paint` is a valid place for the paint information.
            let hdc = unsafe { BeginPaint(hwnd, &mut paint) };
            let _ = inner.paint_request();
            if !hdc.is_null() {
                let r = paint.rcPaint;
                if let Ok(back) = inner.back().try_borrow() {
                    blit_to(
                        hdc,
                        &back,
                        IRect::from_edges(r.left, r.top, r.right, r.bottom),
                    );
                }
                // SAFETY: ends the paint begun above.
                unsafe {
                    EndPaint(hwnd, &paint);
                }
            }
            0
        }
        WM_PRINTCLIENT => {
            let hdc: HDC = ptr::with_exposed_provenance_mut(wparam);
            if !hdc.is_null() {
                if let Ok(back) = inner.back().try_borrow() {
                    let size = back.size();
                    blit_to(hdc, &back, size.bounds());
                }
            }
            0
        }
        WM_SIZE => {
            inner.on_size(convert::size_from_lparam(lparam));
            0
        }
        WM_DPICHANGED_AFTERPARENT => {
            if let Some(scale) = platform.query_scale() {
                inner.on_system_scale(scale);
            }
            0
        }
        WM_MOUSEMOVE => {
            inner.input(RawInput::PointerMove {
                at: convert::point_from_lparam(lparam),
                modifiers: mouse_modifiers(wparam),
            });
            0
        }
        WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK | WM_MBUTTONDOWN
        | WM_MBUTTONDBLCLK => {
            let button = match message {
                WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => Button::Primary,
                WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => Button::Secondary,
                _ => Button::Middle,
            };
            inner.input(RawInput::PointerDown {
                at: convert::point_from_lparam(lparam),
                button,
                modifiers: mouse_modifiers(wparam),
                time_ms: message_time(),
            });
            0
        }
        WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP => {
            let button = match message {
                WM_LBUTTONUP => Button::Primary,
                WM_RBUTTONUP => Button::Secondary,
                _ => Button::Middle,
            };
            inner.input(RawInput::PointerUp {
                at: convert::point_from_lparam(lparam),
                button,
                modifiers: mouse_modifiers(wparam),
            });
            0
        }
        WM_MOUSELEAVE => {
            inner.input(RawInput::PointerLeft);
            0
        }
        WM_CAPTURECHANGED => {
            let to: HWND = ptr::with_exposed_provenance_mut(lparam as usize);
            if to != hwnd {
                inner.input(RawInput::CaptureLost);
            }
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL | WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP
        | WM_CHAR
            if platform.passing.get() =>
        {
            // The parent sent back what this window is passing on to it.
            0
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            // Wheel messages carry screen coordinates.
            let screen = convert::point_from_lparam(lparam);
            let mut at = POINT {
                x: screen.x,
                y: screen.y,
            };
            // SAFETY: `at` is a valid place for the converted point.
            unsafe {
                ScreenToClient(hwnd, &mut at);
            }
            let delta = convert::wheel_from_wparam(wparam);
            let (delta_x, delta_y) = if message == WM_MOUSEWHEEL {
                (0, delta)
            } else {
                (delta, 0)
            };
            // A turn the handler does not use goes on to the parent.
            inner.input_from(
                RawInput::Wheel {
                    at: crate::PhysicalPoint::new(at.x, at.y),
                    delta_x,
                    delta_y,
                    modifiers: mouse_modifiers(wparam),
                },
                Some(origin),
            );
            0
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let pressed = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
            let vk = (wparam & 0xFFFF) as u32;
            // SAFETY: MapVirtualKeyW accepts any code.
            let mapped = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) };
            let key = KeyEvent {
                key: convert::key_from_vk(vk, mapped),
                modifiers: keyboard_modifiers(),
                pressed,
            };
            let repeat = pressed && convert::key_repeat(lparam);
            // A key the handler does not use goes on to the parent.
            inner.input_from(RawInput::Key { key, repeat }, Some(origin));
            0
        }
        WM_CHAR => {
            let unit = (wparam & 0xFFFF) as u16;
            inner.input_from(RawInput::Utf16(unit), Some(origin));
            if convert::is_control_unit(unit) && platform.key_passed.get() {
                platform.pass_on(origin, None);
            }
            0
        }
        WM_SETFOCUS => {
            inner.input(RawInput::Focus(true));
            0
        }
        WM_KILLFOCUS => {
            inner.input(RawInput::Focus(false));
            0
        }
        WM_TIMER => {
            if let Ok(id) = u32::try_from(wparam) {
                inner.input(RawInput::Timer(TimerId(id)));
            }
            0
        }
        WM_GETDLGCODE => {
            let mut code = DLGC_WANTARROWS | DLGC_WANTCHARS;
            if inner.wants_all_keys.get() {
                code |= DLGC_WANTTAB | DLGC_WANTALLKEYS;
            }
            code as LRESULT
        }
        _ => default(),
    }
}

/// Copies `rect` of the back buffer to the same place in `hdc`.
fn blit_to(hdc: HDC, back: &BackBuffer, rect: IRect) {
    let size = back.size();
    let Some(rect) = rect.intersect(&size.bounds()) else {
        return;
    };
    let stride = size.width as usize * 4;
    // The rows of `rect`, as a DIB of its own: top-down (negative height),
    // as wide as the buffer, starting at the rectangle's first row. Its
    // source rectangle then starts at row 0, which reads the same in
    // either row order.
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size.width as i32,
            biHeight: -(rect.height as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    };
    let rows =
        &back.bgra()[rect.y as usize * stride..(rect.y as usize + rect.height as usize) * stride];
    // SAFETY: `rows` holds exactly `rect.height` rows of `size.width`
    // 32-bit pixels, which is what `info` describes, and the source
    // rectangle (x = rect.x, rows 0..rect.height, rect.width wide) lies
    // inside it because `rect` lies inside the buffer.
    unsafe {
        SetDIBitsToDevice(
            hdc,
            rect.x,
            rect.y,
            rect.width,
            rect.height,
            rect.x,
            0,
            0,
            rect.height,
            rows.as_ptr().cast::<c_void>(),
            &info,
            DIB_RGB_COLORS,
        );
    }
}

impl Win32Platform {
    fn hwnd(&self) -> Option<HWND> {
        let hwnd = self.hwnd.get();
        (!hwnd.is_null()).then_some(hwnd)
    }

    pub(crate) fn handle(&self) -> Option<usize> {
        self.hwnd().map(|hwnd| hwnd.addr())
    }

    pub(crate) fn blit(&self, back: &BackBuffer, rect: IRect) {
        let Some(hwnd) = self.hwnd() else { return };
        // SAFETY: `hwnd` is our window; GetDC on it gives its client area.
        let hdc = unsafe { GetDC(hwnd) };
        if hdc.is_null() {
            return;
        }
        blit_to(hdc, back, rect);
        // SAFETY: releases the DC taken above, on the same window.
        unsafe {
            ReleaseDC(hwnd, hdc);
        }
    }

    /// The handler answered an event that came from `origin`.
    pub(crate) fn answered(&self, origin: Origin, event: &Event, handled: Handled) {
        if matches!(origin.message, WM_KEYDOWN | WM_SYSKEYDOWN) {
            self.key_passed.set(handled == Handled::No);
        }
        if handled == Handled::No {
            self.pass_on(origin, Some(event));
        }
    }

    /// Registers the window as a drop target, if OLE takes it.
    fn register_drops(&self, inner: &Rc<Inner>) {
        let Some(hwnd) = self.hwnd() else { return };
        let registration = drop::Registration::register(hwnd, inner);
        inner.drops.set(registration.is_some());
        *self.drops.borrow_mut() = registration;
    }

    /// The drop target the window registered, as a COM pointer, for tests.
    #[cfg(feature = "testing")]
    pub(crate) fn drop_target(&self) -> Option<*mut c_void> {
        self.drops.borrow().as_ref().map(drop::Registration::target)
    }

    /// The window's DPI now.
    fn dpi(&self) -> u32 {
        self.query_dpi().unwrap_or(96)
    }

    /// The outer size of a top-level window whose inside is `client`, at
    /// `dpi`; `client` itself for a child window.
    fn outer_size(&self, client: PhysicalSize, dpi: u32) -> PhysicalSize {
        let Some(top) = &self.top_level else {
            return client;
        };
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: client.width.min(i32::MAX as u32) as i32,
            bottom: client.height.min(i32::MAX as u32) as i32,
        };
        // SAFETY: `rect` is a valid place; the styles are the window's.
        let ok = unsafe {
            match top.adjust {
                Some(adjust) => adjust(&mut rect, top.style, 0, top.ex_style, dpi),
                None => AdjustWindowRectEx(&mut rect, top.style, 0, top.ex_style),
            }
        };
        if ok == 0 {
            return client;
        }
        PhysicalSize::new(
            (i64::from(rect.right) - i64::from(rect.left)).max(1) as u32,
            (i64::from(rect.bottom) - i64::from(rect.top)).max(1) as u32,
        )
    }

    /// Fills in how small and how large the person may make the window, in
    /// outer pixels, from the frame's limits; the system's own limits stay
    /// where the frame sets none.
    fn limit_tracking(&self, frame: &Frame, scale: f64, info: &mut MINMAXINFO) {
        let dpi = self.dpi();
        let (min, max) = frame.limits(scale);
        if frame.min.is_some() {
            let outer = self.outer_size(min, dpi);
            info.ptMinTrackSize = POINT {
                x: outer.width as i32,
                y: outer.height as i32,
            };
        }
        if frame.max.is_some() {
            let outer = self.outer_size(max, dpi);
            info.ptMaxTrackSize = POINT {
                x: outer.width as i32,
                y: outer.height as i32,
            };
        }
    }

    /// The window moved to a monitor of `dpi`: the scale changes, then the
    /// window takes the rectangle the system suggests, its inside within
    /// the frame's limits.
    fn move_to_dpi(&self, inner: &Inner, dpi: u32, scale: f64, suggested: RECT) {
        let Some(hwnd) = self.hwnd() else { return };
        let frame = self.outer_size(PhysicalSize::new(0, 0), dpi);
        let width = (i64::from(suggested.right) - i64::from(suggested.left)).max(0) as u32;
        let height = (i64::from(suggested.bottom) - i64::from(suggested.top)).max(0) as u32;
        let client = PhysicalSize::new(
            width.saturating_sub(frame.width).max(1),
            height.saturating_sub(frame.height).max(1),
        );
        let client = inner.on_dpi_changed(scale, client);
        let outer = self.outer_size(client, dpi);
        // SAFETY: our own window; the size messages this sends come back to
        // our window procedure.
        unsafe {
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                suggested.left,
                suggested.top,
                outer.width as i32,
                outer.height as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    pub(crate) fn set_title(&self, title: &str) -> Result<(), WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        let title = com::wide(title);
        // SAFETY: our own window and a NUL-terminated title the system
        // copies.
        if unsafe { SetWindowTextW(hwnd, title.as_ptr()) } == 0 {
            return Err(last_error("SetWindowTextW"));
        }
        Ok(())
    }

    /// Destroys the window now; its WM_NCDESTROY tells the shared state.
    /// The class stays registered until the window's value is dropped.
    pub(crate) fn close(&self) {
        if let Some(hwnd) = self.hwnd() {
            // SAFETY: our own window, destroyed on its own thread.
            unsafe {
                DestroyWindow(hwnd);
            }
        }
    }

    pub(crate) fn dialog(
        &self,
        kind: DialogKind,
        dialog: &FileDialog,
    ) -> Result<Option<Vec<PathBuf>>, WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        dialog::show(hwnd, kind, dialog)
    }

    /// Sends the message `origin` to the parent window, or, for a
    /// top-level window, gives it to the system's default handling, which
    /// turns Alt+F4 into a close request and Alt and F10 into the window
    /// menu. Text goes as the character the handler declined, one
    /// `WM_CHAR` per UTF-16 unit, so a surrogate pair reaches the parent
    /// whole.
    fn pass_on(&self, origin: Origin, event: Option<&Event>) {
        let Some(hwnd) = self.hwnd() else { return };
        if self.top_level.is_some() {
            // SAFETY: the system's own message, with the parameters it
            // came with, to the default handling of our own window.
            unsafe {
                DefWindowProcW(hwnd, origin.message, origin.wparam, origin.lparam);
            }
            return;
        }
        if self.passing.get() {
            return;
        }
        // SAFETY: `hwnd` is our window.
        let parent = unsafe { GetParent(hwnd) };
        if parent.is_null() {
            return;
        }
        self.passing.set(true);
        match event {
            Some(Event::Text(c)) if origin.message == WM_CHAR => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    // SAFETY: a WM_CHAR with plain numbers to a live
                    // window; what the parent sends back meanwhile is
                    // dropped (see `passing`).
                    unsafe {
                        SendMessageW(parent, WM_CHAR, usize::from(*unit), origin.lparam);
                    }
                }
            }
            _ => {
                // SAFETY: the system's own message, with the parameters it
                // came with, none of them pointers; a parent that is gone
                // only fails.
                unsafe {
                    SendMessageW(parent, origin.message, origin.wparam, origin.lparam);
                }
            }
        }
        self.passing.set(false);
    }

    pub(crate) fn invalidate(&self, rect: IRect) {
        let Some(hwnd) = self.hwnd() else { return };
        let r = RECT {
            left: rect.x,
            top: rect.y,
            right: rect.right().clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            bottom: rect
                .bottom()
                .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        };
        // SAFETY: `r` is a valid rectangle for the length of the call.
        unsafe {
            InvalidateRect(hwnd, &r, 0);
        }
    }

    pub(crate) fn client_size(&self) -> PhysicalSize {
        let Some(hwnd) = self.hwnd() else {
            return PhysicalSize::default();
        };
        let mut r = RECT::default();
        // SAFETY: `r` is a valid place for the client rectangle.
        if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
            return PhysicalSize::default();
        }
        PhysicalSize::new(
            (i64::from(r.right) - i64::from(r.left)).max(0) as u32,
            (i64::from(r.bottom) - i64::from(r.top)).max(0) as u32,
        )
    }

    /// Makes the window's inside `size`: the whole window, for a child,
    /// and the inside within the frame, for a top-level window.
    pub(crate) fn set_size(&self, size: PhysicalSize) -> Result<PhysicalSize, WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        let outer = self.outer_size(size, self.dpi());
        // SAFETY: `hwnd` is our window. The size messages this sends come
        // back to our window procedure, which queues them for a running
        // handler.
        let ok = unsafe {
            SetWindowPos(
                hwnd,
                ptr::null_mut(),
                0,
                0,
                outer.width as i32,
                outer.height as i32,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
        if ok == 0 {
            return Err(last_error("SetWindowPos"));
        }
        Ok(self.client_size())
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        let Some(hwnd) = self.hwnd() else { return };
        let show = match (&self.top_level, visible) {
            (_, false) => SW_HIDE,
            (Some(top), true) if top.activate => SW_SHOW,
            (Some(_), true) => SW_SHOWNOACTIVATE,
            (None, true) => SW_SHOWNA,
        };
        // SAFETY: `hwnd` is our window.
        unsafe {
            ShowWindow(hwnd, show);
        }
    }

    pub(crate) fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        // SAFETY: a timer on our own window with no callback; it posts
        // WM_TIMER to the window and ends with it.
        let ok = unsafe { SetTimer(hwnd, id.0 as usize, period_ms.max(USER_TIMER_MINIMUM), None) };
        if ok == 0 {
            Err(last_error("SetTimer"))
        } else {
            Ok(())
        }
    }

    pub(crate) fn kill_timer(&self, id: TimerId) {
        let Some(hwnd) = self.hwnd() else { return };
        // SAFETY: kills a timer of our own window; an unknown id only
        // fails.
        unsafe {
            KillTimer(hwnd, id.0 as usize);
        }
    }

    pub(crate) fn capture(&self, on: bool) {
        let Some(hwnd) = self.hwnd() else { return };
        if on {
            // SAFETY: our own window, on its own thread.
            unsafe {
                SetCapture(hwnd);
            }
        // SAFETY: GetCapture has no arguments.
        } else if unsafe { GetCapture() } == hwnd {
            // SAFETY: releases the capture our window holds; the message it
            // sends comes back to our window procedure, which ignores it
            // because the window no longer counts itself as capturing.
            unsafe {
                ReleaseCapture();
            }
        }
    }

    pub(crate) fn take_focus(&self) {
        let Some(hwnd) = self.hwnd() else { return };
        // SAFETY: our own window, on its own thread. The focus messages
        // come back to our window procedure.
        unsafe {
            SetFocus(hwnd);
        }
    }

    /// The clipboard's text, at most `MAX_CLIPBOARD_UNITS` units of it.
    pub(crate) fn read_clipboard(&self) -> Result<Option<String>, WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        // SAFETY: takes a format number only.
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } == 0 {
            return Ok(None);
        }
        let _open = OpenClipboardGuard::open(hwnd)?;
        // SAFETY: the clipboard is open; the handle stays the clipboard's
        // and is only read, until the clipboard is closed.
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
        if handle.is_null() {
            return Ok(None);
        }
        // SAFETY: a global memory handle from the clipboard.
        let bytes = unsafe { GlobalSize(handle) };
        // SAFETY: as above; locking a clipboard handle gives its memory, or
        // null.
        let data = unsafe { GlobalLock(handle) }.cast::<u16>().cast_const();
        if data.is_null() {
            return Ok(None);
        }
        let units = (bytes / 2).min(crate::clipboard::MAX_CLIPBOARD_UNITS);
        // SAFETY: the block holds `bytes` bytes, so `units` 16-bit units
        // from its start are inside it; it stays locked until below, and
        // the memory of a global block is aligned for any type.
        let slice = unsafe { std::slice::from_raw_parts(data, units) };
        let end = slice.iter().position(|&unit| unit == 0).unwrap_or(units);
        let text = String::from_utf16_lossy(&slice[..end]);
        // SAFETY: unlocks what was locked above.
        unsafe {
            GlobalUnlock(handle);
        }
        Ok(Some(text))
    }

    /// Puts `text` on the clipboard as Unicode text.
    pub(crate) fn write_clipboard(&self, text: &str) -> Result<(), WindowError> {
        let hwnd = self.hwnd().ok_or(WindowError::Closed)?;
        let units: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = units.len() * mem::size_of::<u16>();
        // SAFETY: allocates a movable global block, as the clipboard wants.
        let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
        if memory.is_null() {
            return Err(last_error("GlobalAlloc"));
        }
        let free = |memory| {
            // SAFETY: a block this function allocated and still owns.
            unsafe {
                GlobalFree(memory);
            }
        };
        // SAFETY: the block just allocated.
        let target = unsafe { GlobalLock(memory) }.cast::<u16>();
        if target.is_null() {
            let error = last_error("GlobalLock");
            free(memory);
            return Err(error);
        }
        // SAFETY: the block holds `bytes` bytes, exactly `units`, and is
        // aligned for any type; the two do not overlap.
        unsafe {
            ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            GlobalUnlock(memory);
        }
        let open = match OpenClipboardGuard::open(hwnd) {
            Ok(open) => open,
            Err(error) => {
                free(memory);
                return Err(error);
            }
        };
        // SAFETY: the clipboard is open, by this window.
        if unsafe { EmptyClipboard() } == 0 {
            let error = last_error("EmptyClipboard");
            drop(open);
            free(memory);
            return Err(error);
        }
        // SAFETY: the clipboard is open and empty; on success the system
        // owns the block.
        if unsafe { SetClipboardData(CF_UNICODETEXT, memory) }.is_null() {
            let error = last_error("SetClipboardData");
            drop(open);
            free(memory);
            return Err(error);
        }
        Ok(())
    }

    /// Shows the system where the text caret is, or that there is none:
    /// a hidden system caret of this window's, and the input method's
    /// composition at the caret with its candidates below. Called only
    /// while the window has the keyboard, and with `None` as it loses it,
    /// so the caret this thread has is always this window's.
    pub(crate) fn show_caret(&self, caret: Option<IRect>) {
        let Some(hwnd) = self.hwnd() else {
            self.caret.set(None);
            return;
        };
        let Some(rect) = caret else {
            if self.caret.take().is_some() {
                // SAFETY: destroys the caret this window made while it had
                // the keyboard, which it still has.
                unsafe {
                    DestroyCaret();
                }
            }
            return;
        };
        let width = rect.width.clamp(1, 64) as i32;
        let height = rect.height.clamp(1, 1024) as i32;
        let resized = self
            .caret
            .get()
            .is_none_or(|shown| (shown.width, shown.height) != (rect.width, rect.height));
        // SAFETY: a plain caret of our own window, on its own thread, which
        // has the keyboard; it is never shown (no ShowCaret): the
        // interface draws its own.
        unsafe {
            if resized {
                CreateCaret(hwnd, ptr::null_mut(), width, height);
            }
            SetCaretPos(rect.x, rect.y);
        }
        self.caret.set(Some(rect));
        let bottom = rect.y.saturating_add(height);
        // SAFETY: the input context of our own window, released below.
        let context = unsafe { ImmGetContext(hwnd) };
        if !context.is_null() {
            let at = POINT {
                x: rect.x,
                y: rect.y,
            };
            let composition = COMPOSITIONFORM {
                dwStyle: CFS_POINT,
                ptCurrentPos: at,
                rcArea: RECT::default(),
            };
            let candidates = CANDIDATEFORM {
                dwIndex: 0,
                dwStyle: CFS_EXCLUDE,
                ptCurrentPos: POINT {
                    x: rect.x,
                    y: bottom,
                },
                rcArea: RECT {
                    left: rect.x,
                    top: rect.y,
                    right: rect.x.saturating_add(width),
                    bottom,
                },
            };
            // SAFETY: the forms are initialised and outlive the calls; the
            // context is released once.
            unsafe {
                ImmSetCompositionWindow(context, &composition);
                ImmSetCandidateWindow(context, &candidates);
                ImmReleaseContext(hwnd, context);
            }
        }
    }

    pub(crate) fn track_leave(&self) {
        let Some(hwnd) = self.hwnd() else { return };
        let mut track = TRACKMOUSEEVENT {
            cbSize: mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        // SAFETY: `track` is fully initialised for the length of the call.
        unsafe {
            TrackMouseEvent(&mut track);
        }
    }

    pub(crate) fn click_settings(&self) -> ClickSettings {
        // SAFETY: these read system settings and take no pointers.
        let (interval_ms, width, height) = unsafe {
            (
                GetDoubleClickTime(),
                GetSystemMetrics(SM_CXDOUBLECLK),
                GetSystemMetrics(SM_CYDOUBLECLK),
            )
        };
        ClickSettings {
            interval_ms,
            half_width: width.max(0) / 2,
            half_height: height.max(0) / 2,
        }
    }

    /// The system's scale for the window: its DPI over 96.
    fn query_scale(&self) -> Option<f64> {
        convert::scale_from_dpi(self.query_dpi()?)
    }

    /// The system's DPI for the window.
    fn query_dpi(&self) -> Option<u32> {
        let hwnd = self.hwnd()?;
        let dpi = match self.get_dpi {
            // SAFETY: the function was found under its documented name and
            // takes a window handle.
            Some(get_dpi) => unsafe { get_dpi(hwnd) },
            None => 0,
        };
        let dpi = if dpi != 0 {
            dpi
        } else {
            // SAFETY: a DC of our own window, released below.
            let hdc = unsafe { GetDC(hwnd) };
            if hdc.is_null() {
                return None;
            }
            // SAFETY: `hdc` is valid until released.
            let dpi = unsafe { GetDeviceCaps(hdc, LOGPIXELSX as i32) };
            // SAFETY: releases the DC taken above.
            unsafe {
                ReleaseDC(hwnd, hdc);
            }
            u32::try_from(dpi).unwrap_or(0)
        };
        (dpi != 0).then_some(dpi)
    }

    pub(crate) fn destroy(&self) {
        if let Some(hwnd) = self.hwnd() {
            // SAFETY: our own window, destroyed on its own thread (the
            // window value cannot leave it). Its WM_NCDESTROY releases the
            // window's reference to the shared state.
            unsafe {
                DestroyWindow(hwnd);
            }
        }
        unregister_class(self.module, &self.class);
    }
}
