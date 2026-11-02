//! The sampler's windowed-sinc reading at a ratio that may change on every
//! frame, for any player: [`SincReader`]. The sampler's own voices find
//! their taps with the same function.

use std::fmt;
use std::sync::Arc;

use crate::interpolate::{MAX_TAPS, SincTable};

/// One in the readers' fixed-point positions and steps: 32 bits of
/// fraction.
pub(crate) const ONE: u64 = 1 << 32;

/// Most taps a read takes: 64 when reading at or below a sample's own rate,
/// up to 256 when reading faster (the kernel is stretched, up to four
/// times, so that nothing above half the output rate folds back).
pub const SINC_MAX_TAPS: usize = MAX_TAPS;

/// The first frame and the number of the taps for a read at `position`
/// (32 bits of fraction) moving `step` per output frame (likewise), with
/// their weights in `weights`. A read at a step of exactly one from a
/// whole frame takes that frame as it is.
#[inline]
pub(crate) fn sinc_taps(
    table: &SincTable,
    position: u64,
    step: u64,
    weights: &mut [f64; MAX_TAPS],
) -> (i64, usize) {
    let index = (position >> 32) as i64;
    let fraction_bits = position & (ONE - 1);
    if step == ONE && fraction_bits == 0 {
        weights[0] = 1.0;
        return (index, 1);
    }
    let fraction = fraction_bits as f64 / ONE as f64;
    let half = table.weights(fraction, step as f64 / ONE as f64, weights);
    (index + 1 - half as i64, 2 * half)
}

/// A step of `ratio` frames in 32 bits of fraction, within what a reader
/// takes: 1/65536 to 256.
#[inline]
pub(crate) fn fixed_step(ratio: f64) -> u64 {
    let ratio = if ratio.is_finite() { ratio } else { 1.0 };
    (ratio.clamp(1.0 / 65_536.0, 256.0) * ONE as f64).round() as u64
}

/// Reads a signal between its frames with the windowed sinc the sampler's
/// voices use ([`Interpolation::Sinc`](crate::Interpolation::Sinc), whose
/// measured quality is given there), at a ratio given for every output
/// frame: frequency modulation that is exact to the frame, such as a wind
/// model's on every pipe of an organ.
///
/// The position is kept to 32 bits of fraction of a frame, as the voices
/// keep theirs, so a ratio is exact to far better than a thousandth of a
/// cent. Reading faster than the signal's own rate stretches the kernel,
/// as the voices do, so that nothing above half the output rate folds
/// back. Real-time safe: nothing allocates, locks or waits.
///
/// ```
/// use manguebyte_sampler::SincReader;
///
/// // A second of 1 kHz at 48 kHz, read 2 cents sharp.
/// let input: Vec<f32> = (0..48_000)
///     .map(|n| (std::f64::consts::TAU * 1_000.0 * n as f64 / 48_000.0).sin() as f32)
///     .collect();
/// let ratio = 2f32.powf(2.0 / 1_200.0);
/// let mut reader = SincReader::new();
/// reader.set_position(100.0);
/// let mut output = [0.0f32; 480];
/// reader.process(&input, &[ratio; 480], &mut output);
/// assert!((reader.position() - (100.0 + 480.0 * f64::from(ratio))).abs() < 1e-6);
/// ```
#[derive(Clone)]
pub struct SincReader {
    table: Arc<SincTable>,
    position: u64,
}

impl fmt::Debug for SincReader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SincReader")
            .field("position", &self.position())
            .finish_non_exhaustive()
    }
}

impl Default for SincReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SincReader {
    /// A reader at frame 0, on the table every instrument shares (made the
    /// first time it is needed: see [`prepare_tables`](crate::prepare_tables)).
    pub fn new() -> Self {
        Self {
            table: SincTable::shared(),
            position: 0,
        }
    }

    /// The frame of the input the next output frame is read at.
    pub fn position(&self) -> f64 {
        self.position as f64 / ONE as f64
    }

    /// Moves to `frame` of the input (0 or more; held to what 32 bits of
    /// whole frames reach).
    pub fn set_position(&mut self, frame: f64) {
        let frame = if frame.is_finite() { frame } else { 0.0 };
        self.position = (frame.clamp(0.0, f64::from(u32::MAX)) * ONE as f64).round() as u64;
    }

    /// Reads `output.len()` frames from `input` (frames outside it are
    /// silence), from where the reader stands, each `ratios[n]` input
    /// frames after the one before (the last ratio holds if `ratios` is
    /// shorter; 1 if it is empty), and moves on. Ratios are held to 1/65536
    /// to 256.
    pub fn process(&mut self, input: &[f32], ratios: &[f32], output: &mut [f32]) {
        let mut weights = [0.0; MAX_TAPS];
        let mut ratio = ratios.first().copied().unwrap_or(1.0);
        for (n, out) in output.iter_mut().enumerate() {
            if let Some(&next) = ratios.get(n) {
                ratio = next;
            }
            let step = fixed_step(f64::from(ratio));
            let (first, count) = sinc_taps(&self.table, self.position, step, &mut weights);
            let mut sum = 0.0;
            for (tap, &weight) in weights[..count].iter().enumerate() {
                let at = first + tap as i64;
                if let Some(&value) = usize::try_from(at).ok().and_then(|at| input.get(at)) {
                    sum += weight * f64::from(value);
                }
            }
            *out = sum as f32;
            self.position = self.position.saturating_add(step);
        }
    }
}
