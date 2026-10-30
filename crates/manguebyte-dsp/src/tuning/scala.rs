//! Scala scale (`.scl`) and keyboard mapping (`.kbm`) files.
//!
//! Both formats are those of Manuel Op de Coul's Scala program, as its
//! help describes them ("Scala scale file format" and "Keyboard mappings").
//! Only the formats are followed here; no code comes from any program that
//! reads them.

use core::fmt;

/// Text that could not be read as a Scala scale or keyboard mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalaError {
    /// The line the problem is on, counting from 1. A file that ends too
    /// soon gives the number of the line after its last.
    pub line: usize,
    /// What the problem is.
    pub kind: ScalaErrorKind,
}

/// What made a Scala file unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScalaErrorKind {
    /// The file ends before this line: in a scale, `"description"` or
    /// `"note count"`; in a mapping, the name of a header field.
    Missing(&'static str),
    /// The note count of a scale is not a whole number.
    BadCount(String),
    /// A scale has no notes: it needs at least its period.
    NoNotes,
    /// A pitch that is neither cents (a number with a period, such as
    /// `701.955` or `-5.0`) nor a ratio of whole numbers (`3/2`, or `2`).
    BadPitch(String),
    /// A ratio that is not above zero, such as `0/1` or `3/0`, or a
    /// negative one: Scala has no negative ratios.
    NotPositive(String),
    /// The scale ends after `found` of its `expected` pitches.
    TooFewPitches {
        /// The note count.
        expected: usize,
        /// The pitches given.
        found: usize,
    },
    /// A scale has more pitch lines than its note count says, or a mapping
    /// more entries than its size.
    TooMany(String),
    /// The period, a scale's last pitch, is not above its first degree
    /// (1/1): the scale would not rise from one period to the next.
    PeriodNotRising,
    /// A field of a mapping header that is not a valid number for it: a
    /// key outside 0 to 127, a frequency that is not above zero, and so on.
    BadField {
        /// Which field.
        field: &'static str,
        /// What was written.
        value: String,
    },
    /// The first key a mapping retunes comes after the last.
    KeyRange,
    /// A mapping entry that is neither a scale degree (a whole number from
    /// 0) nor `x`.
    BadEntry(String),
    /// The mapping leaves its reference key unmapped, so the reference
    /// frequency belongs to no pitch of the scale.
    UnmappedReference,
}

impl ScalaError {
    fn new(line: usize, kind: ScalaErrorKind) -> Self {
        Self { line, kind }
    }
}

impl fmt::Display for ScalaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: ", self.line)?;
        match &self.kind {
            ScalaErrorKind::Missing(what) => write!(f, "the file ends before the {what}"),
            ScalaErrorKind::BadCount(text) => write!(f, "{text:?} is not a note count"),
            ScalaErrorKind::NoNotes => f.write_str("a scale needs at least one note, its period"),
            ScalaErrorKind::BadPitch(text) => {
                write!(f, "{text:?} is neither cents (with a '.') nor a ratio")
            }
            ScalaErrorKind::NotPositive(text) => write!(f, "the ratio {text:?} is not above 0"),
            ScalaErrorKind::TooFewPitches { expected, found } => {
                write!(f, "the scale ends after {found} of its {expected} pitches")
            }
            ScalaErrorKind::TooMany(text) => write!(f, "{text:?} is one line too many"),
            ScalaErrorKind::PeriodNotRising => f.write_str("the period is not above 1/1"),
            ScalaErrorKind::BadField { field, value } => {
                write!(f, "{value:?} is not a valid {field}")
            }
            ScalaErrorKind::KeyRange => f.write_str("the first key to retune comes after the last"),
            ScalaErrorKind::BadEntry(text) => {
                write!(f, "{text:?} is neither a scale degree nor x")
            }
            ScalaErrorKind::UnmappedReference => f.write_str("the reference key is unmapped"),
        }
    }
}

impl std::error::Error for ScalaError {}

/// The lines of a file that are not comments, with their numbers. A
/// comment line starts with `!`.
fn content_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.trim_start().starts_with('!'))
}

/// The number of the line after the last.
fn end_line(text: &str) -> usize {
    text.lines().count() + 1
}

/// The first word of a line: everything after it is free text.
fn first_word(line: &str) -> &str {
    line.split_whitespace().next().unwrap_or("")
}

