//! The plugin's URI, worked out from its id at compile time.

use std::ffi::CStr;

use manguebyte_plugin::Plugin;

/// What every plugin URI starts with. The rest is the plugin's id, with
/// every byte other than ASCII letters, digits, `-`, `.`, `_` and `~`
/// written as `%` and two upper-case hexadecimal digits.
pub const URI_PREFIX: &str = "urn:manguebyte:";

/// The URI of the property under which the plugin's state is stored
/// through the `state:interface` extension. Its value is an `atom:String`
/// holding the text [`save_state`](manguebyte_plugin::save_state) writes.
pub const STATE_KEY_URI: &str = "urn:manguebyte:lv2:state";

/// [`STATE_KEY_URI`] as a C string.
pub(crate) const STATE_KEY: &CStr = c"urn:manguebyte:lv2:state";

const fn is_kept(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// How many bytes the URI of the plugin with id `id` takes as a C string,
/// its NUL included. Used by [`descriptor!`](crate::descriptor).
#[doc(hidden)]
pub const fn uri_c_len(id: &str) -> usize {
    let id = id.as_bytes();
    let mut len = URI_PREFIX.len() + 1;
    let mut at = 0;
    while at < id.len() {
        len += if is_kept(id[at]) { 1 } else { 3 };
        at += 1;
    }
    len
}

/// The URI of the plugin with id `id` as a C string of `N` bytes, its NUL
/// included. Fails to compile if `N` is not [`uri_c_len`]`(id)`. Used by
/// [`descriptor!`](crate::descriptor).
#[doc(hidden)]
pub const fn uri_c_bytes<const N: usize>(id: &str) -> [u8; N] {
    assert!(N == uri_c_len(id), "wrong length for the plugin URI");
    let mut out = [0u8; N];
    let prefix = URI_PREFIX.as_bytes();
    let mut len = 0;
    while len < prefix.len() {
        out[len] = prefix[len];
        len += 1;
    }
    let id = id.as_bytes();
    let mut at = 0;
    while at < id.len() {
        let byte = id[at];
        if is_kept(byte) {
            out[len] = byte;
            len += 1;
        } else {
            out[len] = b'%';
            out[len + 1] = HEX[(byte >> 4) as usize];
            out[len + 2] = HEX[(byte & 0x0F) as usize];
            len += 3;
        }
        at += 1;
    }
    out
}

/// `bytes` as a C string. Fails to compile unless `bytes` ends with its
/// only NUL. Used by [`descriptor!`](crate::descriptor).
#[doc(hidden)]
pub const fn c_str(bytes: &'static [u8]) -> &'static CStr {
    match CStr::from_bytes_with_nul(bytes) {
        Ok(text) => text,
        Err(_) => panic!("not a C string"),
    }
}

/// The URI of the plugin with id `id`: [`URI_PREFIX`] and the id, encoded
/// as [`URI_PREFIX`] describes.
///
/// ```
/// assert_eq!(manguebyte_lv2::uri_for_id("org.example.gain"), "urn:manguebyte:org.example.gain");
/// assert_eq!(manguebyte_lv2::uri_for_id("a b/c"), "urn:manguebyte:a%20b%2Fc");
/// ```
pub fn uri_for_id(id: &str) -> String {
    format!("{URI_PREFIX}{}", percent_encode(id))
}

/// `text` with every byte other than ASCII letters, digits, `-`, `.`, `_`
/// and `~` written as `%` and two upper-case hexadecimal digits.
pub(crate) fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        if is_kept(byte) {
            out.push(char::from(byte));
        } else {
            out.push('%');
            out.push(char::from(HEX[usize::from(byte >> 4)]));
            out.push(char::from(HEX[usize::from(byte & 0x0F)]));
        }
    }
    out
}

/// The URI of plugin `P`, as its descriptor and its Turtle description
/// give it.
pub fn plugin_uri<P: Plugin>() -> String {
    uri_for_id(P::INFO.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compile_time_and_run_time_uris_agree() {
        const ID: &str = "org.ex ample/Ünï=1";
        const LEN: usize = uri_c_len(ID);
        const BYTES: [u8; LEN] = uri_c_bytes::<LEN>(ID);
        const URI: &CStr = c_str(&BYTES);
        assert_eq!(URI.to_str().unwrap(), uri_for_id(ID));
        assert_eq!(
            uri_for_id(ID),
            "urn:manguebyte:org.ex%20ample%2F%C3%9Cn%C3%AF%3D1"
        );
        assert_eq!(STATE_KEY.to_str().unwrap(), STATE_KEY_URI);
        // A NUL in an id is encoded too, so the C string never ends early.
        assert_eq!(uri_for_id("a\0b"), "urn:manguebyte:a%00b");
    }
}
