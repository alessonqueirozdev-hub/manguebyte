//! WebAssembly's 128-bit SIMD, in a module built with `-C
//! target-feature=+simd128`.
//!
//! The instructions are those of the WebAssembly SIMD specification
//! (<https://github.com/WebAssembly/simd/blob/main/proposals/simd/SIMD.md>,
//! part of WebAssembly 2.0), as `core::arch::wasm32` exposes them. They
//! are safe to call in such a module; only loads and stores, which take
//! pointers, are `unsafe`. `min` and `max` are the "pseudo-minimum" and
//! "pseudo-maximum" with their operands exchanged, which makes
//! `pmin(b, a)` the definition's `if a < b { a } else { b }`.

use core::arch::wasm32::*;

use super::raw::{Pair, Raw};
use super::register::Reg;
use super::{Isa, Kernel, sealed};
use crate::vector::Backend;

/// Proof that the module was built with SIMD. Only [`simd128`] makes one.
#[derive(Debug, Clone, Copy)]
pub struct Simd128Isa(());

impl sealed::Sealed for Simd128Isa {}

impl Isa for Simd128Isa {
    type F32 = Reg<Self, Pair<Wasm32>>;
    type F64 = Reg<Self, Pair<Wasm64>>;

    #[inline(always)]
    fn backend(self) -> Backend {
        Backend::Simd128
    }
}

/// Runs a kernel with `simd128`.
pub(super) fn simd128<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(Simd128Isa(()))
}

/// `f32` in a SIMD register.
#[derive(Clone, Copy)]
pub struct Wasm32(v128);

/// `f64` in a SIMD register.
#[derive(Clone, Copy)]
pub struct Wasm64(v128);

/// Implements [`Raw`] for one register type out of the intrinsics named.
macro_rules! wasm_raw {
    (
        $reg:ident, $t:ty, $lanes:literal,
        splat: $splat:ident,
        arithmetic: $add:ident, $sub:ident, $mul:ident, $div:ident,
        unary: $neg:ident, $abs:ident, $sqrt:ident,
        order: $pmin:ident, $pmax:ident,
        compare: $eq:ident, $ne:ident, $lt:ident, $le:ident, $gt:ident, $ge:ident,
        mask_bits: $bitmask:ident $(,)?
    ) => {
        impl Raw for $reg {
            type T = $t;
            type M = v128;
            const LANES: usize = $lanes;

            #[inline(always)]
            unsafe fn splat(x: $t) -> Self {
                $reg($splat(x))
            }
            #[inline(always)]
            unsafe fn load(p: *const $t) -> Self {
                // SAFETY: the caller passes a valid pointer to `LANES`
                // elements; WebAssembly loads need no alignment.
                $reg(unsafe { v128_load(p.cast()) })
            }
            #[inline(always)]
            unsafe fn load_aligned(p: *const $t) -> Self {
                // SAFETY: as for `load`.
                unsafe { Self::load(p) }
            }
            #[inline(always)]
            unsafe fn store(self, p: *mut $t) {
                // SAFETY: as for `load`.
                unsafe { v128_store(p.cast(), self.0) }
            }
            #[inline(always)]
            unsafe fn store_aligned(self, p: *mut $t) {
                // SAFETY: as for `load`.
                unsafe { self.store(p) }
            }
            #[inline(always)]
            unsafe fn add(self, other: Self) -> Self {
                $reg($add(self.0, other.0))
            }
            #[inline(always)]
            unsafe fn sub(self, other: Self) -> Self {
                $reg($sub(self.0, other.0))
            }
            #[inline(always)]
            unsafe fn mul(self, other: Self) -> Self {
                $reg($mul(self.0, other.0))
            }
            #[inline(always)]
            unsafe fn div(self, other: Self) -> Self {
                $reg($div(self.0, other.0))
            }
            #[inline(always)]
            unsafe fn neg(self) -> Self {
                $reg($neg(self.0))
            }
            #[inline(always)]
            unsafe fn abs(self) -> Self {
                $reg($abs(self.0))
            }
            #[inline(always)]
            unsafe fn sqrt(self) -> Self {
                $reg($sqrt(self.0))
            }
            #[inline(always)]
            unsafe fn min(self, other: Self) -> Self {
                $reg($pmin(other.0, self.0))
            }
            #[inline(always)]
            unsafe fn max(self, other: Self) -> Self {
                $reg($pmax(other.0, self.0))
            }
            #[inline(always)]
            unsafe fn eq(self, other: Self) -> v128 {
                $eq(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn ne(self, other: Self) -> v128 {
                $ne(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn lt(self, other: Self) -> v128 {
                $lt(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn le(self, other: Self) -> v128 {
                $le(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn gt(self, other: Self) -> v128 {
                $gt(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn ge(self, other: Self) -> v128 {
                $ge(self.0, other.0)
            }
            #[inline(always)]
            unsafe fn mask_and(a: v128, b: v128) -> v128 {
                v128_and(a, b)
            }
            #[inline(always)]
            unsafe fn mask_or(a: v128, b: v128) -> v128 {
                v128_or(a, b)
            }
            #[inline(always)]
            unsafe fn mask_xor(a: v128, b: v128) -> v128 {
                v128_xor(a, b)
            }
            #[inline(always)]
            unsafe fn mask_not(a: v128) -> v128 {
                v128_not(a)
            }
            #[inline(always)]
            unsafe fn mask_bits(a: v128) -> u32 {
                u32::from($bitmask(a))
            }
            #[inline(always)]
            unsafe fn select(mask: v128, if_set: Self, if_clear: Self) -> Self {
                $reg(v128_bitselect(if_set.0, if_clear.0, mask))
            }
        }
    };
}

wasm_raw! {
    Wasm32, f32, 4,
    splat: f32x4_splat,
    arithmetic: f32x4_add, f32x4_sub, f32x4_mul, f32x4_div,
    unary: f32x4_neg, f32x4_abs, f32x4_sqrt,
    order: f32x4_pmin, f32x4_pmax,
    compare: f32x4_eq, f32x4_ne, f32x4_lt, f32x4_le, f32x4_gt, f32x4_ge,
    mask_bits: i32x4_bitmask,
}

wasm_raw! {
    Wasm64, f64, 2,
    splat: f64x2_splat,
    arithmetic: f64x2_add, f64x2_sub, f64x2_mul, f64x2_div,
    unary: f64x2_neg, f64x2_abs, f64x2_sqrt,
    order: f64x2_pmin, f64x2_pmax,
    compare: f64x2_eq, f64x2_ne, f64x2_lt, f64x2_le, f64x2_gt, f64x2_ge,
    mask_bits: i64x2_bitmask,
}
