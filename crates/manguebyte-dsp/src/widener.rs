use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::numeric::{glide_to, limit, step_towards};
use crate::{Crossover, CrossoverSlope, ProcessSpec, Processor};

/// Limits of the settings.
const MAX_WIDTH: f64 = 2.0;
const MIN_CUTOFF_HZ: f64 = 20.0;
const MAX_CUTOFF_HZ: f64 = 1_000.0;

/// How long a change of width, or turning bass mono on or off, takes.
const GLIDE_SECONDS: f64 = 0.02;

/// A stereo widener: the difference between left and right made larger or
/// smaller, the sum left alone.
///
/// The first two channels are taken apart into mid and side, `M = (L + R) /
/// 2` and `S = (L - R) / 2`; the side is scaled by the width and they are
/// put back together, `L = M + width * S` and `R = M - width * S`. A width
/// of 0 makes the signal mono, 1 leaves it as it is, and 2 doubles the
/// side.
///
/// - **Mono compatibility.** The mid is never touched, so the mono sum
///   `L + R` comes out exactly as it went in, at any width and with bass
///   mono on or off: widening cannot make anything cancel when the mix is
///   summed to mono.
/// - **Identity.** At a width of 1 with bass mono off, the input passes
///   through untouched, sample for sample.
/// - **Bass mono.** Optionally the side is split by a Linkwitz-Riley
///   crossover of the fourth order ([`Crossover`]), and only the part above
///   the cutoff is kept: below it the signal becomes mono, as cutting a
///   record or a club system wants. An octave below the cutoff the side is
///   24 dB down, at a third of it 38 dB.
///
/// The width and bass mono glide over 20 ms, and a change of cutoff glides
/// as the crossover's does. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, StereoWidener};
///
/// let mut widener = StereoWidener::<f32>::new().with_width(0.0);
/// widener.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 2 });
///
/// // A width of 0 is mono: both sides carry the mid.
/// let mut block = AudioBuffer::<f32>::new(2, 64);
/// block.channel_mut(0).fill(0.8);
/// block.channel_mut(1).fill(0.2);
/// widener.process(&mut block);
/// assert_eq!(block.channel(0), &[0.5; 64]);
/// assert_eq!(block.channel(1), &[0.5; 64]);
/// ```
///
/// # Limits
///
/// With bass mono on, the side above the cutoff comes out through the
/// crossover's all-pass, so its phase turns against the mid near the
/// cutoff, and the output is no longer the input at a width of 1. The
/// width is limited to `0.0..=2.0` and the cutoff to 20 Hz to 1 kHz. A
/// buffer with fewer than two channels is left as it is; channels beyond
/// two are not touched.
#[derive(Debug, Clone)]
pub struct StereoWidener<S: Sample = f32> {
    width: f64,
    bass_mono: bool,
    cutoff_hz: f64,
    width_now: Smoothed<f64>,
    /// How much of the side below the cutoff is taken away, from 0 to 1.
    mono_now: f64,
    fade_step: f64,
    crossover: Crossover<S>,
    /// The side, for the crossover to split.
    side: AudioBuffer<S>,
}

impl<S: Sample> Default for StereoWidener<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> StereoWidener<S> {
    /// Creates a widener at a width of 1.5, with bass mono off and its
    /// cutoff at 120 Hz.
    pub fn new() -> Self {
        Self {
            width: 1.5,
            bass_mono: false,
            cutoff_hz: 120.0,
            width_now: Smoothed::new(1.5),
            mono_now: 0.0,
            fade_step: 1.0,
            crossover: Crossover::two_way(CrossoverSlope::Lr4, 120.0),
            side: AudioBuffer::new(0, 0),
        }
    }

    /// Sets the width: `0.0` is mono, `1.0` leaves the signal as it is,
    /// `2.0` doubles the side. Limited to `0.0..=2.0`.
    pub fn set_width(&mut self, width: f64) {
        self.width = limit(width, 0.0, MAX_WIDTH);
        glide_to(&mut self.width_now, self.width);
    }

    /// Turns bass mono on or off; the change fades over 20 ms.
    pub fn set_bass_mono(&mut self, on: bool) {
        self.bass_mono = on;
    }

    /// Sets the frequency below which bass mono makes the signal mono,
    /// limited to 20 Hz to 1 kHz.
    pub fn set_bass_mono_hz(&mut self, hertz: f64) {
        self.cutoff_hz = limit(hertz, MIN_CUTOFF_HZ, MAX_CUTOFF_HZ);
        self.crossover.set_frequency(0, self.cutoff_hz);
    }

    /// Builder form of [`set_width`](Self::set_width).
    #[must_use]
    pub fn with_width(mut self, width: f64) -> Self {
        self.set_width(width);
        self.width_now.set_immediate(self.width);
        self
    }

    /// Builder form of [`set_bass_mono`](Self::set_bass_mono).
    #[must_use]
    pub fn with_bass_mono(mut self, on: bool) -> Self {
        self.set_bass_mono(on);
        self.mono_now = if on { 1.0 } else { 0.0 };
        self
    }

