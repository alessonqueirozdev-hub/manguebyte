use super::*;

/// The formula every voice used before tunings existed.
fn equal_tempered(note: f64) -> f64 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}

/// The cents of `key` above `from` in `tuning`.
fn cents_between(tuning: &Tuning, from: u8, key: u8) -> f64 {
    100.0 * (tuning.key_pitch(key) - tuning.key_pitch(from))
}

fn temperament(temperament: Temperament, key: u8) -> Tuning {
    TuningSettings {
        scale: Scale::Temperament { temperament, key },
        ..TuningSettings::default()
    }
    .build()
    .unwrap()
}

fn scala(scl: &str, kbm: Option<&str>) -> Tuning {
    TuningSettings {
        scale: Scale::Scala {
            scale: ScalaScale::parse(scl).unwrap(),
            mapping: kbm.map(|text| KeyboardMapping::parse(text).unwrap()),
        },
        ..TuningSettings::default()
    }
    .build()
    .unwrap()
}

/// Notes from -60 to 200 in steps of 1/1024, which hit every key and many
/// fractions exactly, and some that do not divide evenly.
fn notes() -> impl Iterator<Item = f64> {
    (-60 * 1_024..=200 * 1_024)
        .map(|step| f64::from(step) / 1_024.0)
        .chain((0..20_000).map(|step| -60.0 + f64::from(step) * 0.013_1))
}

#[test]
fn the_default_is_the_equal_tempered_formula_bit_for_bit() {
    let standard = Tuning::default();
    for note in notes().chain([
        0.0,
        -0.0,
        127.0,
        1e6,
        -1e6,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ]) {
        assert_eq!(
            standard.frequency(note).to_bits(),
            equal_tempered(note).to_bits(),
            "{note}"
        );
        assert_eq!(standard.pitch(note).to_bits(), (note + 0.0).to_bits());
    }
    assert!(standard.frequency(f64::NAN).is_nan());
    for key in 0..=127 {
        assert_eq!(standard.key_frequency(key), equal_tempered(f64::from(key)));
        assert_eq!(standard.key_pitch(key), f64::from(key));
        assert!(standard.is_mapped(key));
    }
    assert_eq!(standard.mapped_keys(), 128);
    // Built from settings, from equal temperament as a temperament, and
    // from a Scala file of 100-cent steps, it is the same table exactly.
    assert_eq!(TuningSettings::default().build().unwrap(), standard);
    assert_eq!(temperament(Temperament::equal(), 0), standard);
    assert_eq!(temperament(Temperament::equal(), 7), standard);
    let steps: String = (1..=12).map(|step| format!("{}.0\n", step * 100)).collect();
    assert_eq!(scala(&format!("12-TET\n12\n{steps}"), None), standard);
}

#[test]
fn equal_divisions_are_exact() {
    for divisions in [5, 12, 19, 22, 24, 31, 41, 53, 72] {
        let tuning = TuningSettings {
            scale: Scale::equal(divisions),
            ..TuningSettings::default()
        }
        .build()
        .unwrap();
        assert!((tuning.key_frequency(69) - 440.0).abs() < 1e-12);
        let step = 2f64.powf(1.0 / f64::from(divisions));
        for key in 1..=127 {
            let ratio = tuning.key_frequency(key) / tuning.key_frequency(key - 1);
            assert!((ratio / step - 1.0).abs() < 1e-13, "{divisions}: {key}");
        }
        let octave = 69 + divisions as u8;
        if octave <= 127 {
            assert!((tuning.key_frequency(octave) / 880.0 - 1.0).abs() < 1e-14);
        }
    }

    // Bohlen-Pierce: 13 steps to the tritave, 3/1.
    let tritave = 1_200.0 * 3f64.log2();
    let tuning = TuningSettings {
        scale: Scale::Equal {
            divisions: 13,
            period_cents: tritave,
        },
        reference_key: 60,
        reference_hz: 220.0,
        ..TuningSettings::default()
    }
    .build()
    .unwrap();
    assert!((tuning.key_frequency(60) / 220.0 - 1.0).abs() < 1e-14);
    assert!((tuning.key_frequency(73) / 660.0 - 1.0).abs() < 1e-14);
    assert!((tuning.key_frequency(47) / (220.0 / 3.0) - 1.0).abs() < 1e-14);
}

