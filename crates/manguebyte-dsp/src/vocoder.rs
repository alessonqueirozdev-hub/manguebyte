use core::f64::consts::{PI, SQRT_2};

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::dynamics::to_gain;
use crate::numeric::{glide_to, limit};
use crate::{EnvelopeFollower, LevelDetector, ProcessContext, ProcessSpec, Processor};

/// Fewest and most bands.
const MIN_BANDS: usize = 8;
const MAX_BANDS: usize = 32;
/// Limits of the settings.
const MIN_HZ: f64 = 40.0;
const MAX_HZ: f64 = 16_000.0;
const MAX_SHIFT_SEMITONES: f64 = 12.0;
const MAX_GAIN_DB: f64 = 24.0;
/// The highest a band may sit, as a fraction of the sample rate.
const HIGHEST_RATIO: f64 = 0.45;

/// How long a change of gain or mix takes.
const GLIDE_SECONDS: f64 = 0.02;

/// A band-pass state-variable filter section, with unity gain at its
/// centre: what one step needs.
#[derive(Debug, Clone, Copy)]
struct Section {
    a1: f64,
    a2: f64,
    a3: f64,
    k: f64,
}

impl Section {
    const SILENT: [f64; 2] = [0.0; 2];

    fn new(frequency: f64, q: f64, sample_rate: f64) -> Self {
        let frequency = limit(frequency, 1.0, HIGHEST_RATIO * sample_rate);
        let g = (PI * frequency / sample_rate).tan();
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        Self {
            a1,
            a2,
            a3: g * a2,
            k,
        }
    }

    /// One sample through two of these in a row: a fourth-order band-pass.
    ///
    /// The trapezoidal state-variable filter in the topology-preserving
    /// form (V. Zavalishin, "The Art of VA Filter Design", chapter 4).
    #[inline]
    fn twice(&self, memory: &mut [[f64; 2]; 2], x: f64) -> f64 {
        let mut signal = x;
        for state in memory {
            let v3 = signal - state[1];
            let v1 = self.a1 * state[0] + self.a2 * v3;
            let v2 = state[1] + self.a2 * state[0] + self.a3 * v3;
            state[0] = 2.0 * v1 - state[0];
            state[1] = 2.0 * v2 - state[1];
            signal = self.k * v1;
        }
        signal
    }
}

/// The memory of the band-pass filters of one signal.
type Bank = [[[f64; 2]; 2]; MAX_BANDS];
const SILENT_BANK: Bank = [[Section::SILENT; 2]; MAX_BANDS];

/// A channel vocoder: the spectral envelope of one signal, the modulator,
/// imposed on another, the carrier. A voice as the modulator and a synth as
/// the carrier make the synth speak.
///
/// - The modulator is split into 8 to 32 bands, spaced evenly in pitch
///   between a low and a high frequency, each by a fourth-order band-pass
///   (two state-variable sections). An [`EnvelopeFollower`] reads the level
///   of each band, as its RMS level scaled by `√2` so that a sine reads its
///   amplitude.
/// - The carrier goes through a bank of the same filters, and each of its
///   bands is multiplied by the level of the modulator's band; the bands
///   are added up. A sine in the modulator at a band's centre, of amplitude
///   `a`, therefore lets a carrier's sine at that centre through at `a`
///   times its own amplitude.
/// - **Formant shift** moves the carrier's bands up or down against the
///   modulator's, by up to an octave either way, so that a voice sounds
///   smaller or larger without changing the pitch of the carrier.
/// - The output is `carrier * (1 - mix) + vocoded * gain * mix`.
///
/// The carrier is the input. The modulator is the
/// [`sidechain`](ProcessContext::sidechain) of the context given to
/// [`process_with`](Processor::process_with), the average of its channels,
/// from the context's offset; without a sidechain, or through
/// [`process`](Processor::process), the average of the input's own
/// channels, which leaves a filter bank shaping the input by itself.
///
/// The gain and the mix glide over 20 ms; the follower's times change at
/// once. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::EventBuffer;
/// use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport, Vocoder};
///
/// let mut vocoder = Vocoder::<f32>::new().with_bands(16);
/// vocoder.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
///
/// let mut voice = AudioBuffer::<f32>::new(1, 256);
/// let mut synth = AudioBuffer::<f32>::new(2, 256);
/// let events = EventBuffer::new();
/// let context = ProcessContext::new(&events, &Transport::STOPPED).with_sidechain(&voice);
/// // A silent voice silences the synth.
/// synth.channel_mut(0).fill(0.5);
/// vocoder.process_with(&mut synth, &context);
/// assert_eq!(synth.channel(0), &[0.0; 256]);
/// ```
///
/// # Limits
///
/// Neighbouring bands overlap, as a vocoder's must for an even response:
/// with 8 bands a tone at one band's centre reads about 14 dB down in the
/// next, and a carrier's band picks up some of its neighbours' levels. A
/// change of the number of bands, of their range or of the formant shift
/// retunes the filters at once, without a cross-fade; the state-variable
/// filters stay stable however they are retuned. There is no detection of
/// unvoiced sounds and no noise for them: hiss in the voice is only heard
/// as far as the carrier has energy at those frequencies. The range is
/// limited to 40 Hz to 16 kHz, no band sits above 0.45 of the sample rate,
/// the shift is limited to ±12 semitones and the gain to ±24 dB.
#[derive(Debug, Clone)]
pub struct Vocoder<S: Sample = f32> {
    bands: usize,
    low_hz: f64,
    high_hz: f64,
    shift_semitones: f64,
    gain_db: f64,
    mix: f64,
    sample_rate: f64,
    analysis: [Section; MAX_BANDS],
    synthesis: [Section; MAX_BANDS],
    modulator: Bank,
    carriers: Vec<Bank>,
    follower: EnvelopeFollower<f64>,
    levels: [f64; MAX_BANDS],
    gain_now: Smoothed<f64>,
    mix_now: Smoothed<f64>,
    marker: core::marker::PhantomData<S>,
}

