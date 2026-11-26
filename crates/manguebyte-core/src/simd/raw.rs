//! What each backend implements: a group of lanes as wide as one public
//! register, made of one hardware register, two of them, or a plain array.

use core::array;

use super::SimdSample;

/// The lanes behind one public register, and the instructions on them.
///
/// Every method is `unsafe` because it may run instructions the processor
/// lacks: the caller must know that it has those of the implementing type's
/// backend. The methods are inlined into the caller, and so into a function
/// compiled for that instruction set, so that each is one instruction or
/// two.
///
/// A mask holds a lane per lane of the register, set or clear.
pub trait Raw: Copy + Send + Sync + 'static {
    /// The element type.
    type T: SimdSample;
    /// The result of a comparison.
    type M: Copy + Send + Sync + 'static;
    /// Elements in the register.
    const LANES: usize;

    /// Every lane set to `x`.
    unsafe fn splat(x: Self::T) -> Self;
    /// Loads `LANES` elements from `p`, which need not be aligned.
    unsafe fn load(p: *const Self::T) -> Self;
    /// Loads `LANES` elements from `p`, aligned to 32 bytes.
    unsafe fn load_aligned(p: *const Self::T) -> Self;
    /// Stores `LANES` elements at `p`, which need not be aligned.
    unsafe fn store(self, p: *mut Self::T);
    /// Stores `LANES` elements at `p`, aligned to 32 bytes.
    unsafe fn store_aligned(self, p: *mut Self::T);

    unsafe fn add(self, other: Self) -> Self;
    unsafe fn sub(self, other: Self) -> Self;
    unsafe fn mul(self, other: Self) -> Self;
    unsafe fn div(self, other: Self) -> Self;
    /// The sign bit flipped.
    unsafe fn neg(self) -> Self;
    /// The sign bit cleared.
    unsafe fn abs(self) -> Self;
    unsafe fn sqrt(self) -> Self;
    /// `if self < other { self } else { other }` in each lane.
    unsafe fn min(self, other: Self) -> Self;
    /// `if self > other { self } else { other }` in each lane.
    unsafe fn max(self, other: Self) -> Self;

    unsafe fn eq(self, other: Self) -> Self::M;
    /// True where a lane is NaN.
    unsafe fn ne(self, other: Self) -> Self::M;
    unsafe fn lt(self, other: Self) -> Self::M;
    unsafe fn le(self, other: Self) -> Self::M;
    unsafe fn gt(self, other: Self) -> Self::M;
    unsafe fn ge(self, other: Self) -> Self::M;

    unsafe fn mask_and(a: Self::M, b: Self::M) -> Self::M;
    unsafe fn mask_or(a: Self::M, b: Self::M) -> Self::M;
    unsafe fn mask_xor(a: Self::M, b: Self::M) -> Self::M;
    unsafe fn mask_not(a: Self::M) -> Self::M;
    /// Bit `i` set where lane `i` of the mask is.
    unsafe fn mask_bits(a: Self::M) -> u32;
    /// `if_set` in the lanes where the mask is set, `if_clear` elsewhere,
    /// every bit as it is.
    unsafe fn select(mask: Self::M, if_set: Self, if_clear: Self) -> Self;
}

/// Plain Rust on an array: the definition every other backend must match.
impl<T: SimdSample, const N: usize> Raw for [T; N] {
    type T = T;
    type M = [bool; N];
    const LANES: usize = N;

