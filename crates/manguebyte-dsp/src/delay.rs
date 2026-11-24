use manguebyte_core::{AudioBuffer, Sample};

use crate::curve::DelayTaps;
use crate::numeric::limit;
use crate::{ProcessSpec, Processor};

/// Shortest delay the all-pass interpolator serves, in samples.
const THIRAN_MIN_DELAY: f64 = 1.5;

/// How a [`DelayLine`] reads a delay that falls between two stored samples.
///
/// A delay of a whole number of samples is a plain lookup. Anything in
/// between has to be estimated from the neighbours, and each way of doing
/// that has its own cost and its own flaw. The level column is the worst
/// case: half-way between two samples, at a quarter of the sample rate.
///
/// | | Samples read | Level | Use it for |
/// |---|---|---|---|
/// | [`None`](Self::None) | 1 | unchanged, but the delay is rounded | delays of whole samples |
/// | [`Linear`](Self::Linear) | 2 | -3.0 dB | echoes, cheap modulation |
/// | [`Cubic`](Self::Cubic) | 4 | -1.1 dB | chorus, flanger, vibrato |
/// | [`Thiran`](Self::Thiran) | 2, and it keeps memory | unchanged | tuned resonators, fixed delays |
///
/// The methods are those surveyed in T. I. Laakso, V. Välimäki, M. Karjalainen
/// and U. K. Laine, "Splitting the unit delay", IEEE Signal Processing
/// Magazine, 1996.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DelayInterpolation {
    /// No interpolation: the delay is rounded to the nearest whole sample.
    /// The level is untouched at every frequency, but a delay that moves does
    /// so in steps, which is heard as a rough "zipper" sound.
    None,
    /// A straight line between the two nearest samples. Cheap, and it moves
    /// smoothly, but it dulls high frequencies by an amount that depends on
    /// where between the samples the read point is.
    #[default]
    Linear,
    /// A cubic through the four nearest samples: Lagrange interpolation of
    /// third order. It keeps far more of the high frequencies than a straight
    /// line and moves just as smoothly, for twice the work.
    ///
    /// A delay of less than two samples has no sample on the near side of the
    /// read point, and is read along a straight line instead, with the loss
    /// of level that [`Linear`](Self::Linear) has: see
    /// [`DelayLine::read_cubic`].
    Cubic,
    /// A first-order all-pass filter, after J.-P. Thiran, "Recursive digital
    /// filters with maximally flat group delay", IEEE Transactions on Circuit
    /// Theory, 1971.
    ///
    /// The level is exactly unchanged at every frequency, which is what a
    /// tuned resonator needs: a string or a comb whose loop loses level only
    /// where you say so. The price is paid in time instead. The delay is the
    /// requested one at low frequencies and drifts from it higher up, and the
    /// filter has memory, so moving the delay quickly disturbs it. Use it for
    /// delays that are fixed or change slowly.
    Thiran,
}

/// A circular buffer that reads back what was written some samples ago, at
/// whole or fractional delays.
///
/// This is the primitive behind echoes, choruses, flangers and comb filters.
///
/// [`read`](Self::read) interpolates linearly. The other ways of reading
/// between samples are listed in [`DelayInterpolation`]: each has a method
/// of its own, and [`read_interpolated`](Self::read_interpolated) uses the
/// one chosen with [`with_interpolation`](Self::with_interpolation).
///
/// ```
/// use manguebyte_dsp::{DelayInterpolation, DelayLine};
///
/// let mut line = DelayLine::<f64>::new(16).with_interpolation(DelayInterpolation::Cubic);
/// // A parabola: 0, 1, 4, 9, ... up to 11 squared.
/// for n in 0..12 {
///     line.push(f64::from(n * n));
/// }
///
/// // Three pushes ago 9 squared was written, and four pushes ago 8 squared.
/// assert_eq!(line.read(3.0), 81.0);
/// assert_eq!(line.read(4.0), 64.0);
/// // Half-way between them a straight line is a little off the curve,
/// assert_eq!(line.read(3.5), 72.5);
/// // and a cubic is on it: 8.5 squared.
/// assert!((line.read_interpolated(3.5) - 72.25).abs() < 1e-9);
/// ```
#[derive(Debug, Clone)]
pub struct DelayLine<S: Sample = f32> {
    buffer: Vec<S>,
    mask: usize,
    /// Slot the next [`push`](Self::push) writes to.
    write: usize,
    max_delay: usize,
    interpolation: DelayInterpolation,
    /// Last output of the all-pass interpolator.
    allpass: S,
}

impl<S: Sample> DelayLine<S> {
    /// Creates a silent line able to delay by up to `max_delay` samples.
    /// This allocates.
    pub fn new(max_delay: usize) -> Self {
        // One slot for the newest sample and one for interpolation headroom,
        // and never fewer than the four a cubic read looks at.
        let len = (max_delay + 2).next_power_of_two().max(4);
        Self {
            buffer: vec![S::ZERO; len],
            mask: len - 1,
            write: 0,
            max_delay,
            interpolation: DelayInterpolation::Linear,
            allpass: S::ZERO,
        }
    }

    /// Builder form of [`set_interpolation`](Self::set_interpolation).
    #[must_use]
    pub fn with_interpolation(mut self, interpolation: DelayInterpolation) -> Self {
        self.set_interpolation(interpolation);
        self
    }

