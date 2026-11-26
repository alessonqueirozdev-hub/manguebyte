//! Lookup tables: a function worked out once, off the audio thread, and
//! read back with interpolation (ADR 0029).
//!
//! - [`LookupTable`]: values at the indices `0..points`, read at any
//!   position between them.
//! - [`LookupTransform`]: a function over a range of inputs, which maps an
//!   input to a position in such a table. [`LookupTransform::tanh`] and
//!   [`LookupTransform::exp`] are ready made.
//! - [`SineTable`]: one period of the sine, read at any phase.
//!
//! ```
//! use manguebyte_core::lookup::{LookupTransform, SineTable};
//!
//! // In `prepare`:
//! let shaper = LookupTransform::<f32>::new(-4.0, 4.0, 1_024, |x| x / (1.0 + x.abs()))?;
//! let sine = SineTable::<f32>::new(2_048)?;
//! // On the audio thread:
//! assert!((shaper.cubic(1.0) - 0.5).abs() < 1e-5);
//! assert_eq!(shaper.linear(100.0), 0.8, "held at the end of the range");
//! assert!((sine.linear(0.25) - 1.0).abs() < 1e-6);
//! # Ok::<(), manguebyte_core::lookup::TableError>(())
//! ```
//!
//! # Interpolation
//!
//! `linear` reads the straight line between the two values around a
//! position. `cubic` reads the cubic through four of them (a Lagrange
//! polynomial): the two on each side, or in the first and last intervals of
//! a table that does not repeat, the four at that end. Neither reads past
//! the table. Both give a value of the table, exactly, at its position (a
//! negative zero may come back positive). The cubic follows any polynomial
//! up to the third degree, but for rounding; its slope jumps from one
//! interval to the next.
//!
//! # How far from the function
//!
//! For values `h` apart, and before rounding, the difference from a smooth
//! function is at most
//!
//! | Reading | Bound |
//! |---|---|
//! | `linear` | `h^2 / 8 * max abs(f'')` |
//! | `cubic`, away from the ends | `3 h^4 / 128 * max abs(f'''')` |
//! | `cubic`, first and last intervals | `h^4 / 24 * max abs(f'''')` |
//!
//! so doubling the points of a table divides the error of `linear` by 4 and
//! of `cubic` by 16, until rounding in the sample type is what is left: of the
//! values and of the arithmetic between them, as an input is placed in the
//! table in `f64`. The ready-made tables state what the tests measure of
//! them.
//!
//! # Real time
//!
//! Building a table allocates and calls the function once a point: do it in
//! `prepare` or on another thread. Reading allocates nothing, takes the
//! same few operations for every input, and gives a finite value for every
//! input: one outside the range reads the value at the nearer end, and a
//! NaN reads the value at the start of the range (for [`SineTable`], an
//! infinite or NaN phase reads phase 0). No reading panics.
//!
//! # The same bits everywhere
//!
//! Reading uses additions, subtractions, multiplications, divisions and
//! conversions only, which round the same way everywhere, so a table gives
//! the same bits on every processor in the usual floating-point mode, which
//! keeps subnormals. What it holds is what
//! the function it was built with returned: [`SineTable`],
//! [`LookupTransform::tanh`] and [`LookupTransform::exp`] use the crate's
//! own functions and are the same on every system, while a function of your
//! own that calls the system's `sin`, `exp` or `powf` may differ in the last
//! bits from one to another.

use core::fmt;

use crate::Sample;

mod reference;

/// Why a table could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableError {
    /// Fewer than [`LookupTable::MIN_POINTS`] points, or more than
    /// [`LookupTable::MAX_POINTS`].
    Points {
        /// The number asked for.
        points: usize,
    },
    /// The range does not start below its end, an end is not finite, or the
    /// range is too wide or too narrow for `f64` to place an input in it.
    Range,
    /// A value is not finite, or is within a factor of 32 of the largest
    /// number of the sample type, too large to interpolate.
    Value {
        /// The index of the first such value.
        index: usize,
    },
}

impl fmt::Display for TableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            TableError::Points { points } => write!(
                f,
                "a lookup table takes {} to {} points, not {points}",
                LookupTable::<f32>::MIN_POINTS,
                LookupTable::<f32>::MAX_POINTS
            ),
            TableError::Range => f.write_str("the range of a lookup table is not usable"),
            TableError::Value { index } => write!(
                f,
                "the value at index {index} of a lookup table is not finite or too large"
            ),
        }
    }
}

impl std::error::Error for TableError {}

