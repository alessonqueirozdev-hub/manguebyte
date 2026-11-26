//! The lookup tables: how far they are from their functions, what they do
//! outside their ranges, and that they read the same bits everywhere.

#[path = "support/lookup_golden.rs"]
mod golden;

use core::f64::consts::TAU;

use manguebyte_core::lookup::{LookupTable, LookupTransform, SineTable, TableError};

/// Inputs swept over each range; Miri is slow.
const SWEEP: usize = if cfg!(miri) { 200 } else { 1_000_003 };

/// The largest `error(x)` for `x` evenly spaced from `from` to `to`.
fn largest(from: f64, to: f64, mut error: impl FnMut(f64) -> f64) -> f64 {
    (0..=SWEEP)
        .map(|k| error(from + (to - from) * (k as f64 / SWEEP as f64)))
        .fold(0.0, |worst, e| {
            assert!(e.is_finite());
            f64::max(worst, e)
        })
}

#[track_caller]
fn below(measured: f64, bound: f64, what: &str) {
    println!("{what}: {measured:.3e} (bound {bound:.2e})");
    assert!(
        measured < bound,
        "{what}: {measured:e} is not below {bound:e}"
    );
}

// The bounds below are the ones the documentation of each table states.

#[test]
fn the_sine_table_is_within_its_stated_error() {
    let narrow = SineTable::<f32>::new(2_048).unwrap();
    let wide = SineTable::<f64>::new(2_048).unwrap();
    let of_f32 = |read: &dyn Fn(f32) -> f32| {
        largest(-2.0, 2.0, |x| {
            let x = x as f32;
            (f64::from(read(x)) - (TAU * f64::from(x)).sin()).abs()
        })
    };
    let of_f64 =
        |read: &dyn Fn(f64) -> f64| largest(-2.0, 2.0, |x| (read(x) - (TAU * x).sin()).abs());
    below(of_f32(&|x| narrow.linear(x)), 1.3e-6, "sine f32 linear");
    below(of_f32(&|x| narrow.cubic(x)), 2.1e-7, "sine f32 cubic");
    below(of_f64(&|x| wide.linear(x)), 1.2e-6, "sine f64 linear");
    below(of_f64(&|x| wide.cubic(x)), 2.1e-12, "sine f64 cubic");
}

#[test]
fn the_tanh_table_is_within_its_stated_error() {
    let narrow = LookupTransform::<f32>::tanh(4_096).unwrap();
    let wide = LookupTransform::<f64>::tanh(4_096).unwrap();
    // Past both ends of the range, where the table holds.
    let of_f32 = |read: &dyn Fn(f32) -> f32| {
        largest(-16.0, 16.0, |x| {
            let x = x as f32;
            (f64::from(read(x)) - f64::from(x).tanh()).abs()
        })
    };
    let of_f64 = |read: &dyn Fn(f64) -> f64| largest(-16.0, 16.0, |x| (read(x) - x.tanh()).abs());
    below(of_f32(&|x| narrow.linear(x)), 3.4e-6, "tanh f32 linear");
    below(of_f32(&|x| narrow.cubic(x)), 2.5e-7, "tanh f32 cubic");
    below(of_f64(&|x| wide.linear(x)), 3.4e-6, "tanh f64 linear");
    below(of_f64(&|x| wide.cubic(x)), 1.2e-10, "tanh f64 cubic");
    for x in [12.0, 13.0, 1e30, f64::INFINITY] {
        for y in [
            wide.linear(x),
            wide.cubic(x),
            -wide.linear(-x),
            -wide.cubic(-x),
        ] {
            assert!((1.0 - y).abs() < 8e-11, "{x}: {y}");
        }
    }
}

