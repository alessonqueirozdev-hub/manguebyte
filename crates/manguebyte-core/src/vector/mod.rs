//! Operations on slices of samples, using the processor's vector unit.
//!
//! These are the loops audio code runs most: scaling, adding and mixing
//! blocks, gain ramps, peaks, sums of squares, dot products for filters,
//! the complex products of a convolution, a bank of sine partials for
//! additive synthesis, stereo interleaving and the conversions between
//! integer samples and `f32`. Each one has a plain
//! scalar version, which is its definition, and versions that use the
//! vector instructions of the processor it runs on:
//!
//! | [`Backend`] | Where | How it is chosen |
//! |---|---|---|
//! | [`Scalar`](Backend::Scalar) | everywhere | always available |
//! | [`Sse2`](Backend::Sse2) | x86 and x86-64 | detected at run time (always there on x86-64) |
//! | [`Avx`](Backend::Avx) | x86 and x86-64 | detected at run time |
//! | [`Avx2`](Backend::Avx2) | x86 and x86-64 | detected at run time (AVX and AVX2) |
//! | [`Neon`](Backend::Neon) | AArch64 | part of the architecture |
//! | [`Simd128`](Backend::Simd128) | WebAssembly | when built with `-C target-feature=+simd128` |
//!
//! The free functions of this module run on [`Backend::detect`], the best
//! backend of this processor. The processor is asked once, with the
//! `cpuid` instruction and no system call, and the answer is kept in an
//! atomic, so choosing costs a call on the audio thread a relaxed load.
//! Every method of [`Backend`] runs one backend in particular, which is how
//! the tests compare them; a backend the processor lacks runs the scalar
//! code instead. Nothing here allocates. Under Miri only the scalar code
//! runs.
//!
//! The functions take any [`Sample`]. `f32` and `f64` use the vector
//! instructions; another sample type runs the scalar definition.
//!
//! # Results are the same on every backend
//!
//! - **Element-wise operations** ([`scale`], [`copy_scaled`], [`add`],
//!   [`add_scaled`], [`multiply`], [`scale_ramp`], [`add_scaled_ramp`],
//!   [`add_product`], [`complex_multiply_add`], the stereo
//!   (de)interleaving and every
//!   conversion) do exactly what their scalar definition does to each
//!   element, in the same order: a product and then a sum, never a fused
//!   multiply-add, which rounds once instead of twice. Their results are
//!   the same bit for bit on every backend, and the tests check it. The
//!   one exception is a NaN: a result that is NaN is NaN everywhere, but
//!   its sign and payload are not specified, as in Rust itself.
//! - **[`peak`]** is a maximum, which is exact in any order, so it too is
//!   the same on every backend. A slice holding a NaN has a NaN peak.
//! - **Sums** ([`dot`] and [`sum_of_squares`]) are rounded after each
//!   addition, so their result depends on the order of the additions. The
//!   order here is fixed and does not depend on the backend: the elements
//!   are dealt in turn to 16 running sums for `f32` (8 for `f64`, and for
//!   [`sum_of_squares`], which sums in `f64`), as many whole rounds as the
//!   slice holds; the sums are added in pairs (`s[j] + s[j + half]`,
//!   halving until one is left); and the elements left over, fewer than
//!   one round, are added to that one after another. So a sum is the same
//!   on every backend and every machine, and depends only on the values it
//!   is given, not on where a block starts in memory. It is not the same
//!   as adding the elements one after another from the first; it is
//!   usually closer to the exact sum.
//! - **[`add_partials`]** works out every partial's samples, phasor and
//!   gain exactly as its definition does, one lane of a register per
//!   partial, and adds the samples of a frame in the same kind of fixed
//!   order: 16 running sums, then pairs, then the rest. Its output is the
//!   same on every backend too. The function says how far that order can
//!   take the result from adding the partials one after another.
//!
//! ```
//! use manguebyte_core::vector;
//!
//! let mut mix = [0.0f32; 5];
//! vector::add_scaled(&mut mix, &[1.0, 2.0, 3.0, 4.0, 5.0], 0.5);
//! assert_eq!(mix, [0.5, 1.0, 1.5, 2.0, 2.5]);
//! assert_eq!(vector::peak(&mix), 2.5);
//! assert_eq!(vector::dot(&mix, &[2.0; 5]), 15.0);
//! ```

