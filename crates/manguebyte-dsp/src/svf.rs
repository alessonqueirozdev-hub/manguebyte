use core::f64::consts::PI;
use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, Sample};

use crate::{ProcessSpec, Processor};

/// Which response a [`StateVariableFilter`] produces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SvfMode {
    /// Passes what is below the cutoff, 12 dB per octave.
    Lowpass,
    /// Passes what is above the cutoff, 12 dB per octave.
    Highpass,
    /// Passes a band around the cutoff, with unity gain at its centre.
    Bandpass,
    /// Removes a band around the cutoff.
    Notch,
    /// Passes everything, shifting phase around the cutoff.
    Allpass,
}

/// A second-order state-variable filter that stays stable and well-behaved
/// while its cutoff is being swept, even at audio rate.
///
/// A [`Biquad`](crate::Biquad) computes the same responses more cheaply, but
/// its internal state has no physical meaning, so changing its coefficients
/// abruptly makes it click and can make it blow up. This filter keeps the
/// state of two integrators, which stays valid whatever the cutoff does. Use
/// it for synthesiser filters, wah, auto-filter and anything else that moves.
///
/// The structure is the trapezoidal-integrated state-variable filter from
/// Andrew Simper, "Linear Trapezoidal Integrated SVF" (Cytomic, 2013), built
/// on the zero-delay-feedback method in Vadim Zavalishin, "The Art of VA
/// Filter Design".
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, StateVariableFilter, SvfMode};
///
/// let mut filter = StateVariableFilter::<f32>::new(SvfMode::Lowpass, 500.0, 0.707);
/// filter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
///
/// let mut block = AudioBuffer::<f32>::new(1, 64);
/// block.channel_mut(0)[0] = 1.0;
///
/// // Sweep the cutoff on every block, as an envelope or LFO would.
/// for step in 0..100 {
///     filter.set_frequency(500.0 + 50.0 * step as f64);
///     filter.process(&mut block);
/// }
/// assert!(block.peak().is_finite());
/// ```
#[derive(Debug, Clone)]
pub struct StateVariableFilter<S: Sample = f32> {
    mode: SvfMode,
    frequency: f64,
    q: f64,
    sample_rate: f64,
    /// Integrator gain, `tan(π·fc/fs)`.
    g: f64,
    /// Damping, `1/Q`.
    k: f64,
    a1: f64,
    a2: f64,
    a3: f64,
    /// The two integrator states of each channel.
    state: Vec<[f64; 2]>,
    marker: PhantomData<S>,
}

impl<S: Sample> StateVariableFilter<S> {
    /// Creates a filter. `q` of 0.707 gives the flattest passband; higher
    /// values add a resonant peak at the cutoff.
    pub fn new(mode: SvfMode, frequency: f64, q: f64) -> Self {
        let mut filter = Self {
            mode,
            frequency,
            q,
            sample_rate: 44_100.0,
            g: 0.0,
            k: 0.0,
            a1: 0.0,
            a2: 0.0,
            a3: 0.0,
            state: Vec::new(),
            marker: PhantomData,
        };
        filter.update();
        filter
    }

    /// Changes the response. Real-time safe, and click-free: the state
    /// carries over.
    pub fn set_mode(&mut self, mode: SvfMode) {
        self.mode = mode;
    }

    /// Changes the cutoff in hertz. Real-time safe, and cheap enough to call
    /// for every sample. It is limited to just below half the sample rate.
    pub fn set_frequency(&mut self, frequency: f64) {
        self.frequency = frequency;
        self.update();
    }

    /// Changes the resonance. Real-time safe.
    pub fn set_q(&mut self, q: f64) {
        self.q = q;
        self.update();
    }

    /// The cutoff in hertz, as last set.
    pub fn frequency(&self) -> f64 {
        self.frequency
    }

    /// The resonance, as last set.
    pub fn q(&self) -> f64 {
        self.q
    }

