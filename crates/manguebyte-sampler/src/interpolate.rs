//! Reading a sample between its frames.

use manguebyte_dsp::fir::{kaiser_beta, sinc};

/// How a voice reads its sample between frames, which is what changing a
/// sample's pitch requires.
///
/// A voice that plays a sample at its own pitch and rate reads its frames
/// as they are, whichever interpolation is chosen.
///
/// The figures below are measured by `tests/aliasing.rs`, with samples
/// played at 48 kHz: "everything else" is what is left after the pitched
/// tone is fitted and taken away, and "what comes out" is the whole output
/// against the tone that went in.
///
/// | | Sinc | Cubic |
/// |---|---|---|
/// | 1 kHz up a fifth: everything else | SINC_UP | CUBIC_UP |
/// | 1 kHz down a fifth: everything else | SINC_DOWN | |
/// | 8 kHz up a fourth: everything else | SINC_HIGH | CUBIC_HIGH |
/// | 18 kHz in a 44.1 kHz sample, at its pitch: its level | SINC_PASS18 | |
/// | 20 kHz down a semitone (to 18.9 kHz): its level | SINC_PASS20 | |
/// | 20 kHz up four semitones (would fold back to 22.8 kHz): what comes out | SINC_NEAR | CUBIC_NEAR |
/// | 15 kHz up an octave (would fold back to 18 kHz): what comes out | SINC_OCTAVE | CUBIC_OCTAVE |
/// | 9 kHz up two octaves (would fold back to 12 kHz): what comes out | SINC_TWO | |
///
/// `examples/bench.rs` and `examples/wasm_bench.rs` time both natively
/// and in WebAssembly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Interpolation {
    /// A windowed sinc low-pass: 64 taps (32 frames each side) under a
    /// Kaiser window designed for 80 dB, with its cutoff at 0.91 of half
    /// the rate it filters at, so that its stopband begins at half that
    /// rate and nothing within reach of the window folds back. In return,
    /// frequencies above about 0.83 of half the sample's rate are turned
    /// down, by 6 dB at 0.91 (see the table).
    ///
    /// When a sample is played slower than it was recorded, the filter
    /// works at the sample's rate and its weights come from a table of 512
    /// phases per frame, interpolated linearly. When it is played faster,
    /// the kernel is stretched by the same factor so that the filter works
    /// at the output's rate, and its weights are computed for each output
    /// frame; the stretch, and with it the cost, stops growing at four
    /// times (two octaves up), and beyond that what lies above half the
    /// output rate is no longer fully removed. The weights are scaled to
    /// sum to one (to single precision when read from the table of phases,
    /// which is kept in single precision).
    #[default]
    Sinc,
    /// Four-point cubic interpolation (Catmull-Rom): 4 taps instead of 64
    /// to 256, with no protection against aliasing. Good for low and
    /// middle frequencies; high ones come out with distortion, and a tone
    /// pitched past half the output rate folds back at full level.
    Cubic,
}

/// Frames of the kernel on each side of its centre, unstretched.
pub(crate) const HALF_TAPS: usize = 32;

/// Largest factor the kernel is stretched by when reading faster than the
/// sample's own rate.
pub(crate) const MAX_STRETCH: usize = 4;

/// Most taps a read can use.
pub(crate) const MAX_TAPS: usize = 2 * HALF_TAPS * MAX_STRETCH;

/// The kernel's cutoff, as a fraction of half the rate it filters at. A
/// Kaiser window of `2 * HALF_TAPS` frames designed for 80 dB has a
/// transition band about 0.079 of that wide on each side of the cutoff
/// (Kaiser's estimate, (A - 7.95) / (14.36 N)), so this puts the stopband's
/// start just below half the rate.
const CUTOFF: f64 = 0.91;

/// Table points per frame of distance, and phases per frame.
const RESOLUTION: usize = 512;

/// One in the fixed point the stretched kernel is read with.
const FIXED_ONE: f64 = 4_294_967_296.0;

/// Stopband the window is designed for, in decibels.
const ATTENUATION_DB: f64 = 80.0;

/// The windowed sinc, finely sampled, and the weights it gives at each
/// phase when it is not stretched. Both are kept in single precision,
/// which is far below the window's 80 dB and halves what the voices read.
#[derive(Clone)]
pub(crate) struct SincTable {
    /// One side of the kernel, `RESOLUTION` points per frame of distance:
    /// each point's value and the step to the next, so that one read gives
    /// both ends of the interval. The last points are zero.
    points: Vec<[f32; 2]>,
    /// For each of `RESOLUTION + 1` phases, the `2 * HALF_TAPS` weights,
    /// scaled to sum to one.
    phases: Vec<f32>,
}

impl core::fmt::Debug for SincTable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SincTable")
            .field("points", &self.points.len())
            .field("phases", &(self.phases.len() / (2 * HALF_TAPS)))
            .finish()
    }
}

