//! Measures the interpolators: how far below a pitched tone everything else
//! stays, how much of a tone pitched past half the output rate folds back,
//! and how much of a high tone passes. The figures in `Interpolation`'s
//! documentation come from here; run with `--nocapture` to see them.

mod common;

use common::*;
use manguebyte_sampler::{Interpolation, MemorySource};

/// Plays a sample of a sine at `frequency`, recorded at `rate`, on `key`
/// (60 is the sample's own pitch), and returns the output after the start.
fn play_at(frequency: f64, rate: u32, key: u8, interpolation: Interpolation) -> Vec<f32> {
    let mut source = MemorySource::new().with("t.wav", sine_wav(frequency, rate, 2.0, 0.5));
    let mut sampler = sampler_with("<region> sample=t.wav", &mut source, 2, 1);
    sampler.set_interpolation(interpolation);
    let output = render_mono(&mut sampler, 24_000, &[(0, on(key, 1.0))]);
    output[2_000..22_000].to_vec()
}

fn play(frequency: f64, key: u8, interpolation: Interpolation) -> Vec<f32> {
    play_at(frequency, 48_000, key, interpolation)
}

/// Level of everything but the expected tone, relative to it, in dB.
fn spurious_db(frequency: f64, key: u8, interpolation: Interpolation) -> f64 {
    let pitched = frequency * 2f64.powf((f64::from(key) - 60.0) / 12.0);
    let output = play(frequency, key, interpolation);
    let (amplitude, residual) = sine_fit(&output, pitched, RATE);
    assert!((amplitude - 0.5).abs() < 0.01, "amplitude {amplitude}");
    residual
}

/// Level of whatever comes out, relative to the tone that went in, in dB.
fn folded_db(frequency: f64, key: u8, interpolation: Interpolation) -> f64 {
    let output = play(frequency, key, interpolation);
    20.0 * (rms(&output) / (0.5 / 2f64.sqrt())).log10()
}

/// Level of the pitched tone, relative to the tone that went in, in dB.
fn passed_db(frequency: f64, rate: u32, key: u8, interpolation: Interpolation) -> f64 {
    let pitched = frequency * 2f64.powf((f64::from(key) - 60.0) / 12.0);
    let output = play_at(frequency, rate, key, interpolation);
    let (amplitude, _) = sine_fit(&output, pitched, RATE);
    20.0 * (amplitude / 0.5).log10()
}

#[test]
fn sinc_keeps_unwanted_components_far_down() {
    let up = spurious_db(1_000.0, 67, Interpolation::Sinc);
    let down = spurious_db(1_000.0, 53, Interpolation::Sinc);
    let high = spurious_db(8_000.0, 65, Interpolation::Sinc);
    eprintln!(
        "sinc: 1 kHz up a fifth {up:.1} dB, down a fifth {down:.1} dB, 8 kHz up a fourth {high:.1} dB"
    );
    assert!(up < -95.0, "{up}");
    assert!(down < -90.0, "{down}");
    assert!(high < -100.0, "{high}");
}

#[test]
fn sinc_removes_what_would_fold_back() {
    // 15 kHz up an octave would be 30 kHz; at 48 kHz it would fold back to
    // 18 kHz.
    let octave = folded_db(15_000.0, 72, Interpolation::Sinc);
    // 9 kHz up two octaves would be 36 kHz, folding to 12 kHz.
    let two = folded_db(9_000.0, 84, Interpolation::Sinc);
    eprintln!("sinc: 15 kHz up an octave {octave:.1} dB, 9 kHz up two octaves {two:.1} dB");
    assert!(octave < -80.0, "{octave}");
    assert!(two < -100.0, "{two}");
}

#[test]
fn a_tone_pitched_just_past_half_the_output_rate_does_not_fold_back() {
    // 20 kHz up four semitones is 25.2 kHz, which a 48 kHz output cannot
    // hold; what passes folds back to 22.8 kHz. The worst case: the tone
    // lies just past the cutoff, where a filter centred on half the rate
    // would let much of it through.
    let near = folded_db(20_000.0, 64, Interpolation::Sinc);
    // A semitone up from 23 kHz: 24.4 kHz, folding to 23.6 kHz.
    let nearer = folded_db(23_000.0, 61, Interpolation::Sinc);
    let cubic = folded_db(20_000.0, 64, Interpolation::Cubic);
    eprintln!(
        "20 kHz up four semitones: sinc {near:.1} dB, cubic {cubic:.1} dB; \
         23 kHz up a semitone: sinc {nearer:.1} dB"
    );
    assert!(near < -75.0, "fold-back at {near:.1} dB");
    assert!(nearer < -75.0, "fold-back at {nearer:.1} dB");
    assert!(cubic > -20.0, "{cubic}");
}

#[test]
fn sinc_passes_high_tones_to_its_stated_level() {
    // What the cutoff below half the rate costs: high tones are turned
    // down a little.
    let pass16 = passed_db(16_000.0, 48_000, 59, Interpolation::Sinc);
    let pass18 = passed_db(18_000.0, 44_100, 60, Interpolation::Sinc);
    let pass20 = passed_db(20_000.0, 48_000, 59, Interpolation::Sinc);
    // At 0.91 of half the sample's rate, the cutoff: about 6 dB down.
    let cutoff = passed_db(21_840.0, 48_000, 59, Interpolation::Sinc);
    eprintln!(
        "sinc: 16 kHz down a semitone {pass16:.3} dB, 18 kHz of a 44.1 kHz sample at its \
         pitch {pass18:.3} dB, 20 kHz down a semitone {pass20:.3} dB, 21.84 kHz down a \
         semitone {cutoff:.2} dB"
    );
    for level in [pass16, pass18, pass20] {
        assert!(level.abs() < 0.01, "{level}");
    }
    assert!((-6.5..-5.5).contains(&cutoff), "{cutoff}");
    // At its own pitch and rate the sample is read as it is: a 23 kHz tone
    // passes whole.
    let own = passed_db(23_000.0, 48_000, 60, Interpolation::Sinc);
    assert!(own.abs() < 1e-4, "{own}");
}

#[test]
fn cubic_is_cheaper_and_aliases() {
    let up = spurious_db(1_000.0, 67, Interpolation::Cubic);
    let high = spurious_db(8_000.0, 65, Interpolation::Cubic);
    let octave = folded_db(15_000.0, 72, Interpolation::Cubic);
    eprintln!(
        "cubic: 1 kHz up a fifth {up:.1} dB, 8 kHz up a fourth {high:.1} dB, \
         15 kHz up an octave {octave:.1} dB"
    );
    assert!(up < -85.0, "{up}");
    assert!(high > -60.0, "{high}");
    // This is what the sinc is for: the fold-back is barely attenuated.
    assert!(octave > -20.0, "{octave}");
}
