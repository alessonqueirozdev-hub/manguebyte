//! The filter of a voice: a one-pole filter or up to three state-variable
//! sections, whose cutoff and resonance can move on every sample.

use core::f64::consts::{FRAC_1_SQRT_2, PI};

use crate::region::FilterType;

/// Most sections a filter cascades.
const MAX_SECTIONS: usize = 3;

/// The quality factors of the sections of a Butterworth filter of 2, 4 and
/// 6 poles: `1 / (2 sin((2i + 1) π / 2n))`. Their product is `1/√2` each
/// time, which is the gain at the cutoff.
const BUTTERWORTH: [[f64; MAX_SECTIONS]; MAX_SECTIONS] = [
    [FRAC_1_SQRT_2, 0.0, 0.0],
    [0.541_196_100_146_197, 1.306_562_964_876_376_7, 0.0],
    [
        0.517_638_090_205_041_5,
        FRAC_1_SQRT_2,
        1.931_851_652_578_136_6,
    ],
];

/// The lowest cutoff, in hertz.
const LOWEST: f64 = 1.0;

/// The highest cutoff, as a fraction of the sample rate: just below
/// Nyquist, where the prewarped integrator gain is still finite.
const HIGHEST: f64 = 0.49;

/// What one section passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Response {
    Lowpass,
    Highpass,
    Bandpass,
    Notch,
}

/// How a [`FilterType`] is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    OnePole { high: bool },
    Sections { response: Response, count: usize },
}

impl Shape {
    fn of(kind: FilterType) -> Self {
        let sections = |response, count| Self::Sections { response, count };
        match kind {
            FilterType::Lowpass1 => Self::OnePole { high: false },
            FilterType::Highpass1 => Self::OnePole { high: true },
            FilterType::Lowpass2 => sections(Response::Lowpass, 1),
            FilterType::Highpass2 => sections(Response::Highpass, 1),
            FilterType::Bandpass2 => sections(Response::Bandpass, 1),
            FilterType::Bandreject2 => sections(Response::Notch, 1),
            FilterType::Lowpass4 => sections(Response::Lowpass, 2),
            FilterType::Highpass4 => sections(Response::Highpass, 2),
            FilterType::Lowpass6 => sections(Response::Lowpass, 3),
            FilterType::Highpass6 => sections(Response::Highpass, 3),
        }
    }
}

/// The settings a filter moves to: its integrator gain and each section's
/// damping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Tuning {
    g: f64,
    k: [f64; MAX_SECTIONS],
}

impl Tuning {
    const REST: Self = Self {
        g: 0.0,
        k: [0.0; MAX_SECTIONS],
    };
}

/// The filter of one voice, for one or two channels. Holds no memory of
/// its own beyond its fields, so it is copied with the voice and never
/// allocates.
///
/// The one-pole filter and the sections are the trapezoidal-integrated
/// (zero-delay-feedback) forms of their analogue prototypes, as in Vadim
/// Zavalishin, "The Art of VA Filter Design", and, for the
/// state-variable section, Andrew Simper, "Linear Trapezoidal Integrated
/// SVF" (Cytomic, 2013). Their state stays valid however the settings
/// move, so a cutoff swept at audio rate neither clicks nor blows up.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VoiceFilter {
    shape: Shape,
    /// The settings in use, and what is added to them per sample while
    /// they glide to new ones.
    now: Tuning,
    step: Tuning,
    /// Samples of glide left.
    gliding: u32,
    /// Per section: `1 / (1 + g (g + k))`, and it times `g` and `g²`.
    a: [[f64; 3]; MAX_SECTIONS],
    /// The one-pole filter's `g / (1 + g)`.
    one_pole: f64,
    /// Per channel and section, the two integrators' states.
    state: [[[f64; 2]; MAX_SECTIONS]; 2],
}

impl VoiceFilter {
    pub(crate) const IDLE: Self = Self {
        shape: Shape::OnePole { high: false },
        now: Tuning::REST,
        step: Tuning::REST,
        gliding: 0,
        a: [[0.0; 3]; MAX_SECTIONS],
        one_pole: 0.0,
        state: [[[0.0; 2]; MAX_SECTIONS]; 2],
    };

