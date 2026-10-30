//! Plucked strings by physical modelling: guitars and basses.

use core::f64::consts::{PI, TAU};

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    Handover, Noise, SILENCE, Tone, add_to_every_channel, flush, key_hz, setting, t60_factor,
    velocity_gain,
};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning as KeyTuning;
use crate::{DelayLine, ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice};

/// The lowest note a string plays, in hertz. Lower keys sound here.
const LOWEST_HZ: f64 = 20.0;

/// The highest note, as a fraction of the sample rate. Up to about a
/// twelfth of it the tuning stays within a cent; see [`PluckedString`].
const HIGHEST_RATIO: f64 = 0.2;

/// How many all-pass stages make the stiffness of the string.
const DISPERSION_STAGES: usize = 4;

/// The all-pass coefficient of the stiffest string.
const STIFFEST: f64 = -0.75;

/// The loop gain no setting may reach: the string always dies away.
const MAX_LOOP_GAIN: f64 = 0.999_99;

/// The frequency whose decay time [`StringParams::high_decay`] sets.
const HIGH_HZ: f64 = 3_000.0;

/// The level of a pluck at full velocity before the loop: the RMS of its
/// lowest harmonics over a period, and the peak it may not pass.
const EXCITATION_RMS: f64 = 0.25;
const EXCITATION_PEAK: f64 = 2.0;

/// The settings of a [`PluckedString`].
///
/// Times are in seconds, frequencies in hertz and levels in decibels
/// unless said otherwise. Out-of-range values are limited when the
/// settings are taken (see [`PluckedString::set_params`]), and a value that
/// is not a number takes the default of
/// [`nylon_guitar`](Self::nylon_guitar).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StringParams {
    /// How long the fundamental of middle C takes to fall by 60 dB while
    /// the note is held, 0.05 to 60.
    pub decay: f64,
    /// How many times shorter that decay is an octave higher, 1 to 4: real
    /// strings ring longer the lower they are.
    pub decay_per_octave: f64,
    /// The decay time at 3 kHz as a fraction of the fundamental's, 0.01 to
    /// 1: how fast the brightness of the pluck dies away.
    pub high_decay: f64,
    /// How long the string takes to fall by 60 dB once the note is
    /// released and the string is damped, 0.01 to 10.
    pub release: f64,
    /// Where the string is plucked, as a fraction of its length from the
    /// bridge, 0.02 to 0.5. Plucking at a fraction `p` removes every
    /// harmonic that is a multiple of `1 / p`: near the bridge the sound is
    /// thin and bright, at the middle hollow and round.
    pub pluck_position: f64,
    /// The brightness of a pluck at full velocity: the cut-off of the
    /// low-pass that shapes it, in hertz, 100 to 20 000. A nail or a pick
    /// is bright, a fingertip dark.
    pub pluck_brightness: f64,
    /// How many octaves lower that cut-off is at the lowest velocity, 0 to
    /// 6: a string plucked softly is darker as well as quieter.
    pub velocity_darkening: f64,
    /// How much quieter the lowest velocity is than the highest, 0 to 60.
    pub velocity_range_db: f64,
    /// The string's stiffness, 0 to 1: a stiff string's overtones are
    /// sharper than whole multiples of its fundamental, which wound bass
    /// strings and steel strings are, and nylon hardly at all.
    pub stiffness: f64,
    /// How much of the pluck is a strike rather than a pull, 0 to 1. A
    /// string pulled aside and let go starts as a triangle, whose overtones
    /// fall quickly; a string struck, as the thumb slaps a bass string,
    /// starts moving from one point, and its overtones fall half as fast in
    /// decibels. A pick does some of each.
    pub strike: f64,
    /// How much noise the pluck carries, 0 to 1: the scrape of a pick or a
    /// nail on the string's winding.
    pub pluck_noise: f64,
    /// The resonances of the instrument's body, added to the string.
    pub body: [BodyMode; 4],
    /// How much of the body is heard, 0 to 1. An electric instrument has
    /// none.
    pub body_mix: f64,
    /// Where an electric pickup sits, as a fraction of the string's length
    /// from the bridge, 0.02 to 0.5, or 0 for an acoustic instrument heard
    /// at the bridge. Like the pluck, a pickup misses the harmonics that
    /// have a node under it.
    pub pickup_position: f64,
    /// The cut-off of the final low-pass, 200 to 20 000: an electric
    /// pickup's resonance, or the top an acoustic body lets out.
    pub tone: f64,
    /// The resonance of that low-pass, as its Q, 0.5 to 4.
    pub tone_q: f64,
    /// How far the string buzzes against the frets just after it is
    /// struck, 0 to 1, as a slapped bass string does.
    pub buzz: f64,
    /// How long the buzz lasts, 0.005 to 1.
    pub buzz_time: f64,
    /// How much louder an accented note is, 0 to 24.
    pub accent_db: f64,
    /// How many octaves brighter an accented note is plucked, 0 to 4.
    pub accent_brightness: f64,
    /// How much more an accented note buzzes, 0 to 1. With a bright pluck
    /// and buzz this turns a slap into a pop.
    pub accent_buzz: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

/// A resonance of an instrument's body: a band-pass filter whose output is
/// added to the string's sound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BodyMode {
    /// Its frequency in hertz, 20 to 10 000.
    pub frequency: f64,
    /// How narrow it is, as a Q, 0.5 to 50.
    pub q: f64,
    /// How loud it is, as a gain, 0 to 4.
    pub gain: f64,
}

impl BodyMode {
    /// A resonance.
    pub const fn new(frequency: f64, q: f64, gain: f64) -> Self {
        Self { frequency, q, gain }
    }

    /// No resonance.
    pub const NONE: Self = Self::new(100.0, 1.0, 0.0);
}