    /// The phase shift, in radians, that the filter gives a steady sine of
    /// `frequency` hertz at its current settings. Negative is a delay.
    ///
    /// The filter is the bilinear transform, with the cutoff prewarped, of
    /// the analogue prototype `1 / (s² + s/Q + 1)` and its relatives, so this
    /// is exact for the digital filter, not an approximation. The result
    /// lies in `-2π..=π`, going from low frequencies to high: from 0 to -π
    /// for the low-pass, π to 0 for the high-pass, π/2 to -π/2 for the
    /// band-pass, 0 to -π/2 and then π/2 to 0 for the notch, and 0 to -2π
    /// for the all-pass. Where the
    /// response is zero (DC for the high-pass and band-pass, the cutoff for
    /// the notch) the phase has no meaning and the value is that limit.
    ///
    /// A synthesiser can use it to keep a note in tune while a filter sweeps:
    /// as a low-pass closes, its delay at the note's pitch grows, which
    /// lowers the pitch heard while it moves, as a Doppler shift does.
    ///
    /// The filter needs to have been prepared, so that it knows the sample
    /// rate. Real-time safe.
    ///
    /// ```
    /// use core::f64::consts::FRAC_PI_2;
    /// use manguebyte_dsp::{ProcessSpec, Processor, StateVariableFilter, SvfMode};
    ///
    /// let mut filter = StateVariableFilter::<f32>::new(SvfMode::Lowpass, 1_000.0, 0.707);
    /// filter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
    /// // A second-order low-pass lags by a quarter turn at its cutoff.
    /// assert!((filter.phase_response(1_000.0) + FRAC_PI_2).abs() < 1e-12);
    /// assert!(filter.phase_response(10.0) > -0.02);
    /// ```
    pub fn phase_response(&self, frequency: f64) -> f64 {
        let nyquist = 0.5 * self.sample_rate;
        let frequency = frequency.clamp(0.0, nyquist * 0.999_999);
        // The frequency as the analogue prototype sees it, cutoff at 1.
        let w = (PI * frequency / self.sample_rate).tan() / self.g;
        let real = 1.0 - w * w;
        // The denominator's angle: 0 well below the cutoff, π/2 at it and
        // π well above it.
        let denominator = (self.k * w).atan2(real);
        match self.mode {
            SvfMode::Lowpass => -denominator,
            SvfMode::Highpass => PI - denominator,
            SvfMode::Bandpass => 0.5 * PI - denominator,
            SvfMode::Notch => {
                if real >= 0.0 {
                    -denominator
                } else {
                    PI - denominator
                }
            }
            SvfMode::Allpass => -2.0 * denominator,
        }
    }

    /// The integrator gain `g` and the damping `k` the filter runs on:
    /// `g = tan(π·cutoff / sample rate)` and `k = 1/Q`, with the cutoff and
    /// the resonance as limited. They give the response at any frequency in
    /// closed form: with `w = tan(π·frequency / sample rate) / g`, the
    /// low-pass's is `1 / (1 - w² + i·k·w)`, which is what
    /// [`phase_response`](Self::phase_response) works from.
    ///
    /// A caller that asks for the response at one frequency on every sample
    /// while the cutoff moves can work out that frequency's tangent once and
    /// use these, instead of a tangent per call. Real-time safe.
    ///
    /// ```
    /// use core::f64::consts::PI;
    /// use manguebyte_dsp::{ProcessSpec, Processor, StateVariableFilter, SvfMode};
    ///
    /// let mut filter = StateVariableFilter::<f32>::new(SvfMode::Lowpass, 1_000.0, 2.0);
    /// filter.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
    /// let (g, k) = filter.coefficients();
    /// assert_eq!(g, (PI * 1_000.0 / 48_000.0).tan());
    /// assert_eq!(k, 0.5);
    /// // The low-pass's phase at 3 kHz, from them.
    /// let w = (PI * 3_000.0 / 48_000.0).tan() / g;
    /// let lag = (k * w).atan2(1.0 - w * w);
    /// assert!((filter.phase_response(3_000.0) + lag).abs() < 1e-15);
    /// ```
    pub fn coefficients(&self) -> (f64, f64) {
        (self.g, self.k)
    }

    fn update(&mut self) {
        let frequency = self.frequency.clamp(1.0, self.sample_rate * 0.49);
        self.g = (PI * frequency / self.sample_rate).tan();
        self.k = 1.0 / self.q.max(0.01);
        self.a1 = 1.0 / (1.0 + self.g * (self.g + self.k));
        self.a2 = self.g * self.a1;
        self.a3 = self.g * self.a2;
    }

    /// Filters one sample of one channel. Real-time safe.
    ///
    /// Use this, with [`set_frequency`](Self::set_frequency) before each
    /// call, to modulate the cutoff at audio rate.
    ///
    /// # Panics
    ///
    /// Panics if `channel` is not below the channel count the filter was
    /// prepared for.
    #[inline]
    pub fn process_sample(&mut self, channel: usize, input: S) -> S {
        let [ic1, ic2] = &mut self.state[channel];
        let x = input.to_f64();
        let v3 = x - *ic2;
        let v1 = self.a1 * *ic1 + self.a2 * v3;
        let v2 = *ic2 + self.a2 * *ic1 + self.a3 * v3;
        *ic1 = 2.0 * v1 - *ic1;
        *ic2 = 2.0 * v2 - *ic2;
        S::from_f64(match self.mode {
            SvfMode::Lowpass => v2,
            SvfMode::Highpass => x - self.k * v1 - v2,
            SvfMode::Bandpass => self.k * v1,
            SvfMode::Notch => x - self.k * v1,
            SvfMode::Allpass => x - 2.0 * self.k * v1,
        })
    }
}

