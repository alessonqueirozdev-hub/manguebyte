//! A unit's state: the property list of `kAudioUnitProperty_ClassInfo`
//! (`AudioUnitProperties.h`), as bytes a session can keep.
//!
//! The bytes are the XML form of that property list, which is what an
//! `.aupreset` file holds. On macOS CoreFoundation writes and reads them
//! (`CFPropertyListCreateData` and `CFPropertyListCreateWithData`), so
//! every key a unit puts in its dictionary goes out and comes back. On
//! other systems, where a unit can only be hosted in process from its
//! factory, the dictionary is the CoreFoundation stand-in of the
//! framework's AU wrapper, which cannot list its keys: the keys Apple's
//! base classes write (`version`, `type`, `subtype`, `manufacturer`,
//! `name` and `data`) are read and written by this crate's own property
//! list code, into the same form.

use super::sys::*;
use super::unit::Unit;
use super::{AuError, cf};

/// Largest state taken from or given to a unit.
pub const MAX_STATE_BYTES: usize = 64 << 20;

/// The unit's state, as the XML of its `ClassInfo`.
pub(crate) fn save(unit: &Unit) -> Result<Vec<u8>, AuError> {
    let list: CFPropertyListRef = unit
        .get(kAudioUnitProperty_ClassInfo, kAudioUnitScope_Global, 0)
        .map_err(|status| AuError::Status {
            call: "getting ClassInfo",
            status,
        })?;
    let list = cf::Owned::new(list).ok_or(AuError::StateSaveFailed)?;
    // SAFETY: a live property list the unit handed over.
    let bytes = unsafe { to_bytes(list.as_ptr()) }?;
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AuError::StateTooLarge);
    }
    Ok(bytes)
}

/// Gives the unit back a state [`save`] made.
pub(crate) fn load(unit: &Unit, bytes: &[u8]) -> Result<(), AuError> {
    if bytes.len() > MAX_STATE_BYTES {
        return Err(AuError::StateTooLarge);
    }
    let list = from_bytes(bytes)?;
    let pointer: CFPropertyListRef = list.as_ptr();
    unit.set(
        kAudioUnitProperty_ClassInfo,
        kAudioUnitScope_Global,
        0,
        &pointer,
    )
    .map_err(|status| AuError::Status {
        call: "setting ClassInfo",
        status,
    })
}

/// # Safety
///
/// `list` must be a live property list.
#[cfg(target_os = "macos")]
unsafe fn to_bytes(list: CFPropertyListRef) -> Result<Vec<u8>, AuError> {
    // SAFETY: a live property list, written as XML, with no error wanted.
    let data = unsafe {
        CFPropertyListCreateData(
            std::ptr::null(),
            list,
            kCFPropertyListXMLFormat_v1_0,
            0,
            std::ptr::null_mut(),
        )
    };
    let data = cf::Owned::new(data).ok_or(AuError::StateSaveFailed)?;
    // SAFETY: live data, readable for its length.
    unsafe {
        let length = usize::try_from(CFDataGetLength(data.as_ptr())).unwrap_or(0);
        if length > MAX_STATE_BYTES {
            return Err(AuError::StateTooLarge);
        }
        let start = CFDataGetBytePtr(data.as_ptr());
        if start.is_null() {
            return Ok(Vec::new());
        }
        Ok(std::slice::from_raw_parts(start, length).to_vec())
    }
}

