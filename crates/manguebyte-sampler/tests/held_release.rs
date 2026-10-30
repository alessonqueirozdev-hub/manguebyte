//! Release samples chosen by how long the note was held
//! (`mangue_loheld`, `mangue_hiheld`), and a release that starts with an
//! equal-power crossfade from the sustaining loop
//! (`mangue_release_crossfade`), aligned in phase with it
//! (`mangue_release_align`).

mod common;

use common::*;
use manguebyte_sampler::MemorySource;

#[test]
fn the_release_follows_how_long_the_note_was_held() {
    let text = "
<region> sample=*silence key=60
<group> key=60 trigger=release
<region> sample=short.wav mangue_hiheld=0.5
<region> sample=middle.wav mangue_loheld=0.5 mangue_hiheld=2
<region> sample=long.wav mangue_loheld=2
";
    let mut source = MemorySource::new()
        .with("short.wav", constant_wav(0.1, 48_000, 48_000))
        .with("middle.wav", constant_wav(0.2, 48_000, 48_000))
        .with("long.wav", constant_wav(0.4, 48_000, 48_000));
    for (held, level) in [(0.2, 0.1), (0.5, 0.2), (1.0, 0.2), (2.0, 0.4), (3.0, 0.4)] {
        let mut sampler = sampler_with(text, &mut source, 8, 1);
        let off_at = (held * RATE) as usize;
        let output = render_mono(
            &mut sampler,
            off_at + 2_000,
            &[(0, on(60, 1.0)), (off_at, off(60))],
        );
        assert!(output[..off_at].iter().all(|&s| s == 0.0));
        let heard = output[off_at + 1_000];
        assert!((heard - level).abs() < 1e-6, "held {held} s: {heard}");
        assert_eq!(sampler.active_voices(), 1, "held {held} s");
    }
}

const FREQUENCY: f64 = 375.0;
/// Frames in one period of [`FREQUENCY`] at 48 kHz.
const PERIOD: usize = 128;
const CROSSFADE: usize = 480;

/// A sustain looped on a whole number of periods, and a release of the same
/// sine from phase 0.
fn source() -> MemorySource {
    MemorySource::new()
        .with("sustain.wav", sine_wav(FREQUENCY, 48_000, 2.0, 0.5))
        .with("release.wav", sine_wav(FREQUENCY, 48_000, 1.0, 0.5))
}

fn text(align: u32) -> String {
    format!(
        "<region> sample=sustain.wav key=60 pitch_keycenter=60 loop_mode=loop_sustain \
         loop_start=12800 loop_end=25599 ampeg_release=0.005
<region> sample=release.wav key=60 pitch_keycenter=60 trigger=release \
         mangue_release_crossfade=0.01 mangue_release_align={align}"
    )
}

/// The phase of the sine at [`FREQUENCY`] that fits `signal`, whose first
/// frame is frame `start` of the output.
fn phase(signal: &[f32], start: usize) -> f64 {
    let omega = core::f64::consts::TAU * FREQUENCY / RATE;
    let (mut sine, mut cosine) = (0.0, 0.0);
    for (n, &value) in signal.iter().enumerate() {
        let angle = omega * (start + n) as f64;
        sine += f64::from(value) * angle.sin();
        cosine += f64::from(value) * angle.cos();
    }
    cosine.atan2(sine)
}

/// The output of a note held to `off_at`, and how far the phase after the
/// crossfade is from the phase before it, in radians.
fn phase_jump(align: u32, off_at: usize) -> (Vec<f32>, f64) {
    let mut source = source();
    let mut sampler = sampler_with(&text(align), &mut source, 8, 1);
    let frames = off_at + CROSSFADE + 4_000;
    let output = render_mono(&mut sampler, frames, &[(0, on(60, 1.0)), (off_at, off(60))]);
    // The crossfade over, the sustain has stopped.
    assert_eq!(sampler.active_voices(), 1);
    let window = 8 * PERIOD;
    let before = phase(&output[off_at - window..off_at], off_at - window);
    let from = off_at + CROSSFADE + PERIOD;
    let after = phase(&output[from..from + window], from);
    let jump = (after - before + core::f64::consts::PI).rem_euclid(core::f64::consts::TAU)
        - core::f64::consts::PI;
    (output, jump.abs())
}

#[test]
fn an_aligned_release_carries_the_loops_phase_on() {
    let mut random = Random::new(17);
    let mut worst_aligned: f64 = 0.0;
    let mut worst_plain: f64 = 0.0;
    for _ in 0..8 {
        // A random place in the loop, from a random time held.
        let off_at = 30_000 + random.below(40_000);
        let (output, aligned) = phase_jump(2 * PERIOD as u32, off_at);
        let (_, plain) = phase_jump(0, off_at);
        worst_aligned = worst_aligned.max(aligned);
        // No step between frames larger than two sines in phase take, plus
        // what the crossfade's gains add over a frame.
        let omega = core::f64::consts::TAU * FREQUENCY / RATE;
        let bound = 0.5 * core::f64::consts::SQRT_2 * omega
            + 0.5 * core::f64::consts::FRAC_PI_2 * core::f64::consts::SQRT_2 / CROSSFADE as f64;
        let largest = output[off_at - 1_000..off_at + CROSSFADE + 1_000]
            .windows(2)
            .map(|pair| f64::from((pair[1] - pair[0]).abs()))
            .fold(0.0, f64::max);
        assert!(
            largest <= bound,
            "{off_at}: a step of {largest} against {bound}"
        );
        worst_plain = worst_plain.max(plain);
        // Equal power: the level never dips while the two cross, and goes
        // no higher than the sum of two sines in phase.
        let peak = |range: core::ops::Range<usize>| {
            output[range]
                .iter()
                .fold(0.0f32, |peak, s| peak.max(s.abs()))
        };
        for start in (off_at..off_at + CROSSFADE).step_by(PERIOD) {
            let level = peak(start..start + PERIOD);
            assert!(level > 0.49 && level < 0.5 * 1.42, "{off_at}: {level}");
        }
        let after = peak(off_at + CROSSFADE..off_at + CROSSFADE + 2 * PERIOD);
        assert!((after - 0.5).abs() < 0.01, "{after}");
    }
    println!(
        "phase jump: aligned at most {worst_aligned:.4} rad, without alignment {worst_plain:.3}"
    );
    assert!(worst_aligned < 0.01, "{worst_aligned}");
    assert!(worst_plain > 1.0, "{worst_plain}");
}

#[test]
fn a_release_with_no_sustain_to_cross_from_starts_at_full_level() {
    let mut source = source();
    let text = "<region> sample=*silence key=60
<region> sample=release.wav key=60 trigger=release mangue_release_crossfade=0.01 mangue_release_align=256";
    let mut sampler = sampler_with(text, &mut source, 8, 1);
    let output = render_mono(&mut sampler, 10_000, &[(0, on(60, 1.0)), (1_000, off(60))]);
    // Played from its offset, at once at full level.
    let expected = sine(FREQUENCY, RATE, 200, 0.5);
    for (n, &value) in output[1_000..1_200].iter().enumerate() {
        assert!((f64::from(value) - f64::from(expected[n])).abs() < 1e-6);
    }
}
