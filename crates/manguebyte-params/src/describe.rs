use std::fmt::Write as _;

use manguebyte_core::ParamId;

use crate::{Param, ParamFlags, ParamInfo, Smoothing};

/// How the values of a parameter are spread over the travel of a control.
///
/// Each variant says how to turn a value into a position in `0..=1` and
/// back, so that an editor written in another language can draw a control
/// that behaves like the parameter. `linear` below stands for
/// `(value - min) / (max - min)`.
///
/// ```
/// use manguebyte_params::{Curve, FloatParam, FloatRange, Param, ParamScale};
///
/// // A step that does not divide the range: 0.9 is the highest value, and
/// // it sits where the range puts it, at position 0.9.
/// let coarse = FloatParam::new("coarse", "Coarse", FloatRange::linear(0.0, 1.0), 0.0)
///     .with_step(0.3);
/// let description = coarse.describe();
/// assert_eq!(description.max, 0.90000004); // what the `f32` holds
/// assert_eq!(
///     description.scale,
///     ParamScale::Stepped { steps: 3, step: 0.3, names: vec![], curve: Curve::Linear, end: 1.0 }
/// );
/// assert!((coarse.plain_to_normalized(description.max) - 0.9).abs() < 1e-6);
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum ParamScale {
    /// The position is `linear`.
    Linear,
    /// The position is `ln(value / min) / ln(max / min)`: equal steps of
    /// position are equal ratios of value. Both ends are positive.
    Log,
    /// A power curve.
    ///
    /// When `symmetric` is false the position is `linear ^ factor`. When it
    /// is true the curve is mirrored around the middle of the range: with
    /// `d = 2 * linear - 1`, the position is
    /// `0.5 * (1 + sign(d) * |d| ^ factor)`.
    Skew {
        /// The exponent. Below 1 gives the low end (or, mirrored, the centre)
        /// more of the travel.
        factor: f64,
        /// Whether the curve is mirrored around the middle of the range.
        symmetric: bool,
    },
    /// A fixed set of values: `min + k * step` for `k` in `0..=steps`, the
    /// last one limited to `max`, so the highest value is `max`.
    ///
    /// A value's position follows `curve` from `min` (position 0) to `end`
    /// (position 1), with `end` in place of `max` in the formulas of the
    /// other variants. For whole numbers, switches and choices, and for a
    /// stepped [`FloatParam`](crate::FloatParam) on a linear range whose
    /// step divides it, the curve is linear and `end` is `max`, so value `k`
    /// sits at position `k / steps`.
    ///
    /// A stepped float on a logarithmic or skewed range keeps that range's
    /// curve. A step that does not divide the range leaves a remainder at
    /// the top. When the remainder is at least half a step, the last step is
    /// that remainder: with a step of 0.4 on `0..1` the values are 0, 0.4,
    /// 0.8 and 1, at positions 0, 0.4, 0.8 and 1. When it is less, the
    /// highest value lies below `end` and never reaches position 1: with a
    /// step of 0.3 on `0..1`, `max` is 0.9
    /// (0.90000004, the `f32` three steps make), `end` is 1, and the highest
    /// value sits at position 0.9. A step more than twice as wide as the
    /// range leaves one value, `min`, and `steps` is 0.
    Stepped {
        /// How many steps separate the lowest value from the highest. A
        /// switch has one.
        steps: u32,
        /// The distance between neighbouring values: 1 for whole numbers,
        /// switches and choices.
        step: f64,
        /// The name of each value, lowest first, for a choice or a switch.
        /// Empty when the values have no names.
        names: Vec<String>,
        /// How positions are spread between `min` and `end`.
        curve: Curve,
        /// The value at position 1. It is `max` unless the highest value
        /// lies below the end of the range.
        end: f64,
    },
}

/// The curve the values of a [`ParamScale::Stepped`] parameter sit on.
///
/// Each variant computes a position the way the [`ParamScale`] variant of
/// the same name does, between `min` and the scale's `end`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Curve {
    /// As [`ParamScale::Linear`].
    Linear,
    /// As [`ParamScale::Log`].
    Log,
    /// As [`ParamScale::Skew`].
    Skew {
        /// The exponent.
        factor: f64,
        /// Whether the curve is mirrored around the middle of the range.
        symmetric: bool,
    },
}

