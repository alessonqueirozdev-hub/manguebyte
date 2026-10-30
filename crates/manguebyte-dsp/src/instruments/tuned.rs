//! Every pitched voice plays in the tuning its synth gives it.

use super::common::key_hz;
use super::measure::{Act, RATE, between, cents, frequency_near, off, on, play, synth_of};
use super::*;
use crate::Voice;
use crate::tuning::{Scale, Temperament, Tuning, TuningSettings};

/// The frequency `voice` sounds at for `key`, measured between `from` and
/// `to` seconds into a second of the note held, in `tuning`.
fn sounds<V: Voice<f64>>(voice: V, tuning: &Tuning, key: u8, from: f64, to: f64) -> f64 {
    let mut synth = synth_of([voice]);
    synth.set_tuning(tuning);
    let signal = play(
        &mut synth,
        &[(0, on(key, 0.8)), ((1.2 * RATE) as usize, off(key))],
        (1.2 * RATE) as usize,
        256,
    );
    frequency_near(between(&signal, from, to), tuning.key_frequency(key))
}

/// Checks that `make`'s voice, played on `key`, moves by what each tuning
/// moves the key by, against the same voice in equal temperament.
fn follows<V: Voice<f64>>(name: &str, make: impl Fn() -> V, key: u8, from: f64, to: f64) {
    let standard = Tuning::default();
    let baroque = TuningSettings {
        reference_hz: 415.0,
        ..TuningSettings::default()
    }
    .build()
    .unwrap();
    let nineteen = TuningSettings {
        scale: Scale::equal(19),
        ..TuningSettings::default()
    }
    .build()
    .unwrap();
    let meantone = TuningSettings {
        scale: Scale::Temperament {
            temperament: Temperament::quarter_comma_meantone(),
            key: 0,
        },
        ..TuningSettings::default()
    }
    .build()
    .unwrap();
    let plain = sounds(make(), &standard, key, from, to);
    for (tuning_name, tuning) in [
        ("A4 = 415 Hz", baroque),
        ("19-TET", nineteen),
        ("meantone", meantone),
    ] {
        let heard = sounds(make(), &tuning, key, from, to);
        let expected = cents(tuning.key_frequency(key), key_hz(f64::from(key)));
        let moved = cents(heard, plain);
        assert!(
            (moved - expected).abs() < 1.5,
            "{name} on key {key} in {tuning_name}: moved {moved:.2} cents, expected {expected:.2}"
        );
    }
}

#[test]
fn every_pitched_voice_follows_the_tuning() {
    follows(
        "nylon guitar",
        || PluckedString::new(StringParams::nylon_guitar()),
        52,
        0.1,
        0.6,
    );
    follows(
        "electric piano",
        || FmVoice::new(FmParams::electric_piano()),
        64,
        0.1,
        0.6,
    );
    follows(
        "organ",
        || OrganVoice::new(OrganParams::jazz()),
        64,
        0.2,
        1.0,
    );
    follows(
        "acid bass",
        || AcidVoice::new(AcidParams::classic()),
        45,
        0.2,
        0.8,
    );
    follows(
        "kick bass",
        || KickBassVoice::new(KickBassParams::default()),
        40,
        0.4,
        1.0,
    );
    follows(
        "polysynth",
        || PolySynthVoice::new(PolySynthParams::default()),
        57,
        0.2,
        1.0,
    );
    follows(
        "violin",
        || BowedString::new(BowedParams::violin()),
        67,
        0.5,
        1.1,
    );
    follows(
        "flute",
        || WindVoice::new(WindParams::flute()),
        72,
        0.5,
        1.1,
    );
    follows(
        "marimba",
        || MalletVoice::new(MalletParams::marimba()),
        64,
        0.05,
        0.5,
    );
    follows(
        "voice",
        || ChoirVoice::new(ChoirParams::solo_voice()),
        57,
        0.5,
        1.1,
    );
}

#[test]
fn a_voice_given_the_default_tuning_sounds_exactly_as_before() {
    // `Tuning::default()` gives every key the number it had, so a voice
    // handed it renders what a voice never handed a tuning renders, bit for
    // bit, bends and glides included.
    fn same<V: Voice<f64>>(name: &str, make: impl Fn() -> V) {
        let script = [
            (0, on(48, 0.9)),
            (4_000, Act::Bend(1.3)),
            (9_000, on(55, 0.6)),
            (14_000, off(48)),
            (20_000, off(55)),
        ];
        let mut untouched = synth_of([make(), make()]);
        let mut tuned = synth_of([make(), make()]);
        tuned.set_tuning(&TuningSettings::default().build().unwrap());
        let a = play(&mut untouched, &script, 24_000, 512);
        let b = play(&mut tuned, &script, 24_000, 512);
        assert!(
            a.iter().zip(&b).all(|(a, b)| a.to_bits() == b.to_bits()),
            "{name}"
        );
        assert!(a.iter().any(|&x| x != 0.0), "{name} is silent");
    }
    same(
        "string",
        || PluckedString::new(StringParams::nylon_guitar()),
    );
    same("fm", || FmVoice::new(FmParams::electric_piano()));
    same("organ", || OrganVoice::new(OrganParams::jazz()));
    same("acid", || AcidVoice::new(AcidParams::classic()));
    same(
        "kick bass",
        || KickBassVoice::new(KickBassParams::default()),
    );
    same("poly", || PolySynthVoice::new(PolySynthParams::default()));
    same("bowed", || BowedString::new(BowedParams::violin()));
    same("wind", || WindVoice::new(WindParams::flute()));
    same("mallet", || MalletVoice::new(MalletParams::marimba()));
    same("choir", || ChoirVoice::new(ChoirParams::solo_voice()));
}
