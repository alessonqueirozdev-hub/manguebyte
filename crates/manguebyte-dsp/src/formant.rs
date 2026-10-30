use core::f64::consts::PI;

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::dynamics::to_gain;
use crate::numeric::{glide_to, limit};
use crate::{ProcessSpec, Processor};

/// How many formants each vowel has here.
const FORMANTS: usize = 3;

/// The vowels, in the order of the morph: A, E, I, O, U.
///
/// The frequencies, in hertz, are the averages for adult men measured by
/// G. E. Peterson and H. L. Barney, "Control methods used in a study of the
/// vowels", Journal of the Acoustical Society of America 24 (1952), table
/// II, for the vowels of "father" (/ɑ/), "bet" (/ɛ/), "beet" (/i/),
/// "bought" (/ɔ/) and "boot" (/u/). The bandwidths and the levels are this
/// crate's own choice, not from that table: bandwidths that widen with the
/// formant, as measured vowels' do, and the second and third formants 6
/// and 12 dB below the first, a rough average of the levels a voice's
/// falling spectrum gives them.
const VOWELS: [[f64; FORMANTS]; 5] = [
    [730.0, 1_090.0, 2_440.0],
    [530.0, 1_840.0, 2_480.0],
    [270.0, 2_290.0, 3_010.0],
    [570.0, 840.0, 2_410.0],
    [300.0, 870.0, 2_240.0],
];
const BANDWIDTHS_HZ: [f64; FORMANTS] = [80.0, 100.0, 140.0];
const LEVELS_DB: [f64; FORMANTS] = [0.0, -6.0, -12.0];

/// The highest a formant may sit, as a fraction of the sample rate.
const HIGHEST_RATIO: f64 = 0.45;
const MAX_SHIFT_SEMITONES: f64 = 12.0;
const MAX_GAIN_DB: f64 = 24.0;

/// How long a change of vowel, shift, gain or mix takes.
const GLIDE_SECONDS: f64 = 0.02;

/// What one step of a band-pass state-variable section needs.
#[derive(Debug, Clone, Copy)]
struct Resonance {
    a1: f64,
    a2: f64,
    a3: f64,
    /// The damping, which also scales the band output to unity at the
    /// centre.
    k: f64,
}

impl Resonance {
    fn new(frequency: f64, bandwidth: f64, sample_rate: f64) -> Self {
        let frequency = limit(frequency, 10.0, HIGHEST_RATIO * sample_rate);
        let g = (PI * frequency / sample_rate).tan();
        let k = limit(bandwidth / frequency, 1e-3, 10.0);
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        Self {
            a1,
            a2,
            a3: g * a2,
            k,
        }
    }

    /// One sample, with unity gain at the centre: the trapezoidal
    /// state-variable filter (V. Zavalishin, "The Art of VA Filter
    /// Design", chapter 4).
    #[inline]
    fn step(&self, state: &mut [f64; 2], x: f64) -> f64 {
        let v3 = x - state[1];
        let v1 = self.a1 * state[0] + self.a2 * v3;
        let v2 = state[1] + self.a2 * state[0] + self.a3 * v3;
        state[0] = 2.0 * v1 - state[0];
        state[1] = 2.0 * v2 - state[1];
        self.k * v1
    }
}

/// The formant frequencies at a place on the morph from A (0) to U (4),
/// moved by `shift` (a frequency ratio): each glides from one vowel to the
/// next evenly in pitch.
fn formants_at(vowel: f64, shift: f64) -> [f64; FORMANTS] {
    let place = limit(vowel, 0.0, 4.0);
    let below = (place.floor() as usize).min(3);
    let t = place - below as f64;
    let mut out = [0.0; FORMANTS];
    for (index, frequency) in out.iter_mut().enumerate() {
        let (a, b) = (VOWELS[below][index], VOWELS[below + 1][index]);
        // The vowels themselves are spelled out, so that they are exact.
        let between = if t == 0.0 {
            a
        } else if t == 1.0 {
            b
        } else {
            a * (b / a).powf(t)
        };
        *frequency = between * shift;
    }
    out
}

