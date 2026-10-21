//! The few CoreFoundation objects a host hands to a unit or takes from it:
//! strings, for names and parameter text.
//!
//! The functions are those the AU wrapper declares (`manguebyte_au::sys`):
//! CoreFoundation's own on macOS, the wrapper's stand-in elsewhere, so
//! that an object made by a unit hosted in process is one this side can
//! read.

use super::sys::*;

/// Longest string read from a unit.
pub(crate) const MAX_STRING_BYTES: usize = 4_096;

/// A CoreFoundation object this side holds one reference to, released when
/// it is dropped.
pub(crate) struct Owned(CFTypeRef);

impl Owned {
    /// Takes over a reference the caller owns. `None` for null.
    pub(crate) fn new(object: CFTypeRef) -> Option<Self> {
        (!object.is_null()).then_some(Self(object))
    }

    pub(crate) fn as_ptr(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: this value holds one reference to a live object.
        unsafe { CFRelease(self.0) };
    }
}

/// A new string holding `text`.
pub(crate) fn string(text: &str) -> Option<Owned> {
    // SAFETY: the bytes are `text`'s, of the length given, in UTF-8.
    Owned::new(unsafe {
        CFStringCreateWithBytes(
            std::ptr::null(),
            text.as_ptr(),
            text.len() as CFIndex,
            kCFStringEncodingUTF8,
            0,
        )
    })
}

/// Whether `object` is of the type `kind`.
///
/// # Safety
///
/// `object` must be null or a live CoreFoundation object.
pub(crate) unsafe fn is(object: CFTypeRef, kind: CFTypeID) -> bool {
    // SAFETY: per this function's contract.
    !object.is_null() && unsafe { CFGetTypeID(object) } == kind
}

/// The text of a string, if it is one and not too long.
///
/// # Safety
///
/// `string` must be null or a live CoreFoundation object.
pub(crate) unsafe fn read_string(string: CFTypeRef) -> Option<String> {
    // SAFETY: per this function's contract.
    unsafe {
        if !is(string, CFStringGetTypeID()) {
            return None;
        }
        let length = CFStringGetLength(string);
        let room = CFStringGetMaximumSizeForEncoding(length, kCFStringEncodingUTF8);
        let room = usize::try_from(room).ok()?.saturating_add(1);
        if room > MAX_STRING_BYTES * 4 + 1 {
            return None;
        }
        let mut buffer = vec![0u8; room];
        if CFStringGetCString(
            string,
            buffer.as_mut_ptr().cast(),
            room as CFIndex,
            kCFStringEncodingUTF8,
        ) == 0
        {
            return None;
        }
        let end = buffer.iter().position(|&byte| byte == 0).unwrap_or(room);
        buffer.truncate(end.min(MAX_STRING_BYTES));
        // A string cut short may end inside a character.
        Some(String::from_utf8_lossy(&buffer).into_owned())
    }
}

/// The text of a string the caller owns, which is released.
///
/// # Safety
///
/// As for [`read_string`], with a reference the caller hands over.
pub(crate) unsafe fn take_string(string: CFTypeRef) -> Option<String> {
    let owned = Owned::new(string)?;
    // SAFETY: per this function's contract.
    unsafe { read_string(owned.as_ptr()) }
}
