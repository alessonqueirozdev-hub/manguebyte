//! Reading SFZ text.
//!
//! An SFZ file is plain text made of headers in angle brackets and
//! `opcode=value` pairs after them:
//!
//! ```text
//! <control> default_path=Piano/
//! <global>  ampeg_release=0.4
//! <group>   lovel=1 hivel=80
//! <region>  sample=C4p.wav key=60
//! <region>  sample=D4p.wav lokey=61 hikey=63 pitch_keycenter=62
//! ```
//!
//! [`Sfz::parse`] turns such text into a list of [`Region`]s with every
//! opcode they inherit from `<global>`, `<master>` and `<group>` applied.
//! It also handles `//` and `/* */` comments, `#define $NAME value`, and
//! `#include "file.sfz"` when [`Sfz::parse_with`] is given a
//! [`SampleSource`] to read included files from.
//!
//! Text that cannot be understood is an [`SfzError`] that names the line;
//! an opcode the sampler does not know, or a value outside its range, is a
//! [`Warning`] and parsing goes on. Neither ever panics, whatever the input.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use crate::region::Region;
use crate::source::{SampleSource, normalize_path};

/// Where something was written: a file and a line, counted from 1.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Location {
    /// The included file, or `None` for the text given to the parser.
    pub file: Option<Arc<str>>,
    /// Line number, counted from 1.
    pub line: usize,
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.file {
            Some(file) => write!(f, "{file}:{}", self.line),
            None => write!(f, "line {}", self.line),
        }
    }
}

/// One `opcode=value` pair, as written.
///
/// Name and value are shared, not copied, by every region that inherits the
/// opcode from a `<global>`, `<master>` or `<group>`, so a long value
/// written once is held once however many regions it applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opcode {
    /// The opcode's name, such as `lokey`.
    pub name: Arc<str>,
    /// Its value, with `#define` variables replaced and, for `sample`, the
    /// `default_path` in force put in front.
    pub value: Arc<str>,
    /// Where it was written.
    pub location: Location,
}

/// Text that could not be read as SFZ.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SfzError {
    /// Where the problem is.
    pub location: Location,
    /// What the problem is.
    pub kind: SfzErrorKind,
}

impl SfzError {
    fn new(location: Location, kind: SfzErrorKind) -> Self {
        Self { location, kind }
    }
}

/// What made SFZ text unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SfzErrorKind {
    /// A `/*` comment is never closed.
    UnterminatedComment,
    /// A `<` with no `>` after it on the same line.
    UnterminatedHeader,
    /// A header this format does not have, such as `<regoin>`.
    UnknownHeader(String),
    /// Something that is neither a header nor `opcode=value`.
    ExpectedOpcode(String),
    /// A value that does not fit its opcode, such as `lokey=high`.
    InvalidValue {
        /// The opcode.
        opcode: String,
        /// The value written.
        value: String,
    },
    /// A `#define` without a `$NAME` and a value, or another line starting
    /// with `#` that is not a directive.
    BadDirective(String),
    /// A `#define` whose value is longer than [`MAX_DEFINE_LEN`] bytes.
    DefineTooLong,
    /// A line longer than [`MAX_LINE_LEN`] bytes once variables are replaced.
    LineTooLong,
    /// An `#include` in text parsed with [`Sfz::parse`], which has no
    /// [`SampleSource`] to read it from.
    IncludeWithoutSource(String),
    /// The source could not read an included file.
    IncludeFailed {
        /// The path as written.
        path: String,
        /// What the source said.
        message: String,
    },
    /// A file includes itself, directly or through others.
    IncludeCycle(String),
    /// More than [`MAX_INCLUDE_DEPTH`] files deep, or more than
    /// [`MAX_INCLUDES`] includes in all.
    TooManyIncludes,
    /// The regions would take more than [`MAX_REGION_OPCODES`] opcodes in
    /// all, counting for each region those it inherits.
    TooManyOpcodes,
}

