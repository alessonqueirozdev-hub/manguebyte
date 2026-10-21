//! What an Audio Unit component is called and says about itself: its three
//! codes, its name, maker and version, as a bundle's `Info.plist` or
//! AudioToolbox's registry gives them.

use std::fmt;
use std::str::FromStr;

use super::plist::{self, Value};
use super::sys::*;
use super::{AuError, MAX_INFO_PLIST_BYTES};
use crate::any::{Format, Kind, PluginEntry};

/// The three codes an Audio Unit is found by: its type, which says what
/// kind of unit it is (`aumu` for an instrument, `aufx` for an effect, and
/// so on), its subtype and its manufacturer. Each is a four-character code,
/// as `OSType` holds it, the first character in the high byte.
///
/// As text, which is what [`PluginEntry::id`] holds for a unit, the three
/// are written `type:subtype:manufacturer`, such as `aufx:dcmp:appl`. A
/// code that is not four printable ASCII characters, or that holds a colon,
/// is written as eight hexadecimal digits after `0x`.
///
/// ```
/// use manguebyte_host::au::ComponentCodes;
///
/// let codes = ComponentCodes::new(*b"aufx", *b"dcmp", *b"appl");
/// assert_eq!(codes.to_string(), "aufx:dcmp:appl");
/// assert_eq!("aufx:dcmp:appl".parse::<ComponentCodes>(), Ok(codes));
/// let odd = ComponentCodes { manufacturer: 0x0102_0304, ..codes };
/// assert_eq!(odd.to_string(), "aufx:dcmp:0x01020304");
/// assert_eq!(odd.to_string().parse::<ComponentCodes>(), Ok(odd));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ComponentCodes {
    /// The component type.
    pub kind: u32,
    /// The subtype.
    pub subtype: u32,
    /// The manufacturer.
    pub manufacturer: u32,
}

impl ComponentCodes {
    /// The codes from their characters.
    pub const fn new(kind: [u8; 4], subtype: [u8; 4], manufacturer: [u8; 4]) -> Self {
        Self {
            kind: u32::from_be_bytes(kind),
            subtype: u32::from_be_bytes(subtype),
            manufacturer: u32::from_be_bytes(manufacturer),
        }
    }

    /// What kind of plugin the type says this is.
    pub fn plugin_kind(&self) -> Kind {
        match self.kind {
            kAudioUnitType_MusicDevice => Kind::Instrument,
            kAudioUnitType_Effect | kAudioUnitType_MusicEffect => Kind::Effect,
            kAudioUnitType_MIDIProcessor => Kind::NoteEffect,
            _ => Kind::Other,
        }
    }

    /// Whether the type is one that takes MIDI: a music device (`aumu`), a
    /// music effect (`aumf`) or a MIDI processor (`aumi`).
    pub fn takes_midi(&self) -> bool {
        matches!(
            self.kind,
            kAudioUnitType_MusicDevice | kAudioUnitType_MusicEffect | kAudioUnitType_MIDIProcessor
        )
    }

    /// Whether a unit of this type can be run as a processor: every type
    /// but an output unit (`auou`), which drives a device, and a format
    /// converter (`aufc`), which changes the rate or the format of what it
    /// is given.
    pub fn is_processor(&self) -> bool {
        !matches!(
            self.kind,
            kAudioUnitType_Output | kAudioUnitType_FormatConverter
        )
    }

    pub(crate) fn description(&self) -> AudioComponentDescription {
        AudioComponentDescription {
            componentType: self.kind,
            componentSubType: self.subtype,
            componentManufacturer: self.manufacturer,
            componentFlags: 0,
            componentFlagsMask: 0,
        }
    }
}

/// A code as text: its characters, or `0x` and its digits.
fn code_text(code: u32) -> String {
    let bytes = code.to_be_bytes();
    if bytes
        .iter()
        .all(|&byte| (0x20..0x7F).contains(&byte) && byte != b':')
    {
        bytes.iter().map(|&byte| char::from(byte)).collect()
    } else {
        format!("0x{code:08X}")
    }
}

fn code_from_text(text: &str) -> Option<u32> {
    if let Some(hex) = text.strip_prefix("0x") {
        if hex.len() == 8 {
            return u32::from_str_radix(hex, 16).ok();
        }
    }
    let bytes: [u8; 4] = text.as_bytes().try_into().ok()?;
    bytes
        .iter()
        .all(|&byte| (0x20..0x7F).contains(&byte))
        .then(|| u32::from_be_bytes(bytes))
}

