//! The partial bank's voicing: air, attacks and wind jitter. That a bank
//! which sets none of them renders what it always did, bit for bit; that
//! what it renders with them does not depend on the backend or the blocks;
//! and, measured on the output, that the sound is what the settings say.
//!
//! Three tricks make the measurements clean. A partial at 0 Hz with its
//! phasor at `(0, 1)` outputs its gain times the real part of its modulator
//! and nothing else, so its output is its envelope; with its phasor at
//! `(1, 0)` it outputs the imaginary part. A bank with air has the same
//! phasors and gains, bit for bit, as one without, so the difference
//! between their outputs is the skirt alone, with no line to leak into its
//! spectrum. And the phasors can be read: the angle one turned between two
//! reads is its frequency, without any analysis of the output.

use manguebyte_core::vector::Backend;
use manguebyte_dsp::fft::{Complex, RealFft};
use manguebyte_dsp::{Partial, PartialAir, PartialAttack, PartialBank, PipeVoicing, WindJitter};

const RATE: f64 = 48_000.0;
const UPDATE: usize = PartialBank::VOICE_FRAMES;

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

/// FNV-1a over the bits of the samples, to pin a render in one number.
fn fingerprint(samples: &[f32]) -> u64 {
    let mut hash = 0xCBF2_9CE4_8422_2325u64;
    for sample in samples {
        for byte in sample.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01B3);
        }
    }
    hash
}

fn same_bits(a: &[f32], b: &[f32]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.to_bits() == y.to_bits())
}

fn render(bank: &mut PartialBank, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    bank.render_add(&mut left, &mut right);
    (left, right)
}

fn left_only(bank: &mut PartialBank, frames: usize) -> Vec<f32> {
    render(bank, frames).0
}

/// A bank of `count` partials set up with nothing but stores and `f32`
/// products, so that the numbers do not depend on a platform's sines:
/// rotations that are powers of one fixed rotation, phasors that are powers
/// of another, gains and pans from the index.
fn pinned_bank(backend: Backend, count: usize) -> PartialBank {
    let mut bank = PartialBank::new(count);
    bank.set_backend(backend);
    bank.set_sample_rate(RATE);
    bank.set_len(count);
    // 261.6 Hz or so at 48 kHz, and an angle of about 0.7 radians.
    let (base_c, base_s) = (0.999_414_f32, 0.034_243_7_f32);
    let (turn_c, turn_s) = (0.764_842_2_f32, 0.644_217_7_f32);
    let (mut c, mut s) = (base_c, base_s);
    let (mut x, mut y) = (1.0f32, 0.0f32);
    for i in 0..count {
        bank.set_rotation(i, c, s);
        bank.set_phasor(i, x, y);
        bank.set_gain(i, 0.25 / (i + 1) as f32);
        let side = (i % 5) as f32 * 0.2;
        bank.set_pan_gains(i, 1.0 - side, 0.2 + side);
        (c, s) = (c * base_c - s * base_s, c * base_s + s * base_c);
        (x, y) = (x * turn_c - y * turn_s, x * turn_s + y * turn_c);
    }
    bank
}

/// Plays a pinned bank through a few ramps in uneven blocks.
fn pinned_render(bank: &mut PartialBank) -> Vec<f32> {
    let count = bank.len();
    let mut out = Vec::new();
    for (round, frames) in [64usize, 1, 500, 33, 2_000, 64, 7].into_iter().enumerate() {
        for i in 0..count {
            let target = (0.5 + 0.1 * (round % 3) as f32) / (i + 1) as f32;
            bank.ramp_to(i, target, 40 + i % 90);
        }
        let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
        bank.render_add(&mut left, &mut right);
        out.extend_from_slice(&left);
        out.extend_from_slice(&right);
    }
    out
}

/// What the bank rendered before voicing was added, as measured on the
/// commit before it (`cd1458e`), for 3 partials (added one after another),
/// 23 (one group and the rest) and 64 (groups only).
const PINNED: [(usize, u64); 3] = [
    (3, 0x9058_9B2C_8A53_4834),
    (23, 0x8041_5A64_8591_89BF),
    (64, 0x88E1_3413_E20C_B838),
];

#[test]
fn a_bank_that_sets_no_voicing_renders_what_it_did_before_there_was_any() {
    for backend in Backend::available() {
        for (count, want) in PINNED {
            let mut bank = pinned_bank(backend, count);
            let got = fingerprint(&pinned_render(&mut bank));
            assert_eq!(got, want, "{} count {count}: {got:#018X}", backend.name());
        }
    }
}

#[test]
fn voicing_set_to_nothing_leaves_the_bank_plain() {
    for backend in Backend::available() {
        for (count, want) in PINNED {
            let mut bank = pinned_bank(backend, count);
            for i in 0..count {
                bank.set_air(
                    i,
                    PartialAir {
                        depth: 0.0,
                        bandwidth_hz: 80.0,
                    },
                );
                bank.set_air_depth(i, 0.0);
                bank.start_attack(i, &PartialAttack::default());
                // A level with no time to act, a breath with no length, a
                // pitch with no length: nothing.
                bank.start_attack(
                    i,
                    &PartialAttack {
                        overshoot: 0.5,
                        breath: 0.5,
                        pitch_cents: 20.0,
                        ..PartialAttack::default()
                    },
                );
            }
            bank.set_wind_jitter(WindJitter::OFF);
            bank.set_voicing_seed(77);
            assert!(!bank.is_voiced());
            let got = fingerprint(&pinned_render(&mut bank));
            assert_eq!(got, want, "{} count {count}", backend.name());
            assert!(!bank.is_voiced());
        }
    }
}

/// A partial at `hz`, hard left, under a gain of `gain`.
fn lone_partial(backend: Backend, rate: f64, hz: f64, gain: f32) -> PartialBank {
    let mut bank = PartialBank::new(1);
    bank.set_backend(backend);
    bank.set_sample_rate(rate);
    bank.set_len(1);
    bank.set(
        0,
        Partial {
            freq_hz: hz,
            gain,
            pan: -1.0,
        },
    );
    bank
}

/// A partial at 0 Hz, hard left, whose output is the real part of its
/// modulator.
fn envelope_partial() -> PartialBank {
    let mut bank = PartialBank::new(1);
    bank.set_sample_rate(RATE);
    bank.set_len(1);
    bank.set_rotation(0, 1.0, 0.0);
    bank.set_phasor(0, 0.0, 1.0);
    bank.set_gain(0, 1.0);
    bank.set_pan_gains(0, 1.0, 0.0);
    bank
}

/// One event in a played script, applied on a given frame.
#[derive(Clone, Copy)]
enum Event {
    Air(usize, PartialAir),
    Depth(usize, f32),
    Attack(usize, PartialAttack),
    Jitter(WindJitter),
    Frequency(usize, f64),
    Rotation(usize, f32, f32),
    Ramp(usize, f32, usize),
    Pan(usize, f32),
    Seed(u64),
}

fn apply(bank: &mut PartialBank, event: Event) {
    match event {
        Event::Air(i, air) => bank.set_air(i, air),
        Event::Depth(i, depth) => bank.set_air_depth(i, depth),
        Event::Attack(i, attack) => bank.start_attack(i, &attack),
        Event::Jitter(jitter) => bank.set_wind_jitter(jitter),
        Event::Frequency(i, hz) => bank.set_frequency(i, hz),
        Event::Rotation(i, c, s) => bank.set_rotation(i, c, s),
        Event::Ramp(i, target, frames) => bank.ramp_to(i, target, frames),
        Event::Pan(i, pan) => bank.set_pan(i, pan),
        Event::Seed(seed) => bank.set_voicing_seed(seed),
    }
}

