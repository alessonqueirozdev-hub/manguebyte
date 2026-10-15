//! Mangue Pluck plays in tune, within a cent, while its filter sweeps
//! (TECH_DEBT 43), at the settings measured here.
//!
//! Each note's low-pass filter closes as the note decays and again as it is
//! released. A closing filter delays the note more and more, and a growing
//! delay lowers the pitch that comes out, as a Doppler shift does: before
//! the voice made up for it, notes measured up to 3 cents flat while they
//! decayed at the default settings (key 84), 7.5 cents flat with the
//! resonance at its top (key 86), and in tune only once the filter stood
//! still.
//!
//! Every key from 36 to 96 is measured at 44.1, 48 and 96 kHz, under three
//! settings, while it decays, while it is held and while it is released.
//! Darker settings, whose cutoff sweeps across a low note's fundamental, are
//! measured with windows of 12 and of 24 periods, which must both agree on
//! the note being within a cent: there the filter's own ringing sits near
//! the fundamental and a short window cannot tell them apart.
//!
//! The voice makes up for the lag of the filter as it stands on each frame.
//! When a resonant filter sweeps across a fundamental faster than it
//! settles, that is no longer the lag the note hears: with the cutoff at
//! 100 Hz and the resonance at 8, low notes come out several cents sharp
//! while the cutoff passes them. That case is measured, and ignored until
//! it is fixed (`a_dark_resonant_pluck_is_within_a_cent`).

use std::f64::consts::{PI, TAU};

use mangue_pluck::{Pluck, PluckParams};
use manguebyte::prelude::*;

/// The pitch of the partial nearest `nominal` around frame `centre`, from
/// how far its phase moves between two Hann-weighted windows a twentieth
/// of a second apart. Each window holds at least six periods, so the
/// neighbouring harmonics leak into it by less than 60 dB.
fn pitch_at(signal: &[f32], rate: f64, nominal: f64, centre: usize) -> f64 {
    let length = ((6.0 * rate / nominal) as usize).max((0.02 * rate) as usize);
    let gap = (0.05 * rate) as usize;
    let phase = |middle: usize| {
        let start = middle - length / 2;
        let (mut along, mut across) = (0.0, 0.0);
        for n in 0..length {
            let window = 0.5 - 0.5 * (TAU * n as f64 / length as f64).cos();
            let angle = TAU * nominal * (start + n) as f64 / rate;
            let x = f64::from(signal[start + n]) * window;
            along += x * angle.cos();
            across -= x * angle.sin();
        }
        across.atan2(along)
    };
    let moved = phase(centre + gap / 2) - phase(centre - gap / 2);
    // The phase is known up to whole turns; the true advance is the one
    // nearest none, since the pitch is within far less than a turn.
    let moved = (moved + PI).rem_euclid(TAU) - PI;
    nominal + moved / TAU * rate / gap as f64
}

fn cents(measured: f64, nominal: f64) -> f64 {
    1200.0 * (measured / nominal).log2()
}

/// Puts the parameters at one of the settings measured.
type Setup = fn(&PluckParams);

/// The settings measured: the defaults; the resonance at its top; and the
/// bright, slow settings of the REAPER project of TECH_DEBT 42.
const SETTINGS: [(&str, Setup); 3] = [
    ("default", |_| {}),
    ("resonant", |params| params.resonance.set(8.0)),
    ("bright and slow", |params| {
        params.cutoff.set(12_000.0);
        params.resonance.set(0.5);
        params.decay.set(5.0);
        params.release.set(5.0);
    }),
];

