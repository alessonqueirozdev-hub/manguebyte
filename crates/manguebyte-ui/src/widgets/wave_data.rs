//! A sample buffer with a summary of it at several resolutions, so that
//! the lowest and highest sample of any run of frames is found without
//! reading the run.

use std::fmt;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// How many frames the finest level of the summary holds per entry, as a
/// power of two: 64.
const BASE_SHIFT: u32 = 6;

/// Frames per entry of the finest level.
const BASE: usize = 1 << BASE_SHIFT;

/// The furthest [`Waveform::zero_crossing_near`] looks either way, in
/// frames: a search never costs more than this many reads per channel.
pub const MAX_ZERO_SEARCH: u64 = 1 << 14;

/// One level of a channel's summary: the lowest and highest sample of each
/// block of frames, as `[low, high, low, high, ...]`.
type Level = Vec<f32>;

/// One level of a channel's energy: the sum of the squares of the samples
/// of each block of frames.
type Energy = Vec<f64>;

/// A sample buffer of one or more channels of `f32`, as a
/// [`waveform_view`](crate::waveform_view) shows it, with a summary of it
/// at several resolutions.
///
/// The summary keeps, for each channel, the lowest and highest sample and
/// the sum of the squares of the samples of every block of 64 frames, of
/// every block of 128, and so on up to one block for the whole buffer:
/// about a tenth of the buffer's size on top of it.
/// [`min_max`](Waveform::min_max) and [`rms`](Waveform::rms) of any run of
/// frames read at most 126 samples at its two ends and two entries per
/// level in between, whatever the length of the run, so a view of a
/// ten-minute file costs what a view of a short one costs. The lowest and
/// highest are exact: the same as reading every sample of the run; the
/// root mean square is summed in `f64` and is exact to far below what a
/// view shows.
///
/// Making the summary reads every sample once, so it belongs where a file
/// is loaded, not in a frame. The buffer and the summary never change
/// afterwards; a waveform is shared as an `Arc`, and is `Send` and `Sync`
/// so that it can be made on a loading thread.
///
/// ```
/// use manguebyte_ui::Waveform;
///
/// let wave = Waveform::new(vec![vec![0.0, 0.5, -0.25, 1.0, -1.0, 0.125]], 48_000.0);
/// assert_eq!(wave.len(), 6);
/// assert_eq!(wave.min_max(0, 1..4), Some((-0.25, 1.0)));
/// assert_eq!(wave.min_max(0, 3..3), None, "no frames, no answer");
/// ```
pub struct Waveform {
    name: String,
    channels: Vec<Arc<[f32]>>,
    len: usize,
    sample_rate: f64,
    /// For each channel, its levels from the finest.
    levels: Vec<Vec<Level>>,
    /// For each channel, its energy at the same levels.
    energy: Vec<Vec<Energy>>,
    /// How many samples and summary entries queries have read.
    reads: AtomicU64,
}

/// The lowest and highest of `low` and `high` and the pair `(a, b)`.
fn widen(low: &mut f32, high: &mut f32, a: f32, b: f32) {
    // `min` and `max` ignore a NaN, so a NaN sample counts for nothing.
    *low = low.min(a);
    *high = high.max(b);
}

/// The square of a sample, a NaN counting as silence.
fn square(sample: f32) -> f64 {
    if sample.is_nan() {
        0.0
    } else {
        f64::from(sample) * f64::from(sample)
    }
}

/// The energy of one channel at the levels [`summarise`] makes.
fn energies(samples: &[f32]) -> Vec<Energy> {
    let mut levels: Vec<Energy> = Vec::new();
    let finest: Energy = samples
        .chunks(BASE)
        .map(|block| block.iter().map(|&s| square(s)).sum())
        .collect();
    levels.push(finest);
    while levels.last().is_some_and(|level| level.len() > 1) {
        let below = levels.last().map(Vec::as_slice).unwrap_or_default();
        let level = below.chunks(2).map(|pair| pair.iter().sum()).collect();
        levels.push(level);
    }
    levels
}

