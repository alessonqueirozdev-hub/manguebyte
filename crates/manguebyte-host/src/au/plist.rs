//! Property lists in their XML form, read and written.
//!
//! Two things an Audio Unit host handles are property lists: the
//! `Info.plist` of a component bundle, whose `AudioComponents` entry says
//! which units the bundle holds, and a unit's state, the dictionary of
//! `kAudioUnitProperty_ClassInfo`, which hosts keep as a property list (an
//! `.aupreset` file is one). The form is Apple's: the document type
//! `-//Apple//DTD PLIST 1.0//EN`, as `CFPropertyList.h` and the
//! *Information Property List Key Reference* describe it, with the eight
//! kinds of value below.
//!
//! The reader takes any document as hostile input: it has limits on depth
//! and length, and answers with an error, never a panic. On macOS a binary
//! property list is turned into this form by CoreFoundation first.

use std::fmt::Write as _;

/// Deepest a document may nest arrays and dictionaries.
const MAX_DEPTH: usize = 64;

/// A value of a property list.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    String(String),
    Integer(i64),
    Real(f64),
    Bool(bool),
    Data(Vec<u8>),
    /// A date, kept as the text the document gives it.
    Date(String),
    Array(Vec<Value>),
    /// A dictionary, in the order of its keys in the document.
    Dict(Vec<(String, Value)>),
}

impl Value {
    /// The value of `key`, if this is a dictionary that has it.
    pub(crate) fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Self::Dict(entries) => entries
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(number) => Some(*number),
            _ => None,
        }
    }

    pub(crate) fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(values) => Some(values),
            _ => None,
        }
    }
}

// --- Writing ----------------------------------------------------------------------

