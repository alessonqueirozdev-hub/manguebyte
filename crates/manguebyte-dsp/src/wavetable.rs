//! Wavetable synthesis.
//!
//! A [`Wavetable`] holds one or more single-cycle waveforms, its frames, of
//! any length. Building one splits every frame into harmonics with the FFT
//! and makes a mip-map: a ladder of copies, each with fewer harmonics than
//! the one before, two rungs per octave. A [`WavetableOscillator`] plays the
//! copy whose highest harmonic stays below half the sample rate, so the
//! waveform does not fold back into audible aliasing as the pitch rises.
//!
//! Building is slow and allocates, so it is done off the audio thread. The
//! table is shared through an [`Arc`], so any number of voices can play one
//! table without copying it.
//!
//! ```
//! use std::sync::Arc;
//!
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::{ProcessSpec, Processor, Wavetable, WavetableOscillator};
//!
//! // Two frames: a sawtooth and a square, 256 samples each.
//! let saw: Vec<f32> = (0..256).map(|n| 2.0 * n as f32 / 256.0 - 1.0).collect();
//! let square: Vec<f32> = (0..256).map(|n| if n < 128 { 1.0 } else { -1.0 }).collect();
//! let table = Arc::new(Wavetable::new(&[saw, square]).unwrap());
//!
//! let mut oscillator = WavetableOscillator::new(table, 220.0);
//! oscillator.set_position(0.5); // halfway from saw to square
//!
//! let spec = ProcessSpec { sample_rate: 48_000.0, max_block_size: 512, num_channels: 2 };
//! Processor::<f32>::prepare(&mut oscillator, &spec);
//! let mut block = AudioBuffer::<f32>::new(2, 512);
//! oscillator.process(&mut block);
//! assert!(block.peak() > 0.5);
//! ```

use core::fmt;
use std::sync::Arc;

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::fft::{Complex, Fft, RealFft};
use crate::{ProcessSpec, Processor};

/// The most harmonics a frame keeps. At 48 kHz this covers a full audio
/// band for fundamentals down to about 11.7 Hz.
pub const MAX_HARMONICS: usize = 2_048;

/// Table samples per period of the highest harmonic of a level, at least.
/// With the interpolation used, this keeps its own images low.
const SAMPLES_PER_TOP_PERIOD: usize = 16;

/// The shortest table stored for a level.
const MIN_TABLE: usize = 256;

/// Why a [`Wavetable`] could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WavetableError {
    /// No frames were given.
    NoFrames,
    /// A frame has no samples.
    EmptyFrame,
    /// The frames are not all the same length.
    FrameLengths {
        /// Length of the first frame.
        expected: usize,
        /// Length of the first frame that differs.
        found: usize,
    },
    /// A sample is not-a-number or infinite.
    NotFinite,
}

impl fmt::Display for WavetableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFrames => write!(f, "a wavetable needs at least one frame"),
            Self::EmptyFrame => write!(f, "a wavetable frame has no samples"),
            Self::FrameLengths { expected, found } => write!(
                f,
                "wavetable frames must have one length: {expected}, then {found}"
            ),
            Self::NotFinite => write!(f, "a wavetable sample is not finite"),
        }
    }
}

impl std::error::Error for WavetableError {}

/// One rung of the mip-map: every frame, band-limited to `harmonics`.
#[derive(Debug, Clone)]
struct Level {
    harmonics: usize,
    size: usize,
    /// Per frame, `size + GUARD` samples: the last two samples, the table,
    /// then the first three again, so interpolation never has to wrap.
    data: Vec<f32>,
}

/// Extra samples stored around each table for interpolation.
const GUARD: usize = 5;

impl Level {
    #[inline]
    fn read(&self, frame: usize, phase: f64) -> f64 {
        let stride = self.size + GUARD;
        let position = phase * self.size as f64;
        let index = (position as usize).min(self.size - 1);
        let fraction = position - index as f64;
        let points = &self.data[frame * stride + index..][..6];
        lagrange6(points, fraction)
    }
}