/// The summary of one channel.
fn summarise(samples: &[f32]) -> Vec<Level> {
    let mut levels = Vec::new();
    let mut finest = Vec::with_capacity(samples.len().div_ceil(BASE) * 2);
    for block in samples.chunks(BASE) {
        let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
        for &sample in block {
            widen(&mut low, &mut high, sample, sample);
        }
        finest.extend([low, high]);
    }
    levels.push(finest);
    while levels.last().is_some_and(|level| level.len() > 2) {
        let below = levels.last().map(Vec::as_slice).unwrap_or_default();
        let mut level = Vec::with_capacity(below.len().div_ceil(4) * 2);
        for pair in below.chunks(4) {
            let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
            for entry in pair.chunks(2) {
                widen(&mut low, &mut high, entry[0], entry[1]);
            }
            level.extend([low, high]);
        }
        levels.push(level);
    }
    levels
}

impl Waveform {
    /// A waveform of `channels`, each a buffer of samples at
    /// `sample_rate` frames a second, with its summary made now. Channels
    /// shorter than the longest are read as silence past their end. A rate
    /// that is not a positive number counts as 48 000.
    pub fn new(channels: Vec<Vec<f32>>, sample_rate: f64) -> Waveform {
        Waveform::from_shared(channels.into_iter().map(Arc::from).collect(), sample_rate)
    }

    /// The same as [`new`](Waveform::new), from buffers shared with the
    /// rest of the application (with the audio thread, for one), which
    /// are not copied.
    pub fn from_shared(channels: Vec<Arc<[f32]>>, sample_rate: f64) -> Waveform {
        let len = channels
            .iter()
            .map(|channel| channel.len())
            .max()
            .unwrap_or(0);
        let levels = channels.iter().map(|channel| summarise(channel)).collect();
        let energy = channels.iter().map(|channel| energies(channel)).collect();
        Waveform {
            name: String::new(),
            channels,
            len,
            sample_rate: if sample_rate.is_finite() && sample_rate > 0.0 {
                sample_rate
            } else {
                48_000.0
            },
            levels,
            energy,
            reads: AtomicU64::new(0),
        }
    }

    /// A waveform of `channels` channels from `samples` interleaved frame
    /// by frame, as a WAV file holds them. A last frame that is not whole
    /// is dropped; no channels counts as one.
    pub fn from_interleaved(samples: &[f32], channels: usize, sample_rate: f64) -> Waveform {
        let channels = channels.max(1);
        let frames = samples.len() / channels;
        let split = (0..channels)
            .map(|channel| {
                (0..frames)
                    .map(|frame| samples[frame * channels + channel])
                    .collect()
            })
            .collect();
        Waveform::new(split, sample_rate)
    }

    /// The same waveform called `name`, such as the file's name: what the
    /// view says it shows.
    #[must_use]
    pub fn named(mut self, name: impl Into<String>) -> Waveform {
        self.name = name.into();
        self
    }

    /// What it is called; empty if it was given no name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many frames it has: the length of its longest channel.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether it has no frames.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many channels it has.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// The samples of `channel`, or none if there is no such channel.
    pub fn channel(&self, channel: usize) -> &[f32] {
        self.channels.get(channel).map_or(&[], |samples| samples)
    }

    /// Frames a second.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// How long it lasts, in seconds.
    pub fn seconds(&self) -> f64 {
        self.len as f64 / self.sample_rate
    }

    /// `frame` in seconds from the start.
    pub fn frame_to_seconds(&self, frame: u64) -> f64 {
        frame as f64 / self.sample_rate
    }

    /// How many samples and summary entries every query so far has read:
    /// what tests and benchmarks look at to see that drawing does not read
    /// the whole buffer.
    pub fn samples_read(&self) -> u64 {
        self.reads.load(Ordering::Relaxed)
    }

    fn count(&self, reads: u64) {
        self.reads.fetch_add(reads, Ordering::Relaxed);
    }

    /// The lowest and highest sample of `channel` over `frames`, or `None`
    /// if the run has no frames in the channel, or there is no such
    /// channel. Exact, and read from the summary (see [`Waveform`]).
    pub fn min_max(&self, channel: usize, frames: Range<usize>) -> Option<(f32, f32)> {
        self.summary(channel, frames)
            .map(|(low, high, _)| (low, high))
    }

    /// The root mean square of `channel` over `frames` (a NaN sample counts
    /// as silence), or `None` as for [`min_max`](Waveform::min_max). Read
    /// from the summary in the same way.
    ///
    /// ```
    /// use manguebyte_ui::Waveform;
    ///
    /// let wave = Waveform::new(vec![vec![0.5, -0.5, 0.5, -0.5]], 48_000.0);
    /// assert_eq!(wave.rms(0, 0..4), Some(0.5));
    /// ```
    pub fn rms(&self, channel: usize, frames: Range<usize>) -> Option<f32> {
        self.summary(channel, frames).map(|(_, _, rms)| rms)
    }