impl StringParams {
    /// A classical guitar: nylon strings plucked by the fingers over the
    /// sound hole, dark and round, a wooden body with its air resonance
    /// near 100 Hz and its top plate near 200 Hz.
    ///
    /// The body resonances follow the measurements summarised in N. H.
    /// Fletcher and T. D. Rossing, "The Physics of Musical Instruments",
    /// chapter 9.
    pub fn nylon_guitar() -> Self {
        Self {
            decay: 3.0,
            decay_per_octave: 1.5,
            high_decay: 0.12,
            release: 0.18,
            pluck_position: 0.16,
            pluck_brightness: 2_800.0,
            velocity_darkening: 1.8,
            velocity_range_db: 20.0,
            stiffness: 0.02,
            strike: 0.15,
            pluck_noise: 0.12,
            body: [
                BodyMode::new(100.0, 10.0, 0.9),
                BodyMode::new(205.0, 14.0, 0.7),
                BodyMode::new(390.0, 12.0, 0.45),
                BodyMode::new(580.0, 10.0, 0.3),
            ],
            body_mix: 0.6,
            pickup_position: 0.0,
            tone: 6_500.0,
            tone_q: 0.6,
            buzz: 0.0,
            buzz_time: 0.05,
            accent_db: 3.0,
            accent_brightness: 0.8,
            accent_buzz: 0.0,
            level: 0.9,
        }
    }

    /// A steel-string acoustic guitar played with a pick: brighter, longer
    /// ringing and a little stiff, with a larger body.
    pub fn steel_guitar() -> Self {
        Self {
            decay: 4.5,
            decay_per_octave: 1.45,
            high_decay: 0.25,
            release: 0.25,
            pluck_position: 0.12,
            pluck_brightness: 6_500.0,
            velocity_darkening: 2.2,
            velocity_range_db: 22.0,
            stiffness: 0.12,
            strike: 0.35,
            pluck_noise: 0.25,
            body: [
                BodyMode::new(115.0, 10.0, 0.7),
                BodyMode::new(225.0, 14.0, 0.6),
                BodyMode::new(430.0, 12.0, 0.4),
                BodyMode::new(640.0, 10.0, 0.3),
            ],
            body_mix: 0.45,
            pickup_position: 0.0,
            tone: 11_000.0,
            tone_q: 0.6,
            buzz: 0.0,
            buzz_time: 0.05,
            accent_db: 3.0,
            accent_brightness: 0.6,
            accent_buzz: 0.0,
            level: 0.2,
        }
    }

    /// An electric bass played with the fingers: round wound strings,
    /// plucked between the pickups, heard through a pickup a fifth of the
    /// way from the bridge whose resonance sits near 2.6 kHz.
    pub fn finger_bass() -> Self {
        Self {
            decay: 6.0,
            decay_per_octave: 1.4,
            high_decay: 0.05,
            release: 0.12,
            pluck_position: 0.2,
            pluck_brightness: 1_600.0,
            velocity_darkening: 1.5,
            velocity_range_db: 24.0,
            stiffness: 0.3,
            strike: 0.3,
            pluck_noise: 0.08,
            body: [BodyMode::NONE; 4],
            body_mix: 0.0,
            pickup_position: 0.2,
            tone: 2_600.0,
            tone_q: 1.4,
            buzz: 0.0,
            buzz_time: 0.05,
            accent_db: 3.0,
            accent_brightness: 0.6,
            accent_buzz: 0.0,
            level: 0.6,
        }
    }

    /// A slapped electric bass: the thumb strikes the string over the end
    /// of the neck, which gives a hard, bright attack, and the string
    /// buzzes against the frets for a moment. An accented note is a pop:
    /// louder, brighter and buzzing more, as when a finger snaps the string
    /// against the frets.
    pub fn slap_bass() -> Self {
        Self {
            decay: 5.0,
            decay_per_octave: 1.4,
            high_decay: 0.02,
            release: 0.1,
            pluck_position: 0.28,
            pluck_brightness: 9_000.0,
            velocity_darkening: 2.0,
            velocity_range_db: 22.0,
            stiffness: 0.35,
            strike: 0.9,
            pluck_noise: 0.1,
            body: [BodyMode::NONE; 4],
            body_mix: 0.0,
            pickup_position: 0.1,
            tone: 6_000.0,
            tone_q: 0.9,
            buzz: 0.45,
            buzz_time: 0.07,
            accent_db: 4.0,
            accent_brightness: 0.7,
            accent_buzz: 0.35,
            level: 0.55,
        }
    }

