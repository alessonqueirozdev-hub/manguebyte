//! The vector kernels, written once over a [`Vector`]: a register of a
//! backend and the instructions on it. Each backend module implements the
//! trait for its `f32` and `f64` registers and instantiates the kernels in
//! functions compiled for its instruction set, with
//! [`entry_points!`](crate::vector::generic::entry_points).
//!
//! Every kernel does to each element exactly what the scalar definition in
//! [`scalar`] does, and leaves the elements past the last whole register to
//! that definition. The sums keep the running sums of the definition, one
//! per lane, so they too give the definition's result.

use super::scalar::{self, PARTIAL_FRAMES, PARTIAL_LANES, SQUARE_LANES};
use super::{Partials, Peak};
use crate::Sample;
use core::mem::MaybeUninit;

/// A vector register and the operations the kernels need on it.
///
/// Every method is `unsafe` because it runs instructions that the
/// processor may lack: the caller must know that it has them. The methods
/// are inlined into the kernels, and the kernels into functions compiled
/// for the instruction set, so that each operation is one instruction.
pub(super) trait Vector {
    /// The element type.
    type T: Sample;
    /// The register.
    type Reg: Copy;
    /// Elements in a register.
    const LANES: usize;

    /// Loads `LANES` elements from `p`, which need not be aligned.
    unsafe fn load(p: *const Self::T) -> Self::Reg;
    /// Stores `LANES` elements at `p`, which need not be aligned.
    unsafe fn store(p: *mut Self::T, v: Self::Reg);
    /// Every lane set to `x`.
    unsafe fn splat(x: Self::T) -> Self::Reg;
    unsafe fn add(a: Self::Reg, b: Self::Reg) -> Self::Reg;
    unsafe fn mul(a: Self::Reg, b: Self::Reg) -> Self::Reg;
    /// The sign bit cleared.
    unsafe fn abs(a: Self::Reg) -> Self::Reg;
    /// The larger of each pair. What it gives for a NaN does not matter.
    unsafe fn max(a: Self::Reg, b: Self::Reg) -> Self::Reg;
    /// Every bit set in the lanes that hold a NaN, none in the others.
    unsafe fn nan_mask(a: Self::Reg) -> Self::Reg;
    unsafe fn or(a: Self::Reg, b: Self::Reg) -> Self::Reg;
    unsafe fn xor(a: Self::Reg, b: Self::Reg) -> Self::Reg;
    /// Whether any lane of a mask is set.
    unsafe fn any(mask: Self::Reg) -> bool;
    /// `a0 b0 a1 b1 ...`, in two registers.
    unsafe fn zip(a: Self::Reg, b: Self::Reg) -> (Self::Reg, Self::Reg);
    /// The even elements of `a` then `b`, and the odd ones: the inverse of
    /// [`zip`](Self::zip).
    unsafe fn unzip(a: Self::Reg, b: Self::Reg) -> (Self::Reg, Self::Reg);
    /// `a0 a0 a2 a2 ...`
    unsafe fn dup_even(a: Self::Reg) -> Self::Reg;
    /// `a1 a1 a3 a3 ...`
    unsafe fn dup_odd(a: Self::Reg) -> Self::Reg;
    /// `a1 a0 a3 a2 ...`
    unsafe fn swap_pairs(a: Self::Reg) -> Self::Reg;
    /// The lanes added in the definition's pairs, `a[j] + a[j + half]`
    /// with `half` halving from `LANES / 2` to 1: lane 0 at the end.
    unsafe fn reduce(a: Self::Reg) -> Self::T;
}

/// An `f64` register that can be loaded from `f32` elements.
pub(super) trait Widen: Vector<T = f64> {
    /// Loads 8 `f32` elements from `p`, each made an `f64` (exactly), into
    /// the first `8 / LANES` registers, in order.
    unsafe fn load_widened(p: *const f32) -> [Self::Reg; 4];
}

/// The largest number of registers a kernel keeps sums or maxima in.
const MAX_REGS: usize = 4;

/// Room for the elements of one register of any backend (at most 8, `f32`
/// in AVX), with some to spare.
const MAX_LANES: usize = 16;