/// A scale read from a Scala `.scl` file: a description and the pitches of
/// its degrees, the last of which is the period (usually the octave, 2/1).
///
/// The first degree, 1/1 (0 cents), is not written in the file and not
/// stored. A pitch with a period in it is in cents and may be negative; any
/// other is a ratio of whole numbers, `3/2`, or a whole number alone, `2`.
/// Text after a pitch, and lines starting with `!`, are comments.
///
/// ```
/// use manguebyte_dsp::tuning::ScalaScale;
///
/// let scale = ScalaScale::parse(
///     "! meantone.scl
///      !
///      A pentatonic scale
///       5
///      !
///      9/8
///      386.314    a pure major third, in cents
///      3/2
///      5/3
///      2/1",
/// )?;
/// assert_eq!(scale.description(), "A pentatonic scale");
/// assert_eq!(scale.notes(), 5);
/// assert!((scale.cents()[0] - 203.910).abs() < 0.001);
/// assert_eq!(scale.period_cents(), 1_200.0);
/// # Ok::<(), manguebyte_dsp::tuning::ScalaError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct ScalaScale {
    description: String,
    /// Degrees 1 to n, in cents above 1/1; the last is the period.
    cents: Vec<f64>,
}

impl ScalaScale {
    /// Reads a scale from the text of a `.scl` file.
    ///
    /// The first line that is not a comment is the description, and may be
    /// empty; the next is the note count; then come that many pitches.
    /// Blank lines among and after the pitches are passed over; any further
    /// pitch is an error, as are a period at or below 1/1 and a note count
    /// of 0.
    pub fn parse(text: &str) -> Result<Self, ScalaError> {
        let mut lines = content_lines(text);
        let end = end_line(text);
        let Some((_, description)) = lines.next() else {
            return Err(ScalaError::new(end, ScalaErrorKind::Missing("description")));
        };
        let Some((count_line, count)) = lines.next() else {
            return Err(ScalaError::new(end, ScalaErrorKind::Missing("note count")));
        };
        let count_word = first_word(count);
        let notes: usize = count_word.parse().map_err(|_| {
            ScalaError::new(count_line, ScalaErrorKind::BadCount(count.trim().into()))
        })?;
        if notes == 0 {
            return Err(ScalaError::new(count_line, ScalaErrorKind::NoNotes));
        }

        let mut cents = Vec::with_capacity(notes.min(4_096));
        let mut last_line = count_line;
        for (number, line) in lines {
            let word = first_word(line);
            if word.is_empty() {
                continue;
            }
            if cents.len() == notes {
                return Err(ScalaError::new(
                    number,
                    ScalaErrorKind::TooMany(line.trim().into()),
                ));
            }
            cents.push(parse_pitch(word).map_err(|kind| ScalaError::new(number, kind))?);
            last_line = number;
        }
        if cents.len() < notes {
            return Err(ScalaError::new(
                end,
                ScalaErrorKind::TooFewPitches {
                    expected: notes,
                    found: cents.len(),
                },
            ));
        }
        if cents[notes - 1] <= 0.0 {
            return Err(ScalaError::new(last_line, ScalaErrorKind::PeriodNotRising));
        }
        Ok(Self {
            description: description.trim().into(),
            cents,
        })
    }

    /// The description, the first line of the file that is not a comment,
    /// without the spaces around it.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// How many notes there are to a period: the note count of the file,
    /// which counts the period and not 1/1.
    pub fn notes(&self) -> usize {
        self.cents.len()
    }

    /// The pitches of degrees 1 to [`notes`](Self::notes), in cents above
    /// 1/1, as the file gives them. The last is the period.
    pub fn cents(&self) -> &[f64] {
        &self.cents
    }

    /// The period, the interval at which the scale repeats, in cents: its
    /// last pitch. 1 200 for a scale of the octave.
    pub fn period_cents(&self) -> f64 {
        self.cents[self.cents.len() - 1]
    }

    /// The pitch of any degree, in cents above degree 0: degrees past the
    /// last repeat the scale a period higher, negative degrees a period
    /// lower.
    pub fn degree_cents(&self, degree: i64) -> f64 {
        let notes = self.cents.len() as i64;
        let period = degree.div_euclid(notes);
        let step = degree.rem_euclid(notes) as usize;
        let within = if step == 0 { 0.0 } else { self.cents[step - 1] };
        period as f64 * self.period_cents() + within
    }
}