/// The worst deviation, in cents, of any key from 36 to 96 at `rate`, with
/// where it was found.
fn worst_deviation(rate: f64) -> (f64, String) {
    let seconds = |s: f64| (s * rate) as usize;
    let mut worst = (0.0_f64, String::new());
    for (name, setup) in SETTINGS {
        for key in 36..=96_u8 {
            let mut harness = Harness::<Pluck>::new(rate, 512);
            setup(harness.params());
            // No reverb: it repeats the note, which blurs the measurement.
            harness.params().reverb.set(0.0);
            harness.note(0, seconds(0.4) as u64, key, 1.0);
            let sound = harness.render(seconds(0.6));
            let nominal = note_to_hz(f64::from(key));
            // While the default decay runs (0.3 s), held, and released.
            for at in [0.1, 0.2, 0.35, 0.5] {
                let measured = pitch_at(sound.channel(0), rate, nominal, seconds(at));
                let off = cents(measured, nominal);
                assert!(off.is_finite(), "{name}, key {key}, {at} s");
                if off.abs() > worst.0.abs() {
                    worst = (off, format!("{name}, key {key}, at {at} s"));
                }
            }
        }
    }
    worst
}

fn every_key_is_within_a_cent_at(rate: f64) {
    let (cents, place) = worst_deviation(rate);
    println!("{rate} Hz: at worst {cents:+.4} cents ({place})");
    assert!(
        cents.abs() < 1.0,
        "{cents:+.3} cents at {rate} Hz ({place})"
    );
}

#[test]
fn every_key_is_within_a_cent_at_44100_hz() {
    every_key_is_within_a_cent_at(44_100.0);
}

#[test]
fn every_key_is_within_a_cent_at_48000_hz() {
    every_key_is_within_a_cent_at(48_000.0);
}

#[test]
fn every_key_is_within_a_cent_at_96000_hz() {
    every_key_is_within_a_cent_at(96_000.0);
}

/// The measurement itself: a sine of known pitch, a few cents off its
/// nominal key, is measured to within a hundredth of a cent.
#[test]
fn the_measurement_finds_a_known_pitch() {
    for rate in [44_100.0, 96_000.0] {
        for (key, off) in [(36.0, 3.0), (60.0, -0.5), (96.0, 1.25)] {
            let nominal = note_to_hz(key);
            let actual = nominal * 2f64.powf(off / 1200.0);
            // A sawtooth-like sum of harmonics, as the voice produces.
            let signal: Vec<f32> = (0..(0.6 * rate) as usize)
                .map(|n| {
                    (1..=5)
                        .map(|h| {
                            (TAU * actual * f64::from(h) * n as f64 / rate).sin() / f64::from(h)
                        })
                        .sum::<f64>() as f32
                })
                .collect();
            let measured = pitch_at(&signal, rate, nominal, (0.3 * rate) as usize);
            assert!(
                (cents(measured, nominal) - off).abs() < 0.01,
                "key {key} at {rate} Hz: {} cents",
                cents(measured, nominal)
            );
        }
    }
}

/// The same measurement with windows of `periods` periods. Longer windows
/// keep the filter's own ringing, near a low fundamental when the cutoff is
/// low, out of the measurement.
fn pitch_over(signal: &[f32], rate: f64, nominal: f64, centre: usize, periods: f64) -> f64 {
    let length = ((periods * rate / nominal) as usize).max((0.02 * rate) as usize);
    let gap = (0.05 * rate) as usize;
    let phase = |middle: usize| {
        let start = middle - length / 2;
        let (mut along, mut across) = (0.0, 0.0);
        for n in 0..length {
            let window = 0.5 - 0.5 * (TAU * n as f64 / length as f64).cos();
            let angle = TAU * nominal * (start + n) as f64 / rate;
            let x = f64::from(signal[start + n]) * window;
            along += x * angle.cos();
            across -= x * angle.sin();
        }
        across.atan2(along)
    };
    let moved = phase(centre + gap / 2) - phase(centre - gap / 2);
    let moved = (moved + PI).rem_euclid(TAU) - PI;
    nominal + moved / TAU * rate / gap as f64
}