#[test]
fn reference_transpose_and_detune() {
    let build = |settings: TuningSettings| settings.build().unwrap();
    let standard = Tuning::default();

    // Any key at any frequency.
    let tuning = build(TuningSettings {
        reference_key: 60,
        reference_hz: 256.0,
        ..TuningSettings::default()
    });
    assert!((tuning.key_frequency(60) / 256.0 - 1.0).abs() < 1e-14);
    assert!((tuning.key_frequency(72) / 512.0 - 1.0).abs() < 1e-14);

    // Two keys up: the keyboard plays what is two keys higher.
    let tuning = build(TuningSettings {
        transpose: 2,
        ..TuningSettings::default()
    });
    for key in 0..=125 {
        assert_eq!(tuning.key_frequency(key), standard.key_frequency(key + 2));
    }
    // With a temperament the scale moves with the keys.
    let werckmeister = temperament(Temperament::werckmeister_iii(), 0);
    let moved = build(TuningSettings {
        scale: Scale::Temperament {
            temperament: Temperament::werckmeister_iii(),
            key: 0,
        },
        transpose: -12,
        ..TuningSettings::default()
    });
    for key in 12..=127 {
        assert!((moved.key_pitch(key) - werckmeister.key_pitch(key - 12)).abs() < 1e-12);
    }

    // Detune in cents, on every key.
    let tuning = build(TuningSettings {
        detune_cents: -15.5,
        ..TuningSettings::default()
    });
    for key in 0..=127 {
        assert!((tuning.key_cents_from_standard(key) + 15.5).abs() < 1e-9);
    }

    // Settings that make no tuning.
    let fails = |settings: TuningSettings| settings.build().unwrap_err();
    for reference_hz in [0.0, -440.0, f64::NAN, f64::INFINITY] {
        assert_eq!(
            fails(TuningSettings {
                reference_hz,
                ..TuningSettings::default()
            }),
            TuningError::Reference
        );
    }
    assert_eq!(
        fails(TuningSettings {
            reference_key: 128,
            ..TuningSettings::default()
        }),
        TuningError::Reference
    );
    assert_eq!(
        fails(TuningSettings {
            scale: Scale::equal(0),
            ..TuningSettings::default()
        }),
        TuningError::Division
    );
    assert_eq!(
        fails(TuningSettings {
            scale: Scale::Equal {
                divisions: 12,
                period_cents: -1_200.0
            },
            ..TuningSettings::default()
        }),
        TuningError::Division
    );
    assert_eq!(
        fails(TuningSettings {
            scale: Scale::Temperament {
                temperament: Temperament::equal(),
                key: 12
            },
            ..TuningSettings::default()
        }),
        TuningError::Key
    );
    assert!(matches!(
        fails(TuningSettings {
            detune_cents: 1e9,
            ..TuningSettings::default()
        }),
        TuningError::NotFinite { .. }
    ));
}

/// The cents above C of each temperament, as published, to three decimals:
/// in the articles on each temperament in J. Murray Barbour's *Tuning and
/// Temperament* (1951) and in the standard tables that follow it (for
/// example the Wikipedia articles on each).
const PUBLISHED: [(&str, [f64; 12]); 7] = [
    (
        "Pythagorean",
        [
            0.0, 113.685, 203.910, 294.135, 407.820, 498.045, 611.730, 701.955, 815.640, 905.865,
            996.090, 1109.775,
        ],
    ),
    (
        "quarter-comma meantone",
        [
            0.0, 76.049, 193.157, 310.265, 386.314, 503.422, 579.471, 696.578, 772.627, 889.735,
            1006.843, 1082.892,
        ],
    ),
    (
        "Werckmeister III",
        [
            0.0, 90.225, 192.180, 294.135, 390.225, 498.045, 588.270, 696.090, 792.180, 888.270,
            996.090, 1092.180,
        ],
    ),
    (
        "Kirnberger III",
        [
            0.0, 90.225, 193.157, 294.135, 386.314, 498.045, 590.224, 696.578, 792.180, 889.735,
            996.090, 1088.269,
        ],
    ),
    (
        "Vallotti",
        [
            0.0, 94.135, 196.090, 298.045, 392.180, 501.955, 592.180, 698.045, 796.090, 894.135,
            1000.000, 1090.225,
        ],
    ),
    (
        "Young",
        [
            0.0, 90.225, 196.090, 294.135, 392.180, 498.045, 588.270, 698.045, 792.180, 894.135,
            996.090, 1090.225,
        ],
    ),
    (
        "just",
        [
            0.0, 111.731, 203.910, 315.641, 386.314, 498.045, 590.224, 701.955, 813.686, 884.359,
            1017.596, 1088.269,
        ],
    ),
];

fn temperaments() -> [Temperament; 7] {
    [
        Temperament::pythagorean(),
        Temperament::quarter_comma_meantone(),
        Temperament::werckmeister_iii(),
        Temperament::kirnberger_iii(),
        Temperament::vallotti(),
        Temperament::young(),
        Temperament::just(),
    ]
}