    /// A filter of type `kind`, silent, to be given its settings with
    /// [`glide`](Self::glide) before it runs.
    pub(crate) fn new(kind: FilterType) -> Self {
        Self {
            shape: Shape::of(kind),
            ..Self::IDLE
        }
    }

    /// The settings for a cutoff of `cutoff` hertz, held between 1 Hz and
    /// 0.49 of `sample_rate`, and a resonance of `resonance` decibels.
    pub(crate) fn tuning(&self, cutoff: f64, resonance: f64, sample_rate: f64) -> Tuning {
        let highest = HIGHEST * sample_rate;
        let cutoff = if cutoff.is_nan() {
            highest
        } else {
            cutoff.clamp(LOWEST, highest.max(LOWEST))
        };
        let g = (PI * cutoff / sample_rate).tan();
        let mut k = [0.0; MAX_SECTIONS];
        if let Shape::Sections { response, count } = self.shape {
            let resonance = if resonance.is_finite() {
                resonance.clamp(0.0, 40.0)
            } else {
                0.0
            };
            let qs: &[f64] = match response {
                Response::Lowpass | Response::Highpass => &BUTTERWORTH[count - 1][..count],
                Response::Bandpass | Response::Notch => &BUTTERWORTH[0][..1],
            };
            // The resonance shared out evenly: each section's gain at the
            // cutoff rises by its share.
            let raise = 10f64.powf(resonance / (20.0 * qs.len() as f64));
            for (k, q) in k.iter_mut().zip(qs) {
                *k = 1.0 / (q * raise);
            }
        }
        Tuning { g, k }
    }

    /// Moves to `tuning` in a straight line over the next `samples`
    /// samples, or at once at 0.
    pub(crate) fn glide(&mut self, tuning: Tuning, samples: u32) {
        if samples == 0 {
            self.now = tuning;
            self.gliding = 0;
            self.update();
            return;
        }
        let count = f64::from(samples);
        self.step.g = (tuning.g - self.now.g) / count;
        for section in 0..MAX_SECTIONS {
            self.step.k[section] = (tuning.k[section] - self.now.k[section]) / count;
        }
        self.gliding = samples;
    }

    fn update(&mut self) {
        let g = self.now.g;
        self.one_pole = g / (1.0 + g);
        for (a, &k) in self.a.iter_mut().zip(&self.now.k) {
            let a1 = 1.0 / (1.0 + g * (g + k));
            *a = [a1, g * a1, g * g * a1];
        }
    }

    /// Filters one frame: both channels, or the left alone (returned on
    /// both sides) when `stereo` is false.
    #[inline]
    pub(crate) fn process(&mut self, left: f64, right: f64, stereo: bool) -> (f64, f64) {
        if self.gliding > 0 {
            self.gliding -= 1;
            self.now.g += self.step.g;
            for section in 0..MAX_SECTIONS {
                self.now.k[section] += self.step.k[section];
            }
            self.update();
        }
        let left = self.channel(0, left);
        let right = if stereo { self.channel(1, right) } else { left };
        (left, right)
    }

