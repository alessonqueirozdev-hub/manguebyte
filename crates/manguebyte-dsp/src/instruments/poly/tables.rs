//! The poly synth's wavetables, built once from formulas and shared by
//! every voice.
//!
//! Each [`TableShape`] has [`FRAMES`] frames of up to [`HARMONICS`]
//! harmonics. A frame is stored as a mip-map of ten levels an octave
//! apart, level `n` holding the harmonics up to `512 >> n`, each sampled
//! sixteen times per period of its highest harmonic (and at least 512
//! times a cycle) and read with four-point Hermite interpolation.
//!
//! The voice runs at twice the sample rate, and that is why octaves are
//! enough here where [`Wavetable`](crate::Wavetable) keeps two levels per
//! octave: an oscillator plays the richest level whose highest harmonic is
//! below its own half sample rate, so as the pitch climbs through an
//! octave the highest harmonic it plays climbs from a quarter of the
//! internal rate to a half, that is from the top of the audible band (half
//! the output rate) to twice that. Nothing folds back, and everything up to
//! half the output rate is always there: what changes from level to level
//! is above it, where the voice's decimator takes it away.

use core::f64::consts::{PI, TAU};
use std::sync::OnceLock;

use super::super::common::Noise;
use super::params::TableShape;
use crate::fft::{Complex, RealFft};
use crate::numeric::limit;

/// How many frames a table has.
pub(super) const FRAMES: usize = 16;

/// The most harmonics a frame keeps.
pub(super) const HARMONICS: usize = 512;

/// How many levels a frame's mip-map has: 512 harmonics down to 1.
pub(super) const LEVELS: usize = 10;

/// Table samples per period of a level's highest harmonic. Hermite
/// interpolation keeps its images more than 100 dB below a sawtooth's
/// harmonics at this density (measured in the tests).
const SAMPLES_PER_PERIOD: usize = 16;

/// The shortest table stored for a level. The levels of few harmonics,
/// which high notes play, cost little memory, so they are stored more
/// densely: 64 samples a period of the eighth harmonic, for instance,
/// which keeps their images as far down as the rest's.
const MIN_SIZE: usize = 512;

/// Samples stored around each table so that interpolation never wraps: one
/// before, two after.
const GUARD: usize = 3;

/// The length of a frame drawn in the time domain before its harmonics are
/// taken: enough that what folds back from above harmonic 512 is more than
/// 70 dB down for the shapes drawn this way.
const DRAWN: usize = 16_384;

/// One level of a table: every frame band-limited to the same harmonics.
#[derive(Debug)]
struct Level {
    size: usize,
    /// Per frame, `size + GUARD` samples: the last sample, the table, then
    /// the first two again.
    data: Vec<f32>,
}

/// One wavetable: its frames, as a mip-map.
#[derive(Debug)]
pub(super) struct Table {
    levels: Vec<Level>,
}

/// The harmonics of one frame: `cos` and `sin` amplitudes, index 0 unused.
type Spectrum = Vec<(f64, f64)>;

impl Table {
    /// The level to play at `increment` cycles per sample: the richest
    /// whose highest harmonic stays below half the sample rate.
    #[inline]
    pub(super) fn level_for(increment: f64) -> usize {
        let top = increment.abs() * (2 * HARMONICS) as f64;
        if top < 1.0 {
            0
        } else if top.is_finite() {
            // 512 >> n harmonics fit while 512·increment < 2^(n-1).
            ((top.log2().floor() as usize) + 1).min(LEVELS - 1)
        } else {
            LEVELS - 1
        }
    }

    /// The table at `level`, `position` between its first frame (0) and
    /// its last (1), and `phase` in the cycle, from 0 up to 1.
    #[inline]
    pub(super) fn read(&self, level: usize, position: f64, phase: f64) -> f64 {
        self.reader(level, position).read(phase)
    }

    /// The table at `level` and `position`, set up to be read at many
    /// phases: what every unison voice of an oscillator reads in a sample.
    #[inline]
    pub(super) fn reader(&self, level: usize, position: f64) -> Reader<'_> {
        let level = &self.levels[level.min(LEVELS - 1)];
        let place = limit(position, 0.0, 1.0) * (FRAMES - 1) as f64;
        let frame = (place as usize).min(FRAMES - 2);
        let stride = level.size + GUARD;
        Reader {
            first: &level.data[frame * stride..][..stride],
            second: &level.data[(frame + 1) * stride..][..stride],
            blend: place - frame as f64,
            size: level.size as f64,
            last: level.size - 1,
        }
    }
}

