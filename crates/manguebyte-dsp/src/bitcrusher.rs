use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::iir::{Band, Design};
use crate::numeric::{glide_to, limit, step_towards};
use crate::{IirFilter, ProcessSpec, Processor};

/// Limits of the settings.
const MIN_BITS: f64 = 1.0;
const MAX_BITS: f64 = 32.0;
const MAX_DOWNSAMPLE: f64 = 64.0;

/// How long a change of bit depth, mix or anti-aliasing takes.
const GLIDE_SECONDS: f64 = 0.02;

/// The anti-aliasing filter: an elliptic low-pass of this order, with this
/// much ripple and attenuation, whose pass band ends at this fraction of the
/// reduced rate.
const FILTER_ORDER: usize = 8;
const FILTER_RIPPLE_DB: f64 = 0.1;
const FILTER_ATTENUATION_DB: f64 = 90.0;
const FILTER_EDGE: f64 = 0.4;

/// The size of one step of the quantiser for a bit depth: full scale, from
/// -1 to 1, is cut into `2^bits` steps.
#[inline]
fn step_size(bits: f64) -> f64 {
    (1.0 - bits).exp2()
}

/// Everything that moves while a block is processed, the same on every
/// channel. See the `Motion` of [`Chorus`](crate::Chorus).
#[derive(Debug, Clone, Copy)]
struct Motion {
    bits: Smoothed<f64>,
    mix: Smoothed<f64>,
    /// How much of the signal goes through the anti-aliasing filter, from 0
    /// to 1.
    filtered: f64,
    /// Samples since the held value was last taken.
    since: f64,
}

/// The values of [`Motion`] for one sample.
#[derive(Debug, Clone, Copy)]
struct Step {
    bits: f64,
    mix: f64,
    filtered: f64,
    /// Whether a new value is taken on this sample.
    take: bool,
}

impl Motion {
    #[inline]
    fn step(&mut self, factor: f64, filtering: bool, fade_step: f64) -> Step {
        let target = if filtering { 1.0 } else { 0.0 };
        self.filtered = step_towards(self.filtered, target, fade_step);
        let take = self.since >= factor;
        if take {
            self.since = (self.since - factor).min(factor);
        }
        self.since += 1.0;
        Step {
            bits: self.bits.next_value(),
            mix: self.mix.next_value(),
            filtered: self.filtered,
            take,
        }
    }
}

/// The memory of one channel.
#[derive(Debug, Clone, Copy)]
struct Channel {
    /// The quantised value being held.
    held: f64,
    /// The state of the dither's random numbers (xorshift64).
    random: u64,
}

impl Channel {
    fn new(index: usize) -> Self {
        Self {
            held: 0.0,
            random: 0x2545_F491_4F6C_DD1D ^ (index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15),
        }
    }