    /// Chooses how [`read_interpolated`](Self::read_interpolated) reads
    /// between samples. A new line uses [`DelayInterpolation::Linear`].
    /// Real-time safe.
    pub fn set_interpolation(&mut self, interpolation: DelayInterpolation) {
        self.interpolation = interpolation;
    }

    /// The interpolation [`read_interpolated`](Self::read_interpolated) uses.
    pub fn interpolation(&self) -> DelayInterpolation {
        self.interpolation
    }

    /// Longest delay, in samples, that [`read`](Self::read) can serve.
    pub fn max_delay(&self) -> usize {
        self.max_delay
    }

    /// Writes one sample.
    #[inline]
    pub fn push(&mut self, sample: S) {
        self.buffer[self.write] = sample;
        self.write = (self.write + 1) & self.mask;
    }

    /// The sample written `pushes` pushes ago.
    #[inline]
    fn tap(&self, pushes: usize) -> S {
        self.buffer[self.write.wrapping_sub(pushes) & self.mask]
    }

    /// Reads the sample written `delay` pushes ago: `1.0` is the most recent.
    /// A delay between two samples is interpolated linearly, whichever
    /// interpolation the line is set to.
    ///
    /// `delay` is clamped to `1.0..=max_delay`. A Nan delay reads the most recent value.
    #[inline]
    pub fn read(&self, delay: f64) -> S {
        let delay = limit(delay, 1.0, self.max_delay.max(1) as f64);
        let whole = delay as usize;
        let fraction = S::from_f64(delay - whole as f64);
        let newer = self.buffer[self.write.wrapping_sub(whole) & self.mask];
        let older = self.buffer[self.write.wrapping_sub(whole + 1) & self.mask];
        newer + (older - newer) * fraction
    }

    /// Reads the stored sample nearest to `delay` pushes ago, without
    /// interpolating. Half-way between two samples the older one is taken.
    ///
    /// `delay` is limited to `1.0..=max_delay`.
    #[inline]
    pub fn read_nearest(&self, delay: f64) -> S {
        let delay = limit(delay, 1.0, self.max_delay.max(1) as f64);
        self.tap((delay + 0.5) as usize)
    }

    /// Reads `delay` pushes ago through a cubic fitted to the four nearest
    /// samples (third-order Lagrange interpolation).
    ///
    /// A whole-number delay returns the stored sample exactly. From two
    /// samples of delay up, the read point lies between the middle two of the
    /// four samples: there a signal that is itself a polynomial of up to
    /// third degree is read without error, and no frequency comes back
    /// louder than it went in.
    ///
    /// Below two samples there is no newer sample to put on the near side of
    /// the read point. A cubic through the four newest samples, read between
    /// the first two of them, raises the top of the spectrum: its gain at
    /// half the sample rate is 1.19 for a delay of 1.75 samples. A loop that
    /// feeds such a read back into the line can then grow without limit
    /// although its feedback is below one. So a delay below two samples is
    /// read along a straight line between the two newest samples, exactly as
    /// [`read`](Self::read) does, which never raises a level. The two ways
    /// of reading agree at two samples, so a delay that moves across that
    /// point does not make the output jump.
    ///
    /// `delay` is limited to `1.0..=max_delay`.
    #[inline]
    pub fn read_cubic(&self, delay: f64) -> S {
        let delay = limit(delay, 1.0, self.max_delay.max(1) as f64);
        if delay < 2.0 {
            let newest = self.tap(1);
            return newest + (self.tap(2) - newest) * S::from_f64(delay - 1.0);
        }
        // The newest of the four samples, as a delay.
        let newest = delay as usize - 1;
        // Where the read point lies among them: 0 is the newest, 3 the
        // oldest.
        let t = delay - newest as f64;
        let (a, b, c, d) = (t, t - 1.0, t - 2.0, t - 3.0);
        // The Lagrange polynomials for nodes at 0, 1, 2 and 3.
        let weights = [
            -b * c * d / 6.0,
            a * c * d / 2.0,
            -a * b * d / 2.0,
            a * b * c / 6.0,
        ];
        let mut sum = S::ZERO;
        for (offset, weight) in weights.into_iter().enumerate() {
            sum += self.tap(newest + offset) * S::from_f64(weight);
        }
        sum
    }

    /// Reads `delay` pushes ago through a first-order Thiran all-pass filter.
    ///
    /// The filter advances one step on every call, so call this exactly once
    /// for every [`push`](Self::push). See [`DelayInterpolation::Thiran`] for
    /// what it is good and bad at. A whole-number delay returns the stored
    /// sample exactly.
    ///
    /// `delay` is limited to `1.5..=max_delay`: below a sample and a half the
    /// filter would ring.
    #[inline]
    pub fn read_thiran(&mut self, delay: f64) -> S {
        let longest = (self.max_delay as f64).max(THIRAN_MIN_DELAY);
        let delay = limit(delay, THIRAN_MIN_DELAY, longest);
        // The filter supplies between half a sample and a sample and a half,
        // the range in which it is at its most accurate and settles fastest.
        // Whole samples of delay make up the rest.
        let whole = (delay - 0.5) as usize;
        let remainder = delay - whole as f64;
        // y[n] = eta * (x[n] - y[n-1]) + x[n-1], which delays low frequencies
        // by (1 - eta) / (1 + eta) samples.
        let eta = S::from_f64((1.0 - remainder) / (1.0 + remainder));
        let output = eta * (self.tap(whole) - self.allpass) + self.tap(whole + 1);
        self.allpass = output;
        output
    }

