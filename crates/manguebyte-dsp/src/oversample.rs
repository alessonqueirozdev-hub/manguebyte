use manguebyte_core::{AudioBuffer, Sample};

use crate::fir;
use crate::{ProcessSpec, Processor};

/// Taps of the half-band filter at each stage, first stage first.
///
/// The first stage does the hard work: its transition band has to start
/// above the audible range and be over soon after half the original sample
/// rate. With 95 taps the response is flat to about 0.435 of the original
/// rate, which is 20.9 kHz at 48 kHz. Later stages see a signal that is
/// already band-limited to a small part of their range, so shorter filters
/// do. Each count is chosen so that the stage's delay is a whole number of
/// samples at the original rate.
const STAGE_TAPS: [usize; 4] = [95, 25, 17, 17];

/// Stopband of the half-band filters, in decibels.
const ATTENUATION_DB: f64 = 96.0;

/// One doubling of the sample rate and the matching halving.
#[derive(Debug, Clone)]
struct Stage<S> {
    /// The filter, already doubled for interpolation.
    up_taps: Vec<S>,
    down_taps: Vec<S>,
    /// Recent low-rate input, per channel, for interpolating.
    up_history: Vec<Vec<S>>,
    up_position: usize,
    /// Recent high-rate input, per channel, for decimating.
    down_history: Vec<Vec<S>>,
    down_position: usize,
}

impl<S: Sample> Stage<S> {
    fn new(taps: usize, channels: usize) -> Self {
        let filter = fir::lowpass(taps, 0.25, ATTENUATION_DB);
        Self {
            // Inserting zeros between samples halves the level, so the
            // interpolating filter makes up the difference.
            up_taps: filter.iter().map(|&h| S::from_f64(2.0 * h)).collect(),
            down_taps: filter.iter().map(|&h| S::from_f64(h)).collect(),
            up_history: vec![vec![S::ZERO; taps.div_ceil(2)]; channels],
            up_position: 0,
            down_history: vec![vec![S::ZERO; taps]; channels],
            down_position: 0,
        }
    }

    fn clear(&mut self) {
        for history in self.up_history.iter_mut().chain(&mut self.down_history) {
            history.fill(S::ZERO);
        }
    }

    /// Doubles the rate of `low` into `high`, which must be twice as long.
    fn upsample(&mut self, low: &AudioBuffer<S>, high: &mut AudioBuffer<S>) {
        let mut position = self.up_position;
        for (channel, history) in self.up_history.iter_mut().enumerate() {
            position = self.up_position;
            let len = history.len();
            let out = high.channel_mut(channel);
            for (frame, &sample) in low.channel(channel).iter().enumerate() {
                history[position] = sample;
                // Even taps make the output sample that coincides with an
                // input sample; odd taps make the one in between.
                let (mut even, mut odd) = (S::ZERO, S::ZERO);
                let mut index = position;
                for pair in self.up_taps.chunks(2) {
                    let past = history[index];
                    even += pair[0] * past;
                    if let Some(&tap) = pair.get(1) {
                        odd += tap * past;
                    }
                    index = if index == 0 { len - 1 } else { index - 1 };
                }
                out[2 * frame] = even;
                out[2 * frame + 1] = odd;
                position = (position + 1) % len;
            }
        }
        self.up_position = position;
    }

    /// Halves the rate of `high` into `low`, which must be half as long.
    fn downsample(&mut self, high: &AudioBuffer<S>, low: &mut AudioBuffer<S>) {
        let mut position = self.down_position;
        for (channel, history) in self.down_history.iter_mut().enumerate() {
            position = self.down_position;
            let len = history.len();
            let input = high.channel(channel);
            for (frame, out) in low.channel_mut(channel).iter_mut().enumerate() {
                // The output is taken on the first sample of each pair. On
                // the second it would land half an original sample late.
                history[position] = input[2 * frame];
                let mut sum = S::ZERO;
                let mut index = position;
                for &tap in &self.down_taps {
                    sum += tap * history[index];
                    index = if index == 0 { len - 1 } else { index - 1 };
                }
                *out = sum;
                position = (position + 1) % len;
                history[position] = input[2 * frame + 1];
                position = (position + 1) % len;
            }
        }
        self.down_position = position;
    }
}