use core::any::TypeId;
use core::sync::atomic::{AtomicU16, Ordering};

use crate::Sample;

mod scalar;

#[cfg(all(
    not(miri),
    any(
        target_arch = "x86",
        target_arch = "x86_64",
        all(target_arch = "aarch64", target_feature = "neon"),
        all(target_arch = "wasm32", target_feature = "simd128"),
    )
))]
mod generic;

#[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
mod x86;

#[cfg(all(not(miri), target_arch = "aarch64", target_feature = "neon"))]
mod neon;

#[cfg(all(not(miri), target_arch = "wasm32", target_feature = "simd128"))]
mod wasm;

#[cfg(test)]
mod tests;

/// A set of instructions the operations of this module can run on.
///
/// See the [module documentation](self) for what each one is and how the
/// best one is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// Plain Rust, one element at a time. Runs everywhere, and defines
    /// what every other backend computes.
    Scalar,
    /// SSE2, 128-bit vectors, on x86 and x86-64.
    Sse2,
    /// AVX, 256-bit vectors of floating-point numbers, on x86 and x86-64.
    /// The integer conversions use SSE2.
    Avx,
    /// AVX with AVX2, which adds 256-bit vectors of integers for the
    /// conversions, on x86 and x86-64.
    Avx2,
    /// NEON (Advanced SIMD), 128-bit vectors, on AArch64.
    Neon,
    /// WebAssembly's 128-bit SIMD, in a module built with the `simd128`
    /// target feature.
    Simd128,
}

/// What the processor has: bit `1 << backend as u8` for each backend it can
/// run, the fastest of them in the bits from `BEST_SHIFT` on, and `PROBED`
/// once the processor has been asked.
static AVAILABLE: AtomicU16 = AtomicU16::new(0);
const PROBED: u16 = 1 << 15;
const BEST_SHIFT: u32 = 8;

impl Backend {
    /// Every backend, the scalar one first.
    pub const ALL: [Backend; 6] = [
        Backend::Scalar,
        Backend::Sse2,
        Backend::Avx,
        Backend::Avx2,
        Backend::Neon,
        Backend::Simd128,
    ];

    /// The fastest backend this processor has. Asks the processor on the
    /// first call and remembers the answer, so later calls cost one atomic
    /// load. Real-time safe.
    #[inline]
    pub fn detect() -> Backend {
        match (available() >> BEST_SHIFT) & 0x7 {
            1 => Backend::Sse2,
            2 => Backend::Avx,
            3 => Backend::Avx2,
            4 => Backend::Neon,
            5 => Backend::Simd128,
            _ => Backend::Scalar,
        }
    }

    /// Whether this processor and this build can run the backend. A method
    /// called on a backend that cannot run uses the scalar code instead.
    #[inline]
    pub fn is_available(self) -> bool {
        available() & self.bit() != 0
    }

    /// The backends this processor can run, the scalar one first.
    pub fn available() -> impl Iterator<Item = Backend> {
        Self::ALL
            .into_iter()
            .filter(|backend| backend.is_available())
    }

    /// A short name, such as `"avx2"`.
    pub const fn name(self) -> &'static str {
        match self {
            Backend::Scalar => "scalar",
            Backend::Sse2 => "sse2",
            Backend::Avx => "avx",
            Backend::Avx2 => "avx2",
            Backend::Neon => "neon",
            Backend::Simd128 => "simd128",
        }
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }

    /// The backend to run: this one if it can, the scalar one if not.
    #[inline]
    fn usable(self) -> Backend {
        if self.is_available() {
            self
        } else {
            Backend::Scalar
        }
    }
}

#[inline]
fn available() -> u16 {
    let known = AVAILABLE.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    let found = probe();
    // Fastest first.
    let best = [
        Backend::Avx2,
        Backend::Avx,
        Backend::Sse2,
        Backend::Neon,
        Backend::Simd128,
    ]
    .into_iter()
    .find(|backend| found & backend.bit() != 0)
    .unwrap_or(Backend::Scalar);
    let known = found | (best as u16) << BEST_SHIFT | PROBED;
    AVAILABLE.store(known, Ordering::Relaxed);
    known
}

