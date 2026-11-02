//! SSE2, AVX and AVX2, on x86 and x86-64.
//!
//! The instructions are those of Intel's *Intrinsics Guide*
//! (<https://www.intel.com/content/www/us/en/docs/intrinsics-guide/>) and
//! the *Intel 64 and IA-32 Architectures Software Developer's Manual*,
//! volume 2, as `core::arch` exposes them.
//!
//! Rust 1.87 made most of these intrinsics safe to call where their feature
//! is enabled. The `unsafe` blocks around them keep the crate building with
//! its minimum Rust version, 1.85, and are allowed to be unneeded on newer
//! ones.
#![allow(unused_unsafe)]

#[cfg(target_arch = "x86")]
use core::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;

use super::generic::{Vector, Widen};
use super::scalar;

/// `f32` in SSE registers.
#[derive(Clone, Copy)]
pub(super) struct Sse32;
/// `f64` in SSE2 registers.
#[derive(Clone, Copy)]
pub(super) struct Sse64;
/// `f32` in AVX registers.
#[derive(Clone, Copy)]
pub(super) struct Avx32;
/// `f64` in AVX registers.
#[derive(Clone, Copy)]
pub(super) struct Avx64;

// SAFETY (for every method below): the caller knows the processor has the
// instruction set of the register, which is all the methods require;
// loads and stores need only valid pointers to `LANES` elements, which
// the caller also guarantees.

impl Vector for Sse32 {
    type T = f32;
    type Reg = __m128;
    const LANES: usize = 4;