#[inline(always)]
pub(super) unsafe fn scale<V: Vector>(dst: &mut [V::T], gain: V::T) {
    let body = dst.len() - dst.len() % V::LANES;
    let d = dst.as_mut_ptr();
    // SAFETY: every element read and written is below `body`, inside the
    // slice; the caller knows the processor has the instructions.
    unsafe {
        let g = V::splat(gain);
        let mut i = 0;
        while i < body {
            V::store(d.add(i), V::mul(V::load(d.add(i)), g));
            i += V::LANES;
        }
    }
    scalar::scale(&mut dst[body..], gain);
}

#[inline(always)]
pub(super) unsafe fn copy_scaled<V: Vector>(dst: &mut [V::T], src: &[V::T], gain: V::T) {
    let body = dst.len() - dst.len() % V::LANES;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `scale`; `src` is as long as `dst`.
    unsafe {
        let g = V::splat(gain);
        let mut i = 0;
        while i < body {
            V::store(d.add(i), V::mul(V::load(s.add(i)), g));
            i += V::LANES;
        }
    }
    scalar::copy_scaled(&mut dst[body..], &src[body..], gain);
}

#[inline(always)]
pub(super) unsafe fn add<V: Vector>(dst: &mut [V::T], src: &[V::T]) {
    let body = dst.len() - dst.len() % V::LANES;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `copy_scaled`.
    unsafe {
        let mut i = 0;
        while i < body {
            V::store(d.add(i), V::add(V::load(d.add(i)), V::load(s.add(i))));
            i += V::LANES;
        }
    }
    scalar::add(&mut dst[body..], &src[body..]);
}

#[inline(always)]
pub(super) unsafe fn add_scaled<V: Vector>(dst: &mut [V::T], src: &[V::T], gain: V::T) {
    let body = dst.len() - dst.len() % V::LANES;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `copy_scaled`.
    unsafe {
        let g = V::splat(gain);
        let mut i = 0;
        while i < body {
            let product = V::mul(V::load(s.add(i)), g);
            V::store(d.add(i), V::add(V::load(d.add(i)), product));
            i += V::LANES;
        }
    }
    scalar::add_scaled(&mut dst[body..], &src[body..], gain);
}

#[inline(always)]
pub(super) unsafe fn multiply<V: Vector>(dst: &mut [V::T], src: &[V::T]) {
    let body = dst.len() - dst.len() % V::LANES;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `copy_scaled`.
    unsafe {
        let mut i = 0;
        while i < body {
            V::store(d.add(i), V::mul(V::load(d.add(i)), V::load(s.add(i))));
            i += V::LANES;
        }
    }
    scalar::multiply(&mut dst[body..], &src[body..]);
}

/// The ramp, with the indices kept in a register and stepped by `LANES`.
/// That is exact while every index is exact in the element type, so the
/// registers stop at `exact` elements and the definition does the rest.
#[inline(always)]
pub(super) unsafe fn scale_ramp<V: Vector>(
    dst: &mut [V::T],
    start: V::T,
    step: V::T,
    exact: usize,
) {
    let limit = dst.len().min(exact);
    let body = limit - limit % V::LANES;
    let d = dst.as_mut_ptr();
    let mut first = [V::T::ZERO; MAX_LANES];
    for (k, value) in first.iter_mut().enumerate().take(V::LANES) {
        *value = scalar::index(k);
    }
    // SAFETY: as in `scale`; `first` holds at least `LANES` elements.
    unsafe {
        let mut index = V::load(first.as_ptr());
        let advance = V::splat(scalar::index(V::LANES));
        let (start_v, step_v) = (V::splat(start), V::splat(step));
        let mut i = 0;
        while i < body {
            let gain = V::add(start_v, V::mul(step_v, index));
            V::store(d.add(i), V::mul(V::load(d.add(i)), gain));
            index = V::add(index, advance);
            i += V::LANES;
        }
    }
    scalar::scale_ramp(&mut dst[body..], start, step, body);
}

