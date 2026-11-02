//! The partial bank's voicing: air, attacks and wind jitter. That a bank
//! which sets none of them renders what it always did, bit for bit; that
//! what it renders with them does not depend on the backend or the blocks;
//! and, measured on the output, that the sound is what the settings say.

use manguebyte_core::vector::Backend;
use manguebyte_dsp::PartialBank;

const RATE: f64 = 48_000.0;

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
