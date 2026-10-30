use core::f64::consts::{FRAC_PI_4, PI, SQRT_2};

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::delay::DelayLine;
use crate::numeric::{glide_to, limit};
use crate::{ProcessSpec, Processor};

/// How many grains can sound at once.
const POOL: usize = 64;

/// Limits of the settings.
const MAX_POSITION_SECONDS: f64 = 1.0;
const MAX_SPRAY_SECONDS: f64 = 0.5;
const MIN_GRAIN_SECONDS: f64 = 0.005;
const MAX_GRAIN_SECONDS: f64 = 0.5;
const MIN_DENSITY_HZ: f64 = 0.5;
const MAX_DENSITY_HZ: f64 = 200.0;
const MAX_PITCH_SEMITONES: f64 = 24.0;
const MAX_FEEDBACK: f64 = 0.95;

/// How long a change of feedback or mix takes.
const GLIDE_SECONDS: f64 = 0.02;

/// One grain: a stretch of the delay line read at its own speed through a
/// window.
#[derive(Debug, Clone, Copy)]
struct Grain {
    /// How far behind the input the grain starts reading, in samples.
    delay: f64,
    /// How much the delay changes per sample: `1 - pitch ratio`.
    drift: f64,
    /// How long the grain lasts, in samples.
    length: f64,
    /// Samples since it started.
    age: f64,
    /// The gains of the first two channels, from the grain's place in the
    /// stereo field.
    left: f64,
    right: f64,
}

impl Grain {
    const IDLE: Self = Self {
        delay: 0.0,
        drift: 0.0,
        length: 0.0,
        age: 0.0,
        left: 0.0,
        right: 0.0,
    };
}

/// A granular delay: short grains of the delayed signal, each read at its
/// own pitch through a window, scattered in time and in the stereo field
/// and overlapped into a cloud.
///
/// - **Grains** start at a steady rate, the density, in grains per second.
///   Each lasts the grain size and is shaped by a periodic Hann window,
///   `sin²(π t / size)`.
/// - **Position** and **spray**: a grain starts reading the signal from
///   the position behind the input, plus a random amount up to the spray.
/// - **Pitch**: each grain is read faster or slower by its own ratio: the
///   pitch set, in semitones, plus a random amount up to the pitch spray
///   either way. A grain read faster than the input is written starts
///   further back, so that it never overtakes the write.
/// - **Stereo spread**: each grain is placed at random up to the spread
///   either side of the centre of the first two channels, by the
///   equal-power law.
/// - The cloud is scaled by `2 / (density × size)` once grains overlap
///   more than twice, so that its level does not climb with the overlap. At
///   an overlap of exactly two, with no spray and no pitch, the windows add
///   up to one and the output is the input, delayed by the position.
/// - **Feedback** writes the cloud back into the delay line, through a
///   `tanh` so that it cannot run away, and the output is `dry * (1 - mix)
///   + cloud * mix`.
///
/// The grains are kept in a pool of 64 made when the delay is made; a grain
/// due when all 64 are sounding is not started. Nothing is allocated after
/// [`prepare`](Processor::prepare). The random choices come from a seed,
/// so a render repeats exactly, whatever the block size, and
/// [`reset`](Processor::reset) starts the sequence again.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{GranularDelay, ProcessSpec, Processor};
///
/// let mut cloud = GranularDelay::<f32>::new()
///     .with_grain_ms(80.0)
///     .with_density_hz(40.0)
///     .with_pitch_semitones(12.0)
///     .with_spray_ms(100.0);
/// cloud.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
///
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// for _ in 0..100 {
///     cloud.process(&mut block);
/// }
/// assert!(cloud.active_grains() > 0);
/// ```
///
/// # Limits
///
/// The position is limited to 1 s, the spray to 0.5 s, the grain size to
/// 5 to 500 ms, the density to 0.5 to 200 grains a second, the pitch and its
/// spray to two octaves either way, the stereo spread to `0.0..=1.0` and
/// the feedback to `-0.95..=0.95`. Grains start on a steady clock: there is
/// no random spacing in time, only in position, so grains played at another
/// pitch, each starting where the one before did not leave off, put the
/// cloud's energy on multiples of the density; spray and pitch spray blur
/// that. A change of a grain's
/// settings is heard from the next grain on.
#[derive(Debug, Clone)]
pub struct GranularDelay<S: Sample = f32> {
    position_ms: f64,
    spray_ms: f64,
    grain_ms: f64,
    density_hz: f64,
    pitch_semitones: f64,
    pitch_spray: f64,
    spread: f64,
    feedback: f64,
    mix: f64,
    seed: u64,
    sample_rate: f64,
    lines: Vec<DelayLine<S>>,
    grains: [Grain; POOL],
    active: usize,
    /// Samples since the last grain started.
    since: f64,
    random: u64,
    feedback_now: Smoothed<f64>,
    mix_now: Smoothed<f64>,
}

