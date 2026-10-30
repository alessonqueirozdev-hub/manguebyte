use core::f64::consts::TAU;
use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::numeric::{glide_to, limit};
use crate::{ProcessContext, ProcessSpec, Processor};

/// The highest carrier frequency, as a fraction of the sample rate.
const MAX_FREQUENCY_RATIO: f64 = 0.5;

/// How long a change of frequency, mix or stereo phase takes.
const GLIDE_SECONDS: f64 = 0.02;

/// What a [`RingModulator`] multiplies its input by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum RingCarrier {
    /// Its own cosine oscillator.
    #[default]
    Oscillator,
    /// The [`sidechain`](ProcessContext::sidechain) of the context given to
    /// [`process_with`](Processor::process_with): channel `c` of the input
    /// by channel `c` of the sidechain, or by its last channel if it has
    /// fewer. Without a sidechain, or through
    /// [`process`](Processor::process), the oscillator is used instead.
    Sidechain,
}

/// A ring modulator: the input multiplied by a carrier.
///
/// Multiplying two sines gives their sum and their difference and neither
/// of the two: `cos(a) cos(b) = (cos(a + b) + cos(a - b)) / 2`. A tone at
/// `f` against a carrier at `c` comes out at `f + c` and `|f - c|`, each at
/// half its level, which turns harmonic sounds inharmonic: bells, robots,
/// the classic metallic voice.
///
/// - The carrier is a cosine of a frequency in hertz, or a second signal
///   from the sidechain ([`RingCarrier`]).
/// - Channel `c` reads the oscillator `c * stereo_phase` cycles ahead of
///   the first, so a stereo signal can be modulated out of step on its two
///   sides.
/// - The output is `dry * (1 - mix) + input * carrier * mix`.
///
/// The frequency glides over 20 ms in a straight line, keeping the phase,
/// and so do the mix and the stereo phase. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, RingModulator};
///
/// let mut ring = RingModulator::<f32>::new().with_frequency_hz(0.0);
/// ring.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
///
/// // A carrier at 0 Hz is a cosine stopped at its top: the input passes.
/// let mut block = AudioBuffer::<f32>::new(1, 64);
/// block.channel_mut(0).fill(0.5);
/// ring.process(&mut block);
/// assert_eq!(block.channel(0), &[0.5; 64]);
/// ```
///
/// # Limits
///
/// The frequency is limited to between 0 and half the sample rate. The
/// oscillator is a pure cosine, so it makes no partials of its own; a
/// carrier from the sidechain is used as it is, and partials of the
/// product above half the sample rate fold back.
#[derive(Debug, Clone)]
pub struct RingModulator<S: Sample = f32> {
    frequency_hz: f64,
    carrier: RingCarrier,
    mix: f64,
    stereo_phase: f64,
    sample_rate: f64,
    /// Position in the oscillator's cycle, from 0 up to but not including 1.
    phase: f64,
    frequency: Smoothed<f64>,
    mix_now: Smoothed<f64>,
    phase_now: Smoothed<f64>,
    channels: usize,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for RingModulator<S> {
    fn default() -> Self {
        Self::new()
    }
}

/// The oscillator and the gliding values, moved on sample by sample.
#[derive(Debug, Clone, Copy)]
struct Motion {
    phase: f64,
    frequency: Smoothed<f64>,
    mix: Smoothed<f64>,
    stereo_phase: Smoothed<f64>,
}

impl Motion {
    /// The values for one sample: the oscillator's phase, the mix and the
    /// stereo phase. Moves on by one sample.
    #[inline]
    fn step(&mut self, sample_rate: f64) -> (f64, f64, f64) {
        let values = (
            self.phase,
            self.mix.next_value(),
            self.stereo_phase.next_value(),
        );
        self.phase += self.frequency.next_value() / sample_rate;
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        values
    }
}

impl<S: Sample> RingModulator<S> {
    /// Creates a ring modulator with a 440 Hz carrier, both sides in step
    /// and fully wet.
    pub fn new() -> Self {
        Self {
            frequency_hz: 440.0,
            carrier: RingCarrier::Oscillator,
            mix: 1.0,
            stereo_phase: 0.0,
            sample_rate: 44_100.0,
            phase: 0.0,
            frequency: Smoothed::new(440.0),
            mix_now: Smoothed::new(1.0),
            phase_now: Smoothed::new(0.0),
            channels: 0,
            marker: PhantomData,
        }
    }

    /// Sets the oscillator's frequency in hertz, limited to between 0 and
    /// half the sample rate.
    pub fn set_frequency_hz(&mut self, hertz: f64) {
        self.frequency_hz = limit(hertz, 0.0, MAX_FREQUENCY_RATIO * self.sample_rate);
        glide_to(&mut self.frequency, self.frequency_hz);
    }

    /// Chooses the carrier.
    pub fn set_carrier(&mut self, carrier: RingCarrier) {
        self.carrier = carrier;
    }