/// The modified Bessel function of the first kind, order zero, by its power
/// series, which converges quickly for the arguments a Kaiser window uses.
fn bessel_i0(x: f64) -> f64 {
    let quarter = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..64 {
        term *= quarter / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

impl SincTable {
    pub(crate) fn new() -> Self {
        let beta = kaiser_beta(ATTENUATION_DB);
        let span = (HALF_TAPS * RESOLUTION) as f64;
        // Two points past the end, both zero, so that the last interval
        // needs no special case.
        let values: Vec<f64> = (0..=HALF_TAPS * RESOLUTION + 1)
            .map(|index| {
                let position = index as f64 / span;
                if position >= 1.0 {
                    return 0.0;
                }
                let window = bessel_i0(beta * (1.0 - position * position).sqrt()) / bessel_i0(beta);
                sinc(CUTOFF * index as f64 / RESOLUTION as f64) * window
            })
            .collect();
        // Unstretched, every distance a tap can have is a whole number of
        // table points away from the centre, so the phases read the table
        // exactly.
        let taps = 2 * HALF_TAPS;
        let mut phases = vec![0.0; (RESOLUTION + 1) * taps];
        let mut row = vec![0.0f64; taps];
        for (phase, out) in phases.chunks_exact_mut(taps).enumerate() {
            for (tap, weight) in row.iter_mut().enumerate() {
                let distance = (tap * RESOLUTION).abs_diff((HALF_TAPS - 1) * RESOLUTION + phase);
                *weight = values.get(distance).copied().unwrap_or(0.0);
            }
            let sum: f64 = row.iter().sum();
            for (out, weight) in out.iter_mut().zip(&row) {
                *out = (weight / sum) as f32;
            }
        }
        let points = values
            .iter()
            .enumerate()
            .map(|(index, &value)| {
                let next = values.get(index + 1).copied().unwrap_or(0.0);
                [value as f32, (next - value) as f32]
            })
            .collect();
        Self { points, phases }
    }

    /// The kernel at `scaled` table points from its centre, `scaled >= 0`.
    #[cfg(test)]
    fn at_point(&self, scaled: f64) -> f64 {
        // Saturating float-to-int conversion; anything past the table reads
        // its last point, which is zero with no step.
        let index = (scaled as usize).min(self.points.len() - 1);
        let [value, step] = self.points[index];
        f64::from(value) + f64::from(step) * (scaled - index as f64)
    }

    /// The kernel at `x` frames from its centre, `x >= 0`.
    #[cfg(test)]
    fn at(&self, x: f64) -> f64 {
        self.at_point(x * RESOLUTION as f64)
    }

    /// Half the taps a read `step` frames per output frame uses.
    #[inline]
    pub(crate) fn half(step: f64) -> usize {
        let stretch = step.clamp(1.0, MAX_STRETCH as f64);
        ((HALF_TAPS as f64 * stretch).ceil() as usize).clamp(HALF_TAPS, HALF_TAPS * MAX_STRETCH)
    }

    /// Fills `weights` for an output that falls `fraction` past the frame at
    /// tap `half - 1` (taps run from `1 - half` to `half` around that frame),
    /// reading `step` frames per output frame. Returns `half`.
    #[inline]
    pub(crate) fn weights(&self, fraction: f64, step: f64, weights: &mut [f64; MAX_TAPS]) -> usize {
        let taps = 2 * HALF_TAPS;
        if step <= 1.0 {
            // Between two phases of the table.
            let scaled = fraction.clamp(0.0, 1.0) * RESOLUTION as f64;
            let phase = (scaled as usize).min(RESOLUTION - 1);
            let t = scaled - phase as f64;
            let rows = self.phases.get(phase * taps..(phase + 2) * taps);
            if let Some(rows) = rows {
                let (low, high) = rows.split_at(taps);
                for ((weight, &a), &b) in weights.iter_mut().zip(low).zip(high) {
                    let a = f64::from(a);
                    *weight = a + (f64::from(b) - a) * t;
                }
            }
            return HALF_TAPS;
        }
        let stretch = step.min(MAX_STRETCH as f64);
        let half = Self::half(step);
        // Distances in table points, in fixed point with 32 bits of
        // fraction: whole taps are `per_tap` apart and the output point is
        // `centre` from the first. Integer steps avoid a float-to-integer
        // conversion for every tap. Both fit easily: at most 512 points per
        // tap and 256 taps.
        let per_tap = (RESOLUTION as f64 / stretch * FIXED_ONE).round() as u64;
        let centre =
            ((half as f64 - 1.0 + fraction) * RESOLUTION as f64 / stretch * FIXED_ONE).round() as u64;
        let last = self.points.len() - 1;
        // Four partial sums, so that the additions do not wait on one
        // another.
        let mut sums = [0.0; 4];
        for (tap, weight) in weights.iter_mut().take(2 * half).enumerate() {
            let distance = (tap as u64 * per_tap).abs_diff(centre);
            // Past the table, the last point: zero, with no step.
            let index = ((distance >> 32) as usize).min(last);
            let between = f64::from(distance as u32) / FIXED_ONE;
            let [value, slope] = self.points[index];
            *weight = f64::from(value) + f64::from(slope) * between;
            sums[tap & 3] += *weight;
        }
        let sum = (sums[0] + sums[1]) + (sums[2] + sums[3]);
        if sum.abs() > 1e-6 {
            let scale = 1.0 / sum;
            for weight in weights.iter_mut().take(2 * half) {
                *weight *= scale;
            }
        }
        half
    }
}

/// Catmull-Rom weights for the frames at -1, 0, 1 and 2 around a point
/// `t` past frame 0.
#[inline]
pub(crate) fn cubic_weights(t: f64) -> [f64; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [
        -0.5 * t3 + t2 - 0.5 * t,
        1.5 * t3 - 2.5 * t2 + 1.0,
        -1.5 * t3 + 2.0 * t2 + 0.5 * t,
        0.5 * t3 - 0.5 * t2,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bessel_matches_known_values() {
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-15);
        assert!((bessel_i0(1.0) - 1.266_065_877_752_008_4).abs() < 1e-14);
        assert!((bessel_i0(2.0) - 2.279_585_302_336_067_3).abs() < 1e-14);
    }

    #[test]
    fn phases_agree_with_the_kernel_they_come_from() {
        // The table of phases is the unstretched kernel, read at whole
        // table points: computing the weights directly gives the same.
        let table = SincTable::new();
        let mut from_phases = [0.0; MAX_TAPS];
        for fraction in [0.0, 0.25, 0.5, 0.37, 0.999] {
            let half = table.weights(fraction, 1.0, &mut from_phases);
            assert_eq!(half, HALF_TAPS);
            let mut direct = [0.0; 2 * HALF_TAPS];
            for (tap, weight) in direct.iter_mut().enumerate() {
                *weight = table.at((tap as f64 - (HALF_TAPS as f64 - 1.0) - fraction).abs());
            }
            let sum: f64 = direct.iter().sum();
            for (a, b) in from_phases.iter().zip(&direct) {
                assert!((a - b / sum).abs() < 1e-6, "{fraction}: {a} against {}", b / sum);
            }
        }
    }

    #[test]
    fn stretched_weights_follow_the_kernel() {
        // The fixed-point walk gives the kernel, stretched, at each tap.
        let table = SincTable::new();
        let mut weights = [0.0; MAX_TAPS];
        for (step, fraction) in [(1.25, 0.0), (1.5, 0.37), (2.0, 0.999), (3.3, 0.5), (8.0, 0.1)] {
            let half = table.weights(fraction, step, &mut weights);
            let stretch = f64::min(step, MAX_STRETCH as f64);
            let direct: Vec<f64> = (0..2 * half)
                .map(|tap| table.at((tap as f64 - (half as f64 - 1.0) - fraction).abs() / stretch))
                .collect();
            let sum: f64 = direct.iter().sum();
            for (tap, (a, b)) in weights.iter().zip(&direct).enumerate() {
                assert!((a - b / sum).abs() < 1e-9, "{step} {fraction} tap {tap}: {a} against {}", b / sum);
            }
        }
    }

    #[test]
    fn weights_sum_to_one_and_stretch_with_the_step() {
        let table = SincTable::new();
        let mut weights = [0.0; MAX_TAPS];
        for (step, taps) in [(0.5, 64), (1.0, 64), (1.5, 96), (4.0, 256), (9.0, 256)] {
            let half = table.weights(0.37, step, &mut weights);
            assert_eq!(2 * half, taps);
            assert_eq!(SincTable::half(step), half);
            let sum: f64 = weights[..taps].iter().sum();
            // The table of phases is kept in single precision; stretched
            // weights are scaled in double.
            let tolerance = if step <= 1.0 { 1e-6 } else { 1e-12 };
            assert!((sum - 1.0).abs() < tolerance, "{step}: {sum}");
        }
        let cubic: f64 = cubic_weights(0.3).iter().sum();
        assert!((cubic - 1.0).abs() < 1e-12);
        assert_eq!(cubic_weights(0.0), [0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn the_stopband_starts_below_half_the_rate() {
        // The kernel as the continuous filter the interpolator applies:
        // its response, from the finely sampled table, at frequencies
        // given as fractions of half the rate it filters at. Being
        // symmetric, it is a sum of cosines.
        let table = SincTable::new();
        let response = |fraction_of_half: f64| {
            let omega = core::f64::consts::PI * fraction_of_half / RESOLUTION as f64;
            let at = |omega: f64| {
                f64::from(table.points[0][0])
                    + 2.0
                        * table.points[1..]
                            .iter()
                            .enumerate()
                            .map(|(k, point)| f64::from(point[0]) * (omega * (k + 1) as f64).cos())
                            .sum::<f64>()
            };
            20.0 * (at(omega) / at(0.0)).abs().log10()
        };
        assert!(response(0.5).abs() < 0.01, "{}", response(0.5));
        assert!(response(0.8).abs() < 0.01, "{}", response(0.8));
        for at in [1.0, 1.05, 1.2, 1.5, 1.9] {
            assert!(response(at) < -75.0, "{at}: {}", response(at));
        }
    }
}