    /// Reads `delay` pushes ago with the interpolation chosen by
    /// [`set_interpolation`](Self::set_interpolation).
    ///
    /// It takes `&mut self` because [`DelayInterpolation::Thiran`] keeps
    /// memory between reads; with that setting, call it once per
    /// [`push`](Self::push). The limits on `delay` are those of the method
    /// for the chosen interpolation.
    #[inline]
    pub fn read_interpolated(&mut self, delay: f64) -> S {
        match self.interpolation {
            DelayInterpolation::None => self.read_nearest(delay),
            DelayInterpolation::Linear => self.read(delay),
            DelayInterpolation::Cubic => self.read_cubic(delay),
            DelayInterpolation::Thiran => self.read_thiran(delay),
        }
    }

    /// Silences the line.
    pub fn clear(&mut self) {
        self.buffer.fill(S::ZERO);
        self.allpass = S::ZERO;
    }
}

/// A feedback delay (echo) effect.
///
/// Changing the delay time while audio is running jumps the read position,
/// which can click; smooth it yourself if you need to sweep it.
///
/// A delay time that is not a whole number of samples is read by linear
/// interpolation unless another [`DelayInterpolation`] is chosen with
/// [`with_interpolation`](Self::with_interpolation). None of them returns
/// any frequency louder than it was written, so with every one of them the
/// echoes of a set delay time die away, however short it is.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Delay, ProcessSpec, Processor};
///
/// // 10 samples of delay at 1 kHz, each echo half as loud as the last.
/// let mut echo = Delay::<f32>::new(0.05)
///     .with_time(0.01)
///     .with_feedback(0.5)
///     .with_mix(1.0);
/// echo.prepare(&ProcessSpec { sample_rate: 1_000.0, max_block_size: 32, num_channels: 1 });
///
/// let mut block = AudioBuffer::<f32>::new(1, 32);
/// block.channel_mut(0)[0] = 1.0;
/// echo.process(&mut block);
/// assert_eq!(block.channel(0)[10], 1.0);
/// assert_eq!(block.channel(0)[20], 0.5);
/// ```
#[derive(Debug, Clone)]
pub struct Delay<S: Sample = f32> {
    lines: Vec<DelayLine<S>>,
    max_seconds: f64,
    time_seconds: f64,
    sample_rate: f64,
    feedback: S,
    mix: S,
    interpolation: DelayInterpolation,
}

impl<S: Sample> Delay<S> {
    /// Creates an echo that can delay by up to `max_seconds`, initially set
    /// to that maximum with no feedback and an equal dry/wet mix.
    pub fn new(max_seconds: f64) -> Self {
        Self {
            lines: Vec::new(),
            max_seconds,
            time_seconds: max_seconds,
            sample_rate: 44_100.0,
            feedback: S::ZERO,
            mix: S::from_f64(0.5),
            interpolation: DelayInterpolation::Linear,
        }
    }

    /// Sets the delay time in seconds, clamped to the maximum. a Nan sets the shortest time
    pub fn set_time(&mut self, seconds: f64) {
        self.time_seconds = limit(seconds, 0.0, self.max_seconds);
    }

    /// Sets how much of the delayed signal is fed back in, clamped to
    /// `0.0..=0.99` so the echoes always die away.
    pub fn set_feedback(&mut self, feedback: f64) {
        self.feedback = S::from_f64(limit(feedback, 0.0, 0.99));
    }

    /// Sets the dry/wet balance: `0.0` is all dry, `1.0` all delayed.
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = S::from_f64(limit(mix, 0.0, 1.0));
    }

    /// Sets how a delay time between two samples is read. The default is
    /// [`DelayInterpolation::Linear`]. Real-time safe.
    pub fn set_interpolation(&mut self, interpolation: DelayInterpolation) {
        self.interpolation = interpolation;
        for line in &mut self.lines {
            line.set_interpolation(interpolation);
        }
    }

    /// Builder form of [`set_time`](Self::set_time).
    pub fn with_time(mut self, seconds: f64) -> Self {
        self.set_time(seconds);
        self
    }

    /// Builder form of [`set_feedback`](Self::set_feedback).
    pub fn with_feedback(mut self, feedback: f64) -> Self {
        self.set_feedback(feedback);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// Builder form of [`set_interpolation`](Self::set_interpolation).
    #[must_use]
    pub fn with_interpolation(mut self, interpolation: DelayInterpolation) -> Self {
        self.set_interpolation(interpolation);
        self
    }

    /// The dry signal and the echoes, with their times and gains, for
    /// drawing. The delay time is the one the delay line reads at the
    /// sample rate the delay was prepared for: within the line's limits,
    /// and rounded to a whole sample without interpolation. See
    /// [`DelayTaps`] and [`curve`](crate::curve).
    pub fn taps(&self) -> DelayTaps {
        let longest = (self.max_seconds * self.sample_rate).ceil().max(1.0) as usize as f64;
        let delay = self.time_seconds * self.sample_rate;
        let delay = match self.interpolation {
            DelayInterpolation::None => (limit(delay, 1.0, longest) + 0.5) as usize as f64,
            DelayInterpolation::Linear | DelayInterpolation::Cubic => limit(delay, 1.0, longest),
            DelayInterpolation::Thiran => {
                limit(delay, THIRAN_MIN_DELAY, longest.max(THIRAN_MIN_DELAY))
            }
        };
        DelayTaps::from_parts(
            delay,
            self.feedback.to_f64(),
            self.mix.to_f64(),
            (S::ONE - self.mix).to_f64(),
            self.sample_rate,
        )
    }
}

