//! The types behind [`Isa`](super::Isa), [`Register`](super::Register)
//! and [`Mask`](super::Mask). Public in a private module: code outside the
//! crate reaches them only through those traits.

use core::fmt;
use core::marker::PhantomData;
use core::ops::{
    Add, AddAssign, BitAnd, BitOr, BitXor, Div, DivAssign, Mul, MulAssign, Neg, Not, Sub, SubAssign,
};

use super::raw::Raw;
use super::{Aligned, Backend, Isa, Kernel, Mask, Register, SimdSample, sealed};

/// The instruction set of the scalar backend: plain Rust on arrays.
#[derive(Debug, Clone, Copy)]
pub struct ScalarIsa(());

/// Runs a kernel in plain Rust.
pub(super) fn scalar<K: Kernel>(kernel: K) -> K::Output {
    kernel.run(ScalarIsa(()))
}

impl sealed::Sealed for ScalarIsa {}

impl Isa for ScalarIsa {
    type F32 = Reg<Self, [f32; 8]>;
    type F64 = Reg<Self, [f64; 4]>;

    #[inline(always)]
    fn backend(self) -> Backend {
        Backend::Scalar
    }
}

/// The one implementation of [`Register`]: the lanes `R` of the backend
/// `I`.
///
/// A value exists only if the processor has the instructions of `I`: every
/// constructor takes an `I`, which only [`Backend::dispatch`] makes after
/// asking the processor, and everything else makes a register from
/// registers. That is what makes the calls into [`Raw`] sound.
#[derive(Clone, Copy)]
pub struct Reg<I, R> {
    raw: R,
    isa: PhantomData<I>,
}

/// The one implementation of [`Mask`], with the invariant of [`Reg`].
#[derive(Clone, Copy)]
pub struct RegMask<I, R: Raw> {
    raw: R::M,
    isa: PhantomData<I>,
}

impl<I, R: Raw> Reg<I, R> {
    #[inline(always)]
    fn new(raw: R) -> Self {
        Reg {
            raw,
            isa: PhantomData,
        }
    }
}

impl<I, R: Raw> RegMask<I, R> {
    #[inline(always)]
    fn new(raw: R::M) -> Self {
        RegMask {
            raw,
            isa: PhantomData,
        }
    }
}

impl<I: Isa, R: Raw> sealed::Sealed for Reg<I, R> {}
impl<I: Isa, R: Raw> sealed::Sealed for RegMask<I, R> {}

/// An operator of a register, and its assigning form.
macro_rules! operator {
    ($trait:ident, $method:ident, $assign:ident, $assign_method:ident) => {
        impl<I: Isa, R: Raw> $trait for Reg<I, R> {
            type Output = Self;

            #[inline(always)]
            fn $method(self, other: Self) -> Self {
                // SAFETY: `self` exists, so the processor has the
                // instructions of `I` (see `Reg`).
                Reg::new(unsafe { self.raw.$method(other.raw) })
            }
        }

        impl<I: Isa, R: Raw> $assign for Reg<I, R> {
            #[inline(always)]
            fn $assign_method(&mut self, other: Self) {
                *self = $trait::$method(*self, other);
            }
        }
    };
}

operator!(Add, add, AddAssign, add_assign);
operator!(Sub, sub, SubAssign, sub_assign);
operator!(Mul, mul, MulAssign, mul_assign);
operator!(Div, div, DivAssign, div_assign);

impl<I: Isa, R: Raw> Neg for Reg<I, R> {
    type Output = Self;

    #[inline(always)]
    fn neg(self) -> Self {
        // SAFETY: as for the other operators.
        Reg::new(unsafe { self.raw.neg() })
    }
}

impl<I: Isa, R: Raw> Reg<I, R> {
    /// Checked when the code is compiled: the loads and stores below move
    /// `R::LANES` elements to and from room for a register's lanes.
    const WIDTH: () = assert!(R::LANES == <R::T as SimdSample>::LANES);
}

impl<I: Isa, R: Raw> Register for Reg<I, R> {
    type Isa = I;
    type Elem = R::T;
    type Mask = RegMask<I, R>;

    #[inline(always)]
    fn splat(_: I, value: R::T) -> Self {
        // SAFETY: the argument of type `I` proves the processor has its
        // instructions.
        Reg::new(unsafe { R::splat(value) })
    }

    #[inline(always)]
    fn from_array(_: I, lanes: <R::T as SimdSample>::Lanes) -> Self {
        let () = Self::WIDTH;
        // SAFETY: the processor has the instructions, as in `splat`;
        // `lanes` holds `R::LANES` elements (`WIDTH`).
        Reg::new(unsafe { R::load(lanes.as_ref().as_ptr()) })
    }

