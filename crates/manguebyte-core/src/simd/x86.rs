//! SSE2 and AVX, on x86 and x86-64.
//!
//! The instructions are those of Intel's *Intrinsics Guide*
//! (<https://www.intel.com/content/www/us/en/docs/intrinsics-guide/>), as
//! `core::arch` exposes them. `min` and `max` are `MINPS`/`MAXPS` as they
//! are: the second operand is returned when the first is not smaller
//! (larger), NaN and signed zeros included.
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

use super::raw::{Pair, Raw};
use super::register::Reg;
use super::{Isa, Kernel, sealed};
use crate::vector::Backend;

/// Proof that the processor has SSE2. Only [`sse2`] makes one.
#[derive(Debug, Clone, Copy)]
pub struct Sse2Isa(());

impl sealed::Sealed for Sse2Isa {}

impl Isa for Sse2Isa {
    type F32 = Reg<Self, Pair<__m128>>;
    type F64 = Reg<Self, Pair<__m128d>>;

    #[inline(always)]
    fn backend(self) -> Backend {
        Backend::Sse2
    }
}

/// Proof that the processor has AVX, and AVX2 if it says so. Only [`avx`]
/// and [`avx2`] make one.
#[derive(Debug, Clone, Copy)]
pub struct AvxIsa {
    avx2: bool,
}

impl sealed::Sealed for AvxIsa {}

impl Isa for AvxIsa {
    type F32 = Reg<Self, __m256>;
    type F64 = Reg<Self, __m256d>;

    #[inline(always)]
    fn backend(self) -> Backend {
        if self.avx2 {
            Backend::Avx2
        } else {
            Backend::Avx
        }
    }
}

/// Runs a kernel compiled for SSE2.
///
/// # Safety
///
/// The processor must have SSE2.
#[target_feature(enable = "sse2")]
pub(super) unsafe fn sse2<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(Sse2Isa(()))
}

/// Runs a kernel compiled for AVX.
///
/// # Safety
///
/// The processor must have AVX.
#[target_feature(enable = "avx")]
pub(super) unsafe fn avx<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(AvxIsa { avx2: false })
}

/// Runs a kernel compiled for AVX and AVX2.
///
/// # Safety
///
/// The processor must have AVX and AVX2.
#[target_feature(enable = "avx,avx2")]
pub(super) unsafe fn avx2<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(AvxIsa { avx2: true })
}

