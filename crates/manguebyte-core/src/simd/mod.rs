//! A SIMD register type, for processors to vectorise their own loops
//! (ADR 0029).
//!
//! [`vector`](crate::vector) holds the loops most audio code shares. A loop
//! of your own, such as a waveshaper, a bank of resonators or a mix under
//! gains of your own making, is written once over a [`Register`] and runs
//! on SSE2, AVX, AVX2, NEON, WebAssembly SIMD or plain Rust, chosen at run
//! time, with the same results on each.
//!
//! # Writing a kernel
//!
//! The loop goes in a [`Kernel`]: a struct that holds what the loop reads
//! and writes, whose [`run`](Kernel::run) is generic over the instruction
//! set. [`dispatch`] picks the processor's best [`Backend`] and calls `run`
//! inside a function compiled for it. `run` is given an [`Isa`], the proof
//! that the instructions are there, without which no register can be made.
//!
//! ```
//! use manguebyte_core::simd::{self, Isa, Kernel, Register, SimdSample};
//!
//! /// `x / (1 + |x|)` on a block, after a gain.
//! struct SoftClip<'a, S> {
//!     block: &'a mut [S],
//!     gain: S,
//! }
//!
//! impl<S: SimdSample> Kernel for SoftClip<'_, S> {
//!     type Output = ();
//!
//!     #[inline(always)] // so that it is compiled for the backend
//!     fn run<I: Isa>(self, isa: I) {
//!         let gain = isa.splat(self.gain);
//!         let one = isa.splat(S::ONE);
//!         let mut chunks = self.block.chunks_exact_mut(S::LANES);
//!         for chunk in &mut chunks {
//!             let x = isa.load(chunk) * gain;
//!             (x / (one + x.abs())).store(chunk);
//!         }
//!         // The samples that do not fill a register, in the same way.
//!         let rest = chunks.into_remainder();
//!         let x = isa.load_partial(rest, S::ZERO) * gain;
//!         (x / (one + x.abs())).store_partial(rest);
//!     }
//! }
//!
//! let mut block = [0.5f32; 13];
//! simd::dispatch(SoftClip { block: &mut block, gain: 2.0 });
//! assert_eq!(block, [0.5; 13]);
//! ```
//!
//! **Mark `run` `#[inline(always)]`**, and every function it calls with
//! registers. A backend's instructions are enabled only in the function
//! `dispatch` calls `run` from; code that is not inlined into it is
//! compiled without them. The results are still right, but where the
//! instructions are not ones every processor of the target has, as AVX's are
//! not on x86-64, each operation then becomes a call, and the loop many
//! times slower than plain Rust. Closures inside `run` are subject to the
//! same rule, so write plain loops.
//!
//! # Width
//!
//! A register is 256 bits on every backend: 8 lanes of `f32`, 4 of `f64`
//! ([`SimdSample::LANES`]). AVX holds that in one hardware register; SSE2,
//! NEON and WebAssembly in two; the scalar backend in an array. The number
//! of lanes does not change with the processor, so a loop does the same
//! operations on the same elements everywhere.
//!
//! # Results are the same on every backend
//!
//! - `+`, `-`, `*`, `/` and [`sqrt`](Register::sqrt) are IEEE 754
//!   operations on each lane, rounded to nearest, as the same operations on
//!   `f32` and `f64` are.
//! - [`add_product`](Register::add_product) is a product and then a sum,
//!   two roundings, never a fused multiply-add: SSE2 and WebAssembly have
//!   none.
//! - [`min`](Register::min) and [`max`](Register::max) are
//!   `if a < b { a } else { b }` and `if a > b { a } else { b }`. The second
//!   operand is returned when either is NaN and when both are zeros. This
//!   is not what `f32::min` and `f32::max` do.
//! - Comparisons are false where a lane is NaN, except
//!   [`simd_ne`](Register::simd_ne), which is true there.
//! - Negation, [`abs`](Register::abs), `min`, `max`, selection by a mask,
//!   loads and stores move bits as they are, NaNs included.
//! - The reductions ([`reduce_sum`](Register::reduce_sum),
//!   [`reduce_min`](Register::reduce_min),
//!   [`reduce_max`](Register::reduce_max)) combine lanes in one fixed
//!   order: `a[j]` with `a[j + half]`, `half` halving from half the lanes
//!   to one.
//! - The one thing not specified, as in Rust itself, is the sign and the
//!   payload of a NaN that arithmetic or `sqrt` produces.
//!
//! These hold in the usual floating-point mode, round to nearest with
//! subnormals kept. While subnormals are flushed
//! ([`NoDenormals`](crate::NoDenormals)), registers are flushed as scalar
//! arithmetic on the same processor is, with two things to know:
//!
//! - `min`, `max`, `clamp`, `reduce_min` and `reduce_max` may give a
//!   subnormal operand back as the zero of its sign on one backend and as it
//!   is on another (x86's own instructions flush it). In that mode the two
//!   count as the same number.
//! - x86 and Arm do not flush exactly the same results at the edge of the
//!   normal numbers, so there they may differ from each other, as plain Rust
//!   does.
//!
//! WebAssembly has no such mode.
//!
//! # Real time
//!
//! [`dispatch`] and every method here run on the audio thread: nothing
//! allocates, locks or calls the system. [`load`](Register::load) and
//! [`store`](Register::store) panic on a slice shorter than a register;
//! [`load_partial`](Register::load_partial) and
//! [`store_partial`](Register::store_partial) take any slice.

