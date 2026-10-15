//! Parameter descriptions as data: the exact JSON, its validity as checked by
//! an independent parser, and text conversions in both directions.

use manguebyte_params::{
    BoolParam, ChoiceParam, Curve, FloatParam, FloatRange, IntParam, Param, ParamDescription,
    ParamId, ParamInfo, ParamScale, ParamSet, Smoothing, param_set,
};
use serde_json::Value;

struct Synth {
    gain: FloatParam,
    cutoff: FloatParam,
    drive: FloatParam,
    tone: FloatParam,
    pan: FloatParam,
    detune: FloatParam,
    voices: IntParam,
    bypass: BoolParam,
    shape: ChoiceParam,
    length: FloatParam,
}
param_set!(Synth {
    gain,
    cutoff,
    drive,
    tone,
    pan,
    detune,
    voices,
    bypass,
    shape,
    length
});

/// One parameter of every scale, with units and names that need escaping.
fn synth() -> Synth {
    Synth {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(-60.0, 12.0), 0.0)
            .with_unit("dB")
            .with_smoothing(Smoothing::Linear(10.0)),
        cutoff: FloatParam::new(
            "cutoff",
            "Cutoff",
            FloatRange::logarithmic(20.0, 20_000.0),
            1_000.0,
        )
        .with_unit("Hz")
        .with_smoothing(Smoothing::Exponential(20.0)),
        drive: FloatParam::new(
            "drive",
            "Saturação",
            FloatRange::skewed(0.0, 10.0, 0.3),
            1.5,
        )
        .with_smoothing(Smoothing::None),
        tone: FloatParam::new(
            "tone",
            "Tone",
            FloatRange::skewed_around(20.0, 20_000.0, 1_000.0),
            1_000.0,
        )
        .with_unit("Hz")
        .hidden(),
        pan: FloatParam::new(
            "pan",
            "Pan ↔",
            FloatRange::symmetric_skewed(-1.0, 1.0, 0.5),
            0.0,
        ),
        detune: FloatParam::new("detune", "Detune", FloatRange::linear(0.0, 1.0), 0.33)
            .with_step(0.25),
        voices: IntParam::new("voices", "Voices", 1, 16, 8)
            .with_unit("voices")
            .non_automatable(),
        bypass: BoolParam::new("bypass", "Bypass", false).as_bypass(),
        shape: ChoiceParam::new(
            "shape",
            "Forma \"da onda\"",
            &["Seno", "Dente-de-serra", "Quadrada \\ 50%", "Ruído\tbranco"],
            1,
        ),
        length: FloatParam::new(
            "length",
            "Comprimento\n“polegadas”",
            FloatRange::linear(0.0, 12.5),
            2.0,
        )
        .with_unit("\""),
    }
}

const FLAGS: &str = r#""flags":{"automatable":true,"hidden":false,"bypass":false}"#;

#[test]
fn exact_json_for_every_scale() {
    let expected = [
        r#"{"id":"gain","param_id":458499838,"name":"Gain","unit":"dB","min":-60,"max":12,"default":0,"scale":{"kind":"linear"},"smoothing_ms":10,"#.to_owned() + FLAGS + "}",
        r#"{"id":"cutoff","param_id":1801646368,"name":"Cutoff","unit":"Hz","min":20,"max":20000,"default":1000,"scale":{"kind":"log"},"smoothing_ms":20,"#.to_owned() + FLAGS + "}",
        r#"{"id":"drive","param_id":531555959,"name":"Saturação","unit":"","min":0,"max":10,"default":1.5,"scale":{"kind":"skew","factor":0.3,"symmetric":false},"smoothing_ms":0,"#.to_owned() + FLAGS + "}",
        r#"{"id":"tone","param_id":1550929537,"name":"Tone","unit":"Hz","min":20,"max":20000,"default":1000,"scale":{"kind":"skew","factor":0.22990456,"symmetric":false},"flags":{"automatable":true,"hidden":true,"bypass":false}}"#.to_owned(),
        r#"{"id":"pan","param_id":1331031788,"name":"Pan ↔","unit":"","min":-1,"max":1,"default":0,"scale":{"kind":"skew","factor":0.5,"symmetric":true},"#.to_owned() + FLAGS + "}",
        r#"{"id":"detune","param_id":3143621932,"name":"Detune","unit":"","min":0,"max":1,"default":0.25,"scale":{"kind":"stepped","steps":4,"step":0.25},"#.to_owned() + FLAGS + "}",
        r#"{"id":"voices","param_id":1816020030,"name":"Voices","unit":"voices","min":1,"max":16,"default":8,"scale":{"kind":"stepped","steps":15,"step":1},"flags":{"automatable":false,"hidden":false,"bypass":false}}"#.to_owned(),
        r#"{"id":"bypass","param_id":1573592237,"name":"Bypass","unit":"","min":0,"max":1,"default":0,"scale":{"kind":"stepped","steps":1,"step":1,"names":["Off","On"]},"flags":{"automatable":true,"hidden":false,"bypass":true}}"#.to_owned(),
        r#"{"id":"shape","param_id":2646858022,"name":"Forma \"da onda\"","unit":"","min":0,"max":3,"default":1,"scale":{"kind":"stepped","steps":3,"step":1,"names":["Seno","Dente-de-serra","Quadrada \\ 50%","Ruído\tbranco"]},"#.to_owned() + FLAGS + "}",
        r#"{"id":"length","param_id":2211460629,"name":"Comprimento\n“polegadas”","unit":"\"","min":0,"max":12.5,"default":2,"scale":{"kind":"linear"},"#.to_owned() + FLAGS + "}",
    ];

    let synth = synth();
    let descriptions = synth.descriptions();
    assert_eq!(descriptions.len(), expected.len());
    for (description, expected) in descriptions.iter().zip(&expected) {
        assert_eq!(&description.to_json(), expected);
    }
    assert_eq!(
        synth.descriptions_json(),
        format!("[{}]", expected.join(","))
    );

    // The numbers above were worked out independently: the ids with a
    // separate FNV-1a, the factor as the shortest decimal of the `f32` that
    // `skewed_around` computes.
    assert_eq!(ParamId::from_name("tone").0, 1_550_929_537);
    let FloatRange::Skewed { factor, .. } = *synth.tone.range() else {
        panic!("skewed_around makes a skewed range");
    };
    assert_eq!(factor, 0.229_904_56_f32);
}