impl<S: Sample> Default for GranularDelay<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> GranularDelay<S> {
    /// The most grains that sound at once.
    pub const MAX_GRAINS: usize = POOL;

    /// Creates a granular delay: grains of 60 ms, 30 a second, from 250 ms
    /// back with 50 ms of spray, at their own pitch, spread half-way across
    /// the stereo field, without feedback, mixed half and half.
    pub fn new() -> Self {
        let mut delay = Self {
            position_ms: 250.0,
            spray_ms: 50.0,
            grain_ms: 60.0,
            density_hz: 30.0,
            pitch_semitones: 0.0,
            pitch_spray: 0.0,
            spread: 0.5,
            feedback: 0.0,
            mix: 0.5,
            seed: 0x6EA1_5EED,
            sample_rate: 44_100.0,
            lines: Vec::new(),
            grains: [Grain::IDLE; POOL],
            active: 0,
            since: 0.0,
            random: 1,
            feedback_now: Smoothed::new(0.0),
            mix_now: Smoothed::new(0.5),
        };
        delay.restart();
        delay
    }

    /// Sets how far behind the input grains start reading, in
    /// milliseconds, limited to 1 s.
    pub fn set_position_ms(&mut self, milliseconds: f64) {
        self.position_ms = limit(milliseconds, 0.0, MAX_POSITION_SECONDS * 1e3);
    }

    /// Sets the most a grain's start is moved further back at random, in
    /// milliseconds, limited to 0.5 s.
    pub fn set_spray_ms(&mut self, milliseconds: f64) {
        self.spray_ms = limit(milliseconds, 0.0, MAX_SPRAY_SECONDS * 1e3);
    }

    /// Sets how long each grain lasts, in milliseconds, limited to 5 to
    /// 500 ms.
    pub fn set_grain_ms(&mut self, milliseconds: f64) {
        self.grain_ms = limit(
            milliseconds,
            MIN_GRAIN_SECONDS * 1e3,
            MAX_GRAIN_SECONDS * 1e3,
        );
    }

    /// Sets how many grains start each second, limited to 0.5 to 200.
    pub fn set_density_hz(&mut self, hertz: f64) {
        self.density_hz = limit(hertz, MIN_DENSITY_HZ, MAX_DENSITY_HZ);
    }

    /// Sets the pitch of every grain, in semitones, limited to ±24.
    pub fn set_pitch_semitones(&mut self, semitones: f64) {
        self.pitch_semitones = Self::semitones(semitones);
    }

    /// Sets the most a grain's pitch is moved at random either way, in
    /// semitones, limited to 24.
    pub fn set_pitch_spray(&mut self, semitones: f64) {
        self.pitch_spray = limit(semitones, 0.0, MAX_PITCH_SEMITONES);
    }

    /// Sets how far either side of the centre grains are placed at random,
    /// from `0.0` (all in the centre) to `1.0` (anywhere).
    pub fn set_stereo_spread(&mut self, spread: f64) {
        self.spread = limit(spread, 0.0, 1.0);
    }