/// Everything an editor needs to know about a parameter to draw a control
/// for it, as plain data.
///
/// [`Param::describe`] makes one and [`ParamSet::descriptions`] makes one
/// per parameter of a set. [`to_json`](Self::to_json) writes it as JSON.
///
/// For a [`FloatParam`](crate::FloatParam), `min`, `max`, `default` and the
/// skew factor are the shortest decimals that read back as the same `f32`
/// (`0.3`, not `0.30000001192092896`), so a value typed into an editor and
/// sent back with [`Param::set_plain`] lands on the same number.
///
/// ```
/// use manguebyte_params::{FloatParam, FloatRange, Param, ParamScale, Smoothing};
///
/// let cutoff = FloatParam::new("cutoff", "Cutoff", FloatRange::logarithmic(20.0, 20_000.0), 1_000.0)
///     .with_unit("Hz")
///     .with_smoothing(Smoothing::Exponential(20.0));
/// let description = cutoff.describe();
/// assert_eq!(description.scale, ParamScale::Log);
/// assert_eq!(description.smoothing_ms(), Some(20.0));
/// assert_eq!(
///     description.to_json(),
///     concat!(
///         r#"{"id":"cutoff","param_id":1801646368,"name":"Cutoff","unit":"Hz","#,
///         r#""min":20,"max":20000,"default":1000,"scale":{"kind":"log"},"smoothing_ms":20,"#,
///         r#""flags":{"automatable":true,"hidden":false,"bypass":false}}"#,
///     )
/// );
/// ```
///
/// [`ParamSet::descriptions`]: crate::ParamSet::descriptions
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ParamDescription {
    /// The identifier saved in state and sessions.
    pub id: String,
    /// The compact number that stands for `id` in events.
    pub param_id: ParamId,
    /// The name shown to people.
    pub name: String,
    /// The unit shown after the value, or an empty string.
    pub unit: String,
    /// The lowest value, in the parameter's own unit.
    pub min: f64,
    /// The highest value the parameter can hold, in its own unit. For a
    /// stepped float whose step does not divide the range this lies below
    /// the end of the range; see [`ParamScale::Stepped`].
    pub max: f64,
    /// The value the parameter starts at.
    pub default: f64,
    /// How values are spread over a control's travel.
    pub scale: ParamScale,
    /// How the processor smooths changes to this parameter, if the
    /// parameter says.
    pub smoothing: Option<Smoothing>,
    /// What a host may do with the parameter.
    pub flags: ParamFlags,
}

impl ParamDescription {
    /// A description worked out from the methods every [`Param`] has.
    ///
    /// This is what [`Param::describe`] returns unless a type overrides it.
    /// It knows only what those methods say: the ends are the values at
    /// positions 0 and 1, a parameter with a step count is
    /// [`Stepped`](ParamScale::Stepped) without names, with its values
    /// assumed to be spread evenly (a linear curve that ends at `max`), any
    /// other is
    /// [`Linear`](ParamScale::Linear), and there is no smoothing. A type
    /// that knows better, such as a wrapper around a [`FloatParam`], should
    /// override [`Param::describe`] and forward it.
    ///
    /// [`FloatParam`]: crate::FloatParam
    pub fn derived<P: Param + ?Sized>(param: &P) -> Self {
        let min = param.normalized_to_plain(0.0);
        let max = param.normalized_to_plain(1.0);
        let scale = match param.step_count() {
            Some(steps) => ParamScale::Stepped {
                steps,
                step: if steps == 0 {
                    0.0
                } else {
                    (max - min) / f64::from(steps)
                },
                names: Vec::new(),
                curve: Curve::Linear,
                end: max,
            },
            None => ParamScale::Linear,
        };
        Self::with(param.info(), min, max, param.default_plain(), scale, None)
    }

    pub(crate) fn with(
        info: &ParamInfo,
        min: f64,
        max: f64,
        default: f64,
        scale: ParamScale,
        smoothing: Option<Smoothing>,
    ) -> Self {
        Self {
            id: info.id().to_owned(),
            param_id: info.param_id(),
            name: info.name().to_owned(),
            unit: info.unit().to_owned(),
            min,
            max,
            default,
            scale,
            smoothing,
            flags: info.flags(),
        }
    }