impl<S: Sample> Default for Vocoder<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Vocoder<S> {
    /// The fewest bands.
    pub const MIN_BANDS: usize = MIN_BANDS;
    /// The most bands.
    pub const MAX_BANDS: usize = MAX_BANDS;

    /// Creates a vocoder of 16 bands from 100 Hz to 8 kHz, its followers
    /// rising and falling in 10 ms, no formant shift, 0 dB of gain and
    /// fully wet.
    pub fn new() -> Self {
        let section = Section::new(1_000.0, 1.0, 44_100.0);
        let mut vocoder = Self {
            bands: 16,
            low_hz: 100.0,
            high_hz: 8_000.0,
            shift_semitones: 0.0,
            gain_db: 0.0,
            mix: 1.0,
            sample_rate: 44_100.0,
            analysis: [section; MAX_BANDS],
            synthesis: [section; MAX_BANDS],
            modulator: SILENT_BANK,
            carriers: Vec::new(),
            follower: EnvelopeFollower::new()
                .with_detector(LevelDetector::Rms)
                .with_attack_ms(10.0)
                .with_release_ms(10.0),
            levels: [0.0; MAX_BANDS],
            gain_now: Smoothed::new(1.0),
            mix_now: Smoothed::new(1.0),
            marker: core::marker::PhantomData,
        };
        vocoder.tune();
        vocoder
    }

    /// Sets the number of bands, limited to `8..=32`.
    pub fn set_bands(&mut self, bands: usize) {
        let bands = bands.clamp(MIN_BANDS, MAX_BANDS);
        if bands != self.bands {
            self.bands = bands;
            self.tune();
        }
    }

    /// Sets the centres of the lowest and the highest band, in hertz,
    /// limited to 40 Hz to 16 kHz; the highest is kept above the lowest.
    pub fn set_range_hz(&mut self, low: f64, high: f64) {
        self.low_hz = limit(low, MIN_HZ, MAX_HZ / 2.0);
        self.high_hz = limit(high, 2.0 * self.low_hz, MAX_HZ);
        self.tune();
    }

    /// Moves the carrier's bands against the modulator's by `semitones`,
    /// limited to ±12.
    pub fn set_formant_shift(&mut self, semitones: f64) {
        self.shift_semitones = if semitones.is_nan() {
            0.0
        } else {
            limit(semitones, -MAX_SHIFT_SEMITONES, MAX_SHIFT_SEMITONES)
        };
        self.tune();
    }

    /// Sets how fast each band's level follows a rise of the modulator, in
    /// milliseconds.
    pub fn set_attack_ms(&mut self, milliseconds: f64) {
        self.follower
            .set_attack_ms(limit(milliseconds, 0.0, 1_000.0));
    }

    /// Sets how fast each band's level follows a fall of the modulator, in
    /// milliseconds.
    pub fn set_release_ms(&mut self, milliseconds: f64) {
        self.follower
            .set_release_ms(limit(milliseconds, 0.0, 5_000.0));
    }