/// One pitch of a scale, in cents.
fn parse_pitch(word: &str) -> Result<f64, ScalaErrorKind> {
    let bad = || ScalaErrorKind::BadPitch(word.into());
    if word.contains('.') {
        let digits = word.strip_prefix(['-', '+']).unwrap_or(word);
        if digits == "." || !digits.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return Err(bad());
        }
        let cents: f64 = word.parse().map_err(|_| bad())?;
        return if cents.is_finite() {
            Ok(cents)
        } else {
            Err(bad())
        };
    }
    let (numerator, denominator) = word.split_once('/').unwrap_or((word, "1"));
    let negative = numerator.starts_with('-');
    let numerator = numerator.strip_prefix('-').unwrap_or(numerator);
    let whole = |text: &str| -> Result<f64, ScalaErrorKind> {
        if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        text.parse().map_err(|_| bad())
    };
    let (numerator, denominator) = (whole(numerator)?, whole(denominator)?);
    if negative || numerator == 0.0 || denominator == 0.0 {
        return Err(ScalaErrorKind::NotPositive(word.into()));
    }
    // Each side apart, so that ratios of large whole numbers keep their
    // precision.
    let cents = 1_200.0 * (numerator.log2() - denominator.log2());
    if cents.is_finite() {
        Ok(cents)
    } else {
        Err(bad())
    }
}

/// Which scale degree each key plays, read from a Scala `.kbm` file, and the
/// frequency that ties the scale to absolute pitch.
///
/// A mapping is a pattern of `size` keys that repeats up and down the
/// keyboard. The pattern's first key is the middle key; each entry names
/// the degree its key plays, or `x` for a key that plays nothing. One
/// repetition of the pattern up moves the pitch by the formal octave, the
/// pitch of a stated degree. A size of 0 is a linear mapping: every key
/// plays the next degree, and the scale's own period separates repeats.
/// The reference key sounds the reference frequency. Keys outside the
/// range to retune are not retuned: [`Tuning`](super::Tuning) leaves them
/// in equal temperament with A4 at 440 Hz.
///
/// ```
/// use manguebyte_dsp::tuning::KeyboardMapping;
///
/// // The white keys only, C to C, with A4 at 432 Hz.
/// let mapping = KeyboardMapping::parse(
///     "! white.kbm
///      12
///      0
///      127
///      60
///      69
///      432.0
///      7
///      ! the mapping
///      0
///      x
///      1
///      x
///      2
///      3
///      x
///      4
///      x
///      5
///      x
///      6",
/// )?;
/// assert_eq!(mapping.degree(60), Some(0));
/// assert_eq!(mapping.degree(61), None);
/// assert_eq!(mapping.degree(72), Some(7));
/// assert_eq!(mapping.degree(59), Some(-1));
/// # Ok::<(), manguebyte_dsp::tuning::ScalaError>(())
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct KeyboardMapping {
    first: u8,
    last: u8,
    middle: u8,
    reference_key: u8,
    reference_hz: f64,
    octave_degree: i64,
    /// One per key of the pattern; empty for a linear mapping.
    entries: Vec<Option<i64>>,
    /// The size of the pattern, which may be more than the entries given.
    size: usize,
}

/// The header fields of a `.kbm` file, in order.
const FIELDS: [&str; 7] = [
    "map size",
    "first key to retune",
    "last key to retune",
    "middle key",
    "reference key",
    "reference frequency",
    "formal octave degree",
];

impl KeyboardMapping {
    /// Reads a mapping from the text of a `.kbm` file.
    ///
    /// Seven header lines (the map size, the first and last keys to retune,
    /// the middle key, the reference key, the reference frequency and the
    /// formal octave degree), then one entry per key of the pattern: a
    /// degree, or `x` for an unmapped key. Entries left out at the end are
    /// unmapped. Lines starting with `!` and text after a number are
    /// comments. Keys run from 0 to 127.
    pub fn parse(text: &str) -> Result<Self, ScalaError> {
        let mut lines = content_lines(text).filter(|(_, line)| !first_word(line).is_empty());
        let end = end_line(text);
        let mut header = [(0_usize, ""); 7];
        for (slot, field) in header.iter_mut().zip(FIELDS) {
            let Some((number, line)) = lines.next() else {
                return Err(ScalaError::new(end, ScalaErrorKind::Missing(field)));
            };
            *slot = (number, first_word(line));
        }
        let field_error = |index: usize| {
            ScalaError::new(
                header[index].0,
                ScalaErrorKind::BadField {
                    field: FIELDS[index],
                    value: header[index].1.into(),
                },
            )
        };
        let size: usize = header[0].1.parse().map_err(|_| field_error(0))?;
        let key = |index: usize| -> Result<u8, ScalaError> {
            match header[index].1.parse::<u8>() {
                Ok(key) if key <= 127 => Ok(key),
                _ => Err(field_error(index)),
            }
        };
        let (first, last, middle, reference_key) = (key(1)?, key(2)?, key(3)?, key(4)?);
        if first > last {
            return Err(ScalaError::new(header[2].0, ScalaErrorKind::KeyRange));
        }
        let reference_hz: f64 = header[5].1.parse().map_err(|_| field_error(5))?;
        if !(reference_hz.is_finite() && reference_hz > 0.0) {
            return Err(field_error(5));
        }
        let octave_degree: i64 = match header[6].1.parse::<u32>() {
            Ok(degree) => i64::from(degree),
            Err(_) => return Err(field_error(6)),
        };

        let mut entries = Vec::with_capacity(size.min(1_024));
        for (number, line) in lines {
            let word = first_word(line);
            if entries.len() == size {
                return Err(ScalaError::new(
                    number,
                    ScalaErrorKind::TooMany(line.trim().into()),
                ));
            }
            let entry = if word.eq_ignore_ascii_case("x") {
                None
            } else {
                match word.parse::<u32>() {
                    Ok(degree) => Some(i64::from(degree)),
                    Err(_) => {
                        return Err(ScalaError::new(
                            number,
                            ScalaErrorKind::BadEntry(word.into()),
                        ));
                    }
                }
            };
            entries.push(entry);
        }
        let mapping = Self {
            first,
            last,
            middle,
            reference_key,
            reference_hz,
            octave_degree,
            entries,
            size,
        };
        if mapping.degree(i64::from(reference_key)).is_none() {
            return Err(ScalaError::new(
                header[4].0,
                ScalaErrorKind::UnmappedReference,
            ));
        }
        Ok(mapping)
    }

