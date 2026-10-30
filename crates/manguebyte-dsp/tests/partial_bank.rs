//! The partial bank: every backend against the scalar code, against
//! Caecilia's kernel, how well a phasor keeps its amplitude and frequency
//! over a long run, and that the way a caller splits its blocks changes
//! nothing.

use manguebyte_core::vector::Backend;
use manguebyte_dsp::{Partial, PartialBank};

const RATE: f64 = 48_000.0;
/// The control block: settings change this often, as in Caecilia.
const CONTROL: usize = 64;

/// A small, fixed random generator (xorshift64*), so failures repeat.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }
}

/// What a control block sets on each partial.
#[derive(Clone, Copy)]
struct Setting {
    freq_hz: f64,
    target: f32,
    pan: f32,
}

/// Settings for `blocks` control blocks of `count` partials: frequencies
/// anywhere from 0 to Nyquist (Nyquist itself included), gains that ramp
/// to a new target every block, and pans that move now and then.
fn script(seed: u64, count: usize, blocks: usize) -> Vec<Vec<Setting>> {
    let mut rng = Rng::new(seed);
    let mut now: Vec<Setting> = (0..count)
        .map(|_| Setting {
            freq_hz: match rng.below(10) {
                0 => RATE / 2.0,
                1 => 0.0,
                _ => rng.unit() * RATE / 2.0,
            },
            target: (rng.unit() * 2.0 - 1.0) as f32,
            pan: (rng.unit() * 2.0 - 1.0) as f32,
        })
        .collect();
    (0..blocks)
        .map(|_| {
            for setting in &mut now {
                // A glide of up to a few cents a block, as drift and wind
                // make, and now and then a jump.
                if rng.below(20) == 0 {
                    setting.freq_hz = rng.unit() * RATE / 2.0;
                } else {
                    let cents = (rng.unit() * 2.0 - 1.0) * 3.0;
                    setting.freq_hz = (setting.freq_hz * (cents / 1200.0).exp2()).min(RATE / 2.0);
                }
                setting.target = (rng.unit() * 2.0 - 1.0) as f32;
                if rng.below(8) == 0 {
                    setting.pan = (rng.unit() * 2.0 - 1.0) as f32;
                }
            }
            now.clone()
        })
        .collect()
}

fn bank(backend: Backend, count: usize, seed: u64) -> PartialBank {
    let mut bank = PartialBank::new(count);
    bank.set_backend(backend);
    bank.set_sample_rate(RATE);
    bank.set_len(count);
    let mut rng = Rng::new(seed ^ 0xABCD);
    for i in 0..count {
        bank.set_phase(i, rng.unit());
        bank.set_gain(i, (rng.unit() * 2.0 - 1.0) as f32);
    }
    bank
}