impl fmt::Display for SfzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.location)?;
        match &self.kind {
            SfzErrorKind::UnterminatedComment => f.write_str("a /* comment is never closed"),
            SfzErrorKind::UnterminatedHeader => f.write_str("a header is missing its '>'"),
            SfzErrorKind::UnknownHeader(name) => write!(f, "unknown header <{name}>"),
            SfzErrorKind::ExpectedOpcode(text) => {
                write!(f, "expected a header or opcode=value, found {text:?}")
            }
            SfzErrorKind::InvalidValue { opcode, value } => {
                write!(f, "{value:?} is not a valid value for {opcode}")
            }
            SfzErrorKind::BadDirective(text) => write!(f, "malformed directive {text:?}"),
            SfzErrorKind::DefineTooLong => {
                write!(f, "a #define value is longer than {MAX_DEFINE_LEN} bytes")
            }
            SfzErrorKind::LineTooLong => write!(f, "a line is longer than {MAX_LINE_LEN} bytes"),
            SfzErrorKind::IncludeWithoutSource(path) => {
                write!(f, "#include \"{path}\" needs a source to read it from")
            }
            SfzErrorKind::IncludeFailed { path, message } => {
                write!(f, "could not include \"{path}\": {message}")
            }
            SfzErrorKind::IncludeCycle(path) => write!(f, "\"{path}\" includes itself"),
            SfzErrorKind::TooManyIncludes => f.write_str("too many nested or repeated #includes"),
            SfzErrorKind::TooManyOpcodes => write!(
                f,
                "the regions take more than {MAX_REGION_OPCODES} opcodes in all"
            ),
        }
    }
}

impl std::error::Error for SfzError {}

/// Something in the text that the sampler skips or adjusts.
#[derive(Debug, Clone, PartialEq)]
pub struct Warning {
    /// Where it was written.
    pub location: Location,
    /// What it is.
    pub kind: WarningKind,
}

/// What a [`Warning`] is about.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WarningKind {
    /// An opcode this sampler does not know or does not play. It is kept in
    /// [`Region::unknown`] (or ignored, outside a region's headers).
    UnknownOpcode {
        /// The opcode.
        name: Arc<str>,
        /// Its value.
        value: Arc<str>,
    },
    /// A header of the format that this sampler does not use, such as
    /// `<curve>` or `<effect>`. The opcodes under it are ignored.
    UnsupportedHeader(String),
    /// An opcode before the first header. It is ignored.
    OpcodeOutsideHeader(String),
    /// A value outside its opcode's range, replaced by the nearest one in it.
    OutOfRange {
        /// The opcode.
        opcode: Arc<str>,
        /// The value written.
        value: Arc<str>,
        /// The value used instead.
        used: f64,
    },
    /// A region with no `sample` opcode. It is left out.
    RegionWithoutSample,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: ", self.location)?;
        match &self.kind {
            WarningKind::UnknownOpcode { name, value } => {
                write!(f, "opcode {name}={value} is not supported and is ignored")
            }
            WarningKind::UnsupportedHeader(name) => {
                write!(
                    f,
                    "header <{name}> is not supported; its opcodes are ignored"
                )
            }
            WarningKind::OpcodeOutsideHeader(name) => {
                write!(f, "opcode {name} comes before any header and is ignored")
            }
            WarningKind::OutOfRange {
                opcode,
                value,
                used,
            } => write!(f, "{opcode}={value} is out of range; {used} is used"),
            WarningKind::RegionWithoutSample => {
                f.write_str("a region has no sample and is skipped")
            }
        }
    }
}

/// Settings from the `<control>` header that apply to the whole instrument.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Control {
    /// Semitones added to every incoming note before regions are chosen
    /// (`note_offset`).
    pub note_offset: i32,
    /// Octaves added to every incoming note (`octave_offset`).
    pub octave_offset: i32,
    /// The value each controller starts at, 0 to 1, from `set_ccN` (0 to
    /// 127, divided by 127) and `set_hdccN` (0 to 1), one entry per
    /// controller, in the order first written; a later one replaces the
    /// earlier. Controllers not listed start at 0.
    pub set_cc: Vec<(u16, f64)>,
    /// A name for each controller that has one (`label_ccN`), for a
    /// player to show; the sampler does nothing with them.
    pub label_cc: Vec<(u16, String)>,
}

impl Control {
    /// The total shift applied to incoming notes, in semitones.
    pub fn note_shift(&self) -> i32 {
        self.note_offset + 12 * self.octave_offset
    }

    /// The value controller `controller` starts at, 0 to 1: its `set_ccN`
    /// or `set_hdccN`, or 0.
    pub fn initial_value(&self, controller: u16) -> f64 {
        self.set_cc
            .iter()
            .find(|(number, _)| *number == controller)
            .map_or(0.0, |&(_, value)| value)
    }