impl<S: Sample> Processor<S> for StateVariableFilter<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.state = vec![[0.0; 2]; spec.num_channels];
        self.update();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let channels = buffer.num_channels().min(self.state.len());
        for channel in 0..channels {
            for sample in buffer.channel_mut(channel) {
                *sample = self.process_sample(channel, *sample);
            }
        }
    }

    fn reset(&mut self) {
        self.state.fill([0.0; 2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: f64 = 48_000.0;

    fn prepared(mode: SvfMode, frequency: f64, q: f64) -> StateVariableFilter<f64> {
        let mut filter = StateVariableFilter::new(mode, frequency, q);
        filter.prepare(&ProcessSpec {
            sample_rate: SAMPLE_RATE,
            max_block_size: 512,
            num_channels: 1,
        });
        filter
    }

    /// Gain in decibels for a steady sine, measured after the filter has
    /// settled.
    fn gain_db(filter: &mut StateVariableFilter<f64>, frequency: f64) -> f64 {
        filter.reset();
        let step = 2.0 * PI * frequency / SAMPLE_RATE;
        let mut peak = 0.0f64;
        for n in 0..48_000 {
            let output = filter.process_sample(0, (step * n as f64).sin());
            if n >= 24_000 {
                peak = peak.max(output.abs());
            }
        }
        20.0 * peak.log10()
    }

    #[test]
    fn lowpass_and_highpass_are_three_decibels_down_at_the_cutoff() {
        let q = core::f64::consts::FRAC_1_SQRT_2;
        let mut low = prepared(SvfMode::Lowpass, 1_000.0, q);
        assert!(gain_db(&mut low, 50.0).abs() < 0.05);
        assert!((gain_db(&mut low, 1_000.0) + 3.01).abs() < 0.05);
        // Two octaves up, a 12 dB per octave slope is about 24 dB down.
        assert!((gain_db(&mut low, 4_000.0) + 24.0).abs() < 1.0);

        let mut high = prepared(SvfMode::Highpass, 1_000.0, q);
        assert!(gain_db(&mut high, 15_000.0).abs() < 0.05);
        assert!((gain_db(&mut high, 1_000.0) + 3.01).abs() < 0.05);
        assert!((gain_db(&mut high, 250.0) + 24.0).abs() < 1.0);
    }

    #[test]
    fn bandpass_notch_and_allpass() {
        let mut band = prepared(SvfMode::Bandpass, 2_000.0, 4.0);
        assert!(gain_db(&mut band, 2_000.0).abs() < 0.05);
        assert!(gain_db(&mut band, 200.0) < -20.0);
        assert!(gain_db(&mut band, 12_000.0) < -15.0);

        let mut notch = prepared(SvfMode::Notch, 2_000.0, 4.0);
        assert!(gain_db(&mut notch, 2_000.0) < -60.0);
        assert!(gain_db(&mut notch, 200.0).abs() < 0.1);

        let mut allpass = prepared(SvfMode::Allpass, 2_000.0, 0.7);
        for frequency in [100.0, 2_000.0, 10_000.0] {
            assert!(gain_db(&mut allpass, frequency).abs() < 0.01);
        }
    }

    /// Phase of a steady sine after the filter, in radians, from the
    /// output's correlation with the input's sine and cosine over whole
    /// periods once the filter has settled.
    fn measured_phase(filter: &mut StateVariableFilter<f64>, frequency: f64) -> f64 {
        filter.reset();
        let step = 2.0 * PI * frequency / SAMPLE_RATE;
        let settle = 96_000;
        // A whole number of periods, as near to a second as there is.
        let periods = (frequency.round() as usize).max(1);
        let length = (periods as f64 * SAMPLE_RATE / frequency).round() as usize;
        let (mut along, mut across) = (0.0, 0.0);
        for n in 0..settle + length {
            let angle = step * n as f64;
            let output = filter.process_sample(0, angle.sin());
            if n >= settle {
                along += output * angle.sin();
                across += output * angle.cos();
            }
        }
        across.atan2(along)
    }

    #[test]
    fn the_phase_response_is_what_a_sine_measures() {
        let wrap = |angle: f64| (angle + PI).rem_euclid(2.0 * PI) - PI;
        for mode in [
            SvfMode::Lowpass,
            SvfMode::Highpass,
            SvfMode::Bandpass,
            SvfMode::Notch,
            SvfMode::Allpass,
        ] {
            for q in [0.5, 0.707, 2.0, 8.0] {
                let mut filter = prepared(mode, 1_500.0, q);
                for frequency in [50.0, 400.0, 1_000.0, 1_400.0, 1_700.0, 3_000.0, 15_000.0] {
                    let predicted = filter.phase_response(frequency);
                    let measured = measured_phase(&mut filter, frequency);
                    let error = wrap(predicted - measured).abs();
                    assert!(
                        error < 1e-3,
                        "{mode:?}, Q {q}, {frequency} Hz: predicted {predicted}, measured {measured}"
                    );
                }
            }
        }
        // Exact values: a quarter turn at the cutoff, whatever the Q, and
        // the ends of the ranges far from it.
        let filter = prepared(SvfMode::Lowpass, 1_000.0, 5.0);
        assert!((filter.phase_response(1_000.0) + PI / 2.0).abs() < 1e-12);
        assert_eq!(filter.phase_response(0.0), 0.0);
        assert!((filter.phase_response(23_999.0) + PI).abs() < 1e-2);
        let allpass = prepared(SvfMode::Allpass, 1_000.0, 0.7);
        assert!((allpass.phase_response(1_000.0) + PI).abs() < 1e-12);
        let band = prepared(SvfMode::Bandpass, 1_000.0, 3.0);
        assert!(band.phase_response(1_000.0).abs() < 1e-12);
        // Beyond the Nyquist frequency, or negative, it holds the ends.
        let low = prepared(SvfMode::Lowpass, 1_000.0, 0.7);
        assert_eq!(low.phase_response(-5.0), 0.0);
        assert!(low.phase_response(1e9).is_finite());
        assert!(low.phase_response(f64::INFINITY) >= -PI);
    }

    #[test]
    fn resonance_peaks_by_q_at_the_cutoff() {
        let mut filter = prepared(SvfMode::Lowpass, 1_000.0, 8.0);
        let expected = 20.0 * 8.0f64.log10();
        assert!((gain_db(&mut filter, 1_000.0) - expected).abs() < 0.1);
    }

    #[test]
    fn stays_bounded_while_the_cutoff_is_modulated_at_audio_rate() {
        let mut filter = prepared(SvfMode::Lowpass, 1_000.0, 12.0);
        let mut peak = 0.0f64;
        for n in 0..96_000 {
            let time = n as f64 / SAMPLE_RATE;
            // A 300 Hz modulator sweeping the cutoff across eight octaves.
            let cutoff = 60.0 * 2f64.powf(4.0 + 4.0 * (2.0 * PI * 300.0 * time).sin());
            filter.set_frequency(cutoff);
            let input = (2.0 * PI * 110.0 * time).sin();
            peak = peak.max(filter.process_sample(0, input).abs());
        }
        assert!(peak.is_finite());
        // Resonant gain is Q; a little transient overshoot on top is fine.
        assert!(peak < 40.0, "output reached {peak}");
    }

    #[test]
    fn extreme_settings_are_clamped_rather_than_unstable() {
        let mut filter = prepared(SvfMode::Lowpass, 1e9, 0.0);
        filter.set_frequency(-5.0);
        filter.set_frequency(f64::MAX);
        let mut peak = 0.0f64;
        for n in 0..4_800 {
            let input = if n % 2 == 0 { 1.0 } else { -1.0 };
            peak = peak.max(filter.process_sample(0, input).abs());
        }
        assert!(peak.is_finite() && peak < 4.0);
    }

    #[test]
    fn channels_are_independent_and_reset_clears_them() {
        let mut filter = StateVariableFilter::<f32>::new(SvfMode::Lowpass, 200.0, 0.7);
        filter.prepare(&ProcessSpec {
            sample_rate: SAMPLE_RATE,
            max_block_size: 32,
            num_channels: 2,
        });
        let mut buffer = AudioBuffer::<f32>::new(2, 32);
        buffer.channel_mut(0).fill(1.0);
        filter.process(&mut buffer);
        assert!(buffer.channel(0)[31] > 0.0);
        assert_eq!(buffer.channel(1), &[0.0; 32]);

        filter.reset();
        buffer.clear();
        filter.process(&mut buffer);
        assert_eq!(buffer.peak(), 0.0);
    }
}