    /// The smoothing time in milliseconds, if the parameter declares a
    /// smoothing: 0 for [`Smoothing::None`].
    pub fn smoothing_ms(&self) -> Option<f64> {
        self.smoothing.map(|smoothing| match smoothing {
            Smoothing::None => 0.0,
            Smoothing::Linear(milliseconds) | Smoothing::Exponential(milliseconds) => milliseconds,
        })
    }

    /// The description as one JSON object, with no white space.
    ///
    /// The keys, in this order:
    ///
    /// - `id` (string), `param_id` (number, the `u32`), `name`, `unit`
    ///   (strings, `unit` empty when there is none);
    /// - `min`, `max`, `default` (numbers);
    /// - `scale`: an object whose `kind` is `"linear"`, `"log"`, `"skew"`
    ///   (with `factor` and `symmetric`) or `"stepped"` (with `steps`,
    ///   `step`, `names` when the values have names, and `curve` unless the
    ///   curve is linear and ends at `max`), as in [`ParamScale`]. `curve` is
    ///   an object with `kind` (`"linear"`, `"log"` or `"skew"`, the last
    ///   with `factor` and `symmetric`) and `end`; when it is missing, the
    ///   position is `linear`, which for whole numbers, switches and
    ///   choices puts value `k` at `k / steps`;
    /// - `smoothing_ms` (number), only when the parameter declares a
    ///   smoothing;
    /// - `flags`: `{"automatable":…,"hidden":…,"bypass":…}`.
    ///
    /// Numbers are written in plain decimal notation, never with an
    /// exponent; a number that is not finite, which the parameter types of
    /// this crate never produce, is written as `null`. Strings are escaped
    /// as JSON requires; text outside ASCII is written as it is, except
    /// U+2028 and U+2029, which are escaped so the output is also a valid
    /// JavaScript literal.
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        self.write_json(&mut out);
        out
    }

    /// Appends [`to_json`](Self::to_json)'s text to `out`.
    pub fn write_json(&self, out: &mut String) {
        out.push_str("{\"id\":");
        write_string(out, &self.id);
        let _ = write!(out, ",\"param_id\":{}", self.param_id.0);
        out.push_str(",\"name\":");
        write_string(out, &self.name);
        out.push_str(",\"unit\":");
        write_string(out, &self.unit);
        out.push_str(",\"min\":");
        write_number(out, self.min);
        out.push_str(",\"max\":");
        write_number(out, self.max);
        out.push_str(",\"default\":");
        write_number(out, self.default);
        out.push_str(",\"scale\":");
        match &self.scale {
            ParamScale::Linear => out.push_str("{\"kind\":\"linear\"}"),
            ParamScale::Log => out.push_str("{\"kind\":\"log\"}"),
            ParamScale::Skew { factor, symmetric } => {
                out.push_str("{\"kind\":\"skew\",\"factor\":");
                write_number(out, *factor);
                let _ = write!(out, ",\"symmetric\":{symmetric}}}");
            }
            ParamScale::Stepped {
                steps,
                step,
                names,
                curve,
                end,
            } => {
                let _ = write!(out, "{{\"kind\":\"stepped\",\"steps\":{steps},\"step\":");
                write_number(out, *step);
                if !names.is_empty() {
                    out.push_str(",\"names\":[");
                    for (index, name) in names.iter().enumerate() {
                        if index > 0 {
                            out.push(',');
                        }
                        write_string(out, name);
                    }
                    out.push(']');
                }
                // Compared as bits, so that a `max` that is not a number
                // (from someone else's parameter) still counts as the end.
                if *curve != Curve::Linear || end.to_bits() != self.max.to_bits() {
                    out.push_str(",\"curve\":{\"kind\":");
                    match curve {
                        Curve::Linear => out.push_str("\"linear\""),
                        Curve::Log => out.push_str("\"log\""),
                        Curve::Skew { factor, symmetric } => {
                            out.push_str("\"skew\",\"factor\":");
                            write_number(out, *factor);
                            let _ = write!(out, ",\"symmetric\":{symmetric}");
                        }
                    }
                    out.push_str(",\"end\":");
                    write_number(out, *end);
                    out.push('}');
                }
                out.push('}');
            }
        }
        if let Some(milliseconds) = self.smoothing_ms() {
            out.push_str(",\"smoothing_ms\":");
            write_number(out, milliseconds);
        }
        let ParamFlags {
            automatable,
            hidden,
            bypass,
        } = self.flags;
        let _ = write!(
            out,
            ",\"flags\":{{\"automatable\":{automatable},\"hidden\":{hidden},\"bypass\":{bypass}}}}}"
        );
    }

    /// A JSON array holding [`to_json`](Self::to_json)'s object for each
    /// description, in order.
    ///
    /// ```
    /// use manguebyte_params::ParamDescription;
    ///
    /// assert_eq!(ParamDescription::json_array(&[]), "[]");
    /// ```
    pub fn json_array(descriptions: &[ParamDescription]) -> String {
        let mut out = String::from("[");
        for (index, description) in descriptions.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            description.write_json(&mut out);
        }
        out.push(']');
        out
    }
}

