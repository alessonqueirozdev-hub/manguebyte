//! Sine and cosine that give the same bits on every platform.
//!
//! `f32::sin` and `f64::sin` call the platform's C library, and those
//! libraries round differently in the last bit. A rotation or an arc
//! computed with them would then record different numbers on Windows, Linux
//! and macOS, and [`Scene::dump`](crate::Scene::dump) would stop being
//! exact across them. These functions use only integer arithmetic and
//! floating-point addition, subtraction, multiplication and division, which
//! IEEE 754 defines exactly, so their results are the same everywhere.
//!
//! An angle given as an `f32` is reduced to a quarter turn exactly, with
//! enough bits of `2/pi` for the largest `f32` (the method of Payne and
//! Hanek), so `sin_cos(1e18)` is as accurate as `sin_cos(1.0)`.

use std::f64::consts::FRAC_PI_2;

/// The first 384 bits of `2/pi` after the binary point, most significant
/// first. Computed with integer arithmetic from Machin's formula for `pi`;
/// the reduction reads at most the first 295.
const TWO_OVER_PI: [u64; 6] = [
    0xA2F9_836E_4E44_1529,
    0xFC27_57D1_F534_DDC0,
    0xDB62_9599_3C43_9041,
    0xFE51_63AB_DEBB_C561,
    0xB724_6E3A_424D_D2E0,
    0x0649_2EEA_09D1_921C,
];

/// `pi/2` in three parts whose sum is `pi/2` to 119 bits; the first two
/// have 33 significant bits, so a small whole number times either is exact
/// (the split of Cody and Waite).
const FRAC_PI_2_PARTS: [f64; 3] = [
    1.570_796_326_734_125_6,
    6.077_100_506_303_966e-11,
    2.022_266_248_795_950_6e-21,
];

/// Sine and cosine of `r`, for `|r| <= pi/4`, from their Taylor series.
/// The first omitted terms are below 1e-17 there.
fn kernel(r: f64) -> (f64, f64) {
    let r2 = r * r;
    // Nested so that each term is computed from the last.
    let sin = r
        * (1.0
            - r2 / 6.0
                * (1.0
                    - r2 / 20.0
                        * (1.0
                            - r2 / 42.0
                                * (1.0
                                    - r2 / 72.0
                                        * (1.0
                                            - r2 / 110.0
                                                * (1.0 - r2 / 156.0 * (1.0 - r2 / 210.0)))))));
    let cos = 1.0
        - r2 / 2.0
            * (1.0
                - r2 / 12.0
                    * (1.0
                        - r2 / 30.0
                            * (1.0
                                - r2 / 56.0
                                    * (1.0
                                        - r2 / 90.0
                                            * (1.0
                                                - r2 / 132.0
                                                    * (1.0 - r2 / 182.0 * (1.0 - r2 / 240.0)))))));
    (sin, cos)
}