    /// The settings with every value limited to its range.
    fn limited(self) -> Self {
        let default = Self::nylon_guitar();
        let body_default = BodyMode::NONE;
        Self {
            decay: setting(self.decay, 0.05, 60.0, default.decay),
            decay_per_octave: setting(self.decay_per_octave, 1.0, 4.0, default.decay_per_octave),
            high_decay: setting(self.high_decay, 0.01, 1.0, default.high_decay),
            release: setting(self.release, 0.01, 10.0, default.release),
            pluck_position: setting(self.pluck_position, 0.02, 0.5, default.pluck_position),
            pluck_brightness: setting(
                self.pluck_brightness,
                100.0,
                20_000.0,
                default.pluck_brightness,
            ),
            velocity_darkening: setting(
                self.velocity_darkening,
                0.0,
                6.0,
                default.velocity_darkening,
            ),
            velocity_range_db: setting(
                self.velocity_range_db,
                0.0,
                60.0,
                default.velocity_range_db,
            ),
            stiffness: setting(self.stiffness, 0.0, 1.0, default.stiffness),
            strike: setting(self.strike, 0.0, 1.0, default.strike),
            pluck_noise: setting(self.pluck_noise, 0.0, 1.0, default.pluck_noise),
            body: self.body.map(|mode| BodyMode {
                frequency: setting(mode.frequency, 20.0, 10_000.0, body_default.frequency),
                q: setting(mode.q, 0.5, 50.0, body_default.q),
                gain: setting(mode.gain, 0.0, 4.0, 0.0),
            }),
            body_mix: setting(self.body_mix, 0.0, 1.0, default.body_mix),
            pickup_position: if self.pickup_position > 0.0 {
                setting(self.pickup_position, 0.02, 0.5, 0.0)
            } else {
                0.0
            },
            tone: setting(self.tone, 200.0, 20_000.0, default.tone),
            tone_q: setting(self.tone_q, 0.5, 4.0, default.tone_q),
            buzz: setting(self.buzz, 0.0, 1.0, 0.0),
            buzz_time: setting(self.buzz_time, 0.005, 1.0, default.buzz_time),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            accent_brightness: setting(self.accent_brightness, 0.0, 4.0, default.accent_brightness),
            accent_buzz: setting(self.accent_buzz, 0.0, 1.0, 0.0),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    /// The decay time of the fundamental of a note `f0` hertz high.
    fn decay_at(&self, f0: f64) -> f64 {
        let octaves = (f0 / key_hz(60.0)).log2();
        self.decay / self.decay_per_octave.powf(octaves)
    }
}

impl Default for StringParams {
    fn default() -> Self {
        Self::nylon_guitar()
    }
}

/// The phase delay, in samples, of the all-pass `(c + z⁻¹) / (1 + c z⁻¹)`
/// at `w` radians per sample.
fn allpass_delay(c: f64, w: f64) -> f64 {
    1.0 - 2.0 * (c * w.sin()).atan2(1.0 + c * w.cos()) / w
}

/// The coefficient of the all-pass `(c + z⁻¹) / (1 + c z⁻¹)` whose phase
/// delay at `w` radians per sample is `delay` samples. Solved in closed
/// form from [`allpass_delay`], so that the string is in tune at its
/// fundamental exactly, rather than at the low-frequency limit the usual
/// `(1 - d) / (1 + d)` is exact for.
fn allpass_coefficient(delay: f64, w: f64) -> f64 {
    let t = (0.5 * w * (1.0 - delay)).tan();
    let c = t / (w.sin() - t * w.cos());
    if c.is_finite() {
        c.clamp(-0.999, 0.999)
    } else {
        0.0
    }
}

/// One step of the all-pass `(c + z⁻¹) / (1 + c z⁻¹)`, whose memory is the
/// last input and output.
#[inline]
fn allpass(memory: &mut [f64; 2], x: f64, c: f64) -> f64 {
    let y = c * x + memory[0] - c * memory[1];
    *memory = [x, y];
    y
}

/// The pole `a` of the loss filter `g (1 + a) / (1 + a z⁻¹)` whose gain at
/// `wh` is `ratio` times its gain at `w0`, for a `ratio` of at most 1.
///
/// The condition is a quadratic in `a` whose roots multiply to 1; the one
/// inside the unit circle is the filter. A ratio too small for any
/// one-pole filter gets the steepest one allowed.
fn loss_pole(ratio: f64, w0: f64, wh: f64) -> f64 {
    if ratio.is_nan() || ratio >= 0.999_999 {
        return 0.0;
    }
    let r2 = ratio * ratio;
    let (c0, ch) = (w0.cos(), wh.cos());
    let a = r2 - 1.0;
    let b = 2.0 * (r2 * ch - c0);
    let discriminant = (b * b - 4.0 * a * a).max(0.0).sqrt();
    let roots = [
        (-b + discriminant) / (2.0 * a),
        (-b - discriminant) / (2.0 * a),
    ];
    let inside = if roots[0].abs() <= roots[1].abs() {
        roots[0]
    } else {
        roots[1]
    };
    if inside.is_finite() {
        inside.clamp(-0.98, 0.0)
    } else {
        0.0
    }
}

/// What a string needs to sound one pitch with one decay.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Tuning {
    /// Whole samples of delay in the loop.
    whole: usize,
    /// The all-pass that supplies the rest of a period.
    fraction: f64,
    /// The loss filter `b0 / (1 + a z⁻¹)`.
    b0: f64,
    a: f64,
    /// The stiffness all-passes' coefficient, and how many are used.
    dispersion: f64,
    stages: usize,
    /// The period in samples.
    period: f64,
}

/// One plucked note: the loop of the string and everything after it.
#[derive(Debug, Clone)]
struct StringTone {
    sample_rate: f64,
    /// The string: one period of travelling wave, round trip.
    line: DelayLine<f64>,
    tuning: Tuning,
    /// Memories of the loss filter, the stiffness all-passes and the
    /// fractional delay.
    loss: f64,
    stiffness: [[f64; 2]; DISPERSION_STAGES],
    fraction: [f64; 2],
    /// The excitation still to be fed in, and a buffer to build the next
    /// one in.
    excitation: Vec<f64>,
    excitation_len: usize,
    excitation_at: usize,
    scratch: Vec<f64>,
    /// The fret: how deep the buzz still goes, how fast that fades, and the
    /// level the string hits the fret at.
    buzz: f64,
    buzz_fade: f64,
    fret: f64,
    /// Where the pickup reads the string, in samples behind the newest, or
    /// 0 for none.
    pickup: f64,
    body: [StateVariableFilter<f64>; 4],
    body_gain: [f64; 4],
    body_mix: f64,
    tone: StateVariableFilter<f64>,
    level: f64,
    /// A peak follower of the output, to know when the string is silent.
    follower: f64,
    follower_fall: f64,
    /// The key's frequency before any bend, and whether the key is held.
    f0: f64,
    held: bool,
}

impl StringTone {
    fn new() -> Self {
        let filter = |mode: SvfMode| StateVariableFilter::new(mode, 1_000.0, 0.707);
        Self {
            sample_rate: 48_000.0,
            line: DelayLine::new(4),
            tuning: Tuning {
                whole: 1,
                fraction: 0.0,
                b0: 0.0,
                a: 0.0,
                dispersion: 0.0,
                stages: 0,
                period: 2.0,
            },
            loss: 0.0,
            stiffness: [[0.0; 2]; DISPERSION_STAGES],
            fraction: [0.0; 2],
            excitation: Vec::new(),
            excitation_len: 0,
            excitation_at: 0,
            scratch: Vec::new(),
            buzz: 0.0,
            buzz_fade: 0.0,
            fret: 0.0,
            pickup: 0.0,
            body: [
                filter(SvfMode::Bandpass),
                filter(SvfMode::Bandpass),
                filter(SvfMode::Bandpass),
                filter(SvfMode::Bandpass),
            ],
            body_gain: [0.0; 4],
            body_mix: 0.0,
            tone: filter(SvfMode::Lowpass),
            level: 0.0,
            follower: 0.0,
            follower_fall: 0.0,
            f0: 440.0,
            held: false,
        }
    }

    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        let longest = (sample_rate / LOWEST_HZ).ceil() as usize + 8;
        self.line = DelayLine::new(longest);
        // A burst of a period, delayed copies for the pluck position, and
        // the tail of its low-pass.
        self.excitation = vec![0.0; 2 * longest + 1_024];
        self.scratch = vec![0.0; 2 * longest + 1_024];
        let spec = ProcessSpec {
            sample_rate,
            max_block_size: 1,
            num_channels: 1,
        };
        for filter in self.body.iter_mut().chain(core::iter::once(&mut self.tone)) {
            Processor::<f64>::prepare(filter, &spec);
        }
        self.follower_fall = t60_factor(0.3, sample_rate);
        self.silence();
    }

