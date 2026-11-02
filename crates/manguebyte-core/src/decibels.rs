//! Conversions between decibels and linear gain.

/// Level treated as silence. Anything at or below it maps to a gain of zero.
pub const MINUS_INFINITY_DB: f64 = -100.0;

/// Converts decibels to linear gain.
///
/// Levels at or below [`MINUS_INFINITY_DB`] return exactly `0.0`.
#[inline]
pub fn db_to_gain(db: f64) -> f64 {
    if db <= MINUS_INFINITY_DB {
        0.0
    } else {
        10f64.powf(db / 20.0)
    }
}

/// Converts linear gain to decibels, floored at [`MINUS_INFINITY_DB`].
#[inline]
pub fn gain_to_db(gain: f64) -> f64 {
    if gain <= 0.0 {
        MINUS_INFINITY_DB
    } else {
        (20.0 * gain.log10()).max(MINUS_INFINITY_DB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_values() {
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-12);
        assert!((db_to_gain(-6.0) - 0.501187).abs() < 1e-6);
        assert!((gain_to_db(0.5) + 6.0206).abs() < 1e-4);
    }

    #[test]
    fn silence_floor() {
        assert_eq!(db_to_gain(MINUS_INFINITY_DB), 0.0);
        assert_eq!(gain_to_db(0.0), MINUS_INFINITY_DB);
        assert_eq!(gain_to_db(-1.0), MINUS_INFINITY_DB);
    }

    #[test]
    fn round_trip() {
        for db in [-60.0, -12.0, 0.0, 6.0] {
            assert!((gain_to_db(db_to_gain(db)) - db).abs() < 1e-9);
        }
    }
}
