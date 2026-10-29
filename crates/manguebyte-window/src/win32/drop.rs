//! Files and text dropped onto a window, through OLE's `IDropTarget`.
//!
//! A window that takes drops registers a drop target of its own with
//! `RegisterDragDrop` when it is made, and revokes it in `WM_DESTROY`. The
//! target is a small COM object here: it reads the files (`CF_HDROP`) and
//! the text (`CF_UNICODETEXT`) a drag carries when it comes in, turns the
//! screen positions OLE gives into the window's, and asks the window's
//! handler through the same drag code the headless backend runs. It holds
//! the window weakly, so a target OLE keeps after the window is gone
//! refuses everything.
//!
//! OLE calls a target only on the thread that registered it (a
//! single-threaded apartment), which is the window's: the `Rc` state the
//! target reaches is never touched from another thread.

use std::cell::Cell;
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::ptr;
use std::rc::{Rc, Weak};

use windows_sys::Win32::Foundation::{
    E_NOINTERFACE, E_POINTER, HGLOBAL, HWND, POINT, POINTL, S_OK,
};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Com::{
    APTTYPE_MAINSTA, APTTYPE_STA, CoGetApartmentType, DVASPECT_CONTENT, FORMATETC, STGMEDIUM,
    TYMED_HGLOBAL,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::{
    CF_HDROP, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_NONE, RegisterDragDrop,
    ReleaseStgMedium, RevokeDragDrop,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_LWIN, VK_RWIN};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::core::{GUID, HRESULT};

use super::com::{IDataObjectVtbl, IDropTargetVtbl, IID_IDROPTARGET, IID_IUNKNOWN, IUnknownVtbl};
use super::key_down;
use crate::clipboard::MAX_CLIPBOARD_UNITS;
use crate::convert;
use crate::drag::{DragData, MAX_DRAGGED_FILES, RawDrag};
use crate::event::{Handled, Modifiers, PhysicalPoint};
use crate::window::Inner;

/// `MK_ALT` in a drag's key state.
const MK_ALT: u32 = 0x0020;

/// A window's drop target: the COM object OLE calls.
#[repr(C)]
pub(crate) struct DropTarget {
    /// First, as COM wants: the method table.
    vtable: *const IDropTargetVtbl,
    refs: Cell<u32>,
    hwnd: HWND,
    inner: Weak<Inner>,
}

static VTABLE: IDropTargetVtbl = IDropTargetVtbl {
    unknown: IUnknownVtbl {
        query_interface,
        add_ref,
        release,
    },
    drag_enter,
    drag_over,
    drag_leave,
    drop: drop_on,
};

/// The drop target a window registered, and the reference it keeps on it.
pub(crate) struct Registration {
    target: *mut DropTarget,
}

impl Registration {
    /// Registers a drop target for `hwnd`, which reaches `inner`. `None`
    /// when OLE refuses, as it does on a thread where it is not
    /// initialised.
    pub(crate) fn register(hwnd: HWND, inner: &Rc<Inner>) -> Option<Registration> {
        // Only on a single-threaded apartment. On a thread with no OLE of
        // its own, while another thread of the process has it, this
        // system's `RegisterDragDrop` fails yet leaves the window marked as
        // registered (found by this crate's tests), so it is not called at
        // all there.
        if !thread_is_single_threaded_apartment() {
            return None;
        }
        let target = Box::into_raw(Box::new(DropTarget {
            vtable: &VTABLE,
            refs: Cell::new(1),
            hwnd,
            inner: Rc::downgrade(inner),
        }));
        // SAFETY: `hwnd` is our window, on this thread; `target` is a live
        // IDropTarget, which OLE takes a reference to on success.
        let result = unsafe { RegisterDragDrop(hwnd, target.cast::<c_void>()) };
        let registration = Registration { target };
        if result < 0 {
            // Whatever a failed registration left is taken away before our
            // reference, the only one, goes with the value.
            registration.revoke();
            return None;
        }
        Some(registration)
    }

    /// The target, as a COM pointer, for tests.
    #[cfg(feature = "testing")]
    pub(crate) fn target(&self) -> *mut c_void {
        self.target.cast()
    }

    /// Revokes the registration, before the window is destroyed. OLE gives
    /// back its reference; ours goes when this is dropped.
    pub(crate) fn revoke(&self) {
        // SAFETY: our reference keeps the target alive, and its window is
        // still alive too (this runs in its WM_DESTROY); a window that is
        // not registered only fails.
        unsafe {
            RevokeDragDrop((*self.target).hwnd);
        }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: the reference `register` made, given back once.
        unsafe {
            release(self.target.cast());
        }
    }
}

/// Whether this thread is a single-threaded COM apartment, as `OleInitialize`
/// makes it (`CoGetApartmentType`); false when COM is not initialised for
/// it, or it is in the multithreaded apartment.
pub(crate) fn thread_is_single_threaded_apartment() -> bool {
    let (mut kind, mut qualifier) = (0, 0);
    // SAFETY: two valid places for the answer.
    let result = unsafe { CoGetApartmentType(&mut kind, &mut qualifier) };
    result >= 0 && (kind == APTTYPE_STA || kind == APTTYPE_MAINSTA)
}

/// The target behind a COM pointer.
///
/// # Safety
///
/// `this` must be a pointer `Registration::register` made, still alive.
unsafe fn target<'a>(this: *mut c_void) -> &'a DropTarget {
    // SAFETY: the caller vouches for it.
    unsafe { &*this.cast::<DropTarget>() }
}

unsafe extern "system" fn query_interface(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM passes a valid interface id.
    let iid = unsafe { &*iid };
    let known = |want: &GUID| {
        want.data1 == iid.data1
            && want.data2 == iid.data2
            && want.data3 == iid.data3
            && want.data4 == iid.data4
    };
    if known(&IID_IUNKNOWN) || known(&IID_IDROPTARGET) {
        // SAFETY: `this` is our target; `out` a valid place.
        unsafe {
            add_ref(this);
            *out = this;
        }
        S_OK
    } else {
        // SAFETY: `out` is a valid place.
        unsafe {
            *out = ptr::null_mut();
        }
        E_NOINTERFACE
    }
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: COM calls this on a live target.
    let target = unsafe { target(this) };
    let refs = target.refs.get().saturating_add(1);
    target.refs.set(refs);
    refs
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: COM calls this on a live target.
    let refs = unsafe { target(this) }.refs.get().saturating_sub(1);
    // SAFETY: as above.
    unsafe { target(this) }.refs.set(refs);
    if refs == 0 {
        // SAFETY: the last reference is gone; the box was made by
        // `register` and is freed once.
        drop(unsafe { Box::from_raw(this.cast::<DropTarget>()) });
    }
    refs
}

/// Runs `work` with the window, catching any panic: nothing unwinds into
/// OLE. Answers `DROPEFFECT_NONE` when the window is gone.
fn with_window(this: *mut c_void, effect: *mut u32, work: impl FnOnce(&Inner, HWND) -> Handled) {
    let answer = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: OLE calls the target's methods on a live target.
        let target = unsafe { target(this) };
        match target.inner.upgrade() {
            Some(inner) => work(&inner, target.hwnd),
            None => Handled::No,
        }
    }))
    .unwrap_or(Handled::No);
    if effect.is_null() {
        return;
    }
    // SAFETY: OLE passes a valid place for the effect, holding the effects
    // the source allows.
    unsafe {
        let allowed = *effect;
        *effect = effect_for(answer, allowed);
    }
}

