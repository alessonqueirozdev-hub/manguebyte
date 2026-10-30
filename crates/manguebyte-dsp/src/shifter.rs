use core::f64::consts::{PI, TAU};

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::numeric::{glide_to, limit};
use crate::{ProcessSpec, Processor};

/// Most first-order sections the two all-pass chains have between them.
const MAX_SECTIONS: usize = 32;
/// Fewest.
const MIN_SECTIONS: usize = 8;
/// Below this frequency, and as far below half the sample rate, the two
/// outputs of the network drift from 90 degrees apart.
const EDGE_HZ: f64 = 20.0;
/// How far down the image is meant to be between the edges, in decibels.
const IMAGE_DB: f64 = 90.0;

/// How long a change of shift, feedback or mix takes.
const GLIDE_SECONDS: f64 = 0.02;
const MAX_FEEDBACK: f64 = 0.95;

/// The arithmetic-geometric mean of two positive numbers.
fn agm(mut a: f64, mut b: f64) -> f64 {
    for _ in 0..64 {
        if (a - b).abs() <= 1e-16 * a {
            break;
        }
        (a, b) = (0.5 * (a + b), (a * b).sqrt());
    }
    a
}

/// The coefficients of a pair of all-pass chains whose outputs are 90
/// degrees apart, designed as the polyphase branches of an elliptic
/// half-band low-pass, moved up by a quarter of the sample rate.
///
/// A half-band filter `(A0(z²) + z⁻¹ A1(z²)) / 2`, with `A0` and `A1`
/// chains of first-order all-pass sections in `z²`, passes the lower half
/// of the band because there its two branches are in phase, and stops the
/// upper half because there they are opposite. Moving its response up by a
/// quarter of the sample rate (`z⁻¹ → j z⁻¹`) makes it pass positive
/// frequencies and stop negative ones: its branches become `A0(-z²)` and
/// `j z⁻¹ A1(-z²)`, two real filters whose outputs are 90 degrees apart
/// wherever the half-band filter is flat.
///
/// The section coefficients are those of the elliptic half-band design of
/// R. A. Valenzuela and A. G. Constantinides, "Digital signal processing
/// schemes for efficient interpolation and decimation", IEE Proceedings G,
/// 1983: from the selectivity `k = tan²(π (1 - 2 t) / 4)` of a transition
/// band `t` wide (as a fraction of the sample rate), the nome `q` of the
/// elliptic functions, then each pole by its theta-function series. The
/// stop band of such a design is down by `2 q^(N/4)`, `N` its order, to
/// within a few hundredths of a decibel as measured here, which is how the
/// number of sections is chosen.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HilbertDesign {
    coefficients: [f64; MAX_SECTIONS],
    sections: usize,
}

impl HilbertDesign {
    /// The design for `sample_rate`, accurate from 20 Hz to as far below
    /// half the sample rate.
    pub(crate) fn new(sample_rate: f64) -> Self {
        let transition = limit(2.0 * EDGE_HZ / sample_rate, 1e-6, 0.25);
        let k = ((1.0 - 2.0 * transition) * PI / 4.0).tan().powi(2);
        let k_prime = (1.0 - k * k).sqrt();
        // The nome exp(-π K(k') / K(k)), with each complete elliptic
        // integral from the arithmetic-geometric mean: K(k) = π / (2
        // agm(1, k')). The usual truncated power series in
        // (1 - √k') / (1 + √k') is out by 2e-7 here, which is enough to
        // hold the image at about 95 dB however many sections there are.
        let q = (-PI * agm(1.0, k_prime) / agm(1.0, k)).exp();

        // The fewest sections that take the image down far enough.
        let wanted = 10f64.powf(-IMAGE_DB / 20.0);
        let mut sections = MIN_SECTIONS;
        while sections < MAX_SECTIONS && 2.0 * q.powf((2 * sections + 1) as f64 / 4.0) > wanted {
            sections += 1;
        }

        let order = (2 * sections + 1) as f64;
        let mut coefficients = [0.0; MAX_SECTIONS];
        for (index, coefficient) in coefficients[..sections].iter_mut().enumerate() {
            let i = (index + 1) as f64;
            let mut numerator = 0.0;
            let mut denominator = 0.5;
            let mut sign = 1.0;
            for m in 0..16 {
                let m = f64::from(m);
                numerator +=
                    sign * q.powf(m * (m + 1.0)) * ((2.0 * m + 1.0) * PI * i / order).sin();
                if m > 0.0 {
                    denominator += sign * q.powf(m * m) * (2.0 * m * PI * i / order).cos();
                }
                sign = -sign;
            }
            let w = q.powf(0.25) * numerator / denominator;
            let w2 = w * w;
            let x = ((1.0 - w2 * k) * (1.0 - w2 / k)).sqrt() / (1.0 + w2);
            *coefficient = (1.0 - x) / (1.0 + x);
        }
        Self {
            coefficients,
            sections,
        }
    }