impl fmt::Display for ComponentCodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}",
            code_text(self.kind),
            code_text(self.subtype),
            code_text(self.manufacturer)
        )
    }
}

/// The text is not three codes as [`ComponentCodes`] writes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NotComponentCodes;

impl fmt::Display for NotComponentCodes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("not three Audio Unit codes, such as aufx:dcmp:appl")
    }
}

impl std::error::Error for NotComponentCodes {}

impl FromStr for ComponentCodes {
    type Err = NotComponentCodes;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut parts = text.split(':');
        let mut next = || {
            parts
                .next()
                .and_then(code_from_text)
                .ok_or(NotComponentCodes)
        };
        let codes = Self {
            kind: next()?,
            subtype: next()?,
            manufacturer: next()?,
        };
        match parts.next() {
            None => Ok(codes),
            Some(_) => Err(NotComponentCodes),
        }
    }
}

/// A version number as `AudioComponent.h` lays it out, `0xMMMMmmbb`, as
/// text: `1.2.3` for `0x00010203`.
pub fn version_text(version: u32) -> String {
    format!(
        "{}.{}.{}",
        version >> 16,
        (version >> 8) & 0xFF,
        version & 0xFF
    )
}

/// What a component says about itself, without any of its code running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuComponentInfo {
    /// The codes it is found by.
    pub codes: ComponentCodes,
    /// Its name, without its maker's.
    pub name: String,
    /// Its maker. Audio Units name themselves `Maker: Name`; this is the
    /// part before the colon, or empty if there is none.
    pub vendor: String,
    /// Its version, laid out as `0xMMMMmmbb` (see [`version_text`]).
    pub version: u32,
    /// What it says it is, as the `Info.plist` describes it, or empty.
    pub description: String,
    /// The words it is filed under, such as `Synthesizer` or `Effects`, as
    /// the `Info.plist` gives them.
    pub tags: Vec<String>,
}

impl AuComponentInfo {
    /// The info of a unit whose full name is `full_name`, as Audio Units
    /// give it: `Maker: Name`.
    pub fn named(codes: ComponentCodes, full_name: &str, version: u32) -> Self {
        let (vendor, name) = split_name(full_name);
        Self {
            codes,
            name,
            vendor,
            version,
            description: String::new(),
            tags: Vec::new(),
        }
    }

    /// The entry the one interface lists for this component.
    pub fn entry(&self) -> PluginEntry {
        let mut categories = self.tags.clone();
        if categories.is_empty() {
            categories.push(code_text(self.codes.kind));
        }
        PluginEntry {
            format: Format::Au,
            id: self.codes.to_string(),
            name: self.name.clone(),
            vendor: self.vendor.clone(),
            version: version_text(self.version),
            kind: self.codes.plugin_kind(),
            categories,
        }
    }
}

/// `Maker: Name` taken apart.
fn split_name(full: &str) -> (String, String) {
    match full.split_once(':') {
        Some((vendor, name)) if !vendor.trim().is_empty() && !name.trim().is_empty() => {
            (vendor.trim().to_owned(), name.trim().to_owned())
        }
        _ => (String::new(), full.trim().to_owned()),
    }
}

/// Reads the units a component bundle's `Info.plist` lists in its
/// `AudioComponents` entry, as Apple's *Information Property List Key
/// Reference* and `AudioComponent.h` describe it: each with its `type`,
/// `subtype` and `manufacturer`, its `name`, its `version` and, if given,
/// its `description` and `tags`. Nothing is loaded or run.
///
/// The text must be the XML form of a property list. On macOS,
/// [`AuLibrary::load`](super::AuLibrary::load) reads a bundle in binary
/// form as well, through CoreFoundation.
///
/// ```
/// use manguebyte_host::au::{ComponentCodes, read_info_plist};
///
/// let plist = br#"<plist version="1.0"><dict>
///     <key>AudioComponents</key><array><dict>
///         <key>type</key><string>aufx</string>
///         <key>subtype</key><string>gain</string>
///         <key>manufacturer</key><string>Exmp</string>
///         <key>name</key><string>Example: Gain</string>
///         <key>version</key><integer>66051</integer>
///     </dict></array>
/// </dict></plist>"#;
/// let [unit] = read_info_plist(plist).unwrap().try_into().unwrap();
/// assert_eq!(unit.codes, ComponentCodes::new(*b"aufx", *b"gain", *b"Exmp"));
/// assert_eq!((unit.vendor.as_str(), unit.name.as_str()), ("Example", "Gain"));
/// assert_eq!(unit.entry().version, "1.2.3");
/// ```
///
/// # Errors
///
/// [`AuError::BadInfoPlist`] if the text is not a property list, has no
/// `AudioComponents` entry, or an entry lacks its codes or its name.
pub fn read_info_plist(bytes: &[u8]) -> Result<Vec<AuComponentInfo>, AuError> {
    if bytes.len() > MAX_INFO_PLIST_BYTES {
        return Err(AuError::BadInfoPlist("the Info.plist is too large".into()));
    }
    let root = plist::read(bytes).map_err(AuError::BadInfoPlist)?;
    components_of(&root)
}

