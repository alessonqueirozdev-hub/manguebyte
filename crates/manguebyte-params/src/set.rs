use std::fmt;
use std::str::FromStr;

use manguebyte_core::ParamId;

use crate::param::is_valid_id;
use crate::{Param, ParamDescription};

/// A group of parameters, usually a struct with one field per parameter.
///
/// Implement it with [`param_set!`](crate::param_set) and share the struct
/// between threads in an [`Arc`](std::sync::Arc): the audio thread reads the
/// fields directly, while user interfaces and hosts go through this trait.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_params::{BoolParam, FloatParam, FloatRange, Param, ParamSet, param_set};
///
/// struct FilterParams {
///     cutoff: FloatParam,
///     bypass: BoolParam,
/// }
/// param_set!(FilterParams { cutoff, bypass });
///
/// let params = Arc::new(FilterParams {
///     cutoff: FloatParam::new("cutoff", "Cutoff", FloatRange::logarithmic(20.0, 20_000.0), 1_000.0),
///     bypass: BoolParam::new("bypass", "Bypass", false),
/// });
/// params.validate().unwrap();
///
/// // The audio thread reads typed fields. No lookups by name, no locks.
/// let _cutoff_hz = params.cutoff.value();
///
/// // Anything generic goes through the trait.
/// params.find("cutoff").unwrap().set_normalized(1.0);
/// assert_eq!(params.cutoff.value(), 20_000.0);
///
/// // State survives as text and restores by id.
/// let saved = params.save().to_string();
/// params.reset_all();
/// params.load(&saved.parse().unwrap());
/// assert_eq!(params.cutoff.value(), 20_000.0);
/// ```
pub trait ParamSet: Send + Sync {
    /// Calls `visitor` once for every parameter, always in the same order.
    fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param));

    /// How many parameters the set holds.
    fn len(&self) -> usize {
        let mut count = 0;
        self.visit(&mut |_| count += 1);
        count
    }

    /// Whether the set holds no parameters.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Finds a parameter by identifier.
    ///
    /// This walks the set, so it is for user interfaces and hosts. Audio code
    /// should read the struct's fields directly.
    fn find(&self, id: &str) -> Option<&dyn Param> {
        let mut found = None;
        self.visit(&mut |param| {
            if found.is_none() && param.id() == id {
                found = Some(param);
            }
        });
        found
    }

    /// Finds a parameter by its compact id, as carried in events.
    ///
    /// Real-time safe: it walks the set comparing numbers and allocates
    /// nothing.
    fn find_param(&self, id: ParamId) -> Option<&dyn Param> {
        let mut found = None;
        self.visit(&mut |param| {
            if found.is_none() && param.param_id() == id {
                found = Some(param);
            }
        });
        found
    }

    /// Checks that every parameter can be told apart from the others: no
    /// two share an identifier, and no two identifiers hash to the same
    /// [`ParamId`].
    ///
    /// Call it once at start-up, or in a test. A clash would make saved
    /// state and automation ambiguous.
    fn validate(&self) -> Result<(), ParamSetError> {
        let mut ids: Vec<(ParamId, &str)> = Vec::new();
        self.visit(&mut |param| ids.push((param.param_id(), param.id())));
        ids.sort_unstable();
        match ids.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            Some(pair) if pair[0].1 == pair[1].1 => {
                Err(ParamSetError::DuplicateId(pair[0].1.to_owned()))
            }
            Some(pair) => Err(ParamSetError::HashCollision(
                pair[0].1.to_owned(),
                pair[1].1.to_owned(),
            )),
            None => Ok(()),
        }
    }

    /// A description of every parameter, in the order
    /// [`visit`](Self::visit) gives them. Allocates.
    fn descriptions(&self) -> Vec<ParamDescription> {
        let mut descriptions = Vec::new();
        self.visit(&mut |param| descriptions.push(param.describe()));
        descriptions
    }

    /// [`descriptions`](Self::descriptions) as one JSON array, in the format
    /// of [`ParamDescription::to_json`]. Allocates.
    ///
    /// ```
    /// use manguebyte_params::{BoolParam, IntParam, ParamSet, param_set};
    ///
    /// struct Params { voices: IntParam, mono: BoolParam }
    /// param_set!(Params { voices, mono });
    ///
    /// let params = Params {
    ///     voices: IntParam::new("voices", "Voices", 1, 16, 8),
    ///     mono: BoolParam::new("mono", "Mono", false).hidden(),
    /// };
    /// assert_eq!(
    ///     params.descriptions_json(),
    ///     concat!(
    ///         r#"[{"id":"voices","param_id":1816020030,"name":"Voices","unit":"","#,
    ///         r#""min":1,"max":16,"default":8,"scale":{"kind":"stepped","steps":15,"step":1},"#,
    ///         r#""flags":{"automatable":true,"hidden":false,"bypass":false}},"#,
    ///         r#"{"id":"mono","param_id":4068929130,"name":"Mono","unit":"","#,
    ///         r#""min":0,"max":1,"default":0,"#,
    ///         r#""scale":{"kind":"stepped","steps":1,"step":1,"names":["Off","On"]},"#,
    ///         r#""flags":{"automatable":true,"hidden":true,"bypass":false}}]"#,
    ///     )
    /// );
    /// ```
    fn descriptions_json(&self) -> String {
        ParamDescription::json_array(&self.descriptions())
    }

    /// The text, with its unit, for the value the parameter `id` would hold
    /// after [`set_plain`](Param::set_plain)`(plain)`, as its
    /// [`stored_plain`](Param::stored_plain) predicts, written by its
    /// [`display_plain`](Param::display_plain). Nothing is stored.
    ///
    /// For the parameter types of this crate that value is clamped to the
    /// range and snapped to the step. A parameter of another type that
    /// keeps the default `stored_plain` gets the text of `plain` as it is
    /// given; a wrapper around one of this crate's types should forward
    /// `stored_plain`, `display_plain` and [`parse_display`](Param::parse_display)
    /// so that its text is the wrapped parameter's.
    ///
    /// `None` if there is no such parameter or `plain` is not a number.
    /// Allocates.
    ///
    /// ```
    /// use manguebyte_params::{ChoiceParam, FloatParam, FloatRange, ParamSet, param_set};
    ///
    /// struct Params { cutoff: FloatParam, shape: ChoiceParam }
    /// param_set!(Params { cutoff, shape });
    ///
    /// let params = Params {
    ///     cutoff: FloatParam::new("cutoff", "Cutoff", FloatRange::logarithmic(20.0, 20_000.0), 1_000.0)
    ///         .with_unit("Hz")
    ///         .with_precision(0),
    ///     shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw"], 0),
    /// };
    /// assert_eq!(params.value_to_text("cutoff", 440.0).as_deref(), Some("440 Hz"));
    /// assert_eq!(params.value_to_text("cutoff", 1e9).as_deref(), Some("20000 Hz"));
    /// assert_eq!(params.value_to_text("shape", 1.0).as_deref(), Some("Saw"));
    /// assert_eq!(params.text_to_value("cutoff", "2.5 kHz"), Some(2_500.0));
    /// assert_eq!(params.text_to_value("shape", "SAW"), Some(1.0));
    /// assert_eq!(params.text_to_value("shape", "Square"), None);
    /// assert_eq!(params.value_to_text("nope", 1.0), None);
    /// ```
    fn value_to_text(&self, id: &str, plain: f64) -> Option<String> {
        let param = self.find(id)?;
        let stored = param.stored_plain(plain)?;
        Some(param.display_plain(stored))
    }

    /// Reads a value for the parameter `id` from text a person typed or
    /// [`value_to_text`](Self::value_to_text) wrote, with
    /// [`Param::parse_display`]. Nothing is stored.
    ///
    /// For the parameter types of this crate, unless a float was given a
    /// formatter and a parser that disagree, the text `value_to_text`
    /// writes reads back as a value the parameter can hold whose text is
    /// the same: the same option of a choice or a switch, the same whole
    /// number, and a float to the precision of its text. The one exception
    /// is a choice or a switch with two names that are exactly the same,
    /// which cannot be told apart: both read as the first.
    ///
    /// `None` if there is no such parameter or the text means nothing to
    /// it. Allocates.
    fn text_to_value(&self, id: &str, text: &str) -> Option<f64> {
        self.find(id)?.parse_display(text)
    }

    /// Puts every parameter back to its default.
    fn reset_all(&self) {
        self.visit(&mut |param| param.reset());
    }

    /// Captures the value of every parameter. Allocates.
    fn save(&self) -> ParamState {
        let mut values = Vec::new();
        self.visit(&mut |param| values.push((param.id().to_owned(), param.plain())));
        ParamState { values }
    }

    /// Restores values captured by [`save`](Self::save), matching by
    /// identifier.
    ///
    /// State from another version of the software loads as far as it can:
    /// parameters the state does not mention keep their current value,
    /// entries for parameters that no longer exist are skipped, and values
    /// outside today's range are clamped. The report says what happened.
    fn load(&self, state: &ParamState) -> LoadReport {
        let mut report = LoadReport::default();
        self.visit(&mut |param| match state.get(param.id()) {
            Some(value) => {
                param.set_plain(value);
                report.restored += 1;
            }
            None => report.missing.push(param.id().to_owned()),
        });
        for (id, _) in &state.values {
            if self.find(id).is_none() {
                report.unknown.push(id.clone());
            }
        }
        report
    }
}