#[test]
fn descriptions_hold_the_exact_values() {
    let synth = synth();
    let tone = synth.tone.describe();
    assert_eq!(tone.id, "tone");
    assert_eq!(tone.param_id, ParamId::from_name("tone"));
    assert_eq!(
        (tone.min, tone.max, tone.default),
        (20.0, 20_000.0, 1_000.0)
    );
    assert!(tone.flags.hidden);
    assert_eq!(tone.smoothing, None);
    assert_eq!(tone.smoothing_ms(), None);

    // The shortest decimal, which reads back as the same `f32`.
    let drive = synth.drive.describe();
    assert_eq!(
        drive.scale,
        ParamScale::Skew {
            factor: 0.3,
            symmetric: false
        }
    );
    assert_eq!(drive.smoothing, Some(Smoothing::None));
    assert_eq!(drive.smoothing_ms(), Some(0.0));
    assert_eq!(synth.gain.describe().smoothing_ms(), Some(10.0));
    assert_eq!(synth.cutoff.describe().smoothing_ms(), Some(20.0));

    let shape = synth.shape.describe();
    assert_eq!(
        shape.scale,
        ParamScale::Stepped {
            steps: 3,
            step: 1.0,
            names: ["Seno", "Dente-de-serra", "Quadrada \\ 50%", "Ruído\tbranco"]
                .map(String::from)
                .to_vec(),
            curve: Curve::Linear,
            end: 3.0,
        }
    );
    let bypass = synth.bypass.describe();
    assert!(bypass.flags.bypass);
    let voices = synth.voices.describe();
    assert!(!voices.flags.automatable);

    // A whole-number range as wide as an `i32` allows.
    let wide = IntParam::new("wide", "Wide", i32::MIN, i32::MAX, 0).describe();
    assert_eq!((wide.min, wide.max), (-2_147_483_648.0, 2_147_483_647.0));
    assert_eq!(
        wide.scale,
        ParamScale::Stepped {
            steps: u32::MAX,
            step: 1.0,
            names: Vec::new(),
            curve: Curve::Linear,
            end: 2_147_483_647.0,
        }
    );
    assert!(wide.to_json().contains(r#""steps":4294967295,"#));
}

#[test]
fn descriptions_follow_declaration_order_through_nested_sets() {
    struct Inner {
        b: IntParam,
        c: BoolParam,
    }
    param_set!(Inner { b, c });
    struct Outer {
        a: FloatParam,
        inner: Inner,
        d: ChoiceParam,
    }
    param_set!(Outer { a, nested inner, d });

    let outer = Outer {
        a: FloatParam::new("a", "A", FloatRange::linear(0.0, 1.0), 0.5),
        inner: Inner {
            b: IntParam::new("b", "B", 0, 3, 1),
            c: BoolParam::new("c", "C", true),
        },
        d: ChoiceParam::new("d", "D", &["x", "y"], 0),
    };
    let ids: Vec<String> = outer.descriptions().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["a", "b", "c", "d"]);

    let json: Value = serde_json::from_str(&outer.descriptions_json()).unwrap();
    let ids: Vec<&str> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["a", "b", "c", "d"]);
}

/// Checks one JSON object, as read by serde_json, against the description
/// it was written from: every key, and nothing else.
fn assert_matches(json: &Value, description: &ParamDescription) {
    let object = json.as_object().expect("an object");
    let number = |key: &str| object[key].as_f64().unwrap_or(f64::NAN);
    let same = |key: &str, expected: f64| {
        if expected.is_finite() {
            assert_eq!(
                number(key).to_bits(),
                expected.to_bits(),
                "{key} of {:?}",
                description.id
            );
        } else {
            assert!(object[key].is_null(), "{key} of {:?}", description.id);
        }
    };
    assert_eq!(object["id"], Value::from(description.id.as_str()));
    assert_eq!(object["param_id"], Value::from(description.param_id.0));
    assert_eq!(object["name"], Value::from(description.name.as_str()));
    assert_eq!(object["unit"], Value::from(description.unit.as_str()));
    same("min", description.min);
    same("max", description.max);
    same("default", description.default);

    let scale = object["scale"].as_object().unwrap();
    match &description.scale {
        ParamScale::Linear => {
            assert_eq!(scale["kind"], "linear");
            assert_eq!(scale.len(), 1);
        }
        ParamScale::Log => {
            assert_eq!(scale["kind"], "log");
            assert_eq!(scale.len(), 1);
        }
        ParamScale::Skew { factor, symmetric } => {
            assert_eq!(scale["kind"], "skew");
            assert_eq!(
                scale["factor"].as_f64().unwrap().to_bits(),
                factor.to_bits()
            );
            assert_eq!(scale["symmetric"], *symmetric);
            assert_eq!(scale.len(), 3);
        }
        ParamScale::Stepped {
            steps,
            step,
            names,
            curve,
            end,
        } => {
            assert_eq!(scale["kind"], "stepped");
            assert_eq!(scale["steps"], *steps);
            assert_eq!(scale["step"].as_f64().unwrap().to_bits(), step.to_bits());
            let mut keys = 3;
            if !names.is_empty() {
                let read: Vec<&str> = scale["names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|name| name.as_str().unwrap())
                    .collect();
                assert_eq!(read, *names);
                keys += 1;
            }
            if *curve == Curve::Linear && end.to_bits() == description.max.to_bits() {
                assert!(!scale.contains_key("curve"));
            } else {
                let written = scale["curve"].as_object().unwrap();
                assert_eq!(read_curve(written), *curve);
                if end.is_finite() {
                    assert_eq!(written["end"].as_f64().unwrap().to_bits(), end.to_bits());
                } else {
                    assert!(written["end"].is_null());
                }
                keys += 1;
            }
            assert_eq!(scale.len(), keys);
        }
        _ => unreachable!("no other scale is made in these tests"),
    }

    match description.smoothing_ms() {
        Some(milliseconds) => same("smoothing_ms", milliseconds),
        None => assert!(!object.contains_key("smoothing_ms")),
    }
    let flags = object["flags"].as_object().unwrap();
    assert_eq!(flags["automatable"], description.flags.automatable);
    assert_eq!(flags["hidden"], description.flags.hidden);
    assert_eq!(flags["bypass"], description.flags.bypass);
    assert_eq!(flags.len(), 3);

    let keys = 9 + usize::from(description.smoothing.is_some());
    assert_eq!(object.len(), keys, "{:?}", description.id);
}

#[test]
fn json_reads_back_through_serde_json() {
    let synth = synth();
    let json: Value = serde_json::from_str(&synth.descriptions_json()).unwrap();
    let entries = json.as_array().unwrap();
    let descriptions = synth.descriptions();
    assert_eq!(entries.len(), 10);
    for (entry, description) in entries.iter().zip(&descriptions) {
        assert_matches(entry, description);
    }
}

/// A small xorshift generator, so the hostile cases are the same on every
/// run.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    /// Text drawn from characters that need escaping, characters outside
    /// ASCII and characters JSON leaves alone.
    fn text(&mut self, max_len: usize) -> String {
        const POOL: &[char] = &[
            '"',
            '\\',
            '/',
            '\n',
            '\r',
            '\t',
            '\u{0}',
            '\u{1}',
            '\u{8}',
            '\u{b}',
            '\u{c}',
            '\u{1f}',
            '\u{7f}',
            '\u{80}',
            '\u{a0}',
            '\u{2028}',
            '\u{2029}',
            '\u{feff}',
            '\u{fffd}',
            '\u{ffff}',
            '\u{10000}',
            '\u{10ffff}',
            'é',
            'ç',
            'µ',
            '°',
            '“',
            '”',
            '🎛',
            'a',
            'Z',
            '0',
            '9',
            ' ',
            '{',
            '}',
            '[',
            ']',
            ',',
            ':',
            '=',
            '-',
            '+',
            '.',
            'k',
            'm',
        ];
        let len = self.below(max_len + 1);
        (0..len).map(|_| POOL[self.below(POOL.len())]).collect()
    }

    /// A printable-ASCII identifier, quotes and backslashes included.
    fn id(&mut self) -> String {
        let len = 1 + self.below(12);
        (0..len)
            .map(|_| {
                loop {
                    let byte = b'!' + self.below(94) as u8;
                    if byte != b'=' {
                        break char::from(byte);
                    }
                }
            })
            .collect()
    }

    /// A finite `f32` from any bit pattern.
    fn finite(&mut self) -> f32 {
        loop {
            let value = f32::from_bits(self.next() as u32);
            if value.is_finite() {
                return value;
            }
        }
    }
}