    /// Sets how much of the cloud is written back into the delay line,
    /// limited to `-0.95..=0.95`.
    pub fn set_feedback(&mut self, feedback: f64) {
        self.feedback = if feedback.is_nan() {
            0.0
        } else {
            limit(feedback, -MAX_FEEDBACK, MAX_FEEDBACK)
        };
        glide_to(&mut self.feedback_now, self.feedback);
    }

    /// Sets the balance between the untouched signal (`0.0`) and the cloud
    /// (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.mix_now, self.mix);
    }

    /// Chooses the sequence of random choices, from the next
    /// [`reset`](Processor::reset) or `prepare`.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
    }

    /// Builder form of [`set_position_ms`](Self::set_position_ms).
    #[must_use]
    pub fn with_position_ms(mut self, milliseconds: f64) -> Self {
        self.set_position_ms(milliseconds);
        self
    }

    /// Builder form of [`set_spray_ms`](Self::set_spray_ms).
    #[must_use]
    pub fn with_spray_ms(mut self, milliseconds: f64) -> Self {
        self.set_spray_ms(milliseconds);
        self
    }

    /// Builder form of [`set_grain_ms`](Self::set_grain_ms).
    #[must_use]
    pub fn with_grain_ms(mut self, milliseconds: f64) -> Self {
        self.set_grain_ms(milliseconds);
        self
    }

    /// Builder form of [`set_density_hz`](Self::set_density_hz).
    #[must_use]
    pub fn with_density_hz(mut self, hertz: f64) -> Self {
        self.set_density_hz(hertz);
        self
    }

    /// Builder form of [`set_pitch_semitones`](Self::set_pitch_semitones).
    #[must_use]
    pub fn with_pitch_semitones(mut self, semitones: f64) -> Self {
        self.set_pitch_semitones(semitones);
        self
    }

    /// Builder form of [`set_pitch_spray`](Self::set_pitch_spray).
    #[must_use]
    pub fn with_pitch_spray(mut self, semitones: f64) -> Self {
        self.set_pitch_spray(semitones);
        self
    }

    /// Builder form of [`set_stereo_spread`](Self::set_stereo_spread).
    #[must_use]
    pub fn with_stereo_spread(mut self, spread: f64) -> Self {
        self.set_stereo_spread(spread);
        self
    }

    /// Builder form of [`set_feedback`](Self::set_feedback).
    #[must_use]
    pub fn with_feedback(mut self, feedback: f64) -> Self {
        self.set_feedback(feedback);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    #[must_use]
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// Builder form of [`set_seed`](Self::set_seed).
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.set_seed(seed);
        self
    }

    /// How many grains are sounding.
    pub fn active_grains(&self) -> usize {
        self.active
    }

    fn semitones(semitones: f64) -> f64 {
        if semitones.is_nan() {
            0.0
        } else {
            limit(semitones, -MAX_PITCH_SEMITONES, MAX_PITCH_SEMITONES)
        }
    }

    /// The length of the delay lines: the furthest back a grain can start,
    /// and as far again as the slowest grain falls behind while it plays.
    fn line_length(sample_rate: f64) -> usize {
        let fastest = (MAX_PITCH_SEMITONES * 2.0 / 12.0).exp2();
        let slowest = (-MAX_PITCH_SEMITONES * 2.0 / 12.0).exp2();
        let ahead = (fastest - 1.0) * MAX_GRAIN_SECONDS;
        let behind = (1.0 - slowest) * MAX_GRAIN_SECONDS;
        let seconds = MAX_POSITION_SECONDS + MAX_SPRAY_SECONDS + ahead + behind;
        (seconds * sample_rate).ceil() as usize + 8
    }

    /// A number from 0 up to but not including 1.
    fn unit(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64
    }

    fn restart(&mut self) {
        self.random = self.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        self.grains = [Grain::IDLE; POOL];
        self.active = 0;
        // The first grain starts on the first sample.
        self.since = f64::INFINITY;
        self.feedback_now.set_immediate(self.feedback);
        self.mix_now.set_immediate(self.mix);
    }

    /// Starts a grain, if there is room for one.
    fn spawn(&mut self) {
        // The random choices are drawn whether or not the grain starts, so
        // that a full pool does not change the grains after it.
        let (spray, pitch, place) = (self.unit(), self.unit(), self.unit());
        if self.active == POOL {
            return;
        }
        let rate = self.sample_rate;
        let length = (self.grain_ms * 1e-3 * rate).round().max(2.0);
        let semitones =
            Self::semitones(self.pitch_semitones + self.pitch_spray * (2.0 * pitch - 1.0));
        let ratio = (semitones / 12.0).exp2();
        let mut delay = (self.position_ms + self.spray_ms * spray) * 1e-3 * rate;
        // A grain read faster than the input is written starts far enough
        // back not to overtake the write while it plays.
        delay = delay.max(2.0 + (ratio - 1.0).max(0.0) * length);
        let pan = self.spread * (2.0 * place - 1.0);
        // Equal power, scaled so that the centre is exactly 1 on both sides.
        let (left, right) = if pan == 0.0 {
            (1.0, 1.0)
        } else {
            let angle = FRAC_PI_4 * (pan + 1.0);
            (angle.cos() * SQRT_2, angle.sin() * SQRT_2)
        };
        self.grains[self.active] = Grain {
            delay,
            drift: 1.0 - ratio,
            length,
            age: 0.0,
            left,
            right,
        };
        self.active += 1;
    }

    /// How much the cloud is scaled by: down by the overlap beyond two.
    fn scale(&self) -> f64 {
        let overlap = self.density_hz * self.grain_ms * 1e-3;
        2.0 / overlap.max(2.0)
    }
}