/// The ramp of [`scale_ramp`], applied to `src` and added to `dst`.
#[inline(always)]
pub(super) unsafe fn add_scaled_ramp<V: Vector>(
    dst: &mut [V::T],
    src: &[V::T],
    start: V::T,
    step: V::T,
    exact: usize,
) {
    let limit = dst.len().min(exact);
    let body = limit - limit % V::LANES;
    let (d, s) = (dst.as_mut_ptr(), src.as_ptr());
    let mut first = [V::T::ZERO; MAX_LANES];
    for (k, value) in first.iter_mut().enumerate().take(V::LANES) {
        *value = scalar::index(k);
    }
    // SAFETY: as in `scale_ramp`; `src` is as long as `dst`.
    unsafe {
        let mut index = V::load(first.as_ptr());
        let advance = V::splat(scalar::index(V::LANES));
        let (start_v, step_v) = (V::splat(start), V::splat(step));
        let mut i = 0;
        while i < body {
            let gain = V::add(start_v, V::mul(step_v, index));
            let product = V::mul(V::load(s.add(i)), gain);
            V::store(d.add(i), V::add(V::load(d.add(i)), product));
            index = V::add(index, advance);
            i += V::LANES;
        }
    }
    scalar::add_scaled_ramp(&mut dst[body..], &src[body..], start, step, body);
}

#[inline(always)]
pub(super) unsafe fn add_product<V: Vector>(dst: &mut [V::T], a: &[V::T], b: &[V::T]) {
    let body = dst.len() - dst.len() % V::LANES;
    let (d, pa, pb) = (dst.as_mut_ptr(), a.as_ptr(), b.as_ptr());
    // SAFETY: as in `copy_scaled`; `a` and `b` are as long as `dst`.
    unsafe {
        let mut i = 0;
        while i < body {
            let product = V::mul(V::load(pa.add(i)), V::load(pb.add(i)));
            V::store(d.add(i), V::add(V::load(d.add(i)), product));
            i += V::LANES;
        }
    }
    scalar::add_product(&mut dst[body..], &a[body..], &b[body..]);
}

/// Where a bank of partials is: the slices of [`Partials`] as pointers.
#[derive(Clone, Copy)]
struct Bank {
    cos: *mut f32,
    sin: *mut f32,
    gain: *mut f32,
    cos_step: *const f32,
    sin_step: *const f32,
    gain_step: *const f32,
    left_gain: *const f32,
    right_gain: *const f32,
}

/// The bank of partials with each lane of a register one partial: the
/// registers of a group of [`PARTIAL_LANES`] partials are its running sums,
/// lane for lane, and every lane does what the definition does to its
/// partial, operation for operation. `x * c - y * s` is worked out as
/// `x * c + y * -s`, which is the same number.
#[inline(always)]
pub(super) unsafe fn add_partials<V: Vector<T = f32>>(
    left: &mut [f32],
    right: &mut [f32],
    partials: &mut Partials<'_>,
) {
    let count = partials.len();
    let body = count - count % PARTIAL_LANES;
    if body > 0 {
        let bank = Bank {
            cos: partials.cos.as_mut_ptr(),
            sin: partials.sin.as_mut_ptr(),
            gain: partials.gain.as_mut_ptr(),
            cos_step: partials.cos_step.as_ptr(),
            sin_step: partials.sin_step.as_ptr(),
            gain_step: partials.gain_step.as_ptr(),
            left_gain: partials.left_gain.as_ptr(),
            right_gain: partials.right_gain.as_ptr(),
        };
        // Not cleared: the first group writes every sum of a chunk before
        // anything reads it.
        let mut sums_left = MaybeUninit::<[f32; PARTIAL_LANES * PARTIAL_FRAMES]>::uninit();
        let mut sums_right = MaybeUninit::<[f32; PARTIAL_LANES * PARTIAL_FRAMES]>::uninit();
        let (sl, sr) = (
            sums_left.as_mut_ptr().cast::<f32>(),
            sums_right.as_mut_ptr().cast::<f32>(),
        );
        let mut start = 0;
        while start < left.len() {
            let frames = (left.len() - start).min(PARTIAL_FRAMES);
            // SAFETY: the groups are below `body`, inside every slice of
            // `partials`, which have the same length; the sums written and
            // read are the first `frames * PARTIAL_LANES` of the arrays,
            // all written by the first group before the reduction reads
            // them. The caller knows the processor has the instructions.
            unsafe {
                partial_group::<V, true>(bank, 0, frames, sl, sr);
                let mut group = PARTIAL_LANES;
                while group < body {
                    partial_group::<V, false>(bank, group, frames, sl, sr);
                    group += PARTIAL_LANES;
                }
                let regs = PARTIAL_LANES / V::LANES;
                let zero = V::splat(0.0);
                for t in 0..frames {
                    let (row_left, row_right) =
                        (sl.add(t * PARTIAL_LANES), sr.add(t * PARTIAL_LANES));
                    let mut sums_l = [zero; MAX_REGS];
                    let mut sums_r = [zero; MAX_REGS];
                    for r in 0..regs {
                        sums_l[r] = V::load(row_left.add(r * V::LANES));
                        sums_r[r] = V::load(row_right.add(r * V::LANES));
                    }
                    left[start + t] += reduce_registers::<V>(sums_l, regs);
                    right[start + t] += reduce_registers::<V>(sums_r, regs);
                }
            }
            start += frames;
        }
    }
    for i in body..count {
        scalar::add_partial_in_order(left, right, partials, i);
    }
}

