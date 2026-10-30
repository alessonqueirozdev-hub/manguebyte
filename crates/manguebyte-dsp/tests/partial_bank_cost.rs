//! Measures what a bank of partials costs on each backend. Ignored by
//! default, because the numbers only mean something in an optimised build:
//!
//! ```text
//! cargo test -p manguebyte-dsp --release --test partial_bank_cost -- --ignored --nocapture
//! ```
//!
//! It renders 3000 partials, Caecilia's tutti, for four seconds at 48 kHz
//! in blocks of 256 frames, with every partial given a new rotation and a
//! new gain ramp each 64-frame control block, as Caecilia does. It reports
//! the share of one core that takes and how many partials one core could
//! keep up in real time, for Caecilia's own scalar kernel and for the bank
//! on each backend this processor has; once as one bank of 3000 and once
//! as 200 banks of 15 (Caecilia's 200 voices). Each figure is the best of
//! three runs. Read them as rough figures on a busy machine.

use std::hint::black_box;
use std::time::Instant;

use manguebyte_core::vector::Backend;
use manguebyte_dsp::PartialBank;

const RATE: f64 = 48_000.0;
const BLOCK: usize = 256;
const CONTROL: usize = 64;
const SECONDS: f64 = 4.0;
const PARTIALS: usize = 3_000;

/// Caecilia 2.0's partial kernel (`caecilia-synth/src/math.rs`,
/// Apache-2.0, by the same author), the baseline.
#[allow(clippy::too_many_arguments)]
#[inline]
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

/// Control blocks before the settings repeat.
const VARIANTS: usize = 16;

/// The rotation and target gain of each partial in each of [`VARIANTS`]
/// control blocks: a slow glide, worked out beforehand so that the timing
/// is of the rendering and not of the sines and cosines a caller would
/// work out anyway.
fn settings() -> Vec<Vec<(f32, f32, f32)>> {
    (0..VARIANTS)
        .map(|k| {
            (0..PARTIALS)
                .map(|i| {
                    let hz = 30.0 * (1 + i % 600) as f64 * (1.0 + 1e-4 * k as f64);
                    let w = std::f64::consts::TAU * hz / RATE;
                    let target = 1e-3 * (1.0 + 0.1 * ((i + k) % 5) as f32);
                    (w.cos() as f32, w.sin() as f32, target)
                })
                .collect()
        })
        .collect()
}

/// Seconds of audio rendered per second of work, inverted: the share of a
/// core the render takes.
fn best_of_three(mut run: impl FnMut() -> f32) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        black_box(run());
        best = best.min(start.elapsed().as_secs_f64() / SECONDS);
    }
    best
}

fn blocks() -> usize {
    (SECONDS * RATE) as usize / BLOCK
}

/// Caecilia's loop over `banks` voices of `per_bank` partials each.
fn caecilia(table: &[Vec<(f32, f32, f32)>], banks: usize, per_bank: usize) -> f32 {
    #[derive(Clone, Copy)]
    struct P {
        osc: (f32, f32),
        gain: f32,
        cos_inc: f32,
        sin_inc: f32,
        gain_inc: f32,
    }
    let mut voices = vec![
        vec![
            P {
                osc: (1.0, 0.0),
                gain: 0.0,
                cos_inc: 1.0,
                sin_inc: 0.0,
                gain_inc: 0.0,
            };
            per_bank
        ];
        banks
    ];
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let (mut chunk_l, mut chunk_r) = (vec![0.0f32; CONTROL], vec![0.0f32; CONTROL]);
    let mut control = 0;
    for _ in 0..blocks() {
        left.fill(0.0);
        right.fill(0.0);
        for at in (0..BLOCK).step_by(CONTROL) {
            let settings = &table[control % VARIANTS];
            for (v, voice) in voices.iter_mut().enumerate() {
                for (j, p) in voice.iter_mut().enumerate() {
                    let (c, s, target) = settings[v * per_bank + j];
                    p.cos_inc = c;
                    p.sin_inc = s;
                    let mag = p.osc.0 * p.osc.0 + p.osc.1 * p.osc.1;
                    let renorm = 1.5 - 0.5 * mag;
                    p.osc.0 *= renorm;
                    p.osc.1 *= renorm;
                    p.gain_inc = (target - p.gain) * (1.0 / CONTROL as f32);
                }
                chunk_l.fill(0.0);
                chunk_r.fill(0.0);
                for p in voice.iter_mut() {
                    partial_accumulate(
                        &mut chunk_l,
                        &mut chunk_r,
                        &mut p.osc,
                        p.cos_inc,
                        p.sin_inc,
                        &mut p.gain,
                        p.gain_inc,
                        0.7,
                        0.7,
                    );
                }
                for k in 0..CONTROL {
                    left[at + k] += chunk_l[k];
                    right[at + k] += chunk_r[k];
                }
            }
            control += 1;
        }
    }
    left[7] + right[100]
}

/// The bank on `backend`, as `banks` banks of `per_bank` partials each.
fn bank(table: &[Vec<(f32, f32, f32)>], backend: Backend, banks: usize, per_bank: usize) -> f32 {
    let mut voices: Vec<PartialBank> = (0..banks)
        .map(|_| {
            let mut bank = PartialBank::new(per_bank);
            bank.set_backend(backend);
            bank.set_sample_rate(RATE);
            bank.set_len(per_bank);
            for i in 0..per_bank {
                bank.set_pan_gains(i, 0.7, 0.7);
            }
            bank
        })
        .collect();
    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    let mut control = 0;
    for _ in 0..blocks() {
        left.fill(0.0);
        right.fill(0.0);
        for at in (0..BLOCK).step_by(CONTROL) {
            let settings = &table[control % VARIANTS];
            for (v, bank) in voices.iter_mut().enumerate() {
                for i in 0..per_bank {
                    let (c, s, target) = settings[v * per_bank + i];
                    bank.set_rotation(i, c, s);
                    bank.ramp_to(i, target, CONTROL);
                }
                bank.render_add(&mut left[at..at + CONTROL], &mut right[at..at + CONTROL]);
            }
            control += 1;
        }
    }
    left[7] + right[100]
}

#[test]
#[ignore = "a measurement, not a check; run it in release mode"]
fn partials_per_core_on_each_backend() {
    let table = settings();
    for (banks, per_bank) in [(1, PARTIALS), (200, PARTIALS / 200)] {
        eprintln!(
            "{PARTIALS} partials as {banks} bank(s) of {per_bank}, {SECONDS} s at {RATE} Hz, \
             blocks of {BLOCK}, settings every {CONTROL} frames:"
        );
        let baseline = best_of_three(|| caecilia(&table, banks, per_bank));
        let report = |name: &str, share: f64| {
            eprintln!(
                "  {name:<18} {share:>6.3} of a core, {:>7.0} partials per core, {:>5.2}x Caecilia's kernel",
                PARTIALS as f64 / share,
                baseline / share
            );
        };
        report("caecilia scalar", baseline);
        for backend in Backend::available() {
            let share = best_of_three(|| bank(&table, backend, banks, per_bank));
            report(&format!("bank {}", backend.name()), share);
        }
    }
}