#[test]
fn the_exp_table_is_within_its_stated_error() {
    let narrow = LookupTransform::<f32>::exp(-1.0, 1.0, 1_024).unwrap();
    let wide = LookupTransform::<f64>::exp(-1.0, 1.0, 1_024).unwrap();
    let of_f32 = |read: &dyn Fn(f32) -> f32| {
        largest(-1.0, 1.0, |x| {
            let x = x as f32;
            let exact = f64::from(x).exp();
            (f64::from(read(x)) - exact).abs() / exact
        })
    };
    let of_f64 =
        |read: &dyn Fn(f64) -> f64| largest(-1.0, 1.0, |x| (read(x) - x.exp()).abs() / x.exp());
    below(of_f32(&|x| narrow.linear(x)), 6e-7, "exp f32 linear");
    below(of_f32(&|x| narrow.cubic(x)), 3e-7, "exp f32 cubic");
    below(of_f64(&|x| wide.linear(x)), 4.8e-7, "exp f64 linear");
    below(of_f64(&|x| wide.cubic(x)), 6.2e-13, "exp f64 cubic");
}

/// The bounds of the module documentation, `h^2 / 8 * max |f''|` and the two
/// for cubics, on a function whose derivatives are all within 1, and the
/// rates at which the error falls.
#[test]
fn the_error_of_a_table_follows_the_documented_bounds() {
    let (from, to) = (0.7f64, 2.4f64);
    let mut before: Option<[f64; 3]> = None;
    for points in [17, 33, 65, 129] {
        let table = LookupTransform::<f64>::new(from, to, points, f64::sin).unwrap();
        let h = (to - from) / (points - 1) as f64;
        let linear = largest(from, to, |x| (table.linear(x) - x.sin()).abs());
        let middle = largest(from + h, to - h, |x| (table.cubic(x) - x.sin()).abs());
        let ends = largest(from, from + h, |x| (table.cubic(x) - x.sin()).abs()).max(largest(
            to - h,
            to,
            |x| (table.cubic(x) - x.sin()).abs(),
        ));
        below(linear, h * h / 8.0, "linear");
        below(middle, 3.0 * h.powi(4) / 128.0, "cubic, middle");
        below(ends, h.powi(4) / 24.0, "cubic, ends");
        // Miri sweeps too few inputs to find each largest error.
        if let (false, Some([linear_before, middle_before, ends_before])) = (cfg!(miri), before) {
            assert!((3.5..4.5).contains(&(linear_before / linear)));
            assert!((13.0..19.0).contains(&(middle_before / middle)));
            assert!((10.0..22.0).contains(&(ends_before / ends)));
        }
        before = Some([linear, middle, ends]);
    }
}

#[test]
fn a_cubic_follows_a_cubic_to_its_ends() {
    let f = |x: f64| 0.3 * x * x * x - 1.1 * x * x + 0.7 * x - 2.0;
    for points in [4, 5, 6, 11] {
        let table = LookupTransform::<f64>::new(-2.0, 3.0, points, f).unwrap();
        let worst = largest(-2.0, 3.0, |x| (table.cubic(x) - f(x)).abs());
        below(worst, 2e-14, "a cubic polynomial");
    }
}

#[test]
fn a_linear_function_is_followed_by_both_readings() {
    let table = LookupTransform::<f32>::new(-1.0, 1.0, 4, |x| 3.0 * x + 1.0).unwrap();
    let worst = largest(-1.0, 1.0, |x| {
        let x = x as f32;
        f64::from((table.linear(x) - (3.0 * x + 1.0)).abs())
            .max(f64::from((table.cubic(x) - (3.0 * x + 1.0)).abs()))
    });
    below(worst, 1e-6, "a straight line");
}