#[test]
fn every_temperament_matches_its_published_cents() {
    for (temperament, (name, published)) in temperaments().into_iter().zip(PUBLISHED) {
        for (degree, (&cents, &expected)) in temperament.cents().iter().zip(&published).enumerate()
        {
            // Published to three decimals, so within half a thousandth.
            assert!(
                (cents - expected).abs() <= 0.000_5 + 1e-9,
                "{name}, degree {degree}: {cents:.6}, published {expected}"
            );
        }
        // And in a tuning on C, with A4 at 440 Hz, every octave.
        let tuning = temperament_tuning(temperament, 0);
        for octave in 0..10 {
            let c = 12 * octave;
            for degree in 0..12 {
                let cents = cents_between(&tuning, c, c + degree);
                assert!(
                    (cents - published[usize::from(degree)]).abs() < 0.01,
                    "{name}, C{octave} + {degree}"
                );
            }
        }
        assert!((tuning.key_frequency(69) - 440.0).abs() < 1e-12);
    }

    // The definitions' landmarks: quarter-comma meantone's pure major
    // thirds and Pythagorean's pure fifths.
    let meantone = Temperament::quarter_comma_meantone().cents();
    assert!((meantone[4] - 1_200.0 * 1.25f64.log2()).abs() < 1e-9);
    let pythagorean = Temperament::pythagorean().cents();
    assert!((pythagorean[7] - 1_200.0 * 1.5f64.log2()).abs() < 1e-9);
}

fn temperament_tuning(temperament: Temperament, key: u8) -> Tuning {
    self::temperament(temperament, key)
}

#[test]
fn a_temperament_is_centred_on_its_key() {
    for (temperament, (name, published)) in temperaments().into_iter().zip(PUBLISHED) {
        for key in 0..12_u8 {
            let tuning = temperament_tuning(temperament, key);
            // The reference stays where it is.
            assert!(
                (tuning.key_frequency(69) - 440.0).abs() < 1e-12,
                "{name} on {key}"
            );
            // And the scale starts from the key's pitch class.
            let root = 60 + key;
            for degree in 0..12 {
                let cents = cents_between(&tuning, root, root + degree);
                assert!(
                    (cents - published[usize::from(degree)]).abs() < 0.01,
                    "{name} on {key}, degree {degree}"
                );
            }
        }
    }

    // Just intonation on A with A4 at 440 Hz: E5 a pure fifth above.
    let just_a = temperament_tuning(Temperament::just(), 9);
    assert!((just_a.key_frequency(76) - 660.0).abs() < 1e-9);
    assert!((just_a.key_frequency(73) - 550.0).abs() < 1e-9);
}

#[test]
fn temperaments_from_tables() {
    let werckmeister = Temperament::werckmeister_iii();
    let back = Temperament::from_deviations(werckmeister.deviations());
    for (a, b) in back.cents().iter().zip(werckmeister.cents()) {
        assert!((a - b).abs() < 1e-12);
    }
    let shifted = Temperament::from_cents(werckmeister.cents().map(|cents| cents + 50.0));
    for (a, b) in shifted.cents().iter().zip(werckmeister.cents()) {
        assert!((a - b).abs() < 1e-12);
    }
    assert_eq!(Temperament::default(), Temperament::equal());
    assert_eq!(Temperament::equal().deviations(), [0.0; 12]);
}

#[test]
fn stretch_widens_octaves_from_the_pivot() {
    let stretch = Stretch {
        bass_cents_per_octave: 2.0,
        treble_cents_per_octave: 3.0,
        pivot: 69,
    };
    let tuning = TuningSettings {
        stretch,
        ..TuningSettings::default()
    }
    .build()
    .unwrap();
    assert_eq!(tuning.key_frequency(69), 440.0);
    assert!((tuning.key_cents_from_standard(81) - 3.0).abs() < 1e-9);
    assert!((tuning.key_cents_from_standard(93) - 6.0).abs() < 1e-9);
    assert!((tuning.key_cents_from_standard(57) + 2.0).abs() < 1e-9);
    assert!((tuning.key_cents_from_standard(75) - 1.5).abs() < 1e-9);
    // Every octave above the pivot is wider by 3 cents, below by 2.
    for key in 69..=115 {
        assert!((cents_between(&tuning, key, key + 12) - 1_203.0).abs() < 1e-9);
    }
    for key in 12..=57 {
        assert!((cents_between(&tuning, key, key + 12) - 1_202.0).abs() < 1e-9);
    }
    // On top of a temperament, the same amounts.
    let werckmeister = temperament_tuning(Temperament::werckmeister_iii(), 0);
    let stretched = werckmeister.stretched(&stretch).unwrap();
    for key in 0..=127 {
        let moved = 100.0 * (stretched.key_pitch(key) - werckmeister.key_pitch(key));
        assert!((moved - stretch.cents(f64::from(key))).abs() < 1e-9);
    }
    assert!(Stretch::NONE.is_none() && !stretch.is_none());
    assert_eq!(Stretch::default(), Stretch::NONE);
    assert_eq!(
        Tuning::default().stretched(&Stretch::NONE).unwrap(),
        Tuning::default()
    );
}

