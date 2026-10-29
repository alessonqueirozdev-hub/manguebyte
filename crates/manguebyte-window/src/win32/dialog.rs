//! The system's file dialogs: `IFileOpenDialog` and `IFileSaveDialog`.
//!
//! A dialog is made with `CoCreateInstance`, configured, shown modal to the
//! window's top-level window with `IModalWindow::Show`, which runs a loop
//! of its own until the person closes it, and read with `GetResult` or
//! `GetResults`. Only file-system paths are returned
//! (`FOS_FORCEFILESYSTEM`), and the process's current folder is left
//! alone (`FOS_NOCHANGEDIR`).
//!
//! The dialogs are COM objects, so the thread needs COM. A host's thread
//! and an [`App`](crate::App)'s have it; on a thread that has none, COM is
//! initialised as a single-threaded apartment for the length of the call
//! and uninitialised before it returns, so the thread is left as it was.

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows_sys::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance,
    CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows_sys::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
    FOS_OVERWRITEPROMPT, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, FileSaveDialog,
    SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{GA_ROOT, GetAncestor};
use windows_sys::core::{GUID, HRESULT, PWSTR};

use super::com::{
    ComPtr, IFileDialogVtbl, IFileOpenDialogVtbl, IID_IFILEOPENDIALOG, IID_IFILESAVEDIALOG,
    IID_ISHELLITEM, IShellItemArrayVtbl, IShellItemVtbl, check, wide,
};
use crate::dialog::{DialogKind, FileDialog};
use crate::window::WindowError;

/// `CO_E_NOTINITIALIZED`: the thread has no COM.
const CO_E_NOTINITIALIZED: HRESULT = 0x800401F0_u32 as HRESULT;

/// What `Show` returns when the person cancels.
const CANCELLED: HRESULT = (0x8007_0000 | ERROR_CANCELLED) as HRESULT;

/// The options a dialog of `kind` is given.
pub(crate) fn options_for(kind: DialogKind, dialog: &FileDialog) -> u32 {
    let mut options = FOS_FORCEFILESYSTEM | FOS_NOCHANGEDIR | FOS_PATHMUSTEXIST;
    match kind {
        DialogKind::OpenFiles => {
            options |= FOS_FILEMUSTEXIST;
            if dialog.multiple {
                options |= FOS_ALLOWMULTISELECT;
            }
        }
        DialogKind::PickFolder => options |= FOS_PICKFOLDERS,
        DialogKind::SaveFile => options |= FOS_OVERWRITEPROMPT,
    }
    options
}

/// COM for this thread for as long as it is held, if the thread had none.
struct ComForCall {
    initialised: bool,
}

