//! The definitions: what every backend computes, written one element at a
//! time. The vector backends also run these on the elements left over at
//! the end of a slice.

use super::{Partials, Peak};
use crate::Sample;

/// How many running sums [`dot`] keeps for a sample type: 16 for `f32`, 8
/// for `f64`. Always a power of two.
pub(super) const fn sum_lanes<S>() -> usize {
    if core::mem::size_of::<S>() <= 4 {
        16
    } else {
        8
    }
}

/// Running sums of [`sum_of_squares`], which are `f64` whatever the input.
pub(super) const SQUARE_LANES: usize = 8;

/// Adds running sums in pairs, `s[j] + s[j + half]`, halving their number
/// until one is left. `lanes.len()` is a power of two.
#[inline]
pub(super) fn reduce<S: Sample>(lanes: &mut [S]) -> S {
    let mut width = lanes.len();
    while width > 1 {
        let half = width / 2;
        for j in 0..half {
            lanes[j] += lanes[j + half];
        }
        width = half;
    }
    lanes[0]
}

/// Index `i` as a sample, rounded to the nearest one where it is not exact
/// (beyond 2^24 in `f32`).
#[inline]
pub(super) fn index<S: Sample>(i: usize) -> S {
    S::from_f64(i as f64)
}

pub(super) fn scale<S: Sample>(dst: &mut [S], gain: S) {
    for x in dst {
        *x *= gain;
    }
}

pub(super) fn copy_scaled<S: Sample>(dst: &mut [S], src: &[S], gain: S) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d = s * gain;
    }
}

pub(super) fn add<S: Sample>(dst: &mut [S], src: &[S]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d += s;
    }
}

pub(super) fn add_scaled<S: Sample>(dst: &mut [S], src: &[S], gain: S) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d += s * gain;
    }
}

pub(super) fn multiply<S: Sample>(dst: &mut [S], src: &[S]) {
    for (d, &s) in dst.iter_mut().zip(src) {
        *d *= s;
    }
}

/// The ramp from index `first` on: element `k` is scaled by
/// `start + step * (first + k)`.
pub(super) fn scale_ramp<S: Sample>(dst: &mut [S], start: S, step: S, first: usize) {
    for (k, x) in dst.iter_mut().enumerate() {
        *x *= start + step * index::<S>(first + k);
    }
}

/// The ramp from index `first` on, added: element `k` of `src` is scaled
/// by `start + step * (first + k)` and added to `dst`.
pub(super) fn add_scaled_ramp<S: Sample>(
    dst: &mut [S],
    src: &[S],
    start: S,
    step: S,
    first: usize,
) {
    for (k, (d, &s)) in dst.iter_mut().zip(src).enumerate() {
        *d += s * (start + step * index::<S>(first + k));
    }
}

pub(super) fn add_product<S: Sample>(dst: &mut [S], a: &[S], b: &[S]) {
    for ((d, &x), &y) in dst.iter_mut().zip(a).zip(b) {
        *d += x * y;
    }
}

pub(super) fn peak<S: Sample>(src: &[S]) -> Peak<S> {
    let mut largest = S::ZERO;
    let mut nan = false;
    for &x in src {
        let magnitude = x.abs();
        #[allow(clippy::eq_op)] // Only NaN differs from itself.
        if magnitude != magnitude {
            nan = true;
        } else if magnitude > largest {
            largest = magnitude;
        }
    }
    Peak { largest, nan }
}

pub(super) fn dot<S: Sample>(a: &[S], b: &[S]) -> S {
    if sum_lanes::<S>() == 16 {
        dot_in::<S, 16>(a, b)
    } else {
        dot_in::<S, 8>(a, b)
    }
}

/// [`dot`] with `LANES` running sums, a constant so that the compiler can
/// keep them in registers.
fn dot_in<S: Sample, const LANES: usize>(a: &[S], b: &[S]) -> S {
    let mut sums = [S::ZERO; LANES];
    let (mut a_chunks, mut b_chunks) = (a.chunks_exact(LANES), b.chunks_exact(LANES));
    for (x, y) in (&mut a_chunks).zip(&mut b_chunks) {
        for j in 0..LANES {
            sums[j] += x[j] * y[j];
        }
    }
    let mut sum = reduce(&mut sums);
    // The elements after the last whole chunk, one after another.
    for (&x, &y) in a_chunks.remainder().iter().zip(b_chunks.remainder()) {
        sum += x * y;
    }
    sum
}

pub(super) fn sum_of_squares<S: Sample>(src: &[S]) -> f64 {
    let mut sums = [0.0f64; SQUARE_LANES];
    let mut chunks = src.chunks_exact(SQUARE_LANES);
    for chunk in &mut chunks {
        for j in 0..SQUARE_LANES {
            let x = chunk[j].to_f64();
            sums[j] += x * x;
        }
    }
    let mut sum = reduce(&mut sums);
    for &x in chunks.remainder() {
        let x = x.to_f64();
        sum += x * x;
    }
    sum
}