/// One level of a table between two frames, ready to be read.
#[derive(Debug, Clone, Copy)]
pub(super) struct Reader<'a> {
    first: &'a [f32],
    second: &'a [f32],
    blend: f64,
    size: f64,
    last: usize,
}

impl Reader<'_> {
    /// The waveform at `phase`, from 0 up to 1.
    #[inline]
    pub(super) fn read(&self, phase: f64) -> f64 {
        let at = limit(phase, 0.0, 1.0) * self.size;
        let index = (at as usize).min(self.last);
        let t = at - index as f64;
        let first = hermite(&self.first[index..index + 4], t);
        if self.blend > 0.0 {
            let second = hermite(&self.second[index..index + 4], t);
            first + (second - first) * self.blend
        } else {
            first
        }
    }
}

impl Table {
    /// Builds a table from the spectra of its frames.
    fn from_spectra(spectra: &[Spectrum]) -> Self {
        let levels = (0..LEVELS)
            .map(|level| {
                let harmonics = HARMONICS >> level;
                let size = (SAMPLES_PER_PERIOD * harmonics).max(MIN_SIZE);
                let mut inverse = RealFft::<f64>::new(size);
                let mut bins = vec![Complex::<f64>::ZERO; inverse.spectrum_len()];
                let mut table = vec![0.0; size];
                let stride = size + GUARD;
                let mut data = vec![0.0f32; stride * spectra.len()];
                for (frame, spectrum) in spectra.iter().enumerate() {
                    bins.fill(Complex::ZERO);
                    // a·cos + b·sin of harmonic k is (size/2)·(a - i·b) in
                    // bin k of a transform that divides by its length.
                    let half = 0.5 * size as f64;
                    for (k, &(a, b)) in spectrum.iter().enumerate().take(harmonics + 1).skip(1) {
                        bins[k] = Complex::new(half * a, -half * b);
                    }
                    inverse.inverse(&bins, &mut table);
                    let out = &mut data[frame * stride..][..stride];
                    out[0] = table[size - 1] as f32;
                    for (slot, &value) in out[1..=size].iter_mut().zip(&table) {
                        *slot = value as f32;
                    }
                    out[size + 1] = table[0] as f32;
                    out[size + 2] = table[1] as f32;
                }
                Level { size, data }
            })
            .collect();
        Self { levels }
    }
}

/// Four-point Hermite (Catmull–Rom) interpolation at `t` between
/// `points[1]` and `points[2]`.
#[inline]
fn hermite(points: &[f32], t: f64) -> f64 {
    let [p0, p1, p2, p3] = [points[0], points[1], points[2], points[3]].map(f64::from);
    let c1 = 0.5 * (p2 - p0);
    let c2 = p0 - 2.5 * p1 + 2.0 * p2 - 0.5 * p3;
    let c3 = 0.5 * (p3 - p0) + 1.5 * (p1 - p2);
    ((c3 * t + c2) * t + c1) * t + p1
}

/// Every table, built the first time it is asked for. Building takes a
/// moment and allocates about 14 MB; a voice asks when it is made, never
/// on the audio thread.
pub(super) fn bank() -> &'static [Table] {
    static BANK: OnceLock<Vec<Table>> = OnceLock::new();
    BANK.get_or_init(|| {
        TableShape::ALL
            .iter()
            .map(|&shape| Table::from_spectra(&frames(shape)))
            .collect()
    })
}

/// The spectra of the frames of `shape`, each scaled so that its waveform
/// peaks at 1.
fn frames(shape: TableShape) -> Vec<Spectrum> {
    let mut spectra: Vec<Spectrum> = (0..FRAMES)
        .map(|frame| {
            let u = frame as f64 / (FRAMES - 1) as f64;
            let mut spectrum = match shape {
                TableShape::Basic => basic(u),
                TableShape::Harmonics => additive(|k| {
                    // A smooth low-pass at 1 to 512 harmonics.
                    let cutoff = (9.0 * u).exp2();
                    (0.0, saw(k) / (1.0 + (k as f64 / cutoff).powi(6)))
                }),
                TableShape::Sync => drawn(|t| {
                    let ratio = 1.0 + 7.0 * u;
                    2.0 * (ratio * t).fract() - 1.0
                }),
                TableShape::Fold => drawn(|t| {
                    let gain = 0.6 + 11.4 * u.powf(1.5);
                    (gain * (TAU * t).sin() + 0.5 * u).sin()
                }),
                TableShape::Vowel => vowel(u, &VOWELS, 1.0, 1.0),
                TableShape::Choir => vowel(u, &CHOIR, 1.5, 1.6),
                TableShape::Digital => drawn(|t| {
                    let steps = (FRAMES - frame) as f64;
                    ((TAU * t).sin() * steps).round() / steps
                }),
                TableShape::Glass => glass(u),
                TableShape::Organ => organ(u),
                TableShape::Metal => metal(u),
                TableShape::Spectral => spectral(frame),
                TableShape::Resonant => additive(|k| {
                    let centre = (1.0 + 7.0 * u).exp2();
                    let distance = 4.0 * (k as f64 / centre).log2();
                    (0.0, saw(k) * (1.0 + 8.0 / (1.0 + distance * distance)))
                }),
            };
            spectrum[0] = (0.0, 0.0);
            spectrum
        })
        .collect();
    for spectrum in &mut spectra {
        let peak = peak_of(spectrum);
        if peak > 0.0 {
            for pair in spectrum.iter_mut() {
                *pair = (pair.0 / peak, pair.1 / peak);
            }
        }
    }
    spectra
}

