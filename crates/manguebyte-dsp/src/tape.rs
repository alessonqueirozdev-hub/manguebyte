use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::delay::DelayLine;
use crate::numeric::{glide_to, limit};
use crate::{DcBlocker, DryWet, Oversampled, ProcessSpec, Processor};

/// Limits of the settings.
const MAX_DRIVE_DB: f64 = 30.0;
const MAX_ASYMMETRY: f64 = 0.5;
const MAX_WOW_CENTS: f64 = 30.0;
const MIN_WOW_HZ: f64 = 0.5;
const MAX_WOW_HZ: f64 = 4.0;
const MAX_FLUTTER_CENTS: f64 = 10.0;
const MIN_FLUTTER_HZ: f64 = 2.0;
const MAX_FLUTTER_HZ: f64 = 25.0;
/// How far drift moves the wow's rate and depth, at most, as a fraction.
const DRIFT_SPAN: f64 = 0.25;

/// The pre-emphasis: a first-order shelf with its zero at 1.5 kHz and its
/// pole at 4.7 kHz, rising by 10 dB.
const EMPHASIS_ZERO_HZ: f64 = 1_500.0;
const EMPHASIS_POLE_HZ: f64 = 4_743.4;

/// How many times the sample rate the saturation runs at.
const OVERSAMPLING: usize = 4;

/// How long a change of drive, asymmetry or mix takes, and a change of the
/// depth of wow or flutter.
const GAIN_GLIDE_SECONDS: f64 = 0.02;
const DEPTH_GLIDE_SECONDS: f64 = 0.2;

/// The sweep of a delay, in samples either side of its centre, that moves
/// the pitch by `cents` at most when it sweeps as a sine at `hertz`: the
/// pitch goes as `1 - dD/dn`, and a sine of amplitude `A` changes by at
/// most `2π f A / fs` per sample.
fn sweep_for_cents(cents: f64, hertz: f64, sample_rate: f64) -> f64 {
    ((cents / 1_200.0).exp2() - 1.0) * sample_rate / (TAU * hertz)
}

/// The tape curve: an asymmetric soft saturation, run oversampled.
///
/// `f(x) = (tanh(d x + b) - tanh(b)) / (d (1 - tanh²(b)))`: the drive `d`
/// pushes the signal into a `tanh` whose middle is moved off zero by the
/// asymmetry `b`, which bends the two halves of the wave differently and so
/// makes even harmonics as well as odd ones, as magnetic tape does. The
/// offset is taken back out so that silence stays silent, and the gain is
/// scaled back by the slope at zero, so that quiet signals keep their level
/// whatever the drive: the drive sets how hard the loud parts are pressed.
#[derive(Debug, Clone)]
struct TapeCurve<S: Sample> {
    drive: Smoothed<f64>,
    asymmetry: Smoothed<f64>,
    marker: core::marker::PhantomData<S>,
}

impl<S: Sample> TapeCurve<S> {
    #[inline]
    fn apply(drive: f64, asymmetry: f64, x: f64) -> f64 {
        let offset = asymmetry.tanh();
        let slope = drive * (1.0 - offset * offset);
        ((drive * x + asymmetry).tanh() - offset) / slope
    }
}

impl<S: Sample> Processor<S> for TapeCurve<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.drive.set_ramp(spec.sample_rate, GAIN_GLIDE_SECONDS);
        self.asymmetry
            .set_ramp(spec.sample_rate, GAIN_GLIDE_SECONDS);
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let (mut drive_after, mut asymmetry_after) = (self.drive, self.asymmetry);
        for samples in buffer.iter_channels_mut() {
            let (mut drive, mut asymmetry) = (self.drive, self.asymmetry);
            for sample in samples {
                let (d, b) = (drive.next_value(), asymmetry.next_value());
                *sample = S::from_f64(Self::apply(d, b, sample.to_f64()));
            }
            (drive_after, asymmetry_after) = (drive, asymmetry);
        }
        if buffer.num_channels() == 0 {
            drive_after.skip(buffer.num_frames());
            asymmetry_after.skip(buffer.num_frames());
        }
        (self.drive, self.asymmetry) = (drive_after, asymmetry_after);
    }

    fn reset(&mut self) {
        self.drive.set_immediate(self.drive.target());
        self.asymmetry.set_immediate(self.asymmetry.target());
    }
}

