//! Measures what voicing costs a bank of partials.
//!
//! ```text
//! cargo run --release -p manguebyte-dsp --example partial_voicing_speed
//! ```
//!
//! 2 000 partials at 48 kHz in blocks of 128 frames, on each backend this
//! processor has: once as one bank, once as 125 banks of 16, as
//! the voices of an organ are. Plain, with air, with air and jitter, with
//! air and attacks (each partial's attack restarted every 100 ms, so that
//! every partial is always in one, which no player would ask for), and with
//! all three. Each case also has a version with a new rotation and gain ramp
//! every 64 frames, as Caecilia's wind and drift set them. The cases take turns, half a second of audio
//! each, ten rounds, and each figure is its best round: on a busy machine
//! the turns keep one case's bad moment from landing on it alone. Pin it to
//! one logical processor if you want figures that do not wander, with
//! `start /affinity` or `taskset`.

use std::hint::black_box;
use std::time::Instant;

use manguebyte_core::vector::Backend;
use manguebyte_dsp::{PartialAir, PartialAttack, PartialBank, WindJitter};

const RATE: f64 = 48_000.0;
const BLOCK: usize = 128;
const CONTROL: usize = 64;
const ROUND_SECONDS: f64 = 0.5;
const ROUNDS: usize = 10;
const PARTIALS: usize = 2_000;

/// What a case turns on.
#[derive(Clone, Copy, PartialEq)]
enum Voicing {
    Plain,
    Air,
    AirJitter,
    AirAttack,
    Everything,
}

impl Voicing {
    fn name(self) -> &'static str {
        match self {
            Voicing::Plain => "plain",
            Voicing::Air => "air",
            Voicing::AirJitter => "air and jitter",
            Voicing::AirAttack => "air and attacks",
            Voicing::Everything => "air, attacks and jitter",
        }
    }
}

/// What a harmonic of a rank at some note sets.
fn rotation(i: usize) -> (f32, f32) {
    let w = std::f64::consts::TAU * 30.0 * (1 + i % 600) as f64 / RATE;
    (w.cos() as f32, w.sin() as f32)
}

fn attack(i: usize) -> PartialAttack {
    let step = (i % 16) as u32;
    PartialAttack {
        delay_frames: 16 * (15 - step),
        rise_frames: 1_440 - 64 * step,
        overshoot: 0.2,
        settle_frames: 2_400,
        breath: 0.2,
        breath_frames: 1_920,
        breath_bandwidth_hz: 200.0,
        pitch_cents: -6.0,
        pitch_frames: 1_920,
    }
}

/// Banks of `per_bank` partials, as many as 2 000 partials take.
fn banks(backend: Backend, per_bank: usize, voicing: Voicing) -> Vec<PartialBank> {
    let count = PARTIALS.div_ceil(per_bank);
    (0..count)
        .map(|b| {
            let mut bank = PartialBank::new(per_bank);
            bank.set_backend(backend);
            bank.set_sample_rate(RATE);
            bank.set_len(per_bank);
            bank.set_voicing_seed(b as u64);
            for i in 0..per_bank {
                let (c, s) = rotation(b * per_bank + i);
                bank.set_rotation(i, c, s);
                bank.set_gain(i, 1e-3);
                bank.set_pan_gains(i, 0.7, 0.7);
                if voicing != Voicing::Plain {
                    bank.set_air(
                        i,
                        PartialAir {
                            depth: 0.02,
                            bandwidth_hz: 20.0 + (i % 32) as f32,
                        },
                    );
                }
            }
            if voicing == Voicing::Everything || voicing == Voicing::AirJitter {
                bank.set_wind_jitter(WindJitter {
                    cents: 0.5,
                    db: 0.08,
                    rate_hz: 4.0,
                });
            }
            bank
        })
        .collect()
}

/// The rotations and gain targets of a control block, worked out beforehand.
fn settings() -> Vec<Vec<(f32, f32, f32)>> {
    (0..16)
        .map(|k| {
            (0..PARTIALS.div_ceil(16) * 16)
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

type Case<'a> = (String, Box<dyn FnMut(usize) + 'a>);

fn main() {
    let table = settings();
    let table = &table;
    let backends: Vec<Backend> = Backend::available().collect();
    let mut cases: Vec<Case<'_>> = Vec::new();
    for &backend in &backends {
        for per_bank in [PARTIALS, 16] {
            for voicing in [
                Voicing::Plain,
                Voicing::Air,
                Voicing::AirJitter,
                Voicing::AirAttack,
                Voicing::Everything,
            ] {
                for controlled in [false, true] {
                    let mut voices = banks(backend, per_bank, voicing);
                    let (mut left, mut right) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
                    let shape = if per_bank == PARTIALS {
                        "1 bank".to_string()
                    } else {
                        format!("{} banks of {per_bank}", voices.len())
                    };
                    let what = if controlled { ", new settings" } else { "" };
                    cases.push((
                        format!("{}: {shape}, {}{what}", backend.name(), voicing.name()),
                        Box::new(move |n| {
                            left.fill(0.0);
                            right.fill(0.0);
                            // A 100 ms cycle of attacks, a bank's own turn in it.
                            let cycle = (0.1 * RATE) as usize / BLOCK;
                            if voicing == Voicing::AirAttack || voicing == Voicing::Everything {
                                for (b, bank) in voices.iter_mut().enumerate() {
                                    if (n + b) % cycle == 0 {
                                        for i in 0..bank.len() {
                                            bank.start_attack(i, &attack(i));
                                        }
                                    }
                                }
                            }
                            if controlled {
                                for at in (0..BLOCK).step_by(CONTROL) {
                                    let k = (n * (BLOCK / CONTROL) + at / CONTROL) % table.len();
                                    let settings = &table[k];
                                    for (b, bank) in voices.iter_mut().enumerate() {
                                        for i in 0..bank.len() {
                                            let (c, s, g) = settings[b * per_bank + i];
                                            bank.set_rotation(i, c, s);
                                            bank.ramp_to(i, g, CONTROL);
                                        }
                                        bank.render_add(
                                            &mut left[at..at + CONTROL],
                                            &mut right[at..at + CONTROL],
                                        );
                                    }
                                }
                            } else {
                                for bank in voices.iter_mut() {
                                    bank.render_add(&mut left, &mut right);
                                }
                            }
                            black_box((&left, &right));
                        }),
                    ));
                }
            }
        }
    }

    let blocks = (ROUND_SECONDS * RATE) as usize / BLOCK;
    let mut best = vec![f64::INFINITY; cases.len()];
    for _ in 0..ROUNDS {
        for (case, best) in cases.iter_mut().zip(&mut best) {
            let start = Instant::now();
            for n in 0..blocks {
                (case.1)(n);
            }
            *best = best.min(start.elapsed().as_secs_f64() / blocks as f64);
        }
    }

    let deadline = BLOCK as f64 / RATE;
    println!(
        "| {PARTIALS} partials, {BLOCK} frames at 48 kHz | per block | of a core | against plain |"
    );
    println!("|---|---:|---:|---:|");
    // Each plain case stands at the head of its row of four.
    let mut plain = [1.0f64; 2];
    for (k, ((name, _), seconds)) in cases.iter().zip(&best).enumerate() {
        let group = k % 10;
        if group < 2 {
            plain[group] = *seconds;
        }
        println!(
            "| {name} | {:.1} µs | {:.3} | {:.2} |",
            seconds * 1e6,
            seconds / deadline,
            seconds / plain[group % 2]
        );
    }
}