    fn configure(&mut self, params: &StringParams) {
        for ((filter, gain), mode) in self
            .body
            .iter_mut()
            .zip(&mut self.body_gain)
            .zip(&params.body)
        {
            filter.set_frequency(mode.frequency);
            filter.set_q(mode.q);
            *gain = mode.gain;
        }
        self.body_mix = params.body_mix;
        self.tone
            .set_frequency(params.tone.min(0.45 * self.sample_rate));
        self.tone.set_q(params.tone_q);
        self.level = params.level;
    }

    /// Sets the loop for a note of `f0` hertz whose fundamental decays in
    /// `decay` seconds and whose 3 kHz in `high` seconds. With
    /// `keep_whole`, the whole samples of delay stay as they are, if the
    /// fraction can make up the rest, so that the loop is not read from a
    /// new place while it sounds.
    fn tune(&mut self, f0: f64, decay: f64, high: f64, stiffness: f64, keep_whole: bool) {
        let rate = self.sample_rate;
        let f0 = setting(f0, LOWEST_HZ, HIGHEST_RATIO * rate, 440.0);
        let w0 = TAU * f0 / rate;
        let period = rate / f0;

        // The loss filter, from the gains per period at the fundamental and
        // at the high frequency that give their decay times (V. Välimäki,
        // J. Huopaniemi, M. Karjalainen and Z. Jánosy, "Physical modeling
        // of plucked string instruments with application to real-time
        // sound synthesis", JAES 1996).
        let decay = decay.max(1e-3);
        let high = high.clamp(1e-4, decay);
        let mut wh = TAU * HIGH_HZ.max(3.0 * f0) / rate;
        wh = wh.min(0.9 * PI);
        let gain_at = |seconds: f64| 10f64.powf(-3.0 / (f0 * seconds));
        let (g0, gh) = (gain_at(decay), gain_at(high));
        let a = if wh > 1.05 * w0 {
            loss_pole(gh / g0, w0, wh)
        } else {
            0.0
        };
        let g = (g0 * (1.0 + 2.0 * a * w0.cos() + a * a).sqrt() / (1.0 + a)).min(MAX_LOOP_GAIN);
        let loss_delay = (-a * w0.sin()).atan2(1.0 + a * w0.cos()) / w0;

        // The stiffness: all-passes that delay low frequencies more than
        // high ones, so the overtones come round sooner and sound sharp
        // (S. A. Van Duyne and J. O. Smith, "A simplified approach to
        // modeling dispersion caused by stiffness in strings and plates",
        // ICMC 1994). Weakened, then dropped, where they would take too
        // much of a short period.
        let mut dispersion = STIFFEST * stiffness;
        let mut stages = DISPERSION_STAGES;
        let budget = 0.4 * period;
        let mut tries = 0;
        while DISPERSION_STAGES as f64 * allpass_delay(dispersion, w0) > budget {
            dispersion *= 0.6;
            tries += 1;
            if tries > 12 {
                stages = 0;
                break;
            }
        }
        if stages == 0 || dispersion.abs() < 1e-6 {
            stages = 0;
            dispersion = 0.0;
        }
        let stiffness_delay = stages as f64 * allpass_delay(dispersion, w0);

        // Whole samples, and an all-pass for the rest, tuned at the
        // fundamental (D. A. Jaffe and J. O. Smith, "Extensions of the
        // Karplus-Strong plucked-string algorithm", Computer Music Journal,
        // 1983). The all-pass is kept between half a sample and a sample and
        // a half, where it is at its most accurate.
        let rest = period - loss_delay - stiffness_delay;
        let longest = self.line.max_delay().max(1);
        let mut whole = ((rest - 0.5).floor().max(1.0) as usize).min(longest);
        if keep_whole {
            let fraction = rest - self.tuning.whole as f64;
            if (0.1..1.9).contains(&fraction) {
                whole = self.tuning.whole;
            }
        }
        let fraction = allpass_coefficient(rest - whole as f64, w0);
        self.tuning = Tuning {
            whole,
            fraction,
            b0: g * (1.0 + a),
            a,
            dispersion,
            stages,
            period,
        };
    }