    fn set_initial(&mut self, controller: u16, value: f64) {
        match self
            .set_cc
            .iter_mut()
            .find(|(number, _)| *number == controller)
        {
            Some(entry) => entry.1 = value,
            None => self.set_cc.push((controller, value)),
        }
    }
}

/// The controller number `name` ends with after `prefix`, if it is one and
/// below [`MAX_CONTROLLERS`](crate::MAX_CONTROLLERS).
fn controller_suffix(name: &str, prefix: &str) -> Option<u16> {
    let digits = name.strip_prefix(prefix)?;
    if digits.is_empty() || digits.len() > 6 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: usize = digits.parse().ok()?;
    (number < crate::MAX_CONTROLLERS).then_some(number as u16)
}

/// Longest value a `#define` may have, in bytes. With it, replacing
/// variables can make a line at most this many times longer.
pub const MAX_DEFINE_LEN: usize = 4_096;

/// Longest line, in bytes, once variables are replaced.
pub const MAX_LINE_LEN: usize = 1 << 20;

/// How deep `#include`s may nest.
pub const MAX_INCLUDE_DEPTH: usize = 16;

/// How many `#include`s one parse may follow in all.
pub const MAX_INCLUDES: usize = 1_024;

/// Most opcodes the regions of one parse may take in all, each region
/// counting those it inherits as well as its own. It bounds the work of a
/// parse and the memory its regions hold, which would otherwise grow with
/// the product of the opcodes in a `<global>` and the regions under it.
/// A library of 10 000 regions taking 50 opcodes each uses half of it.
pub const MAX_REGION_OPCODES: usize = 1 << 20;

/// A parsed SFZ instrument: its regions with everything they inherit
/// applied, and what the parser had to say about the text.
///
/// ```
/// use manguebyte_sampler::{Sfz, Trigger};
///
/// let sfz = Sfz::parse(
///     "#define $ROOT 60
///      <group> lovel=1 hivel=64 ampeg_release=0.5
///      <region> sample=soft.wav key=$ROOT
///      <region> sample=noise.wav key=$ROOT trigger=release
///      <region> sample=mystery.wav sw_last=24",
/// )?;
/// assert_eq!(sfz.regions.len(), 3);
/// let soft = &sfz.regions[0];
/// assert_eq!((soft.lokey, soft.hikey, soft.hivel), (60, 60, 64));
/// assert_eq!(soft.ampeg.release, 0.5);
/// assert_eq!(sfz.regions[1].trigger, Trigger::Release);
/// // An opcode the sampler does not play is kept and reported.
/// assert_eq!(&*sfz.regions[2].unknown[0].name, "sw_last");
/// assert_eq!(sfz.warnings.len(), 1);
/// assert_eq!(sfz.warnings[0].location.line, 5);
///
/// // Malformed text is an error that names the line.
/// let error = Sfz::parse("<region> sample=a.wav\n<region lokey=1").unwrap_err();
/// assert_eq!(error.location.line, 2);
/// # Ok::<(), manguebyte_sampler::SfzError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Sfz {
    /// Settings of the `<control>` header.
    pub control: Control,
    /// Every region with a sample, in the order written.
    pub regions: Vec<Region>,
    /// Opcodes and headers that were skipped or adjusted.
    pub warnings: Vec<Warning>,
}

impl Sfz {
    /// Parses SFZ text. An `#include` in it is an error, since there is
    /// nothing to read the file from; use [`parse_with`](Self::parse_with)
    /// for that.
    pub fn parse(text: &str) -> Result<Self, SfzError> {
        Parser::new(None).run(text)
    }

    /// Parses SFZ text, reading `#include`d files from `source`. Included
    /// paths are handed to the source as written, with `\` turned into `/`.
    pub fn parse_with(text: &str, source: &mut dyn SampleSource) -> Result<Self, SfzError> {
        Parser::new(Some(source)).run(text)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Before the first header.
    None,
    Control,
    Global,
    Master,
    Group,
    Region,
    /// Under a header whose opcodes are ignored.
    Ignored,
}

struct Parser<'a> {
    source: Option<&'a mut dyn SampleSource>,
    defines: Vec<(String, String)>,
    default_path: String,
    control: Control,
    scope: Scope,
    global: Vec<Opcode>,
    master: Vec<Opcode>,
    group: Vec<Opcode>,
    region: Vec<Opcode>,
    region_location: Option<Location>,
    regions: Vec<Region>,
    warnings: Vec<Warning>,
    reported: HashSet<(Location, Arc<str>)>,
    include_stack: Vec<String>,
    includes: usize,
    /// Headers met so far, which numbers them.
    headers: u32,
    /// The number of the `<global>`, `<master>`, `<group>` and `<region>`
    /// in force.
    level_ids: [u32; 4],
    /// Opcodes the regions have taken so far, inherited ones included.
    taken: usize,
}