/// Asks the processor which backends it has. `is_x86_feature_detected!`
/// runs `cpuid` (and `xgetbv`, to know that the system saves the AVX
/// registers), which are instructions, not system calls.
fn probe() -> u16 {
    #[allow(unused_mut)]
    let mut found = Backend::Scalar.bit();
    #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
    {
        if std::arch::is_x86_feature_detected!("sse2") {
            found |= Backend::Sse2.bit();
        }
        if std::arch::is_x86_feature_detected!("avx") {
            found |= Backend::Avx.bit();
            if std::arch::is_x86_feature_detected!("avx2") {
                found |= Backend::Avx2.bit();
            }
        }
    }
    #[cfg(all(not(miri), target_arch = "aarch64", target_feature = "neon"))]
    {
        found |= Backend::Neon.bit();
    }
    #[cfg(all(not(miri), target_arch = "wasm32", target_feature = "simd128"))]
    {
        found |= Backend::Simd128.bit();
    }
    found
}

// The slices of a generic sample type seen as the type they are. `TypeId`
// comparisons are worked out when the function is compiled for a type, so
// these cost nothing.

#[inline(always)]
fn as_type<S: 'static, T: 'static>(slice: &[S]) -> Option<&[T]> {
    if TypeId::of::<S>() == TypeId::of::<T>() {
        // SAFETY: `S` and `T` are the same type, so the pointer, length and
        // lifetime describe the same valid slice.
        Some(unsafe { core::slice::from_raw_parts(slice.as_ptr().cast::<T>(), slice.len()) })
    } else {
        None
    }
}

#[inline(always)]
fn as_type_mut<S: 'static, T: 'static>(slice: &mut [S]) -> Option<&mut [T]> {
    if TypeId::of::<S>() == TypeId::of::<T>() {
        // SAFETY: as in `as_type`; the borrow is moved into the result, so
        // the slice stays uniquely borrowed.
        Some(unsafe {
            core::slice::from_raw_parts_mut(slice.as_mut_ptr().cast::<T>(), slice.len())
        })
    } else {
        None
    }
}

#[inline(always)]
fn as_value<S: 'static + Copy, T: 'static + Copy>(value: S) -> T {
    debug_assert!(TypeId::of::<S>() == TypeId::of::<T>());
    // SAFETY: only called once `S` and `T` were found to be the same type.
    unsafe { core::mem::transmute_copy(&value) }
}

/// Runs one operation on the backend `$backend`, which must be available:
/// `$name` names the function each backend module has for the element type.
macro_rules! run {
    ($backend:expr, $name:ident ( $($arg:expr),* )) => {{
        match $backend {
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: `usable` and `detect` return only backends the
            // processor has.
            Backend::Sse2 => unsafe { x86::sse2::$name($($arg),*) },
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: as above, for AVX.
            Backend::Avx => unsafe { x86::avx::$name($($arg),*) },
            #[cfg(all(not(miri), any(target_arch = "x86", target_arch = "x86_64")))]
            // SAFETY: as above, for AVX and AVX2.
            Backend::Avx2 => unsafe { x86::avx2::$name($($arg),*) },
            #[cfg(all(not(miri), target_arch = "aarch64", target_feature = "neon"))]
            // SAFETY: NEON is part of every processor this was built for.
            Backend::Neon => unsafe { neon::$name($($arg),*) },
            #[cfg(all(not(miri), target_arch = "wasm32", target_feature = "simd128"))]
            // SAFETY: the module was built for SIMD, so it only runs where
            // SIMD is supported.
            Backend::Simd128 => unsafe { wasm::$name($($arg),*) },
            _ => scalar::$name($($arg),*),
        }
    }};
}

/// Runs an operation on one slice of samples and values, for `f32`, `f64`
/// or, with the scalar code, any other sample type.
macro_rules! by_type {
    (
        $backend:expr, $sample:ty;
        f32 => $f32:ident ( $($a32:expr),* ),
        f64 => $f64:ident ( $($a64:expr),* ),
        other => $other:expr $(,)?
    ) => {{
        if TypeId::of::<$sample>() == TypeId::of::<f32>() {
            run!($backend, $f32($($a32),*))
        } else if TypeId::of::<$sample>() == TypeId::of::<f64>() {
            run!($backend, $f64($($a64),*))
        } else {
            $other
        }
    }};
}