fn check_points(points: usize) -> Result<(), TableError> {
    if (LookupTable::<f32>::MIN_POINTS..=LookupTable::<f32>::MAX_POINTS).contains(&points) {
        Ok(())
    } else {
        Err(TableError::Points { points })
    }
}

// An index of a table fits in an `i32`, whose conversions to and from `f64`
// are single instructions on x86, where those of `usize` are not.

/// The whole part of a position from 0 to [`LookupTable::MAX_POINTS`].
#[inline(always)]
fn whole(position: f64) -> usize {
    position as i32 as usize
}

/// An index of a table as a number.
#[inline(always)]
fn number(index: usize) -> f64 {
    f64::from(index as i32)
}

/// The straight line from `a` at 0 to `b` at 1, at `t`.
#[inline(always)]
fn line<S: Sample>(a: S, b: S, t: S) -> S {
    a + (b - a) * t
}

/// The cubic through `y` at -1, 0, 1 and 2, at `u`. At each of those the
/// weights are exactly one 1 and three 0.
#[inline(always)]
fn cubic_through<S: Sample>(y: [S; 4], u: S) -> S {
    let one = S::ONE;
    let two = S::from_f64(2.0);
    let half = S::from_f64(0.5);
    let six = S::from_f64(6.0);
    let (after, before, last) = (u + one, u - one, u - two);
    let w0 = -u * before * last / six;
    let w1 = after * before * last * half;
    let w2 = -after * u * last * half;
    let w3 = after * u * before / six;
    w0 * y[0] + w1 * y[1] + w2 * y[2] + w3 * y[3]
}

/// Values at the indices `0..points`, read at any position between them.
/// See the [module documentation](self) for the interpolation, its error
/// and what is safe on the audio thread.
#[derive(Debug, Clone)]
pub struct LookupTable<S> {
    /// The points, and the last one again, so that the line from the last
    /// point has somewhere to go.
    values: Box<[S]>,
}

impl<S: Sample> LookupTable<S> {
    /// The fewest points a table has: what a cubic needs.
    pub const MIN_POINTS: usize = 4;
    /// The most points a table has, 2^24: every index is exact in `f32`.
    pub const MAX_POINTS: usize = 1 << 24;

    /// A table of `points` values, `value(i)` at index `i`. Allocates.
    ///
    /// # Errors
    ///
    /// [`TableError::Points`] or [`TableError::Value`].
    pub fn new(points: usize, mut value: impl FnMut(usize) -> S) -> Result<Self, TableError> {
        check_points(points)?;
        let mut values = Vec::with_capacity(points + 1);
        for index in 0..points {
            let v = value(index);
            // Thirty-two times a usable value is still finite, so no step of
            // an interpolation overflows.
            if !(v * S::from_f64(32.0)).to_f64().is_finite() {
                return Err(TableError::Value { index });
            }
            values.push(v);
        }
        values.push(values[points - 1]);
        Ok(LookupTable {
            values: values.into_boxed_slice(),
        })
    }

    /// A table of these values. Allocates.
    ///
    /// # Errors
    ///
    /// [`TableError::Points`] or [`TableError::Value`].
    pub fn from_values(values: &[S]) -> Result<Self, TableError> {
        Self::new(values.len(), |index| values[index])
    }

    /// The number of points.
    pub fn points(&self) -> usize {
        self.values.len() - 1
    }

    /// The values, one a point.
    pub fn values(&self) -> &[S] {
        &self.values[..self.points()]
    }

    /// The position held between the first index and the last; NaN is held
    /// at the first.
    #[inline(always)]
    fn held(&self, position: f64) -> f64 {
        let last = (self.points() - 1) as f64;
        if position > 0.0 {
            if position < last { position } else { last }
        } else {
            0.0
        }
    }

    #[inline(always)]
    fn linear_at(&self, position: f64) -> S {
        let position = self.held(position);
        let index = whole(position);
        let fraction = S::from_f64(position - number(index));
        line(self.values[index], self.values[index + 1], fraction)
    }

    #[inline(always)]
    fn cubic_at(&self, position: f64) -> S {
        let position = self.held(position);
        // The first of the four points: one back from the position, but for
        // the first interval and the last.
        let first = whole(position).saturating_sub(1).min(self.points() - 4);
        let u = S::from_f64(position - number(first + 1));
        let y = &self.values[first..first + 4];
        cubic_through([y[0], y[1], y[2], y[3]], u)
    }