#[cfg(target_os = "macos")]
fn from_bytes(bytes: &[u8]) -> Result<cf::Owned, AuError> {
    // SAFETY: the bytes are readable for their length.
    let data = cf::Owned::new(unsafe {
        CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex)
    })
    .ok_or(AuError::BadState)?;
    // SAFETY: live data; the list made from it is the caller's.
    let list = unsafe {
        CFPropertyListCreateWithData(
            std::ptr::null(),
            data.as_ptr(),
            kCFPropertyListImmutable,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    cf::Owned::new(list).ok_or(AuError::BadState)
}

/// The keys Apple's base classes put in a unit's state, read where the
/// dictionary cannot be listed.
#[cfg(not(target_os = "macos"))]
const KEYS: [&str; 6] = ["version", "type", "subtype", "manufacturer", "name", "data"];

/// # Safety
///
/// `list` must be a live property list.
#[cfg(not(target_os = "macos"))]
unsafe fn to_bytes(list: CFPropertyListRef) -> Result<Vec<u8>, AuError> {
    use super::plist::{self, Value};
    // SAFETY: per this function's contract.
    if !unsafe { cf::is(list, CFDictionaryGetTypeID()) } {
        return Err(AuError::StateSaveFailed);
    }
    let mut entries = Vec::new();
    for key in KEYS {
        let name = cf::string(key).ok_or(AuError::StateSaveFailed)?;
        // SAFETY: a live dictionary and key; the dictionary keeps the
        // value, which is read before either goes.
        let value = unsafe {
            let found = CFDictionaryGetValue(list, name.as_ptr());
            if found.is_null() {
                continue;
            }
            let kind = CFGetTypeID(found);
            if kind == CFStringGetTypeID() {
                cf::read_string(found).map(Value::String)
            } else if kind == CFNumberGetTypeID() {
                let mut number = 0i64;
                (CFNumberGetValue(found, kCFNumberSInt64Type, (&raw mut number).cast()) != 0)
                    .then_some(Value::Integer(number))
            } else if kind == CFDataGetTypeID() {
                let length = usize::try_from(CFDataGetLength(found)).unwrap_or(0);
                if length > MAX_STATE_BYTES {
                    return Err(AuError::StateTooLarge);
                }
                let start = CFDataGetBytePtr(found);
                Some(Value::Data(if start.is_null() || length == 0 {
                    Vec::new()
                } else {
                    std::slice::from_raw_parts(start, length).to_vec()
                }))
            } else {
                None
            }
        };
        if let Some(value) = value {
            entries.push((key.to_owned(), value));
        }
    }
    Ok(plist::write(&Value::Dict(entries)))
}

#[cfg(not(target_os = "macos"))]
fn from_bytes(bytes: &[u8]) -> Result<cf::Owned, AuError> {
    use super::plist;
    let value = plist::read(bytes).map_err(|_| AuError::BadState)?;
    if !matches!(value, plist::Value::Dict(_)) {
        return Err(AuError::BadState);
    }
    to_object(&value).ok_or(AuError::BadState)
}

/// The CoreFoundation object for a value: strings, numbers, data and
/// dictionaries of them, which is what the stand-in has.
#[cfg(not(target_os = "macos"))]
fn to_object(value: &super::plist::Value) -> Option<cf::Owned> {
    use super::plist::Value;
    match value {
        Value::String(text) => cf::string(text),
        Value::Integer(number) => cf::Owned::new(
            // SAFETY: an `i64`, of the type named.
            unsafe {
                CFNumberCreate(
                    std::ptr::null(),
                    kCFNumberSInt64Type,
                    std::ptr::from_ref(number).cast(),
                )
            },
        ),
        // SAFETY: the bytes are readable for their length.
        Value::Data(bytes) => cf::Owned::new(unsafe {
            CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as CFIndex)
        }),
        Value::Dict(entries) => {
            // SAFETY: the callbacks for a dictionary of objects.
            let dictionary = cf::Owned::new(
                unsafe {
                    CFDictionaryCreateMutable(
                        std::ptr::null(),
                        0,
                        type_dictionary_key_callbacks(),
                        type_dictionary_value_callbacks(),
                    )
                }
                .cast_const(),
            )?;
            for (key, value) in entries {
                // What the stand-in cannot hold is left out.
                let (Some(key), Some(value)) = (cf::string(key), to_object(value)) else {
                    continue;
                };
                // SAFETY: a dictionary made here and live objects, which
                // it now holds references to.
                unsafe {
                    CFDictionarySetValue(
                        dictionary.as_ptr().cast_mut(),
                        key.as_ptr(),
                        value.as_ptr(),
                    );
                }
            }
            Some(dictionary)
        }
        _ => None,
    }
}