#[track_caller]
#[inline]
fn same_length(a: usize, b: usize) {
    assert!(a == b, "slice lengths differ: {a} and {b}");
}

/// What a peak kernel found: the largest magnitude of the values that are
/// not NaN, and whether any was NaN.
#[derive(Debug, Clone, Copy)]
struct Peak<T> {
    largest: T,
    nan: bool,
}

impl<T: Sample> Peak<T> {
    fn value(self) -> T {
        if self.nan {
            T::from_f64(f64::NAN)
        } else {
            self.largest
        }
    }
}

/// A bank of sine partials, one element of each slice per partial, for
/// [`Backend::add_partials`]. Every slice has the same length.
///
/// A partial is a point `(cos, sin)` on the unit circle turned each frame
/// by the rotation `(cos_step, sin_step)`, which is `(cos w, sin w)` for a
/// partial of `w` radians per frame, under a gain that grows by
/// `gain_step` each frame. Its output is `gain * sin`, sent to the left
/// and right channels scaled by `left_gain` and `right_gain`.
#[derive(Debug)]
pub struct Partials<'a> {
    /// The cosines of the phasors: advanced by each call.
    pub cos: &'a mut [f32],
    /// The sines of the phasors, which are what is heard: advanced by each
    /// call.
    pub sin: &'a mut [f32],
    /// The gains: advanced by each call.
    pub gain: &'a mut [f32],
    /// The cosines of the rotations each frame.
    pub cos_step: &'a [f32],
    /// The sines of the rotations each frame.
    pub sin_step: &'a [f32],
    /// What each gain grows by each frame.
    pub gain_step: &'a [f32],
    /// What the output is scaled by on the left channel.
    pub left_gain: &'a [f32],
    /// What the output is scaled by on the right channel.
    pub right_gain: &'a [f32],
}

impl Partials<'_> {
    /// The number of partials: the length of the slices.
    pub fn len(&self) -> usize {
        self.cos.len()
    }

    /// Whether there are no partials.
    pub fn is_empty(&self) -> bool {
        self.cos.is_empty()
    }

    #[track_caller]
    fn check(&self) {
        let n = self.cos.len();
        for len in [
            self.sin.len(),
            self.gain.len(),
            self.cos_step.len(),
            self.sin_step.len(),
            self.gain_step.len(),
            self.left_gain.len(),
            self.right_gain.len(),
        ] {
            same_length(n, len);
        }
    }
}

impl Backend {
    /// Multiplies every element of `dst` by `gain`: `dst[i] = dst[i] * gain`.
    #[inline]
    pub fn scale<S: Sample>(self, dst: &mut [S], gain: S) {
        let backend = self.usable();
        by_type!(backend, S;
            f32 => scale_f32(as_type_mut(dst).unwrap(), as_value(gain)),
            f64 => scale_f64(as_type_mut(dst).unwrap(), as_value(gain)),
            other => scalar::scale(dst, gain),
        );
    }