use core::fmt;
use core::ops::{
    Add, AddAssign, BitAnd, BitOr, BitXor, Div, DivAssign, Index, IndexMut, Mul, MulAssign, Neg,
    Not, Sub, SubAssign,
};

use crate::Sample;
pub use crate::vector::Backend;

mod raw;
mod register;

#[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
mod x86;

#[cfg(all(not(miri), target_arch = "aarch64", target_feature = "neon"))]
mod neon;

#[cfg(all(not(miri), target_arch = "wasm32", target_feature = "simd128"))]
mod wasm;

/// Keeps the traits of this module to the types of this module: the safety
/// of a register rests on only [`dispatch`] making an [`Isa`].
mod sealed {
    pub trait Sealed {}
}

/// The definition of [`Register::min`].
#[inline(always)]
fn lesser<T: PartialOrd>(a: T, b: T) -> T {
    if a < b { a } else { b }
}

/// The definition of [`Register::max`].
#[inline(always)]
fn greater<T: PartialOrd>(a: T, b: T) -> T {
    if a > b { a } else { b }
}

/// Memory aligned to 32 bytes, the size of a register, for
/// [`Register::load_aligned`] and [`Register::store_aligned`].
///
/// Allocate vectors of them off the audio thread:
/// `vec![Aligned([0.0f32; 8]); blocks]`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C, align(32))]
pub struct Aligned<T>(pub T);

/// A sample type with registers: `f32` and `f64`.
pub trait SimdSample: Sample + sealed::Sealed {
    /// Elements in a register, on every backend: 8 for `f32`, 4 for `f64`.
    const LANES: usize;
    /// A register's elements as an array, `[Self; LANES]`.
    type Lanes: Copy
        + fmt::Debug
        + Default
        + PartialEq
        + Send
        + Sync
        + 'static
        + AsRef<[Self]>
        + AsMut<[Self]>
        + Index<usize, Output = Self>
        + IndexMut<usize>;
    /// The register of this type on the instruction set `I`.
    type Register<I: Isa>: Register<Elem = Self, Isa = I>;
}

impl sealed::Sealed for f32 {}
impl sealed::Sealed for f64 {}

impl SimdSample for f32 {
    const LANES: usize = 8;
    type Lanes = [f32; 8];
    type Register<I: Isa> = I::F32;
}

impl SimdSample for f64 {
    const LANES: usize = 4;
    type Lanes = [f64; 4];
    type Register<I: Isa> = I::F64;
}

/// An instruction set the processor has, and its registers.
///
/// A [`Kernel`] is given one, and every way of making a register takes it:
/// holding a value is the proof that the instructions are there. Only
/// [`dispatch`] and [`Backend::dispatch`] make them.
pub trait Isa: Copy + fmt::Debug + Send + Sync + 'static + sealed::Sealed {
    /// The register of 8 `f32`.
    type F32: Register<Elem = f32, Isa = Self>;
    /// The register of 4 `f64`.
    type F64: Register<Elem = f64, Isa = Self>;

    /// The backend these registers run on.
    fn backend(self) -> Backend;

    /// A register with every lane set to `value`: [`Register::splat`].
    #[inline(always)]
    fn splat<S: SimdSample>(self, value: S) -> S::Register<Self> {
        Register::splat(self, value)
    }

    /// A register of the first elements of `src`: [`Register::load`].
    ///
    /// # Panics
    ///
    /// Panics if `src` is shorter than a register.
    #[inline(always)]
    #[track_caller]
    fn load<S: SimdSample>(self, src: &[S]) -> S::Register<Self> {
        Register::load(self, src)
    }

    /// A register of up to a register's worth of `src`, the lanes it does
    /// not reach set to `fill`: [`Register::load_partial`].
    #[inline(always)]
    fn load_partial<S: SimdSample>(self, src: &[S], fill: S) -> S::Register<Self> {
        Register::load_partial(self, src, fill)
    }
}