/// A script with everything in it, on frames that fall anywhere against
/// the bank's own updates.
fn script(seed: u64, count: usize, frames: usize) -> Vec<(usize, Event)> {
    let mut rng = Rng::new(seed);
    let mut events = Vec::new();
    let mut at = |frame: usize, event: Event| events.push((frame, event));
    for i in 0..count {
        at(
            rng.below(400),
            Event::Air(
                i,
                PartialAir {
                    depth: (rng.unit() * 0.1) as f32,
                    bandwidth_hz: (5.0 + rng.unit() * 300.0) as f32,
                },
            ),
        );
    }
    at(rng.below(800), Event::Seed(seed + 3));
    at(
        rng.below(800),
        Event::Jitter(WindJitter {
            cents: 3.0,
            db: 0.5,
            rate_hz: 12.0,
        }),
    );
    let mut frame = 0;
    while frame < frames {
        for _ in 0..1 + count / 4 {
            let i = rng.below(count);
            let attack = PartialAttack {
                delay_frames: rng.below(200) as u32,
                rise_frames: rng.below(1_000) as u32,
                overshoot: (rng.unit() * 0.5) as f32,
                settle_frames: rng.below(800) as u32,
                breath: (rng.unit() * 0.5) as f32,
                breath_frames: rng.below(1_500) as u32,
                breath_bandwidth_hz: (rng.unit() * 700.0) as f32,
                pitch_cents: ((rng.unit() - 0.5) * 60.0) as f32,
                pitch_frames: rng.below(1_500) as u32,
            };
            at(frame + rng.below(300), Event::Attack(i, attack));
            at(
                frame + rng.below(300),
                Event::Depth(i, (rng.unit() * 0.2) as f32),
            );
            at(
                frame + rng.below(300),
                Event::Frequency(i, rng.unit() * RATE / 2.0),
            );
            let w = rng.unit() * std::f64::consts::PI;
            at(
                frame + rng.below(300),
                Event::Rotation(i, w.cos() as f32, w.sin() as f32),
            );
            at(
                frame + rng.below(300),
                Event::Ramp(i, (rng.unit() * 2.0 - 1.0) as f32, 1 + rng.below(400)),
            );
            at(frame + rng.below(300), Event::Pan(i, rng.unit() as f32));
        }
        frame += 300;
    }
    at(
        frames / 2 + 11,
        Event::Jitter(WindJitter {
            cents: 0.0,
            db: 0.0,
            rate_hz: 2.0,
        }),
    );
    events.sort_by_key(|&(frame, _)| frame);
    events
}

/// Plays `script` on a bank, handing it blocks of the sizes `sizes` gives,
/// each cut where an event falls.
fn play(
    mut bank: PartialBank,
    script: &[(usize, Event)],
    frames: usize,
    mut sizes: impl FnMut() -> usize,
) -> (Vec<f32>, Vec<f32>, PartialBank) {
    let (mut left, mut right) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    let mut next = 0;
    let mut at = 0;
    while at < frames {
        while next < script.len() && script[next].0 <= at {
            apply(&mut bank, script[next].1);
            next += 1;
        }
        let mut end = (at + sizes()).min(frames);
        if next < script.len() {
            end = end.min(script[next].0);
        }
        bank.render_add(&mut left[at..end], &mut right[at..end]);
        at = end;
    }
    (left, right, bank)
}

#[test]
fn voicing_is_the_same_bit_for_bit_on_every_backend_whatever_the_blocks() {
    const FRAMES: usize = 6_000;
    let backends: Vec<Backend> = Backend::available().collect();
    for (seed, count) in [1usize, 7, 16, 23, 64, 100].into_iter().enumerate() {
        let script = script(seed as u64 + 1, count, FRAMES);
        let start = |backend: Backend| {
            let mut bank = PartialBank::new(count);
            bank.set_backend(backend);
            bank.set_sample_rate(RATE);
            bank.set_len(count);
            for i in 0..count {
                bank.set(
                    i,
                    Partial {
                        freq_hz: 110.0 * (i + 1) as f64,
                        gain: 0.1,
                        pan: 0.0,
                    },
                );
            }
            bank
        };
        let (want_l, want_r, want_bank) = play(start(Backend::Scalar), &script, FRAMES, || 64);
        assert!(want_l.iter().any(|&x| x != 0.0), "count {count}: silent");
        assert!(want_l.iter().chain(&want_r).all(|x| x.is_finite()));
        for &backend in &backends {
            for sizes in 0..4 {
                let mut rng = Rng::new(sizes as u64 + 10);
                let next = move || match sizes {
                    0 => 64,
                    1 => 1,
                    2 => 4_096,
                    _ => 1 + rng.below(4_096),
                };
                let (l, r, got_bank) = play(start(backend), &script, FRAMES, next);
                let what = format!("{} count {count} sizes {sizes}", backend.name());
                assert!(same_bits(&l, &want_l), "left {what}");
                assert!(same_bits(&r, &want_r), "right {what}");
                for i in 0..count {
                    assert_eq!(got_bank.phasor(i), want_bank.phasor(i), "phasor {i} {what}");
                    assert_eq!(got_bank.gain(i), want_bank.gain(i), "gain {i} {what}");
                }
            }
        }
    }
}

#[test]
fn voiced_renders_are_pinned_too() {
    // With rotations made from f32 products, nothing in the render uses a
    // platform's maths, so this holds on any machine.
    const WANT: [(usize, u64); 3] = [
        (3, 0xCADC_7903_23A3_1869),
        (23, 0x114A_9066_DAFA_8EC0),
        (64, 0xFF93_913A_6575_0E2C),
    ];
    for backend in Backend::available() {
        for (count, want) in WANT {
            let mut bank = pinned_bank(backend, count);
            for i in 0..count {
                bank.set_air(
                    i,
                    PartialAir {
                        depth: 0.03,
                        bandwidth_hz: 40.0 + 10.0 * i as f32,
                    },
                );
                bank.start_attack(
                    i,
                    &PartialAttack {
                        delay_frames: 16 * (count - i) as u32,
                        rise_frames: 480,
                        overshoot: 0.2,
                        settle_frames: 480,
                        breath: 0.2,
                        breath_frames: 1_000,
                        breath_bandwidth_hz: 150.0,
                        pitch_cents: -8.0,
                        pitch_frames: 1_000,
                    },
                );
            }
            bank.set_wind_jitter(WindJitter {
                cents: 2.0,
                db: 0.3,
                rate_hz: 6.0,
            });
            let got = fingerprint(&pinned_render(&mut bank));
            assert_eq!(got, want, "{} count {count}: {got:#018X}", backend.name());
        }
    }
}

#[test]
fn a_bank_goes_back_to_the_plain_kernel_by_itself_with_the_same_bits() {
    let count = 23;
    let mut voiced = pinned_bank(Backend::detect(), count);
    let mut plain = pinned_bank(Backend::detect(), count);
    for i in 0..count {
        voiced.set_air(
            i,
            PartialAir {
                depth: 0.05,
                bandwidth_hz: 50.0,
            },
        );
        voiced.start_attack(
            i,
            &PartialAttack {
                delay_frames: i as u32,
                rise_frames: 300,
                overshoot: 0.2,
                settle_frames: 200,
                breath: 0.1,
                breath_frames: 700,
                ..PartialAttack::default()
            },
        );
    }
    assert!(voiced.is_voiced());
    let frames = 2_000;
    let (noisy, _) = render(&mut voiced, frames);
    let (clean, _) = render(&mut plain, frames);
    assert!(!same_bits(&noisy, &clean));
    assert!((0..count).all(|i| !voiced.is_attacking(i)));
    for i in 0..count {
        voiced.set_air_depth(i, 0.0);
    }
    // Gone over 512 frames, and two updates to bring the modulators home.
    let (_, _) = render(&mut voiced, 512 + 2 * UPDATE);
    let (_, _) = render(&mut plain, 512 + 2 * UPDATE);
    assert!(!voiced.is_voiced());
    let (after_voiced, _) = render(&mut voiced, 1_000);
    let (after_plain, _) = render(&mut plain, 1_000);
    assert!(same_bits(&after_voiced, &after_plain));
}

