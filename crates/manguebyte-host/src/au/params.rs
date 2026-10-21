//! A unit's parameters, as `kAudioUnitProperty_ParameterList` and
//! `kAudioUnitProperty_ParameterInfo` describe them (`AudioUnitProperties.h`).

use std::ffi::c_char;

use manguebyte_core::ParamId;

use super::cf;
use super::sys::*;
use super::unit::Unit;

/// Most parameters read from one unit.
pub(crate) const MAX_PARAMS: usize = 65_536;

/// What a unit says about one of its parameters, in the global scope.
///
/// Values are in the parameter's own unit and range, from
/// [`min`](Self::min) to [`max`](Self::max): what `AudioUnitGetParameter`
/// and `AudioUnitSetParameter` take.
#[derive(Debug, Clone, PartialEq)]
pub struct AuParam {
    /// The parameter's `AudioUnitParameterID`, as the number that stands
    /// for it.
    pub id: ParamId,
    /// The name shown to people.
    pub name: String,
    /// The unit's symbol, such as `dB` or `Hz`, or the name a unit of the
    /// custom kind gives; empty for a plain number.
    pub unit: String,
    /// The `AudioUnitParameterUnit` the unit gives, such as
    /// `kAudioUnitParameterUnit_Decibels` (13).
    pub unit_kind: u32,
    /// The lowest value.
    pub min: f64,
    /// The highest value.
    pub max: f64,
    /// The value it starts at.
    pub default: f64,
    /// How many steps the value moves in, or `None` for a continuous one:
    /// 1 for a switch (`kAudioUnitParameterUnit_Boolean`), the range for
    /// an indexed one (`kAudioUnitParameterUnit_Indexed`).
    pub steps: Option<u32>,
    /// The unit's `AudioUnitParameterOptions` flags, as it gives them.
    pub flags: u32,
}

impl AuParam {
    /// Whether the unit turns values into text and back itself
    /// (`kAudioUnitParameterFlag_ValuesHaveStrings`).
    pub fn has_strings(&self) -> bool {
        self.flags & kAudioUnitParameterFlag_ValuesHaveStrings != 0
    }

    /// Whether the host may change it: it is writable and not a meter.
    pub fn is_writable(&self) -> bool {
        self.flags & kAudioUnitParameterFlag_IsWritable != 0
            && self.flags & kAudioUnitParameterFlag_MeterReadOnly == 0
    }

    /// Whether it may be automated: writable, and not marked as one that
    /// must not change while the unit renders
    /// (`kAudioUnitParameterFlag_NonRealTime`).
    pub fn is_automatable(&self) -> bool {
        self.is_writable() && self.flags & kAudioUnitParameterFlag_NonRealTime == 0
    }

    /// `value` held to the range, and to whole steps where the parameter
    /// moves in them.
    pub fn hold(&self, value: f64) -> f64 {
        let value = value.clamp(self.min, self.max);
        if self.steps.is_some() {
            value.round().clamp(self.min, self.max)
        } else {
            value
        }
    }
}

/// The symbol of an `AudioUnitParameterUnit`, as a person reads it next to
/// a number.
pub(crate) fn unit_symbol(unit: u32) -> &'static str {
    match unit {
        kAudioUnitParameterUnit_Percent => "%",
        kAudioUnitParameterUnit_Seconds => "s",
        kAudioUnitParameterUnit_SampleFrames => "samples",
        kAudioUnitParameterUnit_Phase | kAudioUnitParameterUnit_Degrees => "°",
        kAudioUnitParameterUnit_Rate => "x",
        kAudioUnitParameterUnit_Hertz => "Hz",
        kAudioUnitParameterUnit_Cents | kAudioUnitParameterUnit_AbsoluteCents => "ct",
        kAudioUnitParameterUnit_RelativeSemiTones => "st",
        kAudioUnitParameterUnit_Decibels | kAudioUnitParameterUnit_Meters => "dB",
        kAudioUnitParameterUnit_Octaves => "oct",
        kAudioUnitParameterUnit_BPM => "BPM",
        kAudioUnitParameterUnit_Beats => "beats",
        kAudioUnitParameterUnit_Milliseconds => "ms",
        _ => "",
    }
}