/// Runs a processor at a multiple of the sample rate.
///
/// Anything that bends a waveform (a clipper, a saturator, a waveshaper)
/// creates new frequencies above the ones that went in. Those that land
/// above half the sample rate do not disappear: they fold back down as
/// *aliasing*, tones unrelated to the music. Running the processor at a
/// higher rate gives the new frequencies room, and filtering before coming
/// back down removes them.
///
/// The wrapper raises the rate with half-band filters, runs the inner
/// processor, and lowers it again. The filters are linear-phase, so the
/// only side effect is a delay, reported by
/// [`latency_samples`](Processor::latency_samples).
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Oversampled, ProcessSpec, Processor, Waveshaper};
///
/// // A hard clipper, run at four times the sample rate.
/// let mut clipper = Oversampled::<_, f32>::new(Waveshaper::hard_clip().with_drive_db(18.0), 4);
/// clipper.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// assert_eq!(clipper.latency_samples(), 53);
///
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// clipper.process(&mut block);
/// ```
///
/// Events are not passed to the inner processor, because their positions
/// would need rescaling, and neither is a sidechain, which runs at the outer
/// rate: the inner processor is called through
/// [`process`](Processor::process), so a [`Compressor`](crate::Compressor)
/// inside keys on its own input. Wrap the part that distorts, not a whole
/// instrument.
#[derive(Debug, Clone)]
pub struct Oversampled<P, S: Sample = f32> {
    inner: P,
    factor: usize,
    stages: Vec<Stage<S>>,
    /// The signal at each stage's higher rate.
    buffers: Vec<AudioBuffer<S>>,
    latency: usize,
}

impl<S: Sample, P: Processor<S>> Oversampled<P, S> {
    /// Wraps `inner` to run at `factor` times the sample rate.
    ///
    /// # Panics
    ///
    /// Panics unless `factor` is 2, 4, 8 or 16.
    pub fn new(inner: P, factor: usize) -> Self {
        assert!(
            matches!(factor, 2 | 4 | 8 | 16),
            "oversampling factor must be 2, 4, 8 or 16, got {factor}"
        );
        let stage_count = factor.trailing_zeros() as usize;
        // A stage delays by `(taps - 1) / 2` samples at its own lower rate.
        let latency = STAGE_TAPS[..stage_count]
            .iter()
            .enumerate()
            .map(|(stage, &taps)| ((taps - 1) / 2) >> stage)
            .sum();
        Self {
            inner,
            factor,
            stages: Vec::new(),
            buffers: Vec::new(),
            latency,
        }
    }

    /// How many times the sample rate is multiplied.
    pub fn factor(&self) -> usize {
        self.factor
    }

    /// The wrapped processor.
    pub fn inner(&self) -> &P {
        &self.inner
    }

    /// The wrapped processor, for changing its settings.
    pub fn inner_mut(&mut self) -> &mut P {
        &mut self.inner
    }
}