#[test]
fn air_and_attacks_leave_phasors_and_gains_alone_bit_for_bit() {
    for backend in Backend::available() {
        let count = 23;
        let mut voiced = pinned_bank(backend, count);
        let mut plain = pinned_bank(backend, count);
        for i in 0..count {
            voiced.set_air(
                i,
                PartialAir {
                    depth: 0.1,
                    bandwidth_hz: 70.0,
                },
            );
            voiced.start_attack(
                i,
                &PartialAttack {
                    rise_frames: 200,
                    overshoot: 0.3,
                    settle_frames: 100,
                    ..PartialAttack::default()
                },
            );
        }
        pinned_render(&mut voiced);
        pinned_render(&mut plain);
        for i in 0..count {
            let what = format!("{} partial {i}", backend.name());
            assert_eq!(voiced.phasor(i), plain.phasor(i), "{what}");
            assert_eq!(voiced.gain(i).to_bits(), plain.gain(i).to_bits(), "{what}");
            assert_eq!(voiced.rotation(i), plain.rotation(i), "{what}");
        }
    }
}

#[test]
fn a_seed_repeats_a_render_and_another_seed_does_not() {
    let scene = |seed: u64| {
        let mut bank = lone_partial(Backend::detect(), RATE, 440.0, 0.5);
        bank.set_voicing_seed(seed);
        bank.set_air(
            0,
            PartialAir {
                depth: 0.1,
                bandwidth_hz: 100.0,
            },
        );
        bank.set_wind_jitter(WindJitter {
            cents: 2.0,
            db: 0.2,
            rate_hz: 5.0,
        });
        left_only(&mut bank, 4_800)
    };
    assert!(same_bits(&scene(5), &scene(5)));
    assert!(!same_bits(&scene(5), &scene(6)));
}

#[test]
fn a_partials_noise_is_its_own_whatever_the_others_do() {
    let scene = |others: usize| {
        let mut bank = PartialBank::new(32);
        bank.set_sample_rate(RATE);
        bank.set_len(1 + others);
        bank.set(
            0,
            Partial {
                freq_hz: 500.0,
                gain: 0.5,
                pan: -1.0,
            },
        );
        bank.set_air(
            0,
            PartialAir {
                depth: 0.1,
                bandwidth_hz: 100.0,
            },
        );
        for i in 1..1 + others {
            // On the right, with air and attacks of their own.
            bank.set(
                i,
                Partial {
                    freq_hz: 700.0 * i as f64,
                    gain: 0.5,
                    pan: 1.0,
                },
            );
            bank.set_air(
                i,
                PartialAir {
                    depth: 0.2,
                    bandwidth_hz: 10.0,
                },
            );
            bank.start_attack(
                i,
                &PartialAttack {
                    rise_frames: 900,
                    ..PartialAttack::default()
                },
            );
        }
        left_only(&mut bank, 4_800)
    };
    let alone = scene(0);
    for others in [1, 5, 15, 31] {
        // Exact to the sign of a zero: the left sums only hold this partial.
        for (a, b) in alone.iter().zip(&scene(others)) {
            assert_eq!(a, b, "{others} others");
        }
    }
}

#[test]
fn nonsense_settings_stay_out_of_the_output() {
    let mut bank = lone_partial(Backend::detect(), RATE, 440.0, 0.5);
    bank.set_air(
        0,
        PartialAir {
            depth: f32::NAN,
            bandwidth_hz: f32::NAN,
        },
    );
    bank.set_air(
        0,
        PartialAir {
            depth: f32::INFINITY,
            bandwidth_hz: f32::INFINITY,
        },
    );
    bank.set_air(
        0,
        PartialAir {
            depth: 0.5,
            bandwidth_hz: -3.0,
        },
    );
    bank.set_wind_jitter(WindJitter {
        cents: f32::INFINITY,
        db: f32::NAN,
        rate_hz: f32::NAN,
    });
    bank.start_attack(
        0,
        &PartialAttack {
            delay_frames: u32::MAX,
            rise_frames: u32::MAX,
            overshoot: f32::NAN,
            settle_frames: u32::MAX,
            breath: f32::INFINITY,
            breath_frames: u32::MAX,
            breath_bandwidth_hz: f32::NAN,
            pitch_cents: f32::NEG_INFINITY,
            pitch_frames: u32::MAX,
        },
    );
    let (l, r) = render(&mut bank, 2_000);
    assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    bank.start_attack(
        0,
        &PartialAttack {
            rise_frames: 10,
            breath: 1.0,
            breath_frames: 100,
            breath_bandwidth_hz: 1e9,
            pitch_cents: 1e9,
            pitch_frames: 100,
            ..PartialAttack::default()
        },
    );
    let (l, r) = render(&mut bank, 2_000);
    assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    assert!(l.iter().any(|&x| x != 0.0));
}

/// The average power spectrum of `signal` by Welch's method: Hann windows of
/// `size`, half overlapping, as power per bin such that the bins of a whole
/// spectrum sum to the signal's mean power.
fn spectrum(signal: &[f32], size: usize) -> Vec<f64> {
    let window: Vec<f64> = (0..size)
        .map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / size as f64).cos())
        .collect();
    let energy: f64 = window.iter().map(|w| w * w).sum();
    let mut fft = RealFft::<f64>::new(size);
    let mut bins = vec![Complex::<f64>::ZERO; fft.spectrum_len()];
    let mut input = vec![0.0f64; size];
    let mut power = vec![0.0f64; fft.spectrum_len()];
    let mut windows = 0usize;
    let mut at = 0;
    while at + size <= signal.len() {
        for n in 0..size {
            input[n] = f64::from(signal[at + n]) * window[n];
        }
        fft.forward(&input, &mut bins);
        for (p, bin) in power.iter_mut().zip(&bins) {
            *p += bin.re * bin.re + bin.im * bin.im;
        }
        windows += 1;
        at += size / 2;
    }
    // Each positive bin stands for its negative twin as well.
    let scale = 2.0 / (windows as f64 * energy * size as f64);
    power.iter_mut().for_each(|p| *p *= scale);
    power
}

fn power_of(signal: &[f32]) -> f64 {
    signal
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        / signal.len() as f64
}

fn db(ratio: f64) -> f64 {
    10.0 * ratio.log10()
}

/// The skirt a partial grows: what it adds to the same partial without air,
/// after `settle` frames for the noise to get going.
fn skirt(rate: f64, hz: f64, air: PartialAir, frames: usize) -> (Vec<f32>, Vec<f32>) {
    let mut plain = lone_partial(Backend::detect(), rate, hz, 1.0);
    let mut airy = plain.clone();
    airy.set_air(0, air);
    let settle = (rate as usize / 2).max(4_096);
    render(&mut plain, settle);
    render(&mut airy, settle);
    let clean = left_only(&mut plain, frames);
    let noisy = left_only(&mut airy, frames);
    let added = noisy.iter().zip(&clean).map(|(a, b)| a - b).collect();
    (added, clean)
}