    /// Copies `src` into `dst`, scaled: `dst[i] = src[i] * gain`.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn copy_scaled<S: Sample>(self, dst: &mut [S], src: &[S], gain: S) {
        same_length(dst.len(), src.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => copy_scaled_f32(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(gain)),
            f64 => copy_scaled_f64(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(gain)),
            other => scalar::copy_scaled(dst, src, gain),
        );
    }

    /// Adds `src` to `dst`: `dst[i] = dst[i] + src[i]`.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn add<S: Sample>(self, dst: &mut [S], src: &[S]) {
        same_length(dst.len(), src.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => add_f32(as_type_mut(dst).unwrap(), as_type(src).unwrap()),
            f64 => add_f64(as_type_mut(dst).unwrap(), as_type(src).unwrap()),
            other => scalar::add(dst, src),
        );
    }

    /// Adds `src` scaled by `gain` to `dst`: `dst[i] = dst[i] + src[i] * gain`,
    /// the product rounded before the sum.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn add_scaled<S: Sample>(self, dst: &mut [S], src: &[S], gain: S) {
        same_length(dst.len(), src.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => add_scaled_f32(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(gain)),
            f64 => add_scaled_f64(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(gain)),
            other => scalar::add_scaled(dst, src, gain),
        );
    }

    /// Multiplies `dst` by `src`, element by element: `dst[i] = dst[i] * src[i]`.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn multiply<S: Sample>(self, dst: &mut [S], src: &[S]) {
        same_length(dst.len(), src.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => multiply_f32(as_type_mut(dst).unwrap(), as_type(src).unwrap()),
            f64 => multiply_f64(as_type_mut(dst).unwrap(), as_type(src).unwrap()),
            other => scalar::multiply(dst, src),
        );
    }

    /// Multiplies `dst` by a linear ramp:
    /// `dst[i] = dst[i] * (start + step * i)`, with `i` converted to the
    /// sample type.
    ///
    /// Each gain is worked out from its index, not by adding `step` again
    /// and again, so the ramp does not drift and any element's gain is
    /// known without the ones before it.
    #[inline]
    pub fn scale_ramp<S: Sample>(self, dst: &mut [S], start: S, step: S) {
        let backend = self.usable();
        by_type!(backend, S;
            f32 => scale_ramp_f32(as_type_mut(dst).unwrap(), as_value(start), as_value(step)),
            f64 => scale_ramp_f64(as_type_mut(dst).unwrap(), as_value(start), as_value(step)),
            other => scalar::scale_ramp(dst, start, step, 0),
        );
    }

    /// Adds `src` under a linear ramp to `dst`:
    /// `dst[i] = dst[i] + src[i] * (start + step * i)`, the gain worked out
    /// as in [`scale_ramp`](Self::scale_ramp) and each product rounded
    /// before the sum. This is a gain change mixed into a bus without a
    /// zipper.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn add_scaled_ramp<S: Sample>(self, dst: &mut [S], src: &[S], start: S, step: S) {
        same_length(dst.len(), src.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => add_scaled_ramp_f32(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(start), as_value(step)),
            f64 => add_scaled_ramp_f64(as_type_mut(dst).unwrap(), as_type(src).unwrap(), as_value(start), as_value(step)),
            other => scalar::add_scaled_ramp(dst, src, start, step, 0),
        );
    }

    /// Adds the products of `a` and `b` to `dst`:
    /// `dst[i] = dst[i] + a[i] * b[i]`, the product rounded before the sum
    /// (not a fused multiply-add). This is a block mixed in under a gain
    /// that changes every frame, such as an envelope.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn add_product<S: Sample>(self, dst: &mut [S], a: &[S], b: &[S]) {
        same_length(dst.len(), a.len());
        same_length(dst.len(), b.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => add_product_f32(as_type_mut(dst).unwrap(), as_type(a).unwrap(), as_type(b).unwrap()),
            f64 => add_product_f64(as_type_mut(dst).unwrap(), as_type(a).unwrap(), as_type(b).unwrap()),
            other => scalar::add_product(dst, a, b),
        );
    }

    /// Renders a bank of sine partials for `left.len()` frames and adds it
    /// to `left` and `right`, advancing the phasors and gains in
    /// `partials`.
    ///
    /// Each frame, each partial does exactly this, in this order:
    ///
    /// ```text
    /// gain += gain_step
    /// v = gain * sin
    /// (add v * left_gain to the left sum and v * right_gain to the right)
    /// (cos, sin) = (cos * cos_step - sin * sin_step, cos * sin_step + sin * cos_step)
    /// ```
    ///
    /// so the gain moves before the sample is made and the phasor turns
    /// after. Every backend works out these numbers for every partial bit
    /// for bit as written, a lane of a vector register per partial, which
    /// is what makes a large bank fast: the partials are independent, and
    /// a rotation that waits on the one before it waits once per register
    /// rather than once per partial.
    ///
    /// The sums are added in an order that does not depend on the backend,
    /// so the output too is the same on every backend. For each frame, the
    /// partials before the last whole group of 16 add to 16 running sums
    /// that start at zero, partial `i` to sum `i % 16`, in the order of
    /// `i`; the sums are added in pairs (`s[j] + s[j + 8]`, halving until
    /// one is left) and the result added to the output sample. The
    /// partials after them, fewer than 16, then add to the output one after
    /// another. Against adding every partial straight to the output one
    /// after another, the result differs only by the rounding of the sums:
    /// by less than `2 * (len + 5) * 2^-24` times the sum of the magnitudes
    /// of the terms, and usually far less.
    ///
    /// The phasors are not brought back to the unit circle here. Each
    /// rotation in `f32` moves a phasor's magnitude by up to a few parts in
    /// 10^7, so a caller that runs a bank for long must rescale them every
    /// so often (`PartialBank` in `manguebyte-dsp` does it every 64
    /// frames).
    ///
    /// A bank whose length is a multiple of 16 runs entirely in vector
    /// registers; the partials after the last whole group run one at a
    /// time. Nothing allocates.
    ///
    /// # Panics
    ///
    /// Panics if `left` and `right` differ in length, or the slices of
    /// `partials` do.
    #[inline]
    #[track_caller]
    pub fn add_partials(self, left: &mut [f32], right: &mut [f32], partials: &mut Partials<'_>) {
        same_length(left.len(), right.len());
        partials.check();
        run!(self.usable(), add_partials(left, right, partials));
    }

    /// The largest magnitude in `src`, zero for an empty slice. A slice
    /// holding a NaN has a NaN peak, so that a test such as
    /// `peak < 1.0` fails rather than passing on garbage.
    #[inline]
    pub fn peak<S: Sample>(self, src: &[S]) -> S {
        let backend = self.usable();
        if TypeId::of::<S>() == TypeId::of::<f32>() {
            as_value(run!(backend, peak_f32(as_type(src).unwrap())).value())
        } else if TypeId::of::<S>() == TypeId::of::<f64>() {
            as_value(run!(backend, peak_f64(as_type(src).unwrap())).value())
        } else {
            scalar::peak(src).value()
        }
    }

    /// The sum of `a[i] * b[i]`, in the order the
    /// [module documentation](self) gives.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length.
    #[inline]
    #[track_caller]
    pub fn dot<S: Sample>(self, a: &[S], b: &[S]) -> S {
        same_length(a.len(), b.len());
        let backend = self.usable();
        if TypeId::of::<S>() == TypeId::of::<f32>() {
            as_value(run!(
                backend,
                dot_f32(as_type(a).unwrap(), as_type(b).unwrap())
            ))
        } else if TypeId::of::<S>() == TypeId::of::<f64>() {
            as_value(run!(
                backend,
                dot_f64(as_type(a).unwrap(), as_type(b).unwrap())
            ))
        } else {
            scalar::dot(a, b)
        }
    }

    /// The sum of the squares of `src`, each square and the sum in `f64`,
    /// in the order the [module documentation](self) gives. The square of
    /// an `f32` is exact in `f64`.
    #[inline]
    pub fn sum_of_squares<S: Sample>(self, src: &[S]) -> f64 {
        let backend = self.usable();
        by_type!(backend, S;
            f32 => sum_of_squares_f32(as_type(src).unwrap()),
            f64 => sum_of_squares_f64(as_type(src).unwrap()),
            other => scalar::sum_of_squares(src),
        )
    }

    /// Writes `left` and `right` into `dst` as interleaved frames
    /// (`L R L R ...`).
    ///
    /// # Panics
    ///
    /// Panics unless `left` and `right` have the same length and `dst` is
    /// twice as long.
    #[inline]
    #[track_caller]
    pub fn interleave_stereo<S: Sample>(self, dst: &mut [S], left: &[S], right: &[S]) {
        same_length(left.len(), right.len());
        same_length(dst.len(), 2 * left.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => interleave_stereo_f32(as_type_mut(dst).unwrap(), as_type(left).unwrap(), as_type(right).unwrap()),
            f64 => interleave_stereo_f64(as_type_mut(dst).unwrap(), as_type(left).unwrap(), as_type(right).unwrap()),
            other => scalar::interleave_stereo(dst, left, right),
        );
    }

    /// Splits interleaved frames (`L R L R ...`) from `src` into `left` and
    /// `right`.
    ///
    /// # Panics
    ///
    /// Panics unless `left` and `right` have the same length and `src` is
    /// twice as long.
    #[inline]
    #[track_caller]
    pub fn deinterleave_stereo<S: Sample>(self, left: &mut [S], right: &mut [S], src: &[S]) {
        same_length(left.len(), right.len());
        same_length(src.len(), 2 * left.len());
        let backend = self.usable();
        by_type!(backend, S;
            f32 => deinterleave_stereo_f32(as_type_mut(left).unwrap(), as_type_mut(right).unwrap(), as_type(src).unwrap()),
            f64 => deinterleave_stereo_f64(as_type_mut(left).unwrap(), as_type_mut(right).unwrap(), as_type(src).unwrap()),
            other => scalar::deinterleave_stereo(left, right, src),
        );
    }

    /// Adds the products of complex numbers to `sum`: `sum[k] += a[k] * b[k]`
    /// for complex numbers stored as interleaved parts (`re im re im ...`).
    /// For each number, the real part becomes
    /// `sum.re + (a.re * b.re - a.im * b.im)` and the imaginary part
    /// `sum.im + (a.re * b.im + a.im * b.re)`, rounded in that order.
    ///
    /// This is the inner loop of a convolution in the frequency domain.
    ///
    /// # Panics
    ///
    /// Panics if the slices differ in length or the length is odd.
    #[inline]
    #[track_caller]
    pub fn complex_multiply_add<S: Sample>(self, sum: &mut [S], a: &[S], b: &[S]) {
        same_length(sum.len(), a.len());
        same_length(sum.len(), b.len());
        assert!(
            sum.len() % 2 == 0,
            "complex numbers take two elements each, but the length is odd"
        );
        let backend = self.usable();
        by_type!(backend, S;
            f32 => complex_multiply_add_f32(as_type_mut(sum).unwrap(), as_type(a).unwrap(), as_type(b).unwrap()),
            f64 => complex_multiply_add_f64(as_type_mut(sum).unwrap(), as_type(a).unwrap(), as_type(b).unwrap()),
            other => scalar::complex_multiply_add(sum, a, b),
        );
    }

    /// Decodes little-endian 16-bit integers: `dst[i] = value / 32768`.
    ///
    /// # Panics
    ///
    /// Panics unless `src` holds two bytes for every element of `dst`.
    #[inline]
    #[track_caller]
    pub fn i16_le_to_f32(self, dst: &mut [f32], src: &[u8]) {
        same_length(src.len(), 2 * dst.len());
        run!(self.usable(), i16_le_to_f32(dst, src));
    }

    /// Decodes little-endian 24-bit integers, three bytes each:
    /// `dst[i] = value / 8388608`.
    ///
    /// # Panics
    ///
    /// Panics unless `src` holds three bytes for every element of `dst`.
    #[inline]
    #[track_caller]
    pub fn i24_le_to_f32(self, dst: &mut [f32], src: &[u8]) {
        same_length(src.len(), 3 * dst.len());
        run!(self.usable(), i24_le_to_f32(dst, src));
    }

    /// Decodes little-endian 32-bit integers: `dst[i] = value / 2147483648`,
    /// rounded to the nearest `f32` once.
    ///
    /// # Panics
    ///
    /// Panics unless `src` holds four bytes for every element of `dst`.
    #[inline]
    #[track_caller]
    pub fn i32_le_to_f32(self, dst: &mut [f32], src: &[u8]) {
        same_length(src.len(), 4 * dst.len());
        run!(self.usable(), i32_le_to_f32(dst, src));
    }

    /// Encodes little-endian 16-bit integers, as a WAV file stores them:
    /// each sample is clamped to `-1.0..=1.0`, multiplied by 32767 in
    /// `f32` and rounded to the nearest integer, halves away from zero
    /// ([`f32::round`]). A NaN becomes 0.
    ///
    /// # Panics
    ///
    /// Panics unless `dst` has two bytes for every element of `src`.
    #[inline]
    #[track_caller]
    pub fn f32_to_i16_le(self, dst: &mut [u8], src: &[f32]) {
        same_length(dst.len(), 2 * src.len());
        run!(self.usable(), f32_to_i16_le(dst, src));
    }

    /// Encodes little-endian 24-bit integers, three bytes each, as a WAV
    /// file stores them: each sample is clamped to `-1.0..=1.0`, multiplied
    /// by 8388607 in `f64` (which is exact) and rounded to the nearest
    /// integer, halves away from zero. A NaN becomes 0.
    ///
    /// # Panics
    ///
    /// Panics unless `dst` has three bytes for every element of `src`.
    #[inline]
    #[track_caller]
    pub fn f32_to_i24_le(self, dst: &mut [u8], src: &[f32]) {
        same_length(dst.len(), 3 * src.len());
        run!(self.usable(), f32_to_i24_le(dst, src));
    }
}