/// The XML document holding `value`, as CoreFoundation writes one.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn write(value: &Value) -> Vec<u8> {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n",
    );
    write_value(&mut out, value, 0);
    out.push_str("</plist>\n");
    out.into_bytes()
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push('\t');
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn escape(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML 1.0 cannot carry most control characters, even escaped.
            c if c.is_control() && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn write_value(out: &mut String, value: &Value, depth: usize) {
    indent(out, depth);
    match value {
        Value::String(text) => {
            out.push_str("<string>");
            escape(out, text);
            out.push_str("</string>\n");
        }
        Value::Integer(number) => {
            let _ = writeln!(out, "<integer>{number}</integer>");
        }
        Value::Real(number) => {
            let _ = writeln!(out, "<real>{number:?}</real>");
        }
        Value::Bool(true) => out.push_str("<true/>\n"),
        Value::Bool(false) => out.push_str("<false/>\n"),
        Value::Data(bytes) => {
            out.push_str("<data>\n");
            for line in base64(bytes).as_bytes().chunks(68) {
                indent(out, depth);
                out.push_str(std::str::from_utf8(line).unwrap_or_default());
                out.push('\n');
            }
            indent(out, depth);
            out.push_str("</data>\n");
        }
        Value::Date(text) => {
            out.push_str("<date>");
            escape(out, text);
            out.push_str("</date>\n");
        }
        Value::Array(values) => {
            out.push_str("<array>\n");
            for value in values {
                write_value(out, value, depth + 1);
            }
            indent(out, depth);
            out.push_str("</array>\n");
        }
        Value::Dict(entries) => {
            out.push_str("<dict>\n");
            for (key, value) in entries {
                indent(out, depth + 1);
                out.push_str("<key>");
                escape(out, key);
                out.push_str("</key>\n");
                write_value(out, value, depth + 1);
            }
            indent(out, depth);
            out.push_str("</dict>\n");
        }
    }
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

#[cfg_attr(target_os = "macos", allow(dead_code))]
fn base64(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let word = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0..4 {
            if index <= chunk.len() {
                out.push(char::from(BASE64[(word >> (18 - 6 * index)) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut word = 0u32;
    let mut bits = 0u32;
    let mut ended = false;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                ended = true;
                continue;
            }
            byte if byte.is_ascii_whitespace() => continue,
            _ => return None,
        };
        if ended {
            return None;
        }
        word = (word << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((word >> bits) as u8);
            word &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// --- Reading ------------------------------------------------------------------------

/// What is wrong with a document, for the error a caller reports.
pub(crate) type ReadError = String;

/// Reads a property list in its XML form.
pub(crate) fn read(bytes: &[u8]) -> Result<Value, ReadError> {
    let text = std::str::from_utf8(bytes).map_err(|_| "the property list is not UTF-8")?;
    let mut reader = Reader { text, at: 0 };
    reader.prolog()?;
    let root = match reader.open()? {
        Tag::Open("plist") => {
            let value = reader.value(0)?;
            reader.close("plist")?;
            value
        }
        // A document may also hold a value with no `plist` around it.
        tag => reader.value_from(tag, 0)?,
    };
    reader.skip_space_and_comments()?;
    if reader.at < reader.text.len() {
        return Err("there is more after the property list".into());
    }
    Ok(root)
}

/// One tag of the document.
#[derive(Debug, PartialEq)]
enum Tag<'a> {
    Open(&'a str),
    Empty(&'a str),
    Close(&'a str),
}

struct Reader<'a> {
    text: &'a str,
    at: usize,
}

impl<'a> Reader<'a> {
    fn rest(&self) -> &'a str {
        &self.text[self.at..]
    }

    fn skip_space(&mut self) {
        let rest = self.rest();
        self.at += rest.len() - rest.trim_start().len();
    }

    fn skip_past(&mut self, end: &str) -> Result<(), ReadError> {
        match self.rest().find(end) {
            Some(found) => {
                self.at += found + end.len();
                Ok(())
            }
            None => Err(format!("{end} is missing")),
        }
    }

    fn skip_space_and_comments(&mut self) -> Result<(), ReadError> {
        loop {
            self.skip_space();
            if self.rest().starts_with("<!--") {
                self.skip_past("-->")?;
            } else {
                return Ok(());
            }
        }
    }

    /// Skips the XML declaration, the document type and comments.
    fn prolog(&mut self) -> Result<(), ReadError> {
        if self.rest().starts_with('\u{FEFF}') {
            self.at += '\u{FEFF}'.len_utf8();
        }
        loop {
            self.skip_space_and_comments()?;
            if self.rest().starts_with("<?") {
                self.skip_past("?>")?;
            } else if self.rest().starts_with("<!DOCTYPE") {
                self.skip_past(">")?;
            } else {
                return Ok(());
            }
        }
    }

    /// Reads the next tag, after space and comments.
    fn open(&mut self) -> Result<Tag<'a>, ReadError> {
        self.skip_space_and_comments()?;
        let rest = self.rest();
        if !rest.starts_with('<') {
            return Err("a tag was expected".into());
        }
        let end = rest.find('>').ok_or("a tag does not end")?;
        let inside = &rest[1..end];
        self.at += end + 1;
        if let Some(name) = inside.strip_prefix('/') {
            return Ok(Tag::Close(name.trim()));
        }
        let (inside, empty) = match inside.strip_suffix('/') {
            Some(inside) => (inside, true),
            None => (inside, false),
        };
        // Attributes, such as `version`, are not needed.
        let name = inside.split_ascii_whitespace().next().unwrap_or_default();
        if name.is_empty() {
            return Err("a tag has no name".into());
        }
        Ok(if empty {
            Tag::Empty(name)
        } else {
            Tag::Open(name)
        })
    }

    fn close(&mut self, name: &str) -> Result<(), ReadError> {
        match self.open()? {
            Tag::Close(found) if found == name => Ok(()),
            _ => Err(format!("<{name}> is not closed")),
        }
    }

    /// The text up to the closing tag of `name`, with its entities read.
    fn text_of(&mut self, name: &str) -> Result<String, ReadError> {
        let closing = format!("</{name}>");
        let rest = self.rest();
        let end = rest
            .find(&closing)
            .ok_or_else(|| format!("<{name}> is not closed"))?;
        let raw = &rest[..end];
        if raw.contains('<') {
            return Err(format!("<{name}> holds markup"));
        }
        self.at += end + closing.len();
        unescape(raw)
    }

    fn value(&mut self, depth: usize) -> Result<Value, ReadError> {
        let tag = self.open()?;
        self.value_from(tag, depth)
    }

    fn value_from(&mut self, tag: Tag<'a>, depth: usize) -> Result<Value, ReadError> {
        if depth > MAX_DEPTH {
            return Err("the property list nests too deeply".into());
        }
        Ok(match tag {
            Tag::Empty("true") => Value::Bool(true),
            Tag::Empty("false") => Value::Bool(false),
            Tag::Empty("string") => Value::String(String::new()),
            Tag::Empty("data") => Value::Data(Vec::new()),
            Tag::Empty("array") => Value::Array(Vec::new()),
            Tag::Empty("dict") => Value::Dict(Vec::new()),
            Tag::Open("true") => {
                self.close("true")?;
                Value::Bool(true)
            }
            Tag::Open("false") => {
                self.close("false")?;
                Value::Bool(false)
            }
            Tag::Open("string") => Value::String(self.text_of("string")?),
            Tag::Open("date") => Value::Date(self.text_of("date")?),
            Tag::Open("integer") => {
                let text = self.text_of("integer")?;
                let text = text.trim();
                let number = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                    Some(hex) => i64::from_str_radix(hex, 16).ok(),
                    None => text
                        .parse::<i64>()
                        .ok()
                        .or_else(|| text.parse::<u64>().ok().map(|n| n as i64)),
                };
                Value::Integer(number.ok_or("an integer is not a number")?)
            }
            Tag::Open("real") => {
                let text = self.text_of("real")?;
                Value::Real(text.trim().parse().map_err(|_| "a real is not a number")?)
            }
            Tag::Open("data") => {
                let text = self.text_of("data")?;
                Value::Data(unbase64(&text).ok_or("data is not base 64")?)
            }
            Tag::Open("array") => {
                let mut values = Vec::new();
                loop {
                    match self.open()? {
                        Tag::Close("array") => break,
                        tag => values.push(self.value_from(tag, depth + 1)?),
                    }
                }
                Value::Array(values)
            }
            Tag::Open("dict") => {
                let mut entries = Vec::new();
                loop {
                    match self.open()? {
                        Tag::Close("dict") => break,
                        Tag::Open("key") => {
                            let key = self.text_of("key")?;
                            let value = self.value(depth + 1)?;
                            entries.push((key, value));
                        }
                        Tag::Empty("key") => {
                            let value = self.value(depth + 1)?;
                            entries.push((String::new(), value));
                        }
                        _ => return Err("a dictionary holds something other than keys".into()),
                    }
                }
                Value::Dict(entries)
            }
            Tag::Open(name) | Tag::Empty(name) => {
                return Err(format!("<{name}> is not a property list value"));
            }
            Tag::Close(name) => return Err(format!("</{name}> closes nothing")),
        })
    }
}

/// Reads XML's five named entities and character references.
fn unescape(raw: &str) -> Result<String, ReadError> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(';').ok_or("an entity does not end")?;
        let name = &after[..end];
        let c = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = if let Some(hex) = name.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()
                } else if let Some(decimal) = name.strip_prefix('#') {
                    decimal.parse().ok()
                } else {
                    None
                };
                number
                    .and_then(char::from_u32)
                    .ok_or("an entity is not one XML knows")?
            }
        };
        out.push(c);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_goes_out_and_comes_back() {
        let value = Value::Dict(vec![
            ("name".into(), Value::String("Mangue <Pluck> & co".into())),
            ("version".into(), Value::Integer(-1)),
            ("big".into(), Value::Integer(i64::MAX)),
            ("ratio".into(), Value::Real(0.25)),
            ("on".into(), Value::Bool(true)),
            ("off".into(), Value::Bool(false)),
            ("when".into(), Value::Date("2026-10-20T00:00:00Z".into())),
            (
                "data".into(),
                Value::Data((0..=255u8).chain([1, 2]).collect()),
            ),
            ("empty".into(), Value::Data(Vec::new())),
            (
                "list".into(),
                Value::Array(vec![Value::Integer(7), Value::Dict(Vec::new())]),
            ),
        ]);
        let written = write(&value);
        assert_eq!(read(&written).unwrap(), value);
        for size in 0..7 {
            let bytes: Vec<u8> = (0..size).map(|n| n as u8 * 37).collect();
            assert_eq!(unbase64(&base64(&bytes)).unwrap(), bytes, "{size} bytes");
        }
    }

    #[test]
    fn what_other_writers_write_is_read() {
        // As CoreFoundation writes an Audio Unit's state, with spaces for
        // indentation, a comment, entities and a hexadecimal integer.
        let text = "\u{FEFF}<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
            <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"x\">\n\
            <!-- saved -->\n<plist version=\"1.0\">\n<dict>\n\
              <key>data</key>\n  <data>\n  AAEC\n  /w==\n  </data>\n\
              <key>name</key><string>A &amp; B &#233;&#x41;</string>\n\
              <key>type</key><integer>0x61756D75</integer>\n\
              <key>blank</key><string/>\n\
              <key>tags</key><array><string>Synthesizer</string></array>\n\
            </dict>\n</plist>\n";
        let value = read(text.as_bytes()).unwrap();
        assert_eq!(value.get("data"), Some(&Value::Data(vec![0, 1, 2, 0xFF])));
        assert_eq!(value.get("name").and_then(Value::as_str), Some("A & B éA"));
        assert_eq!(
            value.get("type").and_then(Value::as_integer),
            Some(0x6175_6D75)
        );
        assert_eq!(value.get("blank").and_then(Value::as_str), Some(""));
        assert_eq!(
            value.get("tags").and_then(Value::as_array).unwrap().len(),
            1
        );
        assert_eq!(value.get("missing"), None);
    }

    #[test]
    fn broken_documents_are_errors() {
        let deep = format!(
            "<plist>{}{}</plist>",
            "<array>".repeat(MAX_DEPTH + 2),
            "</array>".repeat(MAX_DEPTH + 2)
        );
        for broken in [
            &b"\xFF\xFE"[..],
            b"",
            b"<plist><dict><key>a</key></dict></plist>",
            b"<plist><dict><string>a</string></dict></plist>",
            b"<plist><integer>ten</integer></plist>",
            b"<plist><real>x</real></plist>",
            b"<plist><data>!!</data></plist>",
            b"<plist><string>a &bogus; b</string></plist>",
            b"<plist><string>a</plist>",
            b"<plist><thing/></plist>",
            b"<plist><string>a</string></plist>trailing",
            b"<plist><array><string>a</string>",
            b"<plist><string><b>bold</b></string></plist>",
            deep.as_bytes(),
        ] {
            assert!(
                read(broken).is_err(),
                "{:?}",
                String::from_utf8_lossy(broken)
            );
        }
    }
}