/// The text of a C name field, which ends at its first NUL or its end.
fn c_name(field: &[c_char]) -> String {
    let bytes: Vec<u8> = field
        .iter()
        .map(|&c| c as u8)
        .take_while(|&byte| byte != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A parameter from what `ParameterInfo` gave, with its name and the name
/// of its custom unit already read.
pub(crate) fn describe(
    id: AudioUnitParameterID,
    info: &AudioUnitParameterInfo,
    name: Option<String>,
    custom_unit: Option<String>,
) -> AuParam {
    let finite = |value: f32, otherwise: f64| {
        if value.is_finite() {
            f64::from(value)
        } else {
            otherwise
        }
    };
    let low = finite(info.minValue, 0.0);
    let high = finite(info.maxValue, 1.0);
    let (min, max) = if low <= high {
        (low, high)
    } else {
        (high, low)
    };
    let default = finite(info.defaultValue, min).clamp(min, max);
    let steps = match info.unit {
        kAudioUnitParameterUnit_Boolean => Some(1),
        kAudioUnitParameterUnit_Indexed => {
            Some((max - min).round().clamp(0.0, f64::from(u32::MAX)) as u32)
        }
        _ => None,
    };
    let unit = match info.unit {
        kAudioUnitParameterUnit_CustomUnit => custom_unit.unwrap_or_default(),
        other => unit_symbol(other).to_owned(),
    };
    AuParam {
        id: ParamId(id),
        name: name.unwrap_or_else(|| c_name(&info.name)),
        unit,
        unit_kind: info.unit,
        min,
        max,
        default,
        steps,
        flags: info.flags,
    }
}

/// The unit's parameters in the global scope, in the order it lists them.
/// One whose description cannot be read is left out, and counted.
pub(crate) fn read_params(unit: &Unit) -> (Vec<AuParam>, usize) {
    let ids: Vec<u32> = unit
        .get_list(
            kAudioUnitProperty_ParameterList,
            kAudioUnitScope_Global,
            0,
            MAX_PARAMS,
        )
        .unwrap_or_default();
    let mut params = Vec::with_capacity(ids.len());
    let mut skipped = 0;
    for id in ids {
        if params.iter().any(|param: &AuParam| param.id.0 == id) {
            skipped += 1;
            continue;
        }
        let info: AudioUnitParameterInfo =
            match unit.get(kAudioUnitProperty_ParameterInfo, kAudioUnitScope_Global, id) {
                Ok(info) => info,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
        let has_name = info.flags & kAudioUnitParameterFlag_HasCFNameString != 0;
        // SAFETY: the unit gave the name as a string; it is released only
        // when the flags hand it over.
        let name = unsafe {
            if !has_name || info.cfNameString.is_null() {
                None
            } else if info.flags & kAudioUnitParameterFlag_CFNameRelease != 0 {
                cf::take_string(info.cfNameString)
            } else {
                cf::read_string(info.cfNameString)
            }
        };
        // The custom unit's name stays the unit's: the header hands over
        // only the parameter's name.
        let custom = (info.unit == kAudioUnitParameterUnit_CustomUnit)
            // SAFETY: the unit gave it as a string, or null.
            .then(|| unsafe { cf::read_string(info.unitName) })
            .flatten();
        params.push(describe(id, &info, name, custom));
    }
    (params, skipped)
}

/// The text a unit gives for `value` of a parameter
/// (`kAudioUnitProperty_ParameterStringFromValue`).
pub(crate) fn text_from_unit(unit: &Unit, id: u32, value: f32) -> Option<String> {
    let query = AudioUnitParameterStringFromValue {
        inParamID: id,
        inValue: &value,
        outString: std::ptr::null(),
    };
    let answer = unit
        .query(
            kAudioUnitProperty_ParameterStringFromValue,
            kAudioUnitScope_Global,
            0,
            query,
        )
        .ok()?;
    // SAFETY: the unit hands over a string the caller releases.
    unsafe { cf::take_string(answer.outString) }
}

/// The value a unit reads from `text` for a parameter
/// (`kAudioUnitProperty_ParameterValueFromString`).
pub(crate) fn value_from_unit(unit: &Unit, id: u32, text: &str) -> Option<f32> {
    let string = cf::string(text)?;
    let query = AudioUnitParameterValueFromString {
        inParamID: id,
        inString: string.as_ptr(),
        outValue: 0.0,
    };
    let answer = unit
        .query(
            kAudioUnitProperty_ParameterValueFromString,
            kAudioUnitScope_Global,
            0,
            query,
        )
        .ok()?;
    answer.outValue.is_finite().then_some(answer.outValue)
}

/// The text of a value where the unit gives none: the number, with the
/// unit's symbol.
pub(crate) fn plain_text(param: &AuParam, value: f64) -> String {
    let value = param.hold(value);
    let number = if param.steps.is_some() {
        format!("{}", value as i64)
    } else {
        format!("{}", value as f32)
    };
    if param.unit.is_empty() {
        number
    } else {
        format!("{number} {}", param.unit)
    }
}

/// Reads a number, with or without the unit's symbol, held to the range.
pub(crate) fn plain_value(param: &AuParam, text: &str) -> Option<f64> {
    let text = text.trim();
    let number = text
        .strip_suffix(param.unit.as_str())
        .filter(|_| !param.unit.is_empty())
        .unwrap_or(text)
        .trim();
    let value: f64 = number.parse().ok()?;
    value.is_finite().then(|| param.hold(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(unit: u32, min: f32, max: f32, default: f32, flags: u32) -> AudioUnitParameterInfo {
        let mut name = [0 as c_char; 52];
        for (slot, &byte) in name.iter_mut().zip(b"Cutoff") {
            *slot = byte as c_char;
        }
        AudioUnitParameterInfo {
            name,
            unitName: std::ptr::null(),
            clumpID: 0,
            cfNameString: std::ptr::null(),
            unit,
            minValue: min,
            maxValue: max,
            defaultValue: default,
            flags,
        }
    }

    const READ_WRITE: u32 = kAudioUnitParameterFlag_IsReadable | kAudioUnitParameterFlag_IsWritable;

    #[test]
    fn a_description_becomes_a_parameter() {
        let cutoff = describe(
            7,
            &info(
                kAudioUnitParameterUnit_Hertz,
                20.0,
                20_000.0,
                1_000.0,
                READ_WRITE,
            ),
            None,
            None,
        );
        assert_eq!(cutoff.id, ParamId(7));
        assert_eq!(cutoff.name, "Cutoff");
        assert_eq!(
            (cutoff.unit.as_str(), cutoff.min, cutoff.max, cutoff.default),
            ("Hz", 20.0, 20_000.0, 1_000.0)
        );
        assert_eq!(cutoff.steps, None);
        assert!(cutoff.is_writable() && cutoff.is_automatable() && !cutoff.has_strings());
        assert_eq!(plain_text(&cutoff, 440.0), "440 Hz");
        assert_eq!(plain_value(&cutoff, " 880 Hz"), Some(880.0));
        assert_eq!(plain_value(&cutoff, "1e9"), Some(20_000.0));
        assert_eq!(plain_value(&cutoff, "loud"), None);

        let switch = describe(
            1,
            &info(kAudioUnitParameterUnit_Boolean, 0.0, 1.0, 0.0, READ_WRITE),
            Some("Bypass".into()),
            None,
        );
        assert_eq!((switch.name.as_str(), switch.steps), ("Bypass", Some(1)));
        assert_eq!(switch.hold(0.7), 1.0);

        let mode = describe(
            2,
            &info(kAudioUnitParameterUnit_Indexed, 0.0, 3.0, 9.0, READ_WRITE),
            None,
            None,
        );
        assert_eq!((mode.steps, mode.default), (Some(3), 3.0));

        // A meter, upside down, with no finite default.
        let meter = describe(
            3,
            &info(
                kAudioUnitParameterUnit_CustomUnit,
                6.0,
                -60.0,
                f32::NAN,
                kAudioUnitParameterFlag_IsReadable | kAudioUnitParameterFlag_MeterReadOnly,
            ),
            None,
            Some("VU".into()),
        );
        assert_eq!((meter.min, meter.max, meter.default), (-60.0, 6.0, -60.0));
        assert_eq!(meter.unit, "VU");
        assert!(!meter.is_writable() && !meter.is_automatable());
        let slow = describe(
            4,
            &info(
                kAudioUnitParameterUnit_Generic,
                0.0,
                1.0,
                0.5,
                READ_WRITE | kAudioUnitParameterFlag_NonRealTime,
            ),
            None,
            None,
        );
        assert!(slow.is_writable() && !slow.is_automatable());
    }

    #[test]
    fn every_unit_has_a_symbol_or_none() {
        assert_eq!(unit_symbol(kAudioUnitParameterUnit_Decibels), "dB");
        assert_eq!(unit_symbol(kAudioUnitParameterUnit_Milliseconds), "ms");
        assert_eq!(unit_symbol(kAudioUnitParameterUnit_Generic), "");
        assert_eq!(unit_symbol(9_999), "");
    }
}