/// [`Backend::scale`] on the [detected](Backend::detect) backend.
#[inline]
pub fn scale<S: Sample>(dst: &mut [S], gain: S) {
    Backend::detect().scale(dst, gain);
}

/// [`Backend::copy_scaled`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn copy_scaled<S: Sample>(dst: &mut [S], src: &[S], gain: S) {
    Backend::detect().copy_scaled(dst, src, gain);
}

/// [`Backend::add`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn add<S: Sample>(dst: &mut [S], src: &[S]) {
    Backend::detect().add(dst, src);
}

/// [`Backend::add_scaled`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn add_scaled<S: Sample>(dst: &mut [S], src: &[S], gain: S) {
    Backend::detect().add_scaled(dst, src, gain);
}

/// [`Backend::multiply`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn multiply<S: Sample>(dst: &mut [S], src: &[S]) {
    Backend::detect().multiply(dst, src);
}

/// [`Backend::scale_ramp`] on the [detected](Backend::detect) backend.
#[inline]
pub fn scale_ramp<S: Sample>(dst: &mut [S], start: S, step: S) {
    Backend::detect().scale_ramp(dst, start, step);
}

/// [`Backend::add_scaled_ramp`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn add_scaled_ramp<S: Sample>(dst: &mut [S], src: &[S], start: S, step: S) {
    Backend::detect().add_scaled_ramp(dst, src, start, step);
}