    /// The lowest and highest sample and the root mean square of `channel`
    /// over `frames`, read together: what one column of a view shows.
    pub fn summary(&self, channel: usize, frames: Range<usize>) -> Option<(f32, f32, f32)> {
        let samples = self.channels.get(channel)?;
        let levels = self.levels.get(channel)?;
        let energy = self.energy.get(channel)?;
        let mut start = frames.start;
        let mut end = frames.end.min(samples.len());
        if start >= end {
            return None;
        }
        let count = (end - start) as f64;
        let (mut low, mut high) = (f32::INFINITY, f32::NEG_INFINITY);
        let mut sum = 0.0f64;
        let mut reads = 0u64;
        let mut raw = |range: Range<usize>, low: &mut f32, high: &mut f32, sum: &mut f64| {
            reads += range.len() as u64;
            for &sample in &samples[range] {
                widen(low, high, sample, sample);
                *sum += square(sample);
            }
        };
        if end - start < 2 * BASE {
            raw(start..end, &mut low, &mut high, &mut sum);
        } else {
            // The ends that are not whole blocks, sample by sample.
            let first = start.next_multiple_of(BASE);
            let last = end - end % BASE;
            raw(start..first, &mut low, &mut high, &mut sum);
            raw(last..end, &mut low, &mut high, &mut sum);
            start = first >> BASE_SHIFT;
            end = last >> BASE_SHIFT;
            // The whole blocks in between, from the coarsest levels that fit,
            // as a segment tree is read from its leaves up.
            for (level, energy) in levels.iter().zip(energy) {
                if start >= end {
                    break;
                }
                if start % 2 == 1 {
                    widen(&mut low, &mut high, level[2 * start], level[2 * start + 1]);
                    sum += energy[start];
                    reads += 1;
                    start += 1;
                }
                if end % 2 == 1 {
                    end -= 1;
                    widen(&mut low, &mut high, level[2 * end], level[2 * end + 1]);
                    sum += energy[end];
                    reads += 1;
                }
                start /= 2;
                end /= 2;
            }
        }
        self.count(reads);
        let rms = (sum / count).sqrt() as f32;
        if low > high {
            // Every sample was NaN.
            Some((0.0, 0.0, 0.0))
        } else {
            Some((low, high, rms))
        }
    }

    /// The channels mixed at `frame`: their mean, silence past a channel's
    /// end.
    fn mix(&self, frame: usize) -> f32 {
        let count = self.channels.len().max(1) as f32;
        self.channels
            .iter()
            .map(|channel| channel.get(frame).copied().unwrap_or(0.0))
            .sum::<f32>()
            / count
    }

    /// Whether the mix changes sign between `frame - 1` and `frame`: a
    /// zero crossing at `frame`, the first frame of the new sign.
    fn crosses(&self, frame: usize) -> bool {
        frame > 0 && frame < self.len && (self.mix(frame - 1) < 0.0) != (self.mix(frame) < 0.0)
    }

    /// The zero crossing nearest `frame`, no further than `within` frames
    /// (and never further than [`MAX_ZERO_SEARCH`]) either way: a frame
    /// where the channels' mix changes sign, the first frame of the new
    /// sign. `None` if there is none that near. Of two at the same
    /// distance, the earlier.
    pub fn zero_crossing_near(&self, frame: u64, within: u64) -> Option<u64> {
        let within = within.min(MAX_ZERO_SEARCH) as usize;
        let frame = usize::try_from(frame).ok()?.min(self.len);
        let mut reads = 0u64;
        let mut found = None;
        for distance in 0..=within {
            reads += 4 * self.channels.len() as u64;
            if let Some(before) = frame.checked_sub(distance).filter(|&f| self.crosses(f)) {
                found = Some(before);
                break;
            }
            if distance > 0 && self.crosses(frame + distance) {
                found = Some(frame + distance);
                break;
            }
        }
        self.count(reads);
        found.map(|frame| frame as u64)
    }