/// Implements [`ParamSet`] for a struct by listing its parameter fields.
///
/// A field that is itself a `ParamSet` can be included by prefixing it with
/// `nested`:
///
/// ```
/// use manguebyte_params::{BoolParam, IntParam, ParamSet, param_set};
///
/// struct Voice { unison: IntParam }
/// param_set!(Voice { unison });
///
/// struct Synth { mono: BoolParam, voice: Voice }
/// param_set!(Synth { mono, nested voice });
///
/// let synth = Synth {
///     mono: BoolParam::new("mono", "Mono", false),
///     voice: Voice { unison: IntParam::new("unison", "Unison", 1, 8, 1) },
/// };
/// assert_eq!(synth.len(), 2);
/// ```
#[macro_export]
macro_rules! param_set {
    ($type:ty { $($body:tt)* }) => {
        impl $crate::ParamSet for $type {
            fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn $crate::Param)) {
                $crate::param_set!(@fields self, visitor, $($body)*);
            }
        }
    };
    (@fields $self:ident, $visitor:ident, ) => {};
    (@fields $self:ident, $visitor:ident, nested $field:ident $(, $($rest:tt)*)?) => {
        $crate::ParamSet::visit(&$self.$field, $visitor);
        $crate::param_set!(@fields $self, $visitor, $($($rest)*)?);
    };
    (@fields $self:ident, $visitor:ident, $field:ident $(, $($rest:tt)*)?) => {
        $visitor(&$self.$field);
        $crate::param_set!(@fields $self, $visitor, $($($rest)*)?);
    };
}