/// A formant filter: the resonances of a vowel, laid over any sound, and
/// morphed from one vowel to the next: A, E, I, O, U.
///
/// The input goes through three band-pass resonances side by side, one for
/// each of the vowel's first three formants, each a second-order
/// state-variable filter with unity gain at its centre, and they are added
/// at the vowel's levels. A buzzy source (a sawtooth, a pulse) through it
/// speaks; a drum loop through it talks.
///
/// - The vowel is a place from `0.0` (A) to `4.0` (U), and between two
///   vowels each formant glides from one to the other evenly in pitch, so
///   `0.5` is half-way from A to E. The formant frequencies are averages
///   measured for men's voices (Peterson and Barney, 1952; see the table
///   in the source); the bandwidths and levels are this crate's own.
/// - **Shift** moves every formant by up to an octave either way: up for a
///   smaller, brighter voice.
/// - The output is `dry * (1 - mix) + vowel * gain * mix`.
///
/// The vowel, the shift, the gain and the mix glide over 20 ms, and the
/// filters are retuned on every sample of a glide; the state-variable form
/// stays stable however fast they move. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{FormantFilter, ProcessSpec, Processor};
///
/// let mut filter = FormantFilter::<f32>::new().with_vowel(2.0); // I
/// filter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 1 });
/// assert_eq!(filter.formants_hz(), [270.0, 2_290.0, 3_010.0]);
///
/// let mut block = AudioBuffer::<f32>::new(1, 256);
/// block.channel_mut(0)[0] = 1.0;
/// filter.process(&mut block);
/// ```
///
/// # Limits
///
/// Three formants, from one table of men's averages; women's and children's
/// formants lie higher, which the shift approximates. Where two formants
/// lie close (O, U) their skirts add and the peaks lean towards each other
/// a little. No formant sits above 0.45 of the sample rate. The vowel is
/// limited to `0.0..=4.0`, the shift to ±12 semitones and the gain to
/// ±24 dB.
#[derive(Debug, Clone)]
pub struct FormantFilter<S: Sample = f32> {
    vowel: f64,
    shift_semitones: f64,
    gain_db: f64,
    mix: f64,
    sample_rate: f64,
    vowel_now: Smoothed<f64>,
    shift_now: Smoothed<f64>,
    gain_now: Smoothed<f64>,
    mix_now: Smoothed<f64>,
    resonances: [Resonance; FORMANTS],
    states: Vec<[[f64; 2]; FORMANTS]>,
    marker: core::marker::PhantomData<S>,
}

impl<S: Sample> Default for FormantFilter<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> FormantFilter<S> {
    /// The frequencies of the first three formants of A, E, I, O and U, in
    /// hertz, as the filter uses them.
    pub const VOWELS: [[f64; 3]; 5] = VOWELS;

    /// Creates a formant filter on the vowel A, unshifted, at 0 dB and
    /// fully wet.
    pub fn new() -> Self {
        let mut filter = Self {
            vowel: 0.0,
            shift_semitones: 0.0,
            gain_db: 0.0,
            mix: 1.0,
            sample_rate: 44_100.0,
            vowel_now: Smoothed::new(0.0),
            shift_now: Smoothed::new(0.0),
            gain_now: Smoothed::new(1.0),
            mix_now: Smoothed::new(1.0),
            resonances: [Resonance::new(1_000.0, 100.0, 44_100.0); FORMANTS],
            states: Vec::new(),
            marker: core::marker::PhantomData,
        };
        filter.retune(0.0, 0.0);
        filter
    }

    /// Sets the vowel: `0.0` is A, `1.0` E, `2.0` I, `3.0` O and `4.0` U,
    /// and a place between two morphs between them.
    pub fn set_vowel(&mut self, vowel: f64) {
        self.vowel = limit(vowel, 0.0, 4.0);
        glide_to(&mut self.vowel_now, self.vowel);
    }

    /// Moves every formant by `semitones`, limited to ±12.
    pub fn set_shift(&mut self, semitones: f64) {
        self.shift_semitones = if semitones.is_nan() {
            0.0
        } else {
            limit(semitones, -MAX_SHIFT_SEMITONES, MAX_SHIFT_SEMITONES)
        };
        glide_to(&mut self.shift_now, self.shift_semitones);
    }

    /// Sets the gain of the filtered signal, in decibels, limited to
    /// ±24 dB.
    pub fn set_gain_db(&mut self, db: f64) {
        self.gain_db = if db.is_nan() {
            0.0
        } else {
            limit(db, -MAX_GAIN_DB, MAX_GAIN_DB)
        };
        glide_to(&mut self.gain_now, to_gain(self.gain_db));
    }