/// `frames` frames of the group of [`PARTIAL_LANES`] partials from
/// `group` on, its terms written to the running sums at `sums_left` and
/// `sums_right` if it is the `FIRST` group and added to them if not.
///
/// # Safety
///
/// The group must be inside the bank, the sums must have room for
/// `frames * PARTIAL_LANES` elements (written already unless `FIRST`), and
/// the processor must have the instructions.
#[inline(always)]
unsafe fn partial_group<V: Vector<T = f32>, const FIRST: bool>(
    bank: Bank,
    group: usize,
    frames: usize,
    sums_left: *mut f32,
    sums_right: *mut f32,
) {
    let regs = PARTIAL_LANES / V::LANES;
    debug_assert!(regs <= MAX_REGS && regs * V::LANES == PARTIAL_LANES);
    // SAFETY: the caller's guarantees.
    unsafe {
        let zero = V::splat(0.0);
        let flip = V::splat(-0.0);
        let mut x = [zero; MAX_REGS];
        let mut y = [zero; MAX_REGS];
        let mut g = [zero; MAX_REGS];
        let mut c = [zero; MAX_REGS];
        let mut s = [zero; MAX_REGS];
        let mut minus_s = [zero; MAX_REGS];
        let mut step = [zero; MAX_REGS];
        let mut to_left = [zero; MAX_REGS];
        let mut to_right = [zero; MAX_REGS];
        for r in 0..regs {
            let at = group + r * V::LANES;
            x[r] = V::load(bank.cos.add(at));
            y[r] = V::load(bank.sin.add(at));
            g[r] = V::load(bank.gain.add(at));
            c[r] = V::load(bank.cos_step.add(at));
            s[r] = V::load(bank.sin_step.add(at));
            minus_s[r] = V::xor(s[r], flip);
            step[r] = V::load(bank.gain_step.add(at));
            to_left[r] = V::load(bank.left_gain.add(at));
            to_right[r] = V::load(bank.right_gain.add(at));
        }
        for t in 0..frames {
            let (row_left, row_right) = (
                sums_left.add(t * PARTIAL_LANES),
                sums_right.add(t * PARTIAL_LANES),
            );
            for r in 0..regs {
                g[r] = V::add(g[r], step[r]);
                let v = V::mul(g[r], y[r]);
                let (term_left, term_right) = (V::mul(v, to_left[r]), V::mul(v, to_right[r]));
                let (at_left, at_right) = (row_left.add(r * V::LANES), row_right.add(r * V::LANES));
                if FIRST {
                    V::store(at_left, term_left);
                    V::store(at_right, term_right);
                } else {
                    V::store(at_left, V::add(V::load(at_left), term_left));
                    V::store(at_right, V::add(V::load(at_right), term_right));
                }
                let next_x = V::add(V::mul(x[r], c[r]), V::mul(y[r], minus_s[r]));
                let next_y = V::add(V::mul(x[r], s[r]), V::mul(y[r], c[r]));
                x[r] = next_x;
                y[r] = next_y;
            }
        }
        for r in 0..regs {
            let at = group + r * V::LANES;
            V::store(bank.cos.add(at), x[r]);
            V::store(bank.sin.add(at), y[r]);
            V::store(bank.gain.add(at), g[r]);
        }
    }
}