/// The tunings the continuity and order tests run through.
fn assorted() -> Vec<(&'static str, Tuning)> {
    let stretch = Stretch {
        bass_cents_per_octave: 4.0,
        treble_cents_per_octave: 6.0,
        pivot: 60,
    };
    vec![
        ("12-TET", Tuning::default()),
        (
            "19-TET",
            TuningSettings {
                scale: Scale::equal(19),
                ..TuningSettings::default()
            }
            .build()
            .unwrap(),
        ),
        (
            "Werckmeister III",
            temperament_tuning(Temperament::werckmeister_iii(), 0),
        ),
        (
            "quarter-comma meantone on D",
            temperament_tuning(Temperament::quarter_comma_meantone(), 2),
        ),
        ("just on G", temperament_tuning(Temperament::just(), 7)),
        (
            "stretched Young",
            temperament_tuning(Temperament::young(), 0)
                .stretched(&stretch)
                .unwrap(),
        ),
        ("Bohlen-Pierce", scala(BOHLEN_PIERCE, None)),
        ("white keys", scala(DIATONIC, Some(WHITE_KEYS))),
    ]
}

#[test]
fn fractional_notes_rise_steadily_and_without_jumps() {
    for (name, tuning) in assorted() {
        let mut previous = tuning.frequency(-30.0);
        for step in 1..=200_000 {
            let note = -30.0 + f64::from(step) * 0.001;
            let frequency = tuning.frequency(note);
            assert!(frequency > previous, "{name}: falls at {note}");
            // A thousandth of a key never moves more than a thousandth of
            // the widest step a key can take here (under 250 cents).
            let cents = 1_200.0 * (frequency / previous).log2();
            assert!(cents < 0.25, "{name}: jumps {cents} cents at {note}");
            previous = frequency;
        }
        // Whole notes give the table, and the two sides of a key meet.
        for key in 0..=127_u8 {
            let at = f64::from(key);
            assert_eq!(tuning.frequency(at), tuning.key_frequency(key), "{name}");
            for side in [at - 1e-9, at + 1e-9] {
                let cents = 1_200.0 * (tuning.frequency(side) / tuning.key_frequency(key)).log2();
                assert!(cents.abs() < 1e-6, "{name}: {cents} at {side}");
            }
        }
    }
}

const DIATONIC: &str = "! A just major scale, written several ways.
!
Just major
 7
9/8
5/4   a pure major third
4/3
701.955
5/3
15/8
2
";

/// Only the white keys, the scale's degree 0 on C4, A4 at 440 Hz.
const WHITE_KEYS: &str = "! white.kbm
!
! map size
12
! first and last keys to retune
0
127
! the middle key, the reference key and its frequency
60
69
440.0
! the formal octave
7
! the mapping
0
x
1
x
2
3
x
4
x
5
x
6
";

const BOHLEN_PIERCE: &str = "! bp.scl
!
Bohlen-Pierce, just, on the tritave
13
!
27/25
25/21
9/7
7/5
75/49
5/3
9/5
49/25
15/7
7/3
63/25
25/9
3/1
";

#[test]
fn scala_scales_with_comments_cents_and_ratios() {
    let scale = ScalaScale::parse(DIATONIC).unwrap();
    assert_eq!(scale.description(), "Just major");
    assert_eq!(scale.notes(), 7);
    let ratio = |n: f64, d: f64| 1_200.0 * (n / d).log2();
    let expected = [
        ratio(9.0, 8.0),
        ratio(5.0, 4.0),
        ratio(4.0, 3.0),
        701.955,
        ratio(5.0, 3.0),
        ratio(15.0, 8.0),
        1_200.0,
    ];
    for (cents, expected) in scale.cents().iter().zip(expected) {
        assert!((cents - expected).abs() < 1e-9);
    }
    assert_eq!(scale.period_cents(), 1_200.0);
    assert_eq!(scale.degree_cents(7), 1_200.0);
    assert!((scale.degree_cents(-1) - (ratio(15.0, 8.0) - 1_200.0)).abs() < 1e-9);
    assert!((scale.degree_cents(9) - (1_200.0 + ratio(5.0, 4.0))).abs() < 1e-9);

    // Negative cents, a blank description, Windows line endings, spaces
    // before the values and blank lines between them.
    let odd =
        ScalaScale::parse("!odd\r\n\r\n 4\r\n -5.0\r\n\r\n  +250.\r\n.5 half a cent\r\n 3/2 \r\n")
            .unwrap();
    assert_eq!(odd.description(), "");
    assert_eq!(odd.cents()[..3], [-5.0, 250.0, 0.5]);
    assert!((odd.period_cents() - ratio(3.0, 2.0)).abs() < 1e-12);

    // Large ratios keep their precision.
    let big = ScalaScale::parse("big\n1\n3486784401/2147483648\n").unwrap();
    assert!((big.period_cents() - 1_200.0 * (3f64.powi(20) / 2f64.powi(31)).log2()).abs() < 1e-9);
}

