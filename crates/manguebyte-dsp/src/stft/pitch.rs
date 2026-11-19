//! A basic phase-vocoder pitch shifter.

use core::f64::consts::TAU;

use manguebyte_core::Sample;

use super::{SpectralFrame, SpectralProcessor, StftLayout, wrap_phase};
use crate::fft::Complex;

/// The smallest pitch ratio a [`PitchShift`] takes, two octaves down.
pub const MIN_PITCH_RATIO: f64 = 0.25;
/// The largest pitch ratio a [`PitchShift`] takes, two octaves up.
pub const MAX_PITCH_RATIO: f64 = 4.0;

/// A basic phase-vocoder pitch shifter, without changing the duration.
///
/// It follows the peak-shifting idea of J. Laroche and M. Dolson, "New
/// phase-vocoder techniques for pitch-shifting, harmonizing and other
/// exotic effects", IEEE WASPAA, 1999. In each frame it finds the peaks of
/// the magnitude spectrum (bins louder than the two on either side) and
/// gives every bin to the peak whose region it lies in, the regions meeting
/// at the quietest bin between two peaks. The true frequency of the peak is found from how far its
/// phase turned since the last frame, as in M. Dolson, "The phase vocoder:
/// a tutorial", Computer Music Journal, 1986. Each region is moved, whole,
/// by the whole number of bins nearest to how far that frequency moves, and
/// its phases are turned, frame after frame, by what makes the frequency
/// come out multiplied by the ratio exactly. Moving a region whole keeps the shape
/// of the window's spectrum, so a steady tone keeps its level.
///
/// It is basic, and sounds it:
///
/// - A peak's turn is carried from the frame before only when the peak
///   stays in the same bin, so tones that glide, and noise, come out rough.
/// - Transients are smeared over a frame.
/// - Formants move with the pitch.
/// - Shifting down leaves the top of the spectrum empty; shifting up drops
///   what would land beyond half the sample rate. Regions that land on one
///   another, shifting down, are added.
///
/// Small hops suit it: a quarter of the transform or less, with Hann
/// windows on both sides (the [`StftSettings`](super::StftSettings)
/// default). At a ratio of exactly 1 every frame passes unchanged.
///
/// ```
/// use manguebyte_dsp::stft::{PitchShift, Stft, StftSettings};
///
/// // A fifth up.
/// let shift = PitchShift::from_semitones(7.0);
/// let mut stft = Stft::<_, f32>::new(StftSettings::new(2_048), shift).unwrap();
/// assert!((stft.processor().ratio() - 1.498).abs() < 1e-3);
/// stft.processor_mut().set_semitones(-12.0);
/// ```
#[derive(Debug, Clone)]
pub struct PitchShift {
    ratio: f64,
    bins: usize,
    /// Phase advance of bin 1's centre frequency over one hop:
    /// `2π · hop / fft_size`.
    hop_turn: f64,
    hop: f64,
    /// Per channel and bin: the input phase of the last frame.
    last_phase: Vec<f64>,
    /// Per channel and bin: the turn given last frame to the region whose
    /// peak landed on this bin.
    turn: Vec<f64>,
    /// Scratch: the turns given this frame, by the bin their peak landed on.
    next_turn: Vec<f64>,
    /// Scratch: the magnitude of each input bin.
    magnitude: Vec<f64>,
    /// Scratch: the input phase of each bin.
    phase: Vec<f64>,
    /// Scratch: the peaks found, in order.
    peaks: Vec<usize>,
    /// Scratch: the shifted spectrum.
    shifted: Vec<Complex<f64>>,
}

impl PitchShift {
    /// A shift by `ratio` (2 is an octave up), limited to
    /// [`MIN_PITCH_RATIO`]..=[`MAX_PITCH_RATIO`]. Not a number is taken as 1.
    pub fn new(ratio: f64) -> Self {
        Self {
            ratio: Self::limit(ratio),
            bins: 0,
            hop_turn: 0.0,
            hop: 1.0,
            last_phase: Vec::new(),
            turn: Vec::new(),
            next_turn: Vec::new(),
            magnitude: Vec::new(),
            phase: Vec::new(),
            peaks: Vec::new(),
            shifted: Vec::new(),
        }
    }

    /// A shift by `semitones` (12 is an octave up).
    pub fn from_semitones(semitones: f64) -> Self {
        Self::new(2f64.powf(semitones / 12.0))
    }

    fn limit(ratio: f64) -> f64 {
        if ratio.is_nan() {
            1.0
        } else {
            ratio.clamp(MIN_PITCH_RATIO, MAX_PITCH_RATIO)
        }
    }