    /// Sets the gain of the vocoded signal, in decibels, limited to
    /// ±24 dB.
    pub fn set_gain_db(&mut self, db: f64) {
        self.gain_db = if db.is_nan() {
            0.0
        } else {
            limit(db, -MAX_GAIN_DB, MAX_GAIN_DB)
        };
        glide_to(&mut self.gain_now, to_gain(self.gain_db));
    }

    /// Sets the balance between the carrier as it is (`0.0`) and the
    /// vocoded signal (`1.0`).
    pub fn set_mix(&mut self, mix: f64) {
        self.mix = limit(mix, 0.0, 1.0);
        glide_to(&mut self.mix_now, self.mix);
    }

    /// Builder form of [`set_bands`](Self::set_bands).
    #[must_use]
    pub fn with_bands(mut self, bands: usize) -> Self {
        self.set_bands(bands);
        self
    }

    /// Builder form of [`set_range_hz`](Self::set_range_hz).
    #[must_use]
    pub fn with_range_hz(mut self, low: f64, high: f64) -> Self {
        self.set_range_hz(low, high);
        self
    }

    /// Builder form of [`set_formant_shift`](Self::set_formant_shift).
    #[must_use]
    pub fn with_formant_shift(mut self, semitones: f64) -> Self {
        self.set_formant_shift(semitones);
        self
    }

    /// Builder form of [`set_attack_ms`](Self::set_attack_ms).
    #[must_use]
    pub fn with_attack_ms(mut self, milliseconds: f64) -> Self {
        self.set_attack_ms(milliseconds);
        self
    }