impl<S: Sample, P: Processor<S>> Processor<S> for Oversampled<P, S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        let stage_count = self.factor.trailing_zeros() as usize;
        self.stages = STAGE_TAPS[..stage_count]
            .iter()
            .map(|&taps| Stage::new(taps, spec.num_channels))
            .collect();
        self.buffers = (1..=stage_count)
            .map(|stage| AudioBuffer::new(spec.num_channels, spec.max_block_size << stage))
            .collect();
        self.inner.prepare(&ProcessSpec {
            sample_rate: spec.sample_rate * self.factor as f64,
            max_block_size: spec.max_block_size * self.factor,
            num_channels: spec.num_channels,
        });
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames();
        let Some(limit) = self.buffers.first().map(|first| first.capacity() / 2) else {
            return;
        };
        if frames > limit {
            // Longer than prepared for: run it in pieces.
            let mut start = 0;
            while start < frames {
                let end = (start + limit).min(frames);
                buffer.windowed(start..end, |piece| self.process(piece));
                start = end;
            }
            return;
        }

        // Up: each stage reads the one before it. The first reads the
        // caller's buffer.
        for stage in 0..self.stages.len() {
            let (lower, higher) = self.buffers.split_at_mut(stage);
            let high = &mut higher[0];
            high.set_num_frames(frames << (stage + 1));
            let low: &AudioBuffer<S> = match lower.last() {
                Some(previous) => previous,
                None => buffer,
            };
            self.stages[stage].upsample(low, high);
        }

        if let Some(top) = self.buffers.last_mut() {
            self.inner.process(top);
        }

        // And down again, in the opposite order.
        for stage in (0..self.stages.len()).rev() {
            let (lower, higher) = self.buffers.split_at_mut(stage);
            let high = &higher[0];
            let low: &mut AudioBuffer<S> = match lower.last_mut() {
                Some(previous) => previous,
                None => buffer,
            };
            self.stages[stage].downsample(high, low);
        }
    }

    fn reset(&mut self) {
        for stage in &mut self.stages {
            stage.clear();
        }
        self.inner.reset();
    }

    fn latency_samples(&self) -> usize {
        self.latency + self.inner.latency_samples() / self.factor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::window::{Symmetry, Window};

    const RATE: f64 = 48_000.0;

    fn spec(frames: usize) -> ProcessSpec {
        ProcessSpec {
            sample_rate: RATE,
            max_block_size: frames,
            num_channels: 1,
        }
    }

    /// Does nothing, so only the oversampler itself is heard.
    struct Wire;
    impl Processor<f64> for Wire {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f64>) {}
    }

    /// Cubes its input, which adds exactly one new frequency: three times
    /// the one that went in.
    struct Cube;
    impl Processor<f64> for Cube {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f64>) {
            for sample in buffer.channel_mut(0) {
                *sample = 4.0 * *sample * *sample * *sample;
            }
        }
    }

    /// Clips hard, which makes strong harmonics.
    struct Clip;
    impl Processor<f64> for Clip {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, buffer: &mut AudioBuffer<f64>) {
            for sample in buffer.channel_mut(0) {
                *sample = (*sample * 4.0).clamp(-1.0, 1.0);
            }
        }
    }

    fn sine(frequency: f64, frames: usize) -> AudioBuffer<f64> {
        let mut buffer = AudioBuffer::new(1, frames);
        for (n, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
            *sample = 0.5 * (core::f64::consts::TAU * frequency * n as f64 / RATE).sin();
        }
        buffer
    }

    /// Level in decibels of the strongest component within 100 Hz of
    /// `frequency`.
    fn level_db(signal: &[f64], frequency: f64) -> f64 {
        let len = signal.len();
        let mut windowed = signal.to_vec();
        Window::BlackmanHarris.apply(&mut windowed, Symmetry::Periodic);
        let mut fft = RealFft::new(len);
        let mut spectrum = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(&windowed, &mut spectrum);
        let bin = |hz: f64| (hz.max(0.0) * len as f64 / RATE).round() as usize;
        let peak = (bin(frequency - 100.0)..=bin(frequency + 100.0))
            .map(|index| spectrum[index].norm())
            .fold(0.0, f64::max);
        // A sine of amplitude 1 peaks at half the length, times the window's
        // average value.
        let full_scale =
            len as f64 / 2.0 * Window::BlackmanHarris.coherent_gain(len, Symmetry::Periodic);
        20.0 * (peak / full_scale).max(1e-300).log10()
    }

    #[test]
    fn latency_is_exact_for_every_factor() {
        for (factor, expected) in [(2, 47), (4, 53), (8, 55), (16, 56)] {
            let mut oversampler = Oversampled::<_, f64>::new(Wire, factor);
            oversampler.prepare(&spec(256));
            assert_eq!(oversampler.latency_samples(), expected);

            let mut impulse = AudioBuffer::<f64>::new(1, 256);
            impulse.channel_mut(0)[10] = 1.0;
            oversampler.process(&mut impulse);
            let peak = (0..256)
                .max_by(|&a, &b| {
                    impulse.channel(0)[a]
                        .abs()
                        .total_cmp(&impulse.channel(0)[b].abs())
                })
                .unwrap();
            assert_eq!(peak, 10 + expected, "factor {factor}");
            // A little under one: the top few kilohertz below half the
            // sample rate are in the filters' transition band.
            let height = impulse.channel(0)[peak];
            assert!((0.9..=1.0).contains(&height), "factor {factor}: {height}");
        }
    }

    #[test]
    fn the_audible_band_passes_unchanged() {
        for factor in [2, 4, 8] {
            // Frequencies that fall exactly on a bin of the 4096-point
            // measurement, so the reading is not blurred by the window:
            // about 200 Hz, 1 kHz, 10 kHz and 19 kHz.
            for bin in [17.0, 85.0, 853.0, 1_621.0] {
                let frequency = bin * RATE / 4_096.0;
                let mut oversampler = Oversampled::<_, f64>::new(Wire, factor);
                oversampler.prepare(&spec(8_192));
                let mut signal = sine(frequency, 8_192);
                oversampler.process(&mut signal);
                // The input sine has an amplitude of 0.5: -6.02 dB.
                let level = level_db(&signal.channel(0)[1_024..5_120], frequency);
                assert!(
                    (level + 6.02).abs() < 0.05,
                    "{factor}x at {frequency} Hz: {level:.3} dB"
                );
            }
        }
    }

    #[test]
    fn oversampling_removes_aliasing() {
        // Cubing a 10 kHz sine adds a component at 30 kHz. At 48 kHz that is
        // above half the sample rate and folds down to 48 - 30 = 18 kHz, a
        // tone that has nothing to do with the input.
        let alias = |factor: Option<usize>| {
            let mut signal = sine(10_000.0, 8_192);
            match factor {
                None => Cube.process(&mut signal),
                Some(factor) => {
                    let mut oversampled = Oversampled::<_, f64>::new(Cube, factor);
                    oversampled.prepare(&spec(8_192));
                    oversampled.process(&mut signal);
                }
            }
            level_db(&signal.channel(0)[1_024..5_120], 18_000.0)
        };
        let plain = alias(None);
        assert!(
            plain > -25.0,
            "the test needs audible aliasing: {plain:.1} dB"
        );
        for factor in [2, 4, 8, 16] {
            let level = alias(Some(factor));
            assert!(
                level < -100.0,
                "{factor}x: {level:.1} dB against {plain:.1} dB"
            );
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut oversampled = Oversampled::<_, f64>::new(Clip, 4);
            oversampled.prepare(&spec(64));
            let mut signal = sine(3_000.0, 1_000);
            let mut start = 0;
            while start < 1_000 {
                let end = (start + block).min(1_000);
                signal.windowed(start..end, |piece| oversampled.process(piece));
                start = end;
            }
            signal
        };
        let reference = render(64);
        assert!(render(1) == reference);
        assert!(render(37) == reference);
        // Longer than it was prepared for.
        assert!(render(1_000) == reference);
    }

    #[test]
    fn reset_clears_the_filters() {
        let mut oversampler = Oversampled::<_, f64>::new(Wire, 4);
        oversampler.prepare(&spec(256));
        let mut signal = sine(1_000.0, 256);
        oversampler.process(&mut signal);
        oversampler.reset();
        let mut silence = AudioBuffer::<f64>::new(1, 256);
        oversampler.process(&mut silence);
        assert_eq!(silence.peak(), 0.0);
    }
}