    /// How many sections the two chains have between them.
    pub(crate) fn sections(&self) -> usize {
        self.sections
    }
}

/// The memory of the two chains for one channel.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HilbertState {
    /// For each section, its last two inputs and its last two outputs.
    memory: [[f64; 4]; MAX_SECTIONS],
    /// The sample before, for the `z⁻¹` of the second branch.
    previous: f64,
}

impl HilbertState {
    pub(crate) const SILENT: Self = Self {
        memory: [[0.0; 4]; MAX_SECTIONS],
        previous: 0.0,
    };

    /// One sample through both chains: the in-phase and the quadrature
    /// output, the second 90 degrees behind the first for positive
    /// frequencies.
    #[inline]
    pub(crate) fn step(&mut self, design: &HilbertDesign, x: f64) -> (f64, f64) {
        let mut in_phase = x;
        let mut quadrature = self.previous;
        self.previous = x;
        let pairs = self.memory[..design.sections]
            .iter_mut()
            .zip(&design.coefficients[..design.sections]);
        for (index, (memory, &c)) in pairs.enumerate() {
            // (c - z⁻²) / (1 - c z⁻²): y = c (x + y₂) - x₂.
            let signal = if index % 2 == 0 {
                &mut in_phase
            } else {
                &mut quadrature
            };
            let [x1, x2, y1, y2] = *memory;
            let y = c * (*signal + y2) - x2;
            *memory = [*signal, x1, y, y1];
            *signal = y;
        }
        (in_phase, quadrature)
    }
}

/// A frequency shifter: every partial moved by the same number of hertz.
///
/// Unlike a pitch shift, which multiplies frequencies, a shift adds to them,
/// so harmonics stop being multiples of one another: small shifts make a
/// slow beating or a phasing sweep, large ones bells and ring-modulator
/// clangour, with only one sideband instead of two.
///
/// The method is the single-sideband modulator of the Bode frequency
/// shifter. A pair of all-pass chains turns the input into two signals 90
/// degrees apart, `I` and `Q`, the real and imaginary parts of the analytic
/// signal (a Hilbert transform; see the design below). Multiplied by a
/// cosine and a sine of the shift, `I cos φ - Q sin φ` moves every
/// frequency up by the shift and `I cos φ + Q sin φ` down by it.
///
/// - A positive shift moves up, a negative one down; a shift down past 0 Hz
///   comes back up the other side, as it does in any frequency shifter.
/// - [`process_sample`](Self::process_sample) gives both sidebands at once.
/// - The output is `dry * (1 - mix) + shifted * mix`. Feedback returns the
///   shifted signal to the input a sample later, so that each trip round
///   the loop shifts it again: a rising or falling spiral, the
///   "barber-pole" sound.
///
/// The shift glides over 20 ms in a straight line, keeping the phase, and
/// so do the feedback and the mix. All setters are real-time safe.
///
/// # The 90-degree network
///
/// The two chains are the branches of an elliptic half-band low-pass moved
/// up by a quarter of the sample rate, with coefficients from the elliptic
/// half-band design of Valenzuela and Constantinides (1983). Their outputs
/// are 90 degrees apart from 20 Hz to as far below half the sample rate,
/// closely enough that the image, the sideband that should be gone, is
/// down by 90 dB or more there: 16 sections at 44.1 and 48 kHz, 18 at
/// 96 kHz and 20 at 192 kHz.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{FrequencyShifter, ProcessSpec, Processor};
///
/// let mut shifter = FrequencyShifter::<f32>::new().with_shift_hz(100.0);
/// shifter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 9_600, num_channels: 1 });
///
/// // A 1 kHz tone comes out at 1.1 kHz, at the same level once the
/// // network has settled.
/// let mut block = AudioBuffer::<f32>::new(1, 9_600);
/// for (n, sample) in block.channel_mut(0).iter_mut().enumerate() {
///     *sample = (std::f32::consts::TAU * 1_000.0 * n as f32 / 48_000.0).sin();
/// }
/// shifter.process(&mut block);
/// let peak = block.channel(0)[4_800..].iter().fold(0.0f32, |p, s| p.max(s.abs()));
/// assert!((peak - 1.0).abs() < 1e-3);
/// ```
///
/// # Limits
///
/// The all-pass chains change the phase of the dry path too, so even at a
/// shift of 0 Hz the output is not the input sample for sample, only at the
/// same level at every frequency. Below 20 Hz the image is less far down:
/// about 52 dB at 15 Hz and 32 dB at 10 Hz at 48 kHz. The shift is limited
/// to half the sample rate either way, and the feedback to `-0.95..=0.95`.
#[derive(Debug, Clone)]
pub struct FrequencyShifter<S: Sample = f32> {
    shift_hz: f64,
    feedback: f64,
    mix: f64,
    sample_rate: f64,
    design: HilbertDesign,
    motion: Motion,
    channels: Vec<Channel>,
    marker: core::marker::PhantomData<S>,
}

