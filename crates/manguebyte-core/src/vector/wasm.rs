//! WebAssembly's 128-bit SIMD, in a module built with `-C
//! target-feature=+simd128`.
//!
//! The instructions are those of the WebAssembly SIMD specification
//! (<https://github.com/WebAssembly/simd/blob/main/proposals/simd/SIMD.md>,
//! part of WebAssembly 2.0), as `core::arch::wasm32` exposes them. They
//! are safe to call in such a module; only loads and stores, which take
//! pointers, are `unsafe`.

use core::arch::wasm32::*;

use super::generic::{Vector, Widen, entry_points};
use super::scalar;

/// `f32` in SIMD registers.
#[derive(Clone, Copy)]
pub(super) struct Wasm32;
/// `f64` in SIMD registers.
#[derive(Clone, Copy)]
pub(super) struct Wasm64;

impl Vector for Wasm32 {
    type T = f32;
    type Reg = v128;
    const LANES: usize = 4;

    #[inline(always)]
    unsafe fn load(p: *const f32) -> v128 {
        // SAFETY: the caller passes a valid pointer to four elements;
        // WebAssembly loads need no alignment.
        unsafe { v128_load(p.cast()) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f32, v: v128) {
        // SAFETY: as for `load`.
        unsafe { v128_store(p.cast(), v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f32) -> v128 {
        f32x4_splat(x)
    }
    #[inline(always)]
    unsafe fn add(a: v128, b: v128) -> v128 {
        f32x4_add(a, b)
    }
    #[inline(always)]
    unsafe fn mul(a: v128, b: v128) -> v128 {
        f32x4_mul(a, b)
    }
    #[inline(always)]
    unsafe fn abs(a: v128) -> v128 {
        f32x4_abs(a)
    }
    #[inline(always)]
    unsafe fn max(a: v128, b: v128) -> v128 {
        f32x4_max(a, b)
    }
    #[inline(always)]
    unsafe fn nan_mask(a: v128) -> v128 {
        f32x4_ne(a, a)
    }
    #[inline(always)]
    unsafe fn or(a: v128, b: v128) -> v128 {
        v128_or(a, b)
    }
    #[inline(always)]
    unsafe fn xor(a: v128, b: v128) -> v128 {
        v128_xor(a, b)
    }
    #[inline(always)]
    unsafe fn any(mask: v128) -> bool {
        v128_any_true(mask)
    }
    #[inline(always)]
    unsafe fn zip(a: v128, b: v128) -> (v128, v128) {
        (
            i32x4_shuffle::<0, 4, 1, 5>(a, b),
            i32x4_shuffle::<2, 6, 3, 7>(a, b),
        )
    }
    #[inline(always)]
    unsafe fn unzip(a: v128, b: v128) -> (v128, v128) {
        (
            i32x4_shuffle::<0, 2, 4, 6>(a, b),
            i32x4_shuffle::<1, 3, 5, 7>(a, b),
        )
    }
    #[inline(always)]
    unsafe fn dup_even(a: v128) -> v128 {
        i32x4_shuffle::<0, 0, 2, 2>(a, a)
    }
    #[inline(always)]
    unsafe fn dup_odd(a: v128) -> v128 {
        i32x4_shuffle::<1, 1, 3, 3>(a, a)
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: v128) -> v128 {
        i32x4_shuffle::<1, 0, 3, 2>(a, a)
    }
    #[inline(always)]
    unsafe fn reduce(a: v128) -> f32 {
        // `(a0 + a2) + (a1 + a3)`.
        let pairs = f32x4_add(a, i32x4_shuffle::<2, 3, 2, 3>(a, a));
        f32x4_extract_lane::<0>(pairs) + f32x4_extract_lane::<1>(pairs)
    }
}

impl Vector for Wasm64 {
    type T = f64;
    type Reg = v128;
    const LANES: usize = 2;

    #[inline(always)]
    unsafe fn load(p: *const f64) -> v128 {
        // SAFETY: as for `Wasm32::load`.
        unsafe { v128_load(p.cast()) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f64, v: v128) {
        // SAFETY: as for `Wasm32::load`.
        unsafe { v128_store(p.cast(), v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f64) -> v128 {
        f64x2_splat(x)
    }
    #[inline(always)]
    unsafe fn add(a: v128, b: v128) -> v128 {
        f64x2_add(a, b)
    }
    #[inline(always)]
    unsafe fn mul(a: v128, b: v128) -> v128 {
        f64x2_mul(a, b)
    }
    #[inline(always)]
    unsafe fn abs(a: v128) -> v128 {
        f64x2_abs(a)
    }
    #[inline(always)]
    unsafe fn max(a: v128, b: v128) -> v128 {
        f64x2_max(a, b)
    }
    #[inline(always)]
    unsafe fn nan_mask(a: v128) -> v128 {
        f64x2_ne(a, a)
    }
    #[inline(always)]
    unsafe fn or(a: v128, b: v128) -> v128 {
        v128_or(a, b)
    }
    #[inline(always)]
    unsafe fn xor(a: v128, b: v128) -> v128 {
        v128_xor(a, b)
    }
    #[inline(always)]
    unsafe fn any(mask: v128) -> bool {
        v128_any_true(mask)
    }
    #[inline(always)]
    unsafe fn zip(a: v128, b: v128) -> (v128, v128) {
        (i64x2_shuffle::<0, 2>(a, b), i64x2_shuffle::<1, 3>(a, b))
    }
    #[inline(always)]
    unsafe fn unzip(a: v128, b: v128) -> (v128, v128) {
        // With two lanes, splitting pairs is the same shuffle as making
        // them.
        (i64x2_shuffle::<0, 2>(a, b), i64x2_shuffle::<1, 3>(a, b))
    }
    #[inline(always)]
    unsafe fn dup_even(a: v128) -> v128 {
        i64x2_shuffle::<0, 0>(a, a)
    }
    #[inline(always)]
    unsafe fn dup_odd(a: v128) -> v128 {
        i64x2_shuffle::<1, 1>(a, a)
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: v128) -> v128 {
        i64x2_shuffle::<1, 0>(a, a)
    }
    #[inline(always)]
    unsafe fn reduce(a: v128) -> f64 {
        f64x2_extract_lane::<0>(a) + f64x2_extract_lane::<1>(a)
    }
}

impl Widen for Wasm64 {
    #[inline(always)]
    unsafe fn load_widened(p: *const f32) -> [v128; 4] {
        // SAFETY: `p` points to 8 elements.
        let (low, high) = unsafe { (v128_load(p.cast()), v128_load(p.add(4).cast())) };
        [
            f64x2_promote_low_f32x4(low),
            f64x2_promote_low_f32x4(i64x2_shuffle::<1, 1>(low, low)),
            f64x2_promote_low_f32x4(high),
            f64x2_promote_low_f32x4(i64x2_shuffle::<1, 1>(high, high)),
        ]
    }
}

entry_points!("simd128", Wasm32, Wasm64);

/// 16-bit integers to `f32`: 8 at a time, widened, converted exactly and
/// scaled by 2^-15.
#[target_feature(enable = "simd128")]
pub(super) unsafe fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    let scale = f32x4_splat(1.0 / 32_768.0);
    let mut i = 0;
    while i < body {
        // SAFETY: element `i` reads bytes `2i..2i + 16` and writes elements
        // `i..i + 8`, all below `body`.
        unsafe {
            let v = v128_load(s.add(2 * i).cast());
            let low = f32x4_convert_i32x4(i32x4_extend_low_i16x8(v));
            let high = f32x4_convert_i32x4(i32x4_extend_high_i16x8(v));
            v128_store(d.add(i).cast(), f32x4_mul(low, scale));
            v128_store(d.add(i + 4).cast(), f32x4_mul(high, scale));
        }
        i += 8;
    }
    scalar::i16_le_to_f32(&mut dst[body..], &src[2 * body..]);
}

/// 32-bit integers to `f32`: 4 at a time; the conversion rounds to the
/// nearest and the scaling by 2^-31 is exact.
#[target_feature(enable = "simd128")]
pub(super) unsafe fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 4;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    let scale = f32x4_splat(1.0 / 2_147_483_648.0);
    let mut i = 0;
    while i < body {
        // SAFETY: element `i` reads bytes `4i..4i + 16`, below `4 * body`.
        unsafe {
            let v = f32x4_convert_i32x4(v128_load(s.add(4 * i).cast()));
            v128_store(d.add(i).cast(), f32x4_mul(v, scale));
        }
        i += 4;
    }
    scalar::i32_le_to_f32(&mut dst[body..], &src[4 * body..]);
}

/// Four samples made 16-bit integers, in 32-bit lanes: a NaN made 0,
/// clamped, multiplied by 32767, truncated and then moved one away from
/// zero where the part cut off, which is exact, was a half or more, which
/// is `f32::round`.
#[inline(always)]
fn quantise_i16(x: v128) -> v128 {
    let x = v128_and(x, f32x4_eq(x, x));
    let y = f32x4_mul(
        f32x4_max(f32x4_min(x, f32x4_splat(1.0)), f32x4_splat(-1.0)),
        f32x4_splat(32_767.0),
    );
    let truncated = i32x4_trunc_sat_f32x4(y);
    let fraction = f32x4_sub(y, f32x4_convert_i32x4(truncated));
    let one = i32x4_splat(1);
    let up = v128_and(f32x4_ge(fraction, f32x4_splat(0.5)), one);
    let down = v128_and(f32x4_le(fraction, f32x4_splat(-0.5)), one);
    i32x4_sub(i32x4_add(truncated, up), down)
}

/// `f32` to 16-bit integers: 8 at a time.
#[target_feature(enable = "simd128")]
pub(super) unsafe fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
    let body = src.len() - src.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    let mut i = 0;
    while i < body {
        // SAFETY: element `i` reads elements `i..i + 8` and writes bytes
        // `2i..2i + 16`, all below `body`.
        unsafe {
            let low = quantise_i16(v128_load(s.add(i).cast()));
            let high = quantise_i16(v128_load(s.add(i + 4).cast()));
            v128_store(d.add(2 * i).cast(), i16x8_narrow_i32x4(low, high));
        }
        i += 8;
    }
    scalar::f32_to_i16_le(&mut dst[2 * body..], &src[body..]);
}

// The 24-bit conversions are left to the definitions.

pub(super) unsafe fn i24_le_to_f32(dst: &mut [f32], src: &[u8]) {
    scalar::i24_le_to_f32(dst, src);
}

pub(super) unsafe fn f32_to_i24_le(dst: &mut [u8], src: &[f32]) {
    scalar::f32_to_i24_le(dst, src);
}