/// A first-order section `(b0 + b1 z⁻¹) / (1 + a1 z⁻¹)`, with its memory
/// per channel.
#[derive(Debug, Clone, Copy)]
struct Shelf {
    b0: f64,
    b1: f64,
    a1: f64,
}

impl Shelf {
    /// `(1 + s/ωz) / (1 + s/ωp)` by the bilinear transform: the
    /// pre-emphasis, or with the two swapped, its exact inverse.
    fn new(zero_hz: f64, pole_hz: f64, sample_rate: f64) -> Self {
        let k = 2.0 * sample_rate;
        let (z, p) = (k / (TAU * zero_hz), k / (TAU * pole_hz));
        let norm = 1.0 / (1.0 + p);
        Self {
            b0: (1.0 + z) * norm,
            b1: (1.0 - z) * norm,
            a1: (1.0 - p) * norm,
        }
    }

    /// One sample; `memory` holds the last input and output.
    #[inline]
    fn step(&self, memory: &mut [f64; 2], x: f64) -> f64 {
        let y = self.b0 * x + self.b1 * memory[0] - self.a1 * memory[1];
        *memory = [x, y];
        y
    }
}

/// A smooth random value in -1 to 1 that takes a new target once per
/// cycle of the wow and eases towards it over the cycle.
#[derive(Debug, Clone, Copy)]
struct Wander {
    from: f64,
    to: f64,
}

impl Wander {
    #[inline]
    fn at(&self, phase: f64) -> f64 {
        let ease = phase * phase * (3.0 - 2.0 * phase);
        self.from + (self.to - self.from) * ease
    }
}

/// The tape transport: the phases of wow and flutter, and the drift.
#[derive(Debug, Clone, Copy)]
struct Transport {
    wow_phase: f64,
    flutter_phase: f64,
    /// Sweeps in samples either side of the centre.
    wow_sweep: Smoothed<f64>,
    flutter_sweep: Smoothed<f64>,
    /// How much the wow's rate and depth wander, from 0 to 1.
    drift: Smoothed<f64>,
    rate: Wander,
    depth: Wander,
    random: u64,
}

impl Transport {
    fn next_random(&mut self) -> f64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        (self.random >> 11) as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }

    /// How far the read is moved from the centre on this sample, in
    /// samples; moves on by one sample.
    #[inline]
    fn step(&mut self, wow_hz: f64, flutter_hz: f64, sample_rate: f64) -> f64 {
        let drift = self.drift.next_value() * DRIFT_SPAN;
        let depth = 1.0 + drift * self.depth.at(self.wow_phase);
        let rate = 1.0 + drift * self.rate.at(self.wow_phase);
        let offset = self.wow_sweep.next_value() * depth * (TAU * self.wow_phase).sin()
            + self.flutter_sweep.next_value() * (TAU * self.flutter_phase).sin();
        self.wow_phase += wow_hz * rate / sample_rate;
        if self.wow_phase >= 1.0 {
            self.wow_phase -= 1.0;
            self.rate = Wander {
                from: self.rate.to,
                to: self.next_random(),
            };
            self.depth = Wander {
                from: self.depth.to,
                to: self.next_random(),
            };
        }
        self.flutter_phase += flutter_hz / sample_rate;
        if self.flutter_phase >= 1.0 {
            self.flutter_phase -= 1.0;
        }
        offset
    }
}