#[test]
fn hostile_text_and_numbers_read_back_exactly() {
    let mut random = Random(0x9E37_79B9_7F4A_7C15);
    for round in 0..600 {
        let id = random.id();
        let name = random.text(24);
        let unit = random.text(4);
        // Each parameter with the ends it was made with.
        let (param, min, max): (Box<dyn Param>, f64, f64) = match round % 5 {
            0 => {
                let (mut a, mut b) = (random.finite(), random.finite());
                while a == b {
                    b = random.finite();
                }
                if a > b {
                    std::mem::swap(&mut a, &mut b);
                }
                let range = match random.below(3) {
                    0 => FloatRange::linear(a, b),
                    1 => FloatRange::skewed(a, b, random.finite().abs().max(1e-30)),
                    _ => FloatRange::symmetric_skewed(a, b, random.finite().abs().max(1e-30)),
                };
                let param = FloatParam::new(&id, &name, range, random.finite())
                    .with_unit(&unit)
                    .with_smoothing(Smoothing::Linear(f64::from(random.finite().abs())));
                (Box::new(param), f64::from(a), f64::from(b))
            }
            1 => {
                let a = random.finite().abs().clamp(f32::MIN_POSITIVE, 1e30);
                let b = a * (1.001 + (random.next() % 10_000) as f32);
                let param =
                    FloatParam::new(&id, &name, FloatRange::logarithmic(a, b), random.finite())
                        .with_unit(&unit);
                (Box::new(param), f64::from(a), f64::from(b))
            }
            2 => {
                let a = random.next() as i32;
                let b = a.saturating_add(1 + (random.next() % 1_000) as i32);
                let (a, b) = if a < b { (a, b) } else { (a - 1, a) };
                let param = IntParam::new(&id, &name, a, b, random.next() as i32).with_unit(&unit);
                (Box::new(param), f64::from(a), f64::from(b))
            }
            3 => {
                let param = BoolParam::new(&id, &name, round % 2 == 0)
                    .with_labels(&random.text(8), &random.text(8))
                    .with_unit(&unit);
                (Box::new(param), 0.0, 1.0)
            }
            _ => {
                let choices: Vec<String> =
                    (0..2 + random.below(6)).map(|_| random.text(10)).collect();
                let choices: Vec<&str> = choices.iter().map(String::as_str).collect();
                let param =
                    ChoiceParam::new(&id, &name, &choices, random.below(9)).with_unit(&unit);
                (Box::new(param), 0.0, (choices.len() - 1) as f64)
            }
        };
        let description = param.describe();
        let json = description.to_json();
        let value: Value =
            serde_json::from_str(&json).unwrap_or_else(|error| panic!("{error} in {json:?}"));
        assert_matches(&value, &description);

        // Every number reads back as the very `f32` the parameter holds:
        // the shortest decimal loses nothing.
        let same_f32 = |written: f64, held: f64| {
            assert_eq!(
                (written as f32).to_bits(),
                (held as f32).to_bits(),
                "{json}"
            );
        };
        same_f32(description.min, min);
        same_f32(description.max, max);
        same_f32(description.default, param.default_plain());
    }
}

struct Units {
    mode: ChoiceParam,
    hold: BoolParam,
    time: FloatParam,
    plain: FloatParam,
}
param_set!(Units {
    mode,
    hold,
    time,
    plain
});

/// Parameters whose text needs the unit handled: a choice and a switch with
/// units, and a unit outside ASCII.
fn units() -> Units {
    Units {
        mode: ChoiceParam::new("mode", "Mode", &["Mono", "Stereo"], 0).with_unit("ch"),
        hold: BoolParam::new("hold", "Hold", false).with_unit("°"),
        time: FloatParam::new("time", "Time", FloatRange::linear(0.0, 500.0), 5.0)
            .with_unit("µs")
            .with_precision(1),
        plain: FloatParam::new("plain", "Plain", FloatRange::linear(-1.0, 1.0), 0.0),
    }
}