/// The distance from `hz` within which half of a skirt's power lies.
fn half_power_width(power: &[f64], bin_hz: f64, hz: f64) -> f64 {
    let centre = (hz / bin_hz).round() as usize;
    let half: f64 = power.iter().sum::<f64>() / 2.0;
    let mut sum = power[centre];
    for away in 1..power.len() {
        if sum >= half {
            return (away as f64 - 0.5) * bin_hz;
        }
        sum += power.get(centre + away).copied().unwrap_or(0.0);
        if away <= centre {
            sum += power[centre - away];
        }
    }
    power.len() as f64 * bin_hz
}

/// The distance from `hz` at which a skirt's power is half its peak, from
/// bands an eighth of `bandwidth` wide, which smooth out the noise of the
/// estimate.
fn half_peak_width(power: &[f64], bin_hz: f64, hz: f64, bandwidth: f64) -> f64 {
    let wide = (bandwidth / 8.0).max(3.0 * bin_hz);
    let around = |away: f64| {
        0.5 * (band(
            power,
            bin_hz,
            hz + away - wide / 2.0,
            hz + away + wide / 2.0,
        ) + band(
            power,
            bin_hz,
            hz - away - wide / 2.0,
            hz - away + wide / 2.0,
        ))
    };
    let top = band(power, bin_hz, hz - wide / 2.0, hz + wide / 2.0);
    let mut away = wide;
    while around(away) > 0.5 * top {
        away += bin_hz;
    }
    away
}

#[test]
fn the_skirt_has_the_level_and_the_width_the_settings_say() {
    // Level within 0.5 dB. Half the peak's power at half the bandwidth from
    // the partial, within 20 % up to a quarter of the update rate (375 Hz at
    // 48 kHz) and 30 % at the top of the range, where the straight lines
    // between updates narrow it. Half of the skirt's whole power that close
    // in as well, as long as the bandwidth is under a twentieth of the update
    // rate: wider than that, what the updates cannot carry is missing from
    // the far tails. A narrow
    // band is few independent values: 20 Hz over 21 s is some 400, whose
    // power is known to 5 %, or 0.2 dB.
    const SIZE: usize = 1 << 15;
    let frames = 1 << 20;
    let bin_hz = RATE / SIZE as f64;
    for (depth, bandwidth_hz) in [
        (0.02f32, 20.0f32),
        (0.1, 20.0),
        (0.02, 60.0),
        (0.1, 200.0),
        (0.05, 375.0),
        (0.05, 750.0),
    ] {
        let hz = 6_000.0;
        let (added, clean) = skirt(
            RATE,
            hz,
            PartialAir {
                depth,
                bandwidth_hz,
            },
            frames,
        );
        let level = db(power_of(&added) / power_of(&clean));
        let want = db(f64::from(depth).powi(2));
        let power = spectrum(&added, SIZE);
        let half = f64::from(bandwidth_hz) / 2.0;
        let peak_ratio = half_peak_width(&power, bin_hz, hz, f64::from(bandwidth_hz)) / half;
        let whole_ratio = half_power_width(&power, bin_hz, hz) / half;
        eprintln!(
            "depth {depth} bandwidth {bandwidth_hz}: level {level:.2} dB against {want:.2} dB, \
             half the peak at {peak_ratio:.2} of half the bandwidth, half the power at \
             {whole_ratio:.2}"
        );
        assert!((level - want).abs() < 0.5, "{level} against {want}");
        let tolerance = if bandwidth_hz > 375.0 { 0.3 } else { 0.2 };
        assert!(
            (peak_ratio - 1.0).abs() < tolerance,
            "{bandwidth_hz}: {peak_ratio}"
        );
        if bandwidth_hz <= 75.0 {
            assert!(
                (whole_ratio - 1.0).abs() < 0.2,
                "{bandwidth_hz}: {whole_ratio}"
            );
        }
    }
}

#[test]
fn the_skirt_keeps_its_level_and_width_at_other_sample_rates() {
    // The update counts frames, so its rate moves with the sample rate.
    for (rate, bandwidth_hz) in [
        (8_000.0f64, 30.0f32),
        (44_100.0, 50.0),
        (96_000.0, 80.0),
        (384_000.0, 200.0),
    ] {
        let hz = rate / 16.0;
        let depth = 0.05f32;
        let (added, clean) = skirt(
            rate,
            hz,
            PartialAir {
                depth,
                bandwidth_hz,
            },
            1 << 19,
        );
        let level = db(power_of(&added) / power_of(&clean));
        let want = db(f64::from(depth).powi(2));
        // A wide enough window for the narrowest of them.
        let size = ((rate / 2.0) as usize).next_power_of_two();
        let width = half_power_width(&spectrum(&added, size), rate / size as f64, hz);
        let ratio = width / f64::from(bandwidth_hz / 2.0);
        eprintln!("{rate} Hz: level {level:.2} dB against {want:.2}, width ratio {ratio:.2}");
        assert!((level - want).abs() < 1.0, "{rate}: {level} against {want}");
        assert!((ratio - 1.0).abs() < 0.25, "{rate}: {ratio}");
    }
}

/// The mean power of the bins from `from_hz` to `to_hz`.
fn band(power: &[f64], bin_hz: f64, from_hz: f64, to_hz: f64) -> f64 {
    let (from, to) = ((from_hz / bin_hz) as usize, (to_hz / bin_hz) as usize);
    power[from..=to].iter().sum::<f64>() / (to - from + 1) as f64
}

#[test]
fn the_skirt_falls_as_a_resonance_does_and_the_updates_leave_no_images() {
    const SIZE: usize = 1 << 15;
    let bin_hz = RATE / SIZE as f64;
    let (hz, bandwidth) = (6_000.0, 40.0f64);
    let (added, _) = skirt(
        RATE,
        hz,
        PartialAir {
            depth: 0.1,
            bandwidth_hz: bandwidth as f32,
        },
        1 << 20,
    );
    let power = spectrum(&added, SIZE);
    let at = |away: f64| {
        let wide = 0.1 * away.max(10.0);
        0.5 * (band(&power, bin_hz, hz + away - wide, hz + away + wide)
            + band(&power, bin_hz, hz - away - wide, hz - away + wide))
    };
    // A resonance: 1 / (1 + (2 f / B)²), so 3 dB down at B / 2 and 9.3 dB
    // more at 2 B, then 6 dB an octave.
    let top = band(&power, bin_hz, hz - 5.0, hz + 5.0);
    let corner = db(at(bandwidth / 2.0) / top);
    let further = db(at(2.0 * bandwidth) / at(bandwidth / 2.0));
    let octave = db(at(8.0 * bandwidth) / at(4.0 * bandwidth));
    eprintln!(
        "3 dB point {corner:.1} dB, at 2 B {further:.1} dB more, an octave out {octave:.1} dB"
    );
    assert!((corner + 3.0).abs() < 1.0, "{corner}");
    assert!((further + 9.3).abs() < 1.5, "{further}");
    assert!((octave + 6.0).abs() < 1.5, "{octave}");
    // The updates, at 1 500 Hz: the noise is a sequence at that rate, so its
    // spectrum repeats there, and the straight lines between its values take
    // those copies far down: in theory 2 sinc² of the distance, some 80 dB
    // for a skirt this narrow against the peak.
    let update_hz = RATE / UPDATE as f64;
    let below = db(at(update_hz) / top);
    eprintln!("the first image {below:.1} dB below the skirt's peak");
    assert!(below < -55.0, "{below}");
}

#[test]
fn air_leaves_the_partials_own_level_where_it_was() {
    // The noise has a mean of zero, so the line stays: what the output has
    // in common with the partial without air is the partial, to 0.02 dB for
    // a skirt of -20 dB, and the total power is one part in depth² more.
    for depth in [0.01f32, 0.03, 0.1] {
        let (added, clean) = skirt(
            RATE,
            1_000.0,
            PartialAir {
                depth,
                bandwidth_hz: 100.0,
            },
            1 << 20,
        );
        let common: f64 = added
            .iter()
            .zip(&clean)
            .map(|(a, b)| f64::from(*a) * f64::from(*b))
            .sum::<f64>()
            / clean.len() as f64;
        let line = 1.0 + common / power_of(&clean);
        let line_db = 20.0 * line.log10();
        eprintln!("depth {depth}: the line moved {line_db:+.4} dB");
        assert!(line_db.abs() < 0.02, "{depth}: {line_db}");
    }
}

