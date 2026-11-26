//! The functions the ready-made tables are filled from, in `f64`, with
//! additions, multiplications and divisions only, so that a table holds the
//! same bits on every system. The system's `sin`, `exp` and `tanh` differ in
//! their last bits from one to another.
//!
//! The methods are the textbook ones: Taylor series on a reduced argument,
//! with Cody and Waite's two-part reduction by `ln 2` for the exponential
//! (W. J. Cody and W. Waite, *Software Manual for the Elementary
//! Functions*, 1980).

use core::cmp::Ordering;
use core::f64::consts::{FRAC_1_SQRT_2, FRAC_PI_2, LN_2};

/// `sin(2 pi k / n)`, for `n` from 1 to 2^61. The symmetries of the sine
/// hold exactly, as the argument is reduced in integers.
pub(super) fn sine_of_turn(k: usize, n: usize) -> f64 {
    let n = n as u64;
    let quarters = 4 * (k as u64 % n);
    let (quadrant, rest) = (quarters / n, quarters % n);
    // The angle within the quadrant is `rest / n` of a right angle; past
    // half of one, work from the other end.
    let angle = |part: u64| FRAC_PI_2 * (part as f64 / n as f64);
    let (sin, cos) = match (2 * rest).cmp(&n) {
        Ordering::Less => {
            let a = angle(rest);
            (sin_series(a), cos_series(a))
        }
        // The one angle that is its own complement: the two series round
        // differently there.
        Ordering::Equal => (FRAC_1_SQRT_2, FRAC_1_SQRT_2),
        Ordering::Greater => {
            let a = angle(n - rest);
            (cos_series(a), sin_series(a))
        }
    };
    match quadrant {
        0 => sin,
        1 => cos,
        2 => 0.0 - sin,
        _ => 0.0 - cos,
    }
}

/// The sine of `|a| <= pi / 4`, to the term in `a^17`.
fn sin_series(a: f64) -> f64 {
    let square = a * a;
    let mut tail = 1.0;
    for k in (1..=8u32).rev() {
        tail = 1.0 - square / f64::from(2 * k * (2 * k + 1)) * tail;
    }
    a * tail
}

/// The cosine of `|a| <= pi / 4`, to the term in `a^16`.
fn cos_series(a: f64) -> f64 {
    let square = a * a;
    let mut tail = 1.0;
    for k in (1..=8u32).rev() {
        tail = 1.0 - square / f64::from((2 * k - 1) * 2 * k) * tail;
    }
    tail
}

/// `e^r - 1` for `|r|` up to about `ln 2 / 2`, to the term in `r^14`.
fn exp_minus_one_series(r: f64) -> f64 {
    let mut tail = 1.0;
    for k in (2..=14u32).rev() {
        tail = 1.0 + r / f64::from(k) * tail;
    }
    r * tail
}

/// `2^exponent`, for exponents of normal numbers.
fn power_of_two(exponent: i32) -> f64 {
    f64::from_bits(((1023 + exponent) as u64) << 52)
}

/// `e^x`.
pub(super) fn exp(x: f64) -> f64 {
    // `ln 2` in two parts: the first has zeros in its low bits, so that its
    // product with a small integer is exact.
    const LN_2_HIGH: f64 = f64::from_bits(0x3FE6_2E42_FEE0_0000);
    const LN_2_LOW: f64 = f64::from_bits(0x3DEA_39EF_3579_3C76);
    if x.is_nan() {
        return x;
    }
    if x > 709.8 {
        return f64::INFINITY;
    }
    if x < -745.2 {
        return 0.0;
    }
    let k = (x / LN_2).round();
    let r = (x - k * LN_2_HIGH) - k * LN_2_LOW;
    let k = k as i32;
    // In two steps, so that neither factor leaves the normal numbers.
    let half = k / 2;
    (1.0 + exp_minus_one_series(r)) * power_of_two(half) * power_of_two(k - half)
}

/// The hyperbolic tangent.
pub(super) fn tanh(x: f64) -> f64 {
    let a = x.abs();
    let magnitude = if a > 22.0 {
        // `1 - tanh(22)` is below half the spacing of `f64` under 1.
        1.0
    } else {
        let twice = 2.0 * a;
        // `e^(2a) - 1`, without the subtraction where it would cancel.
        let m = if twice < LN_2 / 2.0 {
            exp_minus_one_series(twice)
        } else {
            exp(twice) - 1.0
        };
        m / (m + 2.0)
    };
    magnitude.copysign(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the system's functions, which are within an ulp or so.
    #[test]
    #[cfg_attr(miri, ignore = "Miri blurs the system's functions on purpose")]
    fn agree_with_the_system() {
        let steps = 100_003;
        for k in 0..=steps {
            let turn = k as f64 / steps as f64;
            let expected = (core::f64::consts::TAU * turn).sin();
            let got = sine_of_turn(k, steps);
            // The system's argument `2 pi k / n` is itself rounded.
            assert!((got - expected).abs() < 2e-15, "sin of {turn}: {got}");

            let x = -30.0 + 60.0 * turn;
            let expected = x.exp();
            assert!(
                (exp(x) - expected).abs() <= 1e-15 * expected,
                "exp {x}: {}",
                exp(x)
            );
            let expected = x.tanh();
            assert!((tanh(x) - expected).abs() < 1e-15, "tanh {x}: {}", tanh(x));
        }
    }

    #[test]
    fn exact_where_the_functions_are() {
        for n in [4, 8, 12, 4096] {
            assert_eq!(sine_of_turn(0, n).to_bits(), 0.0f64.to_bits());
            assert_eq!(sine_of_turn(n / 4, n), 1.0);
            assert_eq!(sine_of_turn(n / 2, n).to_bits(), 0.0f64.to_bits());
            assert_eq!(sine_of_turn(3 * n / 4, n), -1.0);
            for k in 0..n {
                assert_eq!(sine_of_turn(k, n), -sine_of_turn(n - k, n));
            }
        }
        assert_eq!(exp(0.0), 1.0);
        assert_eq!(tanh(0.0).to_bits(), 0.0f64.to_bits());
        assert_eq!(tanh(-0.0).to_bits(), (-0.0f64).to_bits());
        assert_eq!(tanh(-0.75), -tanh(0.75));
        assert_eq!(tanh(100.0), 1.0);
    }

    #[test]
    #[cfg_attr(miri, ignore = "Miri blurs the system's functions on purpose")]
    fn exp_covers_the_whole_range() {
        for x in [-745.0, -720.0, -708.0, 708.0, 709.7] {
            let expected = f64::exp(x);
            assert!(
                (exp(x) - expected).abs() <= 1e-13 * expected + f64::from_bits(2),
                "exp {x}: {} against {expected}",
                exp(x)
            );
        }
        assert_eq!(exp(1000.0), f64::INFINITY);
        assert_eq!(exp(-1000.0), 0.0);
        assert!(exp(f64::NAN).is_nan());
    }
}