#[inline(always)]
pub(super) unsafe fn peak<V: Vector>(src: &[V::T]) -> Peak<V::T> {
    let step = V::LANES * MAX_REGS;
    let body = src.len() - src.len() % step;
    let s = src.as_ptr();
    let mut maxima = [V::T::ZERO; MAX_LANES * MAX_REGS];
    let nan;
    // SAFETY: every element read is below `body`; `maxima` has room for
    // `MAX_REGS` registers.
    unsafe {
        let zero = V::splat(V::T::ZERO);
        let mut largest = [zero; MAX_REGS];
        let mut nans = zero;
        let mut i = 0;
        while i < body {
            for (r, largest) in largest.iter_mut().enumerate() {
                let x = V::load(s.add(i + r * V::LANES));
                *largest = V::max(*largest, V::abs(x));
                nans = V::or(nans, V::nan_mask(x));
            }
            i += step;
        }
        nan = V::any(nans);
        for (r, &largest) in largest.iter().enumerate() {
            V::store(maxima.as_mut_ptr().add(r * V::LANES), largest);
        }
    }
    // A maximum is the same in any order. With a NaN about, the lanes may
    // hold anything, but the answer is NaN anyway.
    let tail = scalar::peak(&src[body..]);
    let mut largest = tail.largest;
    for &value in &maxima[..MAX_REGS * V::LANES] {
        if value > largest {
            largest = value;
        }
    }
    Peak {
        largest,
        nan: nan || tail.nan,
    }
}

/// Adds the running sums kept in `acc`, `regs` registers, in the
/// definition's pairs: `s[j] + s[j + half]` is register `r` plus register
/// `r + regs / 2` while more than one register is left, then
/// [`Vector::reduce`] within the last.
#[inline(always)]
unsafe fn reduce_registers<V: Vector>(mut acc: [V::Reg; MAX_REGS], regs: usize) -> V::T {
    // SAFETY: the caller knows the processor has the instructions.
    unsafe {
        let mut regs = regs;
        while regs > 1 {
            regs /= 2;
            for r in 0..regs {
                acc[r] = V::add(acc[r], acc[r + regs]);
            }
        }
        V::reduce(acc[0])
    }
}

/// The sum of products in the order of [`scalar::dot`]: running sum `j`
/// is lane `j % LANES` of register `j / LANES`, and the elements after the
/// last whole chunk are added to the result one after another.
#[inline(always)]
pub(super) unsafe fn dot<V: Vector>(a: &[V::T], b: &[V::T]) -> V::T {
    let total = scalar::sum_lanes::<V::T>();
    let regs = total / V::LANES;
    debug_assert!(regs <= MAX_REGS && regs * V::LANES == total);
    let body = a.len() - a.len() % total;
    let (pa, pb) = (a.as_ptr(), b.as_ptr());
    // SAFETY: every element read is below `body`.
    let mut sum = unsafe {
        let zero = V::splat(V::T::ZERO);
        let mut acc = [zero; MAX_REGS];
        let mut i = 0;
        while i < body {
            for (r, acc) in acc.iter_mut().enumerate().take(regs) {
                let at = i + r * V::LANES;
                *acc = V::add(*acc, V::mul(V::load(pa.add(at)), V::load(pb.add(at))));
            }
            i += total;
        }
        reduce_registers::<V>(acc, regs)
    };
    for (&x, &y) in a[body..].iter().zip(&b[body..]) {
        sum += x * y;
    }
    sum
}

/// The sum of squares of `f32` elements, in `f64`, in the order of
/// [`scalar::sum_of_squares`].
#[inline(always)]
pub(super) unsafe fn sum_of_squares_widened<V: Widen>(src: &[f32]) -> f64 {
    let regs = SQUARE_LANES / V::LANES;
    debug_assert!(regs <= MAX_REGS && regs * V::LANES == SQUARE_LANES);
    let body = src.len() - src.len() % SQUARE_LANES;
    let s = src.as_ptr();
    // SAFETY: every element read is below `body`.
    let mut sum = unsafe {
        let zero = V::splat(0.0);
        let mut acc = [zero; MAX_REGS];
        let mut i = 0;
        while i < body {
            let x = V::load_widened(s.add(i));
            for (acc, &x) in acc.iter_mut().zip(&x).take(regs) {
                *acc = V::add(*acc, V::mul(x, x));
            }
            i += SQUARE_LANES;
        }
        reduce_registers::<V>(acc, regs)
    };
    for &x in &src[body..] {
        let x = f64::from(x);
        sum += x * x;
    }
    sum
}