/// Fifth-order Lagrange interpolation at `t` between `points[2]` and
/// `points[3]`, through six neighbouring samples.
///
/// Its images fall off with the sixth power of how far the signal is below
/// the table's own Nyquist frequency, so a table with 16 samples per period
/// of its highest harmonic keeps them more than 100 dB down.
#[inline]
fn lagrange6(points: &[f32], t: f64) -> f64 {
    // Distances from the six nodes, at -2, -1, 0, 1, 2 and 3.
    let (a, b, c, d, e, f) = (t + 2.0, t + 1.0, t, t - 1.0, t - 2.0, t - 3.0);
    let ab = a * b;
    let de = d * e;
    let ef = e * f;
    let weights = [
        -(b * c * de * f) / 120.0,
        (a * c * de * f) / 24.0,
        -(ab * de * f) / 12.0,
        (ab * c * ef) / 12.0,
        -(ab * c * d * f) / 24.0,
        (ab * c * de) / 120.0,
    ];
    weights
        .iter()
        .zip(points)
        .map(|(&weight, &point)| weight * f64::from(point))
        .sum()
}

/// Single-cycle waveforms, band-limited into a mip-map for alias-free
/// playback.
///
/// Every frame is one period. Frames may be any length, but all frames of a
/// table share it. A frame of `n` samples holds harmonics up to `(n - 1) / 2`,
/// and at most [`MAX_HARMONICS`] are kept.
///
/// The levels are spaced half an octave apart, so where the harmonic counts
/// are large the oscillator plays everything up to between 70% and 100% of
/// half the sample rate. At the very top of the range, where only a few
/// harmonics fit, the steps between levels are coarser. Each level is stored
/// with at least 16 samples per period of its highest harmonic, and at least
/// 256 samples, and is read with fifth-order Lagrange interpolation.
///
/// Memory, in single-precision samples per frame: about 64 per harmonic of
/// the richest level, so 256 kB for a 2 048-sample sawtooth and 512 kB with
/// the full 2 048 harmonics.
#[derive(Debug, Clone)]
pub struct Wavetable {
    frame_len: usize,
    num_frames: usize,
    levels: Vec<Level>,
}

impl Wavetable {
    /// Builds a table from `frames`, each one a single period. Allocates and
    /// takes time; call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Fails if there are no frames, a frame is empty, the frames differ in
    /// length, or a sample is not finite.
    pub fn new<F: AsRef<[f32]>>(frames: &[F]) -> Result<Self, WavetableError> {
        let first = frames.first().ok_or(WavetableError::NoFrames)?.as_ref();
        let frame_len = first.len();
        if frame_len == 0 {
            return Err(WavetableError::EmptyFrame);
        }
        for frame in frames {
            let frame = frame.as_ref();
            if frame.len() != frame_len {
                return Err(WavetableError::FrameLengths {
                    expected: frame_len,
                    found: frame.len(),
                });
            }
            if frame.iter().any(|s| !s.is_finite()) {
                return Err(WavetableError::NotFinite);
            }
        }

        let top = ((frame_len - 1) / 2).min(MAX_HARMONICS);
        let ladder = harmonic_ladder(top);

        // The spectrum of every frame, scaled so that bin k is the complex
        // amplitude of harmonic k.
        let mut fft = Fft::<f64>::new(frame_len);
        let spectra: Vec<Vec<Complex<f64>>> = frames
            .iter()
            .map(|frame| {
                let mut data: Vec<Complex<f64>> = frame
                    .as_ref()
                    .iter()
                    .map(|&s| Complex::new(f64::from(s), 0.0))
                    .collect();
                fft.forward(&mut data);
                data.truncate(top + 1);
                data
            })
            .collect();

        let levels = ladder
            .into_iter()
            .map(|harmonics| {
                let size = (SAMPLES_PER_TOP_PERIOD * harmonics)
                    .next_power_of_two()
                    .max(MIN_TABLE);
                let mut inverse = RealFft::<f64>::new(size);
                let mut spectrum = vec![Complex::ZERO; inverse.spectrum_len()];
                let mut table = vec![0.0f64; size];
                let stride = size + GUARD;
                let mut data = vec![0.0f32; stride * frames.len()];
                let scale = size as f64 / frame_len as f64;
                for (frame, source) in spectra.iter().enumerate() {
                    spectrum.fill(Complex::ZERO);
                    for (bin, &value) in spectrum.iter_mut().zip(source).take(harmonics + 1) {
                        *bin = value.scale(scale);
                    }
                    inverse.inverse(&spectrum, &mut table);
                    let out = &mut data[frame * stride..][..stride];
                    out[0] = table[size - 2] as f32;
                    out[1] = table[size - 1] as f32;
                    for (slot, &value) in out[2..2 + size].iter_mut().zip(&table) {
                        *slot = value as f32;
                    }
                    out[size + 2] = table[0] as f32;
                    out[size + 3] = table[1] as f32;
                    out[size + 4] = table[2] as f32;
                }
                Level {
                    harmonics,
                    size,
                    data,
                }
            })
            .collect();

        Ok(Self {
            frame_len,
            num_frames: frames.len(),
            levels,
        })
    }