    /// Sets the balance between the untouched signal (`0.0`) and the
    /// filtered one (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.mix_now, self.mix);
    }

    /// Builder form of [`set_vowel`](Self::set_vowel).
    #[must_use]
    pub fn with_vowel(mut self, vowel: f64) -> Self {
        self.set_vowel(vowel);
        self
    }

    /// Builder form of [`set_shift`](Self::set_shift).
    #[must_use]
    pub fn with_shift(mut self, semitones: f64) -> Self {
        self.set_shift(semitones);
        self
    }

    /// Builder form of [`set_gain_db`](Self::set_gain_db).
    #[must_use]
    pub fn with_gain_db(mut self, db: f64) -> Self {
        self.set_gain_db(db);
        self
    }

    /// Builder form of [`set_mix`](Self::set_mix).
    #[must_use]
    pub fn with_mix(mut self, mix: f64) -> Self {
        self.set_mix(mix);
        self
    }

    /// The formant frequencies of the vowel and shift as last set, in
    /// hertz.
    pub fn formants_hz(&self) -> [f64; 3] {
        formants_at(self.vowel, (self.shift_semitones / 12.0).exp2())
    }

    fn retune(&mut self, vowel: f64, shift_semitones: f64) {
        let frequencies = formants_at(vowel, (shift_semitones / 12.0).exp2());
        for ((resonance, &frequency), &bandwidth) in self
            .resonances
            .iter_mut()
            .zip(&frequencies)
            .zip(&BANDWIDTHS_HZ)
        {
            *resonance = Resonance::new(frequency, bandwidth, self.sample_rate);
        }
    }
}