/// The exact level of an attack without breath, `frames` frames from its
/// start.
fn exact_level(attack: &PartialAttack, frames: usize) -> f64 {
    let smooth = |p: f64| p * p * (3.0 - 2.0 * p);
    let peak = 1.0 + f64::from(attack.overshoot);
    let (delay, rise, settle) = (
        attack.delay_frames as usize,
        attack.rise_frames as usize,
        attack.settle_frames as usize,
    );
    if frames <= delay {
        0.0
    } else if frames - delay < rise {
        peak * smooth((frames - delay) as f64 / rise as f64)
    } else if frames - delay - rise < settle {
        1.0 + (peak - 1.0) * (1.0 - smooth((frames - delay - rise) as f64 / settle as f64))
    } else {
        1.0
    }
}

#[test]
fn an_attack_follows_its_delay_rise_overshoot_and_settling() {
    // At every frame, within what a straight line of an update's length
    // misses of a smoothstep: `U² · 6 p / (8 r²)` for a rise of r frames to
    // a peak of p, and the same for the settling.
    for (offset, attack) in [
        (
            0usize,
            PartialAttack {
                delay_frames: 96,
                rise_frames: 1_440,
                overshoot: 0.25,
                settle_frames: 2_400,
                ..PartialAttack::default()
            },
        ),
        (
            7,
            PartialAttack {
                delay_frames: 5,
                rise_frames: 480,
                overshoot: 0.1,
                settle_frames: 480,
                ..PartialAttack::default()
            },
        ),
        (
            31,
            PartialAttack {
                delay_frames: 0,
                rise_frames: 960,
                ..PartialAttack::default()
            },
        ),
        (
            16,
            PartialAttack {
                delay_frames: 300,
                rise_frames: 256,
                overshoot: 1.0,
                settle_frames: 256,
                ..PartialAttack::default()
            },
        ),
    ] {
        let mut bank = envelope_partial();
        // The grid of updates is counted from the start of the bank.
        let lead = left_only(&mut bank, offset);
        assert!(lead.iter().all(|&x| x == 1.0));
        bank.start_attack(0, &attack);
        let span = (attack.delay_frames + attack.rise_frames + attack.settle_frames) as usize;
        let level = left_only(&mut bank, span + 256);
        let peak = 1.0 + f64::from(attack.overshoot);
        let shortest = attack.rise_frames.min(if attack.settle_frames > 0 {
            attack.settle_frames
        } else {
            attack.rise_frames
        });
        let u = UPDATE as f64;
        let bound = u * u * 6.0 * peak / (8.0 * f64::from(shortest).powi(2)) + 1e-5;
        let mut worst = 0.0f64;
        for (n, &got) in level.iter().enumerate() {
            // The first sample of the attack is one frame into it: the
            // modulator moves before the sample, as a gain ramp does.
            let error = (f64::from(got) - exact_level(&attack, n + 1)).abs();
            worst = worst.max(error);
        }
        eprintln!(
            "{} frames of rise from {offset}: worst {worst:.5}, bound {bound:.5}",
            attack.rise_frames
        );
        assert!(worst <= bound, "{worst} against {bound}");

        // Read as a person would: the peak, the settled level, and the
        // rise from 10 % to 90 % of the peak, 0.61 of the rise.
        let got_peak = level.iter().fold(0.0f32, |m, &x| m.max(x));
        assert!(
            (f64::from(got_peak) - peak).abs() < 0.01 * peak,
            "peak {got_peak} against {peak}"
        );
        assert!(level[span + 200] == 1.0 || (level[span + 200] - 1.0).abs() < 1e-5);
        let time_to = |share: f64| {
            level
                .iter()
                .position(|&x| f64::from(x) >= share * peak)
                .unwrap() as f64
        };
        let rise_time = time_to(0.9) - time_to(0.1);
        let want = 0.608_4 * f64::from(attack.rise_frames);
        assert!(
            (rise_time - want).abs() <= u / 2.0,
            "10 to 90 %: {rise_time} frames against {want}"
        );
    }
}

/// How far a sine at `hz` is present in `signal` around each frame: its
/// magnitude under a Hann window of `size`, read every `hop` frames, against
/// a sine of magnitude 1.
fn magnitude_track(signal: &[f32], hz: f64, size: usize, hop: usize) -> Vec<(usize, f64)> {
    let window: Vec<f64> = (0..size)
        .map(|n| 0.5 - 0.5 * (std::f64::consts::TAU * n as f64 / size as f64).cos())
        .collect();
    let sum: f64 = window.iter().sum();
    let w = std::f64::consts::TAU * hz / RATE;
    let mut track = Vec::new();
    let mut at = 0;
    while at + size <= signal.len() {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for n in 0..size {
            let x = f64::from(signal[at + n]) * window[n];
            let phase = w * (at + n) as f64;
            re += x * phase.cos();
            im += x * phase.sin();
        }
        track.push((at + size / 2, 2.0 * (re * re + im * im).sqrt() / sum));
        at += hop;
    }
    track
}

#[test]
fn upper_partials_reach_their_level_first_when_asked() {
    let lengths: [(usize, u32, u32); 4] = [(1, 192, 960), (2, 96, 720), (3, 48, 480), (4, 0, 240)];
    let mut bank = PartialBank::new(16);
    bank.set_sample_rate(RATE);
    bank.set_len(4);
    let f0 = 1_000.0;
    for &(h, delay_frames, rise_frames) in &lengths {
        bank.set(
            h - 1,
            Partial {
                freq_hz: f0 * h as f64,
                gain: 0.5 / h as f32,
                pan: -1.0,
            },
        );
        bank.start_attack(
            h - 1,
            &PartialAttack {
                delay_frames,
                rise_frames,
                ..PartialAttack::default()
            },
        );
    }
    let out = left_only(&mut bank, 4_800);
    // Hann windows of 5 ms pass the partial and none of its neighbours.
    let mut last = usize::MAX;
    for &(h, delay, rise) in lengths.iter().rev() {
        let full = 0.5 / h as f64;
        let track = magnitude_track(&out, f0 * h as f64, 240, 4);
        let half_way = track.iter().find(|&&(_, m)| m >= 0.5 * full).unwrap().0;
        // A smoothstep is half way half way through.
        let want = delay as usize + rise as usize / 2;
        eprintln!("harmonic {h}: half way at {half_way}, against {want}");
        assert!(
            half_way.abs_diff(want) <= UPDATE,
            "harmonic {h}: {half_way}"
        );
        if last != usize::MAX {
            assert!(half_way > last, "harmonic {h} before its upper neighbour");
        }
        last = half_way;
        // And it arrives.
        let end = track.last().unwrap().1;
        assert!((end - full).abs() < 0.01 * full, "harmonic {h}: {end}");
    }
}