    #[inline(always)]
    unsafe fn splat(x: T) -> Self {
        [x; N]
    }
    #[inline(always)]
    unsafe fn load(p: *const T) -> Self {
        // SAFETY: the caller passes a pointer to `N` elements.
        unsafe { p.cast::<[T; N]>().read_unaligned() }
    }
    #[inline(always)]
    unsafe fn load_aligned(p: *const T) -> Self {
        // SAFETY: as for `load`.
        unsafe { Self::load(p) }
    }
    #[inline(always)]
    unsafe fn store(self, p: *mut T) {
        // SAFETY: the caller passes a pointer to room for `N` elements.
        unsafe { p.cast::<[T; N]>().write_unaligned(self) }
    }
    #[inline(always)]
    unsafe fn store_aligned(self, p: *mut T) {
        // SAFETY: as for `store`.
        unsafe { self.store(p) }
    }
    #[inline(always)]
    unsafe fn add(self, other: Self) -> Self {
        array::from_fn(|i| self[i] + other[i])
    }
    #[inline(always)]
    unsafe fn sub(self, other: Self) -> Self {
        array::from_fn(|i| self[i] - other[i])
    }
    #[inline(always)]
    unsafe fn mul(self, other: Self) -> Self {
        array::from_fn(|i| self[i] * other[i])
    }
    #[inline(always)]
    unsafe fn div(self, other: Self) -> Self {
        array::from_fn(|i| self[i] / other[i])
    }
    #[inline(always)]
    unsafe fn neg(self) -> Self {
        array::from_fn(|i| -self[i])
    }
    #[inline(always)]
    unsafe fn abs(self) -> Self {
        array::from_fn(|i| self[i].abs())
    }
    #[inline(always)]
    unsafe fn sqrt(self) -> Self {
        array::from_fn(|i| self[i].sqrt())
    }
    #[inline(always)]
    unsafe fn min(self, other: Self) -> Self {
        array::from_fn(|i| super::lesser(self[i], other[i]))
    }
    #[inline(always)]
    unsafe fn max(self, other: Self) -> Self {
        array::from_fn(|i| super::greater(self[i], other[i]))
    }
    #[inline(always)]
    unsafe fn eq(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] == other[i])
    }
    #[inline(always)]
    unsafe fn ne(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] != other[i])
    }
    #[inline(always)]
    unsafe fn lt(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] < other[i])
    }
    #[inline(always)]
    unsafe fn le(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] <= other[i])
    }
    #[inline(always)]
    unsafe fn gt(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] > other[i])
    }
    #[inline(always)]
    unsafe fn ge(self, other: Self) -> [bool; N] {
        array::from_fn(|i| self[i] >= other[i])
    }
    #[inline(always)]
    unsafe fn mask_and(a: [bool; N], b: [bool; N]) -> [bool; N] {
        array::from_fn(|i| a[i] & b[i])
    }
    #[inline(always)]
    unsafe fn mask_or(a: [bool; N], b: [bool; N]) -> [bool; N] {
        array::from_fn(|i| a[i] | b[i])
    }
    #[inline(always)]
    unsafe fn mask_xor(a: [bool; N], b: [bool; N]) -> [bool; N] {
        array::from_fn(|i| a[i] ^ b[i])
    }
    #[inline(always)]
    unsafe fn mask_not(a: [bool; N]) -> [bool; N] {
        array::from_fn(|i| !a[i])
    }
    #[inline(always)]
    unsafe fn mask_bits(a: [bool; N]) -> u32 {
        let mut bits = 0;
        for (i, set) in a.into_iter().enumerate() {
            bits |= u32::from(set) << i;
        }
        bits
    }
    #[inline(always)]
    unsafe fn select(mask: [bool; N], if_set: Self, if_clear: Self) -> Self {
        array::from_fn(|i| if mask[i] { if_set[i] } else { if_clear[i] })
    }
}

/// Two 128-bit registers side by side, the lanes of the first one first:
/// one public register on the backends with 128-bit vectors.
#[cfg(all(
    not(miri),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "wasm32", target_feature = "simd128"),
    )
))]
#[derive(Clone, Copy)]
pub struct Pair<R>(R, R);

/// Runs a method of [`Raw`] on each half of a [`Pair`].
#[cfg(all(
    not(miri),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "wasm32", target_feature = "simd128"),
    )
))]
macro_rules! halves {
    (unary: $($name:ident)*) => {
        $(
            #[inline(always)]
            unsafe fn $name(self) -> Self {
                // SAFETY: the caller knows the processor has the
                // instructions of `R`.
                unsafe { Pair(R::$name(self.0), R::$name(self.1)) }
            }
        )*
    };
    (binary: $($name:ident)*) => {
        $(
            #[inline(always)]
            unsafe fn $name(self, other: Self) -> Self {
                // SAFETY: as for the unary methods.
                unsafe { Pair(R::$name(self.0, other.0), R::$name(self.1, other.1)) }
            }
        )*
    };
}