/// A SIMD register: [`SimdSample::LANES`] samples worked on at once. See
/// the [module documentation](self) for what every operation computes.
pub trait Register:
    Copy
    + fmt::Debug
    + Send
    + Sync
    + 'static
    + sealed::Sealed
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
    + DivAssign
{
    /// The instruction set the register belongs to.
    type Isa: Isa;
    /// The type of a lane.
    type Elem: SimdSample;
    /// What comparing two registers gives.
    type Mask: Mask<Register = Self>;
    /// Elements in the register.
    const LANES: usize = <Self::Elem as SimdSample>::LANES;

    /// Every lane set to `value`.
    fn splat(isa: Self::Isa, value: Self::Elem) -> Self;

    /// Every lane zero.
    #[inline(always)]
    fn zero(isa: Self::Isa) -> Self {
        Self::splat(isa, Self::Elem::ZERO)
    }

    /// The elements of an array, in order.
    fn from_array(isa: Self::Isa, lanes: <Self::Elem as SimdSample>::Lanes) -> Self;

    /// The lanes as an array, in order.
    fn to_array(self) -> <Self::Elem as SimdSample>::Lanes;

    /// The first [`LANES`](Self::LANES) elements of `src`, which need no
    /// particular alignment.
    ///
    /// # Panics
    ///
    /// Panics if `src` is shorter than that.
    fn load(isa: Self::Isa, src: &[Self::Elem]) -> Self;

    /// Writes the lanes to the first [`LANES`](Self::LANES) elements of
    /// `dst`, which need no particular alignment.
    ///
    /// # Panics
    ///
    /// Panics if `dst` is shorter than that.
    fn store(self, dst: &mut [Self::Elem]);

    /// The elements of `src`, at most [`LANES`](Self::LANES) of them, in
    /// the first lanes; the other lanes are `fill`.
    #[inline(always)]
    fn load_partial(isa: Self::Isa, src: &[Self::Elem], fill: Self::Elem) -> Self {
        let mut lanes = <Self::Elem as SimdSample>::Lanes::default();
        for (i, lane) in lanes.as_mut().iter_mut().enumerate() {
            *lane = src.get(i).copied().unwrap_or(fill);
        }
        Self::from_array(isa, lanes)
    }

    /// Writes the first lanes to `dst`, as many as it holds, at most
    /// [`LANES`](Self::LANES). Elements of `dst` past them are left alone.
    #[inline(always)]
    fn store_partial(self, dst: &mut [Self::Elem]) {
        let lanes = self.to_array();
        for (element, lane) in dst.iter_mut().zip(lanes.as_ref()) {
            *element = *lane;
        }
    }

    /// Loads from aligned memory.
    fn load_aligned(isa: Self::Isa, src: &Aligned<<Self::Elem as SimdSample>::Lanes>) -> Self;

    /// Stores to aligned memory.
    fn store_aligned(self, dst: &mut Aligned<<Self::Elem as SimdSample>::Lanes>);

    /// `self + a * b` in each lane, the product rounded before the sum (not
    /// a fused multiply-add), as [`vector::add_product`] does.
    ///
    /// [`vector::add_product`]: crate::vector::add_product
    #[inline(always)]
    fn add_product(self, a: Self, b: Self) -> Self {
        self + a * b
    }

    /// The sign bit of each lane cleared.
    fn abs(self) -> Self;

    /// The square root of each lane.
    fn sqrt(self) -> Self;

    /// `if self < other { self } else { other }` in each lane.
    fn min(self, other: Self) -> Self;

    /// `if self > other { self } else { other }` in each lane.
    fn max(self, other: Self) -> Self;

    /// `self.max(low).min(high)`: each lane held between `low` and `high`,
    /// where `low <= high`. A NaN lane gives `low`.
    #[inline(always)]
    fn clamp(self, low: Self, high: Self) -> Self {
        self.max(low).min(high)
    }

    /// Where `self == other`.
    fn simd_eq(self, other: Self) -> Self::Mask;
    /// Where `self != other`, which includes every NaN.
    fn simd_ne(self, other: Self) -> Self::Mask;
    /// Where `self < other`.
    fn simd_lt(self, other: Self) -> Self::Mask;
    /// Where `self <= other`.
    fn simd_le(self, other: Self) -> Self::Mask;
    /// Where `self > other`.
    fn simd_gt(self, other: Self) -> Self::Mask;
    /// Where `self >= other`.
    fn simd_ge(self, other: Self) -> Self::Mask;

    /// The sum of the lanes, in the fixed order: for 8 lanes
    /// `((a0 + a4) + (a2 + a6)) + ((a1 + a5) + (a3 + a7))`, for 4
    /// `(a0 + a2) + (a1 + a3)`.
    #[inline(always)]
    fn reduce_sum(self) -> Self::Elem {
        fold_in_pairs(self.to_array(), |a, b| a + b)
    }

    /// The smallest lane, by [`min`](Self::min) in the order of
    /// [`reduce_sum`](Self::reduce_sum). With a NaN among the lanes the
    /// result depends on where it is, though not on the backend.
    #[inline(always)]
    fn reduce_min(self) -> Self::Elem {
        fold_in_pairs(self.to_array(), lesser)
    }

    /// The largest lane, by [`max`](Self::max), as
    /// [`reduce_min`](Self::reduce_min).
    #[inline(always)]
    fn reduce_max(self) -> Self::Elem {
        fold_in_pairs(self.to_array(), greater)
    }
}