    /// A linear mapping of every key: `middle` plays degree 0, each key
    /// above the next degree up, and `reference_key` sounds at
    /// `reference_hz`. What a scale without a mapping file gets, with the
    /// middle key 60.
    pub fn linear(middle: u8, reference_key: u8, reference_hz: f64) -> Self {
        Self {
            first: 0,
            last: 127,
            middle: middle.min(127),
            reference_key: reference_key.min(127),
            reference_hz,
            octave_degree: 0,
            entries: Vec::new(),
            size: 0,
        }
    }

    /// The size of the repeating pattern; 0 for a linear mapping.
    pub fn size(&self) -> usize {
        self.size
    }

    /// The first and last keys retuned, both included.
    pub fn retuned_keys(&self) -> core::ops::RangeInclusive<u8> {
        self.first..=self.last
    }

    /// The key that plays degree 0.
    pub fn middle_key(&self) -> u8 {
        self.middle
    }

    /// The key whose frequency is given.
    pub fn reference_key(&self) -> u8 {
        self.reference_key
    }

    /// The frequency of the reference key, in hertz.
    pub fn reference_hz(&self) -> f64 {
        self.reference_hz
    }

    /// The degree whose pitch is the distance between one repetition of the
    /// pattern and the next. Not used by a linear mapping.
    pub fn octave_degree(&self) -> i64 {
        self.octave_degree
    }

    /// Which degree `key` plays, or `None` for an unmapped key: its entry in
    /// the pattern, plus the [formal octave degree](Self::octave_degree)
    /// for each repetition of the pattern above the middle key (less for
    /// each below); for a linear mapping, its distance from the middle key.
    /// The key may lie outside 0 to 127.
    pub fn degree(&self, key: i64) -> Option<i64> {
        let (repeats, entry) = self.place(key)?;
        Some(repeats * self.octave_degree + entry)
    }

    /// How many repetitions of the pattern `key` lies above the middle key,
    /// and its entry; for a linear mapping, no repetitions and the distance
    /// from the middle key.
    fn place(&self, key: i64) -> Option<(i64, i64)> {
        let offset = key - i64::from(self.middle);
        if self.size == 0 {
            return Some((0, offset));
        }
        let size = self.size as i64;
        let entry = (*self.entries.get(offset.rem_euclid(size) as usize)?)?;
        Some((offset.div_euclid(size), entry))
    }

    /// The pitch `key` plays in `scale`, in cents above the middle key's
    /// degree 0, or `None` for an unmapped key. A repetition of the pattern
    /// moves by the pitch of the formal octave degree, which need not be
    /// the scale's period.
    pub(crate) fn key_cents(&self, scale: &ScalaScale, key: i64) -> Option<f64> {
        let (repeats, entry) = self.place(key)?;
        let octave = if repeats == 0 {
            0.0
        } else {
            repeats as f64 * scale.degree_cents(self.octave_degree)
        };
        Some(octave + scale.degree_cents(entry))
    }

    /// Whether `key` is in the range to retune.
    pub(crate) fn retunes(&self, key: i64) -> bool {
        (i64::from(self.first)..=i64::from(self.last)).contains(&key)
    }
}