    /// Plucks the string, which may still be sounding from an earlier
    /// pluck: the new excitation is added to what is left of the old one.
    fn pluck(&mut self, params: &StringParams, f0: f64, velocity: f64, accent: bool, seed: Noise) {
        self.f0 = f0;
        self.held = true;
        let decay = params.decay_at(f0);
        self.tune(
            f0,
            decay,
            decay * params.high_decay,
            params.stiffness,
            false,
        );
        let rate = self.sample_rate;
        let period = self.tuning.period;
        let mut gain = velocity_gain(velocity, params.velocity_range_db);
        let mut brightness =
            params.pluck_brightness * (-params.velocity_darkening * (1.0 - velocity)).exp2();
        let mut buzz = params.buzz;
        if accent {
            gain *= super::common::db(params.accent_db);
            brightness *= params.accent_brightness.exp2();
            buzz += params.accent_buzz;
        }
        let brightness = brightness.clamp(f0.min(0.45 * rate), 0.45 * rate);

        // A period of the pluck: the grain of the pick first, a period of
        // noise through the comb of the pluck position (Jaffe and Smith): the
        // wave leaves both ways, and the half that goes to the nut comes
        // back inverted `p` of a period later.
        let room = self.scratch.len();
        let burst = (period.round() as usize).clamp(1, (room / 2).max(1));
        let offset = ((params.pluck_position * period).round() as usize).clamp(1, burst);
        let mut noise = seed;
        let scratch = &mut self.scratch;
        scratch.fill(0.0);
        let grain = 0.5 * params.pluck_noise;
        for slot in scratch.iter_mut().take(burst) {
            *slot = grain * noise.next();
        }
        let combed = (burst + offset).min(room);
        for n in (0..combed).rev() {
            let earlier = if n >= offset {
                scratch[n - offset]
            } else {
                0.0
            };
            scratch[n] -= earlier;
        }
        // Then the string's own shape as it is let go, over one period of
        // the loop: plucked, a triangle with its apex at the pluck position,
        // whose harmonics fall as sin(nπp) / n²; struck, the step its
        // velocity makes, whose harmonics fall as sin(nπp) / n. Both are
        // started where the triangle crosses zero, so the note starts with
        // no step of its own but the strike's.
        let apex = params.pluck_position;
        for (n, slot) in scratch.iter_mut().take(burst).enumerate() {
            let x = ((n as f64 + 0.5) / burst as f64 + 0.5 * apex).fract();
            let triangle = if x < apex {
                x / apex
            } else {
                (1.0 - x) / (1.0 - apex)
            } - 0.5;
            let step = if x < apex { 1.0 - apex } else { -apex };
            *slot += (1.0 - params.strike) * triangle + 2.0 * params.strike * step;
        }
        // The pluck's own brightness and the velocity's darkening, as one
        // low-pass: the dynamic level filter of Jaffe and Smith.
        let pole = (-TAU * brightness / rate).exp();
        let tail = ((8.0 / (1.0 - pole)).ceil() as usize).min(512);
        let len = (combed + tail).min(room);
        // The energy of the lowest harmonics, measured through a third-order
        // low-pass at three times the fundamental.
        let reference = (-TAU * (3.0 * f0).min(0.45 * rate) / rate).exp();
        let mut low = 0.0;
        let mut lower = [0.0; 3];
        let mut loudest = 0.0f64;
        let mut energy = 0.0;
        for slot in scratch.iter_mut().take(len) {
            low = (1.0 - pole) * *slot + pole * low;
            *slot = low;
            loudest = loudest.max(low.abs());
            let mut x = low;
            for memory in &mut lower {
                *memory = (1.0 - reference) * x + reference * *memory;
                x = *memory;
            }
            energy += x * x;
        }
        // Every pluck carries the same energy in its lowest harmonics
        // before its gain, so that velocity and accent change the level by
        // what they say and a brighter pluck adds overtones on top. A
        // pluck with so little in its lowest harmonics that this would make
        // it huge is held to a peak instead.
        let rms = (energy / period.max(1.0)).sqrt();
        let scale = if loudest > 0.0 && rms > 0.0 {
            gain * (EXCITATION_RMS / rms).min(EXCITATION_PEAK / loudest)
        } else {
            0.0
        };

        // Whatever of the last excitation has not gone in yet goes in with
        // this one.
        let left = self.excitation_len - self.excitation_at;
        self.excitation
            .copy_within(self.excitation_at..self.excitation_len, 0);
        for (n, slot) in self.excitation.iter_mut().enumerate().take(len.max(left)) {
            let old = if n < left { *slot } else { 0.0 };
            let new = if n < len { scratch[n] * scale } else { 0.0 };
            *slot = old + new;
        }
        self.excitation_len = len.max(left);
        self.excitation_at = 0;

        // The fret is met at a level below the pluck's peak, which the
        // buzz brings down and then lets go of.
        self.fret = loudest * scale;
        self.buzz = buzz.min(0.95);
        self.buzz_fade = 0.01f64.powf(1.0 / (params.buzz_time * rate));
        self.pickup = if params.pickup_position > 0.0 {
            1.0 + params.pickup_position * period
        } else {
            0.0
        };
    }

    /// The key is released: the string is damped.
    fn release(&mut self, params: &StringParams, bend: f64) {
        if !self.held {
            return;
        }
        self.held = false;
        self.retune(params, bend);
    }

