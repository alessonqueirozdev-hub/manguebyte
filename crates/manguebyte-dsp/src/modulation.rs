use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample};

use crate::curve::LfoCycle;
use crate::{ProcessSpec, Processor};

/// The shape of an [`Lfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LfoShape {
    /// A smooth sine.
    Sine,
    /// Straight lines up and down.
    Triangle,
    /// A ramp up, then a drop.
    SawUp,
    /// A ramp down, then a jump.
    SawDown,
    /// High for half the cycle, low for the other half.
    Square,
    /// A new random level each cycle, held until the next.
    SampleAndHold,
}

impl LfoShape {
    /// The value of the shape at `phase`, from 0 at the start of a cycle up
    /// to but not including 1, from -1 to 1.
    ///
    /// A [`SampleAndHold`](Self::SampleAndHold) holds a random level drawn
    /// at the start of each cycle, which only an [`Lfo`] knows; for it this
    /// gives 0, the middle of the range the levels are drawn from.
    #[inline]
    pub fn value_at(self, phase: f64) -> f64 {
        match self {
            Self::Sine => (TAU * phase).sin(),
            Self::Triangle => 4.0 * (phase - 0.5).abs() - 1.0,
            Self::SawUp => 2.0 * phase - 1.0,
            Self::SawDown => 1.0 - 2.0 * phase,
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::SampleAndHold => 0.0,
        }
    }
}

/// A low-frequency oscillator: a slow, repeating control signal for vibrato,
/// tremolo, filter sweeps and anything else that should move by itself.
///
/// Unlike [`Oscillator`](crate::Oscillator) it is not band-limited, because
/// its output is not listened to directly, and it can be locked to the beat.
///
/// ```
/// use manguebyte_dsp::{Lfo, LfoShape};
///
/// let mut lfo = Lfo::new(LfoShape::Triangle, 2.0);
/// lfo.set_sample_rate(8.0); // four samples per cycle, for the example
/// let cycle: Vec<f64> = (0..4).map(|_| lfo.next_value()).collect();
/// assert_eq!(cycle, [1.0, 0.0, -1.0, 0.0]);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Lfo {
    shape: LfoShape,
    frequency: f64,
    sample_rate: f64,
    /// Position in the cycle, from 0 up to but not including 1.
    phase: f64,
    held: f64,
    random: u64,
}

impl Lfo {
    /// Creates an oscillator at `frequency` hertz.
    pub fn new(shape: LfoShape, frequency: f64) -> Self {
        let mut lfo = Self {
            shape,
            frequency,
            sample_rate: 44_100.0,
            phase: 0.0,
            held: 0.0,
            random: 0x9E37_79B9_7F4A_7C15,
        };
        lfo.held = lfo.next_random();
        lfo
    }

    /// Sets the sample rate in hertz.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
    }

    /// Sets the rate in hertz. Real-time safe.
    pub fn set_frequency(&mut self, frequency: f64) {
        self.frequency = frequency;
    }

    /// Changes the shape. Real-time safe.
    pub fn set_shape(&mut self, shape: LfoShape) {
        self.shape = shape;
    }

    /// Jumps to a position in the cycle, from 0 to 1.
    pub fn set_phase(&mut self, phase: f64) {
        self.phase = phase.rem_euclid(1.0);
    }

    /// Locks the oscillator to the music: one cycle every `beats_per_cycle`
    /// quarter notes, positioned so that a cycle starts on beat zero.
    ///
    /// Call it once per block with the position from the
    /// [`Transport`](crate::Transport) and the tempo's beats per sample, and
    /// the oscillator stays in step through tempo changes and jumps.
    pub fn sync_to_beats(
        &mut self,
        position_beats: f64,
        beats_per_cycle: f64,
        beats_per_sample: f64,
    ) {
        if beats_per_cycle > 0.0 {
            self.phase = (position_beats / beats_per_cycle).rem_euclid(1.0);
            self.frequency = beats_per_sample * self.sample_rate / beats_per_cycle;
        }
    }

    fn next_random(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    /// The value at the current position, from -1 to 1, without advancing.
    pub fn value(&self) -> f64 {
        match self.shape {
            LfoShape::SampleAndHold => self.held,
            shape => shape.value_at(self.phase),
        }
    }

    /// One cycle of the shape, for drawing; a sample-and-hold is drawn at
    /// the level it holds now. See [`LfoCycle`] and
    /// [`curve`](crate::curve).
    pub fn cycle(&self) -> LfoCycle {
        match self.shape {
            LfoShape::SampleAndHold => LfoCycle::holding(self.shape, self.held),
            shape => LfoCycle::new(shape),
        }
    }

    /// Returns the current value and moves on by one sample.
    #[inline]
    pub fn next_value(&mut self) -> f64 {
        let value = self.value();
        self.advance(1);
        value
    }

    /// Moves on by `samples` without producing values. Use it to read the
    /// oscillator once per block: take [`value`](Self::value), then skip the
    /// block's length.
    pub fn advance(&mut self, samples: usize) {
        let moved = self.phase + self.frequency / self.sample_rate * samples as f64;
        if !(0.0..1.0).contains(&moved) {
            self.held = self.next_random();
        }
        self.phase = moved.rem_euclid(1.0);
    }
}