/// Tape: saturation with pre- and de-emphasis, and the wow and flutter of
/// a transport that does not run quite steadily.
///
/// The signal goes through what a tape machine does to it, in order:
///
/// 1. **Pre-emphasis**: a first-order shelf, 10 dB up from about 1.5 to
///    4.7 kHz, as a recorder lifts the treble before it reaches the tape.
/// 2. **Saturation**, at four times the sample rate
///    ([`Oversampled`]), by a curve of this crate's own: `tanh` with its
///    middle moved off zero by the asymmetry, the offset taken back out and
///    the gain scaled back by the slope at zero, so that quiet signals keep
///    their level whatever the drive, loud ones are pressed, and the two
///    halves of the wave are bent differently, which adds even harmonics to
///    the odd ones. The emphasis makes the treble saturate first, as on
///    tape.
/// 3. **De-emphasis**: the exact inverse of the shelf, so that below
///    saturation the two cancel.
/// 4. A [`DcBlocker`], for the offset the asymmetry leaves.
/// 5. **Wow and flutter**: a delay line read at a moving position by cubic
///    interpolation. A delay that sweeps as a sine at `f` by `A` samples
///    moves the pitch by at most `2π f A / fs`, so the sweeps are set from
///    a depth in cents and a rate: wow slow and deep, flutter fast and
///    shallow. The pitch then rises by the depth at most and falls by the
///    same ratio, which is a hair more in cents: 10.06 for a depth of 10. **Drift** makes the wow's rate and depth wander, by up to a
///    quarter either way, along a smooth random path that takes a new turn
///    once per cycle, from a seed, so that renders repeat.
///
/// The output is `dry * (1 - mix) + tape * mix`, with the dry signal
/// delayed to line up ([`DryWet`]). All settings glide (the drive,
/// asymmetry and mix over 20 ms, the depths of wow and flutter over
/// 200 ms), and all setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, TapeSaturation};
///
/// let mut tape = TapeSaturation::<f32>::new().with_drive_db(12.0).with_wow_cents(8.0);
/// tape.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 512, num_channels: 2 });
/// // The oversampling filters and the centre of the wow's delay.
/// assert_eq!(tape.latency_samples(), 53 + 358);
///
/// let mut block = AudioBuffer::<f32>::new(2, 512);
/// tape.process(&mut block);
/// ```
///
/// # Limits
///
/// The latency is fixed, so that a host can rely on it: the delay of the
/// oversampling filters and the centre of the wow's delay, which leaves
/// room for the deepest wow and flutter the limits allow (7.5 ms at
/// 48 kHz), even while they are off. The drive is limited to 0 to 30 dB,
/// the asymmetry to `0.0..=0.5`, wow to 30 cents at 0.5 to 4 Hz, flutter to
/// 10 cents at 2 to 25 Hz, drift to `0.0..=1.0`. With drift, the wow's
/// largest deviation can reach `1.25²` of the depth set. There is no
/// modelling of a tape's frequency response, noise or dropouts, and the
/// curve is memoryless: no hysteresis loop.
#[derive(Debug, Clone)]
pub struct TapeSaturation<S: Sample = f32> {
    drive_db: f64,
    asymmetry: f64,
    wow_cents: f64,
    wow_hz: f64,
    flutter_cents: f64,
    flutter_hz: f64,
    drift: f64,
    seed: u64,
    sample_rate: f64,
    pre: Shelf,
    de: Shelf,
    /// The memory of the two shelves, per channel.
    emphasis: Vec<[[f64; 2]; 2]>,
    saturation: Oversampled<TapeCurve<S>, S>,
    dc: DcBlocker<S>,
    transport: Transport,
    lines: Vec<DelayLine<S>>,
    centre: usize,
    /// The longest piece processed at once: the block size prepared for.
    block: usize,
    mixer: DryWet<S>,
}

impl<S: Sample> Default for TapeSaturation<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> TapeSaturation<S> {
    /// Creates a tape effect with a drive of 6 dB, an asymmetry of 0.15,
    /// wow of 3 cents at 0.8 Hz, flutter of 1 cent at 7 Hz, a drift of 0.5
    /// and fully wet.
    pub fn new() -> Self {
        let curve = TapeCurve {
            drive: Smoothed::new(1.0),
            asymmetry: Smoothed::new(0.15),
            marker: core::marker::PhantomData,
        };
        let mut tape = Self {
            drive_db: 6.0,
            asymmetry: 0.15,
            wow_cents: 3.0,
            wow_hz: 0.8,
            flutter_cents: 1.0,
            flutter_hz: 7.0,
            drift: 0.5,
            seed: 0x7A9E_5EED,
            sample_rate: 44_100.0,
            pre: Shelf::new(EMPHASIS_ZERO_HZ, EMPHASIS_POLE_HZ, 44_100.0),
            de: Shelf::new(EMPHASIS_POLE_HZ, EMPHASIS_ZERO_HZ, 44_100.0),
            emphasis: Vec::new(),
            saturation: Oversampled::new(curve, OVERSAMPLING),
            dc: DcBlocker::new(),
            transport: Transport {
                wow_phase: 0.0,
                flutter_phase: 0.0,
                wow_sweep: Smoothed::new(0.0),
                flutter_sweep: Smoothed::new(0.0),
                drift: Smoothed::new(0.5),
                rate: Wander { from: 0.0, to: 0.0 },
                depth: Wander { from: 0.0, to: 0.0 },
                random: 1,
            },
            lines: Vec::new(),
            centre: 0,
            block: 0,
            mixer: DryWet::new().with_mix(1.0),
        };
        tape.set_drive_db(6.0);
        tape.snap();
        tape
    }