    /// Tunes the loop for the key, bent by `bend` semitones, with the decay
    /// of a held or a released string.
    fn retune(&mut self, params: &StringParams, bend: f64) {
        let f0 = self.f0 * (bend / 12.0).exp2();
        let (decay, high) = if self.held {
            let decay = params.decay_at(f0);
            (decay, decay * params.high_decay)
        } else {
            (
                params.release,
                0.4 * params.release * params.high_decay.sqrt(),
            )
        };
        self.tune(f0, decay, high, params.stiffness, true);
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let tuning = self.tuning;
        let delayed = self.line.read_nearest(tuning.whole as f64);
        self.loss = flush(tuning.b0 * delayed - tuning.a * self.loss);
        let mut v = self.loss;
        for memory in self.stiffness.iter_mut().take(tuning.stages) {
            v = allpass(memory, v, tuning.dispersion);
        }
        v = allpass(&mut self.fraction, v, tuning.fraction);
        if self.buzz > 0.0 {
            // The fret stops the string going further one way: what would
            // pass it is mostly absorbed.
            let fret = self.fret * (1.0 - self.buzz);
            if v < -fret {
                v = -fret + 0.25 * (v + fret);
            }
            self.buzz *= self.buzz_fade;
            if self.buzz < 1e-4 {
                self.buzz = 0.0;
            }
        }
        if self.excitation_at < self.excitation_len {
            v += self.excitation[self.excitation_at];
            self.excitation_at += 1;
        }
        self.line.push(v);

        let mut out = v;
        if self.pickup > 0.0 {
            out -= self.line.read(self.pickup);
        }
        let mut body = 0.0;
        for (filter, gain) in self.body.iter_mut().zip(&self.body_gain) {
            if *gain > 0.0 {
                body += gain * filter.process_sample(0, out);
            }
        }
        let out = self.tone.process_sample(0, out + self.body_mix * body) * self.level;
        self.follower = (self.follower * self.follower_fall).max(out.abs());
        out
    }
}

impl Tone for StringTone {
    fn is_sounding(&self) -> bool {
        self.excitation_at < self.excitation_len || self.follower > SILENCE
    }

    fn silence(&mut self) {
        self.line.clear();
        self.loss = 0.0;
        self.stiffness = [[0.0; 2]; DISPERSION_STAGES];
        self.fraction = [0.0; 2];
        self.excitation_len = 0;
        self.excitation_at = 0;
        self.buzz = 0.0;
        self.follower = 0.0;
        self.held = false;
        for filter in self.body.iter_mut().chain(core::iter::once(&mut self.tone)) {
            Processor::<f64>::reset(filter);
        }
    }
}

/// A plucked string, by physical modelling: a voice for
/// [`Synth`](crate::Synth) that plays guitars and basses.
///
/// The string is a loop of delay, after K. Karplus and A. Strong, "Digital
/// synthesis of plucked-string and drum timbres" (Computer Music Journal,
/// 1983), with the extensions of D. A. Jaffe and J. O. Smith, "Extensions
/// of the Karplus-Strong plucked-string algorithm" (same journal, 1983):
///
/// ```text
///  pluck: shape at release + combed noise -> dynamic low-pass ---------------+
///                                                                            v
///   +--[delay N]--[loss filter]--[stiffness all-passes]--[tuning all-pass]--(+)--+--> pickup comb
///   |                                                                       |  -> body resonances
///   +-----------------------------------------------------------------------+  -> tone low-pass
/// ```
///
/// - **In tune.** The delay of the loop at the fundamental, that of the
///   loss filter and the all-passes included, is worked out exactly and the
///   last fraction of a sample is made up by a first-order all-pass tuned
///   at the fundamental, so every note from 20 Hz up to a twelfth of the
///   sample rate (4 kHz at 48 kHz) is within a cent of equal temperament,
///   measured. Higher notes drift sharp by a few cents.
/// - **A decay that depends on pitch.** The loss filter is a one-pole
///   low-pass designed for each note from two decay times, the
///   fundamental's ([`StringParams::decay`], shorter the higher the note)
///   and that at 3 kHz ([`StringParams::high_decay`]), so low notes ring
///   longer and every note darkens as it fades.
/// - **The pluck.** One period of the string's shape as it is let go goes
///   into the loop: a triangle with its apex at the pluck position for a
///   string pulled aside, the step of its velocity for one struck, or a
///   mix ([`StringParams::strike`]), both of which lack the harmonics with
///   a node there, and a little noise for the grain of the pick, through a
///   comb that removes the same harmonics. A low-pass whose cut-off falls
///   with the velocity (the dynamic level filter) follows: a soft note is
///   quieter and darker.
/// - **Stiffness.** All-passes make the overtones of a stiff string sharp.
/// - **The instrument.** A pickup comb for an electric instrument, a few
///   resonant band-passes for an acoustic body, and a final low-pass.
/// - **Fret buzz.** For a slap or a pop the string meets a fret that it
///   cannot pass, for a moment after it is struck. That barrier is a
///   clipper inside the loop, and the one part of the instrument that is
///   not band-limited: the rattle it makes is noisy by nature and over in
///   a tenth of a second.
///
/// Releasing the key damps the string ([`StringParams::release`]). Striking
/// a sounding key again plucks the string again, on top of what it is
/// doing. A voice that is taken over for a new note lets the old one fade
/// over [`HANDOVER_SECONDS`](super::HANDOVER_SECONDS) while the new one
/// starts. Pitch bend and [`Expression::Tuning`] retune the loop; a bend
/// that moves the loop's length by a whole sample makes a small
/// discontinuity.
///
/// An accent plucks louder and brighter ([`StringParams::accent_db`],
/// [`StringParams::accent_brightness`]) and, for the slapped bass, pops.
/// The noise of each pluck is seeded by the key, so a note
/// sounds the same every time it is played.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{PluckedString, StringParams};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut guitar = Synth::with_voices(6, || PluckedString::new(StringParams::nylon_guitar()));
/// guitar.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [40, 47, 52, 56, 59, 64] {
///     guitar.note_on(NoteRef::new(0, key), 0.8);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// guitar.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct PluckedString {
    params: StringParams,
    tones: Handover<StringTone>,
    /// Pitch bend and per-note tuning, in semitones.
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: KeyTuning,
    key: u8,
}

impl PluckedString {
    /// A string with these settings. Until it is prepared it assumes
    /// 48 kHz and has no room to sound.
    pub fn new(params: StringParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            tones: Handover::new(StringTone::new(), StringTone::new()),
            bend: 0.0,
            tuning: 0.0,
            key_tuning: KeyTuning::default(),
            key: 60,
        };
        let params = voice.params;
        voice.tones.now.configure(&params);
        voice.tones.old.configure(&params);
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note keeps its pluck and its tuning, and takes the new
    /// body, pickup, tone and level at once.
    pub fn set_params(&mut self, params: StringParams) {
        self.params = params.limited();
        let params = self.params;
        self.tones.now.configure(&params);
        self.tones.old.configure(&params);
    }