#[test]
fn value_to_text_speaks_the_parameters_words() {
    let synth = synth();
    let text = |id: &str, plain: f64| synth.value_to_text(id, plain);
    assert_eq!(text("gain", -6.0).as_deref(), Some("-6.00 dB"));
    assert_eq!(text("gain", 99.0).as_deref(), Some("12.00 dB"));
    assert_eq!(text("gain", -0.001).as_deref(), Some("0.00 dB"));
    assert_eq!(text("cutoff", 1_000.0).as_deref(), Some("1000.00 Hz"));
    assert_eq!(text("cutoff", 0.0).as_deref(), Some("20.00 Hz"));
    assert_eq!(text("detune", 0.6).as_deref(), Some("0.50"));
    assert_eq!(text("voices", 3.4).as_deref(), Some("3 voices"));
    assert_eq!(text("voices", -1e300).as_deref(), Some("1 voices"));
    assert_eq!(text("bypass", 1.0).as_deref(), Some("On"));
    assert_eq!(text("bypass", 0.2).as_deref(), Some("Off"));
    assert_eq!(text("shape", 3.0).as_deref(), Some("Ruído\tbranco"));
    assert_eq!(text("shape", 2.6).as_deref(), Some("Ruído\tbranco"));
    assert_eq!(text("shape", 99.0).as_deref(), Some("Ruído\tbranco"));
    assert_eq!(text("length", 2.0).as_deref(), Some("2.00 \""));
    assert_eq!(text("gain", f64::NAN), None);
    assert_eq!(text("shape", f64::NAN), None);
    assert_eq!(text("missing", 1.0), None);
    assert_eq!(text("", 1.0), None);

    let units = units();
    assert_eq!(
        units.value_to_text("mode", 1.0).as_deref(),
        Some("Stereo ch")
    );
    assert_eq!(units.value_to_text("hold", 1.0).as_deref(), Some("On °"));
    assert_eq!(units.value_to_text("time", 5.0).as_deref(), Some("5.0 µs"));

    // Nothing was stored.
    assert_eq!(synth.gain.value(), 0.0);
    assert_eq!(synth.shape.index(), 1);
}

#[test]
fn text_to_value_reads_the_parameters_words() {
    let synth = synth();
    let value = |id: &str, text: &str| synth.text_to_value(id, text);
    assert_eq!(value("gain", "-6 dB"), Some(-6.0));
    assert_eq!(value("gain", " -6,5DB "), Some(-6.5));
    assert_eq!(value("gain", "1000"), Some(12.0));
    assert_eq!(value("gain", "loud"), None);
    assert_eq!(value("cutoff", "2.5 kHz"), Some(2_500.0));
    assert_eq!(value("cutoff", "2.5k"), Some(2_500.0));
    assert_eq!(value("detune", "0.6"), Some(0.5));
    assert_eq!(value("voices", "12 voices"), Some(12.0));
    assert_eq!(value("voices", "12 VOICES"), Some(12.0));
    assert_eq!(value("voices", "12.6"), Some(13.0));
    assert_eq!(value("bypass", "on"), Some(1.0));
    assert_eq!(value("bypass", "false"), Some(0.0));
    assert_eq!(value("shape", "dente-de-serra"), Some(1.0));
    assert_eq!(value("shape", "Ruído\tbranco"), Some(3.0));
    assert_eq!(value("shape", "Quadrada \\ 50%"), Some(2.0));
    assert_eq!(value("shape", "Triangle"), None);
    assert_eq!(value("length", "3.5 \""), Some(3.5));
    assert_eq!(value("missing", "1"), None);

    let units = units();
    let value = |id: &str, text: &str| units.text_to_value(id, text);
    // A choice and a switch read back their own text, unit and all.
    assert_eq!(value("mode", "Stereo ch"), Some(1.0));
    assert_eq!(value("mode", "stereo CH  "), Some(1.0));
    assert_eq!(value("mode", "Stereo"), Some(1.0));
    assert_eq!(value("mode", "ch"), None);
    assert_eq!(value("mode", "Stereo dB"), None);
    assert_eq!(value("hold", "On °"), Some(1.0));
    assert_eq!(value("hold", "Off°"), Some(0.0));
    assert_eq!(value("time", "5.0 µs"), Some(5.0));
    // The unit cannot be cut out of the middle of a character.
    assert_eq!(value("hold", "é"), None);
    assert_eq!(value("hold", "°"), None);
    assert_eq!(value("plain", "°"), None);
}