impl<S: Sample> Processor<S> for FormantFilter<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.states = vec![[[0.0; 2]; FORMANTS]; spec.num_channels];
        for value in [
            &mut self.vowel_now,
            &mut self.shift_now,
            &mut self.gain_now,
            &mut self.mix_now,
        ] {
            value.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        }
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let levels = LEVELS_DB.map(to_gain);
        let channels = buffer.num_channels().min(self.states.len());
        for frame in 0..buffer.num_frames() {
            if self.vowel_now.is_smoothing() || self.shift_now.is_smoothing() {
                let (vowel, shift) = (self.vowel_now.next_value(), self.shift_now.next_value());
                self.retune(vowel, shift);
            }
            let gain = self.gain_now.next_value();
            let mix = self.mix_now.next_value();
            for (channel, state) in self.states.iter_mut().enumerate().take(channels) {
                let sample = &mut buffer.channel_mut(channel)[frame];
                let input = sample.to_f64();
                let mut vowel = 0.0;
                for ((resonance, memory), &level) in
                    self.resonances.iter().zip(state.iter_mut()).zip(&levels)
                {
                    vowel += resonance.step(memory, input) * level;
                }
                *sample = S::from_f64(input * (1.0 - mix) + vowel * gain * mix);
            }
        }
    }

    fn reset(&mut self) {
        self.states.fill([[0.0; 2]; FORMANTS]);
        self.vowel_now.set_immediate(self.vowel);
        self.shift_now.set_immediate(self.shift_semitones);
        self.gain_now.set_immediate(to_gain(self.gain_db));
        self.mix_now.set_immediate(self.mix);
        self.retune(self.vowel, self.shift_semitones);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, gain_at, largest_jump, noise, run_in_blocks, sine, tail_peak};

    const RATE: f64 = 48_000.0;

    fn prepared(filter: FormantFilter<f64>, channels: usize) -> FormantFilter<f64> {
        let mut filter = filter;
        filter.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        filter
    }

    fn impulse_response(filter: &mut FormantFilter<f64>) -> Vec<f64> {
        let mut impulse = AudioBuffer::<f64>::new(1, 1 << 15);
        impulse.channel_mut(0)[0] = 1.0;
        run_in_blocks(filter, &mut impulse, 512);
        let response = impulse.channel(0).to_vec();
        assert!(tail_peak(&response, 256) < 1e-12, "the response is cut off");
        response
    }

    /// The frequency of the highest point of the response within 20% of
    /// `near`, found on a grid of a thousandth of an octave.
    fn peak_near(response: &[f64], near: f64) -> (f64, f64) {
        let mut best = (near, 0.0);
        let mut frequency = near * 0.8;
        while frequency <= near * 1.2 {
            let level = gain_at(response, frequency, RATE);
            if level > best.1 {
                best = (frequency, level);
            }
            frequency *= 2f64.powf(0.001);
        }
        best
    }

    #[test]
    fn each_vowel_peaks_at_its_formants() {
        for (index, formants) in VOWELS.iter().enumerate() {
            let mut filter = prepared(FormantFilter::new().with_vowel(index as f64), 1);
            assert_eq!(filter.formants_hz(), *formants);
            let response = impulse_response(&mut filter);
            for (formant, (&frequency, &level)) in formants.iter().zip(&LEVELS_DB).enumerate() {
                let (found, height) = peak_near(&response, frequency);
                // Where two formants lie close (O and U), the skirt of each
                // leans the other's peak towards it by up to 4%.
                let close = formants
                    .iter()
                    .any(|&other| other != frequency && (other / frequency).log2().abs() < 1.0);
                let tolerance = if close { 0.04 } else { 0.01 };
                assert!(
                    (found / frequency - 1.0).abs() < tolerance,
                    "vowel {index}, formant {formant}: a peak at {found} Hz for {frequency} Hz"
                );
                assert!(
                    (db(height) - level).abs() < 1.5,
                    "vowel {index}, formant {formant}: {} dB for {level} dB",
                    db(height)
                );
                // And it is a peak: the response falls away either side.
                assert!(gain_at(&response, found * 1.01, RATE) < height);
                assert!(gain_at(&response, found / 1.01, RATE) < height);
            }
        }
    }

    #[test]
    fn the_morph_and_the_shift_move_the_formants_evenly_in_pitch() {
        let filter = FormantFilter::<f64>::new().with_vowel(0.5);
        let half: Vec<f64> = (0..3)
            .map(|k| (VOWELS[0][k] * VOWELS[1][k]).sqrt())
            .collect();
        for (got, want) in filter.formants_hz().iter().zip(&half) {
            assert!((got - want).abs() < 1e-9);
        }
        let filter = FormantFilter::<f64>::new().with_vowel(3.0).with_shift(12.0);
        for (got, want) in filter.formants_hz().iter().zip(&VOWELS[3]) {
            assert!((got - 2.0 * want).abs() < 1e-9);
        }
        // The shifted filter's peaks are where it says.
        let mut filter = prepared(FormantFilter::new().with_vowel(2.0).with_shift(-7.0), 1);
        let response = impulse_response(&mut filter);
        let low = filter.formants_hz()[0];
        let (found, _) = peak_near(&response, low);
        assert!((found / low - 1.0).abs() < 0.01, "{found} for {low}");
    }

    #[test]
    fn output_does_not_depend_on_block_size_and_a_morph_does_not_click() {
        let render = |block: usize| {
            let mut filter = prepared(FormantFilter::new(), 2);
            let mut signal = sine::<f64>(2, 12_000, 110.0, RATE, 0.5);
            let mut start = 0;
            for (end, vowel) in [(3_000, 4.0), (6_000, 1.5), (9_000, 3.0), (12_000, 0.0)] {
                signal.windowed(start..end, |part| run_in_blocks(&mut filter, part, block));
                filter.set_vowel(vowel);
                filter.set_shift(vowel - 2.0);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 12_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
        // A 110 Hz tone of 0.5 moves by 0.0072 a sample; the resonances
        // raise that by their gain at its harmonics, not by a click.
        for channel in reference.iter_channels() {
            let jump = largest_jump(channel);
            assert!(jump < 0.02, "a jump of {jump}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut filter = prepared(FormantFilter::new().with_gain_db(24.0), 2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            filter.set_vowel(garbage);
            filter.set_shift(garbage);
            filter.set_gain_db(garbage);
            filter.set_mix(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut filter, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak < 100.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_clears_the_resonances() {
        let build = || prepared(FormantFilter::new().with_vowel(1.3), 2);
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
    fn single_precision_and_unprepared_use_work() {
        let mut filter = FormantFilter::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        filter.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        filter.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        filter.process(&mut buffer);
        assert!(buffer.channel(0) != [0.25; 64]);
        assert_eq!(buffer.channel(1), &[0.0; 64]);
        assert_eq!(Processor::<f32>::latency_samples(&filter), 0);
        filter.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