impl<S: Sample> Processor<S> for GranularDelay<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        let length = Self::line_length(spec.sample_rate);
        self.lines = (0..spec.num_channels)
            .map(|_| DelayLine::new(length))
            .collect();
        self.feedback_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.mix_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.restart();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let channels = buffer.num_channels().min(self.lines.len());
        let interval = self.sample_rate / self.density_hz;
        let scale = self.scale();
        for frame in 0..buffer.num_frames() {
            if self.since >= interval {
                self.since = if self.since.is_finite() {
                    (self.since - interval).min(interval)
                } else {
                    0.0
                };
                self.spawn();
            }
            self.since += 1.0;
            let feedback = self.feedback_now.next_value();
            let mix = self.mix_now.next_value();
            for channel in 0..channels {
                let line = &mut self.lines[channel];
                let mut cloud = 0.0;
                for grain in &self.grains[..self.active] {
                    let window = (PI * grain.age / grain.length).sin();
                    let gain = match channel {
                        0 => grain.left,
                        1 => grain.right,
                        _ => 1.0,
                    };
                    let delay = grain.delay + grain.drift * grain.age;
                    cloud += line.read_cubic(delay).to_f64() * window * window * gain;
                }
                cloud *= scale;
                let sample = &mut buffer.channel_mut(channel)[frame];
                let input = sample.to_f64();
                line.push(S::from_f64(input + (feedback * cloud).tanh()));
                *sample = S::from_f64(input * (1.0 - mix) + cloud * mix);
            }
            // Age the grains, and let go of those that have ended.
            let mut index = 0;
            while index < self.active {
                let grain = &mut self.grains[index];
                grain.age += 1.0;
                if grain.age >= grain.length {
                    self.active -= 1;
                    self.grains[index] = self.grains[self.active];
                } else {
                    index += 1;
                }
            }
        }
    }

    fn reset(&mut self) {
        for line in &mut self.lines {
            line.clear();
        }
        self.restart();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn prepared(delay: GranularDelay<f64>, channels: usize) -> GranularDelay<f64> {
        let mut delay = delay;
        delay.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        delay
    }

    /// Grains of 20 ms, 100 a second: an overlap of two, with nothing
    /// random, fully wet.
    fn plain() -> GranularDelay<f64> {
        GranularDelay::new()
            .with_grain_ms(20.0)
            .with_density_hz(100.0)
            .with_spray_ms(0.0)
            .with_pitch_semitones(0.0)
            .with_pitch_spray(0.0)
            .with_stereo_spread(0.0)
            .with_mix(1.0)
    }

    #[test]
    fn an_overlap_of_two_without_randomness_is_an_exact_delay() {
        // 100 ms is 4 800 samples; the windows add up to one from the
        // second grain on, 480 samples in.
        let mut delay = prepared(plain().with_position_ms(100.0), 2);
        let input = noise::<f64>(2, 12_000, 1);
        let mut output = input.clone();
        run_in_blocks(&mut delay, &mut output, 512);
        for channel in 0..2 {
            for n in 4_800 + 480..12_000 {
                let expected = input.channel(channel)[n - 4_800];
                let got = output.channel(channel)[n];
                assert!(
                    (got - expected).abs() < 1e-12,
                    "channel {channel}, frame {n}"
                );
            }
        }
        assert!(delay.active_grains() <= 2);
    }

    /// The amplitude at each bin of the last `len` frames.
    fn spectrum(signal: &[f64], len: usize) -> Vec<f64> {
        let mut fft = RealFft::<f64>::new(len);
        let mut bins = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(&signal[signal.len() - len..], &mut bins);
        bins.iter()
            .map(|bin| 2.0 * bin.norm() / len as f64)
            .collect()
    }

    #[test]
    fn each_grain_plays_at_its_pitch() {
        // A 1 kHz tone through grains an octave up, and an octave and a
        // fifth down: the strongest part of the cloud is at 2 kHz, or near
        // 1 kHz times 2^(-19/12). Grains that start 40 times a second, each
        // where the last did not leave off, put the cloud's energy on
        // multiples of 40 Hz, so it is found on the multiple nearest.
        const LEN: usize = 24_000;
        for (semitones, expected) in [(12.0, 2_000.0), (-19.0, 1_000.0 * (-19.0f64 / 12.0).exp2())]
        {
            let mut delay = prepared(
                plain()
                    .with_grain_ms(50.0)
                    .with_density_hz(40.0)
                    .with_pitch_semitones(semitones),
                1,
            );
            let mut tone = sine::<f64>(1, 2 * LEN, 1_000.0, RATE, 0.5);
            run_in_blocks(&mut delay, &mut tone, 512);
            let bins = spectrum(tone.channel(0), LEN);
            let (loudest, _) =
                bins.iter()
                    .enumerate()
                    .fold((0, 0.0), |(at, peak), (bin, &level)| {
                        if level > peak {
                            (bin, level)
                        } else {
                            (at, peak)
                        }
                    });
            // Bin k is 2k hertz.
            let found = 2.0 * loudest as f64;
            assert!((found - expected).abs() <= 20.0, "{semitones}: {found} Hz");
            assert!(
                db(bins[500] / bins[loudest]) < -20.0,
                "{semitones}: the tone is still there"
            );
        }
    }

    #[test]
    fn spray_scatters_the_grains_within_its_range() {
        // 50 ms of noise, then silence, read back by grains of 10 ms from
        // 50 ms behind with up to 100 ms of spray: nothing comes back before
        // 50 ms or after 210 ms, and different seeds scatter it
        // differently.
        let render = |seed: u64| {
            let mut delay = prepared(
                plain()
                    .with_grain_ms(10.0)
                    .with_density_hz(150.0)
                    .with_position_ms(50.0)
                    .with_spray_ms(100.0)
                    .with_seed(seed),
                1,
            );
            let mut signal = AudioBuffer::<f64>::new(1, 12_000);
            let burst = noise::<f64>(1, 2_400, 6);
            signal.channel_mut(0)[..2_400].copy_from_slice(burst.channel(0));
            run_in_blocks(&mut delay, &mut signal, 512);
            signal
        };
        let first = render(1);
        let heard: Vec<usize> = (0..12_000)
            .filter(|&n| first.channel(0)[n] != 0.0)
            .collect();
        assert!(heard.len() > 1_000);
        assert!(heard[0] >= 2_400, "{}", heard[0]);
        assert!(heard[heard.len() - 1] < 2_400 + 4_800 + 2_400);
        assert!(render(1) == first);
        assert!(render(2) != first);
    }

    #[test]
    fn the_pool_never_grows_past_its_size_and_the_render_repeats() {
        let render = |block: usize| {
            let mut delay = prepared(
                GranularDelay::new()
                    .with_grain_ms(500.0)
                    .with_density_hz(200.0)
                    .with_spray_ms(500.0)
                    .with_pitch_spray(24.0)
                    .with_stereo_spread(1.0)
                    .with_feedback(0.5),
                2,
            );
            let mut signal = noise::<f64>(2, 24_000, 3);
            let mut most = 0;
            let mut start = 0;
            while start < 24_000 {
                let end = (start + block).min(24_000);
                signal.windowed(start..end, |part| delay.process(part));
                most = most.max(delay.active_grains());
                start = end;
            }
            (signal, most)
        };
        let (reference, most) = render(512);
        assert_eq!(most, GranularDelay::<f64>::MAX_GRAINS);
        for block in [1, 7, 64, 333, 24_000] {
            assert!(render(block).0 == reference, "blocks of {block}");
        }
    }

    #[test]
    fn the_stereo_spread_places_grains_by_equal_power() {
        // A steady level on both sides through an overlap of two with no
        // spray: each grain's two gains have a power of two together, so
        // the power of the two outputs is at most twice that of the two
        // inputs, and the sides differ.
        let mut delay = prepared(plain().with_stereo_spread(1.0).with_position_ms(10.0), 2);
        let mut signal = AudioBuffer::<f64>::new(2, 12_000);
        for channel in signal.iter_channels_mut() {
            channel.fill(0.5);
        }
        run_in_blocks(&mut delay, &mut signal, 512);
        let (left, right) = (signal.channel(0), signal.channel(1));
        assert!(left[11_000] != right[11_000]);
        for n in 6_000..12_000 {
            assert!(left[n] >= 0.0 && right[n] >= 0.0);
            assert!(left[n] * left[n] + right[n] * right[n] <= 2.0 * 0.5 + 1e-9);
        }
    }

    #[test]
    fn feedback_stays_bounded_and_garbage_is_survived() {
        for feedback in [0.95, -0.95] {
            let mut delay = prepared(
                GranularDelay::new()
                    .with_feedback(feedback)
                    .with_density_hz(200.0)
                    .with_grain_ms(10.0)
                    .with_position_ms(5.0)
                    .with_spray_ms(0.0)
                    .with_mix(1.0),
                2,
            );
            let mut signal = noise::<f64>(2, 48_000, 4);
            run_in_blocks(&mut delay, &mut signal, 512);
            // The line holds at most the input and one more from the tanh;
            // the windows, scaled, add up to at most two of that, panned.
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 2.0 * 2.0 * SQRT_2, "feedback {feedback}: {peak}");
        }
        let mut delay = prepared(GranularDelay::new(), 2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            delay.set_position_ms(garbage);
            delay.set_spray_ms(garbage);
            delay.set_grain_ms(garbage);
            delay.set_density_hz(garbage);
            delay.set_pitch_semitones(garbage);
            delay.set_pitch_spray(garbage);
            delay.set_stereo_spread(garbage);
            delay.set_feedback(garbage);
            delay.set_mix(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut delay, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 6.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_starts_the_cloud_again() {
        let build = || prepared(GranularDelay::new().with_pitch_spray(7.0), 2);
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 6_000, 1);
        run_in_blocks(&mut fresh, &mut expected, 512);
        let mut used = build();
        let mut other = noise::<f64>(2, 5_000, 5);
        run_in_blocks(&mut used, &mut other, 512);
        used.reset();
        assert_eq!(used.active_grains(), 0);
        let mut again = noise::<f64>(2, 6_000, 1);
        run_in_blocks(&mut used, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut delay = GranularDelay::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        delay.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        delay.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        delay.process(&mut buffer);
        // Half dry, and the delay line has nothing to give back yet.
        assert_eq!(buffer.channel(0)[0], 0.125);
        assert_eq!(Processor::<f32>::latency_samples(&delay), 0);
        delay.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