#[inline(always)]
pub(super) unsafe fn interleave_stereo<V: Vector>(dst: &mut [V::T], left: &[V::T], right: &[V::T]) {
    let body = left.len() - left.len() % V::LANES;
    let (d, l, r) = (dst.as_mut_ptr(), left.as_ptr(), right.as_ptr());
    // SAFETY: frames below `body` are in `left` and `right`, and their
    // samples in `dst`, which is twice as long.
    unsafe {
        let mut i = 0;
        while i < body {
            let (low, high) = V::zip(V::load(l.add(i)), V::load(r.add(i)));
            V::store(d.add(2 * i), low);
            V::store(d.add(2 * i + V::LANES), high);
            i += V::LANES;
        }
    }
    scalar::interleave_stereo(&mut dst[2 * body..], &left[body..], &right[body..]);
}

#[inline(always)]
pub(super) unsafe fn deinterleave_stereo<V: Vector>(
    left: &mut [V::T],
    right: &mut [V::T],
    src: &[V::T],
) {
    let body = left.len() - left.len() % V::LANES;
    let (l, r, s) = (left.as_mut_ptr(), right.as_mut_ptr(), src.as_ptr());
    // SAFETY: as in `interleave_stereo`.
    unsafe {
        let mut i = 0;
        while i < body {
            let (even, odd) = V::unzip(V::load(s.add(2 * i)), V::load(s.add(2 * i + V::LANES)));
            V::store(l.add(i), even);
            V::store(r.add(i), odd);
            i += V::LANES;
        }
    }
    scalar::deinterleave_stereo(&mut left[body..], &mut right[body..], &src[2 * body..]);
}

/// `sum += a * b` for interleaved complex numbers. With `x = a` and
/// `y = b`, the real lanes get `xr * yr + -(xi * yi)`, which is
/// `xr * yr - xi * yi` exactly, and the imaginary lanes
/// `xr * yi + xi * yr`, in the definition's order.
#[inline(always)]
pub(super) unsafe fn complex_multiply_add<V: Vector>(sum: &mut [V::T], a: &[V::T], b: &[V::T]) {
    // `LANES` is even and so is the length: no number is split.
    let body = sum.len() - sum.len() % V::LANES;
    let (out, pa, pb) = (sum.as_mut_ptr(), a.as_ptr(), b.as_ptr());
    let mut signs = [V::T::ZERO; MAX_LANES];
    for sign in signs.iter_mut().step_by(2) {
        *sign = -V::T::ZERO;
    }
    // SAFETY: every element read and written is below `body`, in all three
    // slices, which have the same length; `signs` holds `LANES` elements.
    unsafe {
        let flip_real = V::load(signs.as_ptr());
        let mut i = 0;
        while i < body {
            let (x, y) = (V::load(pa.add(i)), V::load(pb.add(i)));
            let straight = V::mul(V::dup_even(x), y);
            let crossed = V::mul(V::dup_odd(x), V::swap_pairs(y));
            let product = V::add(straight, V::xor(crossed, flip_real));
            V::store(out.add(i), V::add(V::load(out.add(i)), product));
            i += V::LANES;
        }
    }
    scalar::complex_multiply_add(&mut sum[body..], &a[body..], &b[body..]);
}