#[test]
fn the_breath_starts_at_once_and_dies_as_it_should() {
    // A partial at 0 Hz with its phasor at (1, 0) outputs the imaginary part
    // of its modulator: for a breath, `breath (1 - t / T)² n(t)`, whose power
    // is half of `n`'s 1. Averaged over many seeds, in eight slices of T,
    // against that, to 10 %; and the delay does not hold it back.
    const T: usize = 2_400;
    const SLICES: usize = 8;
    let breath = 0.5f32;
    let seeds = 400;
    let mut power = [0.0f64; SLICES];
    for seed in 0..seeds {
        let mut bank = envelope_partial();
        bank.set_phasor(0, 1.0, 0.0);
        bank.set_voicing_seed(seed);
        bank.start_attack(
            0,
            &PartialAttack {
                delay_frames: 600,
                rise_frames: 600,
                breath,
                breath_frames: T as u32,
                breath_bandwidth_hz: 300.0,
                ..PartialAttack::default()
            },
        );
        let out = left_only(&mut bank, T);
        for (slice, chunk) in out.chunks(T / SLICES).enumerate() {
            power[slice] += power_of(chunk);
        }
    }
    for (slice, &got) in power.iter().enumerate() {
        let got = got / seeds as f64;
        let mid = (slice as f64 + 0.5) / SLICES as f64;
        // The mean of the fourth power across the slice, by a fine sum, of
        // the breath's fall.
        let fine: f64 = (0..100)
            .map(|k| {
                let t = (slice as f64 + (k as f64 + 0.5) / 100.0) / SLICES as f64;
                (1.0 - t).powi(4)
            })
            .sum::<f64>()
            / 100.0;
        let want = f64::from(breath * breath) * fine / 2.0;
        eprintln!("slice around {mid:.2} of the breath: {got:.5} against {want:.5}");
        // The first stretch includes the modulator's first line up from 0,
        // and noise that has not filled out.
        let tolerance = if slice == 0 { 0.2 } else { 0.15 };
        assert!(
            (got / want - 1.0).abs() < tolerance,
            "{slice}: {got} against {want}"
        );
    }
}

#[test]
fn the_breath_narrows_to_the_partials_own_bandwidth() {
    // How much the noise changes between one update and the next, against
    // its power, tells how wide it is. The air is deep and narrow, the
    // breath a mere trigger for the wide start.
    let measure = |from: usize, to: usize, breath_bandwidth_hz: f32| {
        let (mut change, mut total) = (0.0f64, 0.0f64);
        for seed in 0..100 {
            let mut bank = envelope_partial();
            bank.set_phasor(0, 1.0, 0.0);
            bank.set_voicing_seed(seed);
            bank.set_air(
                0,
                PartialAir {
                    depth: 0.5,
                    bandwidth_hz: 10.0,
                },
            );
            render(&mut bank, 2_048);
            bank.start_attack(
                0,
                &PartialAttack {
                    breath: 1e-3,
                    breath_frames: 4_800,
                    breath_bandwidth_hz,
                    ..PartialAttack::default()
                },
            );
            let out = left_only(&mut bank, to);
            for n in (from..to - UPDATE).step_by(UPDATE) {
                let (a, b) = (f64::from(out[n]), f64::from(out[n + UPDATE]));
                change += (b - a) * (b - a);
                total += a * a;
            }
        }
        change / total
    };
    let wide = measure(320, 800, 700.0);
    let narrow = measure(6_400, 8_000, 700.0);
    let unasked = measure(320, 800, 0.0);
    eprintln!("change per update: early {wide:.4}, later {narrow:.4}, without asking {unasked:.4}");
    assert!(wide > 10.0 * narrow, "{wide} against {narrow}");
    assert!(unasked < wide / 10.0, "{unasked} against {wide}");
}

/// Reads partial `i`'s turn over `frames` frames from its phasor, in
/// radians, and renders them.
fn turned(bank: &mut PartialBank, i: usize, frames: usize) -> f64 {
    let (x0, y0) = bank.phasor(i);
    let (mut l, mut r) = (vec![0.0f32; frames], vec![0.0f32; frames]);
    bank.render_add(&mut l, &mut r);
    let (x1, y1) = bank.phasor(i);
    let (x0, y0) = (f64::from(x0), f64::from(y0));
    let (x1, y1) = (f64::from(x1), f64::from(y1));
    (y1 * x0 - x1 * y0).atan2(x1 * x0 + y1 * y0)
}

#[test]
fn an_attacks_pitch_starts_off_and_settles() {
    // Read from the phasor each update, within 0.01 cents of the smoothstep
    // the settings give at the end of that update.
    let hz = 440.0;
    let cents = -50.0f64;
    let (delay, frames) = (320usize, 3_200usize);
    let mut bank = lone_partial(Backend::detect(), RATE, hz, 0.5);
    bank.start_attack(
        0,
        &PartialAttack {
            delay_frames: delay as u32,
            pitch_cents: cents as f32,
            pitch_frames: frames as u32,
            ..PartialAttack::default()
        },
    );
    let base = std::f64::consts::TAU * hz / RATE * UPDATE as f64;
    let smooth = |p: f64| p * p * (3.0 - 2.0 * p);
    let mut worst = 0.0f64;
    let mut update = 1;
    while update * UPDATE < delay + frames + UPDATE {
        let got = 1_200.0 * (turned(&mut bank, 0, UPDATE) / base).log2();
        let after = (update * UPDATE).saturating_sub(delay) as f64;
        let want = cents * (1.0 - smooth((after / frames as f64).min(1.0)));
        worst = worst.max((got - want).abs());
        update += 1;
    }
    eprintln!("the pitch's worst error: {worst:.4} cents");
    assert!(worst < 0.01, "{worst} cents");
    assert!(!bank.is_attacking(0));
    let got = 1_200.0 * (turned(&mut bank, 0, UPDATE) / base).log2();
    assert!(got.abs() < 1e-3, "{got} cents after settling");
}

/// The partials the jitter tests read: 0 Hz for the level, then a rank of
/// harmonics, with the voicing's wind on, read one update at a time.
struct WindReading {
    level_db: Vec<f64>,
    /// For each harmonic, the cents off its rotation, each update.
    cents: Vec<Vec<f64>>,
}

const WIND_HARMONICS: [f64; 4] = [1.0, 2.0, 7.0, 40.0];

fn read_wind(jitter: WindJitter, seconds: f64, f0: f64) -> WindReading {
    let mut bank = PartialBank::new(16);
    bank.set_sample_rate(RATE);
    bank.set_len(1 + WIND_HARMONICS.len());
    bank.set_rotation(0, 1.0, 0.0);
    bank.set_phasor(0, 0.0, 1.0);
    bank.set_gain(0, 1.0);
    bank.set_pan_gains(0, 1.0, 0.0);
    let mut base = Vec::new();
    for (k, &h) in WIND_HARMONICS.iter().enumerate() {
        bank.set(
            k + 1,
            Partial {
                freq_hz: f0 * h,
                gain: 0.0,
                pan: 0.0,
            },
        );
        let (c, s) = bank.rotation(k + 1);
        base.push(f64::from(s).atan2(f64::from(c)) * UPDATE as f64);
    }
    bank.set_wind_jitter(jitter);
    // The wander takes a few of its time constants to fill out.
    let start = (RATE * 2.0) as usize;
    render(&mut bank, start / UPDATE * UPDATE);
    let updates = (seconds * RATE) as usize / UPDATE;
    let mut level_db = Vec::with_capacity(updates);
    let mut cents = vec![Vec::with_capacity(updates); WIND_HARMONICS.len()];
    let (mut l, mut r) = (vec![0.0f32; UPDATE], vec![0.0f32; UPDATE]);
    for _ in 0..updates {
        let before: Vec<(f32, f32)> = (1..=WIND_HARMONICS.len()).map(|i| bank.phasor(i)).collect();
        l.fill(0.0);
        r.fill(0.0);
        bank.render_add(&mut l, &mut r);
        level_db.push(20.0 * f64::from(l[UPDATE - 1]).log10());
        for (k, &(x0, y0)) in before.iter().enumerate() {
            let (x1, y1) = bank.phasor(k + 1);
            let (x0, y0) = (f64::from(x0), f64::from(y0));
            let (x1, y1) = (f64::from(x1), f64::from(y1));
            let angle = (y1 * x0 - x1 * y0).atan2(x1 * x0 + y1 * y0);
            // Upper harmonics turn more than half a turn in an update.
            let turns = ((base[k] - angle) / std::f64::consts::TAU).round();
            let angle = angle + turns * std::f64::consts::TAU;
            cents[k].push(1_200.0 * (angle / base[k]).log2());
        }
    }
    WindReading { level_db, cents }
}