    /// Builder form of [`set_bass_mono_hz`](Self::set_bass_mono_hz).
    #[must_use]
    pub fn with_bass_mono_hz(mut self, hertz: f64) -> Self {
        self.set_bass_mono_hz(hertz);
        self
    }

    /// The width, as last set.
    pub fn width(&self) -> f64 {
        self.width
    }

    /// Widens a piece no longer than the side buffer.
    fn run(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames();
        let filtering = self.bass_mono || self.mono_now > 0.0;
        if !filtering && self.width == 1.0 && !self.width_now.is_smoothing() {
            // At rest at a width of 1: nothing to do, and nothing done.
            self.width_now.skip(frames);
            return;
        }
        let (left, right) = buffer.channel_pair_mut(0, 1);
        if filtering {
            self.side.set_num_frames(frames);
            for ((side, &l), &r) in self.side.channel_mut(0).iter_mut().zip(&*left).zip(&*right) {
                *side = S::from_f64(0.5 * (l.to_f64() - r.to_f64()));
            }
            self.crossover.split(&self.side);
        }
        let target = if self.bass_mono { 1.0 } else { 0.0 };
        let high = self.crossover.band(1).channel(0);
        for (frame, (l, r)) in left.iter_mut().zip(right.iter_mut()).enumerate() {
            let width = self.width_now.next_value();
            self.mono_now = step_towards(self.mono_now, target, self.fade_step);
            if width == 1.0 && self.mono_now == 0.0 {
                // Nothing to change: the input stays exactly as it is.
                continue;
            }
            let (x, y) = (l.to_f64(), r.to_f64());
            let mid = 0.5 * (x + y);
            let mut side = 0.5 * (x - y);
            if filtering {
                side += (high[frame].to_f64() - side) * self.mono_now;
            }
            *l = S::from_f64(mid + width * side);
            *r = S::from_f64(mid - width * side);
        }
        if filtering && !self.bass_mono && self.mono_now == 0.0 {
            // Faded out: the crossover rests until it is wanted again, and
            // then starts from silence, under the fade in.
            self.crossover.reset();
        }
    }
}