/// The shortest decimal that reads back as `value`, as an `f64`.
///
/// `f64::from(0.3_f32)` is `0.30000001192092896`, which is what the `f32`
/// holds but not what anyone wrote; this gives `0.3`, which converts back to
/// the same `f32`.
pub(crate) fn decimal(value: f32) -> f64 {
    value
        .to_string()
        .parse()
        .unwrap_or_else(|_| f64::from(value))
}

/// Writes `value` as a JSON number, or `null` if it is not finite.
fn write_number(out: &mut String, value: f64) {
    if value.is_finite() {
        // `{}` on an `f64` never uses an exponent and prints the shortest
        // text that reads back as the same number; `-0` is valid JSON.
        let _ = write!(out, "{value}");
    } else {
        out.push_str("null");
    }
}

/// Writes `text` as a JSON string (RFC 8259, section 7).
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if c < ' ' || c == '\u{2028}' || c == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> String {
        let mut out = String::new();
        write_string(&mut out, text);
        out
    }

    #[test]
    fn escapes_what_json_requires_and_nothing_else() {
        assert_eq!(string(""), r#""""#);
        assert_eq!(string(r#"a"b\c/d"#), r#""a\"b\\c/d""#);
        assert_eq!(
            string("\n\r\t\u{8}\u{c}\u{0}\u{1f}\u{7f}"),
            "\"\\n\\r\\t\\b\\f\\u0000\\u001f\u{7f}\""
        );
        assert_eq!(string("Frequência µs 🎛"), "\"Frequência µs 🎛\"");
        assert_eq!(string("\u{2028}\u{2029}"), "\"\\u2028\\u2029\"");
    }

    #[test]
    fn numbers_are_plain_decimals_or_null() {
        let number = |value: f64| {
            let mut out = String::new();
            write_number(&mut out, value);
            out
        };
        assert_eq!(number(20_000.0), "20000");
        assert_eq!(number(-0.5), "-0.5");
        assert_eq!(number(-0.0), "-0");
        assert_eq!(number(1e-7), "0.0000001");
        assert_eq!(number(1e21), "1000000000000000000000");
        assert_eq!(number(f64::NAN), "null");
        assert_eq!(number(f64::INFINITY), "null");
        assert_eq!(number(f64::NEG_INFINITY), "null");
    }

    #[test]
    fn decimal_is_the_shortest_text_of_the_f32() {
        assert_eq!(decimal(0.3), 0.3);
        assert_eq!(decimal(0.3) as f32, 0.3_f32);
        assert_eq!(decimal(20_000.0), 20_000.0);
        assert_eq!(decimal(f32::MAX) as f32, f32::MAX);
        assert_eq!(decimal(f32::MIN_POSITIVE) as f32, f32::MIN_POSITIVE);
        assert_eq!(decimal(1.0e-45) as f32, 1.0e-45_f32);
        assert_eq!(decimal(-0.0).to_bits(), (-0.0_f64).to_bits());
    }
}