    /// The settings, as limited.
    pub fn params(&self) -> &StringParams {
        &self.params
    }

    fn retune(&mut self) {
        let params = self.params;
        let bend = self.bend + self.tuning;
        if self.tones.now.is_sounding() {
            self.tones.now.retune(&params, bend);
        }
    }
}

impl<S: Sample> Voice<S> for PluckedString {
    fn prepare(&mut self, spec: &ProcessSpec) {
        let params = self.params;
        self.tones.prepare(spec.sample_rate);
        for tone in [&mut self.tones.now, &mut self.tones.old] {
            tone.prepare(spec.sample_rate);
            tone.configure(&params);
        }
    }

    fn set_tuning(&mut self, tuning: &KeyTuning) {
        self.key_tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let params = self.params;
        self.key = note.key.min(127);
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let f0 = key_hz(self.key_tuning.key_pitch(self.key) + self.bend + self.tuning);
        // Seeded by the key alone: every pluck of a key has the same grain,
        // so that its level and brightness are what the velocity and the
        // accent make them and not a matter of chance.
        let seed = Noise::for_note(self.key, 0.0, 1);
        self.tones.now.pluck(&params, f0, velocity, accent, seed);
        // The loop's tuning includes the bend; the key's own frequency is
        // what later bends start from.
        self.tones.now.f0 = self.key_tuning.key_frequency(self.key);
    }

    fn note_off(&mut self, _velocity: f32) {
        let params = self.params;
        let bend = self.bend + self.tuning;
        self.tones.now.release(&params, bend);
    }

    fn choke(&mut self) {
        self.tones.choke();
        self.tuning = 0.0;
    }