#[test]
fn a_scala_tuning_round_trips_to_the_table() {
    // Without a mapping: degree 0 on key 60, A4 (degree 9 of a 12-note
    // scale) at 440 Hz. A Werckmeister III written as a Scala file, to six
    // decimals, gives Werckmeister III.
    let cents = Temperament::werckmeister_iii().cents();
    let mut text = String::from("Werckmeister III\n12\n");
    for degree in &cents[1..] {
        text.push_str(&format!("{degree:.6}\n"));
    }
    text.push_str("2/1\n");
    let from_file = scala(&text, None);
    let direct = temperament_tuning(Temperament::werckmeister_iii(), 0);
    for key in 0..=127 {
        assert!((from_file.key_pitch(key) - direct.key_pitch(key)).abs() < 1e-8);
    }

    // The white-key mapping of a seven-note scale.
    let white = scala(DIATONIC, Some(WHITE_KEYS));
    let scale = ScalaScale::parse(DIATONIC).unwrap();
    assert_eq!(white.mapped_keys(), 75);
    assert!((white.key_frequency(69) - 440.0).abs() < 1e-12);
    let white_keys = [0, 2, 4, 5, 7, 9, 11];
    let a4_cents = scale.degree_cents(5);
    for key in 0..=127_u8 {
        let offset = i64::from(key) - 60;
        let within = offset.rem_euclid(12) as usize;
        match white_keys.iter().position(|&step| step == within) {
            Some(degree) => {
                assert!(white.is_mapped(key));
                let degree = offset.div_euclid(12) * 7 + degree as i64;
                let expected = 69.0 + (scale.degree_cents(degree) - a4_cents) / 100.0;
                assert!((white.key_pitch(key) - expected).abs() < 1e-12, "{key}");
            }
            None => {
                assert!(!white.is_mapped(key));
                // Halfway between its neighbours, which are both mapped.
                let between = (white.key_pitch(key - 1) + white.key_pitch(key + 1)) / 2.0;
                assert!((white.key_pitch(key) - between).abs() < 1e-12);
            }
        }
    }

    // A non-octave scale: 13 degrees to 3/1.
    let bp = scala(BOHLEN_PIERCE, None);
    for key in 0..=114 {
        let ratio = bp.key_frequency(key + 13) / bp.key_frequency(key);
        assert!((ratio - 3.0).abs() < 1e-12);
    }
    assert!((bp.key_frequency(60 + 6) / bp.key_frequency(60) - 5.0 / 3.0).abs() < 1e-12);
    // Degree 9 above key 60 is 440 Hz.
    assert!((bp.key_frequency(69) - 440.0).abs() < 1e-12);
}

#[test]
fn keyboard_mappings_with_unmapped_keys_ranges_and_a_formal_octave() {
    // A pattern of 5 keys over a 12-note scale, its formal octave degree 7
    // (a fifth), with the reference key not on the middle key, keys from
    // 36 to 96 retuned and the last two entries left out (unmapped).
    let kbm = "5\n36\n96\n60\n62\n300.0\n7\n0\n2\n4\n";
    let mapping = KeyboardMapping::parse(kbm).unwrap();
    assert_eq!(mapping.size(), 5);
    assert_eq!(mapping.retuned_keys(), 36..=96);
    assert_eq!(
        (
            mapping.middle_key(),
            mapping.reference_key(),
            mapping.octave_degree()
        ),
        (60, 62, 7)
    );
    assert_eq!(mapping.reference_hz(), 300.0);
    assert_eq!(mapping.degree(60), Some(0));
    assert_eq!(mapping.degree(62), Some(4));
    assert_eq!(mapping.degree(63), None);
    assert_eq!(mapping.degree(65), Some(7));
    assert_eq!(mapping.degree(56), Some(-5));

    let steps: String = (1..=12).map(|step| format!("{}.0\n", step * 100)).collect();
    let tuning = scala(&format!("12-TET\n12\n{steps}"), Some(kbm));
    assert!((tuning.key_frequency(62) - 300.0).abs() < 1e-12);
    // Key 65 is one pattern up: the formal octave, 700 cents, above key 60,
    // which is 400 cents below the reference.
    assert!((cents_between(&tuning, 60, 65) - 700.0).abs() < 1e-9);
    assert!((cents_between(&tuning, 60, 62) - 400.0).abs() < 1e-9);
    assert!(!tuning.is_mapped(63) && !tuning.is_mapped(64) && tuning.is_mapped(65));
    // Outside the retuned range, plain equal temperament at A4 = 440 Hz.
    for key in (0..36).chain(97..=127) {
        assert!(tuning.is_mapped(key));
        assert_eq!(tuning.key_pitch(key), f64::from(key));
    }

    // A linear mapping of a size of 0 maps every key to the next degree.
    let linear = "0\n0\n127\n60\n69\n440\n12\n";
    let mapping = KeyboardMapping::parse(linear).unwrap();
    assert_eq!(mapping.size(), 0);
    assert_eq!(mapping.degree(-5), Some(-65));
    let equal = scala(&format!("12-TET\n12\n{steps}"), Some(linear));
    assert_eq!(equal, Tuning::default());
}