    /// The number of frames.
    pub fn num_frames(&self) -> usize {
        self.num_frames
    }

    /// The length of a frame as given, in samples.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// The number of levels in the mip-map.
    pub fn num_levels(&self) -> usize {
        self.levels.len()
    }

    /// The highest harmonic kept at `level`, or zero past the last level.
    pub fn harmonics(&self, level: usize) -> usize {
        self.levels.get(level).map_or(0, |level| level.harmonics)
    }

    /// The level to play at `increment` cycles per sample (the frequency over
    /// the sample rate): the richest one whose highest harmonic is below half
    /// the sample rate.
    pub fn level_for(&self, increment: f64) -> usize {
        let increment = if increment.is_finite() {
            increment.abs()
        } else {
            0.5
        };
        self.levels
            .iter()
            .position(|level| level.harmonics as f64 * increment < 0.5)
            .unwrap_or(self.levels.len() - 1)
    }

    /// Reads the table at `level`, at `position` between frames (`0.0` is the
    /// first frame, `1.5` halfway from the second to the third) and at
    /// `phase` in the cycle, `0.0..1.0`. Out-of-range arguments are clamped
    /// or wrapped. Real-time safe.
    pub fn read(&self, level: usize, position: f64, phase: f64) -> f64 {
        let level = &self.levels[level.min(self.levels.len() - 1)];
        let phase = wrap_phase(phase);
        let last = (self.num_frames - 1) as f64;
        let position = if position.is_nan() {
            0.0
        } else {
            position.clamp(0.0, last)
        };
        let frame = position as usize;
        let fraction = position - frame as f64;
        let here = level.read(frame, phase);
        if fraction > 0.0 {
            let next = level.read(frame + 1, phase);
            here + (next - here) * fraction
        } else {
            here
        }
    }
}

/// The harmonic counts of the mip-map, half an octave apart, from `top` down
/// to a single harmonic.
fn harmonic_ladder(top: usize) -> Vec<usize> {
    let mut ladder = vec![top];
    let mut step = 1;
    while *ladder.last().unwrap_or(&0) > 1 {
        let next = (top as f64 * 0.5f64.powf(f64::from(step) / 2.0)).floor() as usize;
        let next = next.max(1);
        if next < *ladder.last().unwrap_or(&0) {
            ladder.push(next);
        }
        step += 1;
    }
    ladder
}

/// A phase brought into `0.0..1.0`; not-a-number and infinity become zero.
#[inline]
fn wrap_phase(phase: f64) -> f64 {
    if !phase.is_finite() {
        return 0.0;
    }
    let wrapped = phase.rem_euclid(1.0);
    // A tiny negative phase rounds up to exactly one.
    if wrapped < 1.0 { wrapped } else { 0.0 }
}

/// An oscillator that plays a [`Wavetable`].
///
/// - The pitch picks the mip-map level, so the waveform stays band-limited
///   across the keyboard.
/// - The position morphs between frames, with an optional glide so that
///   moving it does not click.
/// - Phase modulation: as a [`Processor`], it reads the first channel of the
///   block it is given as a phase offset in cycles, scaled by the depth set
///   with [`set_phase_modulation`](Self::set_phase_modulation), before
///   overwriting every channel with its output. Put another oscillator before
///   it in a chain for FM-style sounds. The depth is zero by default, so the
///   input is ignored. The level is chosen from the base pitch; deep
///   modulation spreads the spectrum and can alias.
///
/// Its output is the same, bit for bit, however the audio is cut into blocks.
#[derive(Debug, Clone)]
pub struct WavetableOscillator {
    table: Arc<Wavetable>,
    frequency: f64,
    sample_rate: f64,
    phase: f64,
    increment: f64,
    level: usize,
    position: Smoothed<f64>,
    glide_ms: f64,
    depth: f64,
}

impl WavetableOscillator {
    /// Creates an oscillator playing the first frame of `table`. Until
    /// [`prepare`](Processor::prepare) or
    /// [`set_sample_rate`](Self::set_sample_rate) is called it assumes
    /// 44.1 kHz.
    pub fn new(table: Arc<Wavetable>, frequency: f64) -> Self {
        let mut oscillator = Self {
            table,
            frequency,
            sample_rate: 44_100.0,
            phase: 0.0,
            increment: 0.0,
            level: 0,
            position: Smoothed::new(0.0),
            glide_ms: 0.0,
            depth: 0.0,
        };
        oscillator.update();
        oscillator
    }