#[test]
fn every_point_reads_as_its_value() {
    let mut random = 0x2545_F491_4F6C_DD1Du64;
    for points in [4, 5, 8, 257] {
        let values: Vec<f32> = (0..points)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                (random >> 40) as f32 / 1000.0 - 8000.0
            })
            .collect();
        let table = LookupTable::from_values(&values).unwrap();
        assert_eq!(table.points(), points);
        assert_eq!(table.values(), values);
        for (i, value) in values.iter().enumerate() {
            assert_eq!(table.linear(i as f32), *value, "linear at {i} of {points}");
            assert_eq!(table.cubic(i as f32), *value, "cubic at {i} of {points}");
        }
        // Outside the indices the table holds its ends.
        for (position, end) in [
            (-0.5, 0),
            (-1e30, 0),
            (f32::NEG_INFINITY, 0),
            (f32::NAN, 0),
            (points as f32 - 0.999, points - 1),
            (1e30, points - 1),
            (f32::INFINITY, points - 1),
        ] {
            assert_eq!(table.linear(position), values[end], "linear {position}");
            assert_eq!(table.cubic(position), values[end], "cubic {position}");
        }
    }
}

#[test]
fn a_value_halfway_is_the_mean_of_its_neighbours() {
    let table = LookupTable::from_values(&[0.0f64, 2.0, 6.0, 12.0]).unwrap();
    assert_eq!(table.linear(1.5), 4.0);
    assert_eq!(table.linear(2.25), 7.5);
    // The values are of `x^2 + x`, which the cubic follows.
    for position in [0.5, 1.5, 2.5] {
        assert!((table.cubic(position) - (position * position + position)).abs() < 1e-14);
    }
}

#[test]
fn a_transform_places_its_inputs() {
    let table = LookupTransform::<f32>::new(10.0, 20.0, 11, |x| x * 2.0).unwrap();
    assert_eq!((table.start(), table.end()), (10.0, 20.0));
    assert_eq!(table.position(10.0), 0.0);
    assert_eq!(table.position(13.5), 3.5);
    assert_eq!(table.position(20.0), 10.0);
    assert_eq!(table.table().values()[3], 26.0);
    assert_eq!(table.linear(13.5), 27.0);
    assert_eq!(table.linear(-100.0), 20.0);
    assert_eq!(table.cubic(100.0), 40.0);
    assert_eq!(table.linear(f32::NAN), 20.0, "NaN reads the start");
}

#[test]
fn no_input_reads_anything_but_a_finite_value() {
    let huge = f32::MAX / 33.0;
    let table = LookupTransform::<f32>::new(-1.0, 1.0, 6, |x| {
        if (x * 5.0).round() as i64 % 2 == 0 {
            f64::from(huge)
        } else {
            -f64::from(huge)
        }
    })
    .unwrap();
    let far = LookupTransform::<f32>::new(1e38, 3e38, 100, |_| 1.0).unwrap();
    let sine = SineTable::<f32>::new(4).unwrap();
    let step = if cfg!(miri) { 40_000_003 } else { 4_099 };
    for bits in (0..=u32::MAX).step_by(step) {
        let x = f32::from_bits(bits);
        for y in [
            table.linear(x),
            table.cubic(x),
            far.linear(x),
            far.cubic(x),
            sine.linear(x),
            sine.cubic(x),
        ] {
            assert!(y.is_finite(), "{x:e} ({bits:#x}) reads {y}");
        }
    }
}

#[test]
fn the_sine_repeats_and_takes_any_phase() {
    let sine = SineTable::<f32>::new(1_000).unwrap();
    assert_eq!(sine.points(), 1_000);
    for phase in [0.0f32, 0.125, 0.25, 0.5, 0.75, 0.9375] {
        for turns in [-7.0, -1.0, 1.0, 12.0] {
            assert_eq!(sine.linear(phase + turns), sine.linear(phase));
            assert_eq!(sine.cubic(phase + turns), sine.cubic(phase));
        }
    }
    assert_eq!(sine.linear(0.0), 0.0);
    assert_eq!(sine.linear(0.25), 1.0);
    assert_eq!(sine.cubic(0.75), -1.0);
    for phase in [1.0, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1e30] {
        assert_eq!(sine.linear(phase), 0.0, "{phase}");
        assert_eq!(sine.cubic(phase), 0.0, "{phase}");
    }
    // A hair below zero is a hair below a whole turn.
    for phase in [-1e-30f32, -1e-7] {
        assert!(sine.linear(phase).abs() < 1e-6);
        assert!(sine.cubic(phase).abs() < 1e-6);
    }
    // The smallest table still reads a sine through its four points.
    let coarse = SineTable::<f64>::new(4).unwrap();
    assert_eq!(coarse.linear(0.125), 0.5);
    assert_eq!(coarse.cubic(0.125), 0.625);
}