    /// Builder form of [`set_release_ms`](Self::set_release_ms).
    #[must_use]
    pub fn with_release_ms(mut self, milliseconds: f64) -> Self {
        self.set_release_ms(milliseconds);
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

    /// The number of bands.
    pub fn bands(&self) -> usize {
        self.bands
    }

    /// The centre of the modulator's band `index`, in hertz, or `None` for
    /// a band the vocoder does not have. The carrier's band is there moved
    /// by the formant shift.
    pub fn band_hz(&self, index: usize) -> Option<f64> {
        (index < self.bands).then(|| self.centre(index))
    }

    /// The level of the modulator in band `index` as last read, as the
    /// amplitude of a sine; zero for a band the vocoder does not have.
    pub fn band_level(&self, index: usize) -> f64 {
        if index < self.bands {
            self.levels[index]
        } else {
            0.0
        }
    }

    fn centre(&self, index: usize) -> f64 {
        let place = index as f64 / (self.bands - 1) as f64;
        self.low_hz * (self.high_hz / self.low_hz).powf(place)
    }

    /// Works out every band's filters.
    fn tune(&mut self) {
        // Neighbouring bands are an equal step apart in pitch, and each
        // section's bandwidth is that step.
        let step = (self.high_hz / self.low_hz).log2() / (self.bands - 1) as f64;
        let ratio = step.exp2();
        let q = ratio.sqrt() / (ratio - 1.0);
        let shift = (self.shift_semitones / 12.0).exp2();
        for index in 0..self.bands {
            let centre = self.centre(index);
            self.analysis[index] = Section::new(centre, q, self.sample_rate);
            self.synthesis[index] = Section::new(centre * shift, q, self.sample_rate);
        }
    }

    /// One frame: reads the modulator's bands, then vocodes every channel.
    #[inline]
    fn frame(&mut self, buffer: &mut AudioBuffer<S>, frame: usize, modulator: f64) {
        let bands = self.bands;
        for (band, (section, memory)) in self.analysis[..bands]
            .iter()
            .zip(&mut self.modulator[..bands])
            .enumerate()
        {
            let filtered = section.twice(memory, modulator);
            self.levels[band] = SQRT_2 * self.follower.process_sample(band, filtered);
        }
        let gain = self.gain_now.next_value();
        let mix = self.mix_now.next_value();
        for (channel, bank) in self
            .carriers
            .iter_mut()
            .enumerate()
            .take(buffer.num_channels())
        {
            let sample = &mut buffer.channel_mut(channel)[frame];
            let input = sample.to_f64();
            let mut vocoded = 0.0;
            for ((section, memory), &level) in self.synthesis[..bands]
                .iter()
                .zip(&mut bank[..bands])
                .zip(&self.levels[..bands])
            {
                vocoded += section.twice(memory, input) * level;
            }
            *sample = S::from_f64(input * (1.0 - mix) + vocoded * gain * mix);
        }
    }

    fn run(&mut self, buffer: &mut AudioBuffer<S>, context: Option<&ProcessContext<'_, S>>) {
        let sidechain = context.and_then(|context| {
            context
                .sidechain
                .filter(|sidechain| sidechain.num_channels() > 0)
                .map(|sidechain| (sidechain, context.offset))
        });
        for frame in 0..buffer.num_frames() {
            let modulator = match sidechain {
                Some((sidechain, offset)) => average(sidechain, offset.saturating_add(frame)),
                None => average(buffer, frame),
            };
            self.frame(buffer, frame, modulator);
        }
    }
}

/// The average of the channels of `buffer` at `frame`, or zero past its end.
#[inline]
fn average<S: Sample>(buffer: &AudioBuffer<S>, frame: usize) -> f64 {
    let channels = buffer.num_channels();
    if channels == 0 || frame >= buffer.num_frames() {
        return 0.0;
    }
    let sum: f64 = buffer
        .iter_channels()
        .map(|channel| channel[frame].to_f64())
        .sum();
    sum / channels as f64
}

impl<S: Sample> Processor<S> for Vocoder<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.tune();
        self.carriers = vec![SILENT_BANK; spec.num_channels];
        Processor::<f64>::prepare(
            &mut self.follower,
            &ProcessSpec {
                num_channels: MAX_BANDS,
                ..*spec
            },
        );
        self.gain_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.mix_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.run(buffer, None);
    }

    /// Like [`process`](Processor::process), with the modulator taken from
    /// the context's sidechain when it has one.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        self.run(buffer, Some(context));
    }

    fn reset(&mut self) {
        self.modulator = SILENT_BANK;
        self.carriers.fill(SILENT_BANK);
        Processor::<f64>::reset(&mut self.follower);
        self.levels = [0.0; MAX_BANDS];
        self.gain_now.set_immediate(to_gain(self.gain_db));
        self.mix_now.set_immediate(self.mix);
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::Transport;
    use crate::event::EventBuffer;
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks};

    const RATE: f64 = 48_000.0;

    fn prepared(vocoder: Vocoder<f64>, channels: usize) -> Vocoder<f64> {
        let mut vocoder = vocoder;
        vocoder.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        vocoder
    }

    /// Sines at the given frequencies and amplitudes, on one channel.
    fn tones(frames: usize, parts: &[(f64, f64)]) -> AudioBuffer<f64> {
        let mut buffer = AudioBuffer::new(1, frames);
        for (n, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
            let t = n as f64 / RATE;
            *sample = parts
                .iter()
                .map(|&(hz, amplitude)| amplitude * (TAU * hz * t).sin())
                .sum();
        }
        buffer
    }

    /// The amplitude of the part of `signal` at `frequency`, through a Hann
    /// window, so that parts far away do not leak in.
    fn amplitude_at(signal: &[f64], frequency: f64) -> f64 {
        let len = signal.len() as f64;
        let (mut re, mut im, mut weight) = (0.0, 0.0, 0.0);
        for (n, &sample) in signal.iter().enumerate() {
            let window = 0.5 - 0.5 * (TAU * n as f64 / len).cos();
            let (sin, cos) = (TAU * frequency * n as f64 / RATE).sin_cos();
            re += sample * window * cos;
            im += sample * window * sin;
            weight += window;
        }
        2.0 * re.hypot(im) / weight
    }

    /// Runs `carrier` through the vocoder with `modulator` as the
    /// sidechain, in blocks of `block`.
    fn vocode(
        vocoder: &mut Vocoder<f64>,
        carrier: &mut AudioBuffer<f64>,
        modulator: &AudioBuffer<f64>,
        block: usize,
    ) {
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        let context = ProcessContext::new(&events, &transport).with_sidechain(modulator);
        let frames = carrier.num_frames();
        let mut start = 0;
        while start < frames {
            let end = (start + block).min(frames);
            let piece = context.for_piece(start);
            carrier.windowed(start..end, |part| vocoder.process_with(part, &piece));
            start = end;
        }
    }

    #[test]
    fn the_bands_are_spaced_evenly_in_pitch() {
        for bands in [8, 16, 32] {
            let vocoder = Vocoder::<f64>::new()
                .with_bands(bands)
                .with_range_hz(100.0, 6_400.0);
            assert_eq!(vocoder.band_hz(0), Some(100.0));
            assert!((vocoder.band_hz(bands - 1).unwrap() - 6_400.0).abs() < 1e-9);
            assert_eq!(vocoder.band_hz(bands), None);
            let step = 6.0 / (bands - 1) as f64;
            for index in 1..bands {
                let octaves =
                    (vocoder.band_hz(index).unwrap() / vocoder.band_hz(index - 1).unwrap()).log2();
                assert!((octaves - step).abs() < 1e-12);
            }
        }
        assert_eq!(Vocoder::<f64>::new().with_bands(3).bands(), 8);
        assert_eq!(Vocoder::<f64>::new().with_bands(99).bands(), 32);
    }

    #[test]
    fn each_band_reads_the_level_of_the_modulator_in_it() {
        let mut vocoder = prepared(Vocoder::new().with_bands(16), 1);
        let centre = vocoder.band_hz(6).unwrap();
        let modulator = tones(24_000, &[(centre, 0.4)]);
        let mut carrier = AudioBuffer::<f64>::new(1, 24_000);
        vocode(&mut vocoder, &mut carrier, &modulator, 512);
        let level = vocoder.band_level(6);
        assert!((level / 0.4 - 1.0).abs() < 0.05, "band 6 reads {level}");
        // Two bands away, the fourth-order filters hold it far down.
        for far in [3, 4, 8, 9] {
            let leak = db(vocoder.band_level(far) / level);
            assert!(leak < -20.0, "band {far} reads {leak} dB");
        }
        assert_eq!(vocoder.band_level(16), 0.0);
    }

    #[test]
    fn the_output_follows_the_modulators_band_levels() {
        // The carrier has a sine at the centre of the second band and of the
        // last but one; the modulator has the same two, 12 dB apart. Each
        // comes out at the carrier's amplitude times the modulator's. (The
        // bands overlap: a band between the two, in reach of both, would
        // add the louder one to the quieter in part.)
        for bands in [8, 16, 32] {
            let mut vocoder = prepared(Vocoder::new().with_bands(bands), 1);
            let (low, high) = (
                vocoder.band_hz(1).unwrap(),
                vocoder.band_hz(bands - 2).unwrap(),
            );
            let modulator = tones(48_000, &[(low, 0.4), (high, 0.1)]);
            let mut carrier = tones(48_000, &[(low, 0.5), (high, 0.5)]);
            vocode(&mut vocoder, &mut carrier, &modulator, 512);
            let tail = &carrier.channel(0)[24_000..];
            let (a, b) = (amplitude_at(tail, low), amplitude_at(tail, high));
            assert!(db(a / 0.2).abs() < 0.5, "{bands} bands: {a} at {low} Hz");
            assert!(db(b / 0.05).abs() < 0.5, "{bands} bands: {b} at {high} Hz");
            assert!(
                (db(a / b) - 12.04).abs() < 0.5,
                "{bands} bands: {} dB apart",
                db(a / b)
            );
        }

        // A modulator that moves from one band to another takes the output
        // with it.
        let mut vocoder = prepared(Vocoder::new(), 1);
        let (one, other) = (vocoder.band_hz(4).unwrap(), vocoder.band_hz(12).unwrap());
        let mut modulator = tones(48_000, &[(one, 0.5)]);
        let later = tones(24_000, &[(other, 0.5)]);
        modulator.channel_mut(0)[24_000..].copy_from_slice(later.channel(0));
        let mut carrier = noise::<f64>(1, 48_000, 2);
        vocode(&mut vocoder, &mut carrier, &modulator, 256);
        let (first, second) = carrier.channel(0).split_at(24_000);
        let (first, second) = (&first[12_000..], &second[12_000..]);
        assert!(amplitude_at(first, one) > 10.0 * amplitude_at(second, one));
        assert!(amplitude_at(second, other) > 10.0 * amplitude_at(first, other));
    }

    #[test]
    fn a_formant_shift_moves_the_carriers_bands() {
        // An octave up: the modulator in band 4 opens the carrier at twice
        // that band's centre, not at it.
        let mut vocoder = prepared(Vocoder::new().with_formant_shift(12.0), 1);
        let centre = vocoder.band_hz(4).unwrap();
        let modulator = tones(48_000, &[(centre, 0.4)]);
        let mut carrier = tones(48_000, &[(centre, 0.5), (2.0 * centre, 0.5)]);
        vocode(&mut vocoder, &mut carrier, &modulator, 512);
        let tail = &carrier.channel(0)[24_000..];
        let (moved, left) = (amplitude_at(tail, 2.0 * centre), amplitude_at(tail, centre));
        assert!(db(moved / 0.2).abs() < 0.5, "{moved}");
        assert!(db(left / moved) < -20.0, "{left} against {moved}");
    }

    #[test]
    fn a_silent_modulator_silences_the_carrier_and_the_input_stands_in_without_one() {
        let mut vocoder = prepared(Vocoder::new(), 2);
        let silence = AudioBuffer::<f64>::new(1, 48_000);
        let mut carrier = noise::<f64>(2, 48_000, 1);
        vocode(&mut vocoder, &mut carrier, &silence, 512);
        assert_eq!(strict_buffer_peak(&carrier), 0.0);

        // Through `process`, the input is its own modulator: a sine at a
        // band's centre comes out at its amplitude squared.
        let mut vocoder = prepared(Vocoder::new(), 1);
        let centre = vocoder.band_hz(8).unwrap();
        let mut signal = tones(48_000, &[(centre, 0.5)]);
        run_in_blocks(&mut vocoder, &mut signal, 512);
        let level = amplitude_at(&signal.channel(0)[24_000..], centre);
        assert!(db(level / 0.25).abs() < 0.5, "{level}");
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut vocoder = prepared(Vocoder::new().with_bands(24), 2);
            let modulator = noise::<f64>(1, 6_000, 9);
            let mut carrier = noise::<f64>(2, 6_000, 3);
            let mut start = 0;
            for (end, amount) in [(2_000, 0.3), (4_000, 0.9), (6_000, 0.5)] {
                let mut part_carrier = AudioBuffer::<f64>::new(2, end - start);
                for channel in 0..2 {
                    part_carrier
                        .channel_mut(channel)
                        .copy_from_slice(&carrier.channel(channel)[start..end]);
                }
                let mut part_modulator = AudioBuffer::<f64>::new(1, end - start);
                part_modulator
                    .channel_mut(0)
                    .copy_from_slice(&modulator.channel(0)[start..end]);
                vocode(&mut vocoder, &mut part_carrier, &part_modulator, block);
                for channel in 0..2 {
                    carrier.channel_mut(channel)[start..end]
                        .copy_from_slice(part_carrier.channel(channel));
                }
                vocoder.set_formant_shift(24.0 * amount - 12.0);
                vocoder.set_gain_db(12.0 * amount);
                vocoder.set_mix(amount);
                vocoder.set_release_ms(100.0 * amount);
                start = end;
            }
            carrier
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 6_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut vocoder = prepared(Vocoder::new().with_bands(32).with_gain_db(24.0), 2);
        let modulator = noise::<f64>(1, 24_000, 4);
        let mut carrier = noise::<f64>(2, 24_000, 5);
        vocode(&mut vocoder, &mut carrier, &modulator, 512);
        let peak = strict_buffer_peak(&carrier);
        assert!(peak > 0.01 && peak < 200.0, "{peak}");
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            vocoder.set_range_hz(garbage, garbage);
            vocoder.set_formant_shift(garbage);
            vocoder.set_attack_ms(garbage);
            vocoder.set_release_ms(garbage);
            vocoder.set_gain_db(garbage);
            vocoder.set_mix(garbage);
            let mut carrier = noise::<f64>(2, 4_800, 2);
            vocode(&mut vocoder, &mut carrier, &modulator, 512);
            let peak = strict_buffer_peak(&carrier);
            assert!(peak < 200.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_clears_the_filters_and_the_levels() {
        let build = || prepared(Vocoder::new(), 2);
        let modulator = noise::<f64>(1, 3_000, 7);
        let mut fresh = build();
        let mut expected = noise::<f64>(2, 3_000, 1);
        vocode(&mut fresh, &mut expected, &modulator, 512);
        let mut used = build();
        let mut other = noise::<f64>(2, 3_000, 5);
        vocode(&mut used, &mut other, &modulator, 512);
        used.reset();
        assert_eq!(used.band_level(3), 0.0);
        let mut again = noise::<f64>(2, 3_000, 1);
        vocode(&mut used, &mut again, &modulator, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut vocoder = Vocoder::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        // Not prepared: no carrier filters, so nothing is touched.
        vocoder.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        vocoder.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        vocoder.process(&mut buffer);
        assert_eq!(buffer.channel(1), &[0.0; 64]);
        assert_eq!(Processor::<f32>::latency_samples(&vocoder), 0);
        vocoder.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
