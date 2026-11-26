//! NEON (Advanced SIMD), on AArch64, where every processor has it.
//!
//! The instructions are those of Arm's *Intrinsics* reference
//! (<https://developer.arm.com/architectures/instruction-sets/intrinsics/>),
//! as `core::arch::aarch64` exposes them. `min` and `max` are a comparison
//! and a bit select, not `FMIN`/`FMAX`, which treat NaN and signed zeros
//! otherwise than the definition does.
//!
//! As on x86, the `unsafe` blocks keep the crate building with Rust 1.85,
//! before most intrinsics became safe to call.
#![allow(unused_unsafe)]

use core::arch::aarch64::*;

use super::raw::{Pair, Raw};
use super::register::Reg;
use super::{Isa, Kernel, sealed};
use crate::vector::Backend;

/// Proof that the processor has NEON, which every processor this module is
/// built for does. Only [`neon`] makes one.
#[derive(Debug, Clone, Copy)]
pub struct NeonIsa(());

impl sealed::Sealed for NeonIsa {}

impl Isa for NeonIsa {
    type F32 = Reg<Self, Pair<float32x4_t>>;
    type F64 = Reg<Self, Pair<float64x2_t>>;

    #[inline(always)]
    fn backend(self) -> Backend {
        Backend::Neon
    }
}

/// Runs a kernel with NEON.
pub(super) fn neon<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(NeonIsa(()))
}

/// Implements [`Raw`] for one register type out of the intrinsics named.
macro_rules! neon_raw {
    (
        $reg:ty, $t:ty, $mask:ty, $lanes:literal,
        splat: $splat:ident,
        memory: $load:ident, $store:ident,
        arithmetic: $add:ident, $sub:ident, $mul:ident, $div:ident,
        unary: $neg:ident, $abs:ident, $sqrt:ident,
        compare: $eq:ident, $lt:ident, $le:ident, $gt:ident, $ge:ident,
        masks: $and:ident, $or:ident, $xor:ident,
        mask_splat: $mask_splat:ident,
        select: $select:ident,
        mask_bits: |$m:ident| $bits:expr $(,)?
    ) => {
        // SAFETY (for every method): NEON is part of every processor this
        // module is built for; loads and stores need only valid pointers to
        // `LANES` elements, which the caller guarantees.
        impl Raw for $reg {
            type T = $t;
            type M = $mask;
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
                // NEON has one load for any alignment.
                // SAFETY: see above.
                unsafe { $load(p) }
            }
            #[inline(always)]
            unsafe fn store(self, p: *mut $t) {
                // SAFETY: see above.
                unsafe { $store(p, self) }
            }
            #[inline(always)]
            unsafe fn store_aligned(self, p: *mut $t) {
                // SAFETY: see above.
                unsafe { $store(p, self) }
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
                unsafe { $neg(self) }
            }
            #[inline(always)]
            unsafe fn abs(self) -> Self {
                // SAFETY: see above.
                unsafe { $abs(self) }
            }
            #[inline(always)]
            unsafe fn sqrt(self) -> Self {
                // SAFETY: see above.
                unsafe { $sqrt(self) }
            }
            #[inline(always)]
            unsafe fn min(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $select($lt(self, other), self, other) }
            }
            #[inline(always)]
            unsafe fn max(self, other: Self) -> Self {
                // SAFETY: see above.
                unsafe { $select($gt(self, other), self, other) }
            }
            #[inline(always)]
            unsafe fn eq(self, other: Self) -> $mask {
                // SAFETY: see above.
                unsafe { $eq(self, other) }
            }
            #[inline(always)]
            unsafe fn ne(self, other: Self) -> $mask {
                // The opposite of "equal", so true for NaN.
                // SAFETY: see above.
                unsafe { Self::mask_not($eq(self, other)) }
            }
            #[inline(always)]
            unsafe fn lt(self, other: Self) -> $mask {
                // SAFETY: see above.
                unsafe { $lt(self, other) }
            }
            #[inline(always)]
            unsafe fn le(self, other: Self) -> $mask {
                // SAFETY: see above.
                unsafe { $le(self, other) }
            }
            #[inline(always)]
            unsafe fn gt(self, other: Self) -> $mask {
                // SAFETY: see above.
                unsafe { $gt(self, other) }
            }
            #[inline(always)]
            unsafe fn ge(self, other: Self) -> $mask {
                // SAFETY: see above.
                unsafe { $ge(self, other) }
            }
            #[inline(always)]
            unsafe fn mask_and(a: $mask, b: $mask) -> $mask {
                // SAFETY: see above.
                unsafe { $and(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_or(a: $mask, b: $mask) -> $mask {
                // SAFETY: see above.
                unsafe { $or(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_xor(a: $mask, b: $mask) -> $mask {
                // SAFETY: see above.
                unsafe { $xor(a, b) }
            }
            #[inline(always)]
            unsafe fn mask_not(a: $mask) -> $mask {
                // SAFETY: see above.
                unsafe { $xor(a, $mask_splat(!0)) }
            }
            #[inline(always)]
            unsafe fn mask_bits($m: $mask) -> u32 {
                // SAFETY: see above.
                unsafe { $bits }
            }
            #[inline(always)]
            unsafe fn select(mask: $mask, if_set: Self, if_clear: Self) -> Self {
                // SAFETY: see above.
                unsafe { $select(mask, if_set, if_clear) }
            }
        }
    };
}

neon_raw! {
    float32x4_t, f32, uint32x4_t, 4,
    splat: vdupq_n_f32,
    memory: vld1q_f32, vst1q_f32,
    arithmetic: vaddq_f32, vsubq_f32, vmulq_f32, vdivq_f32,
    unary: vnegq_f32, vabsq_f32, vsqrtq_f32,
    compare: vceqq_f32, vcltq_f32, vcleq_f32, vcgtq_f32, vcgeq_f32,
    masks: vandq_u32, vorrq_u32, veorq_u32,
    mask_splat: vdupq_n_u32,
    select: vbslq_f32,
    mask_bits: |m| (vgetq_lane_u32::<0>(m) & 1)
        | (vgetq_lane_u32::<1>(m) & 2)
        | (vgetq_lane_u32::<2>(m) & 4)
        | (vgetq_lane_u32::<3>(m) & 8),
}

neon_raw! {
    float64x2_t, f64, uint64x2_t, 2,
    splat: vdupq_n_f64,
    memory: vld1q_f64, vst1q_f64,
    arithmetic: vaddq_f64, vsubq_f64, vmulq_f64, vdivq_f64,
    unary: vnegq_f64, vabsq_f64, vsqrtq_f64,
    compare: vceqq_f64, vcltq_f64, vcleq_f64, vcgtq_f64, vcgeq_f64,
    masks: vandq_u64, vorrq_u64, veorq_u64,
    mask_splat: vdupq_n_u64,
    select: vbslq_f64,
    mask_bits: |m| ((vgetq_lane_u64::<0>(m) & 1) | (vgetq_lane_u64::<1>(m) & 2)) as u32,
}