    /// Sets the balance between the untouched signal (`0.0`) and the
    /// product (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.mix_now, self.mix);
    }

    /// Sets how far ahead in the oscillator's cycle each channel is of the
    /// one before, from `0.0` to `1.0` of a cycle.
    pub fn set_stereo_phase(&mut self, cycles: f64) {
        self.stereo_phase = limit(cycles, 0.0, 1.0);
        glide_to(&mut self.phase_now, self.stereo_phase);
    }

    /// Builder form of [`set_frequency_hz`](Self::set_frequency_hz).
    #[must_use]
    pub fn with_frequency_hz(mut self, hertz: f64) -> Self {
        self.set_frequency_hz(hertz);
        self
    }

    /// Builder form of [`set_carrier`](Self::set_carrier).
    #[must_use]
    pub fn with_carrier(mut self, carrier: RingCarrier) -> Self {
        self.set_carrier(carrier);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    #[must_use]
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// Builder form of [`set_stereo_phase`](Self::set_stereo_phase).
    #[must_use]
    pub fn with_stereo_phase(mut self, cycles: f64) -> Self {
        self.set_stereo_phase(cycles);
        self
    }

    /// The carrier, as last chosen.
    pub fn carrier(&self) -> RingCarrier {
        self.carrier
    }

    fn motion(&self) -> Motion {
        Motion {
            phase: self.phase,
            frequency: self.frequency,
            mix: self.mix_now,
            stereo_phase: self.phase_now,
        }
    }

    fn keep(&mut self, motion: Motion) {
        self.phase = motion.phase;
        self.frequency = motion.frequency;
        self.mix_now = motion.mix;
        self.phase_now = motion.stereo_phase;
    }

    fn run(&mut self, buffer: &mut AudioBuffer<S>, context: Option<&ProcessContext<'_, S>>) {
        let sidechain = context.and_then(|context| {
            context
                .sidechain
                .filter(|sidechain| sidechain.num_channels() > 0)
                .map(|sidechain| (sidechain, context.offset))
        });
        let rate = self.sample_rate;
        let start = self.motion();
        let mut after = start;
        let channels = buffer.num_channels().min(self.channels);
        for channel in 0..channels {
            let mut motion = start;
            let place = channel as f64;
            let key = sidechain.map(|(sidechain, offset)| {
                let index = channel.min(sidechain.num_channels() - 1);
                (sidechain.channel(index), offset)
            });
            for (frame, sample) in buffer.channel_mut(channel).iter_mut().enumerate() {
                let (phase, mix, stereo) = motion.step(rate);
                let carrier = match key {
                    Some((key, offset)) => key
                        .get(offset.saturating_add(frame))
                        .map_or(0.0, |value| value.to_f64()),
                    None => (TAU * (phase + place * stereo)).cos(),
                };
                let input = sample.to_f64();
                *sample = S::from_f64(input * (1.0 - mix) + input * carrier * mix);
            }
            after = motion;
        }
        if channels == 0 {
            // No channel to process, but time still passes.
            for _ in 0..buffer.num_frames() {
                after.step(rate);
            }
        }
        self.keep(after);
    }
}

impl<S: Sample> Processor<S> for RingModulator<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.channels = spec.num_channels;
        self.frequency_hz = limit(
            self.frequency_hz,
            0.0,
            MAX_FREQUENCY_RATIO * spec.sample_rate,
        );
        self.frequency.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.mix_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.phase_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.run(buffer, None);
    }

    /// Like [`process`](Processor::process). With the carrier set to
    /// [`RingCarrier::Sidechain`] and a sidechain in the context, the input
    /// is multiplied by the sidechain, from its frame
    /// [`offset`](ProcessContext::offset) on; frames the sidechain lacks
    /// count as silence.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        let key = (self.carrier == RingCarrier::Sidechain).then_some(context);
        self.run(buffer, key);
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.frequency.set_immediate(self.frequency_hz);
        self.mix_now.set_immediate(self.mix);
        self.phase_now.set_immediate(self.stereo_phase);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventBuffer;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{largest_jump, noise, run_in_blocks, sine};
    use crate::{Transport, split_at_events_with};

    const RATE: f64 = 48_000.0;

    fn prepared(ring: RingModulator<f64>, channels: usize) -> RingModulator<f64> {
        let mut ring = ring;
        ring.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        ring
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
    fn a_tone_comes_out_at_exactly_the_sum_and_the_difference() {
        // 24 000 frames: bin `k` is `2k` hertz, and every frequency here
        // falls on a bin.
        const LEN: usize = 24_000;
        for (tone, carrier) in [(1_000.0, 300.0), (440.0, 1_234.0), (3_000.0, 7_000.0)] {
            let mut ring = prepared(RingModulator::new().with_frequency_hz(carrier), 1);
            let mut signal = sine::<f64>(1, LEN, tone, RATE, 0.5);
            run_in_blocks(&mut ring, &mut signal, 512);
            let bins = spectrum(signal.channel(0), LEN);
            let sum = ((tone + carrier) / 2.0) as usize;
            let difference = ((tone - carrier) / 2.0f64).abs() as usize;
            for (bin, &level) in bins.iter().enumerate() {
                // Half the level each, and nothing anywhere else: not the
                // tone, not the carrier.
                let expected = if bin == sum || bin == difference {
                    0.25
                } else {
                    0.0
                };
                assert!(
                    (level - expected).abs() < 1e-9,
                    "{tone} Hz by {carrier} Hz, bin {bin} ({} Hz): {level}",
                    2 * bin
                );
            }
        }
    }

    #[test]
    fn a_sidechain_carrier_multiplies_sample_by_sample() {
        let input = noise::<f64>(2, 1_000, 1);
        let mut key = noise::<f64>(1, 1_000, 2);
        key.set_num_frames(900);
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
        let check = |output: &AudioBuffer<f64>| {
            for channel in 0..2 {
                for n in 0..1_000 {
                    let carrier = if n < 900 { key.channel(0)[n] } else { 0.0 };
                    let expected = input.channel(channel)[n] * carrier;
                    assert_eq!(output.channel(channel)[n], expected, "frame {n}");
                }
            }
        };
        let mut ring = prepared(RingModulator::new().with_carrier(RingCarrier::Sidechain), 2);
        let mut output = input.clone();
        ring.process_with(&mut output, &context);
        check(&output);

        // In pieces, each reads the sidechain from its own offset.
        let mut ring = prepared(RingModulator::new().with_carrier(RingCarrier::Sidechain), 2);
        let mut output = input.clone();
        split_at_events_with(
            &mut ring,
            &mut output,
            &context,
            |_, _| {},
            |ring, piece, piece_context| {
                let mut start = 0;
                while start < piece.num_frames() {
                    let end = (start + 77).min(piece.num_frames());
                    let inner = piece_context.for_piece(start);
                    piece.windowed(start..end, |part| ring.process_with(part, &inner));
                    start = end;
                }
            },
        );
        check(&output);

        // Without a sidechain, the oscillator.
        let mut ring = prepared(
            RingModulator::new()
                .with_carrier(RingCarrier::Sidechain)
                .with_frequency_hz(0.0),
            2,
        );
        let mut output = input.clone();
        ring.process_with(&mut output, &ProcessContext::new(&events, &transport));
        assert!(output == input);
    }

    #[test]
    fn the_stereo_phase_and_the_mix_do_what_they_say() {
        // A steady level shows the carrier itself.
        let mut ring = prepared(
            RingModulator::new()
                .with_frequency_hz(100.0)
                .with_stereo_phase(0.25)
                .with_mix(0.75),
            2,
        );
        let mut signal = AudioBuffer::<f64>::new(2, 2_000);
        for channel in signal.iter_channels_mut() {
            channel.fill(1.0);
        }
        run_in_blocks(&mut ring, &mut signal, 100);
        for n in 0..2_000 {
            for channel in 0..2 {
                let phase = 100.0 * n as f64 / RATE + 0.25 * channel as f64;
                let expected = 0.25 + 0.75 * (TAU * phase).cos();
                assert!((signal.channel(channel)[n] - expected).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size_and_changes_do_not_click() {
        let render = |block: usize| {
            let mut ring = prepared(RingModulator::new().with_frequency_hz(30.0), 2);
            let mut signal = sine::<f64>(2, 9_000, 220.0, RATE, 0.5);
            let mut start = 0;
            for (end, frequency) in [(3_000, 90.0), (6_000, 10.0), (9_000, 40.0)] {
                signal.windowed(start..end, |part| run_in_blocks(&mut ring, part, block));
                ring.set_frequency_hz(frequency);
                ring.set_mix(frequency / 100.0);
                ring.set_stereo_phase(frequency / 100.0);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 9_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
        // A 220 Hz tone of 0.5 by a slow carrier moves by little more than
        // the tone does, 0.0144 per sample.
        for channel in reference.iter_channels() {
            let jump = largest_jump(channel);
            assert!(jump < 0.016, "a jump of {jump}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut ring = prepared(RingModulator::new(), 2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            ring.set_frequency_hz(garbage);
            ring.set_mix(garbage);
            ring.set_stereo_phase(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut ring, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak <= 1.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_starts_the_carrier_again() {
        let mut ring = prepared(RingModulator::new().with_frequency_hz(123.0), 2);
        let mut expected = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut ring, &mut expected, 512);
        let mut other = noise::<f64>(2, 1_234, 5);
        run_in_blocks(&mut ring, &mut other, 512);
        ring.reset();
        let mut again = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut ring, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut ring = RingModulator::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        ring.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        ring.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        assert_eq!(ring.carrier(), RingCarrier::Oscillator);
        ring.process(&mut buffer);
        assert_eq!(buffer.channel(0)[0], 0.25);
        assert!(buffer.channel(0)[63] < 0.25);
        ring.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