impl Drop for ComForCall {
    fn drop(&mut self) {
        if self.initialised {
            // SAFETY: balances the successful CoInitializeEx below, on the
            // same thread.
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// A dialog object of `kind`, configured as `dialog` says.
pub(crate) struct Prepared {
    pub(crate) dialog: ComPtr,
    /// The strings the filters point into, kept while the dialog lives.
    _filters: Vec<(Vec<u16>, Vec<u16>)>,
    /// Kept for the call that made it, so that COM outlives the dialog.
    _com: ComForCall,
}

// What a configured dialog holds, read back by the tests (the testing
// feature) that cannot show it.
#[cfg_attr(not(feature = "testing"), allow(dead_code))]
impl Prepared {
    fn table(&self) -> &IFileDialogVtbl {
        // SAFETY: both dialogs' tables start with IFileDialog's.
        unsafe { self.dialog.vtable::<IFileDialogVtbl>() }
    }

    /// The dialog's options now (`GetOptions`).
    pub(crate) fn options(&self) -> Result<u32, WindowError> {
        let mut options = 0;
        // SAFETY: a live dialog; `options` is a valid place.
        check("IFileDialog::GetOptions", unsafe {
            (self.table().get_options)(self.dialog.as_raw(), &mut options)
        })?;
        Ok(options)
    }

    /// The file type chosen, from 1 (`GetFileTypeIndex`).
    pub(crate) fn file_type_index(&self) -> Result<u32, WindowError> {
        let mut index = 0;
        // SAFETY: a live dialog; `index` is a valid place.
        check("IFileDialog::GetFileTypeIndex", unsafe {
            (self.table().get_file_type_index)(self.dialog.as_raw(), &mut index)
        })?;
        Ok(index)
    }

    /// The folder the dialog will start in (`GetFolder`), if one is set.
    pub(crate) fn folder(&self) -> Result<Option<PathBuf>, WindowError> {
        let mut item = ptr::null_mut();
        // SAFETY: a live dialog; `item` receives a reference we own.
        let result = unsafe { (self.table().get_folder)(self.dialog.as_raw(), &mut item) };
        if result < 0 {
            return Ok(None);
        }
        // SAFETY: GetFolder gave an IShellItem reference.
        let Some(item) = (unsafe { ComPtr::from_raw(item) }) else {
            return Ok(None);
        };
        path_of(&item).map(Some)
    }
}

/// Takes a string the system allocated with `CoTaskMemAlloc`.
///
/// # Safety
///
/// `text` must be null or such a NUL-terminated string, not used after.
unsafe fn take_string(text: PWSTR) -> Option<OsString> {
    if text.is_null() {
        return None;
    }
    let mut len = 0;
    // SAFETY: the string ends with a NUL.
    while unsafe { *text.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` units were read above.
    let units = unsafe { std::slice::from_raw_parts(text, len) };
    let owned = OsString::from_wide(units);
    // SAFETY: the system's allocation, freed once.
    unsafe {
        CoTaskMemFree(text.cast::<c_void>().cast_const());
    }
    Some(owned)
}

/// The file-system path of a shell item.
fn path_of(item: &ComPtr) -> Result<PathBuf, WindowError> {
    let mut name: PWSTR = ptr::null_mut();
    // SAFETY: a live IShellItem; `name` receives a string the caller frees.
    let result = unsafe {
        (item.vtable::<IShellItemVtbl>().get_display_name)(
            item.as_raw(),
            SIGDN_FILESYSPATH,
            &mut name,
        )
    };
    check("IShellItem::GetDisplayName", result)?;
    // SAFETY: the item gave a NUL-terminated string, freed once read.
    unsafe { take_string(name) }
        .map(PathBuf::from)
        .ok_or(WindowError::Platform {
            call: "IShellItem::GetDisplayName",
            code: 0,
        })
}

/// A path as the shell parses it: backslashes, and no `\\?\` prefix.
pub(crate) fn shell_path(path: &Path) -> String {
    let path = path.to_string_lossy().replace('/', "\\");
    match path.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => path
            .strip_prefix(r"\\?\")
            .map_or(path.clone(), str::to_owned),
    }
}

/// A shell item for `path`.
fn item_for(path: &Path) -> Result<ComPtr, WindowError> {
    let path = wide(&shell_path(path));
    let mut item = ptr::null_mut();
    // SAFETY: a NUL-terminated path, no bind context, and a valid place
    // for the item, whose reference we then own.
    let result = unsafe {
        SHCreateItemFromParsingName(path.as_ptr(), ptr::null_mut(), &IID_ISHELLITEM, &mut item)
    };
    check("SHCreateItemFromParsingName", result)?;
    // SAFETY: the call gave a reference on success.
    unsafe { ComPtr::from_raw(item) }.ok_or(WindowError::Platform {
        call: "SHCreateItemFromParsingName",
        code: 0,
    })
}

/// Makes the dialog object for `kind`, with COM for the call if the thread
/// has none.
fn create(kind: DialogKind) -> Result<(ComPtr, ComForCall), WindowError> {
    let (class, interface): (&GUID, &GUID) = match kind {
        DialogKind::SaveFile => (&FileSaveDialog, &IID_IFILESAVEDIALOG),
        _ => (&FileOpenDialog, &IID_IFILEOPENDIALOG),
    };
    let make = || {
        let mut object = ptr::null_mut();
        // SAFETY: documented class and interface ids, no outer object, and
        // a valid place for the reference we then own.
        let result = unsafe {
            CoCreateInstance(
                class,
                ptr::null_mut(),
                CLSCTX_INPROC_SERVER,
                interface,
                &mut object,
            )
        };
        (result, object)
    };
    let mut com = ComForCall { initialised: false };
    let (mut result, mut object) = make();
    if result == CO_E_NOTINITIALIZED {
        // SAFETY: the reserved argument is null; balanced in ComForCall's
        // drop, on this thread.
        let init = unsafe {
            CoInitializeEx(
                ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        check("CoInitializeEx", init)?;
        com.initialised = true;
        (result, object) = make();
    }
    check("CoCreateInstance", result)?;
    // SAFETY: CoCreateInstance gave a reference on success.
    let object = unsafe { ComPtr::from_raw(object) }.ok_or(WindowError::Platform {
        call: "CoCreateInstance",
        code: 0,
    })?;
    Ok((object, com))
}

/// Makes and configures the dialog `kind` for `dialog`, without showing it.
pub(crate) fn prepare(kind: DialogKind, dialog: &FileDialog) -> Result<Prepared, WindowError> {
    let (object, com) = create(kind)?;
    let mut prepared = Prepared {
        dialog: object,
        _filters: Vec::new(),
        _com: com,
    };
    let raw = prepared.dialog.as_raw();
    // SAFETY: both dialogs' tables start with IFileDialog's.
    let table = unsafe { prepared.dialog.vtable::<IFileDialogVtbl>() };

    let mut options = 0;
    // SAFETY: a live dialog; `options` is a valid place.
    check("IFileDialog::GetOptions", unsafe {
        (table.get_options)(raw, &mut options)
    })?;
    // SAFETY: a live dialog.
    check("IFileDialog::SetOptions", unsafe {
        (table.set_options)(raw, options | options_for(kind, dialog))
    })?;

    if kind != DialogKind::PickFolder && !dialog.filters.is_empty() {
        let strings: Vec<(Vec<u16>, Vec<u16>)> = dialog
            .filters
            .iter()
            .map(|filter| (wide(&filter.name), wide(&filter.pattern())))
            .collect();
        let specs: Vec<COMDLG_FILTERSPEC> = strings
            .iter()
            .map(|(name, spec)| COMDLG_FILTERSPEC {
                pszName: name.as_ptr(),
                pszSpec: spec.as_ptr(),
            })
            .collect();
        // SAFETY: `specs` points into `strings`, which the dialog keeps
        // alive with it below.
        check("IFileDialog::SetFileTypes", unsafe {
            (table.set_file_types)(raw, specs.len() as u32, specs.as_ptr())
        })?;
        // SAFETY: a live dialog; types are numbered from 1.
        check("IFileDialog::SetFileTypeIndex", unsafe {
            (table.set_file_type_index)(raw, 1)
        })?;
        prepared._filters = strings;
    }
    if let Some(title) = &dialog.title {
        let title = wide(title);
        // SAFETY: a NUL-terminated string the dialog copies.
        check("IFileDialog::SetTitle", unsafe {
            (table.set_title)(raw, title.as_ptr())
        })?;
    }
    if kind == DialogKind::SaveFile {
        if let Some(name) = &dialog.file_name {
            let name = wide(name);
            // SAFETY: a NUL-terminated string the dialog copies.
            check("IFileDialog::SetFileName", unsafe {
                (table.set_file_name)(raw, name.as_ptr())
            })?;
        }
        if let Some(extension) = dialog.default_extension() {
            let extension = wide(extension);
            // SAFETY: a NUL-terminated string the dialog copies.
            check("IFileDialog::SetDefaultExtension", unsafe {
                (table.set_default_extension)(raw, extension.as_ptr())
            })?;
        }
    }
    if let Some(folder) = &dialog.folder {
        // A folder that does not exist is left to the system's choice.
        if let Ok(item) = item_for(folder) {
            // SAFETY: a live dialog and a live item, which it references.
            check("IFileDialog::SetFolder", unsafe {
                (table.set_folder)(raw, item.as_raw())
            })?;
        }
    }
    Ok(prepared)
}

/// Shows the dialog `kind` for `dialog`, modal to the top-level window of
/// `hwnd`, and returns the paths chosen, or `None` if it was cancelled.
pub(crate) fn show(
    hwnd: HWND,
    kind: DialogKind,
    dialog: &FileDialog,
) -> Result<Option<Vec<PathBuf>>, WindowError> {
    let prepared = prepare(kind, dialog)?;
    // SAFETY: GetAncestor accepts any window.
    let owner = unsafe { GetAncestor(hwnd, GA_ROOT) };
    let raw = prepared.dialog.as_raw();
    // SAFETY: a live dialog and our window's top-level window. The dialog
    // runs a modal loop on this thread until it closes.
    let shown = unsafe { (prepared.table().show)(raw, owner) };
    if shown == CANCELLED {
        return Ok(None);
    }
    check("IModalWindow::Show", shown)?;
    if kind == DialogKind::OpenFiles {
        // SAFETY: an open dialog's table starts with IFileOpenDialog's.
        let table = unsafe { prepared.dialog.vtable::<IFileOpenDialogVtbl>() };
        let mut array = ptr::null_mut();
        // SAFETY: a live dialog; `array` receives a reference we own.
        check("IFileOpenDialog::GetResults", unsafe {
            (table.get_results)(raw, &mut array)
        })?;
        // SAFETY: GetResults gave an IShellItemArray reference.
        let Some(array) = (unsafe { ComPtr::from_raw(array) }) else {
            return Ok(None);
        };
        // SAFETY: the array's table.
        let items = unsafe { array.vtable::<IShellItemArrayVtbl>() };
        let mut count = 0;
        // SAFETY: a live array; `count` is a valid place.
        check("IShellItemArray::GetCount", unsafe {
            (items.get_count)(array.as_raw(), &mut count)
        })?;
        let mut paths = Vec::with_capacity(count as usize);
        for index in 0..count {
            let mut item = ptr::null_mut();
            // SAFETY: a live array and an index below its count.
            check("IShellItemArray::GetItemAt", unsafe {
                (items.get_item_at)(array.as_raw(), index, &mut item)
            })?;
            // SAFETY: GetItemAt gave an IShellItem reference.
            if let Some(item) = unsafe { ComPtr::from_raw(item) } {
                paths.push(path_of(&item)?);
            }
        }
        return Ok(Some(paths));
    }
    let mut item = ptr::null_mut();
    // SAFETY: a live dialog; `item` receives a reference we own.
    check("IFileDialog::GetResult", unsafe {
        (prepared.table().get_result)(raw, &mut item)
    })?;
    // SAFETY: GetResult gave an IShellItem reference.
    let Some(item) = (unsafe { ComPtr::from_raw(item) }) else {
        return Ok(None);
    };
    Ok(Some(vec![path_of(&item)?]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_reach_the_shell_with_backslashes_and_no_long_prefix() {
        assert_eq!(shell_path(Path::new("C:/Samples/Kits")), r"C:\Samples\Kits");
        assert_eq!(shell_path(Path::new(r"\\?\C:\Samples")), r"C:\Samples");
        assert_eq!(
            shell_path(Path::new(r"\\?\UNC\server\share\x")),
            r"\\server\share\x"
        );
        assert_eq!(shell_path(Path::new(r"\\server\share")), r"\\server\share");
    }
}