    /// Changes the frequency in hertz. The phase carries on. Frequencies are
    /// held between zero and half the sample rate; not-a-number is zero.
    pub fn set_frequency(&mut self, frequency: f64) {
        self.frequency = frequency;
        self.update();
    }

    /// The frequency in hertz.
    pub fn frequency(&self) -> f64 {
        self.frequency
    }

    /// Changes the sample rate in hertz.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.position
            .set_ramp(self.sample_rate, self.glide_ms.max(0.0) * 1e-3);
        self.update();
    }

    /// Moves between frames: `0.0` is the first, `1.0` the second, and values
    /// between blend the two. Clamped to the frames there are.
    pub fn set_position(&mut self, position: f64) {
        let last = (self.table.num_frames() - 1) as f64;
        let position = if position.is_nan() {
            0.0
        } else {
            position.clamp(0.0, last)
        };
        self.position.set_target(position);
    }

    /// The frame position being played or approached.
    pub fn position(&self) -> f64 {
        self.position.target()
    }

    /// Sets how long a change of position takes, in milliseconds. Zero, the
    /// default, applies it at once.
    pub fn set_position_glide_ms(&mut self, milliseconds: f64) {
        self.glide_ms = if milliseconds.is_finite() {
            milliseconds.max(0.0)
        } else {
            0.0
        };
        self.position
            .set_ramp(self.sample_rate, self.glide_ms * 1e-3);
    }

    /// Sets how strongly the input modulates the phase: the input sample
    /// times `depth` is added to the phase, in cycles.
    pub fn set_phase_modulation(&mut self, depth: f64) {
        self.depth = if depth.is_finite() { depth } else { 0.0 };
    }

    /// Restarts the cycle from the beginning.
    pub fn reset_phase(&mut self) {
        self.phase = 0.0;
    }

    /// Plays `table` from now on and returns the one it replaces. The phase
    /// carries on and the position is clamped to the new table's frames.
    ///
    /// No memory is freed here: the old table comes back to the caller, so
    /// the audio thread can pass it elsewhere to be dropped.
    pub fn set_table(&mut self, table: Arc<Wavetable>) -> Arc<Wavetable> {
        let old = core::mem::replace(&mut self.table, table);
        let last = (self.table.num_frames() - 1) as f64;
        if self.position.target() > last || self.position.current() > last {
            self.position.set_immediate(last);
        }
        self.update();
        old
    }

    /// The table being played.
    pub fn table(&self) -> &Arc<Wavetable> {
        &self.table
    }

    /// The mip-map level being played.
    pub fn level(&self) -> usize {
        self.level
    }

    /// Produces the next sample.
    #[inline]
    pub fn next_sample(&mut self) -> f64 {
        self.next_sample_with(0.0)
    }

    /// Produces the next sample with `offset` cycles added to the phase.
    #[inline]
    pub fn next_sample_with(&mut self, offset: f64) -> f64 {
        let position = self.position.next_value();
        let phase = if offset == 0.0 {
            self.phase
        } else {
            wrap_phase(self.phase + offset)
        };
        let value = self.table.read(self.level, position, phase);
        self.phase = wrap_phase(self.phase + self.increment);
        value
    }

    fn update(&mut self) {
        let increment = self.frequency / self.sample_rate;
        self.increment = if increment.is_nan() {
            0.0
        } else {
            increment.clamp(0.0, 0.5)
        };
        self.level = self.table.level_for(self.increment);
    }
}