/// Why the parameters of a set cannot be told apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamSetError {
    /// Two parameters share this identifier.
    DuplicateId(String),
    /// These two identifiers hash to the same [`ParamId`]. Rename one of
    /// them. With 32-bit hashes this is rare but possible.
    HashCollision(String, String),
}

impl fmt::Display for ParamSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId(id) => {
                write!(f, "more than one parameter has the id \"{id}\"")
            }
            Self::HashCollision(first, second) => write!(
                f,
                "parameter ids \"{first}\" and \"{second}\" hash to the same number"
            ),
        }
    }
}

impl std::error::Error for ParamSetError {}

/// What [`ParamSet::load`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// How many parameters were given a value from the state.
    pub restored: usize,
    /// Parameters the state said nothing about. They kept their value.
    pub missing: Vec<String>,
    /// Entries in the state that match no parameter. They were skipped.
    pub unknown: Vec<String>,
}

impl LoadReport {
    /// Whether the state matched the set exactly.
    pub fn is_exact(&self) -> bool {
        self.missing.is_empty() && self.unknown.is_empty()
    }
}

/// A snapshot of parameter values, keyed by identifier.
///
/// It holds real values in each parameter's own unit rather than `0..=1`
/// positions, so a later version of the software can widen or reshape a
/// range without changing what an old session sounds like.
///
/// As text, it is a header line followed by one `id=value` line per
/// parameter:
///
/// ```text
/// manguebyte-params 1
/// cutoff=1000
/// bypass=0
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParamState {
    values: Vec<(String, f64)>,
}

const HEADER: &str = "manguebyte-params";
const VERSION: u32 = 1;

impl ParamState {
    /// An empty snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// The saved value of a parameter, if the snapshot has one.
    pub fn get(&self, id: &str) -> Option<f64> {
        self.values
            .iter()
            .find(|(candidate, _)| candidate == id)
            .map(|&(_, value)| value)
    }

