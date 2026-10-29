//! The few COM interfaces the backend calls or implements, declared by
//! hand: `windows-sys` has the functions and structures but not the
//! interfaces' method tables.
//!
//! Each table lists its methods in the order the Windows SDK declares
//! them; methods the backend never calls are kept as plain pointers so
//! that the ones after them are at the right offsets.

use std::ffi::c_void;
use std::ptr;

use windows_sys::Win32::Foundation::{HWND, POINTL};
use windows_sys::Win32::System::Com::{FORMATETC, STGMEDIUM};
use windows_sys::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows_sys::core::{GUID, HRESULT, PCWSTR, PWSTR};

use crate::window::WindowError;

pub(crate) const IID_IUNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
pub(crate) const IID_IDROPTARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);
#[cfg_attr(not(feature = "testing"), allow(dead_code))]
pub(crate) const IID_IDATAOBJECT: GUID = GUID::from_u128(0x0000010e_0000_0000_c000_000000000046);
pub(crate) const IID_IFILEOPENDIALOG: GUID =
    GUID::from_u128(0xd57c7288_d4ad_4768_be02_9d969532d960);
pub(crate) const IID_IFILESAVEDIALOG: GUID =
    GUID::from_u128(0x84bccd23_5fde_4cdb_aea4_af64b83d78ab);
pub(crate) const IID_ISHELLITEM: GUID = GUID::from_u128(0x43826d1e_e718_42ee_bc55_a1e261c37bfe);

/// What a method left unused takes the place of in a table.
pub(crate) type Unused = usize;

/// `IUnknown`.
#[repr(C)]
pub(crate) struct IUnknownVtbl {
    pub(crate) query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    pub(crate) add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    pub(crate) release: unsafe extern "system" fn(*mut c_void) -> u32,
}

/// `IDropTarget`.
#[repr(C)]
pub(crate) struct IDropTargetVtbl {
    pub(crate) unknown: IUnknownVtbl,
    pub(crate) drag_enter:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    pub(crate) drag_over: unsafe extern "system" fn(*mut c_void, u32, POINTL, *mut u32) -> HRESULT,
    pub(crate) drag_leave: unsafe extern "system" fn(*mut c_void) -> HRESULT,
    pub(crate) drop:
        unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> HRESULT,
}

/// `IDataObject`.
#[repr(C)]
pub(crate) struct IDataObjectVtbl {
    pub(crate) unknown: IUnknownVtbl,
    pub(crate) get_data:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    pub(crate) get_data_here:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> HRESULT,
    pub(crate) query_get_data: unsafe extern "system" fn(*mut c_void, *const FORMATETC) -> HRESULT,
    pub(crate) get_canonical_format_etc:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut FORMATETC) -> HRESULT,
    pub(crate) set_data:
        unsafe extern "system" fn(*mut c_void, *const FORMATETC, *const STGMEDIUM, i32) -> HRESULT,
    pub(crate) enum_format_etc:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    pub(crate) d_advise: unsafe extern "system" fn(
        *mut c_void,
        *const FORMATETC,
        u32,
        *mut c_void,
        *mut u32,
    ) -> HRESULT,
    pub(crate) d_unadvise: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    pub(crate) enum_d_advise: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
}

/// `IFileDialog`, with `IModalWindow::Show` before it, and the two methods
/// `IFileOpenDialog` adds after it.
#[repr(C)]
pub(crate) struct IFileDialogVtbl {
    pub(crate) unknown: IUnknownVtbl,
    pub(crate) show: unsafe extern "system" fn(*mut c_void, HWND) -> HRESULT,
    pub(crate) set_file_types:
        unsafe extern "system" fn(*mut c_void, u32, *const COMDLG_FILTERSPEC) -> HRESULT,
    pub(crate) set_file_type_index: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    pub(crate) get_file_type_index: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    pub(crate) advise: Unused,
    pub(crate) unadvise: Unused,
    pub(crate) set_options: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
    pub(crate) get_options: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    pub(crate) set_default_folder: Unused,
    pub(crate) set_folder: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub(crate) get_folder: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub(crate) get_current_selection: Unused,
    pub(crate) set_file_name: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    pub(crate) get_file_name: Unused,
    pub(crate) set_title: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    pub(crate) set_ok_button_label: Unused,
    pub(crate) set_file_name_label: Unused,
    pub(crate) get_result: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub(crate) add_place: Unused,
    pub(crate) set_default_extension: unsafe extern "system" fn(*mut c_void, PCWSTR) -> HRESULT,
    pub(crate) close: Unused,
    pub(crate) set_client_guid: Unused,
    pub(crate) clear_client_data: Unused,
    pub(crate) set_filter: Unused,
}