pub(crate) fn components_of(root: &Value) -> Result<Vec<AuComponentInfo>, AuError> {
    let bad = |what: &str| AuError::BadInfoPlist(what.to_owned());
    let list = root
        .get("AudioComponents")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("there is no AudioComponents entry"))?;
    let mut found = Vec::new();
    for entry in list {
        let code = |key: &str| {
            // A code is four characters; some bundles give the number.
            match entry.get(key) {
                Some(Value::String(text)) => code_from_text(text),
                Some(Value::Integer(number)) => u32::try_from(*number).ok(),
                _ => None,
            }
        };
        let (Some(kind), Some(subtype), Some(manufacturer)) =
            (code("type"), code("subtype"), code("manufacturer"))
        else {
            return Err(bad("an AudioComponents entry lacks a code"));
        };
        let full_name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| bad("an AudioComponents entry has no name"))?;
        let version = match entry.get("version") {
            Some(Value::Integer(number)) => u32::try_from(*number).unwrap_or(0),
            Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
            _ => 0,
        };
        let codes = ComponentCodes {
            kind,
            subtype,
            manufacturer,
        };
        let mut info = AuComponentInfo::named(codes, full_name, version);
        info.description = entry
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        info.tags = entry
            .get("tags")
            .and_then(Value::as_array)
            .unwrap_or_default()
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        found.push(info);
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_as_text_and_back() {
        let codes = ComponentCodes::new(*b"aumu", *b"Plk ", *b"Mngb");
        assert_eq!(codes.to_string(), "aumu:Plk :Mngb");
        assert_eq!(codes.to_string().parse(), Ok(codes));
        let colon = ComponentCodes::new(*b"aumu", *b"a:bc", *b"\x00\x01\x02\x03");
        assert_eq!(colon.to_string(), "aumu:0x613A6263:0x00010203");
        assert_eq!(colon.to_string().parse(), Ok(colon));
        for bad in [
            "",
            "aumu",
            "aumu:abcd",
            "aumu:abcd:ef",
            "a:b:c:d",
            "aumu:abcd:0x123",
        ] {
            assert_eq!(
                bad.parse::<ComponentCodes>(),
                Err(NotComponentCodes),
                "{bad}"
            );
        }
        assert_eq!(codes.plugin_kind(), Kind::Instrument);
        assert!(codes.takes_midi() && codes.is_processor());
        let output = ComponentCodes::new(*b"auou", *b"def ", *b"appl");
        assert!(!output.is_processor() && !output.takes_midi());
        assert_eq!(version_text(0x0001_0203), "1.2.3");
        assert_eq!(version_text(256), "0.1.0");
    }

    #[test]
    fn names_are_split_at_the_colon() {
        assert_eq!(
            split_name("Apple: AUDelay"),
            ("Apple".into(), "AUDelay".into())
        );
        assert_eq!(
            split_name("Just a name"),
            (String::new(), "Just a name".into())
        );
        assert_eq!(split_name(": odd"), (String::new(), ": odd".into()));
    }

    #[test]
    fn a_plist_without_components_is_refused() {
        for bad in [
            &b"<plist><dict/></plist>"[..],
            b"<plist><dict><key>AudioComponents</key><array><dict>\
              <key>type</key><string>aufx</string></dict></array></dict></plist>",
            b"not a plist",
        ] {
            assert!(matches!(
                read_info_plist(bad),
                Err(AuError::BadInfoPlist(_))
            ));
        }
    }
}