pub(super) fn interleave_stereo<S: Sample>(dst: &mut [S], left: &[S], right: &[S]) {
    for ((frame, &l), &r) in dst.chunks_exact_mut(2).zip(left).zip(right) {
        frame[0] = l;
        frame[1] = r;
    }
}

pub(super) fn deinterleave_stereo<S: Sample>(left: &mut [S], right: &mut [S], src: &[S]) {
    for ((frame, l), r) in src.chunks_exact(2).zip(left).zip(right) {
        *l = frame[0];
        *r = frame[1];
    }
}

pub(super) fn complex_multiply_add<S: Sample>(sum: &mut [S], a: &[S], b: &[S]) {
    for ((s, x), y) in sum
        .chunks_exact_mut(2)
        .zip(a.chunks_exact(2))
        .zip(b.chunks_exact(2))
    {
        let (ar, ai, br, bi) = (x[0], x[1], y[0], y[1]);
        s[0] += ar * br - ai * bi;
        s[1] += ar * bi + ai * br;
    }
}

/// Running sums [`add_partials`] keeps for each frame: partial `i` of those
/// before the last whole group of 16 goes to sum `i % 16`.
pub(super) const PARTIAL_LANES: usize = 16;

/// Frames whose running sums [`add_partials`] keeps at once. Only the
/// memory it takes depends on it, not the result.
pub(super) const PARTIAL_FRAMES: usize = 64;

/// The bank of partials, in the order
/// [`Backend::add_partials`](super::Backend::add_partials) gives.
pub(super) fn add_partials(left: &mut [f32], right: &mut [f32], partials: &mut Partials<'_>) {
    let count = partials.len();
    let body = count - count % PARTIAL_LANES;
    if body > 0 {
        let mut sums_left = [0.0f32; PARTIAL_LANES * PARTIAL_FRAMES];
        let mut sums_right = [0.0f32; PARTIAL_LANES * PARTIAL_FRAMES];
        for (out_left, out_right) in left
            .chunks_mut(PARTIAL_FRAMES)
            .zip(right.chunks_mut(PARTIAL_FRAMES))
        {
            let frames = out_left.len();
            let sums_left = &mut sums_left[..frames * PARTIAL_LANES];
            let sums_right = &mut sums_right[..frames * PARTIAL_LANES];
            sums_left.fill(0.0);
            sums_right.fill(0.0);
            for i in 0..body {
                let lane = i % PARTIAL_LANES;
                let (mut x, mut y, mut g) = (partials.cos[i], partials.sin[i], partials.gain[i]);
                let (c, s, step) = (
                    partials.cos_step[i],
                    partials.sin_step[i],
                    partials.gain_step[i],
                );
                let (to_left, to_right) = (partials.left_gain[i], partials.right_gain[i]);
                for t in 0..frames {
                    g += step;
                    let v = g * y;
                    sums_left[t * PARTIAL_LANES + lane] += v * to_left;
                    sums_right[t * PARTIAL_LANES + lane] += v * to_right;
                    let next_x = x * c - y * s;
                    let next_y = x * s + y * c;
                    x = next_x;
                    y = next_y;
                }
                partials.cos[i] = x;
                partials.sin[i] = y;
                partials.gain[i] = g;
            }
            for (t, (l, r)) in out_left.iter_mut().zip(out_right.iter_mut()).enumerate() {
                let at = t * PARTIAL_LANES;
                *l += reduce(&mut sums_left[at..at + PARTIAL_LANES]);
                *r += reduce(&mut sums_right[at..at + PARTIAL_LANES]);
            }
        }
    }
    for i in body..count {
        add_partial_in_order(left, right, partials, i);
    }
}

/// Partial `i` added straight to the output, frame after frame: what the
/// partials after the last whole group of [`PARTIAL_LANES`] do.
pub(super) fn add_partial_in_order(
    left: &mut [f32],
    right: &mut [f32],
    partials: &mut Partials<'_>,
    i: usize,
) {
    let (mut x, mut y, mut g) = (partials.cos[i], partials.sin[i], partials.gain[i]);
    let (c, s, step) = (
        partials.cos_step[i],
        partials.sin_step[i],
        partials.gain_step[i],
    );
    let (to_left, to_right) = (partials.left_gain[i], partials.right_gain[i]);
    for (l, r) in left.iter_mut().zip(right.iter_mut()) {
        g += step;
        let v = g * y;
        *l += v * to_left;
        *r += v * to_right;
        let next_x = x * c - y * s;
        let next_y = x * s + y * c;
        x = next_x;
        y = next_y;
    }
    partials.cos[i] = x;
    partials.sin[i] = y;
    partials.gain[i] = g;
}

pub(super) fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(2)) {
        *d = f32::from(i16::from_le_bytes([s[0], s[1]])) / 32_768.0;
    }
}

pub(super) fn i24_le_to_f32(dst: &mut [f32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(3)) {
        // Shifting up and back down sign-extends the 24-bit value.
        let value = i32::from_le_bytes([0, s[0], s[1], s[2]]) >> 8;
        *d = value as f32 / 8_388_608.0;
    }
}