/// `IFileOpenDialog`: `IFileDialog` and two methods more.
#[repr(C)]
pub(crate) struct IFileOpenDialogVtbl {
    pub(crate) dialog: IFileDialogVtbl,
    pub(crate) get_results: unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT,
    pub(crate) get_selected_items: Unused,
}

/// `IShellItem`.
#[repr(C)]
pub(crate) struct IShellItemVtbl {
    pub(crate) unknown: IUnknownVtbl,
    pub(crate) bind_to_handler: Unused,
    pub(crate) get_parent: Unused,
    pub(crate) get_display_name: unsafe extern "system" fn(*mut c_void, i32, *mut PWSTR) -> HRESULT,
    pub(crate) get_attributes: Unused,
    pub(crate) compare: Unused,
}

/// `IShellItemArray`.
#[repr(C)]
pub(crate) struct IShellItemArrayVtbl {
    pub(crate) unknown: IUnknownVtbl,
    pub(crate) bind_to_handler: Unused,
    pub(crate) get_property_store: Unused,
    pub(crate) get_property_description_list: Unused,
    pub(crate) get_attributes: Unused,
    pub(crate) get_count: unsafe extern "system" fn(*mut c_void, *mut u32) -> HRESULT,
    pub(crate) get_item_at:
        unsafe extern "system" fn(*mut c_void, u32, *mut *mut c_void) -> HRESULT,
    pub(crate) enum_items: Unused,
}

/// A COM object this code holds a reference to, released when dropped.
#[derive(Debug)]
pub(crate) struct ComPtr(ptr::NonNull<c_void>);

impl ComPtr {
    /// Takes over the reference `raw` carries, or `None` for null.
    ///
    /// # Safety
    ///
    /// `raw` must be null or a COM interface pointer whose reference the
    /// caller owns and gives away here.
    pub(crate) unsafe fn from_raw(raw: *mut c_void) -> Option<ComPtr> {
        ptr::NonNull::new(raw).map(ComPtr)
    }

    pub(crate) fn as_raw(&self) -> *mut c_void {
        self.0.as_ptr()
    }

    /// The object's method table, as a table of type `T`.
    ///
    /// # Safety
    ///
    /// The object must implement an interface whose table starts with `T`.
    pub(crate) unsafe fn vtable<T>(&self) -> &T {
        // SAFETY: a COM pointer points to a pointer to its method table,
        // which lives as long as the object; the caller vouches for the
        // table's type.
        unsafe { &**self.0.as_ptr().cast::<*const T>() }
    }
}

impl Drop for ComPtr {
    fn drop(&mut self) {
        // SAFETY: every COM interface starts with IUnknown, and this holds
        // one reference, given back once.
        unsafe {
            (self.vtable::<IUnknownVtbl>().release)(self.as_raw());
        }
    }
}

/// `result` as an error from `call` when it is a failure.
pub(crate) fn check(call: &'static str, result: HRESULT) -> Result<(), WindowError> {
    if result < 0 {
        Err(WindowError::Platform {
            call,
            code: result as u32,
        })
    } else {
        Ok(())
    }
}

/// `text` as a NUL-terminated wide string, ending at its first NUL.
pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.split('\0')
        .next()
        .unwrap_or_default()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}