/// The sine amplitude of harmonic `k` of a falling sawtooth.
fn saw(k: usize) -> f64 {
    2.0 / (PI * k as f64)
}

/// A spectrum from the amplitudes `harmonic(k)` gives, as `(cos, sin)`.
fn additive(mut harmonic: impl FnMut(usize) -> (f64, f64)) -> Spectrum {
    (0..=HARMONICS)
        .map(|k| if k == 0 { (0.0, 0.0) } else { harmonic(k) })
        .collect()
}

/// The first harmonics of a waveform drawn in the time domain, `wave(t)`
/// for `t` from 0 up to 1.
fn drawn(wave: impl Fn(f64) -> f64) -> Spectrum {
    let input: Vec<f64> = (0..DRAWN).map(|n| wave(n as f64 / DRAWN as f64)).collect();
    let mut fft = RealFft::<f64>::new(DRAWN);
    let mut bins = vec![Complex::<f64>::ZERO; fft.spectrum_len()];
    fft.forward(&input, &mut bins);
    let scale = 2.0 / DRAWN as f64;
    (0..=HARMONICS)
        .map(|k| (scale * bins[k].re, -scale * bins[k].im))
        .collect()
}

/// The peak of the waveform of `spectrum`, from a drawing of it.
fn peak_of(spectrum: &Spectrum) -> f64 {
    let size = 4 * HARMONICS;
    let mut inverse = RealFft::<f64>::new(size);
    let mut bins = vec![Complex::<f64>::ZERO; inverse.spectrum_len()];
    let half = 0.5 * size as f64;
    for (k, &(a, b)) in spectrum.iter().enumerate().skip(1) {
        bins[k] = Complex::new(half * a, -half * b);
    }
    let mut wave = vec![0.0; size];
    inverse.inverse(&bins, &mut wave);
    wave.iter().fold(0.0f64, |peak, x| peak.max(x.abs()))
}

/// Sine, triangle, sawtooth and square, a third of the way apart, with
/// their spectra blended between.
fn basic(u: f64) -> Spectrum {
    let shape = |which: usize, k: usize| -> f64 {
        let kf = k as f64;
        match which {
            0 => f64::from(u8::from(k == 1)),
            1 if k % 2 == 1 => {
                let sign = if (k / 2) % 2 == 0 { 1.0 } else { -1.0 };
                sign * 8.0 / (PI * PI * kf * kf)
            }
            2 => saw(k),
            3 if k % 2 == 1 => 4.0 / (PI * kf),
            _ => 0.0,
        }
    };
    let place = u * 3.0;
    let from = (place as usize).min(2);
    let blend = place - from as f64;
    additive(|k| {
        (
            0.0,
            shape(from, k) * (1.0 - blend) + shape(from + 1, k) * blend,
        )
    })
}

/// A formant: its frequency in hertz, its bandwidth and its level.
type Formant = (f64, f64, f64);

/// The first three formants of a, e, i, o and u for a man's voice (G. E.
/// Peterson and H. L. Barney, "Control methods used in a study of the
/// vowels", JASA 1952), with bandwidths of the usual size.
pub(super) const VOWELS: [[Formant; 3]; 5] = [
    [
        (730.0, 90.0, 1.0),
        (1_090.0, 110.0, 0.5),
        (2_440.0, 160.0, 0.25),
    ],
    [
        (530.0, 90.0, 1.0),
        (1_840.0, 110.0, 0.5),
        (2_480.0, 160.0, 0.25),
    ],
    [
        (270.0, 90.0, 1.0),
        (2_290.0, 110.0, 0.5),
        (3_010.0, 160.0, 0.25),
    ],
    [
        (570.0, 90.0, 1.0),
        (840.0, 110.0, 0.5),
        (2_410.0, 160.0, 0.25),
    ],
    [
        (300.0, 90.0, 1.0),
        (870.0, 110.0, 0.5),
        (2_240.0, 160.0, 0.25),
    ],
];