#[test]
fn a_block_reads_as_its_samples_do() {
    let table = LookupTransform::<f32>::tanh(64).unwrap();
    let input: Vec<f32> = (-40..41).map(|k| k as f32 * 0.37).collect();
    let mut linear = input.clone();
    table.linear_in_place(&mut linear);
    let mut cubic = input.clone();
    table.cubic_in_place(&mut cubic);
    for ((x, a), b) in input.iter().zip(linear).zip(cubic) {
        assert_eq!(a, table.linear(*x));
        assert_eq!(b, table.cubic(*x));
    }
}

#[test]
fn what_cannot_be_a_table_is_refused() {
    let points = |n| Err(TableError::Points { points: n });
    for n in [0, 1, 3, LookupTable::<f32>::MAX_POINTS + 1] {
        assert_eq!(LookupTable::<f32>::new(n, |_| 0.0).map(drop), points(n));
        assert_eq!(
            LookupTransform::<f32>::new(0.0, 1.0, n, |x| x).map(drop),
            points(n)
        );
        assert_eq!(SineTable::<f32>::new(n).map(drop), points(n));
        assert_eq!(LookupTransform::<f64>::tanh(n).map(drop), points(n));
    }
    for (start, end) in [
        (1.0, 1.0),
        (1.0, 0.0),
        (f64::NAN, 1.0),
        (0.0, f64::NAN),
        (f64::NEG_INFINITY, 0.0),
        (0.0, f64::INFINITY),
        // Wider than `f64` holds, and narrower than it can divide by.
        (f64::MIN, f64::MAX),
        (0.0, f64::from_bits(1)),
    ] {
        assert_eq!(
            LookupTransform::new(start, end, 16, |_| 0.0).map(drop),
            Err(TableError::Range),
            "{start} to {end}"
        );
    }
    // The widest and the narrowest ranges of `f32` are within what `f64`
    // places.
    for (start, end) in [(f32::MIN, f32::MAX), (0.0, f32::from_bits(1))] {
        let table = LookupTransform::new(start, end, 5, |_| 1.0).unwrap();
        assert_eq!(table.linear(end), 1.0);
    }
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 1e39, 2e37] {
        let function = |x: f64| if x >= 0.5 { bad } else { 0.0 };
        assert_eq!(
            LookupTransform::<f32>::new(0.0, 1.0, 5, function).map(drop),
            Err(TableError::Value { index: 2 }),
            "{bad}"
        );
    }
    assert_eq!(
        LookupTable::from_values(&[0.0f64, 1.0, f64::MAX, 2.0]).map(drop),
        Err(TableError::Value { index: 2 })
    );
    assert_eq!(
        LookupTransform::<f32>::exp(0.0, 100.0, 11).map(drop),
        Err(TableError::Value { index: 9 })
    );
    assert_eq!(
        TableError::Points { points: 2 }.to_string(),
        "a lookup table takes 4 to 16777216 points, not 2"
    );
}

#[test]
#[cfg_attr(miri, ignore = "arithmetic only, and slow under Miri")]
fn the_tables_read_the_same_bits_on_every_system() {
    assert_eq!(
        golden::fingerprint(),
        golden::FINGERPRINT,
        "{:#018x}",
        golden::fingerprint()
    );
}
