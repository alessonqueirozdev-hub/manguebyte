//! The file of the binary this code is linked into.
//!
//! A plugin is a shared library that a host loads from wherever it was
//! installed, and some plugins need to know where that is: to read data
//! stored next to the library or appended to its file, or to tell two
//! copies of one library apart. [`file()`] asks the system which loaded
//! binary holds an address of this crate. Every crate of the framework is
//! linked statically into the binary that uses it, so in a plugin that is
//! the plugin's own library, and in a program it is the program.
//!
//! | System | How |
//! |---|---|
//! | Windows | `GetModuleHandleExW` with `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS`, then `GetModuleFileNameW` |
//! | macOS, Linux and other Unix systems | `dladdr` |
//! | WebAssembly, Miri | Nothing: [`file()`] returns `None` |
//!
//! On macOS the result is the binary inside a bundle, such as
//! `Name.clap/Contents/MacOS/Name`, not the bundle's folder.

use std::path::PathBuf;

/// A static whose address is inside this binary. Its value is never read.
static ANCHOR: u8 = 0;

/// The file of the binary this code is linked into: the plugin's shared
/// library in a plugin, the executable in a program. `None` where the
/// system cannot say, or has no files.
///
/// The path is the one the binary was loaded through, which for a library
/// is the path its host passed to the loader. It is worked out on every
/// call; callers that need it often keep it.
///
/// ```
/// let file = manguebyte_core::module::file();
/// if let Some(file) = &file {
///     assert!(file.is_file());
/// }
/// ```
pub fn file() -> Option<PathBuf> {
    system::file_containing((&raw const ANCHOR).cast())
}

#[cfg(all(windows, not(miri)))]
mod system {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::ptr;

    /// `GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT`.
    const UNCHANGED_REFCOUNT: u32 = 0x2;
    /// `GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS`.
    const FROM_ADDRESS: u32 = 0x4;
    /// The longest path Windows has: 32,767 UTF-16 units and a NUL.
    const LONGEST_PATH: usize = 32_768;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut c_void) -> i32;
        fn GetModuleFileNameW(module: *mut c_void, name: *mut u16, size: u32) -> u32;
    }

    pub(super) fn file_containing(address: *const c_void) -> Option<PathBuf> {
        let mut module: *mut c_void = ptr::null_mut();
        // SAFETY: with FROM_ADDRESS the name is read as an address, and any
        // address inside a loaded module is accepted; the caller's is a
        // static of this binary. UNCHANGED_REFCOUNT takes no reference, so
        // there is nothing to release. `module` is a valid place for the
        // result.
        let found = unsafe {
            GetModuleHandleExW(
                FROM_ADDRESS | UNCHANGED_REFCOUNT,
                address.cast::<u16>(),
                &mut module,
            )
        };
        if found == 0 || module.is_null() {
            return None;
        }
        let mut room = 260;
        loop {
            let mut name = vec![0u16; room];
            // SAFETY: `module` is a module of this process, kept loaded by
            // the code running here; `name` has room for `room` units.
            let written = unsafe { GetModuleFileNameW(module, name.as_mut_ptr(), room as u32) };
            let written = written as usize;
            if written == 0 {
                return None;
            }
            // A name that fills the buffer was cut short: try a larger one.
            if written < room {
                name.truncate(written);
                return Some(PathBuf::from(OsString::from_wide(&name)));
            }
            if room >= LONGEST_PATH {
                return None;
            }
            room = (room * 4).min(LONGEST_PATH);
        }
    }
}

#[cfg(all(unix, not(miri)))]
mod system {
    use std::ffi::{CStr, OsStr, c_char, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::ptr;

    /// `Dl_info`, laid out the same on Linux, the BSDs and macOS.
    #[repr(C)]
    struct DlInfo {
        dli_fname: *const c_char,
        dli_fbase: *mut c_void,
        dli_sname: *const c_char,
        dli_saddr: *mut c_void,
    }

    // `dladdr` is in the C library itself since glibc 2.34, and in `libdl`
    // before; elsewhere it is in the C library.
    #[cfg_attr(all(target_os = "linux", target_env = "gnu"), link(name = "dl"))]
    unsafe extern "C" {
        fn dladdr(address: *const c_void, info: *mut DlInfo) -> c_int;
    }

    pub(super) fn file_containing(address: *const c_void) -> Option<PathBuf> {
        let mut info = DlInfo {
            dli_fname: ptr::null(),
            dli_fbase: ptr::null_mut(),
            dli_sname: ptr::null(),
            dli_saddr: ptr::null_mut(),
        };
        // SAFETY: `dladdr` reads nothing at the address, only looks it up
        // among the loaded objects, and writes `info`, which is ours.
        if unsafe { dladdr(address, &mut info) } == 0 || info.dli_fname.is_null() {
            return None;
        }
        // SAFETY: on success `dli_fname` is the NUL-terminated name of the
        // object, which stays loaded while this code runs; it is copied
        // before returning.
        let name = unsafe { CStr::from_ptr(info.dli_fname) };
        if name.is_empty() {
            return None;
        }
        Some(PathBuf::from(OsStr::from_bytes(name.to_bytes())))
    }
}

#[cfg(any(miri, not(any(windows, unix))))]
mod system {
    use std::ffi::c_void;
    use std::path::PathBuf;

    pub(super) fn file_containing(_address: *const c_void) -> Option<PathBuf> {
        None
    }
}

#[cfg(test)]
mod tests {
    /// In a test, the binary is the test's executable, which the standard
    /// library finds another way. On Unix systems `dladdr` names a
    /// program by what it was started as, which may not be a path at all,
    /// so there only a name that is a file is checked.
    #[test]
    #[cfg_attr(miri, ignore = "Miri has no loaded modules to ask")]
    fn the_file_is_the_test_executable() {
        let found = super::file();
        let expected = std::env::current_exe().unwrap().canonicalize().unwrap();
        if cfg!(windows) {
            let found = found.expect("the system names this binary");
            assert_eq!(found.canonicalize().unwrap(), expected);
        } else if let Some(found) = found.filter(|found| found.is_file()) {
            assert_eq!(found.canonicalize().unwrap(), expected);
        }
    }
}
