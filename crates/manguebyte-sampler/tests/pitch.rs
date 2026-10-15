//! Pitch after resampling: within a cent across two octaves each way.

mod common;

use common::*;
use manguebyte_dsp::Processor;
use manguebyte_sampler::{Interpolation, MemorySource, Sampler};

/// A4 recorded at 44.1 kHz, played back at 48 kHz, so every note is also a
/// sample-rate conversion.
fn source() -> MemorySource {
    MemorySource::new().with("a4.wav", sine_wav(440.0, 44_100, 2.0, 0.5))
}

fn measure(sampler: &mut Sampler<f32>, key: u8) -> f64 {
    sampler.reset();
    let output = render_mono(sampler, 19_200, &[(0, on(key, 1.0))]);
    frequency(&output[960..19_200], RATE)
}

#[test]
fn every_key_two_octaves_either_side_is_within_a_cent() {
    for interpolation in [Interpolation::Sinc, Interpolation::Cubic] {
        let mut source = source();
        let mut sampler = sampler_with(
            "<region> sample=a4.wav pitch_keycenter=69",
            &mut source,
            4,
            1,
        );
        sampler.set_interpolation(interpolation);
        assert_eq!(sampler.interpolation(), interpolation);
        let mut worst: f64 = 0.0;
        for key in 45..=93u8 {
            let expected = 440.0 * 2f64.powf((f64::from(key) - 69.0) / 12.0);
            let error = cents(measure(&mut sampler, key), expected);
            worst = worst.max(error.abs());
            assert!(
                error.abs() < 1.0,
                "{interpolation:?} key {key}: {error} cents"
            );
        }
        // The measurement itself is the limit: the position is exact.
        assert!(worst < 0.01, "{interpolation:?}: worst {worst} cents");
    }
}

#[test]
fn tune_transpose_keytrack_and_the_wheel_move_the_pitch() {
    let mut source = source();
    let cases: [(&str, u8, f64); 5] = [
        ("tune=-50", 69, 440.0 * 2f64.powf(-50.0 / 1_200.0)),
        ("transpose=-12", 69, 220.0),
        ("pitch_keytrack=50", 81, 440.0 * 2f64.powf(0.5)),
        ("pitch_keytrack=0", 50, 440.0),
        ("pitch_keycenter=a3", 69, 880.0),
    ];
    for (opcode, key, expected) in cases {
        let text = format!("<region> sample=a4.wav pitch_keycenter=69 {opcode}");
        let mut sampler = sampler_with(&text, &mut source, 4, 1);
        let error = cents(measure(&mut sampler, key), expected);
        assert!(error.abs() < 0.05, "{opcode}: {error} cents");
    }

    // The wheel, with the region's own range: full up is bend_up cents,
    // full down is bend_down.
    let text = "<region> sample=a4.wav pitch_keycenter=69 bend_up=700 bend_down=-1200";
    let mut sampler = sampler_with(text, &mut source, 4, 1);
    for (value, expected) in [
        (8_191, 440.0 * 2f64.powf(7.0 / 12.0)),
        (-8_192, 220.0),
        (0, 440.0),
    ] {
        sampler.reset();
        // The wheel moves before the note, and the note starts bent.
        let events = [(0, bend(value)), (0, on(69, 1.0))];
        let output = render_mono(&mut sampler, 19_200, &events);
        let error = cents(frequency(&output[960..], RATE), expected);
        assert!(error.abs() < 0.05, "wheel {value}: {error} cents");
    }

    // Moving the wheel while the note sounds bends it on the frame.
    sampler.reset();
    let events = [(0, on(69, 1.0)), (9_600, bend(8_191))];
    let output = render_mono(&mut sampler, 24_000, &events);
    let before = frequency(&output[960..9_600], RATE);
    let after = frequency(&output[9_700..], RATE);
    assert!(cents(before, 440.0).abs() < 0.05);
    assert!(cents(after, 440.0 * 2f64.powf(7.0 / 12.0)).abs() < 0.05);
}

#[test]
fn a_sample_at_its_own_pitch_and_rate_comes_out_unchanged() {
    // 48 kHz sample, 48 kHz output, key center: the sinc reads whole
    // frames and gives them back exactly.
    let tone = sine(1_000.0, RATE, 4_800, 0.5);
    let mut source =
        MemorySource::new().with("t.wav", wav_bytes(std::slice::from_ref(&tone), 48_000));
    let mut sampler = sampler_with("<region> sample=t.wav", &mut source, 4, 1);
    let output = render_mono(&mut sampler, 4_800, &[(0, on(60, 1.0))]);
    assert_eq!(output, tone);
}