/// How the energy of [`Noise`] is spread across the spectrum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoiseColor {
    /// Equal energy at every frequency. Sounds bright, like hiss.
    White,
    /// Equal energy in every octave, falling 3 dB per octave. Sounds even
    /// to the ear; the usual test signal for rooms and speakers.
    Pink,
    /// Falling 6 dB per octave. Sounds like a waterfall heard from afar.
    Brown,
}

/// A noise generator.
///
/// Each channel gets its own independent sequence, so stereo noise is wide.
/// The sequence is fixed by the seed, which makes renders repeatable.
///
/// The pink filter is Paul Kellet's refined method, a sum of first-order
/// sections accurate to about ±0.05 dB above 9 Hz at 44.1 kHz.
#[derive(Debug, Clone)]
pub struct Noise {
    color: NoiseColor,
    seed: u64,
    channels: Vec<NoiseChannel>,
}

#[derive(Debug, Clone, Copy)]
struct NoiseChannel {
    random: u64,
    pink: [f64; 7],
    brown: f64,
}

impl NoiseChannel {
    fn new(seed: u64, index: usize) -> Self {
        // Spread the seeds apart so the channels do not shadow each other.
        let mixed = seed.wrapping_add((index as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)) | 1;
        let mut channel = Self {
            random: mixed,
            pink: [0.0; 7],
            brown: 0.0,
        };
        for _ in 0..8 {
            channel.white();
        }
        channel
    }

    #[inline]
    fn white(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    #[inline]
    fn next(&mut self, color: NoiseColor) -> f64 {
        let white = self.white();
        match color {
            NoiseColor::White => white,
            NoiseColor::Pink => {
                let b = &mut self.pink;
                b[0] = 0.99886 * b[0] + white * 0.0555179;
                b[1] = 0.99332 * b[1] + white * 0.0750759;
                b[2] = 0.96900 * b[2] + white * 0.1538520;
                b[3] = 0.86650 * b[3] + white * 0.3104856;
                b[4] = 0.55000 * b[4] + white * 0.5329522;
                b[5] = -0.7616 * b[5] - white * 0.0168980;
                let pink = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + white * 0.5362;
                b[6] = white * 0.115926;
                // Scaled so that peaks beyond full scale are very rare, and
                // limited so that they never get out.
                (pink * 0.11).clamp(-1.0, 1.0)
            }
            NoiseColor::Brown => {
                // A leaky integrator, so it cannot wander off for ever.
                self.brown = 0.995 * self.brown + 0.05 * white;
                (self.brown * 0.6).clamp(-1.0, 1.0)
            }
        }
    }
}

impl Noise {
    /// Creates a generator with a fixed default seed.
    pub fn new(color: NoiseColor) -> Self {
        Self {
            color,
            seed: 0x1234_5678_9ABC_DEF0,
            channels: vec![NoiseChannel::new(0x1234_5678_9ABC_DEF0, 0)],
        }
    }