impl<'a> Parser<'a> {
    fn new(source: Option<&'a mut dyn SampleSource>) -> Self {
        Self {
            source,
            defines: Vec::new(),
            default_path: String::new(),
            control: Control::default(),
            scope: Scope::None,
            global: Vec::new(),
            master: Vec::new(),
            group: Vec::new(),
            region: Vec::new(),
            region_location: None,
            regions: Vec::new(),
            warnings: Vec::new(),
            reported: HashSet::new(),
            include_stack: Vec::new(),
            includes: 0,
            headers: 0,
            level_ids: [0; 4],
            taken: 0,
        }
    }

    fn run(mut self, text: &str) -> Result<Sfz, SfzError> {
        self.text(text, None)?;
        self.finish_region()?;
        Ok(Sfz {
            control: self.control,
            regions: self.regions,
            warnings: self.warnings,
        })
    }

    fn warn(&mut self, location: Location, kind: WarningKind) {
        self.warnings.push(Warning { location, kind });
    }

    /// `parsed` held to `0..=max`, with a warning if it was outside.
    fn clamped(
        &mut self,
        name: &str,
        value: &str,
        location: &Location,
        parsed: f64,
        max: f64,
    ) -> f64 {
        let used = parsed.clamp(0.0, max);
        if used != parsed {
            self.warn(
                location.clone(),
                WarningKind::OutOfRange {
                    opcode: Arc::from(name),
                    value: Arc::from(value),
                    used,
                },
            );
        }
        used
    }