    /// Sets how hard the tape is driven, in decibels, limited to 0 to
    /// 30 dB. Quiet signals keep their level; loud ones are pressed harder.
    pub fn set_drive_db(&mut self, db: f64) {
        self.drive_db = limit(db, 0.0, MAX_DRIVE_DB);
        let drive = 10f64.powf(self.drive_db / 20.0);
        glide_to(&mut self.saturation.inner_mut().drive, drive);
    }

    /// Sets how differently the two halves of the wave are bent, from
    /// `0.0` (alike: odd harmonics only) to `0.5`.
    pub fn set_asymmetry(&mut self, asymmetry: f64) {
        self.asymmetry = limit(asymmetry, 0.0, MAX_ASYMMETRY);
        glide_to(&mut self.saturation.inner_mut().asymmetry, self.asymmetry);
    }

    /// Sets the depth of the wow, the slow wandering of the pitch, in
    /// cents either way, limited to `0.0..=30.0`.
    pub fn set_wow_cents(&mut self, cents: f64) {
        self.wow_cents = limit(cents, 0.0, MAX_WOW_CENTS);
        self.update_sweeps();
    }

    /// Sets the rate of the wow, limited to 0.5 to 4 Hz.
    pub fn set_wow_hz(&mut self, hertz: f64) {
        self.wow_hz = limit(hertz, MIN_WOW_HZ, MAX_WOW_HZ);
        self.update_sweeps();
    }

    /// Sets the depth of the flutter, the fast trembling of the pitch, in
    /// cents either way, limited to `0.0..=10.0`.
    pub fn set_flutter_cents(&mut self, cents: f64) {
        self.flutter_cents = limit(cents, 0.0, MAX_FLUTTER_CENTS);
        self.update_sweeps();
    }

    /// Sets the rate of the flutter, limited to 2 to 25 Hz.
    pub fn set_flutter_hz(&mut self, hertz: f64) {
        self.flutter_hz = limit(hertz, MIN_FLUTTER_HZ, MAX_FLUTTER_HZ);
        self.update_sweeps();
    }

    /// Sets how much the wow's rate and depth wander, from `0.0` (a steady
    /// sine) to `1.0` (by up to a quarter either way).
    pub fn set_drift(&mut self, drift: f64) {
        self.drift = limit(drift, 0.0, 1.0);
        glide_to(&mut self.transport.drift, self.drift);
    }