/// Plays `script` on a bank, handing it blocks of the sizes `sizes` gives
/// in turn, and applying each control block's settings on its first frame.
fn play(
    mut bank: PartialBank,
    script: &[Vec<Setting>],
    mut sizes: impl FnMut() -> usize,
) -> (Vec<f32>, Vec<f32>, PartialBank) {
    let frames = script.len() * CONTROL;
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let mut at = 0;
    while at < frames {
        let end = (at + sizes()).min(frames);
        // The caller's block, split at the control blocks it holds.
        let mut from = at;
        while from < end {
            if from % CONTROL == 0 {
                for (i, setting) in script[from / CONTROL].iter().enumerate() {
                    bank.set_frequency(i, setting.freq_hz);
                    bank.ramp_to(i, setting.target, CONTROL);
                    bank.set_pan(i, setting.pan);
                }
            }
            let to = end.min((from / CONTROL + 1) * CONTROL);
            bank.render_add(&mut left[from..to], &mut right[from..to]);
            from = to;
        }
        at = end;
    }
    (left, right, bank)
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

#[test]
fn every_backend_renders_the_scalar_output_bit_for_bit_whatever_the_block_sizes() {
    let backends: Vec<Backend> = Backend::available().collect();
    for (seed, count) in [1usize, 7, 16, 23, 64, 100].into_iter().enumerate() {
        let script = script(seed as u64, count, 96);
        // The reference: the scalar code, one control block at a time.
        let (want_l, want_r, want_bank) =
            play(bank(Backend::Scalar, count, seed as u64), &script, || {
                CONTROL
            });
        for &backend in &backends {
            for sizes in 0..4 {
                let mut rng = Rng::new(sizes as u64 + 10);
                let next = move || match sizes {
                    0 => CONTROL,
                    1 => 1,
                    2 => 4_096,
                    _ => 1 + rng.below(4_096),
                };
                let (l, r, got_bank) = play(bank(backend, count, seed as u64), &script, next);
                let what = format!("{} count {count} sizes {sizes}", backend.name());
                assert!(same_bits(&l, &want_l), "left {what}");
                assert!(same_bits(&r, &want_r), "right {what}");
                for i in 0..count {
                    assert_eq!(got_bank.phasor(i), want_bank.phasor(i), "phasor {i} {what}");
                    assert_eq!(
                        got_bank.gain(i).to_bits(),
                        want_bank.gain(i).to_bits(),
                        "gain {i} {what}"
                    );
                }
            }
        }
        assert!(want_l.iter().any(|&x| x != 0.0), "count {count}: silent");
    }
}

/// Caecilia 2.0's partial kernel, as `caecilia-synth/src/math.rs` has it
/// (Apache-2.0, by the same author), copied unchanged as the reference.
#[allow(clippy::too_many_arguments)]
fn partial_accumulate(
    left: &mut [f32],
    right: &mut [f32],
    osc: &mut (f32, f32),
    cos_inc: f32,
    sin_inc: f32,
    gain: &mut f32,
    gain_inc: f32,
    pan_l: f32,
    pan_r: f32,
) {
    let (mut x, mut y, mut g) = (osc.0, osc.1, *gain);
    for (l, r) in left.iter_mut().zip(right.iter_mut()) {
        g += gain_inc;
        let v = g * y;
        *l += v * pan_l;
        *r += v * pan_r;
        let nx = x * cos_inc - y * sin_inc;
        let ny = x * sin_inc + y * cos_inc;
        x = nx;
        y = ny;
    }
    *osc = (x, y);
    *gain = g;
}

/// One partial of Caecilia's bank: the state its render loop keeps.
#[derive(Clone, Copy)]
struct CaeciliaPartial {
    osc: (f32, f32),
    cur_gain: f32,
    pan_l: f32,
    pan_r: f32,
}

/// Caecilia's control block: new increments from the frequency in `f32`
/// as it works them out, one Newton step on each phasor, the gain step to
/// the block's target, then each partial in turn into a cleared chunk.
/// Returns the chunk and, for each frame, a bound on the sum of the
/// magnitudes of its terms.
fn caecilia_block(
    partials: &mut [CaeciliaPartial],
    settings: &[Setting],
    increments: &[(f32, f32)],
) -> (Vec<f32>, Vec<f32>, f64) {
    let (mut l, mut r) = (vec![0.0f32; CONTROL], vec![0.0f32; CONTROL]);
    let inv = 1.0 / CONTROL as f32;
    let mut magnitude = 0.0f64;
    for ((p, setting), &(cos_inc, sin_inc)) in partials.iter_mut().zip(settings).zip(increments) {
        let mag = p.osc.0 * p.osc.0 + p.osc.1 * p.osc.1;
        let renorm = 1.5 - 0.5 * mag;
        p.osc.0 *= renorm;
        p.osc.1 *= renorm;
        let gain_inc = (setting.target - p.cur_gain) * inv;
        magnitude += f64::from(p.cur_gain.abs().max(setting.target.abs()))
            * f64::from(p.pan_l.abs().max(p.pan_r.abs()));
        partial_accumulate(
            &mut l,
            &mut r,
            &mut p.osc,
            cos_inc,
            sin_inc,
            &mut p.cur_gain,
            gain_inc,
            p.pan_l,
            p.pan_r,
        );
    }
    (l, r, magnitude)
}

/// Caecilia's increments for a frequency: `2π` as an `f32`, the angle
/// rounded to `f32`, then its cosine and sine in `f32`.
fn caecilia_increments(freq_hz: f64) -> (f32, f32) {
    let w = (f64::from(std::f32::consts::TAU) * freq_hz / RATE) as f32;
    (w.cos(), w.sin())
}

/// Drives Caecilia's loop and the bank (given the same increments and pan
/// gains) side by side, one control block at a time, and calls `check`
/// with both chunks, the bank and Caecilia's partials after each.
fn against_caecilia(
    backend: Backend,
    count: usize,
    blocks: usize,
    mut check: impl FnMut(&[f32], &[f32], &[f32], &[f32], f64, &PartialBank, &[CaeciliaPartial]),
) {
    let script = script(count as u64 + 5, count, blocks);
    let mut bank = bank(backend, count, 99);
    let mut caecilia: Vec<CaeciliaPartial> = (0..count)
        .map(|i| CaeciliaPartial {
            osc: bank.phasor(i),
            cur_gain: bank.gain(i),
            pan_l: 0.0,
            pan_r: 0.0,
        })
        .collect();
    for settings in &script {
        let increments: Vec<(f32, f32)> = settings
            .iter()
            .map(|s| caecilia_increments(s.freq_hz))
            .collect();
        for (i, (setting, &(c, s))) in settings.iter().zip(&increments).enumerate() {
            bank.set_rotation(i, c, s);
            bank.ramp_to(i, setting.target, CONTROL);
            bank.set_pan(i, setting.pan);
            let (pan_l, pan_r) = bank.pan_gains(i);
            caecilia[i].pan_l = pan_l;
            caecilia[i].pan_r = pan_r;
        }
        let (want_l, want_r, magnitude) = caecilia_block(&mut caecilia, settings, &increments);
        let (mut l, mut r) = (vec![0.0f32; CONTROL], vec![0.0f32; CONTROL]);
        bank.render_add(&mut l, &mut r);
        check(&l, &r, &want_l, &want_r, magnitude, &bank, &caecilia);
    }
}

#[test]
fn each_partial_follows_caecilias_kernel_bit_for_bit() {
    for backend in Backend::available() {
        for count in [1, 3, 16, 40] {
            against_caecilia(
                backend,
                count,
                200,
                |l, r, want_l, want_r, _, bank, caecilia| {
                    let what = format!("{} count {count}", backend.name());
                    // Every phasor and gain, bit for bit, block after block.
                    for (i, p) in caecilia.iter().enumerate() {
                        let (x, y) = bank.phasor(i);
                        assert_eq!(x.to_bits(), p.osc.0.to_bits(), "cos {i} {what}");
                        assert_eq!(y.to_bits(), p.osc.1.to_bits(), "sin {i} {what}");
                        assert_eq!(
                            bank.gain(i).to_bits(),
                            p.cur_gain.to_bits(),
                            "gain {i} {what}"
                        );
                    }
                    // A partial alone: its samples, bit for bit, too.
                    if count == 1 {
                        assert_eq!(l, want_l, "{what}");
                        assert_eq!(r, want_r, "{what}");
                    }
                },
            );
        }
    }
}

#[test]
fn the_mix_differs_from_caecilias_only_by_the_order_of_the_sums() {
    // Caecilia adds partial after partial; the bank adds 16 running sums in
    // pairs. The difference is bounded by `2 (n + 5) 2^-24` times the sum
    // of the magnitudes of the terms (`Backend::add_partials`); in
    // practice it is a few roundings of the largest.
    for backend in Backend::available() {
        for count in [2, 15, 16, 17, 64, 200] {
            let bound = 2.0 * (count as f64 + 5.0) * f64::from(f32::EPSILON / 2.0);
            let mut worst = 0.0f64;
            against_caecilia(
                backend,
                count,
                100,
                |l, r, want_l, want_r, magnitude, _, _| {
                    // The phasors are within 1e-4 of the unit circle.
                    let magnitude = magnitude * 1.000_1;
                    for t in 0..CONTROL {
                        for (got, want) in [(l[t], want_l[t]), (r[t], want_r[t])] {
                            let error = f64::from((got - want).abs());
                            assert!(
                                error <= bound * magnitude,
                                "{} count {count}: {error} > {}",
                                backend.name(),
                                bound * magnitude
                            );
                            worst = worst.max(error / magnitude);
                        }
                    }
                },
            );
            // A few roundings of the largest terms.
            assert!(
                worst < 16.0 * f64::from(f32::EPSILON),
                "count {count}: {worst}"
            );
        }
    }
}

/// Frequencies from 0 Hz to Nyquist, for the long runs.
const LONG_RUN_HZ: [f64; 16] = [
    0.0,
    0.5,
    16.351_597,
    27.5,
    55.0,
    110.0,
    261.625_565,
    440.0,
    1_000.123,
    3_520.0,
    5_000.0,
    8_372.018,
    12_345.6,
    20_000.0,
    23_999.0,
    24_000.0,
];

#[test]
fn a_phasor_keeps_its_amplitude_for_a_minute() {
    amplitude_drift(1);
}

#[test]
#[ignore = "ten minutes of audio, a minute in a debug build: run it optimised"]
fn a_phasor_keeps_its_amplitude_for_ten_minutes() {
    amplitude_drift(10);
}

/// Runs the partials of [`LONG_RUN_HZ`] for `minutes` at 48 kHz and checks
/// how far any phasor strays from the unit circle. The output is the same
/// on every backend, so the detected one stands for all.
fn amplitude_drift(minutes: usize) {
    let mut bank = PartialBank::new(LONG_RUN_HZ.len());
    bank.set_sample_rate(RATE);
    bank.set_len(LONG_RUN_HZ.len());
    for (i, &hz) in LONG_RUN_HZ.iter().enumerate() {
        bank.set(
            i,
            Partial {
                freq_hz: hz,
                gain: 1.0 / 16.0,
                pan: 0.0,
            },
        );
        bank.set_phase(i, i as f64 / 16.0 + 0.01);
    }
    let blocks = minutes * 60 * RATE as usize / CONTROL;
    let (mut l, mut r) = ([0.0f32; CONTROL], [0.0f32; CONTROL]);
    let mut worst = 0.0f64;
    let mut worst_output = 0.0f32;
    for block in 0..blocks {
        bank.render_add(&mut l, &mut r);
        // Read at the end of a normalisation period, where a phasor has
        // drifted furthest.
        for i in 0..LONG_RUN_HZ.len() {
            let (x, y) = bank.phasor(i);
            let magnitude = (f64::from(x).powi(2) + f64::from(y).powi(2)).sqrt();
            worst = worst.max((magnitude - 1.0).abs());
        }
        if block + 1 == blocks {
            worst_output = l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        }
        l.fill(0.0);
        r.fill(0.0);
    }
    eprintln!("largest |magnitude - 1| over {minutes} minute(s): {worst:.3e}");
    // The stated bound is 3e-5: 64 rotations, each moving the magnitude by
    // at most 6e-8 for the rotation's own magnitude and 3.6e-7 for its
    // roundings, plus the Newton step's. In practice the roundings cancel
    // and it is a tenth of that.
    assert!(worst < 3e-5, "{worst}");
    // The sum still has its level: 16 partials of 1/16, at most 0.7071 on
    // a side, and the last block is not silent.
    assert!(worst_output > 0.1 && worst_output < 0.71, "{worst_output}");
}

#[test]
fn a_partial_keeps_its_frequency() {
    // Ten seconds of each partial; the phase is read every control block
    // and compared with where an exact oscillator would be.
    let mut bank = PartialBank::new(LONG_RUN_HZ.len());
    bank.set_sample_rate(RATE);
    bank.set_len(LONG_RUN_HZ.len());
    for (i, &hz) in LONG_RUN_HZ.iter().enumerate() {
        bank.set(
            i,
            Partial {
                freq_hz: hz,
                gain: 0.0,
                pan: 0.0,
            },
        );
    }
    let blocks = 10 * RATE as usize / CONTROL;
    let mut error = [0.0f64; LONG_RUN_HZ.len()];
    let (mut l, mut r) = ([0.0f32; CONTROL], [0.0f32; CONTROL]);
    for _ in 0..blocks {
        let before: Vec<(f32, f32)> = (0..LONG_RUN_HZ.len()).map(|i| bank.phasor(i)).collect();
        bank.render_add(&mut l, &mut r);
        for (i, &hz) in LONG_RUN_HZ.iter().enumerate() {
            let (x0, y0) = (f64::from(before[i].0), f64::from(before[i].1));
            let (x1, y1) = bank.phasor(i);
            let (x1, y1) = (f64::from(x1), f64::from(y1));
            // The angle turned, from the product of the new phasor with
            // the conjugate of the old.
            let turned = (y1 * x0 - x1 * y0).atan2(x1 * x0 + y1 * y0);
            let wanted = std::f64::consts::TAU * hz / RATE * CONTROL as f64;
            let mut off = (turned - wanted) % std::f64::consts::TAU;
            if off > std::f64::consts::PI {
                off -= std::f64::consts::TAU;
            } else if off < -std::f64::consts::PI {
                off += std::f64::consts::TAU;
            }
            error[i] += off;
        }
    }
    let seconds = (blocks * CONTROL) as f64 / RATE;
    let mut worst_relative = 0.0f64;
    for (i, &hz) in LONG_RUN_HZ.iter().enumerate() {
        let hz_error = error[i] / (std::f64::consts::TAU * seconds);
        eprintln!("{hz:>10} Hz: off by {hz_error:+.3e} Hz");
        // Within 1 mHz everywhere, and a part in 10^6 above 20 Hz.
        assert!(hz_error.abs() < 1e-3, "{hz} Hz: {hz_error}");
        if hz > 20.0 && hz < RATE / 2.0 {
            worst_relative = worst_relative.max((hz_error / hz).abs());
        }
    }
    eprintln!("worst relative error above 20 Hz: {worst_relative:.3e}");
    assert!(worst_relative < 1e-6, "{worst_relative}");
}

#[test]
fn changing_the_frequency_keeps_the_phase() {
    let mut bank = PartialBank::new(1);
    bank.set_sample_rate(RATE);
    bank.set_len(1);
    bank.set(
        0,
        Partial {
            freq_hz: 1_000.0,
            gain: 1.0,
            pan: -1.0,
        },
    );
    let (mut l, mut r) = (vec![0.0f32; 100], vec![0.0f32; 100]);
    bank.render_add(&mut l, &mut r);
    let before = bank.phasor(0);
    bank.set_frequency(0, 3_000.0);
    assert_eq!(bank.phasor(0), before);
    let (mut l2, mut r2) = (vec![0.0f32; 1], vec![0.0f32; 1]);
    bank.render_add(&mut l2, &mut r2);
    // The first sample after the change is the sine of the old phasor.
    assert!((l2[0] - before.1).abs() < 1e-6);
    // Hard left: nothing on the right.
    assert!(r.iter().chain(&r2).all(|&x| x.abs() < 1e-7));
}