/// The comparisons of a [`Pair`], which give a mask of two halves.
#[cfg(all(
    not(miri),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "wasm32", target_feature = "simd128"),
    )
))]
macro_rules! compare_halves {
    ($($name:ident)*) => {
        $(
            #[inline(always)]
            unsafe fn $name(self, other: Self) -> (R::M, R::M) {
                // SAFETY: as in `halves`.
                unsafe { (R::$name(self.0, other.0), R::$name(self.1, other.1)) }
            }
        )*
    };
}

#[cfg(all(
    not(miri),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "wasm32", target_feature = "simd128"),
    )
))]
impl<R: Raw> Raw for Pair<R> {
    type T = R::T;
    type M = (R::M, R::M);
    const LANES: usize = 2 * R::LANES;

    #[inline(always)]
    unsafe fn splat(x: R::T) -> Self {
        // SAFETY: as in `halves`.
        unsafe { Pair(R::splat(x), R::splat(x)) }
    }
    #[inline(always)]
    unsafe fn load(p: *const R::T) -> Self {
        // SAFETY: the caller passes a pointer to `2 * R::LANES` elements,
        // so both halves are inside them.
        unsafe { Pair(R::load(p), R::load(p.add(R::LANES))) }
    }
    #[inline(always)]
    unsafe fn load_aligned(p: *const R::T) -> Self {
        // SAFETY: as in `load`; a half is 16 bytes, so the second one is
        // aligned to 16, which is all a 128-bit register asks for.
        unsafe { Pair(R::load_aligned(p), R::load_aligned(p.add(R::LANES))) }
    }
    #[inline(always)]
    unsafe fn store(self, p: *mut R::T) {
        // SAFETY: as in `load`.
        unsafe {
            self.0.store(p);
            self.1.store(p.add(R::LANES));
        }
    }
    #[inline(always)]
    unsafe fn store_aligned(self, p: *mut R::T) {
        // SAFETY: as in `load_aligned`.
        unsafe {
            self.0.store_aligned(p);
            self.1.store_aligned(p.add(R::LANES));
        }
    }

    halves!(unary: neg abs sqrt);
    halves!(binary: add sub mul div min max);

    compare_halves!(eq ne lt le gt ge);

    #[inline(always)]
    unsafe fn mask_and(a: Self::M, b: Self::M) -> Self::M {
        // SAFETY: as in `halves`.
        unsafe { (R::mask_and(a.0, b.0), R::mask_and(a.1, b.1)) }
    }
    #[inline(always)]
    unsafe fn mask_or(a: Self::M, b: Self::M) -> Self::M {
        // SAFETY: as in `halves`.
        unsafe { (R::mask_or(a.0, b.0), R::mask_or(a.1, b.1)) }
    }
    #[inline(always)]
    unsafe fn mask_xor(a: Self::M, b: Self::M) -> Self::M {
        // SAFETY: as in `halves`.
        unsafe { (R::mask_xor(a.0, b.0), R::mask_xor(a.1, b.1)) }
    }
    #[inline(always)]
    unsafe fn mask_not(a: Self::M) -> Self::M {
        // SAFETY: as in `halves`.
        unsafe { (R::mask_not(a.0), R::mask_not(a.1)) }
    }
    #[inline(always)]
    unsafe fn mask_bits(a: Self::M) -> u32 {
        // SAFETY: as in `halves`.
        unsafe { R::mask_bits(a.0) | R::mask_bits(a.1) << R::LANES }
    }
    #[inline(always)]
    unsafe fn select(mask: Self::M, if_set: Self, if_clear: Self) -> Self {
        // SAFETY: as in `halves`.
        unsafe {
            Pair(
                R::select(mask.0, if_set.0, if_clear.0),
                R::select(mask.1, if_set.1, if_clear.1),
            )
        }
    }
}