    #[inline(always)]
    fn to_array(self) -> <R::T as SimdSample>::Lanes {
        let () = Self::WIDTH;
        let mut lanes = <R::T as SimdSample>::Lanes::default();
        // SAFETY: `self` exists, so the processor has the instructions;
        // `lanes` has room for `R::LANES` elements (`WIDTH`).
        unsafe { self.raw.store(lanes.as_mut().as_mut_ptr()) };
        lanes
    }

    #[inline(always)]
    #[track_caller]
    fn load(_: I, src: &[R::T]) -> Self {
        let src = &src[..R::LANES];
        // SAFETY: the processor has the instructions, as in `splat`; `src`
        // is `R::LANES` elements long.
        Reg::new(unsafe { R::load(src.as_ptr()) })
    }

    #[inline(always)]
    #[track_caller]
    fn store(self, dst: &mut [R::T]) {
        let dst = &mut dst[..R::LANES];
        // SAFETY: `self` exists, so the processor has the instructions;
        // `dst` is `R::LANES` elements long.
        unsafe { self.raw.store(dst.as_mut_ptr()) }
    }

    #[inline(always)]
    fn load_aligned(_: I, src: &Aligned<<R::T as SimdSample>::Lanes>) -> Self {
        let () = Self::WIDTH;
        // SAFETY: as in `from_array`; `Aligned` is aligned to 32 bytes and
        // its contents start where it does (`repr(C)`).
        Reg::new(unsafe { R::load_aligned(src.0.as_ref().as_ptr()) })
    }

    #[inline(always)]
    fn store_aligned(self, dst: &mut Aligned<<R::T as SimdSample>::Lanes>) {
        let () = Self::WIDTH;
        // SAFETY: as in `to_array` and `load_aligned`.
        unsafe { self.raw.store_aligned(dst.0.as_mut().as_mut_ptr()) }
    }

    #[inline(always)]
    fn abs(self) -> Self {
        // SAFETY: as for the operators.
        Reg::new(unsafe { self.raw.abs() })
    }

    #[inline(always)]
    fn sqrt(self) -> Self {
        // SAFETY: as for the operators.
        Reg::new(unsafe { self.raw.sqrt() })
    }

    #[inline(always)]
    fn min(self, other: Self) -> Self {
        // SAFETY: as for the operators.
        Reg::new(unsafe { self.raw.min(other.raw) })
    }

    #[inline(always)]
    fn max(self, other: Self) -> Self {
        // SAFETY: as for the operators.
        Reg::new(unsafe { self.raw.max(other.raw) })
    }

    #[inline(always)]
    fn simd_eq(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.eq(other.raw) })
    }

    #[inline(always)]
    fn simd_ne(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.ne(other.raw) })
    }

    #[inline(always)]
    fn simd_lt(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.lt(other.raw) })
    }

    #[inline(always)]
    fn simd_le(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.le(other.raw) })
    }

    #[inline(always)]
    fn simd_gt(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.gt(other.raw) })
    }

    #[inline(always)]
    fn simd_ge(self, other: Self) -> RegMask<I, R> {
        // SAFETY: as for the operators.
        RegMask::new(unsafe { self.raw.ge(other.raw) })
    }
}

impl<I: Isa, R: Raw> Mask for RegMask<I, R> {
    type Register = Reg<I, R>;

    #[inline(always)]
    fn select(self, if_set: Reg<I, R>, if_clear: Reg<I, R>) -> Reg<I, R> {
        // SAFETY: as for the operators.
        Reg::new(unsafe { R::select(self.raw, if_set.raw, if_clear.raw) })
    }

    #[inline(always)]
    fn bits(self) -> u32 {
        // SAFETY: `self` exists, so the processor has the instructions (see
        // `Reg`).
        unsafe { R::mask_bits(self.raw) }
    }
}

/// An operator on the masks themselves.
macro_rules! mask_operator {
    ($trait:ident, $method:ident, $raw:ident) => {
        impl<I: Isa, R: Raw> $trait for RegMask<I, R> {
            type Output = Self;

            #[inline(always)]
            fn $method(self, other: Self) -> Self {
                // SAFETY: `self` exists, so the processor has the
                // instructions of `I` (see `Reg`).
                RegMask::new(unsafe { R::$raw(self.raw, other.raw) })
            }
        }
    };
}

mask_operator!(BitAnd, bitand, mask_and);
mask_operator!(BitOr, bitor, mask_or);
mask_operator!(BitXor, bitxor, mask_xor);

impl<I: Isa, R: Raw> Not for RegMask<I, R> {
    type Output = Self;

    #[inline(always)]
    fn not(self) -> Self {
        // SAFETY: as for the other operators of masks.
        RegMask::new(unsafe { R::mask_not(self.raw) })
    }
}

impl<I: Isa, R: Raw> fmt::Debug for Reg<I, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Register").field(&self.to_array()).finish()
    }
}

impl<I: Isa, R: Raw> fmt::Debug for RegMask<I, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Mask({:#b})", Mask::bits(*self))
    }
}