#[test]
fn bad_scala_files_say_which_line() {
    let error = |text: &str| ScalaScale::parse(text).unwrap_err();
    let at = |line, kind| ScalaError { line, kind };
    assert_eq!(error(""), at(1, ScalaErrorKind::Missing("description")));
    assert_eq!(
        error("! only\n! comments\n"),
        at(3, ScalaErrorKind::Missing("description"))
    );
    assert_eq!(
        error("name\n"),
        at(2, ScalaErrorKind::Missing("note count"))
    );
    assert_eq!(
        error("name\ntwelve\n"),
        at(2, ScalaErrorKind::BadCount("twelve".into()))
    );
    assert_eq!(error("name\n0\n"), at(2, ScalaErrorKind::NoNotes));
    assert_eq!(
        error("name\n2\n! a comment\n100.0\nabc\n"),
        at(5, ScalaErrorKind::BadPitch("abc".into()))
    );
    for bad in [
        "1.2.3", "-", "1e3.", "3/x", "/2", "3/", "1,5", "inf.", "--5.0",
    ] {
        assert_eq!(
            error(&format!("name\n1\n{bad}\n")),
            at(3, ScalaErrorKind::BadPitch(bad.into())),
            "{bad}"
        );
    }
    for bad in ["0/1", "3/0", "-3/2", "0"] {
        assert_eq!(
            error(&format!("name\n1\n{bad}\n")),
            at(3, ScalaErrorKind::NotPositive(bad.into())),
            "{bad}"
        );
    }
    assert_eq!(
        error("name\n3\n100.0\n200.0\n"),
        at(
            5,
            ScalaErrorKind::TooFewPitches {
                expected: 3,
                found: 2
            }
        )
    );
    assert_eq!(
        error("name\n1\n2/1\n\n3/1\n"),
        at(5, ScalaErrorKind::TooMany("3/1".into()))
    );
    assert_eq!(
        error("name\n2\n3/2\n-1.0\n"),
        at(4, ScalaErrorKind::PeriodNotRising)
    );
    assert_eq!(
        error("name\n1\n1/1\n"),
        at(3, ScalaErrorKind::PeriodNotRising)
    );
    assert_eq!(
        error("name\n2\n100.0\nnope\n").to_string(),
        "line 4: \"nope\" is neither cents (with a '.') nor a ratio"
    );
}

#[test]
fn bad_mapping_files_say_which_line() {
    let error = |text: &str| KeyboardMapping::parse(text).unwrap_err();
    let at = |line, kind| ScalaError { line, kind };
    let field = |field, value: &str| ScalaErrorKind::BadField {
        field,
        value: value.into(),
    };
    assert_eq!(
        error("12\n0\n127\n"),
        at(4, ScalaErrorKind::Missing("middle key"))
    );
    assert_eq!(
        error("x\n0\n127\n60\n69\n440\n12\n"),
        at(1, field("map size", "x"))
    );
    assert_eq!(
        error("12\n0\n200\n60\n69\n440\n12\n"),
        at(3, field("last key to retune", "200"))
    );
    assert_eq!(
        error("12\n100\n50\n60\n69\n440\n12\n"),
        at(3, ScalaErrorKind::KeyRange)
    );
    assert_eq!(
        error("12\n0\n127\n60\n69\n-440\n12\n"),
        at(6, field("reference frequency", "-440"))
    );
    assert_eq!(
        error("12\n0\n127\n60\n69\n440\n-1\n"),
        at(7, field("formal octave degree", "-1"))
    );
    assert_eq!(
        error("! header\n2\n0\n127\n60\n60\n440\n12\n0\ny\n"),
        at(10, ScalaErrorKind::BadEntry("y".into()))
    );
    assert_eq!(
        error("2\n0\n127\n60\n60\n440\n12\n0\n1\n2\n"),
        at(10, ScalaErrorKind::TooMany("2".into()))
    );
    assert_eq!(
        error("2\n0\n127\n60\n61\n440\n12\n0\nx\n"),
        at(5, ScalaErrorKind::UnmappedReference)
    );
    // The reference is unmapped too when the entries stop before it.
    assert_eq!(
        error("3\n0\n127\n60\n62\n440\n12\n0\n"),
        at(5, ScalaErrorKind::UnmappedReference)
    );
}