    /// Chooses the drift's random path. The same seed gives the same path,
    /// from the next [`reset`](Processor::reset) or `prepare`.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
    }

    /// Sets the balance between the untouched signal (`0.0`) and the tape
    /// (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mixer.set_mix(limit(mix, 0.0, 1.0));
    }

    /// Builder form of [`set_drive_db`](Self::set_drive_db).
    #[must_use]
    pub fn with_drive_db(mut self, db: f64) -> Self {
        self.set_drive_db(db);
        self
    }

    /// Builder form of [`set_asymmetry`](Self::set_asymmetry).
    #[must_use]
    pub fn with_asymmetry(mut self, asymmetry: f64) -> Self {
        self.set_asymmetry(asymmetry);
        self
    }

    /// Builder form of [`set_wow_cents`](Self::set_wow_cents).
    #[must_use]
    pub fn with_wow_cents(mut self, cents: f64) -> Self {
        self.set_wow_cents(cents);
        self
    }

    /// Builder form of [`set_wow_hz`](Self::set_wow_hz).
    #[must_use]
    pub fn with_wow_hz(mut self, hertz: f64) -> Self {
        self.set_wow_hz(hertz);
        self
    }

    /// Builder form of [`set_flutter_cents`](Self::set_flutter_cents).
    #[must_use]
    pub fn with_flutter_cents(mut self, cents: f64) -> Self {
        self.set_flutter_cents(cents);
        self
    }

    /// Builder form of [`set_flutter_hz`](Self::set_flutter_hz).
    #[must_use]
    pub fn with_flutter_hz(mut self, hertz: f64) -> Self {
        self.set_flutter_hz(hertz);
        self
    }

    /// Builder form of [`set_drift`](Self::set_drift).
    #[must_use]
    pub fn with_drift(mut self, drift: f64) -> Self {
        self.set_drift(drift);
        self
    }

    /// Builder form of [`set_seed`](Self::set_seed).
    #[must_use]
    pub fn with_seed(mut self, seed: u64) -> Self {
        self.set_seed(seed);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    #[must_use]
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// The tape curve at the drive and asymmetry as last set: what a steady
    /// level `x` comes out as, before the emphasis and the transport.
    pub fn curve(&self, x: f64) -> f64 {
        TapeCurve::<S>::apply(10f64.powf(self.drive_db / 20.0), self.asymmetry, x)
    }

    fn update_sweeps(&mut self) {
        let rate = self.sample_rate;
        let wow = sweep_for_cents(self.wow_cents, self.wow_hz, rate);
        let flutter = sweep_for_cents(self.flutter_cents, self.flutter_hz, rate);
        glide_to(&mut self.transport.wow_sweep, wow);
        glide_to(&mut self.transport.flutter_sweep, flutter);
    }

    /// The centre of the wow's delay: room for the deepest sweeps the
    /// limits allow, drift included, and for the cubic read.
    fn centre_for(sample_rate: f64) -> usize {
        let wow = sweep_for_cents(MAX_WOW_CENTS, MIN_WOW_HZ, sample_rate) * (1.0 + DRIFT_SPAN);
        let flutter = sweep_for_cents(MAX_FLUTTER_CENTS, MIN_FLUTTER_HZ, sample_rate);
        (wow + flutter).ceil() as usize + 2
    }

    /// Jumps every gliding value to where it is heading and starts the
    /// transport and its random path again.
    fn snap(&mut self) {
        self.update_sweeps();
        let transport = &mut self.transport;
        transport
            .wow_sweep
            .set_immediate(transport.wow_sweep.target());
        transport
            .flutter_sweep
            .set_immediate(transport.flutter_sweep.target());
        transport.drift.set_immediate(self.drift);
        transport.wow_phase = 0.0;
        transport.flutter_phase = 0.0;
        transport.random = self.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let first = transport.next_random();
        let second = transport.next_random();
        transport.rate = Wander {
            from: 0.0,
            to: first,
        };
        transport.depth = Wander {
            from: 0.0,
            to: second,
        };
    }

    /// The tape, on a piece no longer than the block size prepared for.
    fn run(&mut self, buffer: &mut AudioBuffer<S>) {
        self.mixer.push_dry(buffer);
        for (samples, memory) in buffer.iter_channels_mut().zip(&mut self.emphasis) {
            for sample in samples {
                *sample = S::from_f64(self.pre.step(&mut memory[0], sample.to_f64()));
            }
        }
        self.saturation.process(buffer);
        for (samples, memory) in buffer.iter_channels_mut().zip(&mut self.emphasis) {
            for sample in samples {
                *sample = S::from_f64(self.de.step(&mut memory[1], sample.to_f64()));
            }
        }
        self.dc.process(buffer);

        let (wow, flutter, rate) = (self.wow_hz, self.flutter_hz, self.sample_rate);
        let centre = self.centre as f64;
        let mut after = self.transport;
        for (samples, line) in buffer.iter_channels_mut().zip(&mut self.lines) {
            let mut transport = self.transport;
            for sample in samples {
                let offset = transport.step(wow, flutter, rate);
                let input = *sample;
                *sample = line.read_cubic(centre + offset);
                line.push(input);
            }
            after = transport;
        }
        if self.lines.is_empty() || buffer.num_channels() == 0 {
            for _ in 0..buffer.num_frames() {
                after.step(wow, flutter, rate);
            }
        }
        self.transport = after;
        self.mixer.mix_wet(buffer);
    }
}