    /// Chooses the sequence. The same seed always gives the same noise.
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        let count = self.channels.len();
        self.channels = (0..count)
            .map(|index| NoiseChannel::new(seed, index))
            .collect();
        self
    }

    /// Changes the colour. Real-time safe.
    pub fn set_color(&mut self, color: NoiseColor) {
        self.color = color;
    }

    /// The next sample of the first channel's sequence, from -1 to 1.
    #[inline]
    pub fn next_sample(&mut self) -> f64 {
        self.channels[0].next(self.color)
    }
}

impl<S: Sample> Processor<S> for Noise {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.channels = (0..spec.num_channels.max(1))
            .map(|index| NoiseChannel::new(self.seed, index))
            .collect();
    }

    /// Fills the block with noise, replacing what was there.
    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let color = self.color;
        for (samples, channel) in buffer.iter_channels_mut().zip(&mut self.channels) {
            for sample in samples {
                *sample = S::from_f64(channel.next(color));
            }
        }
    }

    fn reset(&mut self) {
        for (index, channel) in self.channels.iter_mut().enumerate() {
            *channel = NoiseChannel::new(self.seed, index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Complex, RealFft};

    fn cycle(shape: LfoShape) -> Vec<f64> {
        let mut lfo = Lfo::new(shape, 1.0);
        lfo.set_sample_rate(8.0);
        (0..8).map(|_| lfo.next_value()).collect()
    }

    #[test]
    fn shapes_have_the_expected_values() {
        let near = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-12);
        assert!(near(
            &cycle(LfoShape::Triangle),
            &[1.0, 0.5, 0.0, -0.5, -1.0, -0.5, 0.0, 0.5]
        ));
        assert!(near(
            &cycle(LfoShape::SawUp),
            &[-1.0, -0.75, -0.5, -0.25, 0.0, 0.25, 0.5, 0.75]
        ));
        assert!(near(
            &cycle(LfoShape::SawDown),
            &[1.0, 0.75, 0.5, 0.25, 0.0, -0.25, -0.5, -0.75]
        ));
        assert!(near(
            &cycle(LfoShape::Square),
            &[1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0, -1.0]
        ));
        let sine = cycle(LfoShape::Sine);
        assert!(
            sine[0].abs() < 1e-12 && (sine[2] - 1.0).abs() < 1e-12 && (sine[6] + 1.0).abs() < 1e-12
        );
    }

    #[test]
    fn rate_is_exact_over_many_cycles() {
        let mut lfo = Lfo::new(LfoShape::SawUp, 3.0);
        lfo.set_sample_rate(48_000.0);
        let mut wraps = 0;
        let mut previous = lfo.next_value();
        // Ten seconds and a little more, so that the thirtieth wrap is not
        // sitting exactly on the last sample.
        for _ in 0..48_000 * 10 + 100 {
            let value = lfo.next_value();
            if value < previous {
                wraps += 1;
            }
            previous = value;
        }
        assert_eq!(wraps, 30);
    }

    #[test]
    fn sample_and_hold_changes_once_per_cycle_and_stays_in_range() {
        let mut lfo = Lfo::new(LfoShape::SampleAndHold, 10.0);
        lfo.set_sample_rate(1_000.0);
        let values: Vec<f64> = (0..1_000).map(|_| lfo.next_value()).collect();
        let changes = values.windows(2).filter(|pair| pair[0] != pair[1]).count();
        assert!(
            (9..=10).contains(&changes),
            "{changes} changes in ten cycles"
        );
        assert!(values.iter().all(|value| (-1.0..=1.0).contains(value)));
    }

    #[test]
    fn reading_once_per_block_matches_reading_every_sample() {
        let mut per_sample = Lfo::new(LfoShape::Sine, 2.0);
        let mut per_block = per_sample;
        for _ in 0..50 {
            let expected = per_sample.value();
            assert!((per_block.value() - expected).abs() < 1e-9);
            for _ in 0..64 {
                per_sample.next_value();
            }
            per_block.advance(64);
        }
    }

    #[test]
    fn syncs_to_the_beat() {
        let mut lfo = Lfo::new(LfoShape::SawUp, 1.0);
        lfo.set_sample_rate(48_000.0);
        // 120 bpm: two beats a second. One cycle per bar of four beats.
        let beats_per_sample = 2.0 / 48_000.0;
        lfo.sync_to_beats(5.0, 4.0, beats_per_sample);
        // Beat 5 is a quarter of the way through the second bar.
        assert!((lfo.value() + 0.5).abs() < 1e-12);
        // One bar lasts two seconds.
        lfo.advance(96_000);
        assert!((lfo.value() + 0.5).abs() < 1e-6);
    }

    fn render(color: NoiseColor, frames: usize) -> Vec<f64> {
        let mut noise = Noise::new(color).with_seed(42);
        (0..frames).map(|_| noise.next_sample()).collect()
    }

    /// Energy between two frequencies, in decibels, at 48 kHz.
    fn band_db(signal: &[f64], low: f64, high: f64) -> f64 {
        let len = signal.len();
        let mut fft = RealFft::new(len);
        let mut spectrum = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(signal, &mut spectrum);
        let bin = |hz: f64| (hz * len as f64 / 48_000.0) as usize;
        let energy: f64 = spectrum[bin(low)..bin(high)]
            .iter()
            .map(|value| value.norm_sqr())
            .sum();
        10.0 * energy.log10()
    }

    #[test]
    fn noise_stays_in_range_and_is_repeatable() {
        for color in [NoiseColor::White, NoiseColor::Pink, NoiseColor::Brown] {
            let first = render(color, 200_000);
            assert!(
                first.iter().all(|s| s.abs() <= 1.0),
                "{color:?} left full scale"
            );
            let mean = first.iter().sum::<f64>() / first.len() as f64;
            assert!(mean.abs() < 0.05, "{color:?} has a mean of {mean}");
            assert_eq!(first, render(color, 200_000));
            let mut other = Noise::new(color).with_seed(43);
            assert_ne!(first[0], other.next_sample());
        }
    }

    #[test]
    fn colours_have_the_right_slopes() {
        let len = 1 << 17;
        // White: the same energy per hertz, so three decibels more for each
        // octave, which is twice as wide.
        let white = render(NoiseColor::White, len);
        let rise = band_db(&white, 4_000.0, 8_000.0) - band_db(&white, 500.0, 1_000.0);
        assert!(
            (rise - 9.03).abs() < 1.0,
            "white rose {rise:.2} dB over three octaves"
        );

        // Pink: the same energy in every octave.
        let pink = render(NoiseColor::Pink, len);
        for (low, high) in [(100.0, 200.0), (800.0, 1_600.0), (6_400.0, 12_800.0)] {
            let difference = band_db(&pink, low, high) - band_db(&pink, 400.0, 800.0);
            assert!(
                difference.abs() < 1.5,
                "pink {low}-{high} Hz: {difference:.2} dB"
            );
        }

        // Brown: three decibels less in each higher octave.
        let brown = render(NoiseColor::Brown, len);
        let fall = band_db(&brown, 500.0, 1_000.0) - band_db(&brown, 4_000.0, 8_000.0);
        assert!(
            (fall - 9.03).abs() < 1.5,
            "brown fell {fall:.2} dB over three octaves"
        );
    }

    #[test]
    fn channels_are_independent() {
        let mut noise = Noise::new(NoiseColor::White);
        Processor::<f32>::prepare(
            &mut noise,
            &ProcessSpec {
                sample_rate: 48_000.0,
                max_block_size: 1_024,
                num_channels: 2,
            },
        );
        let mut buffer = AudioBuffer::<f32>::new(2, 1_024);
        noise.process(&mut buffer);
        let same = buffer
            .channel(0)
            .iter()
            .zip(buffer.channel(1))
            .filter(|(left, right)| left == right)
            .count();
        assert!(same < 4);
        assert!(buffer.peak() > 0.9);

        let first = buffer.clone();
        Processor::<f32>::reset(&mut noise);
        noise.process(&mut buffer);
        assert!(buffer == first, "reset did not restart the sequence");
    }
}