    #[inline(always)]
    unsafe fn load(p: *const f32) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_loadu_ps(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f32, v: __m128) {
        // SAFETY: see above.
        unsafe { _mm_storeu_ps(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f32) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_set1_ps(x) }
    }
    #[inline(always)]
    unsafe fn add(a: __m128, b: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_add_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: __m128, b: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_mul_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_andnot_ps(_mm_set1_ps(-0.0), a) }
    }
    #[inline(always)]
    unsafe fn max(a: __m128, b: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_max_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_cmpunord_ps(a, a) }
    }
    #[inline(always)]
    unsafe fn or(a: __m128, b: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_or_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn xor(a: __m128, b: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_xor_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn any(mask: __m128) -> bool {
        // SAFETY: see above.
        unsafe { _mm_movemask_ps(mask) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: __m128, b: __m128) -> (__m128, __m128) {
        // SAFETY: see above.
        unsafe { (_mm_unpacklo_ps(a, b), _mm_unpackhi_ps(a, b)) }
    }
    #[inline(always)]
    unsafe fn unzip(a: __m128, b: __m128) -> (__m128, __m128) {
        // SAFETY: see above.
        unsafe {
            (
                _mm_shuffle_ps::<0b10_00_10_00>(a, b),
                _mm_shuffle_ps::<0b11_01_11_01>(a, b),
            )
        }
    }
    #[inline(always)]
    unsafe fn dup_even(a: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_shuffle_ps::<0b10_10_00_00>(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_shuffle_ps::<0b11_11_01_01>(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: __m128) -> __m128 {
        // SAFETY: see above.
        unsafe { _mm_shuffle_ps::<0b10_11_00_01>(a, a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: __m128) -> f32 {
        // SAFETY: see above.
        unsafe { reduce_ps(a) }
    }
}

/// `(a0 + a2) + (a1 + a3)`.
#[inline(always)]
unsafe fn reduce_ps(a: __m128) -> f32 {
    // SAFETY: the caller knows the processor has SSE.
    unsafe {
        let pairs = _mm_add_ps(a, _mm_movehl_ps(a, a));
        _mm_cvtss_f32(_mm_add_ss(pairs, _mm_shuffle_ps::<0b01>(pairs, pairs)))
    }
}

/// `a0 + a1`.
#[inline(always)]
unsafe fn reduce_pd(a: __m128d) -> f64 {
    // SAFETY: the caller knows the processor has SSE2.
    unsafe { _mm_cvtsd_f64(_mm_add_sd(a, _mm_unpackhi_pd(a, a))) }
}

impl Vector for Sse64 {
    type T = f64;
    type Reg = __m128d;
    const LANES: usize = 2;

    #[inline(always)]
    unsafe fn load(p: *const f64) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_loadu_pd(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f64, v: __m128d) {
        // SAFETY: see above.
        unsafe { _mm_storeu_pd(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f64) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_set1_pd(x) }
    }
    #[inline(always)]
    unsafe fn add(a: __m128d, b: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_add_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: __m128d, b: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_mul_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_andnot_pd(_mm_set1_pd(-0.0), a) }
    }
    #[inline(always)]
    unsafe fn max(a: __m128d, b: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_max_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_cmpunord_pd(a, a) }
    }
    #[inline(always)]
    unsafe fn or(a: __m128d, b: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_or_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn xor(a: __m128d, b: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_xor_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn any(mask: __m128d) -> bool {
        // SAFETY: see above.
        unsafe { _mm_movemask_pd(mask) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: __m128d, b: __m128d) -> (__m128d, __m128d) {
        // SAFETY: see above.
        unsafe { (_mm_unpacklo_pd(a, b), _mm_unpackhi_pd(a, b)) }
    }
    #[inline(always)]
    unsafe fn unzip(a: __m128d, b: __m128d) -> (__m128d, __m128d) {
        // With two lanes, splitting pairs is the same shuffle as making
        // them.
        // SAFETY: see above.
        unsafe { (_mm_unpacklo_pd(a, b), _mm_unpackhi_pd(a, b)) }
    }
    #[inline(always)]
    unsafe fn dup_even(a: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_unpacklo_pd(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_unpackhi_pd(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: __m128d) -> __m128d {
        // SAFETY: see above.
        unsafe { _mm_shuffle_pd::<0b01>(a, a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: __m128d) -> f64 {
        // SAFETY: see above.
        unsafe { reduce_pd(a) }
    }
}

impl Widen for Sse64 {
    #[inline(always)]
    unsafe fn load_widened(p: *const f32) -> [__m128d; 4] {
        // SAFETY: see above; `p` points to 8 elements.
        unsafe {
            let (low, high) = (_mm_loadu_ps(p), _mm_loadu_ps(p.add(4)));
            [
                _mm_cvtps_pd(low),
                _mm_cvtps_pd(_mm_movehl_ps(low, low)),
                _mm_cvtps_pd(high),
                _mm_cvtps_pd(_mm_movehl_ps(high, high)),
            ]
        }
    }
}

impl Vector for Avx32 {
    type T = f32;
    type Reg = __m256;
    const LANES: usize = 8;

    #[inline(always)]
    unsafe fn load(p: *const f32) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_loadu_ps(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f32, v: __m256) {
        // SAFETY: see above.
        unsafe { _mm256_storeu_ps(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f32) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_set1_ps(x) }
    }
    #[inline(always)]
    unsafe fn add(a: __m256, b: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_add_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: __m256, b: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_mul_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_andnot_ps(_mm256_set1_ps(-0.0), a) }
    }
    #[inline(always)]
    unsafe fn max(a: __m256, b: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_max_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_cmp_ps::<_CMP_UNORD_Q>(a, a) }
    }
    #[inline(always)]
    unsafe fn or(a: __m256, b: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_or_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn xor(a: __m256, b: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_xor_ps(a, b) }
    }
    #[inline(always)]
    unsafe fn any(mask: __m256) -> bool {
        // SAFETY: see above.
        unsafe { _mm256_movemask_ps(mask) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: __m256, b: __m256) -> (__m256, __m256) {
        // The unpacks work within each 128-bit half; the permutes put the
        // halves in order.
        // SAFETY: see above.
        unsafe {
            let (low, high) = (_mm256_unpacklo_ps(a, b), _mm256_unpackhi_ps(a, b));
            (
                _mm256_permute2f128_ps::<0x20>(low, high),
                _mm256_permute2f128_ps::<0x31>(low, high),
            )
        }
    }
    #[inline(always)]
    unsafe fn unzip(a: __m256, b: __m256) -> (__m256, __m256) {
        // SAFETY: see above.
        unsafe {
            let (low, high) = (
                _mm256_permute2f128_ps::<0x20>(a, b),
                _mm256_permute2f128_ps::<0x31>(a, b),
            );
            (
                _mm256_shuffle_ps::<0b10_00_10_00>(low, high),
                _mm256_shuffle_ps::<0b11_01_11_01>(low, high),
            )
        }
    }
    #[inline(always)]
    unsafe fn dup_even(a: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_shuffle_ps::<0b10_10_00_00>(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_shuffle_ps::<0b11_11_01_01>(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: __m256) -> __m256 {
        // SAFETY: see above.
        unsafe { _mm256_shuffle_ps::<0b10_11_00_01>(a, a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: __m256) -> f32 {
        // Lanes `j` and `j + 4` first: the two halves.
        // SAFETY: see above.
        unsafe {
            reduce_ps(_mm_add_ps(
                _mm256_castps256_ps128(a),
                _mm256_extractf128_ps::<1>(a),
            ))
        }
    }
}

impl Vector for Avx64 {
    type T = f64;
    type Reg = __m256d;
    const LANES: usize = 4;

    #[inline(always)]
    unsafe fn load(p: *const f64) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_loadu_pd(p) }
    }
    #[inline(always)]
    unsafe fn store(p: *mut f64, v: __m256d) {
        // SAFETY: see above.
        unsafe { _mm256_storeu_pd(p, v) }
    }
    #[inline(always)]
    unsafe fn splat(x: f64) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_set1_pd(x) }
    }
    #[inline(always)]
    unsafe fn add(a: __m256d, b: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_add_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn mul(a: __m256d, b: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_mul_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn abs(a: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_andnot_pd(_mm256_set1_pd(-0.0), a) }
    }
    #[inline(always)]
    unsafe fn max(a: __m256d, b: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_max_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn nan_mask(a: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_cmp_pd::<_CMP_UNORD_Q>(a, a) }
    }
    #[inline(always)]
    unsafe fn or(a: __m256d, b: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_or_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn xor(a: __m256d, b: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_xor_pd(a, b) }
    }
    #[inline(always)]
    unsafe fn any(mask: __m256d) -> bool {
        // SAFETY: see above.
        unsafe { _mm256_movemask_pd(mask) != 0 }
    }
    #[inline(always)]
    unsafe fn zip(a: __m256d, b: __m256d) -> (__m256d, __m256d) {
        // SAFETY: see above.
        unsafe {
            let (low, high) = (_mm256_unpacklo_pd(a, b), _mm256_unpackhi_pd(a, b));
            (
                _mm256_permute2f128_pd::<0x20>(low, high),
                _mm256_permute2f128_pd::<0x31>(low, high),
            )
        }
    }
    #[inline(always)]
    unsafe fn unzip(a: __m256d, b: __m256d) -> (__m256d, __m256d) {
        // SAFETY: see above.
        unsafe {
            let (low, high) = (
                _mm256_permute2f128_pd::<0x20>(a, b),
                _mm256_permute2f128_pd::<0x31>(a, b),
            );
            (_mm256_unpacklo_pd(low, high), _mm256_unpackhi_pd(low, high))
        }
    }
    #[inline(always)]
    unsafe fn dup_even(a: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_unpacklo_pd(a, a) }
    }
    #[inline(always)]
    unsafe fn dup_odd(a: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_unpackhi_pd(a, a) }
    }
    #[inline(always)]
    unsafe fn swap_pairs(a: __m256d) -> __m256d {
        // SAFETY: see above.
        unsafe { _mm256_permute_pd::<0b0101>(a) }
    }
    #[inline(always)]
    unsafe fn reduce(a: __m256d) -> f64 {
        // SAFETY: see above.
        unsafe {
            reduce_pd(_mm_add_pd(
                _mm256_castpd256_pd128(a),
                _mm256_extractf128_pd::<1>(a),
            ))
        }
    }
}

impl Widen for Avx64 {
    #[inline(always)]
    unsafe fn load_widened(p: *const f32) -> [__m256d; 4] {
        // SAFETY: see above; `p` points to 8 elements. The last two
        // registers are not used with four lanes.
        unsafe {
            let low = _mm256_cvtps_pd(_mm_loadu_ps(p));
            let high = _mm256_cvtps_pd(_mm_loadu_ps(p.add(4)));
            [low, high, low, high]
        }
    }
}

// The conversions, shared by the backends. `$feature` is the instruction
// set they are compiled for; the SSE2 versions are also compiled for AVX,
// whose encoding of the same instructions is shorter.

/// 16-bit integers to `f32`: 8 at a time. The integers are sign-extended
/// to 32 bits by unpacking each next to itself and shifting the copy
/// down, converted exactly, and scaled by 2^-15, which is exact too: the
/// same as the definition's division by 32768.
#[inline(always)]
unsafe fn i16_le_to_f32_sse2(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads bytes `2i..2i + 16` and writes elements
    // `i..i + 8`, all below `body` in both slices; the caller knows the
    // processor has SSE2.
    unsafe {
        let scale = _mm_set1_ps(1.0 / 32_768.0);
        let mut i = 0;
        while i < body {
            let v = _mm_loadu_si128(s.add(2 * i).cast());
            let low = _mm_srai_epi32::<16>(_mm_unpacklo_epi16(v, v));
            let high = _mm_srai_epi32::<16>(_mm_unpackhi_epi16(v, v));
            _mm_storeu_ps(d.add(i), _mm_mul_ps(_mm_cvtepi32_ps(low), scale));
            _mm_storeu_ps(d.add(i + 4), _mm_mul_ps(_mm_cvtepi32_ps(high), scale));
            i += 8;
        }
    }
    scalar::i16_le_to_f32(&mut dst[body..], &src[2 * body..]);
}

/// 24-bit integers to `f32`: 4 at a time. Each is read as the 32 bits
/// starting at its first byte (the last of these reaches one byte past the
/// group, so the loop stops a byte early), shifted up by 8 and back down
/// arithmetically, which drops the next sample's byte and sign-extends,
/// converted exactly and scaled by 2^-23.
#[inline(always)]
unsafe fn i24_le_to_f32_sse2(dst: &mut [f32], src: &[u8]) {
    let groups = src.len().saturating_sub(1) / 12;
    let body = groups * 4;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: group `g` reads bytes `12g..12g + 13`, below `src.len()` by
    // the choice of `groups`, and writes elements below `body`; the caller
    // knows the processor has SSE2.
    unsafe {
        let scale = _mm_set1_ps(1.0 / 8_388_608.0);
        let mut i = 0;
        while i < body {
            let at = s.add(3 * i);
            let word = |offset: usize| at.add(offset).cast::<i32>().read_unaligned();
            let v = _mm_setr_epi32(word(0), word(3), word(6), word(9));
            let v = _mm_srai_epi32::<8>(_mm_slli_epi32::<8>(v));
            _mm_storeu_ps(d.add(i), _mm_mul_ps(_mm_cvtepi32_ps(v), scale));
            i += 4;
        }
    }
    scalar::i24_le_to_f32(&mut dst[body..], &src[3 * body..]);
}

/// 32-bit integers to `f32`: 4 at a time. The conversion rounds to the
/// nearest `f32` (the processor's default), and scaling by 2^-31 after it
/// is exact, so this is the definition's single rounding of
/// `value / 2^31`.
#[inline(always)]
unsafe fn i32_le_to_f32_sse2(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 4;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads bytes `4i..4i + 16`, below `4 * body`; the
    // caller knows the processor has SSE2.
    unsafe {
        let scale = _mm_set1_ps(1.0 / 2_147_483_648.0);
        let mut i = 0;
        while i < body {
            let v = _mm_loadu_si128(s.add(4 * i).cast());
            _mm_storeu_ps(d.add(i), _mm_mul_ps(_mm_cvtepi32_ps(v), scale));
            i += 4;
        }
    }
    scalar::i32_le_to_f32(&mut dst[body..], &src[4 * body..]);
}

/// Rounds to the nearest integer, halves away from zero, as `f32::round`:
/// truncates, then moves one away from zero where the part cut off, which
/// is exact, was a half or more. `y` must lie within ±2^31.
#[inline(always)]
unsafe fn round_away_ps(y: __m128) -> __m128i {
    // SAFETY: the caller knows the processor has SSE2.
    unsafe {
        let truncated = _mm_cvttps_epi32(y);
        let fraction = _mm_sub_ps(y, _mm_cvtepi32_ps(truncated));
        let one = _mm_set1_epi32(1);
        let up = _mm_and_si128(
            _mm_castps_si128(_mm_cmpge_ps(fraction, _mm_set1_ps(0.5))),
            one,
        );
        let down = _mm_and_si128(
            _mm_castps_si128(_mm_cmple_ps(fraction, _mm_set1_ps(-0.5))),
            one,
        );
        _mm_sub_epi32(_mm_add_epi32(truncated, up), down)
    }
}

/// A NaN made 0, then clamped to `-1.0..=1.0`, as `f32::clamp` does
/// (which keeps a NaN, that `as` then makes 0).
#[inline(always)]
unsafe fn clamp_unit_ps(x: __m128) -> __m128 {
    // SAFETY: the caller knows the processor has SSE2.
    unsafe {
        let x = _mm_and_ps(x, _mm_cmpord_ps(x, x));
        _mm_max_ps(_mm_min_ps(x, _mm_set1_ps(1.0)), _mm_set1_ps(-1.0))
    }
}

/// `f32` to 16-bit integers: 8 at a time, clamped, multiplied by 32767 in
/// `f32` as the definition does, rounded halves away from zero and packed
/// (the values already fit, so the saturation never acts).
#[inline(always)]
unsafe fn f32_to_i16_le_sse2(dst: &mut [u8], src: &[f32]) {
    let body = src.len() - src.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: element `i` reads elements `i..i + 8` and writes bytes
    // `2i..2i + 16`, all below `body`; the caller knows the processor has
    // SSE2.
    unsafe {
        let full_scale = _mm_set1_ps(32_767.0);
        let mut i = 0;
        while i < body {
            let low = round_away_ps(_mm_mul_ps(
                clamp_unit_ps(_mm_loadu_ps(s.add(i))),
                full_scale,
            ));
            let high = round_away_ps(_mm_mul_ps(
                clamp_unit_ps(_mm_loadu_ps(s.add(i + 4))),
                full_scale,
            ));
            _mm_storeu_si128(d.add(2 * i).cast(), _mm_packs_epi32(low, high));
            i += 8;
        }
    }
    scalar::f32_to_i16_le(&mut dst[2 * body..], &src[body..]);
}

/// Two `f64` rounded to the nearest integer, halves away from zero, as
/// whole numbers in `f64`. Exact, as in `round_away_ps`.
#[inline(always)]
unsafe fn round_away_pd(y: __m128d) -> __m128d {
    // SAFETY: the caller knows the processor has SSE2.
    unsafe {
        let truncated = _mm_cvtepi32_pd(_mm_cvttpd_epi32(y));
        let fraction = _mm_sub_pd(y, truncated);
        let one = _mm_set1_pd(1.0);
        let up = _mm_and_pd(_mm_cmpge_pd(fraction, _mm_set1_pd(0.5)), one);
        let down = _mm_and_pd(_mm_cmple_pd(fraction, _mm_set1_pd(-0.5)), one);
        _mm_sub_pd(_mm_add_pd(truncated, up), down)
    }
}

/// `f32` to 24-bit integers: 4 at a time, clamped, widened and multiplied
/// by 8388607 in `f64` (exact), rounded halves away from zero; the bytes
/// are then written three to a sample.
#[inline(always)]
unsafe fn f32_to_i24_le_sse2(dst: &mut [u8], src: &[f32]) {
    let body = src.len() - src.len() % 4;
    let s = src.as_ptr();
    let mut values = [0i32; 4];
    for (i, out) in dst[..3 * body].chunks_exact_mut(12).enumerate() {
        // SAFETY: group `i` reads elements `4i..4i + 4`, below `body`, and
        // writes `values`, which holds four; the caller knows the processor
        // has SSE2.
        unsafe {
            let x = clamp_unit_ps(_mm_loadu_ps(s.add(4 * i)));
            let full_scale = _mm_set1_pd(8_388_607.0);
            let low = round_away_pd(_mm_mul_pd(_mm_cvtps_pd(x), full_scale));
            let high = round_away_pd(_mm_mul_pd(_mm_cvtps_pd(_mm_movehl_ps(x, x)), full_scale));
            let packed = _mm_unpacklo_epi64(_mm_cvttpd_epi32(low), _mm_cvttpd_epi32(high));
            _mm_storeu_si128(values.as_mut_ptr().cast(), packed);
        }
        for (bytes, value) in out.chunks_exact_mut(3).zip(values) {
            bytes.copy_from_slice(&value.to_le_bytes()[..3]);
        }
    }
    scalar::f32_to_i24_le(&mut dst[3 * body..], &src[body..]);
}

/// 16-bit integers to `f32` with AVX2: 8 at a time, sign-extended in one
/// instruction.
#[inline(always)]
unsafe fn i16_le_to_f32_avx2(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `i16_le_to_f32_sse2`; the caller knows the processor
    // has AVX2.
    unsafe {
        let scale = _mm256_set1_ps(1.0 / 32_768.0);
        let mut i = 0;
        while i < body {
            let v = _mm256_cvtepi16_epi32(_mm_loadu_si128(s.add(2 * i).cast()));
            _mm256_storeu_ps(d.add(i), _mm256_mul_ps(_mm256_cvtepi32_ps(v), scale));
            i += 8;
        }
    }
    scalar::i16_le_to_f32(&mut dst[body..], &src[2 * body..]);
}

/// 24-bit integers to `f32` with AVX2: 8 at a time. Two loads of 16 bytes,
/// 12 bytes apart, put four samples in each half of a register; a byte
/// shuffle moves each sample's three bytes to the top of a 32-bit lane,
/// and an arithmetic shift brings it down, sign-extended. The second load
/// reaches 4 bytes past the group, so the loop stops 4 bytes early.
#[inline(always)]
unsafe fn i24_le_to_f32_avx2(dst: &mut [f32], src: &[u8]) {
    let groups = src.len().saturating_sub(4) / 24;
    let body = groups * 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: group `g` reads bytes `24g..24g + 28`, below `src.len()` by
    // the choice of `groups`, and writes elements below `body`; the caller
    // knows the processor has AVX2.
    unsafe {
        let order = _mm256_setr_epi8(
            -1, 0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11, //
            -1, 0, 1, 2, -1, 3, 4, 5, -1, 6, 7, 8, -1, 9, 10, 11,
        );
        let scale = _mm256_set1_ps(1.0 / 8_388_608.0);
        let mut i = 0;
        while i < body {
            let at = s.add(3 * i);
            let low = _mm_loadu_si128(at.cast());
            let high = _mm_loadu_si128(at.add(12).cast());
            let v = _mm256_shuffle_epi8(_mm256_set_m128i(high, low), order);
            let v = _mm256_srai_epi32::<8>(v);
            _mm256_storeu_ps(d.add(i), _mm256_mul_ps(_mm256_cvtepi32_ps(v), scale));
            i += 8;
        }
    }
    scalar::i24_le_to_f32(&mut dst[body..], &src[3 * body..]);
}

/// 32-bit integers to `f32` with AVX: 8 at a time, as in
/// `i32_le_to_f32_sse2`.
#[inline(always)]
unsafe fn i32_le_to_f32_avx(dst: &mut [f32], src: &[u8]) {
    let body = dst.len() - dst.len() % 8;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `i32_le_to_f32_sse2`; the caller knows the processor
    // has AVX.
    unsafe {
        let scale = _mm256_set1_ps(1.0 / 2_147_483_648.0);
        let mut i = 0;
        while i < body {
            let v = _mm256_loadu_si256(s.add(4 * i).cast());
            _mm256_storeu_ps(d.add(i), _mm256_mul_ps(_mm256_cvtepi32_ps(v), scale));
            i += 8;
        }
    }
    scalar::i32_le_to_f32(&mut dst[body..], &src[4 * body..]);
}

/// Defines the conversions of one backend, each compiled for `$feature`.
macro_rules! conversions {
    (
        $feature:literal,
        i16 => $i16:ident, i24 => $i24:ident, i32 => $i32:ident,
        to_i16 => $to_i16:ident, to_i24 => $to_i24:ident $(,)?
    ) => {
        #[target_feature(enable = $feature)]
        pub(in crate::vector) unsafe fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { super::$i16(dst, src) }
        }
        #[target_feature(enable = $feature)]
        pub(in crate::vector) unsafe fn i24_le_to_f32(dst: &mut [f32], src: &[u8]) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { super::$i24(dst, src) }
        }
        #[target_feature(enable = $feature)]
        pub(in crate::vector) unsafe fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { super::$i32(dst, src) }
        }
        #[target_feature(enable = $feature)]
        pub(in crate::vector) unsafe fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { super::$to_i16(dst, src) }
        }
        #[target_feature(enable = $feature)]
        pub(in crate::vector) unsafe fn f32_to_i24_le(dst: &mut [u8], src: &[f32]) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { super::$to_i24(dst, src) }
        }
    };
}

pub(super) mod sse2 {
    use super::super::generic::entry_points;
    use super::{Sse32, Sse64};

    entry_points!("sse2", Sse32, Sse64);
    conversions!(
        "sse2",
        i16 => i16_le_to_f32_sse2,
        i24 => i24_le_to_f32_sse2,
        i32 => i32_le_to_f32_sse2,
        to_i16 => f32_to_i16_le_sse2,
        to_i24 => f32_to_i24_le_sse2,
    );
}

pub(super) mod avx {
    use super::super::generic::entry_points;
    use super::{Avx32, Avx64};

    entry_points!("avx", Avx32, Avx64);
    conversions!(
        "avx",
        i16 => i16_le_to_f32_sse2,
        i24 => i24_le_to_f32_sse2,
        i32 => i32_le_to_f32_avx,
        to_i16 => f32_to_i16_le_sse2,
        to_i24 => f32_to_i24_le_sse2,
    );
}

pub(super) mod avx2 {
    use super::super::generic::entry_points;
    use super::{Avx32, Avx64};

    entry_points!("avx,avx2", Avx32, Avx64);
    conversions!(
        "avx,avx2",
        i16 => i16_le_to_f32_avx2,
        i24 => i24_le_to_f32_avx2,
        i32 => i32_le_to_f32_avx,
        to_i16 => f32_to_i16_le_sse2,
        to_i24 => f32_to_i24_le_sse2,
    );
}