    /// Sets the ratio. Real-time safe.
    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = Self::limit(ratio);
    }

    /// Sets the shift in semitones. Real-time safe.
    pub fn set_semitones(&mut self, semitones: f64) {
        self.set_ratio(2f64.powf(semitones / 12.0));
    }

    /// The ratio.
    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Fills `peaks` with the bins louder than the two on either side.
    fn find_peaks(&mut self) {
        self.peaks.clear();
        let magnitude = &self.magnitude;
        let len = magnitude.len();
        for index in 0..len {
            let here = magnitude[index];
            let louder_than = |other: usize, strictly: bool| {
                let there = magnitude[other];
                if strictly {
                    here > there
                } else {
                    here >= there
                }
            };
            let left = (index.saturating_sub(2)..index).all(|other| louder_than(other, true));
            let right = (index + 1..len.min(index + 3)).all(|other| louder_than(other, false));
            if here > 0.0 && left && right {
                self.peaks.push(index);
            }
        }
    }
}

impl<S: Sample> SpectralProcessor<S> for PitchShift {
    fn prepare(&mut self, layout: &StftLayout) {
        let bins = layout.bins;
        self.bins = bins;
        self.hop = layout.hop as f64;
        self.hop_turn = TAU * layout.hop as f64 / layout.fft_size as f64;
        self.last_phase = vec![0.0; bins * layout.channels];
        self.turn = vec![0.0; bins * layout.channels];
        for scratch in [&mut self.next_turn, &mut self.magnitude, &mut self.phase] {
            *scratch = vec![0.0; bins];
        }
        self.peaks = Vec::with_capacity(bins);
        self.shifted = vec![Complex::ZERO; bins];
    }

    fn process_frame(&mut self, frame: SpectralFrame<'_, S>) {
        let bins = self.bins;
        let start = frame.channel * bins;
        if start + bins > self.last_phase.len() || frame.bins.len() != bins {
            return;
        }
        for ((bin, magnitude), phase) in frame
            .bins
            .iter()
            .zip(&mut self.magnitude)
            .zip(&mut self.phase)
        {
            let (re, im) = (bin.re.to_f64(), bin.im.to_f64());
            *magnitude = re.hypot(im);
            *phase = im.atan2(re);
        }
        let unchanged = self.ratio == 1.0;
        if !unchanged {
            self.find_peaks();
        }

        let last_phase = &mut self.last_phase[start..start + bins];
        let turn = &mut self.turn[start..start + bins];
        if unchanged {
            last_phase.copy_from_slice(&self.phase);
            turn.fill(0.0);
            return;
        }

        self.shifted.fill(Complex::ZERO);
        self.next_turn.fill(0.0);
        let mut region_start = 0;
        for (number, &peak) in self.peaks.iter().enumerate() {
            // The region runs to the quietest bin before the next peak.
            let region_end = match self.peaks.get(number + 1) {
                Some(&next) => (peak + 1..next)
                    .min_by(|&a, &b| self.magnitude[a].total_cmp(&self.magnitude[b]))
                    .unwrap_or(next),
                None => bins,
            };

            // The peak's true frequency, in radians per sample.
            let expected = self.hop_turn * peak as f64;
            let deviation = wrap_phase(self.phase[peak] - last_phase[peak] - expected);
            let frequency = (expected + deviation) / self.hop;

            // Moved by the whole number of bins nearest to how far the
            // frequency moves, so that the window's spectrum lands as close
            // as it can to where the new frequency belongs.
            let shift =
                (frequency * (self.ratio - 1.0) / self.hop_turn * self.hop).round() as isize;
            let target = peak.saturating_add_signed(shift);
            if target < bins {
                let rotation = wrap_phase(turn[target] + (self.ratio - 1.0) * frequency * self.hop);
                self.next_turn[target] = rotation;
                let rotate = Complex::<f64>::from_polar(1.0, rotation);
                for index in region_start..region_end {
                    let Some(to) = index.checked_add_signed(shift).filter(|&to| to < bins) else {
                        continue;
                    };
                    let from =
                        Complex::new(frame.bins[index].re.to_f64(), frame.bins[index].im.to_f64());
                    self.shifted[to] = self.shifted[to] + from * rotate;
                }
            }
            region_start = region_end;
        }

        last_phase.copy_from_slice(&self.phase);
        turn.copy_from_slice(&self.next_turn);
        for (bin, shifted) in frame.bins.iter_mut().zip(&self.shifted) {
            *bin = Complex::new(S::from_f64(shifted.re), S::from_f64(shifted.im));
        }
    }

    fn reset(&mut self) {
        self.last_phase.fill(0.0);
        self.turn.fill(0.0);
    }
}