/// A bulk tuning dump of `tuning`, with every key given or, where `skip`
/// says, marked "no change".
fn bulk_dump(tuning: &Tuning, skip: impl Fn(usize) -> bool) -> Vec<u8> {
    let mut message = vec![0xF0, 0x7E, 0x10, 0x08, 0x01, 0x05];
    message.extend_from_slice(b"Werckmeister    ");
    for key in 0..KEYS {
        if skip(key) {
            message.extend_from_slice(&[0x7F; 3]);
        } else {
            message.extend_from_slice(&mts_pitch_bytes(tuning.key_pitch(key as u8)).unwrap());
        }
    }
    let checksum = message[1..].iter().fold(0, |sum, &byte| sum ^ byte) & 0x7F;
    message.extend_from_slice(&[checksum, 0xF7]);
    message
}

/// Half of the 14-bit fraction of a semitone, in cents: the most a pitch
/// moves when it is written in MIDI tuning bytes.
const MTS_ROUNDING_CENTS: f64 = 100.0 / 16_384.0 / 2.0;

#[test]
fn a_bulk_tuning_dump_carries_every_key() {
    let werckmeister = temperament_tuning(Temperament::werckmeister_iii(), 0);
    let dump = bulk_dump(&werckmeister, |_| false);
    assert_eq!(dump.len(), 408);
    let message = MtsMessage::parse(&dump).unwrap();
    let MtsMessage::BulkDump {
        device, program, ..
    } = message
    else {
        panic!("{message:?}");
    };
    assert_eq!((device, program), (0x10, 5));
    assert_eq!(message.name(), Some("Werckmeister"));
    let tuning = message.apply(&Tuning::default());
    for key in 0..=127 {
        let cents = 100.0 * (tuning.key_pitch(key) - werckmeister.key_pitch(key));
        assert!(cents.abs() <= MTS_ROUNDING_CENTS + 1e-9, "{key}: {cents}");
    }
    // Without the F0 and F7, as a stream parser gives it.
    assert_eq!(MtsMessage::parse(&dump[1..dump.len() - 1]), Ok(message));

    // Keys marked "no change" keep the tuning the dump is applied to,
    // mapped or not.
    let white = scala(DIATONIC, Some(WHITE_KEYS));
    let partial = MtsMessage::parse(&bulk_dump(&werckmeister, |key| key < 64))
        .unwrap()
        .apply(&white);
    for key in 0..=127_u8 {
        if key < 64 {
            assert_eq!(partial.key_pitch(key), white.key_pitch(key));
            assert_eq!(partial.is_mapped(key), white.is_mapped(key));
        } else {
            assert!(partial.is_mapped(key));
            assert!((partial.key_pitch(key) - werckmeister.key_pitch(key)).abs() < 1e-4);
        }
    }
}