/// Combines `lanes[j]` with `lanes[j + half]`, `half` halving until one
/// element is left.
#[inline(always)]
fn fold_in_pairs<S: SimdSample>(mut lanes: S::Lanes, combine: impl Fn(S, S) -> S) -> S {
    let mut half = S::LANES / 2;
    while half > 0 {
        for j in 0..half {
            lanes[j] = combine(lanes[j], lanes[j + half]);
        }
        half /= 2;
    }
    lanes[0]
}

/// A truth value for each lane of a [`Register`], the result of comparing
/// two of them. Masks combine with `&`, `|`, `^` and `!`.
pub trait Mask:
    Copy
    + fmt::Debug
    + Send
    + Sync
    + 'static
    + sealed::Sealed
    + BitAnd<Output = Self>
    + BitOr<Output = Self>
    + BitXor<Output = Self>
    + Not<Output = Self>
{
    /// The register type the mask is about.
    type Register: Register<Mask = Self>;

    /// `if_set` in the lanes where the mask is set, `if_clear` in the
    /// others.
    fn select(self, if_set: Self::Register, if_clear: Self::Register) -> Self::Register;

    /// Bit `i` set where lane `i` is.
    fn bits(self) -> u32;

    /// Whether any lane is set.
    #[inline(always)]
    fn any(self) -> bool {
        self.bits() != 0
    }

    /// Whether every lane is set.
    #[inline(always)]
    fn all(self) -> bool {
        self.bits() == (1 << <Self::Register as Register>::LANES) - 1
    }
}

/// A loop written over registers, run by [`dispatch`] on the instruction
/// set the processor has.
pub trait Kernel {
    /// What the loop gives back.
    type Output;

    /// Runs the loop with the registers of `I`. Mark it `#[inline(always)]`
    /// (see the [module documentation](self)).
    fn run<I: Isa>(self, isa: I) -> Self::Output;
}

/// Runs a kernel on the fastest backend this processor has
/// ([`Backend::detect`]). Real-time safe: choosing costs a relaxed load of
/// an atomic.
#[inline]
pub fn dispatch<K: Kernel>(kernel: K) -> K::Output {
    Backend::detect().dispatch(kernel)
}

impl Backend {
    /// Runs a [`Kernel`] on this backend, or on the scalar one if this
    /// processor cannot run it, as every method of [`Backend`] does. This
    /// is how tests compare backends; [`simd::dispatch`](dispatch) picks
    /// the best.
    #[inline]
    pub fn dispatch<K: Kernel>(self, kernel: K) -> K::Output {
        match self.usable() {
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: `usable` returns only backends the processor has.
            Backend::Sse2 => unsafe { x86::sse2(kernel) },
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: as above, for AVX.
            Backend::Avx => unsafe { x86::avx(kernel) },
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: as above, for AVX and AVX2.
            Backend::Avx2 => unsafe { x86::avx2(kernel) },
            #[cfg(all(not(miri), target_arch = "aarch64", target_feature = "neon"))]
            Backend::Neon => neon::neon(kernel),
            #[cfg(all(not(miri), target_arch = "wasm32", target_feature = "simd128"))]
            Backend::Simd128 => wasm::simd128(kernel),
            _ => register::scalar(kernel),
        }
    }
}