/// The memory of one channel.
#[derive(Debug, Clone, Copy)]
struct Channel {
    network: HilbertState,
    /// The last output, for the feedback.
    last: f64,
}

/// The oscillator and the gliding values.
#[derive(Debug, Clone, Copy)]
struct Motion {
    /// Position in the shifting oscillator's cycle, from 0 up to but not
    /// including 1.
    phase: f64,
    shift: Smoothed<f64>,
    feedback: Smoothed<f64>,
    mix: Smoothed<f64>,
}

impl Motion {
    /// The phase, the feedback and the mix for one sample; moves on by one.
    #[inline]
    fn step(&mut self, sample_rate: f64) -> (f64, f64, f64) {
        let values = (
            self.phase,
            self.feedback.next_value(),
            self.mix.next_value(),
        );
        self.phase += self.shift.next_value() / sample_rate;
        self.phase -= self.phase.floor();
        values
    }
}

impl<S: Sample> Default for FrequencyShifter<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> FrequencyShifter<S> {
    /// Creates a frequency shifter moving up by 10 Hz, without feedback and
    /// fully wet.
    pub fn new() -> Self {
        Self {
            shift_hz: 10.0,
            feedback: 0.0,
            mix: 1.0,
            sample_rate: 44_100.0,
            design: HilbertDesign::new(44_100.0),
            motion: Motion {
                phase: 0.0,
                shift: Smoothed::new(10.0),
                feedback: Smoothed::new(0.0),
                mix: Smoothed::new(1.0),
            },
            channels: Vec::new(),
            marker: core::marker::PhantomData,
        }
    }

    /// Sets the shift in hertz: up when positive, down when negative,
    /// limited to half the sample rate either way.
    pub fn set_shift_hz(&mut self, hertz: f64) {
        let most = 0.5 * self.sample_rate;
        self.shift_hz = if hertz.is_nan() {
            0.0
        } else {
            limit(hertz, -most, most)
        };
        glide_to(&mut self.motion.shift, self.shift_hz);
    }

    /// Sets how much of the output is shifted again, limited to
    /// `-0.95..=0.95`.
    pub fn set_feedback(&mut self, feedback: f64) {
        self.feedback = if feedback.is_nan() {
            0.0
        } else {
            limit(feedback, -MAX_FEEDBACK, MAX_FEEDBACK)
        };
        glide_to(&mut self.motion.feedback, self.feedback);
    }