    /// Parses one file's worth of text.
    fn text(&mut self, text: &str, file: Option<Arc<str>>) -> Result<(), SfzError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let stripped = strip_comments(text).map_err(|line| {
            SfzError::new(
                Location {
                    file: file.clone(),
                    line,
                },
                SfzErrorKind::UnterminatedComment,
            )
        })?;
        for (index, line) in stripped.split('\n').enumerate() {
            let location = Location {
                file: file.clone(),
                line: index + 1,
            };
            self.line(line.strip_suffix('\r').unwrap_or(line), &location)?;
        }
        Ok(())
    }

    fn line(&mut self, line: &str, location: &Location) -> Result<(), SfzError> {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix('#') {
            return self.directive(rest, location);
        }
        let line = self.substitute(line, location)?;
        self.tokens(&line, location)
    }

    fn directive(&mut self, rest: &str, location: &Location) -> Result<(), SfzError> {
        let bad = || {
            SfzError::new(
                location.clone(),
                SfzErrorKind::BadDirective(format!("#{}", rest.trim_end())),
            )
        };
        if let Some(body) = rest.strip_prefix("define") {
            if !body.starts_with(|c: char| c.is_ascii_whitespace()) {
                return Err(bad());
            }
            let body = body.trim_start();
            let name_len = body
                .strip_prefix('$')
                .map(|after| {
                    after
                        .bytes()
                        .take_while(|&b| b.is_ascii_alphanumeric() || b == b'_')
                        .count()
                })
                .unwrap_or(0);
            if name_len == 0 {
                return Err(bad());
            }
            let name = &body[1..=name_len];
            let after = &body[1 + name_len..];
            if !after.starts_with(|c: char| c.is_ascii_whitespace()) {
                return Err(bad());
            }
            let value = self.substitute(after.trim(), location)?;
            if value.is_empty() {
                return Err(bad());
            }
            if value.len() > MAX_DEFINE_LEN {
                return Err(SfzError::new(location.clone(), SfzErrorKind::DefineTooLong));
            }
            match self.defines.iter_mut().find(|(known, _)| known == name) {
                Some(entry) => entry.1 = value,
                None => self.defines.push((name.to_owned(), value)),
            }
            Ok(())
        } else if let Some(body) = rest.strip_prefix("include") {
            let body = self.substitute(body.trim(), location)?;
            let path = body
                .strip_prefix('"')
                .and_then(|inner| inner.strip_suffix('"'))
                .filter(|path| !path.is_empty() && !path.contains('"'))
                .ok_or_else(bad)?;
            self.include(&normalize_path(path), location)
        } else {
            Err(bad())
        }
    }

    fn include(&mut self, path: &str, location: &Location) -> Result<(), SfzError> {
        if self.include_stack.iter().any(|open| open == path) {
            return Err(SfzError::new(
                location.clone(),
                SfzErrorKind::IncludeCycle(path.to_owned()),
            ));
        }
        if self.include_stack.len() >= MAX_INCLUDE_DEPTH || self.includes >= MAX_INCLUDES {
            return Err(SfzError::new(
                location.clone(),
                SfzErrorKind::TooManyIncludes,
            ));
        }
        let Some(source) = self.source.as_deref_mut() else {
            return Err(SfzError::new(
                location.clone(),
                SfzErrorKind::IncludeWithoutSource(path.to_owned()),
            ));
        };
        let bytes = source.read(path).map_err(|error| {
            SfzError::new(
                location.clone(),
                SfzErrorKind::IncludeFailed {
                    path: path.to_owned(),
                    message: error.to_string(),
                },
            )
        })?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        self.includes += 1;
        self.include_stack.push(path.to_owned());
        let result = self.text(&text, Some(Arc::from(path)));
        self.include_stack.pop();
        result
    }

    /// Replaces `$NAME` with the value of the longest defined variable the
    /// text after `$` starts with. Anything else is left as written.
    fn substitute(&self, line: &str, location: &Location) -> Result<String, SfzError> {
        if self.defines.is_empty() || !line.contains('$') {
            return Ok(line.to_owned());
        }
        let mut out = String::with_capacity(line.len());
        let mut rest = line;
        while let Some(at) = rest.find('$') {
            out.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            let word = after
                .bytes()
                .take_while(|&b| b.is_ascii_alphanumeric() || b == b'_')
                .count();
            let found = self
                .defines
                .iter()
                .filter(|(name, _)| name.len() <= word && after[..word].starts_with(name.as_str()))
                .max_by_key(|(name, _)| name.len());
            match found {
                Some((name, value)) => {
                    out.push_str(value);
                    rest = &after[name.len()..];
                }
                None => {
                    out.push('$');
                    rest = after;
                }
            }
            if out.len() > MAX_LINE_LEN {
                return Err(SfzError::new(location.clone(), SfzErrorKind::LineTooLong));
            }
        }
        out.push_str(rest);
        if out.len() > MAX_LINE_LEN {
            return Err(SfzError::new(location.clone(), SfzErrorKind::LineTooLong));
        }
        Ok(out)
    }

    fn tokens(&mut self, line: &str, location: &Location) -> Result<(), SfzError> {
        let bytes = line.as_bytes();
        let mut position = 0;
        while position < bytes.len() {
            if bytes[position].is_ascii_whitespace() {
                position += 1;
                continue;
            }
            if bytes[position] == b'<' {
                let Some(length) = line[position..].find('>') else {
                    return Err(SfzError::new(
                        location.clone(),
                        SfzErrorKind::UnterminatedHeader,
                    ));
                };
                let name = line[position + 1..position + length].trim();
                self.header(name, location)?;
                position += length + 1;
                continue;
            }
            let name_len = bytes[position..]
                .iter()
                .take_while(|&&b| is_name_byte(b))
                .count();
            let equals = position + name_len;
            if name_len == 0 || bytes.get(equals) != Some(&b'=') {
                let shown: String = line[position..].chars().take(32).collect();
                return Err(SfzError::new(
                    location.clone(),
                    SfzErrorKind::ExpectedOpcode(shown),
                ));
            }
            let end = value_end(bytes, equals + 1);
            let name = &line[position..equals];
            let value = line[equals + 1..end].trim();
            self.opcode(name, value, location)?;
            position = end;
        }
        Ok(())
    }

    fn header(&mut self, name: &str, location: &Location) -> Result<(), SfzError> {
        let scope = match name {
            "control" => Scope::Control,
            "global" => Scope::Global,
            "master" => Scope::Master,
            "group" => Scope::Group,
            "region" => Scope::Region,
            "curve" | "effect" | "midi" | "sample" => {
                self.warn(
                    location.clone(),
                    WarningKind::UnsupportedHeader(name.to_owned()),
                );
                Scope::Ignored
            }
            _ => {
                return Err(SfzError::new(
                    location.clone(),
                    SfzErrorKind::UnknownHeader(name.chars().take(32).collect()),
                ));
            }
        };
        self.finish_region()?;
        self.headers = self.headers.wrapping_add(1);
        let id = self.headers;
        match scope {
            Scope::Global => {
                self.global.clear();
                self.master.clear();
                self.group.clear();
                self.level_ids = [id, id, id, id];
            }
            Scope::Master => {
                self.master.clear();
                self.group.clear();
                self.level_ids[1..].fill(id);
            }
            Scope::Group => {
                self.group.clear();
                self.level_ids[2..].fill(id);
            }
            Scope::Region => {
                self.region_location = Some(location.clone());
                self.level_ids[3] = id;
            }
            _ => {}
        }
        self.scope = scope;
        Ok(())
    }

    fn opcode(&mut self, name: &str, value: &str, location: &Location) -> Result<(), SfzError> {
        let invalid = || {
            SfzError::new(
                location.clone(),
                SfzErrorKind::InvalidValue {
                    opcode: name.to_owned(),
                    value: value.to_owned(),
                },
            )
        };
        match self.scope {
            Scope::None => {
                self.warn(
                    location.clone(),
                    WarningKind::OpcodeOutsideHeader(name.to_owned()),
                );
                Ok(())
            }
            Scope::Ignored => Ok(()),
            Scope::Control => {
                match name {
                    "default_path" => {
                        let mut path = normalize_path(value);
                        if !path.is_empty() && !path.ends_with('/') {
                            path.push('/');
                        }
                        self.default_path = path;
                    }
                    "note_offset" | "octave_offset" => {
                        let (limit, slot) = if name == "note_offset" {
                            (127, &mut self.control.note_offset)
                        } else {
                            (10, &mut self.control.octave_offset)
                        };
                        let parsed = parse_number(value).ok_or_else(invalid)?.trunc();
                        let used = parsed.clamp(-f64::from(limit), f64::from(limit));
                        *slot = used as i32;
                        if used != parsed {
                            self.warn(
                                location.clone(),
                                WarningKind::OutOfRange {
                                    opcode: Arc::from(name),
                                    value: Arc::from(value),
                                    used,
                                },
                            );
                        }
                    }
                    _ => {
                        if let Some(controller) = controller_suffix(name, "set_cc") {
                            let parsed = parse_number(value).ok_or_else(invalid)?;
                            let used = self.clamped(name, value, location, parsed, 127.0);
                            self.control.set_initial(controller, used / 127.0);
                        } else if let Some(controller) = controller_suffix(name, "set_hdcc") {
                            let parsed = parse_number(value).ok_or_else(invalid)?;
                            let used = self.clamped(name, value, location, parsed, 1.0);
                            self.control.set_initial(controller, used);
                        } else if let Some(controller) = controller_suffix(name, "label_cc") {
                            let label = &mut self.control.label_cc;
                            label.retain(|(number, _)| *number != controller);
                            label.push((controller, value.to_owned()));
                        } else {
                            self.warn(
                                location.clone(),
                                WarningKind::UnknownOpcode {
                                    name: Arc::from(name),
                                    value: Arc::from(value),
                                },
                            );
                        }
                    }
                }
                Ok(())
            }
            Scope::Global | Scope::Master | Scope::Group | Scope::Region => {
                let value: Arc<str> = if name == "sample" {
                    Arc::from(sample_path(&self.default_path, value))
                } else {
                    Arc::from(value)
                };
                let opcode = Opcode {
                    name: Arc::from(name),
                    value,
                    location: location.clone(),
                };
                match self.scope {
                    Scope::Global => self.global.push(opcode),
                    Scope::Master => self.master.push(opcode),
                    Scope::Group => self.group.push(opcode),
                    _ => self.region.push(opcode),
                }
                Ok(())
            }
        }
    }

    /// Builds the region being read, if any, from what it inherits and what
    /// it says itself.
    fn finish_region(&mut self) -> Result<(), SfzError> {
        let Some(location) = self.region_location.take() else {
            return Ok(());
        };
        let region_opcodes = core::mem::take(&mut self.region);
        let levels = [&self.global, &self.master, &self.group, &region_opcodes];
        let count: usize = levels.iter().map(|level| level.len()).sum();
        self.taken = self.taken.saturating_add(count);
        if self.taken > MAX_REGION_OPCODES {
            return Err(SfzError::new(location, SfzErrorKind::TooManyOpcodes));
        }
        // The innermost header that sets `polyphony` is the one whose
        // regions its limit covers.
        let polyphony_header = levels
            .iter()
            .zip(self.level_ids)
            .rev()
            .find(|(level, _)| level.iter().any(|opcode| &*opcode.name == "polyphony"))
            .map_or(0, |(_, id)| id);
        let all = levels.into_iter().flatten();
        let mut warnings = Vec::new();
        let region = Region::from_opcodes(all, location.clone(), &mut warnings)?;
        for warning in warnings {
            // Opcodes inherited by many regions are reported once.
            let key = match &warning.kind {
                WarningKind::UnknownOpcode { name, .. } => Arc::clone(name),
                WarningKind::OutOfRange { opcode, .. } => Arc::clone(opcode),
                _ => Arc::from(""),
            };
            if self.reported.insert((warning.location.clone(), key)) {
                self.warnings.push(warning);
            }
        }
        match region {
            Some(mut region) => {
                region.polyphony_header = polyphony_header;
                self.regions.push(region);
            }
            None => self.warn(location, WarningKind::RegionWithoutSample),
        }
        Ok(())
    }
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Where the value that starts at `start` ends: at the end of the line, at
/// a header, or at whitespace followed by the next `opcode=`. Values may
/// contain spaces, which sample paths need.
fn value_end(bytes: &[u8], start: usize) -> usize {
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'<' => return index,
            byte if byte.is_ascii_whitespace() => {
                let next = bytes[index..]
                    .iter()
                    .position(|b| !b.is_ascii_whitespace())
                    .map_or(bytes.len(), |skip| index + skip);
                if next >= bytes.len() || bytes[next] == b'<' {
                    return index;
                }
                let name = bytes[next..]
                    .iter()
                    .take_while(|&&b| is_name_byte(b))
                    .count();
                if name > 0 && bytes.get(next + name) == Some(&b'=') {
                    return index;
                }
                index = next;
            }
            _ => index += 1,
        }
    }
    bytes.len()
}