/// [`Backend::add_product`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn add_product<S: Sample>(dst: &mut [S], a: &[S], b: &[S]) {
    Backend::detect().add_product(dst, a, b);
}

/// [`Backend::add_partials`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn add_partials(left: &mut [f32], right: &mut [f32], partials: &mut Partials<'_>) {
    Backend::detect().add_partials(left, right, partials);
}

/// [`Backend::peak`] on the [detected](Backend::detect) backend.
#[inline]
pub fn peak<S: Sample>(src: &[S]) -> S {
    Backend::detect().peak(src)
}

/// [`Backend::dot`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn dot<S: Sample>(a: &[S], b: &[S]) -> S {
    Backend::detect().dot(a, b)
}

/// [`Backend::sum_of_squares`] on the [detected](Backend::detect) backend.
#[inline]
pub fn sum_of_squares<S: Sample>(src: &[S]) -> f64 {
    Backend::detect().sum_of_squares(src)
}

/// [`Backend::interleave_stereo`] on the [detected](Backend::detect)
/// backend.
#[inline]
#[track_caller]
pub fn interleave_stereo<S: Sample>(dst: &mut [S], left: &[S], right: &[S]) {
    Backend::detect().interleave_stereo(dst, left, right);
}

/// [`Backend::deinterleave_stereo`] on the [detected](Backend::detect)
/// backend.
#[inline]
#[track_caller]
pub fn deinterleave_stereo<S: Sample>(left: &mut [S], right: &mut [S], src: &[S]) {
    Backend::detect().deinterleave_stereo(left, right, src);
}