/// The effect to report for `answer` when the source allows `allowed`: a
/// copy, or a link when only that is allowed. Never a move, which would
/// have the source delete what it gave.
pub(crate) fn effect_for(answer: Handled, allowed: u32) -> u32 {
    match answer {
        Handled::Yes if allowed & DROPEFFECT_COPY != 0 => DROPEFFECT_COPY,
        Handled::Yes if allowed & DROPEFFECT_LINK != 0 => DROPEFFECT_LINK,
        _ => DROPEFFECT_NONE,
    }
}

/// A screen position, in the window's client pixels.
fn client_point(hwnd: HWND, at: POINTL) -> PhysicalPoint {
    let mut point = POINT { x: at.x, y: at.y };
    // SAFETY: `hwnd` is the target's window; `point` a valid place.
    unsafe {
        ScreenToClient(hwnd, &mut point);
    }
    PhysicalPoint::new(point.x, point.y)
}

/// The modifiers of a drag's key state.
fn drag_modifiers(keys: u32) -> Modifiers {
    convert::mouse_modifiers(
        keys as usize,
        keys & MK_ALT != 0,
        key_down(VK_LWIN) || key_down(VK_RWIN),
    )
}

unsafe extern "system" fn drag_enter(
    this: *mut c_void,
    data: *mut c_void,
    keys: u32,
    at: POINTL,
    effect: *mut u32,
) -> HRESULT {
    with_window(this, effect, |inner, hwnd| {
        // SAFETY: OLE passes the drag's data object, alive for the call.
        let data = unsafe { read(data) };
        inner.drag(RawDrag::Enter {
            at: client_point(hwnd, at),
            data,
            modifiers: drag_modifiers(keys),
        })
    });
    S_OK
}

