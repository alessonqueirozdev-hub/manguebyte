use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, Sample};

use crate::{ProcessSpec, Processor};

/// A look-ahead peak limiter: nothing comes out above the ceiling.
///
/// A compressor reacts after a peak has started, so its first samples get
/// through. A limiter cannot allow that. This one delays the audio by a few
/// milliseconds and uses the time to see each peak coming, so the gain is
/// already down when the peak arrives and comes down smoothly rather than
/// in a step.
///
/// The gain is worked out in three stages:
///
/// 1. the gain each sample would need to stay under the ceiling, with an
///    exponential recovery so the gain rises gently after a peak;
/// 2. the smallest of those over the look-ahead window;
/// 3. the average of that minimum over the same window, which turns its
///    steps into ramps.
///
/// The average of minima that all cover a sample cannot exceed what that
/// sample needs, so the ceiling holds for every sample, by construction.
/// The channels share one gain, which keeps the stereo image still. A
/// sample that is not a finite number (a NaN or an infinity) comes out as
/// silence and does not move the gain, so the ceiling holds for any input.
///
/// The delay is reported by [`latency_samples`](Processor::latency_samples).
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Limiter, ProcessSpec, Processor};
///
/// let mut limiter = Limiter::<f32>::new().with_ceiling_db(-1.0);
/// limiter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 1 });
///
/// let mut block = AudioBuffer::<f32>::new(1, 4_800);
/// block.channel_mut(0).fill(2.0); // 6 dB over full scale
/// limiter.process(&mut block);
/// assert!(block.peak() <= 0.892); // -1 dB
/// ```
#[derive(Debug, Clone)]
pub struct Limiter<S: Sample = f32> {
    ceiling_db: f64,
    lookahead_ms: f64,
    release_ms: f64,
    sample_rate: f64,
    ceiling: f64,
    /// Fraction of the remaining gain reduction kept from one sample to the
    /// next.
    release: f64,
    /// The look-ahead window in frames.
    window: usize,
    /// The audio waiting to be played, per channel.
    delay: Vec<Vec<S>>,
    position: usize,
    /// Stage 1: the needed gain, with recovery.
    envelope: f64,
    /// Stage 2: candidates for the minimum, as value and frame number, oldest
    /// first. A ring, so that nothing is ever shifted or allocated.
    candidates: Vec<(f64, u64)>,
    first: usize,
    count: usize,
    frame: u64,
    /// Stage 3: the last `window` minima and their sum.
    minima: Vec<f64>,
    sum: f64,
    reduction_db: f64,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for Limiter<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Limiter<S> {
    /// Creates a limiter with a ceiling of -0.3 dB, 5 ms of look-ahead and a
    /// 100 ms release.
    pub fn new() -> Self {
        Self {
            ceiling_db: -0.3,
            lookahead_ms: 5.0,
            release_ms: 100.0,
            sample_rate: 44_100.0,
            ceiling: 1.0,
            release: 0.0,
            window: 1,
            delay: Vec::new(),
            position: 0,
            envelope: 1.0,
            candidates: Vec::new(),
            first: 0,
            count: 0,
            frame: 0,
            minima: Vec::new(),
            sum: 0.0,
            reduction_db: 0.0,
            marker: PhantomData,
        }
    }

    /// Sets the level no output sample will exceed, in decibels. Real-time
    /// safe.
    pub fn set_ceiling_db(&mut self, db: f64) {
        self.ceiling_db = db;
        self.ceiling = 10f64.powf(db / 20.0);
    }

    /// Sets how quickly the gain recovers after a peak, in milliseconds.
    /// Real-time safe.
    pub fn set_release_ms(&mut self, milliseconds: f64) {
        self.release_ms = milliseconds;
        self.update_release();
    }

    /// Sets how far ahead the limiter looks, in milliseconds, which is also
    /// how much it delays the audio. It takes effect the next time the
    /// limiter is prepared, because it changes the amount of memory needed.
    pub fn set_lookahead_ms(&mut self, milliseconds: f64) {
        self.lookahead_ms = milliseconds.max(0.0);
    }

    /// Builder form of [`set_ceiling_db`](Self::set_ceiling_db).
    #[must_use]
    pub fn with_ceiling_db(mut self, db: f64) -> Self {
        self.set_ceiling_db(db);
        self
    }

    /// Builder form of [`set_release_ms`](Self::set_release_ms).
    #[must_use]
    pub fn with_release_ms(mut self, milliseconds: f64) -> Self {
        self.set_release_ms(milliseconds);
        self
    }

    /// Builder form of [`set_lookahead_ms`](Self::set_lookahead_ms).
    #[must_use]
    pub fn with_lookahead_ms(mut self, milliseconds: f64) -> Self {
        self.set_lookahead_ms(milliseconds);
        self
    }

    /// How much the gain is currently being reduced, in decibels, as a
    /// positive number. For driving a meter.
    pub fn gain_reduction_db(&self) -> f64 {
        self.reduction_db
    }

    fn update_release(&mut self) {
        let samples = self.release_ms * 1e-3 * self.sample_rate;
        self.release = if samples > 0.0 {
            (-1.0 / samples).exp()
        } else {
            0.0
        };
    }

    /// Adds a needed gain to the window and returns the smallest in it.
    #[inline]
    fn window_minimum(&mut self, value: f64) -> f64 {
        let capacity = self.candidates.len();
        // A candidate that is no smaller than the newcomer can never be the
        // minimum again: the newcomer outlives it.
        while self.count > 0 {
            let last = (self.first + self.count - 1) % capacity;
            if self.candidates[last].0 < value {
                break;
            }
            self.count -= 1;
        }
        let slot = (self.first + self.count) % capacity;
        self.candidates[slot] = (value, self.frame);
        self.count += 1;
        // Drop the oldest once it has left the window.
        if self.candidates[self.first].1 + self.window as u64 <= self.frame {
            self.first = (self.first + 1) % capacity;
            self.count -= 1;
        }
        self.candidates[self.first].0
    }
}

impl<S: Sample> Processor<S> for Limiter<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.ceiling = 10f64.powf(self.ceiling_db / 20.0);
        self.update_release();
        self.window = ((self.lookahead_ms * 1e-3 * spec.sample_rate).round() as usize).max(1);
        self.delay = vec![vec![S::ZERO; self.window]; spec.num_channels];
        self.candidates = vec![(1.0, 0); self.window + 1];
        self.minima = vec![1.0; self.window];
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        if self.minima.is_empty() {
            return;
        }
        let channels = buffer.num_channels().min(self.delay.len());
        let window = self.window;
        for frame in 0..buffer.num_frames() {
            let mut peak = 0.0f64;
            for channel in 0..channels {
                let sample = &mut buffer.channel_mut(channel)[frame];
                let value = sample.to_f64();
                if value.is_finite() {
                    peak = peak.max(value.abs());
                } else {
                    // A fault upstream. Silence is the only output under
                    // the ceiling it has, and it leaves the gain alone.
                    *sample = S::ZERO;
                }
            }
            let needed = if peak > self.ceiling {
                self.ceiling / peak
            } else {
                1.0
            };
            // Recover towards unity, but never above what this sample needs.
            self.envelope = needed.min(1.0 - (1.0 - self.envelope) * self.release);

            let minimum = self.window_minimum(self.envelope);
            self.sum += minimum - self.minima[self.position];
            self.minima[self.position] = minimum;
            if self.position == 0 {
                // Adding and subtracting for ever lets rounding errors build
                // up. Once per window the sum is taken afresh.
                self.sum = self.minima.iter().sum();
            }
            let gain = (self.sum / window as f64).min(1.0);
            self.reduction_db = -20.0 * gain.max(1e-12).log10();

            // Write the new sample, then read the oldest: a delay of one
            // frame less than the window.
            let read = (self.position + 1) % window;
            let gain = S::from_f64(gain);
            for channel in 0..channels {
                let line = &mut self.delay[channel];
                let sample = &mut buffer.channel_mut(channel)[frame];
                line[self.position] = *sample;
                *sample = line[read] * gain;
            }
            self.position = read;
            self.frame += 1;
        }
    }

    fn reset(&mut self) {
        for line in &mut self.delay {
            line.fill(S::ZERO);
        }
        self.minima.fill(1.0);
        self.sum = self.minima.len() as f64;
        self.position = 0;
        self.envelope = 1.0;
        self.first = 0;
        self.count = 0;
        self.frame = 0;
        self.reduction_db = 0.0;
    }

    fn latency_samples(&self) -> usize {
        self.window.saturating_sub(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: ProcessSpec = ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 48_000,
        num_channels: 1,
    };

    fn prepared(limiter: Limiter<f64>) -> Limiter<f64> {
        let mut limiter = limiter;
        limiter.prepare(&SPEC);
        limiter
    }

    /// Deterministic noise with occasional spikes far over full scale.
    fn spiky(frames: usize) -> AudioBuffer<f64> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut buffer = AudioBuffer::new(1, frames);
        for (n, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let noise = (state >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0;
            let spike = if n % 997 < 3 { 6.0 } else { 1.0 };
            *sample = 0.8 * noise * spike;
        }
        buffer
    }

    #[test]
    fn no_sample_exceeds_the_ceiling() {
        for (ceiling_db, lookahead_ms, release_ms) in [
            (-1.0, 5.0, 100.0),
            (-6.0, 1.0, 10.0),
            (0.0, 0.0, 0.0),
            (-3.0, 20.0, 1_000.0),
        ] {
            let mut limiter = prepared(
                Limiter::new()
                    .with_ceiling_db(ceiling_db)
                    .with_lookahead_ms(lookahead_ms)
                    .with_release_ms(release_ms),
            );
            let ceiling = 10f64.powf(ceiling_db / 20.0);
            let mut signal = spiky(48_000);
            assert!(signal.peak() > 3.0);
            limiter.process(&mut signal);
            let peak = signal.peak();
            assert!(
                peak <= ceiling * (1.0 + 1e-9),
                "ceiling {ceiling_db} dB, look-ahead {lookahead_ms} ms: peak {peak}"
            );
            // It limits; it does not just turn everything down.
            assert!(peak > 0.9 * ceiling);
        }
    }

    #[test]
    fn a_signal_under_the_ceiling_is_only_delayed() {
        let mut limiter = prepared(Limiter::new().with_ceiling_db(0.0));
        let latency = limiter.latency_samples();
        assert_eq!(latency, 239, "5 ms at 48 kHz, less one frame");

        let mut signal = AudioBuffer::<f64>::new(1, 2_000);
        for (n, sample) in signal.channel_mut(0).iter_mut().enumerate() {
            *sample = 0.9 * ((n * 7 % 101) as f64 / 50.0 - 1.0);
        }
        let original = signal.clone();
        limiter.process(&mut signal);
        assert_eq!(&signal.channel(0)[..latency], &vec![0.0; latency][..]);
        assert_eq!(
            &signal.channel(0)[latency..],
            &original.channel(0)[..2_000 - latency]
        );
        assert_eq!(limiter.gain_reduction_db(), 0.0);
    }

    #[test]
    fn the_gain_comes_down_before_a_peak_and_recovers_after_it() {
        let mut limiter = prepared(Limiter::new().with_ceiling_db(0.0).with_release_ms(50.0));
        let latency = limiter.latency_samples();
        let mut signal = AudioBuffer::<f64>::new(1, 48_000);
        signal.channel_mut(0).fill(0.5);
        signal.channel_mut(0)[10_000] = 4.0; // one sample, 12 dB over

        limiter.process(&mut signal);
        let out = signal.channel(0);
        // The peak itself lands exactly on the ceiling.
        assert!((out[10_000 + latency] - 1.0).abs() < 1e-9);
        // The gain was already coming down before it, smoothly.
        let before = &out[10_000 + latency - 200..10_000 + latency];
        assert!(before.windows(2).all(|pair| pair[1] <= pair[0] + 1e-12));
        assert!(before[0] > before[199]);
        let largest_step = before
            .windows(2)
            .map(|pair| pair[0] - pair[1])
            .fold(0.0, f64::max);
        assert!(
            largest_step < 0.01,
            "the gain dropped by {largest_step} in one sample"
        );
        // Well after the release, the signal is back to its own level.
        assert!((out[10_000 + latency + 24_000] - 0.5).abs() < 1e-3);
        assert!(limiter.gain_reduction_db() < 0.01);
    }

    #[test]
    fn channels_share_one_gain() {
        let mut limiter = Limiter::<f64>::new().with_ceiling_db(0.0);
        limiter.prepare(&ProcessSpec {
            num_channels: 2,
            ..SPEC
        });
        let mut signal = AudioBuffer::<f64>::new(2, 4_800);
        signal.channel_mut(0).fill(4.0);
        signal.channel_mut(1).fill(0.4);
        limiter.process(&mut signal);
        // The loud side sits on the ceiling and the quiet side keeps its
        // ratio to it.
        assert!((signal.channel(0)[4_799] - 1.0).abs() < 1e-9);
        assert!((signal.channel(1)[4_799] - 0.1).abs() < 1e-9);
        assert!((limiter.gain_reduction_db() - 12.04).abs() < 0.01);
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut limiter = prepared(Limiter::new().with_ceiling_db(-2.0));
            let mut signal = spiky(6_000);
            let mut start = 0;
            while start < 6_000 {
                let end = (start + block).min(6_000);
                signal.windowed(start..end, |piece| limiter.process(piece));
                start = end;
            }
            signal
        };
        let reference = render(6_000);
        assert!(render(1) == reference);
        assert!(render(241) == reference);
    }

    #[test]
    fn samples_that_are_not_finite_come_out_as_silence() {
        for lookahead_ms in [0.0, 5.0] {
            let render = |faults: bool| {
                let mut limiter = Limiter::<f32>::new()
                    .with_ceiling_db(-1.0)
                    .with_lookahead_ms(lookahead_ms);
                limiter.prepare(&ProcessSpec {
                    num_channels: 2,
                    ..SPEC
                });
                let mut signal = AudioBuffer::<f32>::new(2, 4_800);
                for channel in 0..2 {
                    for (n, sample) in signal.channel_mut(channel).iter_mut().enumerate() {
                        *sample = 2.5 * ((n * 7 + channel) % 101) as f32 / 101.0 - 1.2;
                    }
                }
                for (channel, frame, value) in [
                    (0, 100, f32::NAN),
                    (1, 101, f32::INFINITY),
                    (0, 2_000, f32::NEG_INFINITY),
                    (1, 2_000, -f32::NAN),
                ] {
                    signal.channel_mut(channel)[frame] = if faults { value } else { 0.0 };
                }
                limiter.process(&mut signal);
                signal
            };
            let output = render(true);
            // The gain is worked out in f64 and applied in f32, which may
            // round up by half a unit.
            let ceiling = 10f32.powf(-1.0 / 20.0) * (1.0 + f32::EPSILON);
            for channel in 0..2 {
                assert!(
                    output.channel(channel).iter().all(|s| s.abs() <= ceiling),
                    "look-ahead {lookahead_ms} ms, channel {channel}"
                );
            }
            // Exactly as if those samples had been silence.
            assert!(output == render(false), "look-ahead {lookahead_ms} ms");
        }
    }

    #[test]
    fn processing_before_prepare_does_nothing() {
        let mut limiter = Limiter::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(1, 8);
        buffer.channel_mut(0).fill(3.0);
        limiter.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[3.0; 8]);
    }
}