    /// Sets the saved value of a parameter, replacing any earlier one.
    ///
    /// # Panics
    ///
    /// Panics if `id` is not a valid identifier or `value` is not finite.
    pub fn set(&mut self, id: &str, value: f64) {
        assert!(is_valid_id(id), "invalid parameter id {id:?}");
        assert!(value.is_finite(), "parameter values must be finite");
        match self
            .values
            .iter_mut()
            .find(|(candidate, _)| candidate == id)
        {
            Some(entry) => entry.1 = value,
            None => self.values.push((id.to_owned(), value)),
        }
    }

    /// How many values the snapshot holds.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the snapshot holds no values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The saved values, in the order they were recorded.
    pub fn iter(&self) -> impl Iterator<Item = (&str, f64)> {
        self.values.iter().map(|(id, value)| (id.as_str(), *value))
    }
}

impl fmt::Display for ParamState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{HEADER} {VERSION}")?;
        for (id, value) in &self.values {
            // `{}` on an `f64` prints the shortest text that reads back as
            // exactly the same number.
            writeln!(f, "{id}={value}")?;
        }
        Ok(())
    }
}

/// Why text could not be read as a [`ParamState`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseStateError {
    /// The text does not start with the `manguebyte-params` header.
    MissingHeader,
    /// The state was written by a newer, incompatible version of the format.
    UnsupportedVersion(u32),
    /// A line is not of the form `id=value`. Holds the 1-based line number.
    MalformedLine(usize),
}

impl fmt::Display for ParseStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHeader => f.write_str("not a parameter state: header line missing"),
            Self::UnsupportedVersion(version) => {
                write!(
                    f,
                    "parameter state format version {version} is not supported"
                )
            }
            Self::MalformedLine(line) => write!(f, "parameter state line {line} is malformed"),
        }
    }
}

impl std::error::Error for ParseStateError {}

impl FromStr for ParamState {
    type Err = ParseStateError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let mut lines = text.lines().enumerate();
        let version = lines
            .next()
            .and_then(|(_, line)| line.trim().strip_prefix(HEADER))
            .and_then(|rest| rest.trim().parse::<u32>().ok())
            .ok_or(ParseStateError::MissingHeader)?;
        if version != VERSION {
            return Err(ParseStateError::UnsupportedVersion(version));
        }

        let mut state = Self::new();
        for (index, line) in lines {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let entry = line.split_once('=').and_then(|(id, value)| {
                let value = value.trim().parse::<f64>().ok()?;
                (is_valid_id(id) && value.is_finite()).then_some((id, value))
            });
            let (id, value) = entry.ok_or(ParseStateError::MalformedLine(index + 1))?;
            state.set(id, value);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoolParam, ChoiceParam, FloatParam, FloatRange, IntParam};

    struct Envelope {
        attack: FloatParam,
        release: FloatParam,
    }
    param_set!(Envelope { attack, release });

    struct Synth {
        cutoff: FloatParam,
        voices: IntParam,
        shape: ChoiceParam,
        bypass: BoolParam,
        envelope: Envelope,
    }
    param_set!(Synth { cutoff, voices, shape, nested envelope, bypass, });

    fn seconds(id: &str, name: &str, default: f32) -> FloatParam {
        FloatParam::new(id, name, FloatRange::skewed(0.001, 10.0, 0.3), default).with_unit("s")
    }