impl<S: Sample> Processor<S> for StereoWidener<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        let glide = GLIDE_SECONDS * spec.sample_rate;
        self.fade_step = if glide >= 1.0 { 1.0 / glide } else { 1.0 };
        self.width_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        let frames = spec.max_block_size.max(1);
        let mono = ProcessSpec {
            num_channels: 1,
            max_block_size: frames,
            ..*spec
        };
        self.crossover.prepare(&mono);
        self.side = AudioBuffer::new(1, frames);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let piece = self.side.capacity();
        if buffer.num_channels() < 2 || self.side.num_channels() == 0 {
            // Nothing to widen, but time still passes.
            for _ in 0..buffer.num_frames() {
                self.width_now.next_value();
            }
            return;
        }
        let frames = buffer.num_frames();
        let mut start = 0;
        while start < frames {
            let end = (start + piece).min(frames);
            buffer.windowed(start..end, |part| self.run(part));
            start = end;
        }
    }

    fn reset(&mut self) {
        self.crossover.reset();
        self.width_now.set_immediate(self.width);
        self.mono_now = if self.bass_mono { 1.0 } else { 0.0 };
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, largest_jump, noise, run_in_blocks};

    const RATE: f64 = 48_000.0;

    fn prepared(widener: StereoWidener<f64>) -> StereoWidener<f64> {
        let mut widener = widener;
        widener.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: 2,
        });
        widener
    }

    #[test]
    fn a_width_of_one_null_tests_against_the_input() {
        let input = noise::<f64>(2, 5_000, 1);
        let mut widener = prepared(StereoWidener::new().with_width(1.0));
        let mut output = input.clone();
        run_in_blocks(&mut widener, &mut output, 512);
        assert!(output == input);

        // Also after a glide back to 1.
        let mut widener = prepared(StereoWidener::new().with_width(2.0));
        let mut first = noise::<f64>(2, 2_000, 2);
        run_in_blocks(&mut widener, &mut first, 512);
        widener.set_width(1.0);
        let mut glide = noise::<f64>(2, 2_000, 3);
        run_in_blocks(&mut widener, &mut glide, 512);
        let mut output = input.clone();
        run_in_blocks(&mut widener, &mut output, 333);
        assert!(output == input);
    }

    #[test]
    fn the_side_is_scaled_by_the_width_and_the_mono_sum_never_changes() {
        let input = noise::<f64>(2, 5_000, 4);
        for width in [0.0, 0.3, 1.0, 1.5, 2.0] {
            for bass_mono in [false, true] {
                let mut widener = prepared(
                    StereoWidener::new()
                        .with_width(width)
                        .with_bass_mono(bass_mono)
                        .with_bass_mono_hz(300.0),
                );
                let mut output = input.clone();
                run_in_blocks(&mut widener, &mut output, 512);
                for n in 0..5_000 {
                    let (l, r) = (input.channel(0)[n], input.channel(1)[n]);
                    let (x, y) = (output.channel(0)[n], output.channel(1)[n]);
                    assert!(
                        (x + y - (l + r)).abs() < 1e-14,
                        "width {width}, bass mono {bass_mono}: the mono sum moved"
                    );
                    if !bass_mono {
                        assert!((x - y - width * (l - r)).abs() < 1e-14, "width {width}");
                    }
                }
            }
        }
    }

    /// The level of the side of the last half of `output`, against `level`.
    fn side_db(output: &AudioBuffer<f64>, level: f64) -> f64 {
        let half = output.num_frames() / 2;
        let peak = (half..output.num_frames())
            .map(|n| (0.5 * (output.channel(0)[n] - output.channel(1)[n])).abs())
            .fold(0.0, f64::max);
        db(peak / level)
    }

    #[test]
    fn bass_mono_takes_the_side_away_below_the_cutoff_only() {
        // A tone in the side alone: left and right upside down of each
        // other. The crossover is at 120 Hz; LR4's high band is
        // ω⁴ / (1 + ω⁴) at ω = f / fc.
        for (frequency, width) in [(40.0, 1.0), (60.0, 2.0), (120.0, 1.0), (960.0, 1.5)] {
            let mut widener = prepared(
                StereoWidener::new()
                    .with_width(width)
                    .with_bass_mono(true)
                    .with_bass_mono_hz(120.0),
            );
            let mut signal = AudioBuffer::<f64>::new(2, 96_000);
            for n in 0..96_000 {
                let value = 0.5 * (TAU * frequency * n as f64 / RATE).sin();
                signal.channel_mut(0)[n] = value;
                signal.channel_mut(1)[n] = -value;
            }
            run_in_blocks(&mut widener, &mut signal, 512);
            let w4 = (frequency / 120.0f64).powi(4);
            let expected = db(width * w4 / (1.0 + w4));
            let measured = side_db(&signal, 0.5);
            assert!(
                (measured - expected).abs() < 0.05,
                "{frequency} Hz: {measured} dB against {expected} dB"
            );
        }
        // The documented landmarks.
        let high = |ratio: f64| db(ratio.powi(4) / (1.0 + ratio.powi(4)));
        assert!((high(0.5) + 24.6).abs() < 0.1);
        assert!((high(1.0 / 3.0) + 38.2).abs() < 0.1);
    }

    #[test]
    fn output_does_not_depend_on_block_size_and_changes_do_not_click() {
        let render = |block: usize| {
            let mut widener = prepared(StereoWidener::new());
            let mut signal = AudioBuffer::<f64>::new(2, 12_000);
            for n in 0..12_000 {
                let t = n as f64 / RATE;
                signal.channel_mut(0)[n] = 0.5 * (TAU * 220.0 * t).sin();
                signal.channel_mut(1)[n] = 0.5 * (TAU * 330.0 * t).sin();
            }
            let mut start = 0;
            type Change = fn(&mut StereoWidener<f64>);
            let changes: [(usize, Change); 5] = [
                (2_000, |w| w.set_bass_mono(true)),
                (4_500, |w| w.set_width(0.0)),
                (7_000, |w| w.set_bass_mono_hz(500.0)),
                (9_000, |w| {
                    w.set_bass_mono(false);
                    w.set_width(1.0);
                }),
                (12_000, |_| {}),
            ];
            for (end, change) in changes {
                signal.windowed(start..end, |part| run_in_blocks(&mut widener, part, block));
                change(&mut widener);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 12_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
        // The two tones move by at most 0.023 per sample each; widened by
        // 1.5, a side can move by up to 1.25 times that.
        for channel in reference.iter_channels() {
            let jump = largest_jump(channel);
            assert!(jump < 0.04, "a jump of {jump}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut widener = prepared(StereoWidener::new().with_bass_mono(true));
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            widener.set_width(garbage);
            widener.set_bass_mono_hz(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut widener, &mut signal, 512);
            // Mid plus twice the side, and the crossover's overshoot.
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 3.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_clears_the_crossover() {
        let build = || prepared(StereoWidener::new().with_bass_mono(true));
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut fresh, &mut expected, 512);
        let mut used = build();
        let mut other = noise::<f64>(2, 2_000, 5);
        run_in_blocks(&mut used, &mut other, 512);
        used.reset();
        let mut again = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut used, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_mono_and_unprepared_use_work() {
        let mut widener = StereoWidener::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        // Not prepared: left as it is.
        widener.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        assert_eq!(buffer.channel(1), &[0.0; 64]);
        widener.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 16,
            num_channels: 2,
        });
        widener.process(&mut buffer);
        // Mid 0.125, side 0.125 widened to 0.1875.
        assert_eq!(buffer.channel(0), &[0.3125; 64]);
        assert_eq!(buffer.channel(1), &[-0.0625; 64]);
        let mut mono = AudioBuffer::<f32>::new(1, 64);
        mono.channel_mut(0).fill(0.5);
        widener.process(&mut mono);
        assert_eq!(mono.channel(0), &[0.5; 64]);
        assert_eq!(widener.width(), 1.5);
    }
}