unsafe extern "system" fn drag_over(
    this: *mut c_void,
    keys: u32,
    at: POINTL,
    effect: *mut u32,
) -> HRESULT {
    with_window(this, effect, |inner, hwnd| {
        inner.drag(RawDrag::Over {
            at: client_point(hwnd, at),
            modifiers: drag_modifiers(keys),
        })
    });
    S_OK
}

unsafe extern "system" fn drag_leave(this: *mut c_void) -> HRESULT {
    with_window(this, ptr::null_mut(), |inner, _| inner.drag(RawDrag::Leave));
    S_OK
}

unsafe extern "system" fn drop_on(
    this: *mut c_void,
    _data: *mut c_void,
    keys: u32,
    at: POINTL,
    effect: *mut u32,
) -> HRESULT {
    // The data was read when the drag came in; it does not change.
    with_window(this, effect, |inner, hwnd| {
        inner.drag(RawDrag::Drop {
            at: client_point(hwnd, at),
            modifiers: drag_modifiers(keys),
        })
    });
    S_OK
}

/// A global memory block a data object gave, released when dropped.
struct Medium(STGMEDIUM);

impl Drop for Medium {
    fn drop(&mut self) {
        // SAFETY: a medium `GetData` filled in, released once, as the data
        // object's protocol asks of whoever received it.
        unsafe {
            ReleaseStgMedium(&mut self.0);
        }
    }
}

/// Asks `data` for `format` in global memory.
///
/// # Safety
///
/// `data` must be null or a live `IDataObject`.
unsafe fn get_global(data: *mut c_void, format: u16) -> Option<Medium> {
    if data.is_null() {
        return None;
    }
    let format = FORMATETC {
        cfFormat: format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: TYMED_HGLOBAL as u32,
    };
    let mut medium = STGMEDIUM::default();
    // SAFETY: `data` is a live data object (the caller vouches), whose
    // table starts with IDataObject's; `format` and `medium` are valid.
    let result = unsafe {
        let vtable = &**data.cast::<*const IDataObjectVtbl>();
        (vtable.get_data)(data, &format, &mut medium)
    };
    if result < 0 {
        return None;
    }
    let medium = Medium(medium);
    // SAFETY: `tymed` says which member of the union is set.
    let global = unsafe { medium.0.u.hGlobal };
    (medium.0.tymed == TYMED_HGLOBAL as u32 && !global.is_null()).then_some(medium)
}