    fn synth() -> Synth {
        Synth {
            cutoff: FloatParam::new(
                "cutoff",
                "Cutoff",
                FloatRange::logarithmic(20.0, 20_000.0),
                1_000.0,
            ),
            voices: IntParam::new("voices", "Voices", 1, 16, 8),
            shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 0),
            bypass: BoolParam::new("bypass", "Bypass", false),
            envelope: Envelope {
                attack: seconds("env.attack", "Attack", 0.01),
                release: seconds("env.release", "Release", 0.3),
            },
        }
    }

    #[test]
    fn visits_in_declaration_order_including_nested_sets() {
        let synth = synth();
        let mut ids = Vec::new();
        synth.visit(&mut |param| ids.push(param.id()));
        assert_eq!(
            ids,
            [
                "cutoff",
                "voices",
                "shape",
                "env.attack",
                "env.release",
                "bypass"
            ]
        );
        assert_eq!(synth.len(), 6);
        assert!(!synth.is_empty());
        assert!(synth.validate().is_ok());
    }

    #[test]
    fn finds_by_id() {
        let synth = synth();
        assert_eq!(synth.find("env.release").unwrap().name(), "Release");
        assert!(synth.find("nope").is_none());

        const RELEASE: ParamId = ParamId::from_name("env.release");
        assert_eq!(synth.envelope.release.param_id(), RELEASE);
        assert_eq!(synth.find_param(RELEASE).unwrap().name(), "Release");
        assert!(synth.find_param(ParamId::from_name("nope")).is_none());
    }

    #[test]
    fn detects_ids_that_hash_to_the_same_number() {
        // A known 32-bit FNV-1a collision.
        assert_eq!(
            ParamId::from_name("costarring"),
            ParamId::from_name("liquid")
        );
        let broken = Envelope {
            attack: seconds("costarring", "Attack", 0.1),
            release: seconds("liquid", "Release", 0.1),
        };
        assert_eq!(
            broken.validate(),
            Err(ParamSetError::HashCollision(
                String::from("costarring"),
                String::from("liquid")
            ))
        );
    }

    #[test]
    fn detects_duplicate_ids() {
        let broken = Envelope {
            attack: seconds("time", "Attack", 0.1),
            release: seconds("time", "Release", 0.1),
        };
        assert_eq!(
            broken.validate(),
            Err(ParamSetError::DuplicateId(String::from("time")))
        );
    }

    #[test]
    fn state_round_trips_through_text_exactly() {
        let synth = synth();
        synth.cutoff.set(1_234.567);
        synth.voices.set(3);
        synth.shape.set_index(2);
        synth.bypass.set(true);
        synth.envelope.attack.set(0.123_456_79);

        let text = synth.save().to_string();
        assert!(text.starts_with("manguebyte-params 1\ncutoff="));

        let restored = self::synth();
        let report = restored.load(&text.parse().unwrap());
        assert!(report.is_exact());
        assert_eq!(report.restored, 6);
        assert_eq!(restored.cutoff.value(), synth.cutoff.value());
        assert_eq!(restored.voices.value(), 3);
        assert_eq!(restored.shape.index(), 2);
        assert!(restored.bypass.value());
        assert_eq!(
            restored.envelope.attack.value(),
            synth.envelope.attack.value()
        );
    }

    #[test]
    fn state_from_another_version_loads_as_far_as_it_can() {
        let synth = synth();
        synth.voices.set(4);
        let old = "manguebyte-params 1\n\
                   cutoff=90000\n\
                   removed.param=0.5\n\
                   \n\
                   shape=1\n";
        let report = synth.load(&old.parse().unwrap());

        assert_eq!(synth.cutoff.value(), 20_000.0, "clamped to today's range");
        assert_eq!(synth.shape.index(), 1);
        assert_eq!(synth.voices.value(), 4, "not in the state, so untouched");
        assert_eq!(report.restored, 2);
        assert_eq!(report.unknown, ["removed.param"]);
        assert_eq!(
            report.missing,
            ["voices", "env.attack", "env.release", "bypass"]
        );
        assert!(!report.is_exact());
    }

    #[test]
    fn rejects_text_that_is_not_state() {
        assert_eq!(
            "".parse::<ParamState>(),
            Err(ParseStateError::MissingHeader)
        );
        assert_eq!(
            "cutoff=1".parse::<ParamState>(),
            Err(ParseStateError::MissingHeader)
        );
        assert_eq!(
            "manguebyte-params 2\n".parse::<ParamState>(),
            Err(ParseStateError::UnsupportedVersion(2))
        );
        for bad in [
            "cutoff",
            "cutoff=abc",
            "cut off=1",
            "=1",
            "cutoff=NaN",
            "cutoff=inf",
        ] {
            let text = format!("manguebyte-params 1\nok=1\n{bad}\n");
            assert_eq!(
                text.parse::<ParamState>(),
                Err(ParseStateError::MalformedLine(3)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_later_entry_replaces_an_earlier_one() {
        let state: ParamState = "manguebyte-params 1\r\na=1\r\na=2\r\n".parse().unwrap();
        assert_eq!(state.len(), 1);
        assert_eq!(state.get("a"), Some(2.0));
        assert_eq!(state.iter().collect::<Vec<_>>(), [("a", 2.0)]);
    }

    #[test]
    fn a_set_can_be_used_as_a_trait_object() {
        let synth = synth();
        let dynamic: &dyn ParamSet = &synth;
        assert_eq!(dynamic.len(), 6);
    }
}