/// For every parameter and any value, the text of the value reads back as a
/// value with the same text, and that value is one the parameter can hold.
#[test]
fn text_round_trips_for_any_value() {
    let synth = synth();
    let units = units();
    let sets: [&dyn ParamSet; 2] = [&synth, &units];
    let mut random = Random(0xC0FF_EE15_BAD5_EED5);
    let mut checked = 0;
    for set in sets {
        let mut ids = Vec::new();
        set.visit(&mut |param| ids.push(param.id().to_owned()));
        for id in &ids {
            let param = set.find(id).unwrap();
            let (min, max) = (
                param.normalized_to_plain(0.0),
                param.normalized_to_plain(1.0),
            );
            for round in 0..1_500 {
                let plain = match round % 3 {
                    0 => f64::from_bits(random.next()),
                    1 => min + (max - min) * (random.next() % 100_001) as f64 / 100_000.0,
                    _ => param.normalized_to_plain((random.next() % 1_001) as f64 / 1_000.0),
                };
                let Some(text) = set.value_to_text(id, plain) else {
                    assert!(plain.is_nan(), "{id}: no text for {plain:e}");
                    continue;
                };
                let read = set
                    .text_to_value(id, &text)
                    .unwrap_or_else(|| panic!("{id}: {text:?} from {plain:e} does not read back"));
                assert_eq!(param.stored_plain(read), Some(read), "{id}: {text:?}");
                assert_eq!(
                    set.value_to_text(id, read).as_deref(),
                    Some(text.as_str()),
                    "{id}: from {plain:e}"
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 18_000, "only {checked} values checked");
}

/// Text from anywhere never panics, and anything it reads as is a value the
/// parameter can hold, inside its range.
#[test]
fn hostile_text_never_panics_and_reads_only_legal_values() {
    let synth = synth();
    let units = units();
    let sets: [&dyn ParamSet; 2] = [&synth, &units];
    let mut random = Random(0x0BAD_C0DE_0BAD_C0DE);
    let mut read_any = 0;
    for set in sets {
        let mut ids = Vec::new();
        set.visit(&mut |param| ids.push(param.id().to_owned()));
        for id in &ids {
            let param = set.find(id).unwrap();
            for _ in 0..2_000 {
                let text = random.text(12);
                if let Some(value) = set.text_to_value(id, &text) {
                    assert_eq!(param.stored_plain(value), Some(value), "{id}: {text:?}");
                    assert!(
                        value >= param.normalized_to_plain(0.0)
                            && value <= param.normalized_to_plain(1.0),
                        "{id}: {text:?} gave {value}"
                    );
                    read_any += 1;
                }
            }
        }
        assert_eq!(set.text_to_value(&random.text(12), "1"), None);
    }
    assert!(read_any > 0, "the hostile texts never formed a number");
}

/// A wrapper that does not forward `describe` gets the description worked
/// out from the other methods; one that forwards gets the exact one.
#[test]
fn wrappers_get_a_derived_description_unless_they_forward() {
    struct Wrapped<const FORWARD: bool>(FloatParam);
    impl<const FORWARD: bool> Param for Wrapped<FORWARD> {
        fn info(&self) -> &ParamInfo {
            self.0.info()
        }
        fn plain(&self) -> f64 {
            self.0.plain()
        }
        fn set_plain(&self, value: f64) {
            self.0.set_plain(value);
        }
        fn default_plain(&self) -> f64 {
            self.0.default_plain()
        }
        fn normalized_to_plain(&self, normalized: f64) -> f64 {
            self.0.normalized_to_plain(normalized)
        }
        fn plain_to_normalized(&self, plain: f64) -> f64 {
            self.0.plain_to_normalized(plain)
        }
        fn step_count(&self) -> Option<u32> {
            self.0.step_count()
        }
        fn plain_to_text(&self, plain: f64) -> String {
            self.0.plain_to_text(plain)
        }
        fn text_to_plain(&self, text: &str) -> Option<f64> {
            self.0.text_to_plain(text)
        }
        fn describe(&self) -> ParamDescription {
            if FORWARD {
                self.0.describe()
            } else {
                ParamDescription::derived(self)
            }
        }
    }

    let cutoff = || {
        FloatParam::new(
            "cutoff",
            "Cutoff",
            FloatRange::logarithmic(20.0, 20_000.0),
            1_000.0,
        )
        .with_unit("Hz")
        .with_smoothing(Smoothing::Exponential(20.0))
    };
    let forwarded = Wrapped::<true>(cutoff()).describe();
    assert_eq!(forwarded, cutoff().describe());
    assert_eq!(forwarded.scale, ParamScale::Log);

    let derived = Wrapped::<false>(cutoff()).describe();
    assert_eq!(
        (derived.min, derived.max, derived.default),
        (20.0, 20_000.0, 1_000.0)
    );
    assert_eq!(derived.scale, ParamScale::Linear);
    assert_eq!(derived.smoothing, None);
    assert_eq!(derived.unit, "Hz");

    let stepped = FloatParam::new("x", "X", FloatRange::linear(0.0, 1.0), 0.5).with_step(0.25);
    let derived = Wrapped::<false>(stepped).describe();
    assert_eq!(
        derived.scale,
        ParamScale::Stepped {
            steps: 4,
            step: 0.25,
            names: Vec::new(),
            curve: Curve::Linear,
            end: 1.0,
        }
    );
}

/// Someone else's parameter whose numbers are not finite writes `null`,
/// which is still valid JSON.
#[test]
fn a_number_that_is_not_finite_is_written_as_null() {
    struct Odd(ParamInfo);
    impl Param for Odd {
        fn info(&self) -> &ParamInfo {
            &self.0
        }
        fn plain(&self) -> f64 {
            f64::NAN
        }
        fn set_plain(&self, _: f64) {}
        fn default_plain(&self) -> f64 {
            f64::NAN
        }
        fn normalized_to_plain(&self, normalized: f64) -> f64 {
            normalized / 0.0
        }
        fn plain_to_normalized(&self, _: f64) -> f64 {
            0.0
        }
        fn step_count(&self) -> Option<u32> {
            Some(0)
        }
        fn plain_to_text(&self, _: f64) -> String {
            String::new()
        }
        fn text_to_plain(&self, _: &str) -> Option<f64> {
            None
        }
    }
    let info = BoolParam::new("odd", "Odd", false).info().clone();
    let description = Odd(info).describe();
    assert_eq!(
        description.to_json(),
        concat!(
            r#"{"id":"odd","param_id":2906692958,"name":"Odd","unit":"","#,
            r#""min":null,"max":null,"default":null,"#,
            r#""scale":{"kind":"stepped","steps":0,"step":0},"#,
            r#""flags":{"automatable":true,"hidden":false,"bypass":false}}"#,
        )
    );
    let value: Value = serde_json::from_str(&description.to_json()).unwrap();
    assert_matches(&value, &description);
}

#[test]
fn an_identifier_with_quotes_and_backslashes_is_escaped() {
    let param = BoolParam::new(r#"a"b\c"#, "Q", true);
    let json = param.describe().to_json();
    assert!(
        json.starts_with(r#"{"id":"a\"b\\c","param_id":3581114653,"#),
        "{json}"
    );
    let value: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["id"], r#"a"b\c"#);
}

#[test]
fn the_smoothing_hint_changes_nothing_but_the_description_and_the_smoother() {
    use manguebyte_params::ParamSmoother;

    let hinted = FloatParam::new("g", "G", FloatRange::linear(0.0, 1.0), 0.5)
        .with_smoothing(Smoothing::Linear(10.0));
    let plain = FloatParam::new("g", "G", FloatRange::linear(0.0, 1.0), 0.5);
    for value in [0.0, 0.25, 7.0, -1.0, f32::NAN] {
        hinted.set(value);
        plain.set(value);
        assert_eq!(hinted.value().to_bits(), plain.value().to_bits());
        assert_eq!(hinted.display(), plain.display());
        assert_eq!(hinted.normalized(), plain.normalized());
    }
    assert_eq!(plain.smoothing(), None);
    assert_eq!(
        hinted.smoother(),
        ParamSmoother::new(Smoothing::Linear(10.0))
    );
    assert_eq!(plain.smoother(), ParamSmoother::new(Smoothing::None));

    // 10 ms at 1 kHz is ten samples, in equal steps.
    let mut smoother = hinted.smoother();
    smoother.prepare(1_000.0, 0.0);
    smoother.set_target(1.0);
    let ramp: Vec<f32> = (0..10).map(|_| smoother.next_value()).collect();
    assert_eq!(ramp[0], 0.1);
    assert_eq!(ramp[9], 1.0);
}

#[test]
fn a_smoothing_time_must_be_a_real_duration() {
    for bad in [
        Smoothing::Linear(-1.0),
        Smoothing::Exponential(f64::NAN),
        Smoothing::Linear(f64::INFINITY),
    ] {
        let result = std::panic::catch_unwind(|| {
            FloatParam::new("g", "G", FloatRange::linear(0.0, 1.0), 0.5).with_smoothing(bad)
        });
        assert!(result.is_err(), "{bad:?} was accepted");
    }
    let zero = FloatParam::new("g", "G", FloatRange::linear(0.0, 1.0), 0.5)
        .with_smoothing(Smoothing::Exponential(0.0));
    assert_eq!(zero.describe().smoothing_ms(), Some(0.0));
}

/// Reads a `curve` object back, checking that it has exactly its keys.
fn read_curve(object: &serde_json::Map<String, Value>) -> Curve {
    match object["kind"].as_str().unwrap() {
        "linear" => {
            assert_eq!(object.len(), 2, "{object:?}");
            Curve::Linear
        }
        "log" => {
            assert_eq!(object.len(), 2, "{object:?}");
            Curve::Log
        }
        "skew" => {
            assert_eq!(object.len(), 4, "{object:?}");
            Curve::Skew {
                factor: object["factor"].as_f64().unwrap(),
                symmetric: object["symmetric"].as_bool().unwrap(),
            }
        }
        other => panic!("unknown curve {other:?}"),
    }
}

/// The position the JSON description gives `value`, worked out from the
/// documented formulas alone, as an editor in another language would.
fn described_position(json: &Value, value: f64) -> f64 {
    let min = json["min"].as_f64().unwrap();
    let max = json["max"].as_f64().unwrap();
    let scale = &json["scale"];
    let (curve, end) = match scale["kind"].as_str().unwrap() {
        "stepped" => match scale.get("curve") {
            Some(curve) => (curve, curve["end"].as_f64().unwrap()),
            None => (&Value::Null, max),
        },
        _ => (scale, max),
    };
    let kind = curve
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("linear");
    let factor = || curve["factor"].as_f64().unwrap();
    let linear = (value - min) / (end - min);
    match kind {
        "linear" => linear,
        "log" => (value / min).ln() / (end / min).ln(),
        "skew" if curve["symmetric"] == false => linear.powf(factor()),
        "skew" => {
            let distance = 2.0 * linear - 1.0;
            0.5 * (1.0 + distance.abs().powf(factor()).copysign(distance))
        }
        other => panic!("unknown kind {other:?}"),
    }
}

/// Whether the parameter holds `value` as it is, to the `f32` it keeps.
fn holds(param: &dyn Param, value: f64) -> bool {
    param
        .stored_plain(value)
        .is_some_and(|stored| (stored as f32).to_bits() == (value as f32).to_bits())
}

/// Stepped floats on every curve, with steps that divide the range, that
/// leave a short last step, that leave a gap below the end, that are wider
/// than the range, and that are fine.
fn stepped_floats() -> Vec<FloatParam> {
    let ranges = [
        FloatRange::linear(0.0, 1.0),
        FloatRange::linear(-60.0, 12.0),
        FloatRange::logarithmic(20.0, 20_000.0),
        FloatRange::skewed(0.0, 10.0, 0.3),
        FloatRange::skewed_around(20.0, 20_000.0, 1_000.0),
        FloatRange::symmetric_skewed(-1.0, 1.0, 0.5),
        FloatRange::symmetric_skewed(-100.0, 100.0, 2.0),
    ];
    let fractions = [0.25, 0.3, 0.4, 1.0 / 7.0, 0.75, 1.0, 1.5, 2.6, 5.0, 0.001];
    let mut params = Vec::new();
    for range in ranges {
        let span = range.max() - range.min();
        for fraction in fractions {
            let middle = range.denormalize(0.37);
            params.push(FloatParam::new("x", "X", range, middle).with_step(span * fraction));
        }
    }
    params.push(
        FloatParam::new("f", "F", FloatRange::logarithmic(20.0, 20_000.0), 0.0).with_step(10.0),
    );
    params
}

/// Every value a stepped float's description lists is one the parameter
/// holds, the last of them is `max`, each sits at the position the
/// documented formulas give, and every value the parameter can reach is
/// one of them.
#[test]
fn stepped_floats_sit_where_their_description_says() {
    let mut random = Random(0x5EED_0F57_E995_0001);
    for param in stepped_floats() {
        let description = param.describe();
        let json: Value = serde_json::from_str(&description.to_json()).unwrap();
        assert_matches(&json, &description);
        let ParamScale::Stepped { steps, step, .. } = description.scale else {
            panic!("described as {:?}", description.scale);
        };
        let what = format!("{:?} step {step}", param.range());
        let (min, max) = (description.min, description.max);
        assert!(holds(&param, max), "{what}: max {max} cannot be held");
        assert!(holds(&param, description.default), "{what}");
        assert_eq!(param.normalized_to_plain(1.0) as f32, max as f32, "{what}");

        let value = |k: u32| (min + f64::from(k) * step).min(max);
        let tolerance = 1e-5 * (f64::from(param.range().max()) - min);
        assert!(
            (value(steps) - max).abs() <= tolerance,
            "{what}: the last value"
        );
        let mut ks: Vec<u32> = (0..=steps.min(300)).collect();
        ks.extend((0..300).map(|_| (random.next() % (u64::from(steps) + 1)) as u32));
        ks.push(steps);
        for k in ks {
            let value = value(k);
            let stored = param.stored_plain(value).unwrap();
            assert!(
                (stored - value).abs() <= tolerance,
                "{what}: value {k} is {value}, the parameter holds {stored}"
            );
            let described = described_position(&json, value);
            let actual = param.plain_to_normalized(stored);
            assert!(
                (described - actual).abs() < 1e-5,
                "{what}: value {value} is described at {described}, the parameter puts it at {actual}"
            );
        }

        // Whatever a host's position gives is one of the listed values.
        for _ in 0..300 {
            let position = (random.next() % 100_001) as f64 / 100_000.0;
            let reached = param.normalized_to_plain(position);
            let k = if step > 0.0 {
                ((reached - min) / step)
                    .round()
                    .clamp(0.0, f64::from(steps)) as u32
            } else {
                0
            };
            assert!(
                (reached - value(k)).abs() <= tolerance,
                "{what}: position {position} gives {reached}, which is not listed"
            );
        }
    }
}

#[test]
fn exact_json_for_stepped_floats() {
    let json = |param: FloatParam| {
        let id = ParamId::from_name(param.id()).0;
        let text = param.describe().to_json();
        let prefix = format!(
            r#"{{"id":"{}","param_id":{id},"name":"X","unit":"","#,
            param.id()
        );
        let suffix = format!(",{FLAGS}}}");
        assert!(
            text.starts_with(&prefix) && text.ends_with(&suffix),
            "{text}"
        );
        text[prefix.len()..text.len() - suffix.len()].to_owned()
    };
    let linear = || FloatRange::linear(0.0, 1.0);

    // A step that divides a linear range: value k at k / steps.
    assert_eq!(
        json(FloatParam::new("a", "X", linear(), 0.3).with_step(0.25)),
        r#""min":0,"max":1,"default":0.25,"scale":{"kind":"stepped","steps":4,"step":0.25}"#
    );
    // A short last step: 0, 0.4, 0.8 and 1, where a linear range puts them.
    assert_eq!(
        json(FloatParam::new("b", "X", linear(), 0.0).with_step(0.4)),
        r#""min":0,"max":1,"default":0,"scale":{"kind":"stepped","steps":3,"step":0.4}"#
    );
    // A gap below the end: the highest value is 0.9, the curve ends at 1.
    assert_eq!(
        json(FloatParam::new("c", "X", linear(), 0.0).with_step(0.3)),
        concat!(
            r#""min":0,"max":0.90000004,"default":0,"#,
            r#""scale":{"kind":"stepped","steps":3,"step":0.3,"curve":{"kind":"linear","end":1}}"#
        )
    );
    // A step wider than the range: one value.
    assert_eq!(
        json(FloatParam::new("d", "X", linear(), 0.8).with_step(5.0)),
        concat!(
            r#""min":0,"max":0,"default":0,"#,
            r#""scale":{"kind":"stepped","steps":0,"step":5,"curve":{"kind":"linear","end":1}}"#
        )
    );
    // Curved ranges keep their curve.
    assert_eq!(
        json(
            FloatParam::new("e", "X", FloatRange::logarithmic(20.0, 20_000.0), 1_004.0)
                .with_step(10.0)
        ),
        concat!(
            r#""min":20,"max":20000,"default":1000,"#,
            r#""scale":{"kind":"stepped","steps":1998,"step":10,"curve":{"kind":"log","end":20000}}"#
        )
    );
    assert_eq!(
        json(FloatParam::new("f", "X", FloatRange::skewed(0.0, 10.0, 0.3), 1.5).with_step(2.5)),
        concat!(
            r#""min":0,"max":10,"default":2.5,"scale":{"kind":"stepped","steps":4,"step":2.5,"#,
            r#""curve":{"kind":"skew","factor":0.3,"symmetric":false,"end":10}}"#
        )
    );
    assert_eq!(
        json(
            FloatParam::new("g", "X", FloatRange::symmetric_skewed(-1.0, 1.0, 0.5), 0.1)
                .with_step(0.5)
        ),
        concat!(
            r#""min":-1,"max":1,"default":0,"scale":{"kind":"stepped","steps":4,"step":0.5,"#,
            r#""curve":{"kind":"skew","factor":0.5,"symmetric":true,"end":1}}"#
        )
    );
}

/// A step that does not divide the range: the description says what the
/// parameter does, which is to stop at 0.9 and put it at position 0.9.
#[test]
fn a_stepped_float_whose_step_does_not_divide_the_range_is_described_truthfully() {
    let param = FloatParam::new("x", "X", FloatRange::linear(0.0, 1.0), 0.0).with_step(0.3);
    let description = param.describe();
    assert_eq!(
        description.scale,
        ParamScale::Stepped {
            steps: 3,
            step: 0.3,
            names: Vec::new(),
            curve: Curve::Linear,
            end: 1.0,
        }
    );
    assert!(holds(&param, description.max), "{}", description.max);
    assert_eq!(description.max, 0.900_000_04);
    let json: Value = serde_json::from_str(&description.to_json()).unwrap();
    let top = description.min + 3.0 * 0.3;
    assert!((described_position(&json, top) - 0.9).abs() < 1e-6);
    assert!((param.plain_to_normalized(top) - 0.9).abs() < 1e-6);
}

/// A step wider than the range leaves the parameter one value, its
/// minimum: no steps, and a `max` it can hold.
#[test]
fn a_step_wider_than_the_range_leaves_one_value() {
    let param = FloatParam::new("x", "X", FloatRange::linear(0.0, 1.0), 0.0).with_step(5.0);
    let description = param.describe();
    let ParamScale::Stepped { steps, end, .. } = description.scale else {
        panic!("described as {:?}", description.scale);
    };
    assert_eq!((steps, end), (0, 1.0));
    assert_eq!((description.min, description.max), (0.0, 0.0));
    assert!(holds(&param, description.max));
    for position in [0.0, 0.3, 0.5, 0.99, 1.0] {
        assert_eq!(param.normalized_to_plain(position), 0.0);
    }
    let json: Value = serde_json::from_str(&description.to_json()).unwrap();
    assert_eq!(described_position(&json, 0.0), 0.0);
}

struct OneChoice {
    p: ChoiceParam,
}
param_set!(OneChoice { p });

struct OneSwitch {
    p: BoolParam,
}
param_set!(OneSwitch { p });

/// Found in review: the text of a choice is its name, a space and the unit,
/// and that text can be the name of another choice. It must read back as
/// the choice it was written for.
#[test]
fn a_choice_text_that_ends_in_the_unit_reads_back_as_itself() {
    let set = OneChoice {
        p: ChoiceParam::new("c", "C", &["A", "A dB", "A dB dB"], 0).with_unit("dB"),
    };
    for index in [0.0, 1.0, 2.0] {
        let text = set.value_to_text("c", index).unwrap();
        assert_eq!(set.text_to_value("c", &text), Some(index), "{text:?}");
    }
    assert_eq!(set.value_to_text("c", 0.0).as_deref(), Some("A dB"));
    // What a person types still reads.
    assert_eq!(set.text_to_value("c", "a"), Some(0.0));
    assert_eq!(set.text_to_value("c", "a db  "), Some(0.0));
    assert_eq!(set.text_to_value("c", "A dBdB"), Some(1.0));
}

/// Found in review: names with outer white space did not read back.
#[test]
fn a_choice_name_with_outer_spaces_reads_back() {
    let set = OneChoice {
        p: ChoiceParam::new("c", "C", &["  pad", "x ", "x", " x"], 0),
    };
    for index in [0.0, 1.0, 2.0, 3.0] {
        let text = set.value_to_text("c", index).unwrap();
        assert_eq!(set.text_to_value("c", &text), Some(index), "{text:?}");
    }
    // A person need not type the spaces.
    assert_eq!(set.text_to_value("c", "pad"), Some(0.0));
    assert_eq!(set.text_to_value("c", " PAD "), Some(0.0));
}

/// Found in review: names that differ only in ASCII case read back as the
/// first of them.
#[test]
fn choice_and_switch_names_that_differ_in_case_read_back_as_themselves() {
    let set = OneChoice {
        p: ChoiceParam::new("c", "C", &["saw", "Saw", "SAW"], 0).with_unit("Hz"),
    };
    for index in [0.0, 1.0, 2.0] {
        let text = set.value_to_text("c", index).unwrap();
        assert_eq!(set.text_to_value("c", &text), Some(index), "{text:?}");
    }
    assert_eq!(set.text_to_value("c", "sAw"), Some(0.0));

    let switch = OneSwitch {
        p: BoolParam::new("b", "B", false).with_labels("mute", "MUTE"),
    };
    assert_eq!(switch.text_to_value("b", "MUTE"), Some(1.0));
    assert_eq!(switch.text_to_value("b", "mute"), Some(0.0));
    assert_eq!(switch.text_to_value("b", "Mute"), Some(0.0));
    assert_eq!(switch.text_to_value("b", "on"), Some(1.0));
}

/// Hostile names and units, for every parameter type: the text of any
/// value reads back as a value the parameter holds with the same text,
/// and a choice or a switch reads back as the very option it was written
/// for, unless an earlier one has exactly the same name.
#[test]
fn text_round_trips_for_hostile_names_and_units() {
    let mut random = Random(0xA5A5_0123_4567_89AB);
    let mut checked = 0;
    for round in 0..900 {
        let unit = if round % 4 == 0 {
            String::new()
        } else {
            random.text(4)
        };
        let (param, names): (Box<dyn Param>, Vec<String>) = match round % 4 {
            0 | 1 => {
                let names: Vec<String> = (0..2 + random.below(6)).map(|_| random.text(6)).collect();
                let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
                let param = ChoiceParam::new("c", "C", &borrowed, 0).with_unit(&unit);
                (Box::new(param), names)
            }
            2 => {
                let names = vec![random.text(5), random.text(5)];
                let param = BoolParam::new("b", "B", false)
                    .with_labels(&names[0], &names[1])
                    .with_unit(&unit);
                (Box::new(param), names)
            }
            _ => {
                let min = (random.next() % 2_000) as i32 - 1_000;
                let param = IntParam::new("i", "I", min, min + 1 + random.below(500) as i32, min)
                    .with_unit(&unit);
                (Box::new(param), Vec::new())
            }
        };
        let (low, high) = (
            param.normalized_to_plain(0.0),
            param.normalized_to_plain(1.0),
        );
        for plain in [low, high, (low + high) / 2.0, low - 1.0, high + 7.0] {
            let plain = param.stored_plain(plain).unwrap();
            let text = param.display_plain(plain);
            let read = param
                .parse_display(&text)
                .unwrap_or_else(|| panic!("{text:?} (names {names:?}) does not read back"));
            if names.is_empty() {
                assert_eq!(read, plain, "{text:?}");
            } else {
                let first = names
                    .iter()
                    .position(|name| *name == names[plain as usize])
                    .unwrap();
                assert_eq!(
                    read, first as f64,
                    "{text:?} in {names:?} with unit {unit:?}"
                );
            }
            assert_eq!(param.display_plain(read), text);
            checked += 1;
        }
    }
    assert_eq!(checked, 4_500);
}

/// Every parameter type takes a smoothing hint, which shows in its
/// description and changes nothing else.
#[test]
fn every_parameter_type_takes_a_smoothing_hint() {
    let hinted: [Box<dyn Param>; 4] = [
        Box::new(
            FloatParam::new("a", "A", FloatRange::linear(0.0, 1.0), 0.5)
                .with_smoothing(Smoothing::Linear(1.5)),
        ),
        Box::new(IntParam::new("a", "A", -24, 24, 0).with_smoothing(Smoothing::Exponential(50.0))),
        Box::new(BoolParam::new("a", "A", false).with_smoothing(Smoothing::Linear(10.0))),
        Box::new(ChoiceParam::new("a", "A", &["x", "y"], 0).with_smoothing(Smoothing::None)),
    ];
    let plain: [Box<dyn Param>; 4] = [
        Box::new(FloatParam::new("a", "A", FloatRange::linear(0.0, 1.0), 0.5)),
        Box::new(IntParam::new("a", "A", -24, 24, 0)),
        Box::new(BoolParam::new("a", "A", false)),
        Box::new(ChoiceParam::new("a", "A", &["x", "y"], 0)),
    ];
    let expected = [Some(1.5), Some(50.0), Some(10.0), Some(0.0)];
    for ((hinted, plain), expected) in hinted.iter().zip(&plain).zip(expected) {
        let with = hinted.describe();
        let without = plain.describe();
        assert_eq!(with.smoothing_ms(), expected);
        assert_eq!(without.smoothing, None);
        let ms = format!(r#","smoothing_ms":{}"#, expected.unwrap());
        assert_eq!(
            with.to_json(),
            without
                .to_json()
                .replace(r#","flags""#, &format!(r#"{ms},"flags""#))
        );
        for value in [-100.0, -0.3, 0.0, 0.49, 0.5, 0.7, 1.0, 13.6, 1e9, f64::NAN] {
            hinted.set_plain(value);
            plain.set_plain(value);
            assert_eq!(hinted.plain().to_bits(), plain.plain().to_bits());
            assert_eq!(hinted.normalized().to_bits(), plain.normalized().to_bits());
            assert_eq!(hinted.display(), plain.display());
        }
    }
    let int = IntParam::new("a", "A", 0, 1, 0);
    assert_eq!(int.smoothing(), None);
    let int = int.with_smoothing(Smoothing::Linear(2.0));
    assert_eq!(int.smoothing(), Some(Smoothing::Linear(2.0)));
    assert_eq!(BoolParam::new("a", "A", true).smoothing(), None);
    assert_eq!(ChoiceParam::new("a", "A", &["x", "y"], 0).smoothing(), None);
}

#[test]
fn a_smoothing_time_must_be_a_real_duration_for_every_type() {
    for bad in [
        Smoothing::Linear(-1.0),
        Smoothing::Exponential(f64::NAN),
        Smoothing::Linear(f64::INFINITY),
    ] {
        let int = std::panic::catch_unwind(|| IntParam::new("a", "A", 0, 1, 0).with_smoothing(bad));
        let switch =
            std::panic::catch_unwind(|| BoolParam::new("a", "A", true).with_smoothing(bad));
        let choice = std::panic::catch_unwind(|| {
            ChoiceParam::new("a", "A", &["x", "y"], 0).with_smoothing(bad)
        });
        assert!(
            int.is_err() && switch.is_err() && choice.is_err(),
            "{bad:?}"
        );
    }
}