    /// The first zero crossing after `frame` (`forward`) or before it, no
    /// further than `within` frames (and [`MAX_ZERO_SEARCH`]) away.
    pub fn zero_crossing_beyond(&self, frame: u64, forward: bool, within: u64) -> Option<u64> {
        let within = within.min(MAX_ZERO_SEARCH) as usize;
        let frame = usize::try_from(frame).ok()?.min(self.len);
        let mut reads = 0u64;
        let mut found = None;
        for distance in 1..=within {
            reads += 2 * self.channels.len() as u64;
            let at = if forward {
                Some(frame + distance)
            } else {
                frame.checked_sub(distance)
            };
            match at {
                Some(at) if self.crosses(at) => {
                    found = Some(at);
                    break;
                }
                Some(at) if at < self.len => {}
                _ => break,
            }
        }
        self.count(reads);
        found.map(|frame| frame as u64)
    }
}

impl fmt::Debug for Waveform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Waveform")
            .field("name", &self.name)
            .field("channels", &self.channels.len())
            .field("len", &self.len)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A noise of `len` samples from a fixed seed, exact on every platform.
    fn noise(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed.max(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                (state % 20_001) as f32 / 10_000.0 - 1.0
            })
            .collect()
    }

    fn scan(samples: &[f32], range: Range<usize>) -> Option<(f32, f32)> {
        let slice = samples.get(range)?;
        if slice.is_empty() {
            return None;
        }
        Some(
            slice
                .iter()
                .fold((f32::INFINITY, f32::NEG_INFINITY), |(l, h), &s| {
                    (l.min(s), h.max(s))
                }),
        )
    }

    #[test]
    fn every_run_agrees_with_reading_every_sample() {
        for len in [0, 1, 63, 64, 65, 127, 128, 1000, 4096, 10_007] {
            let samples = noise(len, len as u32 + 7);
            let wave = Waveform::new(vec![samples.clone()], 44_100.0);
            let mut state = 99u32;
            let mut next = |below: usize| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (state >> 8) as usize % below.max(1)
            };
            for _ in 0..400 {
                let a = next(len + 1);
                let b = next(len + 1);
                let range = a.min(b)..a.max(b);
                assert_eq!(
                    wave.min_max(0, range.clone()),
                    scan(&samples, range.clone()),
                    "{len}: {range:?}"
                );
            }
            assert_eq!(wave.min_max(0, 0..len), scan(&samples, 0..len));
        }
    }

    #[test]
    fn the_root_mean_square_agrees_with_reading_every_sample() {
        let samples = noise(50_000, 11);
        let wave = Waveform::new(vec![samples.clone()], 48_000.0);
        for (from, to) in [(0, 1), (3, 70), (17, 49_999), (64, 128), (1_000, 31_000)] {
            let exact = (samples[from..to]
                .iter()
                .map(|&s| f64::from(s) * f64::from(s))
                .sum::<f64>()
                / (to - from) as f64)
                .sqrt();
            let rms = f64::from(wave.rms(0, from..to).expect("frames"));
            assert!((rms - exact).abs() < 1e-5, "{from}..{to}: {rms} {exact}");
            // Read together with the lowest and highest, which stay exact.
            let (low, high, _) = wave.summary(0, from..to).expect("frames");
            assert_eq!(wave.min_max(0, from..to), Some((low, high)));
        }
        assert_eq!(wave.rms(0, 10..10), None);
    }

    #[test]
    fn a_long_run_reads_little() {
        let wave = Waveform::new(vec![noise(1 << 20, 3)], 48_000.0);
        let before = wave.samples_read();
        wave.min_max(0, 17..(1 << 20) - 5);
        let read = wave.samples_read() - before;
        assert!(read < 2 * BASE as u64 + 2 * 20, "{read}");
    }

    #[test]
    fn zero_crossings_are_where_the_mix_changes_sign() {
        let wave = Waveform::new(vec![vec![0.5, 0.2, -0.1, -0.4, 0.3, 0.0, -0.2]], 48_000.0);
        assert_eq!(wave.zero_crossing_near(0, 10), Some(2));
        assert_eq!(
            wave.zero_crossing_near(3, 10),
            Some(2),
            "nearest, earlier on a tie"
        );
        assert_eq!(wave.zero_crossing_near(5, 0), None);
        assert_eq!(wave.zero_crossing_beyond(2, true, 10), Some(4));
        assert_eq!(wave.zero_crossing_beyond(4, false, 10), Some(2));
        assert_eq!(wave.zero_crossing_beyond(6, true, 10), None);
    }
}