impl<S: Sample> Processor<S> for Delay<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        let max_delay = (self.max_seconds * spec.sample_rate).ceil().max(1.0) as usize;
        self.lines = (0..spec.num_channels)
            .map(|_| DelayLine::new(max_delay).with_interpolation(self.interpolation))
            .collect();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        debug_assert!(
            buffer.num_channels() <= self.lines.len(),
            "buffer has more channels than the delay was prepared for"
        );
        let delay = self.time_seconds * self.sample_rate;
        let (feedback, mix) = (self.feedback, self.mix);
        let dry = S::ONE - mix;
        for (channel, line) in buffer.iter_channels_mut().zip(&mut self.lines) {
            for sample in channel {
                let input = *sample;
                let delayed = line.read_interpolated(delay);
                line.push(input + delayed * feedback);
                *sample = input * dry + delayed * mix;
            }
        }
    }

    fn reset(&mut self) {
        for line in &mut self.lines {
            line.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::{PI, TAU};

    use super::*;
    use crate::numeric::strict_peak;

    #[test]
    fn line_reads_back_by_whole_samples() {
        let mut line = DelayLine::<f32>::new(8);
        for value in 1..=5 {
            line.push(value as f32);
        }
        assert_eq!(line.read(1.0), 5.0);
        assert_eq!(line.read(3.0), 3.0);
        assert_eq!(line.read(5.0), 1.0);
        assert_eq!(line.read(8.0), 0.0);
    }

    #[test]
    fn line_interpolates_and_clamps() {
        let mut line = DelayLine::<f64>::new(4);
        line.push(10.0);
        line.push(20.0);
        assert_eq!(line.read(1.5), 15.0);
        assert_eq!(line.read(0.0), 20.0);
        assert_eq!(line.read(100.0), line.read(4.0));
    }

    #[test]
    fn line_survives_wrapping() {
        let mut line = DelayLine::<f32>::new(3);
        for value in 0..1_000 {
            line.push(value as f32);
            assert_eq!(line.read(1.0), value as f32);
            if value >= 2 {
                assert_eq!(line.read(3.0), (value - 2) as f32);
            }
        }
    }

    #[test]
    fn echoes_arrive_on_time_and_decay() {
        // 10 samples of delay at 1 kHz.
        let mut delay = Delay::<f64>::new(0.05)
            .with_time(0.01)
            .with_feedback(0.5)
            .with_mix(1.0);
        delay.prepare(&ProcessSpec {
            sample_rate: 1_000.0,
            max_block_size: 32,
            num_channels: 1,
        });

        let mut buffer = AudioBuffer::<f64>::new(1, 32);
        buffer.channel_mut(0)[0] = 1.0;
        delay.process(&mut buffer);

        let out = buffer.channel(0);
        for (frame, &sample) in out.iter().enumerate() {
            let expected = match frame {
                10 => 1.0,
                20 => 0.5,
                30 => 0.25,
                _ => 0.0,
            };
            assert_eq!(sample, expected, "frame {frame}");
        }
    }

    #[test]
    fn dry_mix_leaves_the_signal_alone_and_reset_clears_the_tail() {
        let spec = ProcessSpec {
            sample_rate: 1_000.0,
            max_block_size: 16,
            num_channels: 2,
        };
        let mut delay = Delay::<f32>::new(0.004).with_mix(0.0);
        delay.prepare(&spec);
        let mut buffer = AudioBuffer::<f32>::new(2, 16);
        buffer.channel_mut(0).fill(0.5);
        delay.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.5; 16]);

        delay.set_mix(1.0);
        delay.reset();
        buffer.clear();
        delay.process(&mut buffer);
        assert_eq!(buffer.peak(), 0.0);
    }

    const MODES: [DelayInterpolation; 4] = [
        DelayInterpolation::None,
        DelayInterpolation::Linear,
        DelayInterpolation::Cubic,
        DelayInterpolation::Thiran,
    ];

    /// A line holding a ramp: the sample written `k` pushes ago is `100 - k`.
    fn ramp_line(max_delay: usize) -> DelayLine<f64> {
        let mut line = DelayLine::new(max_delay);
        for value in 0..100 {
            line.push(f64::from(value));
        }
        line
    }

    #[test]
    fn linear_is_the_default_and_the_selected_interpolation_is_used() {
        let mut line = ramp_line(32);
        assert_eq!(line.interpolation(), DelayInterpolation::Linear);
        assert_eq!(line.read_interpolated(4.25), line.read(4.25));

        line.set_interpolation(DelayInterpolation::None);
        assert_eq!(line.read_interpolated(4.25), line.read_nearest(4.25));
        line.set_interpolation(DelayInterpolation::Cubic);
        assert_eq!(line.read_interpolated(4.25), line.read_cubic(4.25));
        // `read` itself stays linear, as it always was.
        assert_eq!(line.read(4.5), 95.5);

        let mut expected = line.clone();
        line.set_interpolation(DelayInterpolation::Thiran);
        assert_eq!(line.interpolation(), DelayInterpolation::Thiran);
        assert_eq!(line.read_interpolated(4.25), expected.read_thiran(4.25));
    }

    #[test]
    fn nearest_rounds_to_whole_samples() {
        let line = ramp_line(32);
        assert_eq!(line.read_nearest(10.0), 90.0);
        assert_eq!(line.read_nearest(10.4), 90.0);
        assert_eq!(line.read_nearest(10.5), 89.0);
        assert_eq!(line.read_nearest(10.6), 89.0);
        assert_eq!(line.read_nearest(0.0), 99.0);
        assert_eq!(line.read_nearest(1e9), 68.0);
    }

    #[test]
    fn whole_delays_are_exact_in_every_mode() {
        // Values with no pattern, so that a wrong tap cannot pass by luck.
        let mut line = DelayLine::<f64>::new(64);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut written = Vec::new();
        for _ in 0..200 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            line.push(value);
            written.push(value);
        }
        for mode in MODES {
            line.set_interpolation(mode);
            for delay in 2..=64usize {
                let expected = written[written.len() - delay];
                assert_eq!(
                    line.read_interpolated(delay as f64),
                    expected,
                    "{mode:?}, delay {delay}"
                );
            }
        }
        // One sample is within reach of every mode but the all-pass.
        let newest = written[written.len() - 1];
        assert_eq!(line.read_nearest(1.0), newest);
        assert_eq!(line.read(1.0), newest);
        assert_eq!(line.read_cubic(1.0), newest);
    }

    #[test]
    fn cubic_reads_a_cubic_signal_without_error() {
        let curve = |x: f64| 0.002 * x * x * x - 0.3 * x * x + 4.0 * x - 7.0;
        let mut line = DelayLine::<f64>::new(40);
        for n in 0..60 {
            line.push(curve(f64::from(n)));
        }
        // The next push would be sample 60, so a delay of `d` reads the
        // curve at `60 - d`. This covers every delay that has a sample on
        // either side of the middle two, from the shortest to the longest.
        for step in 0..=380 {
            let delay = 2.0 + 0.1 * f64::from(step);
            let expected = curve(60.0 - delay);
            let read = line.read_cubic(delay);
            assert!(
                (read - expected).abs() < 1e-9,
                "delay {delay}: {read} against {expected}"
            );
        }
        // A straight line cannot do that.
        assert!((line.read(20.5) - curve(39.5)).abs() > 1e-3);
    }

    #[test]
    fn below_two_samples_the_cubic_read_is_a_straight_line() {
        // Values with no pattern.
        let mut line = DelayLine::<f64>::new(16);
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut written = Vec::new();
        for _ in 0..50 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let value = (state >> 11) as f64 / (1u64 << 53) as f64 - 0.5;
            line.push(value);
            written.push(value);
        }
        let (newest, second) = (written[49], written[48]);
        for step in 0..100 {
            let delay = 1.0 + 0.01 * f64::from(step);
            let read = line.read_cubic(delay);
            assert_eq!(read, line.read(delay), "delay {delay}");
            // Between the two newest samples, and never outside them.
            let expected = newest + (second - newest) * (delay - 1.0);
            assert!((read - expected).abs() < 1e-15, "delay {delay}");
            assert!(read >= newest.min(second) && read <= newest.max(second));
        }
        // The line and the cubic meet at two samples, so a delay that moves
        // across that point reads no jump.
        assert_eq!(line.read_cubic(2.0), second);
        assert!((line.read_cubic(2.0 - 1e-9) - second).abs() < 1e-8);
        assert!((line.read_cubic(2.0 + 1e-9) - second).abs() < 1e-8);

        // What the straight line is there to avoid: the cubic through the
        // four newest samples, read three quarters of the way from the first
        // to the second. At half the sample rate neighbouring samples have
        // opposite signs, and its four weights then add up to more than one.
        let t: f64 = 0.75;
        let weights = [
            -(t - 1.0) * (t - 2.0) * (t - 3.0) / 6.0,
            t * (t - 2.0) * (t - 3.0) / 2.0,
            -t * (t - 1.0) * (t - 3.0) / 2.0,
            t * (t - 1.0) * (t - 2.0) / 6.0,
        ];
        let gain = (weights[0] - weights[1] + weights[2] - weights[3]).abs();
        assert_eq!(gain, 1.1875);
    }

    /// The largest gain, over all frequencies, of a read at a fixed delay,
    /// from its impulse response.
    fn largest_gain(mode: DelayInterpolation, delay: f64) -> f64 {
        let mut line = DelayLine::<f64>::new(16).with_interpolation(mode);
        // Only the all-pass has a response that goes on after the delay, and
        // it falls to a third or less with every sample.
        let mut response = [0.0; 64];
        for (n, output) in response.iter_mut().enumerate() {
            *output = line.read_interpolated(delay);
            line.push(if n == 0 { 1.0 } else { 0.0 });
        }
        let mut largest = 0.0f64;
        for step in 0..=256 {
            let omega = PI * f64::from(step) / 256.0;
            let (mut re, mut im) = (0.0, 0.0);
            for (n, &sample) in response.iter().enumerate() {
                let (sin, cos) = (omega * n as f64).sin_cos();
                re += sample * cos;
                im -= sample * sin;
            }
            largest = largest.max(re.hypot(im));
        }
        largest
    }

    #[test]
    fn no_interpolation_returns_a_frequency_louder_than_it_was_written() {
        // This is what lets a loop with a feedback below one die away. The
        // delays run from one sample to six in steps of a fortieth, which
        // takes in the short ones that the cubic reads along a straight
        // line.
        for mode in MODES {
            for step in 0..=200 {
                let delay = 1.0 + 0.025 * f64::from(step);
                let gain = largest_gain(mode, delay);
                // Zero hertz comes back at exactly its own level, so the
                // largest gain is one and no more.
                assert!(
                    (gain - 1.0).abs() < 1e-12,
                    "{mode:?}, delay {delay}: a gain of {gain}"
                );
            }
        }
    }

    /// `Delay::set_feedback` limits the feedback to 0.99 "so the echoes
    /// always die away".
    ///
    /// Found in review: with `DelayInterpolation::Cubic` and a delay time
    /// between one and two samples they did not. The cubic was then fitted
    /// to the four newest samples and read between the first two, where its
    /// gain reaches 1.19 at half the sample rate. An impulse into a
    /// 1.6-sample echo with a feedback of 0.9 was at 6.3e122 half a second
    /// later, and with a feedback of 0.95 the delay line filled with
    /// not-a-number and stayed that way until it was reset.
    #[test]
    fn a_short_echo_dies_away_with_every_interpolation() {
        for mode in MODES {
            for samples in [1.25, 1.6, 1.75, 1.9, 2.5] {
                for feedback in [0.9, 0.99] {
                    let mut delay = Delay::<f64>::new(0.01)
                        .with_time(samples / 48_000.0)
                        .with_feedback(feedback)
                        .with_mix(1.0)
                        .with_interpolation(mode);
                    delay.prepare(&ProcessSpec {
                        sample_rate: 48_000.0,
                        max_block_size: 512,
                        num_channels: 1,
                    });
                    let mut signal = AudioBuffer::<f64>::new(1, 48_000);
                    signal.channel_mut(0)[0] = 1.0;
                    let mut start = 0;
                    while start < 48_000 {
                        let end = (start + 512).min(48_000);
                        signal.windowed(start..end, |piece| delay.process(piece));
                        start = end;
                    }

                    // 0.99 to the power of the 9,600 trips a loop of two and
                    // a half samples makes in half a second is far below any
                    // threshold. Not-a-number counts as infinite here.
                    let late = strict_peak(&signal.channel(0)[24_000..]);
                    assert!(
                        late < 1e-3,
                        "{mode:?}, {samples} samples, feedback {feedback}: \
                         half a second after an impulse the echo is at {late:e}"
                    );
                }
            }
        }
    }

    /// Runs a sine with `period` samples per cycle through the line at a
    /// fixed delay. Returns the gain it comes back with, and its delay in
    /// samples, which a steady sine can only reveal up to a whole number of
    /// periods.
    fn measure(mode: DelayInterpolation, delay: f64, period: usize) -> (f64, f64) {
        let mut line = DelayLine::<f64>::new(64).with_interpolation(mode);
        let omega = TAU / period as f64;
        let settle = 2_000;
        let measured = 16 * period;
        let (mut in_phase, mut quadrature) = (0.0, 0.0);
        for n in 0..settle + measured {
            let output = line.read_interpolated(delay);
            line.push((omega * n as f64).sin());
            if n >= settle {
                in_phase += output * (omega * n as f64).sin();
                quadrature += output * (omega * n as f64).cos();
            }
        }
        let scale = 2.0 / measured as f64;
        let (in_phase, quadrature) = (in_phase * scale, quadrature * scale);
        let lag = (-quadrature).atan2(in_phase).rem_euclid(TAU);
        (in_phase.hypot(quadrature), lag / omega)
    }

    /// Distance between two delays that are only known modulo `period`.
    fn apart(a: f64, b: f64, period: usize) -> f64 {
        let period = period as f64;
        let difference = (a - b).rem_euclid(period);
        difference.min(period - difference)
    }

    #[test]
    fn interpolation_losses_match_theory() {
        // Half-way between two samples is where each method is at its worst.
        for (period, linear, cubic) in [
            // A quarter of the sample rate: cos(pi/4), and
            // 9/8 cos(pi/4) - 1/8 cos(3 pi/4).
            (4, (PI / 4.0).cos(), 1.25 * (PI / 4.0).cos()),
            // An eighth of it.
            (
                8,
                (PI / 8.0).cos(),
                1.125 * (PI / 8.0).cos() - 0.125 * (3.0 * PI / 8.0).cos(),
            ),
        ] {
            let (gain, delay) = measure(DelayInterpolation::Linear, 10.5, period);
            assert!((gain - linear).abs() < 1e-12, "linear, period {period}");
            assert!(apart(delay, 10.5, period) < 1e-9);

            let (gain, delay) = measure(DelayInterpolation::Cubic, 10.5, period);
            assert!((gain - cubic).abs() < 1e-12, "cubic, period {period}");
            assert!(apart(delay, 10.5, period) < 1e-9);

            let (gain, delay) = measure(DelayInterpolation::None, 10.5, period);
            assert!((gain - 1.0).abs() < 1e-12);
            assert!(apart(delay, 11.0, period) < 1e-9);

            let (gain, _) = measure(DelayInterpolation::Thiran, 10.5, period);
            assert!((gain - 1.0).abs() < 1e-12, "all-pass, period {period}");
        }
        // The figures quoted in the documentation of `DelayInterpolation`.
        let db = |gain: f64| 20.0 * gain.log10();
        assert!((db(measure(DelayInterpolation::Linear, 10.5, 4).0) + 3.01).abs() < 0.01);
        assert!((db(measure(DelayInterpolation::Cubic, 10.5, 4).0) + 1.07).abs() < 0.01);
    }

    #[test]
    fn thiran_is_allpass_and_delays_by_the_requested_amount() {
        for delay in [1.5, 2.25, 7.5, 10.9, 31.0, 40.01] {
            // At a low frequency (240 Hz at 48 kHz) the delay is the one
            // asked for, and the level is untouched.
            let (gain, measured) = measure(DelayInterpolation::Thiran, delay, 200);
            assert!((gain - 1.0).abs() < 1e-12, "delay {delay}: gain {gain}");
            assert!(
                apart(measured, delay, 200) < 1e-3,
                "delay {delay}: measured {measured}"
            );

            // Higher up the level is still untouched, and the delay is the
            // one the filter's phase response predicts.
            for period in [8, 5] {
                let omega = TAU / period as f64;
                let whole = (delay - 0.5).floor();
                let remainder = delay - whole;
                let eta = (1.0 - remainder) / (1.0 + remainder);
                let lead = (eta * omega.sin()).atan2(1.0 + eta * omega.cos());
                let expected = whole + 1.0 - 2.0 * lead / omega;

                let (gain, measured) = measure(DelayInterpolation::Thiran, delay, period);
                assert!((gain - 1.0).abs() < 1e-12, "delay {delay}: gain {gain}");
                assert!(
                    apart(measured, expected, period) < 1e-9,
                    "delay {delay}, period {period}: {measured} against {expected}"
                );
            }
        }
    }

    #[test]
    fn a_slowly_moving_delay_is_followed_without_a_click() {
        // A 1 kHz sine at 48 kHz, read at a delay that glides from 10 to 14
        // samples in a second. That takes the all-pass across four of the
        // places where it takes on another whole sample of delay. The
        // result is compared with the sine delayed by exactly that much.
        let worst_error = |mode| {
            let omega = TAU * 1_000.0 / 48_000.0;
            let mut line = DelayLine::<f64>::new(64).with_interpolation(mode);
            let mut worst = 0.0f64;
            for n in 0..48_000 {
                let delay = 10.0 + 4.0 * f64::from(n) / 48_000.0;
                let output = line.read_interpolated(delay);
                line.push((omega * f64::from(n)).sin());
                if n >= 100 {
                    let ideal = (omega * (f64::from(n) - delay)).sin();
                    worst = worst.max((output - ideal).abs());
                }
            }
            worst
        };
        // Measured: 0.00035 for the all-pass, 0.0021 for a straight line,
        // which loses a little level, and 0.000007 for the cubic.
        let thiran = worst_error(DelayInterpolation::Thiran);
        assert!(thiran < 0.001, "the all-pass is off by {thiran}");
        let linear = worst_error(DelayInterpolation::Linear);
        assert!(linear < 0.003, "linear is off by {linear}");
        let cubic = worst_error(DelayInterpolation::Cubic);
        assert!(cubic < 2e-5, "cubic is off by {cubic}");
        // Without interpolation the delay moves in steps of a sample, each
        // of which is a jump in the output: 0.065 here.
        let stepped = worst_error(DelayInterpolation::None);
        assert!(stepped > 0.05, "stepping is only off by {stepped}");
    }

    #[test]
    fn no_delay_or_line_size_makes_a_read_fail() {
        for max_delay in [0, 1, 2, 3, 5, 64] {
            for mode in MODES {
                let mut line = DelayLine::<f64>::new(max_delay).with_interpolation(mode);
                for n in 0..200 {
                    line.push(f64::from(n % 7) - 3.0);
                    for delay in [
                        f64::NAN,
                        f64::NEG_INFINITY,
                        -1.0,
                        0.0,
                        0.3,
                        1.0,
                        1.2,
                        1.5,
                        1.99,
                        2.0,
                        max_delay as f64 - 0.01,
                        max_delay as f64,
                        max_delay as f64 + 0.5,
                        1e30,
                        f64::INFINITY,
                    ] {
                        let read = line.read_interpolated(delay);
                        assert!(
                            read.is_finite() && read.abs() < 100.0,
                            "{mode:?}, line of {max_delay}, delay {delay}: {read}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn clearing_also_clears_the_allpass_memory() {
        let mut line = DelayLine::<f64>::new(16).with_interpolation(DelayInterpolation::Thiran);
        for n in 0..50 {
            line.read_interpolated(4.3);
            line.push(f64::from(n));
        }
        line.clear();
        assert_eq!(line.read_interpolated(4.3), 0.0);
    }

    #[test]
    fn an_echo_between_samples_uses_the_chosen_interpolation() {
        // A sample rate that makes the delay time exact in binary.
        let spec = ProcessSpec {
            sample_rate: 1_024.0,
            max_block_size: 32,
            num_channels: 1,
        };
        // 10.5 samples: an impulse comes back spread over its neighbours
        // with the weights of each method.
        let impulse_response = |interpolation| {
            let mut delay = Delay::<f64>::new(0.05)
                .with_time(10.5 / 1_024.0)
                .with_mix(1.0)
                .with_interpolation(interpolation);
            delay.prepare(&spec);
            let mut buffer = AudioBuffer::<f64>::new(1, 32);
            buffer.channel_mut(0)[0] = 1.0;
            delay.process(&mut buffer);
            buffer.channel(0).to_vec()
        };
        let near = |response: &[f64], expected: &[(usize, f64)]| {
            response.iter().enumerate().all(|(frame, &sample)| {
                let wanted = expected
                    .iter()
                    .find(|(at, _)| *at == frame)
                    .map_or(0.0, |(_, value)| *value);
                (sample - wanted).abs() < 1e-12
            })
        };
        assert!(near(
            &impulse_response(DelayInterpolation::None),
            &[(11, 1.0)]
        ));
        assert!(near(
            &impulse_response(DelayInterpolation::Linear),
            &[(10, 0.5), (11, 0.5)]
        ));
        assert!(near(
            &impulse_response(DelayInterpolation::Cubic),
            &[(9, -0.0625), (10, 0.5625), (11, 0.5625), (12, -0.0625)]
        ));
        // The all-pass spreads the impulse over all time, with all of its
        // energy kept.
        let energy: f64 = impulse_response(DelayInterpolation::Thiran)
            .iter()
            .map(|sample| sample * sample)
            .sum();
        assert!((energy - 1.0).abs() < 1e-9, "energy {energy}");
    }

    #[test]
    fn an_echo_does_not_depend_on_block_size() {
        let spec = ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 256,
            num_channels: 2,
        };
        for mode in MODES {
            let render = |block: usize| {
                let mut delay = Delay::<f32>::new(0.02)
                    .with_time(0.003_217)
                    .with_feedback(0.6)
                    .with_interpolation(mode);
                delay.prepare(&spec);
                let mut signal = AudioBuffer::<f32>::new(2, 3_000);
                for (index, channel) in signal.iter_channels_mut().enumerate() {
                    for (n, sample) in channel.iter_mut().enumerate() {
                        *sample = ((n * (7 + index)) % 23) as f32 / 11.5 - 1.0;
                    }
                }
                let mut start = 0;
                while start < 3_000 {
                    let end = (start + block).min(3_000);
                    signal.windowed(start..end, |piece| delay.process(piece));
                    start = end;
                }
                signal
            };
            let reference = render(256);
            assert!(reference.peak() > 0.5);
            for block in [1, 7, 100, 255] {
                assert!(render(block) == reference, "{mode:?}, blocks of {block}");
            }
        }
    }

    #[test]
    fn a_nan_read_gives_the_most_recent_sample() {
        let mut line = DelayLine::<f64>::new(16);
        for n in 0..50 {
            line.push(f64::from(n));
            assert_eq!(line.read(f64::NAN), f64::from(n));
        }
    }

    #[test]
    fn a_nan_time_keeps_the_output_finite() {
        stays_finite_after(|d| d.set_time(f64::NAN));

        let mut delay = Delay::<f64>::new(0.1);
        delay.set_time(f64::NAN);
        assert!(delay.time_seconds.is_finite(), "a NaN time is not stored");
    }

    #[test]
    fn a_nan_feedback_keeps_the_output_finite() {
        stays_finite_after(|d| d.set_feedback(f64::NAN));
    }

    #[test]
    fn a_nan_mix_keeps_the_output_finite() {
        stays_finite_after(|d| d.set_mix(f64::NAN));
    }

    /// Runs a delay with real settings, applies `bad`, and checks that the
    /// output stays finite afterwards.
    fn stays_finite_after(bad: impl Fn(&mut Delay<f64>)) {
        let mut delay = Delay::<f64>::new(0.1)
            .with_time(0.01)
            .with_feedback(0.5)
            .with_mix(0.5);
        delay.prepare(&ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 512,
            num_channels: 1,
        });

        let mut signal = AudioBuffer::<f64>::new(1, 512);
        signal.channel_mut(0)[0] = 1.0;
        delay.process(&mut signal);

        bad(&mut delay);

        // Several more blocks, so a NaN would have time to spread through the feedback.
        for _ in 0..8 {
            let mut block = AudioBuffer::<f64>::new(1, 512);
            delay.process(&mut block);
            assert!(
                block.peak().is_finite(),
                "after {block:?} the output is not finite"
            );
        }
    }
}