/// What the data object of a drag carries that a window takes.
///
/// # Safety
///
/// `data` must be null or a live `IDataObject`.
pub(crate) unsafe fn read(data: *mut c_void) -> DragData {
    let mut dragged = DragData::default();
    // SAFETY: the caller vouches for `data`.
    if let Some(medium) = unsafe { get_global(data, CF_HDROP) } {
        // SAFETY: a CF_HDROP medium's global memory is an HDROP.
        dragged.files = unsafe { files(medium.0.u.hGlobal) };
    }
    // SAFETY: as above.
    if let Some(medium) = unsafe { get_global(data, CF_UNICODETEXT) } {
        // SAFETY: a CF_UNICODETEXT medium's memory is UTF-16 text.
        dragged.text = unsafe { text(medium.0.u.hGlobal) };
    }
    dragged
}

/// The paths in an `HDROP`, at most [`MAX_DRAGGED_FILES`].
///
/// # Safety
///
/// `drop` must be a valid `HDROP`.
unsafe fn files(drop: HGLOBAL) -> Vec<PathBuf> {
    let drop: HDROP = drop;
    // SAFETY: index 0xFFFFFFFF asks for the count; no buffer is written.
    let count = unsafe { DragQueryFileW(drop, u32::MAX, ptr::null_mut(), 0) };
    let mut files = Vec::new();
    for index in 0..count.min(MAX_DRAGGED_FILES as u32) {
        // SAFETY: a null buffer asks for the length, without the NUL.
        let len = unsafe { DragQueryFileW(drop, index, ptr::null_mut(), 0) };
        if len == 0 {
            continue;
        }
        let mut name = vec![0u16; len as usize + 1];
        // SAFETY: `name` holds `len + 1` units, which the call is told.
        let got = unsafe { DragQueryFileW(drop, index, name.as_mut_ptr(), len + 1) };
        name.truncate(got.min(len) as usize);
        files.push(PathBuf::from(OsString::from_wide(&name)));
    }
    files
}

/// The text in a global block of UTF-16, up to its NUL and at most
/// [`MAX_CLIPBOARD_UNITS`] units.
///
/// # Safety
///
/// `global` must be a valid global memory handle.
unsafe fn text(global: HGLOBAL) -> Option<String> {
    // SAFETY: a valid handle, as the caller says.
    let bytes = unsafe { GlobalSize(global) };
    // SAFETY: as above; locked until below.
    let data = unsafe { GlobalLock(global) }.cast::<u16>().cast_const();
    if data.is_null() {
        return None;
    }
    let units = (bytes / 2).min(MAX_CLIPBOARD_UNITS);
    // SAFETY: the block holds `bytes` bytes, so `units` units from its start
    // are inside it, and global memory is aligned for any type.
    let slice = unsafe { std::slice::from_raw_parts(data, units) };
    let end = slice.iter().position(|&unit| unit == 0).unwrap_or(units);
    let text = String::from_utf16_lossy(&slice[..end]);
    // SAFETY: unlocks what was locked above.
    unsafe {
        GlobalUnlock(global);
    }
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_taken_drag_is_a_copy_or_a_link_and_never_a_move() {
        use windows_sys::Win32::System::Ole::DROPEFFECT_MOVE;
        let all = DROPEFFECT_COPY | DROPEFFECT_MOVE | DROPEFFECT_LINK;
        assert_eq!(effect_for(Handled::Yes, all), DROPEFFECT_COPY);
        assert_eq!(
            effect_for(Handled::Yes, DROPEFFECT_MOVE | DROPEFFECT_LINK),
            DROPEFFECT_LINK
        );
        assert_eq!(effect_for(Handled::Yes, DROPEFFECT_MOVE), DROPEFFECT_NONE);
        assert_eq!(effect_for(Handled::No, all), DROPEFFECT_NONE);
    }
}