/// Defines, for one backend, the functions the dispatcher calls for the
/// floating-point operations: each one compiled for `$feature` and running
/// the generic kernel on the registers `$f32` and `$f64`.
macro_rules! entry_points {
    ($feature:literal, $f32:ty, $f64:ty) => {
        use $crate::vector::Peak;
        use $crate::vector::generic;

        entry_points!(@pair $feature, scale, scale_f32, scale_f64, $f32, $f64,
            (dst: slice_mut, gain: value) -> unit);
        entry_points!(@pair $feature, copy_scaled, copy_scaled_f32, copy_scaled_f64, $f32, $f64,
            (dst: slice_mut, src: slice, gain: value) -> unit);
        entry_points!(@pair $feature, add, add_f32, add_f64, $f32, $f64,
            (dst: slice_mut, src: slice) -> unit);
        entry_points!(@pair $feature, add_scaled, add_scaled_f32, add_scaled_f64, $f32, $f64,
            (dst: slice_mut, src: slice, gain: value) -> unit);
        entry_points!(@pair $feature, multiply, multiply_f32, multiply_f64, $f32, $f64,
            (dst: slice_mut, src: slice) -> unit);
        entry_points!(@pair $feature, dot, dot_f32, dot_f64, $f32, $f64,
            (a: slice, b: slice) -> value);
        entry_points!(@pair $feature, interleave_stereo, interleave_stereo_f32,
            interleave_stereo_f64, $f32, $f64, (dst: slice_mut, left: slice, right: slice) -> unit);
        entry_points!(@pair $feature, deinterleave_stereo, deinterleave_stereo_f32,
            deinterleave_stereo_f64, $f32, $f64,
            (left: slice_mut, right: slice_mut, src: slice) -> unit);
        entry_points!(@pair $feature, complex_multiply_add, complex_multiply_add_f32,
            complex_multiply_add_f64, $f32, $f64, (sum: slice_mut, a: slice, b: slice) -> unit);
        entry_points!(@pair $feature, add_product, add_product_f32, add_product_f64, $f32, $f64,
            (dst: slice_mut, a: slice, b: slice) -> unit);

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn add_scaled_ramp_f32(
            dst: &mut [f32],
            src: &[f32],
            start: f32,
            step: f32,
        ) {
            // SAFETY: the caller knows the processor has the feature.
            // Indices are exact in `f32` up to 2^24.
            unsafe { generic::add_scaled_ramp::<$f32>(dst, src, start, step, 1 << 24) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn add_scaled_ramp_f64(
            dst: &mut [f64],
            src: &[f64],
            start: f64,
            step: f64,
        ) {
            // SAFETY: as above; every index is exact in `f64`.
            unsafe { generic::add_scaled_ramp::<$f64>(dst, src, start, step, usize::MAX) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn add_partials(
            left: &mut [f32],
            right: &mut [f32],
            partials: &mut $crate::vector::Partials<'_>,
        ) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::add_partials::<$f32>(left, right, partials) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn scale_ramp_f32(dst: &mut [f32], start: f32, step: f32) {
            // SAFETY: the caller knows the processor has the feature.
            // Indices are exact in `f32` up to 2^24.
            unsafe { generic::scale_ramp::<$f32>(dst, start, step, 1 << 24) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn scale_ramp_f64(dst: &mut [f64], start: f64, step: f64) {
            // SAFETY: as above. Every index a slice can hold is exact in
            // `f64`, up to 2^53.
            unsafe { generic::scale_ramp::<$f64>(dst, start, step, usize::MAX) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn peak_f32(src: &[f32]) -> Peak<f32> {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::peak::<$f32>(src) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn peak_f64(src: &[f64]) -> Peak<f64> {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::peak::<$f64>(src) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn sum_of_squares_f32(src: &[f32]) -> f64 {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::sum_of_squares_widened::<$f64>(src) }
        }

        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn sum_of_squares_f64(src: &[f64]) -> f64 {
            // SAFETY: the caller knows the processor has the feature. The
            // definition's sum of squares of `f64` is its dot product with
            // itself: the same eight running sums of `x * x`.
            unsafe { generic::dot::<$f64>(src, src) }
        }
    };
    (@pair $feature:literal, $kernel:ident, $name32:ident, $name64:ident, $f32:ty, $f64:ty,
        ($($arg:ident : $kind:tt),*) -> $ret:tt) => {
        entry_points!(@one $feature, $kernel, $name32, $f32, f32, ($($arg: $kind),*) -> $ret);
        entry_points!(@one $feature, $kernel, $name64, $f64, f64, ($($arg: $kind),*) -> $ret);
    };
    (@one $feature:literal, $kernel:ident, $name:ident, $reg:ty, $t:ty,
        ($($arg:ident : $kind:tt),*) -> unit) => {
        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn $name($($arg: entry_points!(@ty $kind, $t)),*) {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::$kernel::<$reg>($($arg),*) }
        }
    };
    (@one $feature:literal, $kernel:ident, $name:ident, $reg:ty, $t:ty,
        ($($arg:ident : $kind:tt),*) -> $ret:tt) => {
        #[target_feature(enable = $feature)]
        pub(in $crate::vector) unsafe fn $name($($arg: entry_points!(@ty $kind, $t)),*)
            -> entry_points!(@ty $ret, $t)
        {
            // SAFETY: the caller knows the processor has the feature.
            unsafe { generic::$kernel::<$reg>($($arg),*) }
        }
    };
    (@ty slice, $t:ty) => { &[$t] };
    (@ty slice_mut, $t:ty) => { &mut [$t] };
    (@ty value, $t:ty) => { $t };
}

pub(super) use entry_points;