impl<S: Sample> Processor<S> for TapeSaturation<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.pre = Shelf::new(EMPHASIS_ZERO_HZ, EMPHASIS_POLE_HZ, spec.sample_rate);
        self.de = Shelf::new(EMPHASIS_POLE_HZ, EMPHASIS_ZERO_HZ, spec.sample_rate);
        self.emphasis = vec![[[0.0; 2]; 2]; spec.num_channels];
        self.saturation.prepare(spec);
        self.dc.prepare(spec);
        self.centre = Self::centre_for(spec.sample_rate);
        self.block = spec.max_block_size.max(1);
        self.lines = (0..spec.num_channels)
            .map(|_| DelayLine::new(self.centre * 2 + 4))
            .collect();
        let transport = &mut self.transport;
        for value in [
            &mut transport.wow_sweep,
            &mut transport.flutter_sweep,
            &mut transport.drift,
        ] {
            value.set_ramp(spec.sample_rate, DEPTH_GLIDE_SECONDS);
        }
        let latency = self.latency_samples();
        self.mixer.set_max_latency(latency);
        self.mixer.set_latency(latency);
        self.mixer.prepare(spec);
        self.snap();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        if self.lines.is_empty() {
            // Not prepared: nothing to keep the signal in.
            return;
        }
        if buffer.num_channels() == 0 {
            // No channel to process, but the tape still moves.
            let (wow, flutter, rate) = (self.wow_hz, self.flutter_hz, self.sample_rate);
            for _ in 0..buffer.num_frames() {
                self.transport.step(wow, flutter, rate);
            }
            return;
        }
        let piece = self.block;
        let frames = buffer.num_frames();
        let mut start = 0;
        while start < frames {
            let end = (start + piece).min(frames);
            buffer.windowed(start..end, |part| self.run(part));
            start = end;
        }
    }

    fn reset(&mut self) {
        self.emphasis.fill([[0.0; 2]; 2]);
        Processor::<S>::reset(&mut self.saturation);
        Processor::<S>::reset(&mut self.dc);
        for line in &mut self.lines {
            line.clear();
        }
        self.mixer.reset();
        self.snap();
    }

    fn latency_samples(&self) -> usize {
        self.saturation.latency_samples() + self.centre
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn prepared(tape: TapeSaturation<f64>, channels: usize) -> TapeSaturation<f64> {
        let mut tape = tape;
        tape.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        tape
    }

    /// A tape with nothing moving: no wow, no flutter, no drift.
    fn still() -> TapeSaturation<f64> {
        TapeSaturation::new()
            .with_wow_cents(0.0)
            .with_flutter_cents(0.0)
            .with_drift(0.0)
    }

    /// The pitch of a tone over each of its periods, in cents from
    /// `frequency`, from the times of its upward zero crossings, found by
    /// straight lines between samples.
    fn pitch_track(signal: &[f64], frequency: f64) -> Vec<f64> {
        let mut crossings = Vec::new();
        for n in 1..signal.len() {
            let (a, b) = (signal[n - 1], signal[n]);
            if a < 0.0 && b >= 0.0 {
                crossings.push(n as f64 - 1.0 + a / (a - b));
            }
        }
        crossings
            .windows(2)
            .map(|pair| 1_200.0 * (RATE / (pair[1] - pair[0]) / frequency).log2())
            .collect()
    }

    fn extremes(values: &[f64]) -> (f64, f64) {
        values.iter().fold((f64::MAX, f64::MIN), |(low, high), &v| {
            (low.min(v), high.max(v))
        })
    }

    #[test]
    fn wow_and_flutter_move_the_pitch_by_the_stated_cents() {
        // A quiet tone, so that the curve is a straight line, through a
        // transport with only wow or only flutter. A sweep that raises the
        // pitch by `c` cents at most lowers it by a little more, since
        // 1 - x is further from 1 in cents than 1 + x.
        for (wow, flutter) in [((10.0, 1.0), (0.0, 7.0)), ((0.0, 1.0), (4.0, 8.0))] {
            let mut tape = prepared(
                still()
                    .with_drive_db(0.0)
                    .with_wow_cents(wow.0)
                    .with_wow_hz(wow.1)
                    .with_flutter_cents(flutter.0)
                    .with_flutter_hz(flutter.1),
                1,
            );
            let mut tone = sine::<f64>(1, 3 * 48_000, 1_000.0, RATE, 0.01);
            run_in_blocks(&mut tape, &mut tone, 512);
            let track = pitch_track(&tone.channel(0)[24_000..], 1_000.0);
            let cents = wow.0 + flutter.0;
            let (low, high) = extremes(&track);
            let lowest = 1_200.0 * (2.0 - (cents / 1_200.0f64).exp2()).log2();
            assert!(
                (high - cents).abs() < 0.1,
                "{wow:?} {flutter:?}: up to {high}"
            );
            assert!(
                (low - lowest).abs() < 0.1,
                "{wow:?} {flutter:?}: down to {low}"
            );
        }
    }

    #[test]
    fn drift_wanders_from_cycle_to_cycle_and_repeats_from_its_seed() {
        let render = |seed: u64| {
            let mut tape = prepared(
                still()
                    .with_drive_db(0.0)
                    .with_wow_cents(10.0)
                    .with_wow_hz(2.0)
                    .with_drift(1.0)
                    .with_seed(seed),
                1,
            );
            let mut tone = sine::<f64>(1, 4 * 48_000, 1_000.0, RATE, 0.01);
            run_in_blocks(&mut tape, &mut tone, 512);
            tone
        };
        let first = render(1);
        assert!(render(1) == first);
        assert!(render(2) != first);
        // The deepest cycle of each half-second: they differ, and none goes
        // beyond what a quarter more depth at a quarter more rate allows.
        let track = pitch_track(&first.channel(0)[24_000..], 1_000.0);
        let peaks: Vec<f64> = track.chunks(500).map(|chunk| extremes(chunk).1).collect();
        let (least, most) = extremes(&peaks);
        assert!(most - least > 1.0, "the wow does not wander: {peaks:?}");
        assert!(most < 10.0 * 1.25 * 1.25 + 0.1, "{most}");
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

    /// The fundamental and the second and third harmonics of a 500 Hz tone
    /// of `level` through `tape`.
    fn harmonics(tape: TapeSaturation<f64>, level: f64) -> [f64; 3] {
        const LEN: usize = 24_000;
        let mut tape = prepared(tape, 1);
        let mut tone = sine::<f64>(1, 2 * LEN, 500.0, RATE, level);
        run_in_blocks(&mut tape, &mut tone, 512);
        // Bin k is 2k hertz.
        let bins = spectrum(tone.channel(0), LEN);
        [bins[250], bins[500], bins[750]]
    }

    #[test]
    fn quiet_signals_pass_unchanged_and_loud_ones_saturate() {
        // Quiet: the curve's slope at zero is 1 at any drive, and the two
        // shelves cancel.
        let [fundamental, second, third] = harmonics(still().with_drive_db(12.0), 0.001);
        assert!(db(fundamental / 0.001).abs() < 0.01, "{fundamental}");
        assert!(db(second / fundamental) < -60.0 && db(third / fundamental) < -60.0);

        // Loud and symmetric: odd harmonics only.
        let [fundamental, second, third] =
            harmonics(still().with_drive_db(18.0).with_asymmetry(0.0), 0.5);
        assert!(
            db(third / fundamental) > -30.0,
            "{}",
            db(third / fundamental)
        );
        assert!(
            db(second / fundamental) < -100.0,
            "{}",
            db(second / fundamental)
        );
        // Pressed: well below the level that went in.
        assert!(fundamental < 0.3, "{fundamental}");

        // Loud and asymmetric: even harmonics too.
        let [fundamental, second, _] =
            harmonics(still().with_drive_db(18.0).with_asymmetry(0.3), 0.5);
        assert!(
            db(second / fundamental) > -30.0,
            "{}",
            db(second / fundamental)
        );
    }

    /// The largest component between the harmonics of a tone on bin
    /// `tone`, up to bin `up_to`, against the tone.
    fn folded_db(bins: &[f64], tone: usize, up_to: usize) -> f64 {
        let folded = bins[..up_to]
            .iter()
            .enumerate()
            .filter(|(bin, _)| bin % tone != 0)
            .map(|(_, &level)| level)
            .fold(0.0, f64::max);
        db(bins[tone] / folded)
    }

    #[test]
    fn oversampling_keeps_folded_harmonics_down() {
        // A 5 kHz tone driven hard. Its harmonics above 24 kHz fold back
        // between the harmonics below; the same curve and shelves run at the
        // sample rate itself show how much the oversampling saves. Bin k is
        // 2k hertz. Up to 20 kHz what folds back is gone; between 20 and
        // 24 kHz, the transition band of the oversampling filters, some of
        // it is left.
        const LEN: usize = 24_000;
        for drive in [6.0, 12.0] {
            let mut tape = prepared(still().with_drive_db(drive).with_asymmetry(0.2), 1);
            let input = sine::<f64>(1, 2 * LEN, 5_000.0, RATE, 0.5);
            let mut output = input.clone();
            run_in_blocks(&mut tape, &mut output, 512);
            let bins = spectrum(output.channel(0), LEN);
            let (audible, all) = (
                folded_db(&bins, 2_500, 10_000),
                folded_db(&bins, 2_500, 12_001),
            );

            let (pre, de) = (
                Shelf::new(EMPHASIS_ZERO_HZ, EMPHASIS_POLE_HZ, RATE),
                Shelf::new(EMPHASIS_POLE_HZ, EMPHASIS_ZERO_HZ, RATE),
            );
            let (mut a, mut b) = ([0.0; 2], [0.0; 2]);
            let d = 10f64.powf(drive / 20.0);
            let plain: Vec<f64> = input
                .channel(0)
                .iter()
                .map(|&x| {
                    let y = TapeCurve::<f64>::apply(d, 0.2, pre.step(&mut a, x));
                    de.step(&mut b, y)
                })
                .collect();
            let bins = spectrum(&plain, LEN);
            let (plain_audible, plain_all) = (
                folded_db(&bins, 2_500, 10_000),
                folded_db(&bins, 2_500, 12_001),
            );
            assert!(
                audible > 100.0 && plain_audible < 40.0,
                "drive {drive}: up to 20 kHz {audible} dB down, {plain_audible} dB without"
            );
            assert!(
                all - plain_all > 10.0,
                "drive {drive}: {all} dB down, {plain_all} dB without"
            );
        }
    }

    #[test]
    fn the_signal_arrives_at_the_reported_latency() {
        let mut tape = prepared(still().with_drive_db(0.0), 1);
        let latency = tape.latency_samples();
        assert_eq!(latency, 53 + TapeSaturation::<f64>::centre_for(RATE));
        let mut impulse = AudioBuffer::<f64>::new(1, 4_096);
        impulse.channel_mut(0)[0] = 0.001;
        run_in_blocks(&mut tape, &mut impulse, 512);
        let (arrival, _) =
            impulse
                .channel(0)
                .iter()
                .enumerate()
                .fold((0, 0.0f64), |(at, peak), (n, &s)| {
                    if s.abs() > peak {
                        (n, s.abs())
                    } else {
                        (at, peak)
                    }
                });
        assert_eq!(arrival, latency);

        // The dry signal is delayed to line up: all dry is the input, late.
        let input = noise::<f64>(1, 3_000, 9);
        let mut tape = prepared(still().with_mix(0.0), 1);
        let mut output = input.clone();
        run_in_blocks(&mut tape, &mut output, 333);
        assert_eq!(output.channel(0)[..latency], vec![0.0; latency][..]);
        assert_eq!(
            output.channel(0)[latency..],
            input.channel(0)[..3_000 - latency]
        );
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut tape = prepared(TapeSaturation::new().with_wow_cents(20.0), 2);
            let mut signal = noise::<f64>(2, 6_000, 3);
            let mut start = 0;
            for (end, amount) in [(2_000, 0.9), (4_000, 0.1), (6_000, 0.5)] {
                signal.windowed(start..end, |part| run_in_blocks(&mut tape, part, block));
                tape.set_drive_db(30.0 * amount);
                tape.set_asymmetry(amount / 2.0);
                tape.set_wow_hz(4.0 * amount);
                tape.set_flutter_cents(10.0 * amount);
                tape.set_drift(amount);
                tape.set_mix(amount);
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
    fn it_stays_bounded_at_the_extremes_and_survives_garbage() {
        let mut tape = prepared(
            TapeSaturation::new()
                .with_drive_db(30.0)
                .with_asymmetry(0.5)
                .with_wow_cents(30.0)
                .with_wow_hz(0.5)
                .with_flutter_cents(10.0)
                .with_flutter_hz(2.0)
                .with_drift(1.0),
            2,
        );
        let mut signal = noise::<f64>(2, 48_000, 4);
        run_in_blocks(&mut tape, &mut signal, 512);
        let peak = strict_buffer_peak(&signal);
        assert!(peak < 1.0, "{peak}");
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            tape.set_drive_db(garbage);
            tape.set_asymmetry(garbage);
            tape.set_wow_cents(garbage);
            tape.set_wow_hz(garbage);
            tape.set_flutter_cents(garbage);
            tape.set_flutter_hz(garbage);
            tape.set_drift(garbage);
            tape.set_mix(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut tape, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 2.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_starts_the_tape_again() {
        let build = || prepared(TapeSaturation::new().with_drift(1.0), 2);
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 3_000, 1);
        run_in_blocks(&mut fresh, &mut expected, 512);
        let mut used = build();
        let mut other = noise::<f64>(2, 5_000, 5);
        run_in_blocks(&mut used, &mut other, 512);
        used.reset();
        let mut again = noise::<f64>(2, 3_000, 1);
        run_in_blocks(&mut used, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut tape = TapeSaturation::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        tape.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        tape.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 2,
        });
        let mut long = AudioBuffer::<f32>::new(2, 1_000);
        tape.process(&mut long);
        assert!((tape.curve(0.0)).abs() < 1e-15);
        assert!(tape.curve(10.0) < 1.0);
        tape.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