pub(super) fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src.chunks_exact(4)) {
        let value = i32::from_le_bytes([s[0], s[1], s[2], s[3]]);
        *d = (f64::from(value) / 2_147_483_648.0) as f32;
    }
}

/// One sample as a 16-bit integer, as [`f32_to_i16_le`] stores it.
#[inline]
pub(super) fn quantise_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

/// One sample as a 24-bit integer, as [`f32_to_i24_le`] stores it.
#[inline]
pub(super) fn quantise_i24(sample: f32) -> i32 {
    (f64::from(sample.clamp(-1.0, 1.0)) * 8_388_607.0).round() as i32
}

pub(super) fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
    for (d, &s) in dst.chunks_exact_mut(2).zip(src) {
        d.copy_from_slice(&quantise_i16(s).to_le_bytes());
    }
}

pub(super) fn f32_to_i24_le(dst: &mut [u8], src: &[f32]) {
    for (d, &s) in dst.chunks_exact_mut(3).zip(src) {
        d.copy_from_slice(&quantise_i24(s).to_le_bytes()[..3]);
    }
}

// The names the dispatcher calls, one per element type, as every backend
// module has them.

pub(super) fn scale_f32(dst: &mut [f32], gain: f32) {
    scale(dst, gain);
}
pub(super) fn scale_f64(dst: &mut [f64], gain: f64) {
    scale(dst, gain);
}
pub(super) fn copy_scaled_f32(dst: &mut [f32], src: &[f32], gain: f32) {
    copy_scaled(dst, src, gain);
}
pub(super) fn copy_scaled_f64(dst: &mut [f64], src: &[f64], gain: f64) {
    copy_scaled(dst, src, gain);
}
pub(super) fn add_f32(dst: &mut [f32], src: &[f32]) {
    add(dst, src);
}
pub(super) fn add_f64(dst: &mut [f64], src: &[f64]) {
    add(dst, src);
}
pub(super) fn add_scaled_f32(dst: &mut [f32], src: &[f32], gain: f32) {
    add_scaled(dst, src, gain);
}
pub(super) fn add_scaled_f64(dst: &mut [f64], src: &[f64], gain: f64) {
    add_scaled(dst, src, gain);
}
pub(super) fn multiply_f32(dst: &mut [f32], src: &[f32]) {
    multiply(dst, src);
}
pub(super) fn multiply_f64(dst: &mut [f64], src: &[f64]) {
    multiply(dst, src);
}
pub(super) fn scale_ramp_f32(dst: &mut [f32], start: f32, step: f32) {
    scale_ramp(dst, start, step, 0);
}
pub(super) fn scale_ramp_f64(dst: &mut [f64], start: f64, step: f64) {
    scale_ramp(dst, start, step, 0);
}
pub(super) fn add_scaled_ramp_f32(dst: &mut [f32], src: &[f32], start: f32, step: f32) {
    add_scaled_ramp(dst, src, start, step, 0);
}
pub(super) fn add_scaled_ramp_f64(dst: &mut [f64], src: &[f64], start: f64, step: f64) {
    add_scaled_ramp(dst, src, start, step, 0);
}
pub(super) fn add_product_f32(dst: &mut [f32], a: &[f32], b: &[f32]) {
    add_product(dst, a, b);
}
pub(super) fn add_product_f64(dst: &mut [f64], a: &[f64], b: &[f64]) {
    add_product(dst, a, b);
}
pub(super) fn peak_f32(src: &[f32]) -> Peak<f32> {
    peak(src)
}
pub(super) fn peak_f64(src: &[f64]) -> Peak<f64> {
    peak(src)
}
pub(super) fn dot_f32(a: &[f32], b: &[f32]) -> f32 {
    dot(a, b)
}
pub(super) fn dot_f64(a: &[f64], b: &[f64]) -> f64 {
    dot(a, b)
}
pub(super) fn sum_of_squares_f32(src: &[f32]) -> f64 {
    sum_of_squares(src)
}
pub(super) fn sum_of_squares_f64(src: &[f64]) -> f64 {
    sum_of_squares(src)
}
pub(super) fn interleave_stereo_f32(dst: &mut [f32], left: &[f32], right: &[f32]) {
    interleave_stereo(dst, left, right);
}
pub(super) fn interleave_stereo_f64(dst: &mut [f64], left: &[f64], right: &[f64]) {
    interleave_stereo(dst, left, right);
}
pub(super) fn deinterleave_stereo_f32(left: &mut [f32], right: &mut [f32], src: &[f32]) {
    deinterleave_stereo(left, right, src);
}
pub(super) fn deinterleave_stereo_f64(left: &mut [f64], right: &mut [f64], src: &[f64]) {
    deinterleave_stereo(left, right, src);
}
pub(super) fn complex_multiply_add_f32(sum: &mut [f32], a: &[f32], b: &[f32]) {
    complex_multiply_add(sum, a, b);
}
pub(super) fn complex_multiply_add_f64(sum: &mut [f64], a: &[f64], b: &[f64]) {
    complex_multiply_add(sum, a, b);
}
