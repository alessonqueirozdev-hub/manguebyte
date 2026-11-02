//! NEON (Advanced SIMD), on AArch64, where every processor has it.
//!
//! The instructions are those of Arm's *Intrinsics* reference
//! (<https://developer.arm.com/architectures/instruction-sets/intrinsics/>)
//! and the *Arm Architecture Reference Manual for A-profile architecture*,
//! as `core::arch::aarch64` exposes them.
//!
//! As on x86, the `unsafe` blocks keep the crate building with Rust 1.85,
//! before most intrinsics became safe to call.
#![allow(unused_unsafe)]

use core::arch::aarch64::*;

use super::generic::{Vector, Widen, entry_points};
use super::scalar;

/// `f32` in NEON registers.
#[derive(Clone, Copy)]
pub(super) struct Neon32;
/// `f64` in NEON registers.
#[derive(Clone, Copy)]
pub(super) struct Neon64;

// SAFETY (for every method below): NEON is part of every processor this
// module is built for; loads and stores need only valid pointers to
// `LANES` elements, which the caller guarantees.

impl Vector for Neon32 {
    type T = f32;
    type Reg = float32x4_t;
    const LANES: usize = 4;

    #[inline(always)]
    unsafe fn load(p: *const f32) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vld1q_f32(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f32, v: float32x4_t) {
        // SAFETY: see above.
        unsafe { vst1q_f32(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f32) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vdupq_n_f32(x) }
    }
    #[inline(always)]
    unsafe fn add(a: float32x4_t, b: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vaddq_f32(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: float32x4_t, b: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vmulq_f32(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vabsq_f32(a) }
    }
    #[inline(always)]
    unsafe fn max(a: float32x4_t, b: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vmaxq_f32(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vreinterpretq_f32_u32(vmvnq_u32(vceqq_f32(a, a))) }
    }
    #[inline(always)]
    unsafe fn or(a: float32x4_t, b: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe {
            vreinterpretq_f32_u32(vorrq_u32(
                vreinterpretq_u32_f32(a),
                vreinterpretq_u32_f32(b),
            ))
        }
    }
    #[inline(always)]
    unsafe fn xor(a: float32x4_t, b: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe {
            vreinterpretq_f32_u32(veorq_u32(
                vreinterpretq_u32_f32(a),
                vreinterpretq_u32_f32(b),
            ))
        }
    }
    #[inline(always)]
    unsafe fn any(mask: float32x4_t) -> bool {
        // SAFETY: see above.
        unsafe { vmaxvq_u32(vreinterpretq_u32_f32(mask)) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: float32x4_t, b: float32x4_t) -> (float32x4_t, float32x4_t) {
        // SAFETY: see above.
        unsafe { (vzip1q_f32(a, b), vzip2q_f32(a, b)) }
    }
    #[inline(always)]
    unsafe fn unzip(a: float32x4_t, b: float32x4_t) -> (float32x4_t, float32x4_t) {
        // SAFETY: see above.
        unsafe { (vuzp1q_f32(a, b), vuzp2q_f32(a, b)) }
    }
    #[inline(always)]
    unsafe fn dup_even(a: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vtrn1q_f32(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vtrn2q_f32(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: float32x4_t) -> float32x4_t {
        // SAFETY: see above.
        unsafe { vrev64q_f32(a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: float32x4_t) -> f32 {
        // `(a0 + a2) + (a1 + a3)`.
        // SAFETY: see above.
        unsafe {
            let pairs = vadd_f32(vget_low_f32(a), vget_high_f32(a));
            vget_lane_f32::<0>(pairs) + vget_lane_f32::<1>(pairs)
        }
    }
}

impl Vector for Neon64 {
    type T = f64;
    type Reg = float64x2_t;
    const LANES: usize = 2;

    #[inline(always)]
    unsafe fn load(p: *const f64) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vld1q_f64(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f64, v: float64x2_t) {
        // SAFETY: see above.
        unsafe { vst1q_f64(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f64) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vdupq_n_f64(x) }
    }
    #[inline(always)]
    unsafe fn add(a: float64x2_t, b: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vaddq_f64(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: float64x2_t, b: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vmulq_f64(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vabsq_f64(a) }
    }
    #[inline(always)]
    unsafe fn max(a: float64x2_t, b: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vmaxq_f64(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: float64x2_t) -> float64x2_t {
        // There is no 64-bit NOT; an exclusive or with all ones is one.
        // SAFETY: see above.
        unsafe { vreinterpretq_f64_u64(veorq_u64(vceqq_f64(a, a), vdupq_n_u64(u64::MAX))) }
    }
    #[inline(always)]
    unsafe fn or(a: float64x2_t, b: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe {
            vreinterpretq_f64_u64(vorrq_u64(
                vreinterpretq_u64_f64(a),
                vreinterpretq_u64_f64(b),
            ))
        }
    }
    #[inline(always)]
    unsafe fn xor(a: float64x2_t, b: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe {
            vreinterpretq_f64_u64(veorq_u64(
                vreinterpretq_u64_f64(a),
                vreinterpretq_u64_f64(b),
            ))
        }
    }
    #[inline(always)]
    unsafe fn any(mask: float64x2_t) -> bool {
        // SAFETY: see above.
        unsafe { vmaxvq_u32(vreinterpretq_u32_f64(mask)) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: float64x2_t, b: float64x2_t) -> (float64x2_t, float64x2_t) {
        // SAFETY: see above.
        unsafe { (vzip1q_f64(a, b), vzip2q_f64(a, b)) }
    }
    #[inline(always)]
    unsafe fn unzip(a: float64x2_t, b: float64x2_t) -> (float64x2_t, float64x2_t) {
        // SAFETY: see above.
        unsafe { (vuzp1q_f64(a, b), vuzp2q_f64(a, b)) }
    }
    #[inline(always)]
    unsafe fn dup_even(a: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vtrn1q_f64(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vtrn2q_f64(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: float64x2_t) -> float64x2_t {
        // SAFETY: see above.
        unsafe { vextq_f64::<1>(a, a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: float64x2_t) -> f64 {
        // SAFETY: see above.
        unsafe { vgetq_lane_f64::<0>(a) + vgetq_lane_f64::<1>(a) }
    }
}

impl Widen for Neon64 {
    #[inline(always)]
    unsafe fn load_widened(p: *const f32) -> [float64x2_t; 4] {
        // SAFETY: see above; `p` points to 8 elements.
        unsafe {
            let (low, high) = (vld1q_f32(p), vld1q_f32(p.add(4)));
            [
                vcvt_f64_f32(vget_low_f32(low)),
                vcvt_high_f64_f32(low),
                vcvt_f64_f32(vget_low_f32(high)),
                vcvt_high_f64_f32(high),
            ]
        }
    }
}

entry_points!("neon", Neon32, Neon64);

/// 16-bit integers to `f32`: 8 at a time, widened, converted exactly and
/// scaled by 2^-15. Loaded as bytes, which need no alignment.
#[cfg(target_endian = "little")]
#[target_feature(enable = "neon")]
pub(super) unsafe fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads bytes `2i..2i + 16` and writes elements
    // `i..i + 8`, all below `body`.
    unsafe {
        let scale = vdupq_n_f32(1.0 / 32_768.0);
        let mut i = 0;
        while i < body {
            let v = vreinterpretq_s16_u8(vld1q_u8(s.add(2 * i)));
            let low = vcvtq_f32_s32(vmovl_s16(vget_low_s16(v)));
            let high = vcvtq_f32_s32(vmovl_high_s16(v));
            vst1q_f32(d.add(i), vmulq_f32(low, scale));
            vst1q_f32(d.add(i + 4), vmulq_f32(high, scale));
            i += 8;
        }
    }
    scalar::i16_le_to_f32(&mut dst[body..], &src[2 * body..]);
}

/// 32-bit integers to `f32`: 4 at a time; the conversion rounds to the
/// nearest (the default mode) and the scaling by 2^-31 is exact.
#[cfg(target_endian = "little")]
#[target_feature(enable = "neon")]
pub(super) unsafe fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 4;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads bytes `4i..4i + 16`, below `4 * body`.
    unsafe {
        let scale = vdupq_n_f32(1.0 / 2_147_483_648.0);
        let mut i = 0;
        while i < body {
            let v = vreinterpretq_s32_u8(vld1q_u8(s.add(4 * i)));
            vst1q_f32(d.add(i), vmulq_f32(vcvtq_f32_s32(v), scale));
            i += 4;
        }
    }
    scalar::i32_le_to_f32(&mut dst[body..], &src[4 * body..]);
}

/// `f32` to 16-bit integers: 8 at a time. NEON's minimum and maximum keep a
/// NaN, and its conversion that rounds to the nearest with halves away
/// from zero (`FCVTAS`, `f32::round`'s rounding) makes a NaN 0, as `as`
/// does.
#[cfg(target_endian = "little")]
#[target_feature(enable = "neon")]
pub(super) unsafe fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
    let body = src.len() - src.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads elements `i..i + 8` and writes bytes
    // `2i..2i + 16`, all below `body`.
    unsafe {
        let (one, minus_one) = (vdupq_n_f32(1.0), vdupq_n_f32(-1.0));
        let full_scale = vdupq_n_f32(32_767.0);
        let quantise = |x: float32x4_t| {
            vqmovn_s32(vcvtaq_s32_f32(vmulq_f32(
                vmaxq_f32(vminq_f32(x, one), minus_one),
                full_scale,
            )))
        };
        let mut i = 0;
        while i < body {
            let low = quantise(vld1q_f32(s.add(i)));
            let high = quantise(vld1q_f32(s.add(i + 4)));
            vst1q_u8(d.add(2 * i), vreinterpretq_u8_s16(vcombine_s16(low, high)));
            i += 8;
        }
    }
    scalar::f32_to_i16_le(&mut dst[2 * body..], &src[body..]);
}

// The 24-bit conversions, and every conversion on a big-endian processor,
// are left to the definitions.

#[cfg(target_endian = "big")]
pub(super) unsafe fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
    scalar::i16_le_to_f32(dst, src);
}

#[cfg(target_endian = "big")]
pub(super) unsafe fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
    scalar::i32_le_to_f32(dst, src);
}

#[cfg(target_endian = "big")]
pub(super) unsafe fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
    scalar::f32_to_i16_le(dst, src);
}

pub(super) unsafe fn i24_le_to_f32(dst: &mut [f32], src: &[u8]) {
    scalar::i24_le_to_f32(dst, src);
}

pub(super) unsafe fn f32_to_i24_le(dst: &mut [u8], src: &[f32]) {
    scalar::f32_to_i24_le(dst, src);
}