    /// A number from 0 up to but not including 1.
    #[inline]
    fn unit(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A bitcrusher and sample-rate reducer: the sound of early samplers and
/// game consoles, made on purpose.
///
/// Two stages, each optional in effect:
///
/// - **Rate reduction.** A new input value is taken once every `factor`
///   samples and held in between (a zero-order hold), as a converter
///   running at `1 / factor` of the sample rate would. The factor need not
///   be a whole number: with 2.5, values are taken two and three samples
///   apart in turn. Holding makes images of the signal around multiples of
///   the reduced rate, and what was above half the reduced rate folds back
///   below it. That folding, the aliasing, is the sound the effect is
///   usually for; with [anti-aliasing](Self::set_anti_alias) on, the input
///   first goes through an eighth-order elliptic low-pass whose pass band
///   ends at 0.4 of the reduced rate, 90 dB down from about half of it, and
///   the folding is gone while the images stay.
/// - **Bit reduction.** The held value is rounded to the nearest of the
///   steps that cut full scale, -1 to 1, into `2^bits`: a step of
///   `2^(1 - bits)`. The bit depth is a continuous value, so it can be
///   swept: 4.5 bits is a step of `2^-3.5`. Values beyond full scale are
///   rounded the same way, not clipped. With [dither](Self::set_dither) on,
///   triangular (TPDF) dither of one step either side is added before
///   rounding, which makes the error noise that does not follow the
///   signal, at the cost of 4.8 dB more of it: the error of plain rounding
///   has a power of `step² / 12`, the dithered one `step² / 4`.
///
/// The output is `dry * (1 - mix) + crushed * mix`. The bit depth, the mix
/// and turning anti-aliasing on or off glide over 20 ms; a change of the
/// factor takes effect at once and the filter glides to its new design
/// over 20 ms. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Bitcrusher, ProcessSpec, Processor};
///
/// let mut crusher = Bitcrusher::<f32>::new().with_bits(4.0).with_downsample(4.0);
/// crusher.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
///
/// let mut block = AudioBuffer::<f32>::new(1, 64);
/// for (n, sample) in block.channel_mut(0).iter_mut().enumerate() {
///     *sample = (n as f32 * 0.05).sin();
/// }
/// crusher.process(&mut block);
/// // Four bits: steps of 1/8. Each value is held for four samples.
/// let out = block.channel(0);
/// assert!(out.iter().all(|s| (s * 8.0).fract() == 0.0));
/// assert!(out.chunks(4).all(|run| run.iter().all(|s| *s == run[0])));
/// ```
///
/// # Limits
///
/// The bit depth is limited to `1.0..=32.0` and the factor to
/// `1.0..=64.0`. The filter makes the effect's only delay, a few samples of
/// group delay in its pass band, not reported as latency. Each channel has
/// its own dither sequence, fixed by its place, so a render is repeatable
/// and [`reset`](Processor::reset) starts the sequences again.
#[derive(Debug, Clone)]
pub struct Bitcrusher<S: Sample = f32> {
    bits: f64,
    downsample: f64,
    dither: bool,
    anti_alias: bool,
    mix: f64,
    sample_rate: f64,
    fade_step: f64,
    motion: Motion,
    channels: Vec<Channel>,
    filter: IirFilter<S>,
    /// The input, kept for the dry signal.
    dry: AudioBuffer<S>,
    /// The input through the anti-aliasing filter.
    smooth: AudioBuffer<S>,
}

impl<S: Sample> Default for Bitcrusher<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Bitcrusher<S> {
    /// Creates a bitcrusher at 8 bits, no rate reduction, no dither, no
    /// anti-aliasing and fully wet.
    pub fn new() -> Self {
        let mut crusher = Self {
            bits: 8.0,
            downsample: 1.0,
            dither: false,
            anti_alias: false,
            mix: 1.0,
            sample_rate: 44_100.0,
            fade_step: 1.0,
            motion: Motion {
                bits: Smoothed::new(8.0),
                mix: Smoothed::new(1.0),
                filtered: 0.0,
                since: 1.0,
            },
            channels: Vec::new(),
            filter: IirFilter::new(Self::design(1.0, 44_100.0)),
            dry: AudioBuffer::new(0, 0),
            smooth: AudioBuffer::new(0, 0),
        };
        crusher.snap();
        crusher
    }

    /// Sets the bit depth, limited to `1.0..=32.0`; it need not be a whole
    /// number.
    pub fn set_bits(&mut self, bits: f64) {
        self.bits = limit(bits, MIN_BITS, MAX_BITS);
        glide_to(&mut self.motion.bits, self.bits);
    }

    /// Sets how many samples each value is held for, limited to
    /// `1.0..=64.0`: the sample rate is divided by it. It need not be a
    /// whole number.
    pub fn set_downsample(&mut self, factor: f64) {
        let factor = limit(factor, 1.0, MAX_DOWNSAMPLE);
        if factor != self.downsample {
            self.downsample = factor;
            self.filter
                .set_design(Self::design(factor, self.sample_rate));
        }
    }

    /// Turns triangular dither of one step either side on or off.
    pub fn set_dither(&mut self, dither: bool) {
        self.dither = dither;
    }

    /// Turns the low-pass before the rate reduction on or off; the change
    /// fades over 20 ms.
    pub fn set_anti_alias(&mut self, anti_alias: bool) {
        self.anti_alias = anti_alias;
    }

    /// Sets the balance between the untouched signal (`0.0`) and the
    /// crushed one (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.motion.mix, self.mix);
    }

    /// Builder form of [`set_bits`](Self::set_bits).
    #[must_use]
    pub fn with_bits(mut self, bits: f64) -> Self {
        self.set_bits(bits);
        self
    }

    /// Builder form of [`set_downsample`](Self::set_downsample).
    #[must_use]
    pub fn with_downsample(mut self, factor: f64) -> Self {
        self.set_downsample(factor);
        self
    }

    /// Builder form of [`set_dither`](Self::set_dither).
    #[must_use]
    pub fn with_dither(mut self, dither: bool) -> Self {
        self.set_dither(dither);
        self
    }

    /// Builder form of [`set_anti_alias`](Self::set_anti_alias).
    #[must_use]
    pub fn with_anti_alias(mut self, anti_alias: bool) -> Self {
        self.set_anti_alias(anti_alias);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    #[must_use]
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// The size of one step of the quantiser at the bit depth as last set.
    pub fn step(&self) -> f64 {
        step_size(self.bits)
    }

    /// The anti-aliasing filter for a factor at a sample rate.
    fn design(factor: f64, sample_rate: f64) -> Design {
        let edge = FILTER_EDGE * sample_rate / factor;
        Design::elliptic(
            Band::Lowpass { cutoff_hz: edge },
            FILTER_ORDER,
            FILTER_RIPPLE_DB,
            FILTER_ATTENUATION_DB,
        )
    }

    /// Jumps every gliding value to where it is heading.
    fn snap(&mut self) {
        self.motion.bits.set_immediate(self.bits);
        self.motion.mix.set_immediate(self.mix);
        self.motion.filtered = if self.anti_alias { 1.0 } else { 0.0 };
        // The first sample takes a value.
        self.motion.since = self.downsample;
    }

    /// Crushes a piece no longer than the scratch buffers.
    fn run(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames();
        let channels = buffer.num_channels().min(self.channels.len());
        let filtering = self.anti_alias || self.motion.filtered > 0.0;
        self.dry.set_num_frames(frames);
        for channel in 0..channels {
            self.dry
                .channel_mut(channel)
                .copy_from_slice(buffer.channel(channel));
        }
        if filtering {
            self.smooth.set_num_frames(frames);
            for channel in 0..channels {
                self.smooth
                    .channel_mut(channel)
                    .copy_from_slice(buffer.channel(channel));
            }
            self.filter.process(&mut self.smooth);
        }

        let (factor, dither, anti_alias, fade_step) = (
            self.downsample,
            self.dither,
            self.anti_alias,
            self.fade_step,
        );
        let mut after = self.motion;
        for channel in 0..channels {
            let mut motion = self.motion;
            let mut state = self.channels[channel];
            let dry = self.dry.channel(channel);
            let smooth = self.smooth.channel(channel);
            for (frame, sample) in buffer.channel_mut(channel).iter_mut().enumerate() {
                let step = motion.step(factor, anti_alias, fade_step);
                let input = dry[frame].to_f64();
                if step.take {
                    let source = if filtering {
                        let filtered = smooth[frame].to_f64();
                        input + (filtered - input) * step.filtered
                    } else {
                        input
                    };
                    let size = step_size(step.bits);
                    let offset = if dither {
                        state.unit() - state.unit()
                    } else {
                        0.0
                    };
                    state.held = size * (source / size + offset + 0.5).floor();
                }
                *sample = S::from_f64(input + (state.held - input) * step.mix);
            }
            self.channels[channel] = state;
            after = motion;
        }
        if channels == 0 {
            // No channel to process, but time still passes.
            for _ in 0..frames {
                after.step(factor, anti_alias, fade_step);
            }
        }
        self.motion = after;
        if !self.anti_alias && self.motion.filtered == 0.0 && filtering {
            // Faded out: the filter rests until it is wanted again, and then
            // starts from silence, under the fade in.
            self.filter.reset();
        }
    }
}

impl<S: Sample> Processor<S> for Bitcrusher<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        let glide = GLIDE_SECONDS * spec.sample_rate;
        self.fade_step = if glide >= 1.0 { 1.0 / glide } else { 1.0 };
        self.motion.bits.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.motion.mix.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.channels = (0..spec.num_channels).map(Channel::new).collect();
        self.filter = IirFilter::new(Self::design(self.downsample, spec.sample_rate));
        self.filter.prepare(spec);
        let frames = spec.max_block_size.max(1);
        self.dry = AudioBuffer::new(spec.num_channels, frames);
        self.smooth = AudioBuffer::new(spec.num_channels, frames);
        self.snap();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let piece = self.dry.capacity();
        if piece == 0 {
            // Not prepared: nothing to keep the dry signal in.
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
        for (index, channel) in self.channels.iter_mut().enumerate() {
            *channel = Channel::new(index);
        }
        self.filter.reset();
        self.snap();
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn prepared(crusher: Bitcrusher<f64>, channels: usize) -> Bitcrusher<f64> {
        let mut crusher = crusher;
        crusher.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        crusher
    }

    /// The power of a signal, per sample.
    fn power(signal: &[f64]) -> f64 {
        signal.iter().map(|s| s * s).sum::<f64>() / signal.len() as f64
    }

    #[test]
    fn every_output_is_a_whole_number_of_steps_within_half_a_step() {
        for bits in [1.0, 3.0, 4.5, 8.0, 13.25, 24.0] {
            let mut crusher = prepared(Bitcrusher::new().with_bits(bits), 2);
            assert_eq!(crusher.step(), (1.0 - bits).exp2());
            let input = noise::<f64>(2, 4_000, 5);
            let mut output = input.clone();
            run_in_blocks(&mut crusher, &mut output, 512);
            let size = (1.0 - bits).exp2();
            for (out, inp) in output.iter_channels().zip(input.iter_channels()) {
                for (&y, &x) in out.iter().zip(inp) {
                    let steps = y / size;
                    assert!((steps - steps.round()).abs() < 1e-9, "{bits} bits: {y}");
                    assert!(
                        (y - x).abs() <= size / 2.0 + 1e-12,
                        "{bits} bits: {x} to {y}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_noise_of_rounding_has_the_power_theory_gives() {
        // A sine of amplitude 0.5 has a power of 0.125. Rounding to steps of
        // `d` adds noise of power d² / 12; triangular dither of one step
        // either side adds d² / 6 more.
        let tone = sine::<f64>(1, 96_000, 997.0, RATE, 0.5);
        for bits in [6.0, 8.0, 10.5, 12.0] {
            for dither in [false, true] {
                let mut crusher =
                    prepared(Bitcrusher::new().with_bits(bits).with_dither(dither), 1);
                let mut output = tone.clone();
                run_in_blocks(&mut crusher, &mut output, 512);
                let error: Vec<f64> = output
                    .channel(0)
                    .iter()
                    .zip(tone.channel(0))
                    .map(|(y, x)| y - x)
                    .collect();
                let size = (1.0 - bits).exp2();
                let noise = if dither {
                    size * size / 4.0
                } else {
                    size * size / 12.0
                };
                let expected = 10.0 * (0.125 / noise).log10();
                let measured = 10.0 * (power(tone.channel(0)) / power(&error)).log10();
                // Plain rounding of a tone only 16 steps high makes an error
                // that follows the tone, and the model of noise spread
                // evenly over a step is then only roughly right: 0.27 dB
                // out at 6 bits. Dither makes it right at any depth.
                let tolerance = if !dither && bits < 8.0 { 0.5 } else { 0.25 };
                assert!(
                    (measured - expected).abs() < tolerance,
                    "{bits} bits, dither {dither}: SNR {measured} dB against {expected}"
                );
            }
        }
        // The rule of thumb: about 6.02 dB per bit, plus 1.76 dB for a sine
        // at full scale (here 6 dB lower).
        let expected_8 = 6.02 * 8.0 + 1.76 - 6.02;
        let size: f64 = 2f64.powi(-7);
        assert!((10.0 * (0.125 / (size * size / 12.0)).log10() - expected_8).abs() < 0.05);
    }

    #[test]
    fn dither_keeps_a_tone_quieter_than_one_step() {
        // A tone of 0.3 steps rounds to silence; with dither its average
        // comes through at its own level.
        let bits: f64 = 8.0;
        let size = (1.0 - bits).exp2();
        let amplitude = 0.3 * size;
        let frames = 480_000;
        let tone = sine::<f64>(1, frames, 1_000.0, RATE, amplitude);

        let mut plain = prepared(Bitcrusher::new().with_bits(bits), 1);
        let mut output = tone.clone();
        run_in_blocks(&mut plain, &mut output, 512);
        assert_eq!(strict_buffer_peak(&output), 0.0);

        let mut dithered = prepared(Bitcrusher::new().with_bits(bits).with_dither(true), 1);
        let mut output = tone.clone();
        run_in_blocks(&mut dithered, &mut output, 512);
        // The part of the output in step with the tone.
        let found = output
            .channel(0)
            .iter()
            .enumerate()
            .map(|(n, y)| y * (TAU * 1_000.0 * n as f64 / RATE).sin())
            .sum::<f64>()
            * 2.0
            / frames as f64;
        assert!(
            (found / amplitude - 1.0).abs() < 0.05,
            "found {found} of {amplitude}"
        );
    }

    #[test]
    fn rate_reduction_holds_each_value_for_the_factor() {
        let input = noise::<f64>(1, 1_000, 9);
        // A whole factor: runs of four, each the input at its start.
        let mut crusher = prepared(Bitcrusher::new().with_bits(32.0).with_downsample(4.0), 1);
        let mut output = input.clone();
        run_in_blocks(&mut crusher, &mut output, 7);
        let size = 2f64.powi(-31);
        for (n, &y) in output.channel(0).iter().enumerate() {
            let x = input.channel(0)[n - n % 4];
            assert!((y - x).abs() <= size / 2.0, "frame {n}");
        }
        // A factor of 2.5: values two and three samples apart in turn, 400
        // of them in 1 000 samples.
        let mut crusher = prepared(Bitcrusher::new().with_bits(32.0).with_downsample(2.5), 1);
        let mut output = input.clone();
        run_in_blocks(&mut crusher, &mut output, 64);
        let out = output.channel(0);
        let changes: Vec<usize> = (1..out.len()).filter(|&n| out[n] != out[n - 1]).collect();
        assert_eq!(changes.len() + 1, 400);
        for pair in changes.windows(2) {
            let gap = pair[1] - pair[0];
            assert!(gap == 2 || gap == 3, "a gap of {gap}");
        }
    }

    /// The amplitude at each bin of the last `len` frames of `signal`, made
    /// to land on whole bins.
    fn spectrum(signal: &[f64], len: usize) -> Vec<f64> {
        let mut fft = RealFft::<f64>::new(len);
        let mut bins = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(&signal[signal.len() - len..], &mut bins);
        bins.iter()
            .map(|bin| 2.0 * bin.norm() / len as f64)
            .collect()
    }

    #[test]
    fn anti_aliasing_removes_what_folds_back_and_keeps_the_band() {
        // A 10 kHz tone held at a quarter of 48 kHz folds back to 2 kHz.
        // The last 24 000 frames are a whole number of periods of both, so
        // bin `k` is `2k` hertz.
        const LEN: usize = 24_000;
        let tone = sine::<f64>(1, 48_000, 10_000.0, RATE, 0.5);
        let level = |anti_alias: bool, signal: &AudioBuffer<f64>, hz: usize| {
            let mut crusher = prepared(
                Bitcrusher::new()
                    .with_bits(32.0)
                    .with_downsample(4.0)
                    .with_anti_alias(anti_alias),
                1,
            );
            let mut output = signal.clone();
            run_in_blocks(&mut crusher, &mut output, 512);
            spectrum(output.channel(0), LEN)[hz / 2]
        };
        let folded = level(false, &tone, 2_000);
        let filtered = level(true, &tone, 2_000);
        // Without the filter the fold is as loud as a held tone at 2 kHz:
        // a value taken every fourth sample, at a quarter of the level, then
        // held, which is a gain of sin(4ω/2) / sin(ω/2).
        let omega = TAU * 2_000.0 / RATE;
        let held = 0.5 * (2.0 * omega).sin() / (4.0 * (omega / 2.0).sin());
        assert!((folded - held).abs() < 1e-6, "{folded} against {held}");
        let suppression = db(folded / filtered);
        assert!(suppression > 85.0, "the fold is only {suppression} dB down");

        // A 1 kHz tone, well inside the reduced band, keeps its level to
        // within the filter's ripple.
        let low = sine::<f64>(1, 48_000, 1_000.0, RATE, 0.5);
        let plain = level(false, &low, 1_000);
        let kept = level(true, &low, 1_000);
        assert!(
            db(kept / plain).abs() < FILTER_RIPPLE_DB + 1e-3,
            "{plain} {kept}"
        );
    }

    #[test]
    fn the_mix_blends_the_dry_and_the_crushed_signal() {
        let input = noise::<f64>(1, 2_000, 3);
        let render = |mix: f64| {
            let mut crusher = prepared(Bitcrusher::new().with_bits(3.0).with_mix(mix), 1);
            let mut output = input.clone();
            run_in_blocks(&mut crusher, &mut output, 512);
            output
        };
        let wet = render(1.0);
        assert!(render(0.0) == input);
        let half = render(0.25);
        for n in 0..2_000 {
            let expected = 0.75 * input.channel(0)[n] + 0.25 * wet.channel(0)[n];
            assert!((half.channel(0)[n] - expected).abs() < 1e-12);
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut crusher = prepared(
                Bitcrusher::new()
                    .with_bits(5.5)
                    .with_downsample(3.3)
                    .with_dither(true)
                    .with_mix(0.8),
                2,
            );
            let mut signal = noise::<f64>(2, 6_000, 1);
            type Change = fn(&mut Bitcrusher<f64>);
            let changes: [(usize, Change); 4] = [
                (1_000, |c| c.set_anti_alias(true)),
                (2_300, |c| {
                    c.set_bits(12.0);
                    c.set_downsample(7.0);
                }),
                (4_100, |c| {
                    c.set_anti_alias(false);
                    c.set_mix(0.3);
                    c.set_downsample(1.0);
                }),
                (6_000, |_| {}),
            ];
            let mut start = 0;
            for (end, change) in changes {
                signal.windowed(start..end, |part| run_in_blocks(&mut crusher, part, block));
                change(&mut crusher);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 6_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
    }

    #[test]
    fn no_change_of_setting_clicks_more_than_the_crushing_itself() {
        // A 220 Hz sine at 12 bits: the step is 0.0005, far below the size
        // of a click. Changes of mix and of anti-aliasing glide.
        let mut crusher = prepared(Bitcrusher::new().with_bits(12.0), 1);
        let mut signal = sine::<f64>(1, 48_000, 220.0, RATE, 0.5);
        type Change = fn(&mut Bitcrusher<f64>);
        let changes: [Change; 4] = [
            |c| c.set_mix(0.0),
            |c| c.set_anti_alias(true),
            |c| c.set_mix(1.0),
            |c| c.set_anti_alias(false),
        ];
        for (index, change) in changes.iter().enumerate() {
            signal.windowed(index * 9_600..(index + 1) * 9_600, |part| {
                run_in_blocks(&mut crusher, part, 512);
            });
            change(&mut crusher);
        }
        signal.windowed(38_400..48_000, |part| {
            run_in_blocks(&mut crusher, part, 512)
        });
        // The sine moves by at most 0.0144 per sample.
        let jump = crate::testing::largest_jump(signal.channel(0));
        assert!(jump < 0.0144 + 2e-3, "a jump of {jump}");
    }

    #[test]
    fn it_stays_bounded_at_the_extremes_and_survives_garbage() {
        let mut crusher = prepared(
            Bitcrusher::new()
                .with_bits(1.0)
                .with_downsample(64.0)
                .with_anti_alias(true)
                .with_dither(true),
            2,
        );
        let mut signal = noise::<f64>(2, 24_000, 4);
        run_in_blocks(&mut crusher, &mut signal, 512);
        // One bit, dithered: steps of 1, and the input and the dither are
        // each within a step of zero.
        let peak = strict_buffer_peak(&signal);
        assert!(peak <= 2.0 && peak > 0.0, "{peak}");

        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            crusher.set_bits(garbage);
            crusher.set_downsample(garbage);
            crusher.set_mix(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut crusher, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak.is_finite() && peak <= 2.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_starts_again_from_silence_and_the_same_dither() {
        let build = || prepared(Bitcrusher::new().with_dither(true).with_downsample(3.0), 2);
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 3_000, 8);
        run_in_blocks(&mut fresh, &mut expected, 512);

        let mut used = build();
        let mut first = noise::<f64>(2, 3_000, 7);
        run_in_blocks(&mut used, &mut first, 512);
        used.reset();
        let mut second = noise::<f64>(2, 3_000, 8);
        run_in_blocks(&mut used, &mut second, 512);
        assert!(second == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut crusher = Bitcrusher::<f32>::new().with_bits(2.0);
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.3);
        crusher.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.3; 64]);

        crusher.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 16,
            num_channels: 1,
        });
        assert_eq!(Processor::<f32>::latency_samples(&crusher), 0);
        let mut buffer = AudioBuffer::<f32>::new(2, 100);
        buffer.channel_mut(0).fill(0.3);
        buffer.channel_mut(1).fill(0.3);
        crusher.process(&mut buffer);
        // Steps of a half: 0.3 rounds to 0.5. The extra channel is left
        // alone.
        assert_eq!(buffer.channel(0), &[0.5; 100]);
        assert_eq!(buffer.channel(1), &[0.3; 100]);
        crusher.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