/// Replaces comments with spaces, keeping every line break so that line
/// numbers stay right. Gives the line of an unterminated `/*` as an error.
fn strip_comments(text: &str) -> Result<String, usize> {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut line = 1;
    while let Some(c) = chars.next() {
        match c {
            '/' if chars.peek() == Some(&'/') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        line += 1;
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let start = line;
                chars.next();
                let mut previous = '\0';
                let mut closed = false;
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        line += 1;
                    }
                    if previous == '*' && skipped == '/' {
                        closed = true;
                        break;
                    }
                    previous = skipped;
                }
                if !closed {
                    return Err(start);
                }
                out.push(' ');
            }
            '\n' => {
                out.push('\n');
                line += 1;
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

/// The path of a sample: as written when it names a generator (`*sine`) or
/// is absolute, and otherwise after the `default_path` in force.
fn sample_path(default_path: &str, value: &str) -> String {
    let path = value.trim().replace('\\', "/");
    let absolute = path.starts_with('/')
        || path.starts_with('*')
        || (path.len() >= 2
            && path.as_bytes()[1] == b':'
            && path.as_bytes()[0].is_ascii_alphabetic());
    if absolute || default_path.is_empty() {
        normalize_path(&path)
    } else {
        normalize_path(&format!("{default_path}{path}"))
    }
}

/// Parses a decimal number, as SFZ writes them: an optional sign, digits,
/// an optional fraction. Infinite and not-a-number results are refused.
pub(crate) fn parse_number(value: &str) -> Option<f64> {
    let value = value.trim();
    let digits = value.strip_prefix(['+', '-']).unwrap_or(value);
    if digits.is_empty()
        || !digits.bytes().all(|b| {
            b.is_ascii_digit() || b == b'.' || b == b'e' || b == b'E' || b == b'-' || b == b'+'
        })
        || !digits.starts_with(|c: char| c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_end_at_the_next_opcode_and_may_hold_spaces() {
        let sfz =
            Sfz::parse("<region> sample=My Piano/C 4.wav lokey=c4 hikey=d#4 pan=-20").unwrap();
        let region = &sfz.regions[0];
        assert_eq!(&*region.sample, "My Piano/C 4.wav");
        assert_eq!((region.lokey, region.hikey), (60, 63));
        assert_eq!(region.pan, -20.0);
    }

    #[test]
    fn comments_are_removed_and_lines_still_count() {
        let text = "/* one\n two */ <region> sample=a.wav // a comment\n\n<region> lokey=x";
        let error = Sfz::parse(text).unwrap_err();
        assert_eq!(error.location.line, 4);
        assert!(matches!(error.kind, SfzErrorKind::InvalidValue { .. }));
        let error = Sfz::parse("<region>\n/* never closed").unwrap_err();
        assert_eq!(
            (error.location.line, error.kind),
            (2, SfzErrorKind::UnterminatedComment)
        );
    }

    #[test]
    fn inheritance_follows_global_master_group_region() {
        let text = "<global> volume=-6 tune=5
                    <master> transpose=2
                    <group> tune=10
                    <region> sample=a.wav
                    <region> sample=b.wav tune=20
                    <master>
                    <region> sample=c.wav
                    <global>
                    <region> sample=d.wav";
        let sfz = Sfz::parse(text).unwrap();
        let summary: Vec<_> = sfz
            .regions
            .iter()
            .map(|r| (r.volume, r.transpose, r.tune))
            .collect();
        assert_eq!(
            summary,
            [
                (-6.0, 2, 10.0),
                (-6.0, 2, 20.0),
                // A new <master> forgets the group and the old master.
                (-6.0, 0, 5.0),
                // A new <global> forgets everything above the region.
                (0.0, 0, 0.0),
            ]
        );
    }

    #[test]
    fn defines_replace_the_longest_name() {
        let text = "#define $K 10\n#define $KEY 61\n<region> sample=$KEY.wav key=$KEY lovel=$K";
        let sfz = Sfz::parse(text).unwrap();
        assert_eq!(&*sfz.regions[0].sample, "61.wav");
        assert_eq!(sfz.regions[0].lokey, 61);
        assert_eq!(sfz.regions[0].lovel, 10);
    }

    #[test]
    fn default_path_and_backslashes() {
        let text = "<control> default_path=Samples\\Piano\n<region> sample=..\\Drums\\kick.wav\n\
                    <control> default_path=\n<region> sample=*sine";
        let sfz = Sfz::parse(text).unwrap();
        assert_eq!(&*sfz.regions[0].sample, "Samples/Drums/kick.wav");
        assert_eq!(&*sfz.regions[1].sample, "*sine");
    }

    #[test]
    fn malformed_text_is_an_error_on_its_line() {
        let cases: &[(&str, usize)] = &[
            ("<region> sample=a.wav\n<regoin>", 2),
            ("\n\n<region", 3),
            ("<region> =5", 1),
            ("<region>\nlokey 5", 2),
            ("#define NAME 5", 1),
            ("#include \"a.sfz\"", 1),
            ("#pragma", 1),
            ("<region> sample=a.wav trigger=sometimes", 1),
        ];
        for &(text, line) in cases {
            let error = Sfz::parse(text).unwrap_err();
            assert_eq!(error.location.line, line, "{text:?}: {error}");
            assert!(!error.to_string().is_empty());
        }
    }

    #[test]
    fn opcodes_outside_headers_and_unsupported_headers_warn() {
        let text = "lokey=1\n<curve> curve_index=1 v000=0\n<region> sample=a.wav\n<region> key=1";
        let sfz = Sfz::parse(text).unwrap();
        let kinds: Vec<_> = sfz
            .warnings
            .iter()
            .map(|w| (w.location.line, &w.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                (1, &WarningKind::OpcodeOutsideHeader("lokey".into())),
                (2, &WarningKind::UnsupportedHeader("curve".into())),
                (4, &WarningKind::RegionWithoutSample),
            ]
        );
        assert_eq!(sfz.regions.len(), 1);
    }

    #[test]
    fn note_offsets_shift_incoming_notes() {
        let sfz = Sfz::parse("<control> note_offset=-3 octave_offset=1").unwrap();
        assert_eq!(sfz.control.note_shift(), 9);
    }

    #[test]
    fn numbers_are_strict() {
        assert_eq!(parse_number(" -1.5 "), Some(-1.5));
        assert_eq!(parse_number("+3"), Some(3.0));
        assert_eq!(parse_number("1e3"), Some(1000.0));
        for bad in ["", "-", "inf", "NaN", "1e999", "0x10", "e5", "5 6"] {
            assert_eq!(parse_number(bad), None, "{bad}");
        }
    }
}