    /// The straight line between the values around `position`, an index with
    /// a fraction. A position outside the table reads the value at its
    /// nearer end. Real-time safe.
    #[inline]
    pub fn linear(&self, position: S) -> S {
        self.linear_at(position.to_f64())
    }

    /// The cubic through four values around `position`, as the
    /// [module documentation](self) describes. A position outside the table
    /// reads the value at its nearer end. Real-time safe.
    #[inline]
    pub fn cubic(&self, position: S) -> S {
        self.cubic_at(position.to_f64())
    }
}

/// A function over a range of inputs, as a [`LookupTable`] of its values at
/// evenly spaced points and the mapping from an input to a position in it.
/// See the [module documentation](self) for the error and what is safe on
/// the audio thread.
#[derive(Debug, Clone)]
pub struct LookupTransform<S> {
    table: LookupTable<S>,
    start: S,
    end: S,
    /// Positions per unit of input.
    scale: f64,
}

impl<S: Sample> LookupTransform<S> {
    /// The inputs of [`tanh`](Self::tanh) run from minus this to this.
    pub const TANH_LIMIT: f64 = 12.0;

    /// A table of `function` at `points` inputs evenly spaced from `start`
    /// to `end`, both included. The function is evaluated in `f64` and its
    /// values rounded to `S`. Allocates.
    ///
    /// # Errors
    ///
    /// See [`TableError`].
    pub fn new(
        start: S,
        end: S,
        points: usize,
        mut function: impl FnMut(f64) -> f64,
    ) -> Result<Self, TableError> {
        check_points(points)?;
        let (from, to) = (start.to_f64(), end.to_f64());
        if !(from.is_finite() && to.is_finite() && from < to) {
            return Err(TableError::Range);
        }
        let last = points - 1;
        // Zero for a range too wide for `f64` to hold, infinite for one too
        // narrow to divide by.
        let scale = last as f64 / (to - from);
        if !(scale.is_finite() && scale > 0.0) {
            return Err(TableError::Range);
        }
        let table = LookupTable::new(points, |index| {
            let input = if index == last {
                to
            } else {
                from + (to - from) * (index as f64 / last as f64)
            };
            S::from_f64(function(input))
        })?;
        Ok(LookupTransform {
            table,
            start,
            end,
            scale,
        })
    }

    /// The hyperbolic tangent from `-`[`TANH_LIMIT`](Self::TANH_LIMIT) to
    /// `TANH_LIMIT`, and its value there, within `8e-11` of 1 and -1,
    /// beyond. Allocates.
    ///
    /// The largest difference from `tanh`, over every input, that the tests
    /// measure with 4 096 points:
    ///
    /// | | `f32` | `f64` |
    /// |---|---|---|
    /// | `linear` | below `3.4e-6` | below `3.4e-6` |
    /// | `cubic` | below `2.5e-7` | below `1.2e-10` |
    ///
    /// # Errors
    ///
    /// [`TableError::Points`].
    pub fn tanh(points: usize) -> Result<Self, TableError> {
        let limit = S::from_f64(Self::TANH_LIMIT);
        Self::new(-limit, limit, points, reference::tanh)
    }

    /// The exponential `e^x` from `start` to `end`, held at the ends
    /// outside. Allocates.
    ///
    /// The error is a proportion of the value: with the points `h` apart, `h`
    /// small against 1, `linear` is within about `h^2 / 8` of `e^x`,
    /// relatively, and `cubic` within about `h^4 / 24`, plus rounding. With
    /// 1 024 points from -1 to 1 the largest difference the tests measure,
    /// relative to `e^x`:
    ///
    /// | | `f32` | `f64` |
    /// |---|---|---|
    /// | `linear` | below `6e-7` | below `4.8e-7` |
    /// | `cubic` | below `3e-7` | below `6.2e-13` |
    ///
    /// For `2^x`, give the input times `ln 2`.
    ///
    /// # Errors
    ///
    /// See [`TableError`]; a range whose exponential does not fit the sample
    /// type is a [`TableError::Value`].
    pub fn exp(start: S, end: S, points: usize) -> Result<Self, TableError> {
        Self::new(start, end, points, reference::exp)
    }

    /// The first input of the range.
    pub fn start(&self) -> S {
        self.start
    }

    /// The last input of the range.
    pub fn end(&self) -> S {
        self.end
    }

    /// The table of the function's values.
    pub fn table(&self) -> &LookupTable<S> {
        &self.table
    }

    /// Where `input` falls in the table: `0` at the start of the range, the
    /// last index at its end. Outside the range it is outside the indices,
    /// which the table's readings hold to its ends. Real-time safe.
    #[inline]
    pub fn position(&self, input: S) -> S {
        S::from_f64(self.place(input))
    }