    /// Sets the balance between the untouched signal (`0.0`) and the
    /// shifted one (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.motion.mix, self.mix);
    }

    /// Builder form of [`set_shift_hz`](Self::set_shift_hz).
    #[must_use]
    pub fn with_shift_hz(mut self, hertz: f64) -> Self {
        self.set_shift_hz(hertz);
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

    /// The shift as last set, in hertz.
    pub fn shift_hz(&self) -> f64 {
        self.shift_hz
    }

    /// How many first-order sections the 90-degree network has at the
    /// sample rate it was prepared for.
    pub fn network_sections(&self) -> usize {
        self.design.sections()
    }

    /// Shifts one sample of one channel both ways, with neither feedback
    /// nor mix: moved by the shift, and moved by as much the other way (the
    /// upper and the lower sideband, for a positive shift). Real-time
    /// safe.
    ///
    /// The oscillator moves on by one sample on every call, whatever the
    /// channel: call it for one channel, or for each channel of a frame
    /// with `advance` true only on the last. A channel the shifter was not
    /// prepared for gives silence.
    #[inline]
    pub fn process_sample(&mut self, channel: usize, input: S, advance: bool) -> (S, S) {
        let phase = self.motion.phase;
        if advance {
            self.motion.step(self.sample_rate);
        }
        let Some(state) = self.channels.get_mut(channel) else {
            return (S::ZERO, S::ZERO);
        };
        let (i, q) = state.network.step(&self.design, input.to_f64());
        let (sin, cos) = (TAU * phase).sin_cos();
        (
            S::from_f64(i * cos - q * sin),
            S::from_f64(i * cos + q * sin),
        )
    }
}

