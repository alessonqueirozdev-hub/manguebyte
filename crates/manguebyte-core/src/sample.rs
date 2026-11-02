use core::fmt::Debug;
use core::ops::{Add, AddAssign, Div, Mul, MulAssign, Neg, Sub, SubAssign};

/// A floating-point audio sample.
///
/// Implemented for `f32` and `f64`. Processors are generic over this trait so
/// the same code runs in single or double precision.
pub trait Sample:
    Copy
    + Debug
    + Default
    + PartialOrd
    + Send
    + Sync
    + 'static
    + Add<Output = Self>
    + Sub<Output = Self>
    + Mul<Output = Self>
    + Div<Output = Self>
    + Neg<Output = Self>
    + AddAssign
    + SubAssign
    + MulAssign
{
    /// Silence.
    const ZERO: Self;
    /// Full scale.
    const ONE: Self;

    /// Converts from `f64`, rounding to the nearest representable value.
    fn from_f64(value: f64) -> Self;
    /// Converts to `f64`.
    fn to_f64(self) -> f64;
    /// Converts from `f32`.
    fn from_f32(value: f32) -> Self;
    /// Converts to `f32`, rounding to the nearest representable value.
    fn to_f32(self) -> f32;
    /// Absolute value.
    fn abs(self) -> Self;
    /// Square root.
    fn sqrt(self) -> Self;
}

impl Sample for f32 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;

    #[inline]
    fn from_f64(value: f64) -> Self {
        value as f32
    }
    #[inline]
    fn to_f64(self) -> f64 {
        f64::from(self)
    }
    #[inline]
    fn from_f32(value: f32) -> Self {
        value
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self
    }
    #[inline]
    fn abs(self) -> Self {
        f32::abs(self)
    }
    #[inline]
    fn sqrt(self) -> Self {
        f32::sqrt(self)
    }
}

impl Sample for f64 {
    const ZERO: Self = 0.0;
    const ONE: Self = 1.0;

    #[inline]
    fn from_f64(value: f64) -> Self {
        value
    }
    #[inline]
    fn to_f64(self) -> f64 {
        self
    }
    #[inline]
    fn from_f32(value: f32) -> Self {
        f64::from(value)
    }
    #[inline]
    fn to_f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn abs(self) -> Self {
        f64::abs(self)
    }
    #[inline]
    fn sqrt(self) -> Self {
        f64::sqrt(self)
    }
}
