//! The lines the scanner process writes and the cache file holds.
//!
//! Each record is one line of fields separated by tabs, with a tag first.
//! A field escapes the backslash, the tab, the line feed and the carriage
//! return, so any text fits in one. Reading takes nothing on faith: a line
//! that does not parse is skipped or refused, never a panic.

use crate::any::{Format, Kind, PluginEntry};

/// Most fields read from one line.
const MAX_FIELDS: usize = 256;

/// Writes `text` as one field.
pub(crate) fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Reads one field. `None` for an escape that is not one of the four.
pub(crate) fn unescape(field: &str) -> Option<String> {
    let mut out = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            't' => '\t',
            'n' => '\n',
            'r' => '\r',
            _ => return None,
        });
    }
    Some(out)
}

/// The fields of a line, unescaped. `None` if any field is malformed or
/// there are too many.
pub(crate) fn fields(line: &str) -> Option<Vec<String>> {
    let mut found = Vec::new();
    for field in line.split('\t') {
        if found.len() >= MAX_FIELDS {
            return None;
        }
        found.push(unescape(field)?);
    }
    Some(found)
}

/// A line of fields, escaped and joined.
pub(crate) fn line(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|field| escape(field))
        .collect::<Vec<_>>()
        .join("\t")
}

pub(crate) fn format_word(format: Format) -> &'static str {
    match format {
        Format::Clap => "clap",
        Format::Vst3 => "vst3",
        Format::Lv2 => "lv2",
        Format::Au => "au",
    }
}

pub(crate) fn format_from_word(word: &str) -> Option<Format> {
    Some(match word {
        "clap" => Format::Clap,
        "vst3" => Format::Vst3,
        "lv2" => Format::Lv2,
        "au" => Format::Au,
        _ => return None,
    })
}

fn kind_word(kind: Kind) -> &'static str {
    match kind {
        Kind::Instrument => "instrument",
        Kind::Effect => "effect",
        Kind::NoteEffect => "note-effect",
        Kind::Analyzer => "analyzer",
        Kind::Other => "other",
    }
}

fn kind_from_word(word: &str) -> Kind {
    match word {
        "instrument" => Kind::Instrument,
        "effect" => Kind::Effect,
        "note-effect" => Kind::NoteEffect,
        "analyzer" => Kind::Analyzer,
        _ => Kind::Other,
    }
}

/// The fields of a plugin after its tag: id, name, vendor, version, kind,
/// then its categories.
pub(crate) fn plugin_fields(entry: &PluginEntry) -> Vec<&str> {
    let mut fields = vec![
        entry.id.as_str(),
        entry.name.as_str(),
        entry.vendor.as_str(),
        entry.version.as_str(),
        kind_word(entry.kind),
    ];
    fields.extend(entry.categories.iter().map(String::as_str));
    fields
}

/// Reads the fields written by [`plugin_fields`].
pub(crate) fn plugin_from_fields(format: Format, fields: &[String]) -> Option<PluginEntry> {
    let [id, name, vendor, version, kind, categories @ ..] = fields else {
        return None;
    };
    if id.is_empty() {
        return None;
    }
    Some(PluginEntry {
        format,
        id: id.clone(),
        name: name.clone(),
        vendor: vendor.clone(),
        version: version.clone(),
        kind: kind_from_word(kind),
        categories: categories.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_text_survives_a_field() {
        for text in [
            "",
            "plain",
            "a\tb",
            "line\nbreak\r",
            "back\\slash\\t",
            "é\u{1F600}",
        ] {
            let escaped = escape(text);
            assert!(!escaped.contains(['\t', '\n', '\r']));
            assert_eq!(unescape(&escaped).as_deref(), Some(text));
        }
        assert_eq!(unescape("bad\\q"), None);
        assert_eq!(unescape("cut\\"), None);
        let written = line(&["a\tb", "", "c"]);
        assert_eq!(fields(&written).unwrap(), ["a\tb", "", "c"]);
        assert_eq!(fields(&"\t".repeat(MAX_FIELDS)), None);
    }

    #[test]
    fn a_plugin_goes_through_its_fields_and_back() {
        let entry = PluginEntry {
            format: Format::Vst3,
            id: "ABC".into(),
            name: "Name\twith tab".into(),
            vendor: String::new(),
            version: "1.0".into(),
            kind: Kind::NoteEffect,
            categories: vec!["Fx".into(), "Delay".into()],
        };
        let fields: Vec<String> = plugin_fields(&entry)
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(plugin_from_fields(Format::Vst3, &fields), Some(entry));
        assert_eq!(plugin_from_fields(Format::Clap, &fields[..4]), None);
        let mut empty = fields.clone();
        empty[0].clear();
        assert_eq!(plugin_from_fields(Format::Clap, &empty), None);
        for format in [Format::Clap, Format::Vst3, Format::Lv2, Format::Au] {
            assert_eq!(format_from_word(format_word(format)), Some(format));
        }
        assert_eq!(format_from_word("aax"), None);
    }
}