/// [`Backend::complex_multiply_add`] on the [detected](Backend::detect)
/// backend.
#[inline]
#[track_caller]
pub fn complex_multiply_add<S: Sample>(sum: &mut [S], a: &[S], b: &[S]) {
    Backend::detect().complex_multiply_add(sum, a, b);
}

/// [`Backend::i16_le_to_f32`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn i16_le_to_f32(dst: &mut [f32], src: &[u8]) {
    Backend::detect().i16_le_to_f32(dst, src);
}

/// [`Backend::i24_le_to_f32`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn i24_le_to_f32(dst: &mut [f32], src: &[u8]) {
    Backend::detect().i24_le_to_f32(dst, src);
}

/// [`Backend::i32_le_to_f32`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn i32_le_to_f32(dst: &mut [f32], src: &[u8]) {
    Backend::detect().i32_le_to_f32(dst, src);
}

/// [`Backend::f32_to_i16_le`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn f32_to_i16_le(dst: &mut [u8], src: &[f32]) {
    Backend::detect().f32_to_i16_le(dst, src);
}

/// [`Backend::f32_to_i24_le`] on the [detected](Backend::detect) backend.
#[inline]
#[track_caller]
pub fn f32_to_i24_le(dst: &mut [u8], src: &[f32]) {
    Backend::detect().f32_to_i24_le(dst, src);
}