impl<S: Sample> Processor<S> for FrequencyShifter<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.design = HilbertDesign::new(spec.sample_rate);
        self.channels = vec![
            Channel {
                network: HilbertState::SILENT,
                last: 0.0,
            };
            spec.num_channels
        ];
        let most = 0.5 * spec.sample_rate;
        self.shift_hz = limit(self.shift_hz, -most, most);
        let motion = &mut self.motion;
        for value in [&mut motion.shift, &mut motion.feedback, &mut motion.mix] {
            value.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        }
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let rate = self.sample_rate;
        let start = self.motion;
        let mut after = start;
        for (samples, state) in buffer.iter_channels_mut().zip(&mut self.channels) {
            let mut motion = start;
            for sample in samples {
                let (phase, feedback, mix) = motion.step(rate);
                let input = sample.to_f64();
                let (i, q) = state
                    .network
                    .step(&self.design, input + feedback * state.last);
                let (sin, cos) = (TAU * phase).sin_cos();
                // The real part of (I + jQ) e^(jφ): every frequency moved
                // by the shift, whichever its sign.
                let shifted = i * cos - q * sin;
                state.last = shifted;
                *sample = S::from_f64(input * (1.0 - mix) + shifted * mix);
            }
            after = motion;
        }
        if self.channels.is_empty() || buffer.num_channels() == 0 {
            // No channel to process, but time still passes.
            for _ in 0..buffer.num_frames() {
                after.step(rate);
            }
        }
        self.motion = after;
    }

    fn reset(&mut self) {
        for channel in &mut self.channels {
            channel.network = HilbertState::SILENT;
            channel.last = 0.0;
        }
        self.motion.phase = 0.0;
        self.motion.shift.set_immediate(self.shift_hz);
        self.motion.feedback.set_immediate(self.feedback);
        self.motion.mix.set_immediate(self.mix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fft::{Complex, RealFft};
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, largest_jump, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn prepared(shifter: FrequencyShifter<f64>, channels: usize) -> FrequencyShifter<f64> {
        let mut shifter = shifter;
        shifter.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        shifter
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

    /// The response of the network to a frequency, worked out from its
    /// coefficients: how far the wanted sideband is above the image.
    fn image_rejection_db(design: &HilbertDesign, frequency: f64, sample_rate: f64) -> f64 {
        let omega = TAU * frequency / sample_rate;
        // z⁻² with its sign turned: -e^(-2jω).
        let z2 = Complex::<f64>::from_polar(1.0, -2.0 * omega).scale(-1.0);
        let one = Complex::new(1.0, 0.0);
        let divide = |a: Complex<f64>, b: Complex<f64>| {
            let scale = 1.0 / b.norm_sqr();
            (a * b.conj()).scale(scale)
        };
        let (mut a0, mut a1) = (one, Complex::from_polar(1.0, -omega));
        for (index, &c) in design.coefficients[..design.sections].iter().enumerate() {
            let section = divide(Complex::new(c, 0.0) + z2, one + z2.scale(c));
            if index % 2 == 0 {
                a0 = a0 * section;
            } else {
                a1 = a1 * section;
            }
        }
        let j = Complex::new(0.0, 1.0);
        db((a0 + j * a1).norm() / (a0 - j * a1).norm())
    }

    #[test]
    fn the_network_holds_the_image_down_across_the_band() {
        for (rate, sections) in [
            (44_100.0, 16),
            (48_000.0, 16),
            (96_000.0, 18),
            (192_000.0, 20),
        ] {
            let design = HilbertDesign::new(rate);
            assert_eq!(design.sections(), sections, "at {rate} Hz");
            // Every coefficient makes a stable section.
            assert!(
                design.coefficients[..sections]
                    .iter()
                    .all(|c| (0.0..1.0).contains(c))
            );
            let mut worst = f64::MAX;
            let mut frequency: f64 = 20.0;
            while frequency <= rate / 2.0 - 20.0 {
                worst = worst.min(image_rejection_db(&design, frequency, rate));
                frequency *= 1.002;
            }
            assert!(worst > 90.0, "at {rate} Hz: {worst} dB");
        }
        // Below the edge it gets worse, as documented.
        let design = HilbertDesign::new(RATE);
        assert!((image_rejection_db(&design, 15.0, RATE) - 52.1).abs() < 0.5);
        assert!((image_rejection_db(&design, 10.0, RATE) - 32.2).abs() < 0.5);
    }

    #[test]
    fn a_tone_moves_by_the_shift_with_its_image_far_down() {
        // The last 24 000 frames: bin `k` is `2k` hertz, every frequency here
        // falls on a bin, and the network has long since settled.
        const LEN: usize = 24_000;
        for (tone, shift) in [
            (1_000.0, 300.0),
            (1_000.0, -300.0),
            (100.0, 50.0),
            (15_000.0, -2_000.0),
            (440.0, 7_000.0),
            (300.0, -1_000.0),
        ] {
            let mut shifter = prepared(FrequencyShifter::new().with_shift_hz(shift), 1);
            let mut signal = sine::<f64>(1, 2 * LEN, tone, RATE, 0.5);
            run_in_blocks(&mut shifter, &mut signal, 512);
            let bins = spectrum(signal.channel(0), LEN);
            // A shift down past 0 Hz comes back up the other side.
            let moved = ((tone + shift) / 2.0f64).abs() as usize;
            let image = ((tone - shift) / 2.0f64).abs() as usize;
            assert!(
                (bins[moved] - 0.5).abs() < 1e-4,
                "{tone} Hz by {shift} Hz: {}",
                bins[moved]
            );
            let rejection = db(bins[moved] / bins[image]);
            assert!(
                rejection > 90.0,
                "{tone} Hz by {shift} Hz: the image is {rejection} dB down"
            );
            // And nothing else: the tone itself is gone.
            for (bin, &level) in bins.iter().enumerate() {
                if bin != moved && bin != image {
                    assert!(
                        level < 1e-7,
                        "{tone} Hz by {shift} Hz: bin {bin} at {level}"
                    );
                }
            }
        }
    }

    #[test]
    fn both_sidebands_come_out_of_process_sample() {
        const LEN: usize = 24_000;
        let mut shifter = prepared(FrequencyShifter::new().with_shift_hz(250.0), 1);
        let tone = sine::<f64>(1, 2 * LEN, 2_000.0, RATE, 0.5);
        let (mut up, mut down) = (vec![0.0; 2 * LEN], vec![0.0; 2 * LEN]);
        for (n, &x) in tone.channel(0).iter().enumerate() {
            (up[n], down[n]) = shifter.process_sample(0, x, true);
        }
        let (up, down) = (spectrum(&up, LEN), spectrum(&down, LEN));
        assert!((up[1_125] - 0.5).abs() < 1e-4 && up[875] < 0.5e-4);
        assert!((down[875] - 0.5).abs() < 1e-4 && down[1_125] < 0.5e-4);
        assert_eq!(shifter.process_sample(5, 1.0, false), (0.0, 0.0));
    }

    #[test]
    fn without_a_shift_every_frequency_keeps_its_level() {
        const LEN: usize = 1 << 16;
        let mut shifter = prepared(FrequencyShifter::new().with_shift_hz(0.0), 1);
        let mut impulse = AudioBuffer::<f64>::new(1, LEN);
        impulse.channel_mut(0)[0] = 1.0;
        run_in_blocks(&mut shifter, &mut impulse, 512);
        let mut fft = RealFft::<f64>::new(LEN);
        let mut bins = vec![Complex::ZERO; fft.spectrum_len()];
        fft.forward(impulse.channel(0), &mut bins);
        // From 20 Hz up: below it the slowest sections ring for longer than
        // the impulse response is measured.
        for (bin, value) in bins.iter().enumerate().skip(LEN * 20 / 48_000) {
            let level = value.norm();
            assert!((level - 1.0).abs() < 1e-4, "bin {bin}: {level}");
        }
    }

    #[test]
    fn feedback_shifts_again_on_every_trip_and_stays_bounded() {
        // With feedback, a tone gathers copies shifted once, twice, ...
        const LEN: usize = 24_000;
        let mut shifter = prepared(
            FrequencyShifter::new()
                .with_shift_hz(100.0)
                .with_feedback(0.5),
            1,
        );
        let mut signal = sine::<f64>(1, 2 * LEN, 1_000.0, RATE, 0.5);
        run_in_blocks(&mut shifter, &mut signal, 512);
        let bins = spectrum(signal.channel(0), LEN);
        // At 1.1, 1.2 and 1.3 kHz: each trip turns the level down by half.
        let (once, twice, thrice) = (bins[550], bins[600], bins[650]);
        assert!((once - 0.5).abs() < 0.01, "{once}");
        assert!((twice / once - 0.5).abs() < 0.01, "{twice}");
        assert!((thrice / twice - 0.5).abs() < 0.01, "{thrice}");

        for feedback in [0.95, -0.95] {
            let mut shifter = prepared(
                FrequencyShifter::new()
                    .with_shift_hz(-3.0)
                    .with_feedback(feedback),
                2,
            );
            let mut signal = noise::<f64>(2, 48_000, 6);
            run_in_blocks(&mut shifter, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 40.0, "feedback {feedback}: {peak}");
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size_and_changes_do_not_click() {
        let render = |block: usize, feedback: f64| {
            let mut shifter = prepared(FrequencyShifter::new().with_shift_hz(5.0), 2);
            let mut signal = sine::<f64>(2, 9_000, 220.0, RATE, 0.5);
            let mut start = 0;
            for (end, shift) in [(3_000, -20.0), (6_000, 40.0), (9_000, 0.0)] {
                signal.windowed(start..end, |part| run_in_blocks(&mut shifter, part, block));
                shifter.set_shift_hz(shift);
                shifter.set_feedback(feedback * shift / 100.0);
                shifter.set_mix(shift.abs() / 40.0);
                start = end;
            }
            signal
        };
        let reference = render(512, 1.0);
        for block in [1, 7, 64, 333, 9_000] {
            assert!(render(block, 1.0) == reference, "blocks of {block}");
        }
        // The network starts from silence: the first samples rise as fast
        // as the tone does. After that a 220 Hz tone, shifted by a little,
        // moves by little more than it does, 0.0144 per sample.
        for channel in render(512, 0.0).iter_channels() {
            let jump = largest_jump(&channel[100..]);
            assert!(jump < 0.02, "a jump of {jump}");
        }
    }

    #[test]
    fn it_survives_garbage_and_resets() {
        let mut shifter = prepared(FrequencyShifter::new(), 2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            shifter.set_shift_hz(garbage);
            shifter.set_feedback(garbage);
            shifter.set_mix(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut shifter, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 40.0, "after {garbage}: {peak}");
        }
        assert_eq!(shifter.shift_hz(), 24_000.0);

        let mut shifter = prepared(FrequencyShifter::new().with_feedback(0.7), 2);
        let mut expected = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut shifter, &mut expected, 512);
        let mut other = noise::<f64>(2, 1_234, 5);
        run_in_blocks(&mut shifter, &mut other, 512);
        shifter.reset();
        let mut again = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut shifter, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut shifter = FrequencyShifter::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        shifter.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        shifter.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        assert_eq!(shifter.network_sections(), 16);
        shifter.process(&mut buffer);
        assert!(buffer.channel(0) != [0.25; 64]);
        assert_eq!(Processor::<f32>::latency_samples(&shifter), 0);
        shifter.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