/// The vowels of the choir table: oo, oh, ah, eh, with wider formants.
const CHOIR: [[Formant; 3]; 4] = [
    [
        (300.0, 140.0, 1.0),
        (870.0, 170.0, 0.4),
        (2_240.0, 240.0, 0.15),
    ],
    [
        (570.0, 140.0, 1.0),
        (840.0, 170.0, 0.45),
        (2_410.0, 240.0, 0.15),
    ],
    [
        (730.0, 140.0, 1.0),
        (1_090.0, 170.0, 0.5),
        (2_440.0, 240.0, 0.2),
    ],
    [
        (530.0, 140.0, 1.0),
        (1_840.0, 170.0, 0.45),
        (2_480.0, 240.0, 0.2),
    ],
];

/// Where `u` falls among `count` evenly spread anchors: the anchor before
/// it and how far towards the next.
fn among(u: f64, count: usize) -> (usize, f64) {
    let place = u * (count - 1) as f64;
    let from = (place as usize).min(count - 2);
    (from, place - from as f64)
}

/// A voice singing the vowels of `formants`, `u` moving through them: a
/// source falling `tilt` times 6 dB per octave, shaped by the formants as
/// sung on 130 Hz (their bandwidths times `width`), plus two higher
/// formants for presence. Every harmonic of the choir gets a phase of its
/// own (Schroeder's), which keeps the waveform's peaks low, as an ensemble
/// smooths the peaks of one voice.
fn vowel(u: f64, formants: &[[Formant; 3]], tilt: f64, width: f64) -> Spectrum {
    const SUNG_ON: f64 = 130.0;
    let (from, blend) = among(u, formants.len());
    let mut now = [(0.0, 0.0, 0.0); 3];
    for (slot, (a, b)) in now
        .iter_mut()
        .zip(formants[from].iter().zip(&formants[from + 1]))
    {
        // Frequencies move evenly in pitch, levels in decibels.
        *slot = (
            (a.0.ln() * (1.0 - blend) + b.0.ln() * blend).exp(),
            a.1 * (1.0 - blend) + b.1 * blend,
            a.2 * (1.0 - blend) + b.2 * blend,
        );
    }
    let extra = [(3_300.0, 250.0, 0.1), (3_750.0, 300.0, 0.07)];
    let schroeder = tilt > 1.0;
    additive(|k| {
        let frequency = k as f64 * SUNG_ON;
        let envelope: f64 = now
            .iter()
            .chain(&extra)
            .map(|&(centre, bandwidth, level)| {
                let detuning = 2.0 * (frequency - centre) / (bandwidth * width);
                level / (1.0 + detuning * detuning).sqrt()
            })
            .sum();
        let level = (0.02 + envelope) / (k as f64).powf(tilt);
        if schroeder {
            let phase = PI * (k * (k - 1)) as f64 / HARMONICS as f64;
            (level * phase.sin(), level * phase.cos())
        } else {
            (0.0, level)
        }
    })
}

/// A sine and partials on the Fibonacci harmonics, growing with `u`.
fn glass(u: f64) -> Spectrum {
    const PARTIALS: [usize; 9] = [2, 3, 5, 8, 13, 21, 34, 55, 89];
    let mut spectrum = additive(|k| (0.0, f64::from(u8::from(k == 1))));
    for (index, &k) in PARTIALS.iter().enumerate() {
        let level = (0.15 + 0.85 * u) * 0.78f64.powi(index as i32);
        spectrum[k].1 += level * (1.0 - 0.6 * (1.0 - u) * (index as f64 / 8.0));
    }
    spectrum
}