fn rms(x: &[f64]) -> f64 {
    (x.iter().map(|v| v * v).sum::<f64>() / x.len() as f64).sqrt()
}

fn mean(x: &[f64]) -> f64 {
    x.iter().sum::<f64>() / x.len() as f64
}

#[test]
fn jitter_has_the_depth_the_settings_say_and_keeps_partials_in_ratio() {
    // A minute of a wander with a corner of 8 Hz holds some 750 independent
    // values, whose root mean square is known to 3 %: within 10 %.
    let jitter = WindJitter {
        cents: 2.0,
        db: 0.4,
        rate_hz: 8.0,
    };
    let reading = read_wind(jitter, 60.0, 261.63);
    let level = rms(&reading.level_db);
    let cents = rms(&reading.cents[0]);
    eprintln!(
        "level {level:.3} dB, mean {:.4} dB; pitch {cents:.3} cents, mean {:.4} cents",
        mean(&reading.level_db),
        mean(&reading.cents[0])
    );
    assert!(
        (level / f64::from(jitter.db) - 1.0).abs() < 0.1,
        "{level} dB"
    );
    assert!(
        (cents / f64::from(jitter.cents) - 1.0).abs() < 0.1,
        "{cents} cents"
    );
    // Around where it was: the wander has a mean of nearly nothing.
    assert!(mean(&reading.level_db).abs() < 0.04);
    assert!(mean(&reading.cents[0]).abs() < 0.2);

    // Every partial moves by the same ratio. What the phasors can show of
    // it is limited by their rounding; 0.002 cents is a ratio of 1.2e-6.
    let mut worst = 0.0f64;
    for k in 1..WIND_HARMONICS.len() {
        for (a, b) in reading.cents[0].iter().zip(&reading.cents[k]) {
            worst = worst.max((a - b).abs());
        }
    }
    eprintln!("the harmonics differ by at most {worst:.5} cents");
    assert!(worst < 0.002, "{worst} cents");
    // And the same in the long run: the sum of those cents, which is phase.
    for k in 1..WIND_HARMONICS.len() {
        let drift: f64 = reading.cents[k]
            .iter()
            .zip(&reading.cents[0])
            .map(|(a, b)| a - b)
            .sum::<f64>()
            / reading.cents[0].len() as f64;
        assert!(drift.abs() < 1e-4, "harmonic {k}: {drift} cents on average");
    }
}

#[test]
fn jitter_of_other_depths_and_rates_is_as_deep_as_it_says() {
    // Shorter runs of a faster wander.
    for (cents, db_depth, rate_hz) in [(0.5f32, 0.08f32, 20.0f32), (6.0, 1.0, 20.0)] {
        let reading = read_wind(
            WindJitter {
                cents,
                db: db_depth,
                rate_hz,
            },
            20.0,
            261.63,
        );
        let got_db = rms(&reading.level_db);
        let got_cents = rms(&reading.cents[0]);
        eprintln!("{cents} cents, {db_depth} dB: got {got_cents:.3} cents, {got_db:.3} dB");
        assert!((got_db / f64::from(db_depth) - 1.0).abs() < 0.1, "{got_db}");
        assert!(
            (got_cents / f64::from(cents) - 1.0).abs() < 0.1,
            "{got_cents}"
        );
    }
}

#[test]
fn jitter_adds_to_what_a_bellows_model_sets() {
    // The caller glides the pitch with `set_frequency` and `set_rotation`,
    // and ramps a gain, as the wind model does every control block. The
    // cents heard are the caller's plus the jitter's, and `rotation` and
    // `gain` go on saying what the caller set.
    // Low enough to turn under half a turn in a block, for reading it.
    const HZ: f64 = 220.0;
    let jitter = WindJitter {
        cents: 3.0,
        db: 0.5,
        rate_hz: 10.0,
    };
    let start = |gliding: bool| {
        let mut bank = lone_partial(Backend::detect(), RATE, HZ, 1.0);
        bank.set_voicing_seed(9);
        bank.set_wind_jitter(jitter);
        render(&mut bank, 48_000 / 64 * 64);
        (bank, gliding)
    };
    let (mut still, _) = start(false);
    let (mut gliding, _) = start(true);
    let block = 64;
    let base = std::f64::consts::TAU * HZ / RATE * block as f64;
    let mut worst = 0.0f64;
    for n in 0..1_500 {
        let glide = 20.0 * (n as f64 * 0.01).sin();
        let hz = HZ * (glide / 1_200.0).exp2();
        if n % 2 == 0 {
            gliding.set_frequency(0, hz);
        } else {
            let w = std::f64::consts::TAU * hz / RATE;
            gliding.set_rotation(0, w.cos() as f32, w.sin() as f32);
        }
        let (want_c, want_s) = {
            let w = std::f64::consts::TAU * hz / RATE;
            (w.cos() as f32, w.sin() as f32)
        };
        assert_eq!(gliding.rotation(0), (want_c, want_s));
        let a = 1_200.0 * (turned(&mut still, 0, block) / base).log2();
        let b = 1_200.0 * (turned(&mut gliding, 0, block) / base).log2();
        assert!(a.is_finite() && b.is_finite(), "{a} {b}");
        worst = worst.max((b - a - glide).abs());
    }
    eprintln!("caller's cents plus the jitter's, worst error {worst:.5}");
    assert!(worst < 0.01, "{worst} cents");

    let mut bank = lone_partial(Backend::detect(), RATE, 440.0, 1.0);
    bank.set_wind_jitter(jitter);
    bank.ramp_to(0, 0.25, 1_000);
    render(&mut bank, 2_000);
    assert!((bank.gain(0) - 0.25).abs() < 1e-5);
}

#[test]
fn the_phasors_stay_on_the_circle_under_jitter_and_pitch() {
    const HZ: [f64; 8] = [
        0.5,
        27.5,
        261.625_565,
        1_000.123,
        5_000.0,
        12_345.6,
        20_000.0,
        23_900.0,
    ];
    let mut bank = PartialBank::new(16);
    bank.set_sample_rate(RATE);
    bank.set_len(HZ.len());
    for (i, &hz) in HZ.iter().enumerate() {
        bank.set(
            i,
            Partial {
                freq_hz: hz,
                gain: 0.1,
                pan: 0.0,
            },
        );
    }
    bank.set_wind_jitter(WindJitter {
        cents: 25.0,
        db: 3.0,
        rate_hz: 40.0,
    });
    let mut worst = 0.0f64;
    let (mut l, mut r) = ([0.0f32; 64], [0.0f32; 64]);
    for block in 0..15_000 {
        if block % 500 == 0 {
            for i in 0..HZ.len() {
                bank.start_attack(
                    i,
                    &PartialAttack {
                        pitch_cents: if block % 1_000 == 0 { -100.0 } else { 100.0 },
                        pitch_frames: 24_000,
                        ..PartialAttack::default()
                    },
                );
            }
        }
        bank.render_add(&mut l, &mut r);
        // Read at the end of a normalisation period, the furthest out.
        for i in 0..HZ.len() {
            let (x, y) = bank.phasor(i);
            let magnitude = (f64::from(x).powi(2) + f64::from(y).powi(2)).sqrt();
            worst = worst.max((magnitude - 1.0).abs());
        }
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    }
    eprintln!("largest |magnitude - 1|: {worst:.3e}");
    assert!(worst < 3e-5, "{worst}");
}