/// The sine and cosine of `r` turned by `quadrant` quarter turns.
fn turned((sin, cos): (f64, f64), quadrant: u32) -> (f64, f64) {
    match quadrant & 3 {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// 64 bits of `2/pi`, starting `offset` bits after the binary point.
fn two_over_pi_bits(offset: usize) -> u64 {
    let (word, bit) = (offset / 64, offset % 64);
    let high = TWO_OVER_PI.get(word).copied().unwrap_or(0);
    if bit == 0 {
        return high;
    }
    let low = TWO_OVER_PI.get(word + 1).copied().unwrap_or(0);
    (high << bit) | (low >> (64 - bit))
}

/// Reduces a finite, non-negative `angle` of at least `pi/4`: returns the
/// quadrant `q` (modulo 4) and `r` in `[-pi/4, pi/4]` with
/// `angle = q pi/2 + r` exactly, `r` rounded once.
fn reduce(angle: f32) -> (u32, f64) {
    let bits = angle.to_bits();
    // `angle >= pi/4`, so it is a normal number: m 2^e with a 24-bit m.
    let mantissa = u128::from((bits & 0x007F_FFFF) | 0x0080_0000);
    let exponent = ((bits >> 23) & 0xFF) as i32 - 150;
    // angle 2/pi = m 2^e sum(b_i 2^-i). Bits b_i with i <= e - 2 add
    // multiples of 4, which do not change the quadrant: start at s.
    let s = (exponent - 1).max(1);
    let offset = (s - 1) as usize;
    let window = [
        u128::from(two_over_pi_bits(offset)),
        u128::from(two_over_pi_bits(offset + 64)),
        u128::from(two_over_pi_bits(offset + 128)),
    ];
    // product = m W, W the 192 bits from s on; angle 2/pi = product / 2^shift
    // plus less than 2^(24 - shift), and shift >= 190.
    let shift = (s + 191 - exponent) as u32;
    let p2 = mantissa * window[2];
    let p1 = mantissa * window[1] + (p2 >> 64);
    let p0 = mantissa * window[0] + (p1 >> 64);
    let high = p0; // bits 128 and up of the product
    let low = (p1 << 64) | (p2 & u128::from(u64::MAX));
    // The two bits above the binary point and 126 below it.
    let t = shift - 126; // 64 ..= 90
    let x = (high << (128 - t)) | (low >> t);
    let mut quadrant = (x >> 126) as u32;
    let fraction = x & ((1u128 << 126) - 1);
    let signed = if fraction >= 1u128 << 125 {
        quadrant += 1;
        fraction as i128 - (1i128 << 126)
    } else {
        fraction as i128
    };
    // 2^-126, exactly.
    let unit = f64::from_bits((1023 - 126) << 52);
    (quadrant & 3, signed as f64 * unit * FRAC_PI_2)
}

/// The sine and cosine of `angle` (radians), computed in `f64`.
///
/// The angle is reduced to a quarter turn exactly, so the error against the
/// true values is below 2e-16 for every finite `f32`, however large. A
/// non-finite angle gives `NaN` for both.
///
/// Unlike `f32::sin`, the result is the same bits on every platform, so a
/// widget that places what it draws with it records the same scene on
/// Windows, Linux and macOS, as golden tests need.
///
/// ```
/// let (sin, cos) = manguebyte_gfx::sin_cos(std::f32::consts::FRAC_PI_2);
/// assert!((sin - 1.0).abs() < 1e-15 && cos.abs() < 1e-7);
/// ```
pub fn sin_cos(angle: f32) -> (f64, f64) {
    if !angle.is_finite() {
        return (f64::NAN, f64::NAN);
    }
    let magnitude = angle.abs();
    let (sin, cos) = if f64::from(magnitude) <= std::f64::consts::FRAC_PI_4 {
        kernel(f64::from(magnitude))
    } else {
        let (quadrant, r) = reduce(magnitude);
        turned(kernel(r), quadrant)
    };
    if angle < 0.0 { (-sin, cos) } else { (sin, cos) }
}

/// The sine and cosine of an `f64` angle below a million radians,
/// reduced with a three-part `pi/2`. For the angles arcs use (a few turns)
/// the error is below 2e-16. From a million radians on, or for a
/// non-finite angle, both are `NaN`.
pub(crate) fn sin_cos_f64(angle: f64) -> (f64, f64) {
    // `NaN` fails the comparison too.
    if angle.abs().partial_cmp(&1.0e6) != Some(std::cmp::Ordering::Less) {
        return (f64::NAN, f64::NAN);
    }
    // `quadrant` is a whole number below 2^20, so its products with the
    // first two parts are exact.
    let quadrant = (angle / FRAC_PI_2 + 0.5).floor();
    let r = ((angle - quadrant * FRAC_PI_2_PARTS[0]) - quadrant * FRAC_PI_2_PARTS[1])
        - quadrant * FRAC_PI_2_PARTS[2];
    // `rem_euclid` on the float keeps this exact for any whole quadrant.
    turned(kernel(r), quadrant.rem_euclid(4.0) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_at_the_quadrant_points() {
        assert_eq!(sin_cos(0.0), (0.0, 1.0));
        let (s, c) = sin_cos_f64(FRAC_PI_2);
        assert_eq!(s, 1.0);
        assert!(c.abs() < 1e-16);
        let (s, c) = sin_cos_f64(std::f64::consts::PI);
        assert!(s.abs() < 1e-15);
        assert_eq!(c, -1.0);
        let (s, c) = sin_cos_f64(-FRAC_PI_2);
        assert_eq!(s, -1.0);
        assert!(c.abs() < 1e-16);
    }

    #[test]
    fn agrees_with_the_platform_library_to_within_1e_15() {
        let mut worst = 0.0f64;
        let mut check = |(s, c): (f64, f64), angle: f64| {
            worst = worst
                .max((s - angle.sin()).abs())
                .max((c - angle.cos()).abs());
        };
        for step in -20_000..=20_000 {
            let angle = f64::from(step) * 0.000_731;
            check(sin_cos_f64(angle), angle);
            let single = angle as f32;
            check(sin_cos(single), f64::from(single));
        }
        assert!(worst < 1e-15, "worst error {worst:e}");
    }

    #[test]
    fn large_angles_are_reduced_exactly() {
        // Each `f32` angle with its sine and cosine, computed to 400 digits
        // with an exact reduction (Python's `decimal`, `pi` from Machin's
        // formula) and rounded to `f64`.
        let cases: [(f32, f64, f64); 12] = [
            (
                1.0e18,
                -2.167_378_750_621_411_2e-1,
                9.762_298_364_184_264e-1,
            ),
            (3.0e38, 8.749_048_877_644_344e-1, -4.842_947_835_419_069e-1),
            (
                16_777_216.0,
                -7.795_636_732_177_778e-1,
                6.263_229_832_915_329e-1,
            ),
            (1.0e7, 4.205_477_931_907_825e-1, -9.072_703_861_817_396e-1),
            (
                123_456.79,
                -9.986_608_508_596_387e-1,
                5.173_494_911_858_269e-2,
            ),
            (
                f32::MAX,
                -5.218_765_233_336_585e-1,
                8.530_210_398_303_042e-1,
            ),
            (1.0e16, -4.965_746_435_260_546e-1, 8.679_940_226_792_877e-1),
            (1.0e17, -5.699_716_599_391_35e-1, -8.216_643_517_070_867e-1),
            (
                1.570_796_4,
                9.999_999_999_999_99e-1,
                -4.371_139_000_186_241e-8,
            ),
            (
                1_152_921_504_606_846_976.0,
                -8.306_492_176_372_546e-1,
                -5.567_960_822_766_417e-1,
            ),
            (
                -7.5e21,
                -5.795_976_425_482_079e-1,
                -8.149_027_995_733_968e-1,
            ),
            (
                2.356_194_5,
                7.071_067_769_704_656e-1,
                -7.071_067_854_026_294e-1,
            ),
        ];
        for (angle, sin, cos) in cases {
            let (s, c) = sin_cos(angle);
            assert!(
                (s - sin).abs() <= 2e-16 && (c - cos).abs() <= 2e-16,
                "sin_cos({angle:e}) = ({s:e}, {c:e}), expected ({sin:e}, {cos:e})"
            );
        }
    }

    #[test]
    fn every_finite_angle_gives_a_point_on_the_unit_circle() {
        // Every 4099th f32 bit pattern, both signs, all exponents.
        for bits in (0..0x7F80_0000u32).step_by(4099) {
            for angle in [f32::from_bits(bits), -f32::from_bits(bits)] {
                let (s, c) = sin_cos(angle);
                assert!(s.abs() <= 1.0 && c.abs() <= 1.0, "{angle:e}: ({s}, {c})");
                assert!((s * s + c * c - 1.0).abs() < 1e-15, "{angle:e}: ({s}, {c})");
            }
        }
    }

    #[test]
    fn non_finite_angles_give_nan() {
        for angle in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let (s, c) = sin_cos(angle);
            assert!(s.is_nan() && c.is_nan());
        }
        for angle in [f64::NAN, f64::INFINITY, 2.0e6] {
            let (s, c) = sin_cos_f64(angle);
            assert!(s.is_nan() && c.is_nan());
        }
    }
}