/// Implements [`Raw`] for one register type out of the intrinsics named.
macro_rules! x86_raw {
    (
        $reg:ty, $t:ty, $lanes:literal,
        splat: $splat:ident,
        load: $load:ident, $load_aligned:ident,
        store: $store:ident, $store_aligned:ident,
        arithmetic: $add:ident, $sub:ident, $mul:ident, $div:ident, $sqrt:ident,
        order: $min:ident, $max:ident,
        bits: $and:ident, $or:ident, $xor:ident, $andnot:ident,
        movemask: $movemask:ident,
        compare: { $($compare:tt)* },
        select: $select:ident $(,)?
    ) => {
        // SAFETY (for every method): the caller knows the processor has the
        // instruction set of the register, which is all the methods
        // require; loads and stores need valid pointers to `LANES`
        // elements, aligned where the name says so, which the caller also
        // guarantees.
        impl Raw for $reg {
            type T = $t;
            type M = $reg;
            const LANES: usize = $lanes;

            #[inline(always)]
            unsafe fn splat(x: $t) -> Self {
                // SAFETY: see above.
                unsafe { $splat(x) }
            }
            #[inline(always)]
            unsafe fn load(p: *const $t) -> Self {
                // SAFETY: see above.
                unsafe { $load(p) }
            }
            #[inline(always)]
            unsafe fn load_aligned(p: *const $t) -> Self {
                // SAFETY: see above.
                unsafe { $load_aligned(p) }
            }
            #[inline(always)]
            unsafe fn store(self, p: *mut $t) {
                // SAFETY: see above.
                unsafe { $store(p, self) }
            }
            #[inline(always)]
            unsafe fn store_aligned(self, p: *mut $t) {
                // SAFETY: see above.
                unsafe { $store_aligned(p, self) }
            }
            #[inline(always)]
            unsafe fn add(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $add(self, other) }
            }
            #[inline(always)]
            unsafe fn sub(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $sub(self, other) }
            }
            #[inline(always)]
            unsafe fn mul(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $mul(self, other) }
            }
            #[inline(always)]
            unsafe fn div(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $div(self, other) }
            }
            #[inline(always)]
            unsafe fn neg(self) -> Self {
                // SAFETY: see above.
                unsafe { $xor(self, $splat(-0.0)) }
            }
            #[inline(always)]
            unsafe fn abs(self) -> Self {
                // SAFETY: see above.
                unsafe { $andnot($splat(-0.0), self) }
            }
            #[inline(always)]
            unsafe fn sqrt(self) -> Self {
                // SAFETY: see above.
                unsafe { $sqrt(self) }
            }
            #[inline(always)]
            unsafe fn min(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $min(self, other) }
            }
            #[inline(always)]
            unsafe fn max(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $max(self, other) }
            }

            $($compare)*

            #[inline(always)]
            unsafe fn mask_and(a: Self, b: Self) -> Self {
                // SAFETY: see above.
                unsafe { $and(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_or(a: Self, b: Self) -> Self {
                // SAFETY: see above.
                unsafe { $or(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_xor(a: Self, b: Self) -> Self {
                // SAFETY: see above.
                unsafe { $xor(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_not(a: Self) -> Self {
                // Every bit set: zero is equal to zero.
                // SAFETY: see above.
                unsafe {
                    let zero = $splat(0.0);
                    $xor(a, <$reg as Raw>::eq(zero, zero))
                }
            }
            #[inline(always)]
            unsafe fn mask_bits(a: Self) -> u32 {
                // SAFETY: see above.
                unsafe { $movemask(a) as u32 }
            }
            #[inline(always)]
            unsafe fn select(mask: Self, if_set: Self, if_clear: Self) -> Self {
                // SAFETY: see above.
                unsafe { $select(mask, if_set, if_clear) }
            }
        }
    };
}

/// The comparisons of the SSE registers, one intrinsic each.
macro_rules! sse_compare {
    ($reg:ty: $($name:ident = $intrinsic:ident),*) => {
        $(
            #[inline(always)]
            unsafe fn $name(self, other: Self) -> $reg {
                // SAFETY: the caller knows the processor has the
                // instruction set.
                unsafe { $intrinsic(self, other) }
            }
        )*
    };
}

/// The comparisons of the AVX registers: one intrinsic and a predicate,
/// quiet on NaN, ordered except for "not equal".
macro_rules! avx_compare {
    ($reg:ty: $intrinsic:ident, $($name:ident = $predicate:ident),*) => {
        $(
            #[inline(always)]
            unsafe fn $name(self, other: Self) -> $reg {
                // SAFETY: the caller knows the processor has AVX.
                unsafe { $intrinsic::<$predicate>(self, other) }
            }
        )*
    };
}

/// `a` where the mask is set, `b` elsewhere.
#[inline(always)]
unsafe fn select_ps(mask: __m128, a: __m128, b: __m128) -> __m128 {
    // SAFETY: the caller knows the processor has SSE.
    unsafe { _mm_or_ps(_mm_and_ps(mask, a), _mm_andnot_ps(mask, b)) }
}

/// As [`select_ps`].
#[inline(always)]
unsafe fn select_pd(mask: __m128d, a: __m128d, b: __m128d) -> __m128d {
    // SAFETY: the caller knows the processor has SSE2.
    unsafe { _mm_or_pd(_mm_and_pd(mask, a), _mm_andnot_pd(mask, b)) }
}

/// As [`select_ps`]. The blend takes its second operand where the mask is
/// set.
#[inline(always)]
unsafe fn select_ps256(mask: __m256, a: __m256, b: __m256) -> __m256 {
    // SAFETY: the caller knows the processor has AVX.
    unsafe { _mm256_blendv_ps(b, a, mask) }
}

/// As [`select_ps256`].
#[inline(always)]
unsafe fn select_pd256(mask: __m256d, a: __m256d, b: __m256d) -> __m256d {
    // SAFETY: the caller knows the processor has AVX.
    unsafe { _mm256_blendv_pd(b, a, mask) }
}

x86_raw! {
    __m128, f32, 4,
    splat: _mm_set1_ps,
    load: _mm_loadu_ps, _mm_load_ps,
    store: _mm_storeu_ps, _mm_store_ps,
    arithmetic: _mm_add_ps, _mm_sub_ps, _mm_mul_ps, _mm_div_ps, _mm_sqrt_ps,
    order: _mm_min_ps, _mm_max_ps,
    bits: _mm_and_ps, _mm_or_ps, _mm_xor_ps, _mm_andnot_ps,
    movemask: _mm_movemask_ps,
    compare: {
        sse_compare!(__m128: eq = _mm_cmpeq_ps, ne = _mm_cmpneq_ps, lt = _mm_cmplt_ps,
            le = _mm_cmple_ps, gt = _mm_cmpgt_ps, ge = _mm_cmpge_ps);
    },
    select: select_ps,
}

x86_raw! {
    __m128d, f64, 2,
    splat: _mm_set1_pd,
    load: _mm_loadu_pd, _mm_load_pd,
    store: _mm_storeu_pd, _mm_store_pd,
    arithmetic: _mm_add_pd, _mm_sub_pd, _mm_mul_pd, _mm_div_pd, _mm_sqrt_pd,
    order: _mm_min_pd, _mm_max_pd,
    bits: _mm_and_pd, _mm_or_pd, _mm_xor_pd, _mm_andnot_pd,
    movemask: _mm_movemask_pd,
    compare: {
        sse_compare!(__m128d: eq = _mm_cmpeq_pd, ne = _mm_cmpneq_pd, lt = _mm_cmplt_pd,
            le = _mm_cmple_pd, gt = _mm_cmpgt_pd, ge = _mm_cmpge_pd);
    },
    select: select_pd,
}

x86_raw! {
    __m256, f32, 8,
    splat: _mm256_set1_ps,
    load: _mm256_loadu_ps, _mm256_load_ps,
    store: _mm256_storeu_ps, _mm256_store_ps,
    arithmetic: _mm256_add_ps, _mm256_sub_ps, _mm256_mul_ps, _mm256_div_ps, _mm256_sqrt_ps,
    order: _mm256_min_ps, _mm256_max_ps,
    bits: _mm256_and_ps, _mm256_or_ps, _mm256_xor_ps, _mm256_andnot_ps,
    movemask: _mm256_movemask_ps,
    compare: {
        avx_compare!(__m256: _mm256_cmp_ps, eq = _CMP_EQ_OQ, ne = _CMP_NEQ_UQ,
            lt = _CMP_LT_OQ, le = _CMP_LE_OQ, gt = _CMP_GT_OQ, ge = _CMP_GE_OQ);
    },
    select: select_ps256,
}

x86_raw! {
    __m256d, f64, 4,
    splat: _mm256_set1_pd,
    load: _mm256_loadu_pd, _mm256_load_pd,
    store: _mm256_storeu_pd, _mm256_store_pd,
    arithmetic: _mm256_add_pd, _mm256_sub_pd, _mm256_mul_pd, _mm256_div_pd, _mm256_sqrt_pd,
    order: _mm256_min_pd, _mm256_max_pd,
    bits: _mm256_and_pd, _mm256_or_pd, _mm256_xor_pd, _mm256_andnot_pd,
    movemask: _mm256_movemask_pd,
    compare: {
        avx_compare!(__m256d: _mm256_cmp_pd, eq = _CMP_EQ_OQ, ne = _CMP_NEQ_UQ,
            lt = _CMP_LT_OQ, le = _CMP_LE_OQ, gt = _CMP_GT_OQ, ge = _CMP_GE_OQ);
    },
    select: select_pd256,
}