    fn is_active(&self) -> bool {
        self.tones.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        if !self.tones.is_active() {
            return;
        }
        let tones = &mut self.tones;
        add_to_every_channel(buffer, || tones.next(StringTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            self.retune();
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            self.retune();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::measure::{
        self, RATE, accented, between, centroid, cents, decibels, frequency_near, largest_jump,
        note, off, on, play, rms, synth_of, t60_at,
    };

    fn presets() -> [(&'static str, StringParams); 4] {
        [
            ("nylon", StringParams::nylon_guitar()),
            ("steel", StringParams::steel_guitar()),
            ("finger bass", StringParams::finger_bass()),
            ("slap bass", StringParams::slap_bass()),
        ]
    }

    fn voice(params: StringParams) -> [PluckedString; 1] {
        [PluckedString::new(params)]
    }

    #[test]
    fn the_coefficients_are_solved_exactly() {
        for w in [0.003, 0.05, 0.3, 1.0] {
            for delay in [0.5, 0.8, 1.0, 1.3, 1.49] {
                let c = allpass_coefficient(delay, w);
                assert!((allpass_delay(c, w) - delay).abs() < 1e-9, "{w} {delay}");
            }
        }
        // The loss filter has the ratio of gains it was asked for.
        let (w0, wh) = (0.03, 0.4);
        let a = loss_pole(0.9, w0, wh);
        let gain = |w: f64| ((1.0 + a) / (1.0 + 2.0 * a * w.cos() + a * a).sqrt()).abs();
        assert!((gain(wh) / gain(w0) - 0.9).abs() < 1e-9);
        assert!(a < 0.0 && a > -1.0);
        assert_eq!(loss_pole(1.0, w0, wh), 0.0);
        assert_eq!(loss_pole(f64::NAN, w0, wh), 0.0);
        // A ratio no one-pole can reach gets the steepest allowed.
        assert_eq!(loss_pole(1e-6, w0, wh), -0.98);
    }

    #[test]
    fn every_note_is_within_a_cent_across_the_keyboard() {
        let mut worst = 0.0f64;
        for (name, params) in presets() {
            // The guitar's range and the bass's, a little past both ends.
            let keys: &[u8] = if name.contains("bass") {
                &[24, 28, 33, 40, 47, 55, 64, 72]
            } else {
                &[40, 45, 52, 59, 64, 71, 76, 83, 88, 96, 100]
            };
            for &key in keys {
                let signal = note(voice(params), on(key, 0.8), 1.0, 1.0);
                let expected = key_hz(f64::from(key));
                let measured = frequency_near(between(&signal, 0.2, 0.8), expected);
                let error = cents(measured, expected);
                worst = worst.max(error.abs());
                assert!(error.abs() < 1.0, "{name} key {key}: {error:.3} cents");
            }
        }
        println!("worst tuning error {worst:.4} cents");
    }

    #[test]
    fn the_decay_follows_the_settings_and_the_pitch() {
        let params = StringParams {
            stiffness: 0.0,
            body_mix: 0.0,
            ..StringParams::nylon_guitar()
        };
        for key in [48u8, 60, 72] {
            let f0 = key_hz(f64::from(key));
            let expected = params.decay_at(f0);
            let signal = note(voice(params), on(key, 1.0), 3.0, 3.0);
            // Until the note has fallen about 40 dB, well above the level
            // at which the voice counts as silent and stops.
            let end = (0.4 + 0.6 * expected).min(2.8);
            let measured = t60_at(&signal, f0, 0.4, end);
            let error = measured / expected - 1.0;
            println!("key {key}: decay {measured:.3} s against {expected:.3} s");
            assert!(
                error.abs() < 0.1,
                "key {key}: {measured} s against {expected} s"
            );
        }
        // Lower notes ring longer.
        assert!(params.decay_at(key_hz(48.0)) > 1.4 * params.decay_at(key_hz(60.0)));

        // Releasing damps the string within its release time.
        let signal = note(voice(params), on(60, 1.0), 0.5, 1.5);
        let measured = t60_at(&signal, key_hz(60.0), 0.55, 0.75);
        assert!(
            (measured / params.release - 1.0).abs() < 0.25,
            "release {measured}"
        );
    }

    #[test]
    fn velocity_makes_a_note_louder_and_brighter() {
        for (name, params) in presets() {
            let loud = note(voice(params), on(45, 1.0), 1.0, 0.5);
            let soft = note(voice(params), on(45, 0.4), 1.0, 0.5);
            let difference = decibels(rms(&loud) / rms(&soft));
            let expected = params.velocity_range_db * 0.6;
            // Over the pluck, where its brightness is heard.
            let (bright, dark) = (centroid(&loud, 4_096), centroid(&soft, 4_096));
            println!("{name}: {difference:.1} dB, centroid {bright:.0} Hz against {dark:.0} Hz");
            assert!(
                (difference - expected).abs() < 2.0,
                "{name}: {difference} dB"
            );
            assert!(
                bright > 1.08 * dark,
                "{name}: {bright} Hz against {dark} Hz"
            );
        }
    }

    #[test]
    fn an_accent_is_louder_and_brighter() {
        for (name, params) in presets() {
            let plain = note(voice(params), on(45, 0.7), 1.0, 0.5);
            let strong = note(voice(params), accented(45, 0.7), 1.0, 0.5);
            let difference = decibels(rms(&strong) / rms(&plain));
            let (bright, dark) = (centroid(&strong, 8_192), centroid(&plain, 8_192));
            println!("{name}: accent {difference:.1} dB, {bright:.0} Hz against {dark:.0} Hz");
            assert!(
                (difference - params.accent_db).abs() < 1.5,
                "{name}: {difference}"
            );
            assert!(bright > 1.02 * dark, "{name}: {bright} against {dark}");
        }
    }

    #[test]
    fn notes_start_end_and_are_taken_over_without_a_click() {
        for (name, params) in presets() {
            // A note alone, released half way.
            let alone = note(voice(params), on(52, 1.0), 0.3, 0.6);
            let attack = largest_jump(between(&alone, 0.0, 0.05));
            let steady = largest_jump(between(&alone, 0.05, 0.3));
            let release = largest_jump(between(&alone, 0.3, 0.6));
            // The release damps what is there: nothing jumps more than the
            // string already does.
            assert!(
                release <= 1.05 * steady,
                "{name}: release {release} against {steady}"
            );

            // One voice taken over by a new note: what is left once the new
            // note alone is taken away is the old note, fading.
            let steal = (0.25 * RATE) as usize;
            let mut synth = synth_of(voice(params));
            let both = play(
                &mut synth,
                &[(0, on(52, 1.0)), (steal, on(40, 1.0))],
                steal * 2,
                256,
            );
            let mut fresh = synth_of(voice(params));
            let second = play(&mut fresh, &[(steal, on(40, 1.0))], steal * 2, 256);
            let residual: Vec<f64> = both.iter().zip(&second).map(|(a, b)| a - b).collect();
            let at_steal = largest_jump(&residual[steal - 64..steal + 512]);
            assert!(
                at_steal <= 1.05 * steady.max(attack),
                "{name}: {at_steal} at the steal against {steady}"
            );
            // And it is silent soon after.
            assert!(measure::peak(&residual[steal + 400..]) < 1e-9, "{name}");
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(40, 0.9)),
            (1_000, accented(47, 0.7)),
            (7_000, on(52, 0.5)),
            (9_001, off(40)),
            (12_000, on(40, 1.0)),
            (20_000, off(47)),
        ];
        for (name, params) in presets() {
            let run = |block| {
                play(
                    &mut synth_of([PluckedString::new(params), PluckedString::new(params)]),
                    &script,
                    30_000,
                    block,
                )
            };
            let reference = run(256);
            for block in [1, 7, 64, 1_000] {
                assert!(run(block) == reference, "{name} with blocks of {block}");
            }
        }
    }

    #[test]
    fn extreme_settings_and_notes_stay_finite() {
        let extreme = [
            StringParams {
                decay: f64::INFINITY,
                decay_per_octave: f64::NAN,
                high_decay: 1e9,
                release: -1.0,
                pluck_position: 0.0,
                pluck_brightness: f64::INFINITY,
                velocity_darkening: f64::NAN,
                velocity_range_db: -5.0,
                stiffness: 1e9,
                strike: f64::NAN,
                pluck_noise: f64::INFINITY,
                body: [BodyMode::new(f64::NAN, f64::INFINITY, 1e9); 4],
                body_mix: 1e9,
                pickup_position: 1e9,
                tone: 0.0,
                tone_q: f64::NAN,
                buzz: f64::INFINITY,
                buzz_time: 0.0,
                accent_db: 1e9,
                accent_brightness: 1e9,
                accent_buzz: f64::NAN,
                level: f64::NAN,
            },
            StringParams {
                decay: 60.0,
                high_decay: 1.0,
                stiffness: 1.0,
                buzz: 1.0,
                accent_buzz: 1.0,
                accent_db: 24.0,
                level: 4.0,
                body_mix: 1.0,
                tone_q: 4.0,
                body: [BodyMode::new(20.0, 50.0, 4.0); 4],
                ..StringParams::slap_bass()
            },
        ];
        for params in extreme {
            let mut synth = synth_of([PluckedString::new(params), PluckedString::new(params)]);
            let script = [
                (0, accented(0, 1.0)),
                (10, accented(127, 1.0)),
                (20, Act::Bend(48.0)),
                (500, accented(127, 1.0)),
                (900, Act::Bend(-48.0)),
                (4_000, on(60, 0.0)),
                (8_000, off(0)),
            ];
            let signal = play(&mut synth, &script, 24_000, 100);
            assert!(
                measure::finite_within(&signal, 200.0),
                "{}",
                measure::peak(&signal)
            );
        }
    }

    use crate::instruments::measure::Act;
}