#[test]
fn single_note_tuning_changes() {
    let base = temperament_tuning(Temperament::kirnberger_iii(), 0);
    // Key 60 to 261.0 Hz-ish, key 61 a quarter tone above key 60's 12-TET
    // pitch, key 60 again (the last wins), and key 200 (ignored).
    let quarter = mts_pitch_bytes(60.5).unwrap();
    assert_eq!(quarter, [60, 0x40, 0x00]);
    let first = mts_pitch_bytes(59.987).unwrap();
    let last = mts_pitch_bytes(60.25).unwrap();
    let mut sysex = vec![0xF0, 0x7F, 0x7F, 0x08, 0x02, 0x00, 4];
    for (key, pitch) in [(60, first), (61, quarter), (60, last), (127, [0x7F; 3])] {
        sysex.push(key);
        sysex.extend_from_slice(&pitch);
    }
    sysex.push(0xF7);
    let message = MtsMessage::parse(&sysex).unwrap();
    assert!(matches!(
        message,
        MtsMessage::NoteChanges {
            device: 0x7F,
            bank: None,
            program: 0,
            real_time: true,
            ..
        }
    ));
    assert_eq!(message.name(), None);
    let tuning = message.apply(&base);
    assert_eq!(tuning.key_pitch(60), 60.25);
    assert_eq!(tuning.key_pitch(61), 60.5);
    assert_eq!(tuning.key_pitch(127), base.key_pitch(127));
    for key in (0..60).chain(62..=127) {
        assert_eq!(tuning.key_pitch(key), base.key_pitch(key));
    }

    // With a bank, real time or not.
    for universal in [0x7E, 0x7F] {
        let sysex = [universal, 0x00, 0x08, 0x07, 3, 9, 1, 69, 70, 0, 0];
        let message = MtsMessage::parse(&sysex).unwrap();
        assert!(matches!(
            message,
            MtsMessage::NoteChanges {
                bank: Some(3),
                program: 9,
                ..
            }
        ));
        assert_eq!(message.apply(&Tuning::default()).key_pitch(69), 70.0);
    }

    // The resolution: about 0.006 cent, and the bytes round to nearest.
    for pitch in [0.0, 0.3, 12.345_678, 69.0, 100.999_99, 127.9] {
        let bytes = mts_pitch_bytes(pitch).unwrap();
        let read = f64::from(bytes[0])
            + f64::from((u16::from(bytes[1]) << 7) | u16::from(bytes[2])) / 16_384.0;
        assert!(100.0 * (read - pitch).abs() <= MTS_ROUNDING_CENTS + 1e-12);
    }
    for pitch in [-0.1, 128.0, f64::NAN, 127.999_99] {
        assert_eq!(mts_pitch_bytes(pitch), None, "{pitch}");
    }
}

#[test]
fn bad_tuning_messages_are_refused() {
    let parse = |bytes: &[u8]| MtsMessage::parse(bytes).unwrap_err();
    assert_eq!(parse(&[]), MtsError::NotTuning);
    assert_eq!(
        parse(&[0xF0, 0x43, 0x10, 0x08, 0x01, 0xF7]),
        MtsError::NotTuning
    );
    assert_eq!(parse(&[0x7E, 0x7F, 0x06, 0x01]), MtsError::NotTuning);
    assert_eq!(
        parse(&[0x7E, 0x7F, 0x08, 0x03, 0x00]),
        MtsError::Unsupported(0x03)
    );
    assert_eq!(
        parse(&[0x7F, 0x7F, 0x08, 0x08, 0x00]),
        MtsError::Unsupported(0x08)
    );
    assert_eq!(
        parse(&[0x7F, 0x7F, 0x08, 0x02, 0x00, 0x90]),
        MtsError::NotData
    );
    assert_eq!(
        parse(&[0x7F, 0x7F, 0x08, 0x02, 0x00, 2, 60, 60, 0, 0]),
        MtsError::Length {
            expected: 14,
            found: 10
        }
    );
    assert_eq!(
        parse(&[0x7F, 0x7F, 0x08, 0x02]),
        MtsError::Length {
            expected: 6,
            found: 4
        }
    );

    let mut dump = bulk_dump(&Tuning::default(), |_| false);
    let at = dump.len() - 2;
    dump[at] ^= 0x01;
    assert!(matches!(
        MtsMessage::parse(&dump),
        Err(MtsError::Checksum { .. })
    ));
    assert!(matches!(
        MtsMessage::parse(&dump[..300]),
        Err(MtsError::Length { expected: 406, .. })
    ));
}

#[test]
fn tunings_from_tables() {
    let mut frequencies = [0.0; KEYS];
    for (key, value) in frequencies.iter_mut().enumerate() {
        *value = 100.0 + 10.0 * key as f64;
    }
    let tuning = Tuning::from_frequencies(&frequencies).unwrap();
    for key in 0..=127_u8 {
        let expected = frequencies[usize::from(key)];
        assert!((tuning.key_frequency(key) / expected - 1.0).abs() < 1e-14);
    }
    assert_eq!(tuning.frequencies().len(), 128);
    let again = Tuning::from_pitches(tuning.pitches()).unwrap();
    assert_eq!(again, tuning);

    frequencies[5] = 0.0;
    assert_eq!(
        Tuning::from_frequencies(&frequencies),
        Err(TuningError::NotFinite { key: 5 })
    );
    let mut pitches = [60.0; KEYS];
    pitches[9] = f64::NAN;
    assert_eq!(
        Tuning::from_pitches(&pitches),
        Err(TuningError::NotFinite { key: 9 })
    );
    // Keys above 127 read as 127.
    let standard = Tuning::default();
    assert_eq!(standard.key_frequency(200), standard.key_frequency(127));
    assert!(standard.is_mapped(255));
    assert!(format!("{standard:?}").contains("a4_hz: 440.0"));
}