impl<S: Sample> Processor<S> for WavetableOscillator {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.set_sample_rate(spec.sample_rate);
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        if buffer.num_channels() == 0 {
            return;
        }
        let depth = self.depth;
        for sample in buffer.channel_mut(0) {
            let offset = if depth == 0.0 {
                0.0
            } else {
                sample.to_f64() * depth
            };
            *sample = S::from_f64(self.next_sample_with(offset));
        }
        for index in 1..buffer.num_channels() {
            let (first, other) = buffer.channel_pair_mut(0, index);
            other.copy_from_slice(first);
        }
    }

    fn reset(&mut self) {
        self.reset_phase();
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::window::{Symmetry, Window};

    const RATE: f64 = 48_000.0;

    fn sine_frame(len: usize, harmonic: f64) -> Vec<f32> {
        (0..len)
            .map(|n| (TAU * harmonic * n as f64 / len as f64).sin() as f32)
            .collect()
    }

    fn saw_frame(len: usize) -> Vec<f32> {
        (0..len)
            .map(|n| (2.0 * n as f64 / len as f64 - 1.0) as f32)
            .collect()
    }

    fn oscillator(table: Wavetable, frequency: f64) -> WavetableOscillator {
        let mut oscillator = WavetableOscillator::new(Arc::new(table), frequency);
        oscillator.set_sample_rate(RATE);
        oscillator
    }

    #[test]
    fn a_sine_of_any_table_length_plays_back_as_a_sine() {
        for len in [3, 7, 100, 1_000, 2_048, 4_801] {
            let mut oscillator =
                oscillator(Wavetable::new(&[sine_frame(len, 1.0)]).unwrap(), 440.0);
            for n in 0..2_000 {
                let expected = (TAU * 440.0 * n as f64 / RATE).sin();
                let got = oscillator.next_sample();
                assert!(
                    (got - expected).abs() < 2e-6,
                    "length {len}, sample {n}: {got}"
                );
            }
        }
    }

    #[test]
    fn the_ladder_is_half_an_octave_per_level() {
        assert_eq!(harmonic_ladder(0), [0]);
        assert_eq!(harmonic_ladder(1), [1]);
        assert_eq!(harmonic_ladder(8), [8, 5, 4, 2, 1]);
        let table = Wavetable::new(&[saw_frame(2_048)]).unwrap();
        assert_eq!(table.harmonics(0), 1_023);
        assert_eq!(table.harmonics(1), 723);
        assert_eq!(table.harmonics(2), 511);
        assert_eq!(table.harmonics(table.num_levels() - 1), 1);
        assert_eq!(table.harmonics(99), 0);

        // At every pitch, the highest harmonic played is under half the
        // sample rate and above 70% of it.
        for note in 0..128 {
            let frequency = 440.0 * 2f64.powf((f64::from(note) - 69.0) / 12.0);
            let increment = frequency / RATE;
            let level = table.level_for(increment);
            let top = table.harmonics(level) as f64 * frequency;
            assert!(top < RATE / 2.0, "note {note}");
            if level > 0 && table.harmonics(level) >= 64 {
                assert!(top > 0.69 * RATE / 2.0, "note {note}: {top} Hz");
            }
        }
        // A frame too short for harmonics is a constant.
        let flat = Wavetable::new(&[[0.25f32, 0.25]]).unwrap();
        assert_eq!(flat.num_levels(), 1);
        assert!((flat.read(0, 0.0, 0.3) - 0.25).abs() < 1e-7);
    }

    /// Bins on each side of a harmonic that count as that harmonic. The
    /// Kaiser window with beta 20 has a main lobe about 6.5 bins wide on
    /// each side and leaks less than -150 dB beyond it.
    const HARMONIC_BINS: f64 = 8.0;

    /// Harmonics are at least this many bins apart in the analysis, so the
    /// bins counted as harmonic are at most 17 in 128, about 13% of the
    /// spectrum, at every pitch.
    const MIN_SPACING_BINS: f64 = 128.0;

    /// The analysis length for a fundamental of `frequency`: a power of two
    /// from 65 536 up, long enough for [`MIN_SPACING_BINS`].
    fn analysis_len(frequency: f64) -> usize {
        let needed = (MIN_SPACING_BINS * RATE / frequency).ceil() as usize;
        needed.next_power_of_two().max(65_536)
    }

    /// The share of the output's energy that is not at a harmonic of
    /// `frequency`, in decibels, and the share of the bins counted as
    /// harmonic.
    fn inharmonic_db(signal: &[f64], frequency: f64) -> (f64, f64) {
        let len = signal.len();
        let window = Window::Kaiser(20.0);
        let windowed: Vec<f64> = signal
            .iter()
            .enumerate()
            .map(|(n, &s)| s * window.value(n, len, Symmetry::Periodic))
            .collect();
        let mut fft = RealFft::<f64>::new(len);
        let mut spectrum = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(&windowed, &mut spectrum);

        let bin_width = RATE / len as f64;
        let near_harmonic = |bin: usize| {
            let hertz = bin as f64 * bin_width;
            let nearest = (hertz / frequency).round() * frequency;
            (hertz - nearest).abs() <= HARMONIC_BINS * bin_width
        };
        let (mut total, mut stray, mut counted) = (0.0, 0.0, 0usize);
        for (bin, value) in spectrum.iter().enumerate() {
            let power = value.norm_sqr();
            total += power;
            if near_harmonic(bin) {
                counted += 1;
            } else {
                stray += power;
            }
        }
        (
            10.0 * (stray / total).log10(),
            counted as f64 / spectrum.len() as f64,
        )
    }

    /// `len` samples of a sawtooth at `frequency` made of exactly the
    /// harmonics below half the sample rate, computed in double precision.
    fn ideal_signal(frequency: f64, len: usize) -> Vec<f64> {
        let harmonics = ((RATE / 2.0) / frequency).ceil() as usize - 1;
        let increment = frequency / RATE;
        (0..len)
            .map(|n| ideal_saw((n as f64 * increment).fract(), harmonics))
            .collect()
    }

    /// Keys from the bottom of a piano to the top, a few semitones apart.
    fn keyboard() -> impl Iterator<Item = f64> {
        (21..=108)
            .step_by(5)
            .chain([105, 108])
            .map(|note| 440.0 * 2f64.powf((f64::from(note) - 69.0) / 12.0))
    }

    #[test]
    fn the_measurement_floor_is_far_below_the_limit() {
        // A sawtooth with no aliasing at all, at the bottom and the top of
        // the keyboard: what the measurement reads is its own floor.
        for frequency in [27.5, 440.0, 4_186.0] {
            let len = analysis_len(frequency);
            let (floor, share) = inharmonic_db(&ideal_signal(frequency, len), frequency);
            eprintln!("measurement floor at {frequency} Hz: {floor:.1} dB");
            assert!(floor < -150.0, "{frequency} Hz: floor {floor:.1} dB");
            assert!(share < 0.14, "{frequency} Hz: {share}");
        }
    }

    #[test]
    fn a_sawtooth_stays_free_of_aliasing_across_the_keyboard() {
        let table = Arc::new(Wavetable::new(&[saw_frame(2_048)]).unwrap());
        let mut worst = f64::NEG_INFINITY;
        for frequency in keyboard() {
            let mut oscillator = WavetableOscillator::new(Arc::clone(&table), frequency);
            oscillator.set_sample_rate(RATE);
            let len = analysis_len(frequency);
            let signal: Vec<f64> = (0..len).map(|_| oscillator.next_sample()).collect();
            let (level, share) = inharmonic_db(&signal, frequency);
            assert!(level < -125.0, "{frequency:.1} Hz: {level:.1} dB");
            assert!(share < 0.14, "{frequency:.1} Hz: {share}");
            worst = worst.max(level);
        }
        eprintln!("worst inharmonic energy across the keyboard: {worst:.1} dB");
    }

    /// A sawtooth made of exactly `harmonics` harmonics, as a frame.
    fn additive_saw(len: usize, harmonics: usize) -> Vec<f32> {
        (0..len)
            .map(|n| ideal_saw(n as f64 / len as f64, harmonics) as f32)
            .collect()
    }

    fn ideal_saw(phase: f64, harmonics: usize) -> f64 {
        (1..=harmonics)
            .map(|k| -(TAU * k as f64 * phase).sin() / (core::f64::consts::FRAC_PI_2 * k as f64))
            .sum()
    }

    #[test]
    fn the_output_matches_the_ideal_band_limited_sawtooth() {
        // With a frame whose harmonics are known exactly, the output can be
        // compared with the sum of the harmonics the level keeps. The
        // difference holds everything that is wrong: aliasing, interpolation
        // images and droop, and rounding.
        let table = Arc::new(Wavetable::new(&[additive_saw(2_048, 1_023)]).unwrap());
        let mut worst = f64::NEG_INFINITY;
        for frequency in keyboard() {
            let mut oscillator = WavetableOscillator::new(Arc::clone(&table), frequency);
            oscillator.set_sample_rate(RATE);
            let harmonics = table.harmonics(oscillator.level());
            let increment = frequency / RATE;
            let (mut error, mut power) = (0.0, 0.0);
            for n in 0..4_096 {
                let expected = ideal_saw((n as f64 * increment).fract(), harmonics);
                let got = oscillator.next_sample();
                error += (got - expected) * (got - expected);
                power += expected * expected;
            }
            let level = 10.0 * (error / power).log10();
            assert!(level < -120.0, "{frequency:.1} Hz: {level:.1} dB");
            worst = worst.max(level);
        }
        eprintln!("worst error against the ideal sawtooth: {worst:.1} dB");
    }

    #[test]
    fn without_the_mip_map_the_same_sawtooth_aliases() {
        // Reading the richest level at every pitch is what a naive table
        // lookup does. The measurement must catch it at the top of the
        // keyboard, where most harmonics fold back, and at the bottom, where
        // only the few above 24 kHz do, among closely spaced harmonics.
        let table = Wavetable::new(&[saw_frame(2_048)]).unwrap();
        for (frequency, above) in [(2_093.0, -40.0), (27.5, -50.0)] {
            let increment = frequency / RATE;
            let len = analysis_len(frequency);
            let signal: Vec<f64> = (0..len)
                .map(|n| table.read(0, 0.0, (n as f64 * increment).fract()))
                .collect();
            let (level, _) = inharmonic_db(&signal, frequency);
            eprintln!("naive lookup at {frequency} Hz: {level:.1} dB");
            assert!(level > above, "{frequency} Hz: {level:.1} dB");
        }
    }

    #[test]
    fn the_position_morphs_between_frames() {
        let up = sine_frame(512, 1.0);
        let down: Vec<f32> = up.iter().map(|s| -s).collect();
        let double = sine_frame(512, 2.0);
        let table = Wavetable::new(&[up, down, double]).unwrap();
        assert_eq!(table.num_frames(), 3);
        for phase in [0.1, 0.25, 0.7] {
            let sine = (TAU * phase).sin();
            assert!((table.read(0, 0.0, phase) - sine).abs() < 1e-6);
            assert!((table.read(0, 1.0, phase) + sine).abs() < 1e-6);
            assert!(table.read(0, 0.5, phase).abs() < 1e-6);
            assert!((table.read(0, 0.25, phase) - 0.5 * sine).abs() < 1e-6);
            let blend = -0.5 * sine + 0.5 * (TAU * 2.0 * phase).sin();
            assert!((table.read(0, 1.5, phase) - blend).abs() < 1e-6);
            // Beyond the last frame, the last frame.
            assert!((table.read(0, 9.0, phase) - (TAU * 2.0 * phase).sin()).abs() < 1e-6);
            assert!((table.read(0, f64::NAN, phase) - sine).abs() < 1e-6);
        }
    }

    #[test]
    fn a_position_glide_is_linear() {
        let table = Wavetable::new(&[[1.0f32], [0.0]]).unwrap();
        let mut oscillator = oscillator(table, 100.0);
        oscillator.set_position_glide_ms(1_000.0 * 4.0 / RATE); // four samples
        oscillator.set_position(1.0);
        let out: Vec<f64> = (0..6).map(|_| oscillator.next_sample()).collect();
        let expected = [0.75, 0.5, 0.25, 0.0, 0.0, 0.0];
        for (got, want) in out.iter().zip(expected) {
            assert!((got - want).abs() < 1e-12, "{out:?}");
        }
        assert_eq!(oscillator.position(), 1.0);
        oscillator.set_position(-3.0);
        assert_eq!(oscillator.position(), 0.0);
    }

    #[test]
    fn phase_modulation_shifts_the_cycle() {
        let table = Arc::new(Wavetable::new(&[sine_frame(1_024, 1.0)]).unwrap());
        let mut oscillator = WavetableOscillator::new(Arc::clone(&table), 300.0);
        Processor::<f64>::prepare(
            &mut oscillator,
            &ProcessSpec {
                sample_rate: RATE,
                max_block_size: 256,
                num_channels: 2,
            },
        );
        oscillator.set_phase_modulation(0.5);
        let mut buffer = AudioBuffer::<f64>::new(2, 256);
        // An input of 0.5 at depth 0.5 is a quarter cycle: a cosine.
        buffer.channel_mut(0).fill(0.5);
        oscillator.process(&mut buffer);
        for n in 0..256 {
            let expected = (TAU * 300.0 * n as f64 / RATE).cos();
            assert!((buffer.channel(0)[n] - expected).abs() < 2e-6, "{n}");
            assert_eq!(buffer.channel(1)[n], buffer.channel(0)[n]);
        }
        // At depth zero the input is ignored.
        oscillator.set_phase_modulation(0.0);
        oscillator.reset_phase();
        buffer.channel_mut(0).fill(0.5);
        oscillator.process(&mut buffer);
        assert!(buffer.channel(0)[0].abs() < 1e-7);
    }

    #[test]
    fn the_output_does_not_depend_on_block_sizes() {
        let table = Arc::new(Wavetable::new(&[saw_frame(600), sine_frame(600, 3.0)]).unwrap());
        let modulator: Vec<f64> = (0..5_000).map(|n| (n as f64 * 0.01).sin()).collect();
        let mut reference: Option<Vec<u64>> = None;
        for block in [1, 64, 333, 5_000] {
            let mut oscillator = WavetableOscillator::new(Arc::clone(&table), 1_234.5);
            Processor::<f64>::prepare(
                &mut oscillator,
                &ProcessSpec {
                    sample_rate: RATE,
                    max_block_size: block,
                    num_channels: 1,
                },
            );
            oscillator.set_phase_modulation(0.3);
            oscillator.set_position_glide_ms(20.0);
            oscillator.set_position(0.8);
            let mut signal = AudioBuffer::<f64>::new(1, 5_000);
            signal.channel_mut(0).copy_from_slice(&modulator);
            crate::testing::run_in_blocks(&mut oscillator, &mut signal, block);
            let bits: Vec<u64> = signal.channel(0).iter().map(|s| s.to_bits()).collect();
            match &reference {
                None => reference = Some(bits),
                Some(expected) => assert!(*expected == bits, "blocks of {block}"),
            }
        }
    }

    #[test]
    fn swapping_tables_keeps_the_phase_and_returns_the_old_table() {
        let sine = Arc::new(Wavetable::new(&[sine_frame(256, 1.0)]).unwrap());
        let negated: Vec<f32> = sine_frame(256, 1.0).iter().map(|s| -s).collect();
        let minus = Arc::new(Wavetable::new(&[negated]).unwrap());
        let mut oscillator = WavetableOscillator::new(Arc::clone(&sine), 1_000.0);
        oscillator.set_sample_rate(RATE);
        for _ in 0..10 {
            oscillator.next_sample();
        }
        let old = oscillator.set_table(Arc::clone(&minus));
        assert!(Arc::ptr_eq(&old, &sine));
        let expected = -(TAU * 1_000.0 * 10.0 / RATE).sin();
        assert!((oscillator.next_sample() - expected).abs() < 2e-6);
        assert!(Arc::ptr_eq(oscillator.table(), &minus));
    }

    #[test]
    fn hostile_settings_give_finite_output() {
        let table = Arc::new(Wavetable::new(&[saw_frame(64), sine_frame(64, 1.0)]).unwrap());
        let values = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1e300,
            1e300,
            -5.0,
            0.0,
        ];
        let mut oscillator = WavetableOscillator::new(table, 440.0);
        Processor::<f32>::prepare(
            &mut oscillator,
            &ProcessSpec {
                sample_rate: RATE,
                max_block_size: 64,
                num_channels: 1,
            },
        );
        let mut buffer = AudioBuffer::<f32>::new(1, 64);
        for &a in &values {
            for &b in &values {
                oscillator.set_frequency(a);
                oscillator.set_position(b);
                oscillator.set_phase_modulation(a);
                oscillator.set_position_glide_ms(b);
                buffer.channel_mut(0).fill(b as f32);
                oscillator.process(&mut buffer);
                assert!(
                    buffer
                        .channel(0)
                        .iter()
                        .all(|s| s.is_finite() && s.abs() < 2.0)
                );
                assert!(oscillator.next_sample_with(a).is_finite());
            }
        }
        assert_eq!(wrap_phase(-1e-30), 0.0);
        assert_eq!(wrap_phase(2.25), 0.25);
    }

    #[test]
    fn bad_tables_are_refused() {
        let none: [Vec<f32>; 0] = [];
        assert_eq!(Wavetable::new(&none).unwrap_err(), WavetableError::NoFrames);
        assert_eq!(
            Wavetable::new(&[Vec::<f32>::new()]).unwrap_err(),
            WavetableError::EmptyFrame
        );
        assert_eq!(
            Wavetable::new(&[vec![0.0f32; 4], vec![0.0; 5]]).unwrap_err(),
            WavetableError::FrameLengths {
                expected: 4,
                found: 5
            }
        );
        assert_eq!(
            Wavetable::new(&[[0.0f32, f32::NAN]]).unwrap_err(),
            WavetableError::NotFinite
        );
        assert!(!WavetableError::NoFrames.to_string().is_empty());
    }
}