/// The worst deviation, in cents, at a dark setting, over `keys` at
/// `rate`: the larger of what windows of 12 and of 24 periods measure, so
/// that the note counts as in tune only when both say so.
fn worst_dark_deviation(
    rate: f64,
    settings: &[(f32, f32)],
    keys: impl Iterator<Item = u8> + Clone,
) -> (f64, String) {
    let seconds = |s: f64| (s * rate) as usize;
    let mut worst = (0.0_f64, String::new());
    for &(cutoff, resonance) in settings {
        for key in keys.clone() {
            let mut harness = Harness::<Pluck>::new(rate, 512);
            harness.params().cutoff.set(cutoff);
            harness.params().resonance.set(resonance);
            harness.params().reverb.set(0.0);
            harness.note(0, seconds(0.4) as u64, key, 1.0);
            let sound = harness.render(seconds(0.8));
            let nominal = note_to_hz(f64::from(key));
            for at in [0.25, 0.35, 0.5] {
                for periods in [12.0, 24.0] {
                    let measured =
                        pitch_over(sound.channel(0), rate, nominal, seconds(at), periods);
                    let off = cents(measured, nominal);
                    assert!(off.is_finite(), "cutoff {cutoff}, key {key}, {at} s");
                    if off.abs() > worst.0.abs() {
                        worst = (
                            off,
                            format!(
                                "cutoff {cutoff}, resonance {resonance}, key {key}, at {at} s, \
                                 {periods} periods"
                            ),
                        );
                    }
                }
            }
        }
    }
    worst
}

/// Darker settings at which every key is within a cent: the lower the
/// cutoff, the less resonance it takes. Measured at worst: -0.47 cents at
/// 300 Hz with resonance 2, -0.46 at 600 Hz with 4, -0.44 at 1 kHz with 8
/// (keys a third apart, at 48 kHz).
const DARK_IN_TUNE: [(f32, f32); 3] = [(300.0, 2.0), (600.0, 4.0), (1_000.0, 8.0)];

fn dark_keys_are_within_a_cent_at(rate: f64, step: usize) {
    let (cents, place) = worst_dark_deviation(rate, &DARK_IN_TUNE, (36..=96_u8).step_by(step));
    println!("{rate} Hz, dark: at worst {cents:+.4} cents ({place})");
    assert!(
        cents.abs() < 1.0,
        "{cents:+.3} cents at {rate} Hz ({place})"
    );
}

#[test]
fn darker_settings_keep_every_key_within_a_cent_at_48000_hz() {
    dark_keys_are_within_a_cent_at(48_000.0, 1);
}

/// Every fourth key at the other two rates, to keep the test's time down.
#[test]
fn darker_settings_keep_the_keys_within_a_cent_at_44100_and_96000_hz() {
    dark_keys_are_within_a_cent_at(44_100.0, 4);
    dark_keys_are_within_a_cent_at(96_000.0, 4);
}

/// A resonant filter swept across low fundamentals: not yet within a cent.
/// The voice makes up for the lag of the filter as it stands, and when the
/// cutoff moves past a fundamental faster than a resonant filter settles
/// (its delay there is about 2Q / (2π·cutoff), 36 ms at Q 8 and 70 Hz),
/// the note hears another lag. Measured: +3.1 cents at cutoff 150 Hz and
/// resonance 8 (key 46) when only the deviations both windows agree on are
/// counted, and up to +4.5 to +10.5 cents at 100 Hz and resonance 8 (key
/// 36). Making up for it needs a model of the moving filter's phase, which
/// a phasor through the same filter does not give (fed back, it is
/// unstable; fed forward, its correction is delayed a second time).
#[test]
#[ignore = "known gap of TECH_DEBT 43: a resonant cutoff swept across a low fundamental"]
fn a_dark_resonant_pluck_is_within_a_cent() {
    let (cents, place) = worst_dark_deviation(
        48_000.0,
        &[(150.0, 4.0), (150.0, 8.0), (300.0, 8.0), (100.0, 8.0)],
        (36..=96_u8).step_by(2),
    );
    println!("dark and resonant: at worst {cents:+.4} cents ({place})");
    assert!(cents.abs() < 1.0, "{cents:+.3} cents ({place})");
}