/// The largest difference between neighbouring samples.
fn largest_step(signal: &[f32]) -> f64 {
    signal
        .windows(2)
        .map(|w| f64::from(w[1] - w[0]).abs())
        .fold(0.0, f64::max)
}

#[test]
fn changing_the_air_while_playing_does_not_click() {
    // What the air adds, on a partial of gain 1 at 100 Hz, stays smooth while
    // its depth or width moves: the largest step from sample to sample in
    // a change is within 1.3 times that of playing steadily at the larger
    // depth. In absolute terms, under `depth · 4 · (w + (1 + 2 / 16) / 32 · 2)`,
    // four standard deviations of noise turning at `w` and moving between
    // its values and depths in an update.
    let hz = 100.0;
    let w = std::f64::consts::TAU * hz / RATE;
    let depth = 0.1f32;
    let start = |air: PartialAir| {
        let mut plain = lone_partial(Backend::detect(), RATE, hz, 1.0);
        let mut airy = plain.clone();
        airy.set_air(0, air);
        render(&mut plain, 9_600);
        render(&mut airy, 9_600);
        (plain, airy)
    };
    let added = |plain: &mut PartialBank, airy: &mut PartialBank, frames: usize| -> Vec<f32> {
        let clean = left_only(plain, frames);
        let noisy = left_only(airy, frames);
        noisy.iter().zip(&clean).map(|(a, b)| a - b).collect()
    };
    let air = PartialAir {
        depth,
        bandwidth_hz: 100.0,
    };

    let (mut plain, mut airy) = start(air);
    let steady = largest_step(&added(&mut plain, &mut airy, 24_000));
    let bound = f64::from(depth) * 4.0 * (w + 2.25 / UPDATE as f64);
    eprintln!("steady: largest step {steady:.6}, bound {bound:.6}");
    assert!(steady < bound, "{steady} against {bound}");

    type Change = fn(&mut PartialBank, PartialAir);
    let moves: [(&str, Change); 4] = [
        ("to none", |bank, _| bank.set_air_depth(0, 0.0)),
        ("to half", |bank, air| {
            bank.set_air_depth(0, air.depth / 2.0)
        }),
        ("narrower", |bank, air| {
            bank.set_air(
                0,
                PartialAir {
                    bandwidth_hz: air.bandwidth_hz / 4.0,
                    ..air
                },
            );
        }),
        ("wider", |bank, air| {
            bank.set_air(
                0,
                PartialAir {
                    bandwidth_hz: air.bandwidth_hz * 4.0,
                    ..air
                },
            );
        }),
    ];
    for (name, change) in moves {
        let (mut plain, mut airy) = start(air);
        // Some frames after a change, then the change itself, off the grid
        // of updates too.
        added(&mut plain, &mut airy, 1_003);
        change(&mut airy, air);
        let step = largest_step(&added(&mut plain, &mut airy, 1_024));
        eprintln!("{name}: largest step {step:.6}");
        assert!(step < 1.3 * steady, "{name}: {step} against {steady}");
    }

    // From nothing, too: on a bank running the plain kernel.
    let mut plain = lone_partial(Backend::detect(), RATE, hz, 1.0);
    let mut airy = plain.clone();
    added(&mut plain, &mut airy, 1_003);
    airy.set_air(0, air);
    let step = largest_step(&added(&mut plain, &mut airy, 2_048));
    eprintln!("from nothing: largest step {step:.6}");
    assert!(step < bound, "from nothing: {step}");
}

#[test]
fn changing_the_jitter_while_playing_does_not_click() {
    // A partial at 0 Hz shows the gain's jitter alone: 0.5 dB at 10 Hz moves
    // at most 5e-4 from one sample to the next, turned on, off or down, some
    // 0.004 dB. A wander of that depth and rate moves about that anyway.
    let mut bank = envelope_partial();
    let mut worst = 0.0f64;
    let jitter = WindJitter {
        cents: 2.0,
        db: 0.5,
        rate_hz: 10.0,
    };
    for (frames, setting) in [
        (1_003, jitter),
        (48_000, WindJitter { db: 0.1, ..jitter }),
        (9_601, WindJitter::OFF),
        (3_000, jitter),
    ] {
        worst = worst.max(largest_step(&left_only(&mut bank, frames)));
        bank.set_wind_jitter(setting);
    }
    worst = worst.max(largest_step(&left_only(&mut bank, 48_000)));
    eprintln!("the gain's largest step {worst:.6}");
    assert!(worst < 5e-4, "{worst}");
    // Off for good: the bank is plain again, and at its level exactly.
    bank.set_wind_jitter(WindJitter::OFF);
    let out = left_only(&mut bank, 9_600);
    assert!(!bank.is_voiced());
    assert_eq!(out[9_599], 1.0);
}

#[test]
fn the_flue_principal_voices_a_rank_as_its_comments_say() {
    let voicing = PipeVoicing::FLUE_PRINCIPAL;
    let f0 = 261.63;
    let mut last = PartialAttack {
        delay_frames: u32::MAX,
        rise_frames: u32::MAX,
        ..PartialAttack::default()
    };
    let mut last_depth = 0.0;
    for h in [1.0f32, 2.0, 4.0, 8.0, 16.0, 32.0] {
        let air = voicing.air(h, f0 * f64::from(h));
        let attack = voicing.attack(h, RATE);
        // Upper partials: sooner, quicker, with more overshoot, more air.
        assert!(attack.delay_frames <= last.delay_frames, "{h}");
        assert!(attack.rise_frames < last.rise_frames, "{h}");
        assert!(attack.overshoot >= last.overshoot, "{h}");
        assert!(air.depth >= last_depth, "{h}");
        assert!(air.bandwidth_hz > 0.0 && attack.rise_frames > 0, "{h}");
        (last, last_depth) = (attack, air.depth);
    }
    assert_eq!(voicing.attack(1.0, RATE).delay_frames, 144);
    assert_eq!(voicing.attack(16.0, RATE).delay_frames, 0);
    assert_eq!(voicing.attack(1.0, RATE).overshoot, 0.0);

    // A pipe of 16 partials speaks, and settles back at its plain level.
    let build = |voiced: bool| {
        let mut bank = PartialBank::new(16);
        bank.set_sample_rate(RATE);
        bank.set_len(16);
        for i in 0..16 {
            let h = (i + 1) as f32;
            let hz = f0 * f64::from(h);
            bank.set(
                i,
                Partial {
                    freq_hz: hz,
                    gain: 0.3 / h,
                    pan: -1.0,
                },
            );
            if voiced {
                bank.set_air(i, voicing.air(h, hz));
                bank.start_attack(i, &voicing.attack(h, RATE));
            }
        }
        if voiced {
            bank.set_wind_jitter(voicing.jitter);
        }
        bank
    };
    let mut plain = build(false);
    let mut voiced = build(true);
    let early = left_only(&mut voiced, 4_800);
    render(&mut plain, 4_800);
    // Silent for the first few frames, on its way after 10 ms.
    assert!(early[..64].iter().all(|x| x.abs() < 0.3));
    assert!(early[480..4_800].iter().any(|x| x.abs() > 0.1));
    assert!((0..16).all(|i| !voiced.is_attacking(i)));
    let (a, b) = (
        left_only(&mut voiced, 96_000),
        left_only(&mut plain, 96_000),
    );
    let change = db(power_of(&a) / power_of(&b));
    eprintln!("the voiced sustain against the plain: {change:+.3} dB");
    assert!(change.abs() < 0.2, "{change}");
}