    #[inline]
    fn channel(&mut self, channel: usize, input: f64) -> f64 {
        match self.shape {
            Shape::OnePole { high } => {
                let state = &mut self.state[channel][0][0];
                let v = (input - *state) * self.one_pole;
                let low = v + *state;
                *state = low + v;
                if high { input - low } else { low }
            }
            Shape::Sections { response, count } => {
                let mut x = input;
                for section in 0..count {
                    let [a1, a2, a3] = self.a[section];
                    let k = self.now.k[section];
                    let [ic1, ic2] = &mut self.state[channel][section];
                    let v3 = x - *ic2;
                    let v1 = a1 * *ic1 + a2 * v3;
                    let v2 = *ic2 + a2 * *ic1 + a3 * v3;
                    *ic1 = 2.0 * v1 - *ic1;
                    *ic2 = 2.0 * v2 - *ic2;
                    x = match response {
                        Response::Lowpass => v2,
                        Response::Highpass => x - k * v1 - v2,
                        Response::Bandpass => k * v1,
                        Response::Notch => x - k * v1,
                    };
                }
                x
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f64 = 48_000.0;

    /// The gain in decibels a steady sine of `frequency` (a whole number of
    /// cycles a second) gets, from its power after the filter settles.
    fn gain_db(kind: FilterType, cutoff: f64, resonance: f64, frequency: f64) -> f64 {
        let mut filter = VoiceFilter::new(kind);
        filter.glide(filter.tuning(cutoff, resonance, RATE), 0);
        let step = 2.0 * PI * frequency / RATE;
        let mut power = 0.0f64;
        for n in 0..96_000 {
            let (output, _) = filter.process((step * n as f64).sin(), 0.0, false);
            if n >= 48_000 {
                power += output * output;
            }
        }
        10.0 * (2.0 * power / 48_000.0).log10()
    }

    /// The analogue prototype's gain, at the prewarped frequency.
    fn butterworth_db(poles: i32, high: bool, cutoff: f64, frequency: f64) -> f64 {
        let w = (PI * frequency / RATE).tan() / (PI * cutoff / RATE).tan();
        let w = if high { 1.0 / w } else { w };
        -10.0 * (1.0 + w.powi(2 * poles)).log10()
    }

    #[test]
    fn every_type_has_its_butterworth_response() {
        let cases = [
            (FilterType::Lowpass1, 1, false),
            (FilterType::Highpass1, 1, true),
            (FilterType::Lowpass2, 2, false),
            (FilterType::Highpass2, 2, true),
            (FilterType::Lowpass4, 4, false),
            (FilterType::Highpass4, 4, true),
            (FilterType::Lowpass6, 6, false),
            (FilterType::Highpass6, 6, true),
        ];
        for (kind, poles, high) in cases {
            for frequency in [250.0, 1_000.0, 4_000.0] {
                let measured = gain_db(kind, 1_000.0, 0.0, frequency);
                let expected = butterworth_db(poles, high, 1_000.0, frequency);
                assert!(
                    (measured - expected).abs() < 0.02,
                    "{kind:?} at {frequency} Hz: {measured} dB, not {expected}"
                );
            }
        }
    }

    #[test]
    fn band_types_pass_and_reject_their_centre() {
        assert!(gain_db(FilterType::Bandpass2, 1_000.0, 0.0, 1_000.0).abs() < 0.01);
        assert!(gain_db(FilterType::Bandpass2, 1_000.0, 0.0, 100.0) < -16.0);
        assert!(gain_db(FilterType::Bandreject2, 1_000.0, 0.0, 1_000.0) < -40.0);
        assert!(gain_db(FilterType::Bandreject2, 1_000.0, 0.0, 100.0).abs() < 0.15);
    }

    #[test]
    fn resonance_raises_the_cutoff_by_its_decibels() {
        for kind in [
            FilterType::Lowpass2,
            FilterType::Lowpass4,
            FilterType::Lowpass6,
        ] {
            for resonance in [0.0, 6.0, 12.0, 24.0] {
                let measured = gain_db(kind, 1_000.0, resonance, 1_000.0);
                let expected = resonance - 10.0 * 2f64.log10();
                assert!(
                    (measured - expected).abs() < 0.02,
                    "{kind:?} at {resonance} dB: {measured}"
                );
            }
        }
    }

    #[test]
    fn the_cutoff_is_held_below_nyquist_and_stays_stable() {
        for rate in [8_000.0, 22_050.0, 44_100.0, 192_000.0] {
            let mut filter = VoiceFilter::new(FilterType::Lowpass4);
            let top = filter.tuning(1e9, 40.0, rate);
            let bottom = filter.tuning(-5.0, 40.0, rate);
            assert!(top.g.is_finite() && top.g > 0.0);
            filter.glide(bottom, 0);
            // Swept from the bottom to the top and back, every sample.
            let mut largest = 0.0f64;
            for n in 0..20_000u32 {
                if n % 100 == 0 {
                    let target = if (n / 100) % 2 == 0 { top } else { bottom };
                    filter.glide(target, 100);
                }
                let input = if n % 37 < 18 { 1.0 } else { -1.0 };
                let (output, _) = filter.process(input, input, true);
                largest = largest.max(output.abs());
            }
            assert!(
                largest.is_finite() && largest < 1_000.0,
                "{rate}: {largest}"
            );
        }
    }
}