/// Drawbars on the harmonics of the 8' pitch (8', 4', 2⅔', 2', 1⅗', 1⅓',
/// 1'), from a flute through jazz and gospel registrations to full.
fn organ(u: f64) -> Spectrum {
    const HARMONIC: [usize; 7] = [1, 2, 3, 4, 5, 6, 8];
    const REGISTRATIONS: [[f64; 7]; 4] = [
        [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        [1.0, 0.7, 0.55, 0.0, 0.0, 0.0, 0.0],
        [1.0, 1.0, 0.8, 0.7, 0.0, 0.0, 0.5],
        [1.0, 1.0, 1.0, 1.0, 0.8, 0.8, 1.0],
    ];
    let (from, blend) = among(u, REGISTRATIONS.len());
    let mut spectrum = additive(|_| (0.0, 0.0));
    for (index, &k) in HARMONIC.iter().enumerate() {
        let level =
            REGISTRATIONS[from][index] * (1.0 - blend) + REGISTRATIONS[from + 1][index] * blend;
        spectrum[k].1 += level;
    }
    spectrum
}

/// A fundamental and three clusters of neighbouring harmonics, which beat
/// against each other, moving up with `u`.
fn metal(u: f64) -> Spectrum {
    let centre = 6.0 + 40.0 * u;
    let mut spectrum = additive(|k| (0.0, if k == 1 { 0.5 } else { 0.0 }));
    for (ratio, level) in [(1.0, 1.0), (2.3, 0.7), (3.7, 0.45)] {
        let middle = (centre * ratio).round() as usize;
        for (offset, weight) in [(-1i64, 0.6), (0, 1.0), (1, 0.6)] {
            let k = (middle as i64 + offset).clamp(1, HARMONICS as i64) as usize;
            spectrum[k].1 += level * weight;
        }
    }
    spectrum
}

/// Random harmonic levels and phases, falling gently, a new draw for every
/// frame (repeatable: the draw is seeded by the frame).
fn spectral(frame: usize) -> Spectrum {
    let mut random = Noise::for_note(frame as u8, 0.0, 77);
    additive(|k| {
        let level = random.unit().powi(2) / (k as f64).powf(0.8);
        let phase = TAU * random.unit();
        (level * phase.sin(), level * phase.cos())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_level_keeps_the_highest_harmonic_below_half_the_rate() {
        for increment in [1e-5, 0.0009, 0.001, 0.002, 0.01, 0.1, 0.24, 0.49, 2.0] {
            let level = Table::level_for(increment);
            let top = (HARMONICS >> level) as f64 * increment;
            if level < LEVELS - 1 {
                assert!(top < 0.5, "{increment}: level {level}, top {top}");
                // And it is the richest such level: the one before would
                // not fit, so the top is in the upper half.
                if level > 0 {
                    assert!(top >= 0.25, "{increment}: level {level}, top {top}");
                }
            }
        }
        assert_eq!(Table::level_for(0.0), 0);
        assert_eq!(Table::level_for(f64::NAN), LEVELS - 1);
    }

    #[test]
    fn every_frame_peaks_at_one_and_reads_back_its_harmonics() {
        let bank = bank();
        assert_eq!(bank.len(), TableShape::ALL.len());
        for (table, shape) in bank.iter().zip(TableShape::ALL) {
            for frame in [0, FRAMES / 2, FRAMES - 1] {
                let position = frame as f64 / (FRAMES - 1) as f64;
                let peak = (0..4_096)
                    .map(|n| table.read(0, position, n as f64 / 4_096.0).abs())
                    .fold(0.0, f64::max);
                assert!((0.97..=1.03).contains(&peak), "{shape:?} {frame}: {peak}");
            }
        }
        // The basic table starts as a sine: read between samples, it is
        // exact to the interpolation's error.
        let basic = &bank[TableShape::Basic.index()];
        let mut worst = 0.0f64;
        for n in 0..1_000 {
            let phase = (n as f64 + 0.37) / 1_000.0;
            let error = basic.read(0, 0.0, phase) - (TAU * phase).sin();
            worst = worst.max(error.abs());
        }
        assert!(worst < 1e-6, "{worst}");
        // Out-of-range positions and phases are clamped, not trusted.
        for value in [f64::NAN, -1.0, 2.0, f64::INFINITY] {
            assert!(basic.read(3, value, value).is_finite());
        }
    }

    #[test]
    fn the_frames_drawn_are_the_cycles_the_voice_reads() {
        for (table, shape) in bank().iter().zip(TableShape::ALL) {
            let frames = shape.frames(256);
            assert_eq!(frames.len(), FRAMES);
            for (index, frame) in frames.iter().enumerate() {
                assert_eq!(frame.len(), 256);
                let position = index as f64 / (FRAMES - 1) as f64;
                for (n, &sample) in frame.iter().enumerate() {
                    let read = table.read(0, position, n as f64 / 256.0) as f32;
                    assert_eq!(sample, read, "{shape:?} frame {index} sample {n}");
                }
            }
            // Between frames, a cycle is the blend of its neighbours.
            let between = shape.cycle(0.5 / (FRAMES - 1) as f64, 256);
            for (n, &sample) in between.iter().enumerate() {
                let mean = (frames[0][n] + frames[1][n]) / 2.0;
                assert!((sample - mean).abs() < 1e-5, "{shape:?} {n}");
            }
        }
        assert!(TableShape::Fold.cycle(0.3, 0).is_empty());
    }
}