    /// [`position`](Self::position) in `f64`, which the readings use so that
    /// an `f32` input far from the start of the range loses no digits.
    #[inline(always)]
    fn place(&self, input: S) -> f64 {
        (input.to_f64() - self.start.to_f64()) * self.scale
    }

    /// The function at `input`, on the straight line between two points.
    /// Real-time safe.
    #[inline]
    pub fn linear(&self, input: S) -> S {
        self.table.linear_at(self.place(input))
    }

    /// The function at `input`, on the cubic through four points. Real-time
    /// safe.
    #[inline]
    pub fn cubic(&self, input: S) -> S {
        self.table.cubic_at(self.place(input))
    }

    /// Replaces each sample by [`linear`](Self::linear) of it: a waveshaper
    /// on a block. Real-time safe.
    pub fn linear_in_place(&self, samples: &mut [S]) {
        for sample in samples {
            *sample = self.linear(*sample);
        }
    }

    /// Replaces each sample by [`cubic`](Self::cubic) of it. Real-time safe.
    pub fn cubic_in_place(&self, samples: &mut [S]) {
        for sample in samples {
            *sample = self.cubic(*sample);
        }
    }
}

/// One period of the sine in `points` values, read at a phase in turns: `0`
/// and `1` are the start of a period, `0.25` its crest. Any finite phase
/// reads the period it falls in, and the cosine of a phase is the sine a
/// quarter of a turn later.
///
/// The largest difference from the sine that the tests measure with 2 048
/// points, for phases from -2 to 2 turns:
///
/// | | `f32` | `f64` |
/// |---|---|---|
/// | [`linear`](Self::linear) | below `1.3e-6` | below `1.2e-6` |
/// | [`cubic`](Self::cubic) | below `2.1e-7` | below `2.1e-12` |
///
/// The table finds the place in the period in `f64`, so a phase far from
/// zero costs no digits there; but an `f32` that far from zero has few
/// digits left for the fraction of a turn to begin with, so keep a running
/// phase within a turn of zero.
#[derive(Debug, Clone)]
pub struct SineTable<S> {
    /// The sine at the indices -1 to `points + 1`, repeating, so that no
    /// reading wraps.
    values: Box<[S]>,
    points: usize,
}

impl<S: Sample> SineTable<S> {
    /// A table of `points` values a period, from
    /// [`LookupTable::MIN_POINTS`] to [`LookupTable::MAX_POINTS`].
    /// Allocates.
    ///
    /// # Errors
    ///
    /// [`TableError::Points`].
    pub fn new(points: usize) -> Result<Self, TableError> {
        check_points(points)?;
        let values = (0..points + 3)
            .map(|j| S::from_f64(reference::sine_of_turn(j + points - 1, points)))
            .collect();
        Ok(SineTable { values, points })
    }

    /// The number of points a period.
    pub fn points(&self) -> usize {
        self.points
    }

    /// The point before a phase, as an index of the period, and how far past
    /// it the phase is, from 0 to 1.
    #[inline(always)]
    fn place(&self, turns: S) -> (usize, S) {
        let turns = turns.to_f64();
        // The part towards zero, exact; `f64::floor` would be a call on the
        // processors that have no instruction for it.
        let mut part = turns - (turns as i64) as f64;
        if part < 0.0 {
            part += 1.0;
        }
        // An infinite or NaN phase, or one too large to have a fraction. A
        // phase just under a whole turn can round up to 1, which is phase 0.
        // Adding zero makes a negative zero positive.
        part = if (0.0..1.0).contains(&part) {
            part + 0.0
        } else {
            0.0
        };
        let position = part * self.points as f64;
        // The last point of a period can be reached by rounding alone.
        let index = whole(position).min(self.points - 1);
        (index, S::from_f64(position - number(index)))
    }

    /// The sine of `turns` whole periods, on the straight line between two
    /// points. Real-time safe.
    #[inline]
    pub fn linear(&self, turns: S) -> S {
        let (index, fraction) = self.place(turns);
        line(self.values[index + 1], self.values[index + 2], fraction)
    }

    /// The sine of `turns` whole periods, on the cubic through the two
    /// points on each side. Real-time safe.
    #[inline]
    pub fn cubic(&self, turns: S) -> S {
        let (index, fraction) = self.place(turns);
        let y = &self.values[index..index + 4];
        cubic_through([y[0], y[1], y[2], y[3]], fraction)
    }
}
