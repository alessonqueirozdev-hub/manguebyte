//! Mallet percussion by modal synthesis: struck bars, tines, plates and
//! tubes.

use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    Handover, Noise, SILENCE, Tone, add_to_every_channel, db, flush, key_hz, setting, t60_factor,
    velocity_gain,
};
use super::modal::Phasors;
use super::string::BodyMode;
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice};

/// How many modes a [`MalletParams`] describes.
pub const MALLET_MODES: usize = 10;

/// The highest frequency a mode is heard at fully, as a fraction of the
/// sample rate; above it modes fade out, and from 0.45 they are left out,
/// so a mode does not vanish all at once from one key to the next.
const FADE_FROM: f64 = 0.3;
const FADE_TO: f64 = 0.45;

/// About how many periods of the note's pitch a contact can last at most.
const MAX_CONTACT_PERIODS: f64 = 0.4;

/// How many samples apart the voice checks whether it has fallen silent.
const CHECK: u32 = 64;

/// One mode of a struck bar, tine, plate or tube.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MalletMode {
    /// Its frequency over the note's, 0.05 to 64. Zero leaves the mode out.
    pub ratio: f64,
    /// Its share of the strike, 0 to 4, before the strike position and the
    /// mallet's contact shape it.
    pub level: f64,
    /// Its decay time over the fundamental's, 0.001 to 4: high modes of a
    /// wooden bar die away much faster than its fundamental.
    pub decay: f64,
}

impl MalletMode {
    /// A mode.
    pub const fn new(ratio: f64, level: f64, decay: f64) -> Self {
        Self {
            ratio,
            level,
            decay,
        }
    }

    /// No mode.
    pub const NONE: Self = Self::new(0.0, 0.0, 1.0);
}

/// What vibrates, which decides how the strike position shares the strike
/// among the modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MalletShape {
    /// A bar free at both ends, held at its nodes: marimba, xylophone,
    /// vibraphone, glockenspiel. The `n`th mode has the shape of the `n`th
    /// bending mode of a uniform free bar, and a strike at its centre
    /// misses the modes with a node there (the second, the fourth...).
    Bar,
    /// A tine clamped at one end and free at the other, as on a kalimba:
    /// the shapes of a uniform cantilever, plucked near the free end.
    Tine,
    /// Anything else (a steel pan's note, a tubular bell): the modes take
    /// the shares they are given, wherever the strike lands.
    Free,
}

/// The settings of a [`MalletVoice`].
///
/// Times are in seconds, frequencies in hertz and levels in decibels
/// unless said otherwise. Out-of-range values are limited when the
/// settings are taken (see [`MalletVoice::set_params`]), and a value that
/// is not a number takes the default of [`marimba`](Self::marimba).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MalletParams {
    /// The modes, as multiples of the note's frequency. The first is the
    /// note itself for every preset but the tubular bell, whose pitch is
    /// heard an octave below its fourth mode.
    pub modes: [MalletMode; MALLET_MODES],
    /// What vibrates; see [`MalletShape`].
    pub shape: MalletShape,
    /// Where the mallet lands, as a fraction of the bar's length from one
    /// end (0.5 is the centre), or of a tine's from its clamp, 0 to 1.
    pub strike_position: f64,
    /// How long the first mode of middle C takes to fall by 60 dB, 0.05 to
    /// 60.
    pub decay: f64,
    /// How many times shorter that decay is an octave higher, 1 to 4.
    pub decay_per_octave: f64,
    /// How long a released note takes to fall by 60 dB, 0.01 to 60: the
    /// damper of a vibraphone, a celesta or tubular bells. A time as long as
    /// the natural decay lets the note ring, as a marimba's does: it has no
    /// damper.
    pub release: f64,
    /// How long the mallet stays on the bar at full velocity, 0.00005 to
    /// 0.02. A harder mallet, and a faster one, leaves sooner, so it
    /// reaches higher modes: the contact's force lasts this long, and modes
    /// above its inverse are excited less.
    pub contact: f64,
    /// How many times longer the contact is at the lowest velocity, 1 to 8:
    /// a yarn mallet played softly stays soft, played hard it reaches its
    /// core.
    pub softness: f64,
    /// How much quieter the lowest velocity is than the highest, 0 to 60.
    pub velocity_range_db: f64,
    /// How loud the resonator tube under the bar is at the fundamental,
    /// as a multiple of the bar's own fundamental, 0 to 8. A tube closed at
    /// its far end resonates at the fundamental and three times it.
    pub tube: f64,
    /// How loud the tube is at three times the fundamental, which on a
    /// xylophone is the bar's second mode, 0 to 8.
    pub tube_third: f64,
    /// The sharpness of the tube's resonance, as its Q, 2 to 200. The tube
    /// takes about `tube_q / (π f)` seconds to build up: the bloom of a
    /// marimba's fundamental.
    pub tube_q: f64,
    /// The speed of the vibraphone's motor, which turns discs in the tops
    /// of the tubes, opening and closing them, in turns a second, 0 to 16.
    /// Zero stops it, open.
    pub motor_rate: f64,
    /// How much of the tube the closed discs silence, 0 to 1.
    pub motor_depth: f64,
    /// How strongly the second mode is driven by the first, 0 to 1: the
    /// nonlinear coupling of a steel pan's note, whose octave mode grows
    /// from its fundamental after the strike, more the harder it is struck.
    pub coupling: f64,
    /// How loud the mallet's own knock is, 0 to 1: a short burst of noise
    /// over the contact.
    pub knock: f64,
    /// The centre of the knock's band.
    pub knock_frequency: f64,
    /// Resonances of a body the bars or tines are mounted on (a kalimba's
    /// box), fed by them and added to them.
    pub body: [BodyMode; 2],
    /// How much louder an accented note is, 0 to 24.
    pub accent_db: f64,
    /// How many times shorter an accented note's contact is, 1 to 4: a
    /// harder stroke.
    pub accent_hardness: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

/// The modes of a uniform bar free at both ends over its first: `(β_n /
/// β_1)²` with `β_n` the roots of `cos β cosh β = 1` (Fletcher and Rossing,
/// "The Physics of Musical Instruments", section 2.15).
const FREE_BAR: [f64; 8] = [
    1.0, 2.756_4, 5.404_0, 8.933_0, 13.344, 18.637, 24.812, 31.870,
];

impl MalletParams {
    /// A marimba: rosewood bars whose arches are cut so that the first
    /// three modes are tuned 1 : 4 : 10, a fundamental, two octaves above
    /// and three octaves and a major third above (N. H. Fletcher and T. D.
    /// Rossing, "The Physics of Musical Instruments", 2nd ed., section 19.3,
    /// and T. D. Rossing, "Science of Percussion Instruments", chapter 7);
    /// yarn mallets; a resonator tube under each bar tuned to its
    /// fundamental, which blooms after the strike. The wood's high modes
    /// die away much sooner than the fundamental. No damper.
    pub fn marimba() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(1.0, 1.0, 1.0),
                MalletMode::new(4.0, 0.7, 0.28),
                MalletMode::new(10.0, 0.45, 0.1),
                MalletMode::new(17.6, 0.15, 0.05),
            ]),
            shape: MalletShape::Bar,
            strike_position: 0.42,
            decay: 1.6,
            decay_per_octave: 1.45,
            release: 60.0,
            contact: 0.0008,
            softness: 2.6,
            velocity_range_db: 26.0,
            tube: 1.6,
            tube_third: 0.0,
            tube_q: 36.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.1,
            knock_frequency: 900.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.6,
            level: 0.45,
        }
    }

    /// A xylophone: harder rosewood bars tuned 1 : 3 : 6, the second mode a
    /// twelfth above the fundamental (Fletcher and Rossing, section 19.3),
    /// hard mallets, short-lived, with tubes whose second resonance, at
    /// three times the fundamental, reinforces the twelfth.
    pub fn xylophone() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(1.0, 1.0, 1.0),
                MalletMode::new(3.0, 0.75, 0.45),
                MalletMode::new(6.0, 0.45, 0.22),
                MalletMode::new(9.7, 0.2, 0.1),
            ]),
            shape: MalletShape::Bar,
            strike_position: 0.4,
            decay: 1.1,
            decay_per_octave: 1.5,
            release: 60.0,
            contact: 0.00035,
            softness: 2.0,
            velocity_range_db: 24.0,
            tube: 0.9,
            tube_third: 0.6,
            tube_q: 30.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.18,
            knock_frequency: 2_500.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.5,
            level: 0.4,
        }
    }

    /// A vibraphone: aluminium bars tuned 1 : 4 : 10 like the marimba's
    /// (Fletcher and Rossing, section 19.4) that ring for many seconds,
    /// yarn mallets, and tubes whose tops the motor's discs open and close,
    /// so the fundamental trembles. The damper bar stops a note when its
    /// key is released; hold the sustain pedal to let notes ring, as the
    /// vibraphone's pedal lifts the damper.
    pub fn vibraphone() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(1.0, 0.7, 1.0),
                MalletMode::new(4.0, 0.9, 0.35),
                MalletMode::new(10.0, 0.5, 0.12),
                MalletMode::new(17.8, 0.12, 0.06),
            ]),
            shape: MalletShape::Bar,
            strike_position: 0.4,
            decay: 7.0,
            decay_per_octave: 1.35,
            release: 0.12,
            contact: 0.00075,
            softness: 3.0,
            velocity_range_db: 26.0,
            tube: 2.2,
            tube_third: 0.0,
            tube_q: 60.0,
            motor_rate: 5.5,
            motor_depth: 0.75,
            coupling: 0.0,
            knock: 0.03,
            knock_frequency: 1_500.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.5,
            level: 0.3,
        }
    }

    /// A glockenspiel: small steel bars of even thickness, whose modes are
    /// those of a uniform free bar, 1 : 2.76 : 5.40 : 8.93, struck with
    /// brass mallets: bright, ringing for seconds. No damper.
    pub fn glockenspiel() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(FREE_BAR[0], 1.0, 1.0),
                MalletMode::new(FREE_BAR[1], 0.8, 0.6),
                MalletMode::new(FREE_BAR[2], 0.5, 0.35),
                MalletMode::new(FREE_BAR[3], 0.3, 0.2),
            ]),
            shape: MalletShape::Bar,
            strike_position: 0.3,
            decay: 9.0,
            decay_per_octave: 1.35,
            release: 60.0,
            contact: 0.00015,
            softness: 2.0,
            velocity_range_db: 24.0,
            tube: 0.0,
            tube_third: 0.0,
            tube_q: 30.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.12,
            knock_frequency: 6_000.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.4,
            level: 0.4,
        }
    }

    /// A celesta: steel bars struck by felt hammers from a keyboard, each
    /// over a wooden box tuned to its fundamental, so the sound is round and
    /// sweet, with a damper that stops a released key.
    pub fn celesta() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(FREE_BAR[0], 1.0, 1.0),
                MalletMode::new(FREE_BAR[1], 0.35, 0.4),
                MalletMode::new(FREE_BAR[2], 0.15, 0.2),
            ]),
            shape: MalletShape::Bar,
            strike_position: 0.3,
            decay: 3.5,
            decay_per_octave: 1.5,
            release: 0.25,
            contact: 0.0012,
            softness: 2.4,
            velocity_range_db: 22.0,
            tube: 1.0,
            tube_third: 0.0,
            tube_q: 20.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.04,
            knock_frequency: 2_000.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.4,
            level: 0.45,
        }
    }

    /// A kalimba (mbira): steel tines clamped over a bridge on a wooden
    /// box, plucked near their free ends by the thumbs. A uniform tine's
    /// modes go 1 : 6.27 : 17.55 (Fletcher and Rossing, section 2.15); the
    /// tine knocks against the bridge as it is let go, and the box adds
    /// its own resonances.
    pub fn kalimba() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(1.0, 1.0, 1.0),
                MalletMode::new(6.267, 0.55, 0.25),
                MalletMode::new(17.547, 0.25, 0.08),
            ]),
            shape: MalletShape::Tine,
            strike_position: 0.9,
            decay: 2.6,
            decay_per_octave: 1.4,
            release: 60.0,
            contact: 0.00035,
            softness: 2.2,
            velocity_range_db: 22.0,
            tube: 0.0,
            tube_third: 0.0,
            tube_q: 30.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.22,
            knock_frequency: 3_200.0,
            body: [
                BodyMode::new(240.0, 6.0, 0.7),
                BodyMode::new(560.0, 5.0, 0.35),
            ],
            accent_db: 3.0,
            accent_hardness: 1.5,
            level: 0.5,
        }
    }

    /// A steel pan (a tenor pan): each note a dome hammered into the drum
    /// whose first modes are tuned 1 : 2 : 3, fundamental, octave and
    /// twelfth (T. D. Rossing, "Science of Percussion Instruments", chapter
    /// 13). The modes are coupled by the dome's nonlinearity: after the
    /// strike the fundamental feeds the octave, which swells, more the
    /// harder the note is struck (A. Achong, "The steelpan as a system of
    /// non-linear mode-localized oscillators", J. Sound Vib. 197, 1996). A
    /// slightly mistuned twin of the fundamental makes it shimmer.
    pub fn steel_pan() -> Self {
        Self {
            modes: modes(&[
                MalletMode::new(1.0, 1.0, 1.0),
                MalletMode::new(2.0, 0.35, 0.7),
                MalletMode::new(3.0, 0.2, 0.45),
                MalletMode::new(1.004, 0.18, 0.8),
                MalletMode::new(4.03, 0.08, 0.3),
                MalletMode::new(5.2, 0.04, 0.2),
            ]),
            shape: MalletShape::Free,
            strike_position: 0.5,
            decay: 2.2,
            decay_per_octave: 1.3,
            release: 60.0,
            contact: 0.0009,
            softness: 2.0,
            velocity_range_db: 24.0,
            tube: 0.0,
            tube_third: 0.0,
            tube_q: 30.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.6,
            knock: 0.08,
            knock_frequency: 1_800.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.5,
            level: 0.45,
        }
    }

    /// Tubular bells (chimes): long brass tubes struck at the top. Their
    /// modes are a free bar's, and the pitch heard, the strike tone, is an
    /// octave below the fourth mode, because the fourth, fifth and sixth go
    /// nearly 2 : 3 : 4 (Fletcher and Rossing, section 19.6); the lowest
    /// modes, the hum, hardly radiate. Long ringing, with a damper pedal
    /// that stops a released note.
    pub fn tubular_bells() -> Self {
        let mode = |n: usize, level: f64, decay: f64| {
            MalletMode::new(2.0 * FREE_BAR[n] / FREE_BAR[3], level, decay)
        };
        Self {
            modes: modes(&[
                mode(0, 0.05, 2.0),
                mode(1, 0.15, 1.6),
                mode(2, 0.45, 1.3),
                mode(3, 1.0, 1.0),
                mode(4, 0.85, 0.75),
                mode(5, 0.65, 0.55),
                mode(6, 0.4, 0.4),
                mode(7, 0.22, 0.28),
            ]),
            shape: MalletShape::Free,
            strike_position: 0.0,
            decay: 9.0,
            decay_per_octave: 1.3,
            release: 0.5,
            contact: 0.0004,
            softness: 2.2,
            velocity_range_db: 24.0,
            tube: 0.0,
            tube_third: 0.0,
            tube_q: 30.0,
            motor_rate: 0.0,
            motor_depth: 0.0,
            coupling: 0.0,
            knock: 0.05,
            knock_frequency: 4_000.0,
            body: [BodyMode::NONE; 2],
            accent_db: 3.0,
            accent_hardness: 1.4,
            level: 0.3,
        }
    }

    /// The settings with every value limited to its range.
    fn limited(self) -> Self {
        let default = Self::marimba();
        let body_default = BodyMode::NONE;
        Self {
            modes: self.modes.map(|mode| MalletMode {
                ratio: if mode.ratio > 0.0 {
                    setting(mode.ratio, 0.05, 64.0, 0.0)
                } else {
                    0.0
                },
                level: setting(mode.level, 0.0, 4.0, 0.0),
                decay: setting(mode.decay, 0.001, 4.0, 1.0),
            }),
            shape: self.shape,
            strike_position: setting(self.strike_position, 0.0, 1.0, default.strike_position),
            decay: setting(self.decay, 0.05, 60.0, default.decay),
            decay_per_octave: setting(self.decay_per_octave, 1.0, 4.0, default.decay_per_octave),
            release: setting(self.release, 0.01, 60.0, default.release),
            contact: setting(self.contact, 0.000_05, 0.02, default.contact),
            softness: setting(self.softness, 1.0, 8.0, default.softness),
            velocity_range_db: setting(
                self.velocity_range_db,
                0.0,
                60.0,
                default.velocity_range_db,
            ),
            tube: setting(self.tube, 0.0, 8.0, 0.0),
            tube_third: setting(self.tube_third, 0.0, 8.0, 0.0),
            tube_q: setting(self.tube_q, 2.0, 200.0, default.tube_q),
            motor_rate: setting(self.motor_rate, 0.0, 16.0, 0.0),
            motor_depth: setting(self.motor_depth, 0.0, 1.0, 0.0),
            coupling: setting(self.coupling, 0.0, 1.0, 0.0),
            knock: setting(self.knock, 0.0, 1.0, default.knock),
            knock_frequency: setting(self.knock_frequency, 50.0, 16_000.0, 1_000.0),
            body: self.body.map(|mode| BodyMode {
                frequency: setting(mode.frequency, 20.0, 10_000.0, body_default.frequency),
                q: setting(mode.q, 0.5, 50.0, body_default.q),
                gain: setting(mode.gain, 0.0, 4.0, 0.0),
            }),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            accent_hardness: setting(self.accent_hardness, 1.0, 4.0, default.accent_hardness),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    /// The ratio of the mode heard as the note's pitch: 1 if a mode is
    /// there, or else 2 if a mode is there (the strike tone of a tubular
    /// bell is an octave below its fourth mode), or else the first mode's.
    fn main_ratio(&self) -> f64 {
        let near = |target: f64| {
            self.modes
                .iter()
                .any(|mode| mode.level > 0.0 && (mode.ratio - target).abs() < 1e-3)
        };
        if near(1.0) {
            1.0
        } else if near(2.0) {
            2.0
        } else {
            self.modes
                .iter()
                .find(|mode| mode.ratio > 0.0 && mode.level > 0.0)
                .map_or(1.0, |mode| mode.ratio)
        }
    }

    /// The decay time of the first mode of a note `f0` hertz high.
    fn decay_at(&self, f0: f64) -> f64 {
        let octaves = (f0 / key_hz(60.0)).log2();
        self.decay / self.decay_per_octave.powf(octaves)
    }
}

impl Default for MalletParams {
    fn default() -> Self {
        Self::marimba()
    }
}

/// A list of modes padded with none.
fn modes(list: &[MalletMode]) -> [MalletMode; MALLET_MODES] {
    let mut all = [MalletMode::NONE; MALLET_MODES];
    for (slot, mode) in all.iter_mut().zip(list) {
        *slot = *mode;
    }
    all
}

/// `β` of the `n`th bending mode (from 0) of a uniform free bar, or of a
/// cantilever with `clamped`: the roots of `cos β cosh β = 1` and of `cos β
/// cosh β = -1`, then their asymptotes (Fletcher and Rossing, section
/// 2.15).
fn beam_root(n: usize, clamped: bool) -> f64 {
    const FREE: [f64; 5] = [4.730_041, 7.853_205, 10.995_608, 14.137_165, 17.278_760];
    const CLAMPED: [f64; 4] = [1.875_104, 4.694_091, 7.854_757, 10.995_541];
    let half_pi = core::f64::consts::FRAC_PI_2;
    if clamped {
        CLAMPED
            .get(n)
            .copied()
            .unwrap_or((2 * n + 1) as f64 * half_pi)
    } else {
        FREE.get(n).copied().unwrap_or((2 * n + 3) as f64 * half_pi)
    }
}

/// How much of a strike at `x` (a fraction of the length) the `n`th
/// bending mode takes: its shape there over its shape at the free end, 0
/// to about 1.
///
/// The shapes are those of the Euler-Bernoulli beam, free at both ends or
/// clamped at `x = 0` (Fletcher and Rossing, section 2.15). Written with
/// the growing exponential's small coefficient worked out first, they lose
/// nothing to cancellation however high the mode.
fn beam_share(n: usize, x: f64, clamped: bool) -> f64 {
    let beta = beam_root(n, clamped);
    let (sin, cos) = beta.sin_cos();
    let decaying = (-beta).exp();
    let (sinh, cosh) = (beta.sinh(), beta.cosh());
    let (sigma, one_minus_sigma) = if clamped {
        let denominator = sinh + sin;
        (
            (cosh + cos) / denominator,
            (sin - cos - decaying) / denominator,
        )
    } else {
        let denominator = sinh - sin;
        (
            (cosh - cos) / denominator,
            (cos - sin - decaying) / denominator,
        )
    };
    let bx = beta * x;
    let hyperbolic = 0.5 * (one_minus_sigma * bx.exp() + (1.0 + sigma) * (-bx).exp());
    let (sin_x, cos_x) = bx.sin_cos();
    let shape = if clamped {
        hyperbolic - cos_x + sigma * sin_x
    } else {
        hyperbolic + cos_x - sigma * sin_x
    };
    (0.5 * shape).abs()
}

/// A single resonance as a complex one-pole, ringing in phase with what
/// drives it: the tube under a bar, or a mode driven by another.
#[derive(Debug, Clone, Copy, Default)]
struct Resonance {
    pr: f64,
    pi: f64,
    zr: f64,
    zi: f64,
    /// The input gain that makes a sine at the resonance come out at the
    /// level asked for, once built up.
    gain: f64,
}

impl Resonance {
    /// Tunes to `hertz` with a decay of `t60` seconds, built up to `level`
    /// times a sine at the resonance that drives it and itself decays in
    /// `drive_t60` seconds. Above 0.45 of the sample rate it is silent.
    ///
    /// Driven by `A ρⁿ sin(nω)`, the resonance `z ← r e^{iω} z + g x` takes
    /// the half of the sine that turns its way and settles to `g A ρⁿ / (2
    /// (1 - r / ρ))`: a resonance that dies faster than its drive follows
    /// it, a little louder the closer the two decays are.
    fn tune(&mut self, hertz: f64, t60: f64, level: f64, drive_t60: f64, rate: f64) {
        let r = t60_factor(t60, rate);
        let rho = t60_factor(drive_t60, rate).max(1e-12);
        let (sin, cos) = (TAU * hertz / rate).sin_cos();
        (self.pr, self.pi) = (r * cos, r * sin);
        // A resonance that outlasts its drive never settles: it is given
        // the gain of one that rings twice as long as it does.
        self.gain = if hertz < 0.45 * rate {
            2.0 * (1.0 - r / rho).max(0.5 * (1.0 - r)) * level
        } else {
            0.0
        };
    }

    /// The resonance retuned by `factor`, keeping its state and decay.
    fn scale(&mut self, factor: f64) {
        let r = self.pr.hypot(self.pi);
        let w = self.pi.atan2(self.pr) * factor;
        if w >= 0.9 * core::f64::consts::PI {
            self.gain = 0.0;
            self.zr = 0.0;
            self.zi = 0.0;
        }
        let (sin, cos) = w.min(0.9 * core::f64::consts::PI).sin_cos();
        (self.pr, self.pi) = (r * cos, r * sin);
    }

    /// One sample driven by `x`; the component in phase with a sine that
    /// drives it at the resonance.
    #[inline]
    fn next(&mut self, x: f64) -> f64 {
        let re = flush(self.pr * self.zr - self.pi * self.zi + self.gain * x);
        let im = flush(self.pr * self.zi + self.pi * self.zr);
        self.zr = re;
        self.zi = im;
        re
    }

    /// Shortens the decay to at most `t60` seconds from now on.
    fn limit_decay(&mut self, t60: f64, rate: f64) {
        let r = self.pr.hypot(self.pi);
        let limit = t60_factor(t60, rate);
        if limit < r {
            let scale = limit / r;
            self.pr *= scale;
            self.pi *= scale;
        }
    }

    fn rest(&mut self) {
        self.zr = 0.0;
        self.zi = 0.0;
    }
}

/// One note: the modes and everything that colours them.
#[derive(Debug, Clone)]
struct MalletTone {
    rate: f64,
    bar: Phasors,
    pulse: Contact,
    /// The strike's own noise, and how loud it is per unit of force.
    knock: Noise,
    knock_gain: f64,
    knock_filter: StateVariableFilter<f64>,
    /// How many samples the knock's filter still rings for.
    knock_left: u32,
    knock_tail: f64,
    tube: [Resonance; 2],
    /// The mode the first one drives, and how strongly.
    coupled: Resonance,
    coupling: f64,
    body: [StateVariableFilter<f64>; 2],
    body_gain: [f64; 2],
    /// The motor's phase in turns and its step, and the depth.
    motor: f64,
    motor_step: f64,
    motor_depth: f64,
    /// A peak follower of the output, to know when the note is silent.
    follower: f64,
    follower_fall: f64,
    check_left: u32,
    /// Whether the note is damped.
    damped: bool,
    /// The settings and the frequency the modes were laid out for.
    laid: MalletParams,
    laid_f0: f64,
    /// The frequency ratio the modes are tuned to now, from bends.
    tuned: f64,
    /// The output level asked for, and the one heard, following it over a
    /// few milliseconds so that a change does not click.
    level: f64,
    level_now: f64,
    level_follow: f64,
    sounding: bool,
}

impl MalletTone {
    fn new() -> Self {
        let band = || StateVariableFilter::new(SvfMode::Bandpass, 1_000.0, 1.0);
        Self {
            rate: 48_000.0,
            bar: Phasors::default(),
            pulse: Contact::default(),
            knock: Noise::for_note(0, 0.0, 0),
            knock_gain: 0.0,
            knock_filter: band(),
            knock_left: 0,
            knock_tail: 0.0,
            tube: [Resonance::default(); 2],
            coupled: Resonance::default(),
            coupling: 0.0,
            body: [band(), band()],
            body_gain: [0.0; 2],
            motor: 0.0,
            motor_step: 0.0,
            motor_depth: 0.0,
            follower: 0.0,
            follower_fall: 0.0,
            check_left: CHECK,
            damped: false,
            laid: MalletParams::marimba(),
            laid_f0: 0.0,
            tuned: 1.0,
            level: 0.0,
            level_now: 0.0,
            level_follow: 1.0,
            sounding: false,
        }
    }

    fn prepare(&mut self, rate: f64) {
        self.rate = rate;
        let spec = ProcessSpec {
            sample_rate: rate,
            max_block_size: 1,
            num_channels: 1,
        };
        for filter in self.body.iter_mut().chain([&mut self.knock_filter]) {
            Processor::<f64>::prepare(filter, &spec);
        }
        self.follower_fall = t60_factor(0.3, rate);
        self.level_follow = 1.0 - (-1.0 / (0.005 * rate)).exp();
        self.silence();
    }

    /// Takes the settings that apply at once: the body, the motor and the
    /// level.
    fn configure(&mut self, params: &MalletParams) {
        for ((filter, gain), mode) in self
            .body
            .iter_mut()
            .zip(&mut self.body_gain)
            .zip(&params.body)
        {
            filter.set_frequency(mode.frequency.min(0.45 * self.rate));
            filter.set_q(mode.q);
            *gain = mode.gain;
        }
        self.knock_filter
            .set_frequency(params.knock_frequency.min(0.45 * self.rate));
        self.knock_filter.set_q(KNOCK_Q);
        // Thirty of the band's time constants, Q / (π f): the knock has
        // fallen by 260 dB.
        let knock = params.knock_frequency.min(0.45 * self.rate);
        self.knock_tail = (30.0 * KNOCK_Q / (core::f64::consts::PI * knock)).max(0.01);
        self.motor_step = params.motor_rate / self.rate;
        self.motor_depth = params.motor_depth;
        self.level = params.level;
    }

    /// Whether a strike of `params` at `f0` hertz can land on what this
    /// note is ringing with: the same note, laid out by the same settings,
    /// and not damped.
    fn can_strike_again(&self, params: &MalletParams, f0: f64) -> bool {
        self.sounding && !self.damped && self.laid == *params && self.laid_f0 == f0
    }

    /// Strikes the note: `f0` its frequency, `bend` the ratio it is bent
    /// by, `gain` the strike's level and `contact` its duration. A note
    /// still ringing (see [`can_strike_again`](Self::can_strike_again)) is
    /// struck again on top of what it is doing.
    fn strike(
        &mut self,
        params: &MalletParams,
        f0: f64,
        bend: f64,
        gain: f64,
        contact: f64,
        seed: Noise,
    ) {
        let rate = self.rate;
        if !self.sounding {
            self.bar.clear();
            self.tuned = bend;
            self.layout(params, f0 * bend, params.decay_at(f0));
            self.laid = *params;
            self.laid_f0 = f0;
        }
        self.damped = false;

        // The share of the strike each mode takes: its level, its shape
        // where the mallet lands, and a fade towards the top of the band,
        // in the order `layout` added the modes.
        let mut index = 0;
        let mut first = 0.0;
        let inputs = self.bar.inputs_mut();
        for (n, mode) in params.modes.iter().enumerate() {
            let hertz = f0 * bend * mode.ratio;
            if mode.ratio <= 0.0 || mode.level <= 0.0 || hertz >= FADE_TO * rate {
                continue;
            }
            let share = match params.shape {
                MalletShape::Bar => beam_share(n, params.strike_position, false),
                MalletShape::Tine => beam_share(n, params.strike_position, true),
                MalletShape::Free => 1.0,
            };
            let level = mode.level * share * fade(hertz / rate);
            if let Some(input) = inputs.get_mut(index) {
                *input = level * gain;
            }
            if index == 0 {
                first = level;
            }
            index += 1;
        }
        self.pulse.start(contact, rate);
        // The knock: noise over the contact, of the same level whatever
        // its length.
        self.knock = seed;
        self.knock_gain = params.knock * gain * (0.001 / contact).sqrt();
        self.knock_left = ((contact + self.knock_tail) * rate).ceil() as u32;
        // The coupled mode grows to `coupling` times the first mode's level
        // in a tenth of a second when the note is struck at full velocity,
        // and by the square of the strike's level otherwise: the square of
        // a sine of amplitude `A` drives twice its frequency with `A² / 4`.
        self.coupling = if first > 0.0 {
            params.coupling * 4.0 / (first * 0.1 * rate)
        } else {
            0.0
        };
        self.sounding = true;
        self.check_left = CHECK;
    }

    /// Adds the modes, the tube and the coupled mode for a note of `f0`
    /// hertz, bent already, whose first mode decays in `decay` seconds.
    fn layout(&mut self, params: &MalletParams, f0: f64, decay: f64) {
        let rate = self.rate;
        for mode in &params.modes {
            if mode.ratio <= 0.0 || mode.level <= 0.0 || f0 * mode.ratio >= FADE_TO * rate {
                continue;
            }
            // The first mode is also the bank's other output: the motion
            // that drives the coupled mode.
            let other = if self.bar.len() == 0 { 1.0 } else { 0.0 };
            self.bar
                .add(f0 * mode.ratio, decay * mode.decay, 1.0, other, rate);
        }
        // A tube closed at its far end, a quarter of a wavelength long,
        // resonates at the fundamental and at three times it, with the same
        // Q: T60 = Q ln(1000) / (π f).
        let tube_decay = 6.907_755 * params.tube_q / (core::f64::consts::PI * f0);
        let (first, second) = (params.modes[0], params.modes[1]);
        self.tube[0].tune(f0, tube_decay, params.tube, decay * first.decay, rate);
        self.tube[1].tune(
            3.0 * f0,
            tube_decay / 3.0,
            params.tube_third,
            decay * second.decay,
            rate,
        );
        let t60 = decay * second.decay;
        self.coupled.tune(f0 * second.ratio, t60, 1.0, t60, rate);
        // The strike sets how strongly the coupled mode is driven; its own
        // gain only says whether it sounds.
        self.coupled.gain = if second.ratio > 0.0 && f0 * second.ratio < FADE_TO * rate {
            1.0
        } else {
            0.0
        };
        self.motor = 0.0;
    }

    /// The key is released: a damper, if the instrument has one, stops the
    /// note.
    fn damp(&mut self) {
        if self.damped || !self.sounding {
            return;
        }
        self.damped = true;
        let release = self.laid.release;
        self.bar.limit_decay(release, self.rate);
        self.coupled.limit_decay(release, self.rate);
    }

    /// Retunes the sounding note by `bend`, a frequency ratio.
    fn bend(&mut self, bend: f64) {
        if !self.sounding || bend.is_nan() || bend <= 0.0 {
            return;
        }
        let factor = bend / self.tuned;
        self.tuned = bend;
        self.bar.scale_frequencies(factor);
        for resonance in self.tube.iter_mut().chain([&mut self.coupled]) {
            resonance.scale(factor);
        }
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let force = self.pulse.next();
        let (mut out, motion) = self.bar.next(force);
        if self.coupling > 0.0 && self.coupled.gain > 0.0 {
            out += self.coupled.next(self.coupling * motion * motion);
        }
        // The knock rings out and its filter is then set to rest: left to
        // run on silence its state would fall into subnormal numbers, which
        // cost a processor many times more than normal ones, for as long as
        // the note rings.
        if self.knock_left > 0 {
            self.knock_left -= 1;
            let burst = if self.knock_gain > 0.0 && force > 0.0 {
                self.knock.next() * force * self.knock_gain
            } else {
                0.0
            };
            out += self.knock_filter.process_sample(0, burst);
            if self.knock_left == 0 {
                Processor::<f64>::reset(&mut self.knock_filter);
            }
        }
        // The tube, opened and closed by the motor's discs.
        let tube = self.tube[0].next(out) + self.tube[1].next(out);
        let gate = if self.motor_step > 0.0 {
            self.motor += self.motor_step;
            if self.motor >= 1.0 {
                self.motor -= 1.0;
            }
            1.0 - self.motor_depth * (0.5 - 0.5 * (TAU * self.motor).cos())
        } else {
            1.0
        };
        out += tube * gate;
        let mut body = 0.0;
        for (filter, gain) in self.body.iter_mut().zip(&self.body_gain) {
            if *gain > 0.0 {
                body += gain * filter.process_sample(0, out);
            }
        }
        self.level_now += (self.level - self.level_now) * self.level_follow;
        let out = (out + body) * self.level_now;
        self.follower = (self.follower * self.follower_fall).max(out.abs());
        self.check_left -= 1;
        if self.check_left == 0 {
            self.check_left = CHECK;
            self.bar.flush_quiet();
            if !self.pulse.is_running() && self.follower < SILENCE {
                self.silence();
            }
        }
        out
    }
}

/// The force of a mallet on a bar over its contact: a smooth bump, a
/// Gaussian whose standard deviation is a sixth of the contact's length,
/// cut at three of them each side, its samples adding up to 1.
///
/// The force of two elastic bodies is half a sine to the power 3/2 (Hertz;
/// [`Pulse`](super::modal::Pulse)), whose spectrum has zeros: a mode that
/// falls on one is not struck at all, and from one velocity to the next a
/// mode moves in and out of them, so a harder stroke can sound darker. The
/// felt and yarn of mallets stiffen as they are squeezed (their force grows
/// as a power of the compression well above 1, as for piano hammers: A.
/// Stulov, "Hysteretic model of the grand piano hammer felt", JASA 97,
/// 1995), which rounds the pulse; a Gaussian is that rounding taken all the
/// way, and its spectrum falls smoothly with no zeros, so every mode is
/// struck less the longer the contact.
#[derive(Debug, Clone, Copy, Default)]
struct Contact {
    left: u32,
    len: u32,
    scale: f64,
}

impl Contact {
    fn start(&mut self, seconds: f64, rate: f64) {
        let len = (seconds * rate).round().clamp(1.0, 4_096.0) as u32;
        let mut sum = 0.0;
        for k in 0..len {
            sum += Self::shape(k, len);
        }
        self.len = len;
        self.left = len;
        self.scale = 1.0 / sum;
    }

    #[inline]
    fn shape(k: u32, len: u32) -> f64 {
        let x = 6.0 * ((f64::from(k) + 0.5) / f64::from(len) - 0.5);
        (-0.5 * x * x).exp()
    }

    fn is_running(&self) -> bool {
        self.left > 0
    }

    #[inline]
    fn next(&mut self) -> f64 {
        if self.left == 0 {
            return 0.0;
        }
        let k = self.len - self.left;
        self.left -= 1;
        self.scale * Self::shape(k, self.len)
    }

    fn stop(&mut self) {
        self.left = 0;
    }
}

/// How much of a strike's force a mode at `hertz` takes, 0 to 1, when the
/// contact lasts `seconds`: the magnitude of the [`Contact`]'s spectrum
/// there, `exp(-(2π f σ)² / 2)` with `σ` a sixth of the contact (the cut
/// ends change it by less than a percent).
fn contact_response(seconds: f64, hertz: f64) -> f64 {
    let sigma = seconds / 6.0;
    let x = TAU * hertz * sigma;
    (-0.5 * x * x).exp()
}

/// The Q of the band the mallet's knock is heard through.
const KNOCK_Q: f64 = 1.2;

/// How much of a mode at `ratio` of the sample rate is heard.
fn fade(ratio: f64) -> f64 {
    if ratio <= FADE_FROM {
        1.0
    } else if ratio >= FADE_TO {
        0.0
    } else {
        let t = (FADE_TO - ratio) / (FADE_TO - FADE_FROM);
        t * t * (3.0 - 2.0 * t)
    }
}

impl Tone for MalletTone {
    fn is_sounding(&self) -> bool {
        self.sounding
    }

    fn silence(&mut self) {
        self.bar.clear();
        self.pulse.stop();
        self.knock_gain = 0.0;
        self.knock_left = 0;
        for resonance in self.tube.iter_mut().chain([&mut self.coupled]) {
            resonance.rest();
        }
        for filter in self.body.iter_mut().chain([&mut self.knock_filter]) {
            Processor::<f64>::reset(filter);
        }
        self.coupling = 0.0;
        self.motor = 0.0;
        self.follower = 0.0;
        self.check_left = CHECK;
        self.damped = false;
        self.laid_f0 = 0.0;
        self.tuned = 1.0;
        self.level_now = self.level;
        self.sounding = false;
    }
}

/// Mallet percussion by modal synthesis: a voice for
/// [`Synth`](crate::Synth) that plays marimba, xylophone, vibraphone,
/// glockenspiel, celesta, kalimba, steel pan and tubular bells.
///
/// A struck bar rings in each of its modes, each a decaying sine with its
/// own frequency and decay, with an amplitude set by where it is struck and
/// by how long the mallet stays on it (J.-M. Adrien, "The missing link:
/// modal synthesis", 1991). Each mode is a complex one-pole resonator,
/// whose frequency is exactly the mode's:
///
/// ```text
///  mallet: contact force (a smooth bump)               ---+--> modes (shares by strike position) --+--> tube (motor) --+
///          knock noise over the contact --------------------+                                        |                  +--> body --> out
///                                                           first mode squared --> coupled mode -----+------------------+
/// ```
///
/// - **The modes** ([`MalletParams::modes`]) are tuned as each instrument
///   is: a marimba's bar to 1 : 4 : 10, a xylophone's to 1 : 3 : 6, a
///   glockenspiel's as a uniform bar, a kalimba's tine as a cantilever, a
///   steel pan's note to 1 : 2 : 3. Each decays in its own time: the high
///   modes of wood die away within a fraction of the fundamental's decay.
///   Lower notes ring longer ([`MalletParams::decay_per_octave`]).
/// - **The strike position** shares the strike among the modes by their
///   shapes there ([`MalletShape`]): a bar struck at its centre misses its
///   second mode, which has a node there.
/// - **The mallet** is the force of its contact, a pulse as long as the
///   contact lasts ([`MalletParams::contact`]). A soft or slow mallet stays
///   longer and excites the high modes less: velocity shortens the contact
///   as well as raising the level, so a note played harder is brighter, as
///   a yarn mallet is when it reaches its core.
/// - **The resonator tube** under a marimba's, a vibraphone's or a
///   xylophone's bar is a resonance at the fundamental (and at three times
///   it) driven by the bar, which builds up after the strike and blooms.
///   **The vibraphone's motor** opens and closes the tubes, so the
///   fundamental trembles; every note's motor starts open when it is
///   struck.
/// - **The steel pan's coupling** feeds the octave from the square of the
///   fundamental, so the octave swells after the strike, more for a harder
///   one.
///
/// Releasing the key damps the note for an instrument with a damper
/// ([`MalletParams::release`]); the sustain pedal holds it, as a
/// vibraphone's pedal lifts its damper bar. Striking a sounding key again
/// adds the new strike to what rings, as a bar does. A voice that is taken
/// over for a new note lets the old one fade over
/// [`HANDOVER_SECONDS`](super::HANDOVER_SECONDS) while the new one starts.
/// Pitch bend and [`Expression::Tuning`] retune every mode as it rings.
///
/// An accent is louder ([`MalletParams::accent_db`]) and harder
/// ([`MalletParams::accent_hardness`]). [`Expression::Brightness`] makes the
/// next strikes harder or softer (0.5 leaves them as set), and
/// [`Expression::Vibrato`] scales the motor's depth. The knock is seeded
/// by the key and the velocity, so a note sounds the same every time.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{MalletParams, MalletVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut marimba = Synth::with_voices(16, || MalletVoice::new(MalletParams::marimba()));
/// marimba.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [48, 55, 64] {
///     marimba.note_on(NoteRef::new(0, key), 0.8);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// marimba.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct MalletVoice {
    params: MalletParams,
    tones: Handover<MalletTone>,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
    /// [`Expression::Brightness`], 0 to 1, and [`Expression::Volume`].
    brightness: f64,
    volume: f64,
    vibrato: f64,
    key: u8,
}

impl MalletVoice {
    /// A voice with these settings. Until it is prepared it assumes 48 kHz.
    pub fn new(params: MalletParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            tones: Handover::new(MalletTone::new(), MalletTone::new()),
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
            brightness: 0.5,
            volume: 1.0,
            vibrato: 1.0,
            key: 60,
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note keeps its modes and its tube, and takes the new body,
    /// motor and level at once; the next strike takes everything.
    pub fn set_params(&mut self, params: MalletParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &MalletParams {
        &self.params
    }

    fn configure(&mut self) {
        let mut params = self.params;
        params.motor_depth *= self.vibrato;
        params.motor_depth = params.motor_depth.min(1.0);
        params.level *= self.volume;
        self.tones.now.configure(&params);
        self.tones.old.configure(&params);
    }

    fn ratio(&self) -> f64 {
        ((self.bend + self.tuning) / 12.0).exp2()
    }
}

impl<S: Sample> Voice<S> for MalletVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.tones.prepare(spec.sample_rate);
        self.tones.now.prepare(spec.sample_rate);
        self.tones.old.prepare(spec.sample_rate);
        self.configure();
    }

    fn set_tuning(&mut self, tuning: &Tuning) {
        self.key_tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let params = self.params;
        self.key = note.key.min(127);
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let mut gain = velocity_gain(velocity, params.velocity_range_db);
        let mut contact = params.contact * params.softness.powf(1.0 - velocity);
        // Brightness 0 doubles the contact, 1 halves it.
        contact *= (1.0 - 2.0 * self.brightness).exp2();
        if accent {
            gain *= db(params.accent_db);
            contact /= params.accent_hardness;
        }
        let seed = Noise::for_note(self.key, velocity, 21);
        let f0 = self.key_tuning.key_frequency(self.key);
        let ratio = self.ratio();
        // A bar throws the mallet off within about half a period of the
        // mode it sounds most, so a contact much longer than that is
        // shortened (to the geometric mean of the two), however soft the
        // mallet: a soft stroke on a high bar is not muffled away, and is
        // still softer than a hard one.
        let main = f0 * ratio * params.main_ratio();
        contact /= (1.0 + (contact * main / MAX_CONTACT_PERIODS).powi(2)).powf(0.25);
        // The level the velocity and the accent ask for is the level of the
        // pitch heard: what a long contact still takes from it is made up
        // for (by at most 12 dB), so that the contact changes the timbre and
        // not the level.
        let response = contact_response(contact, main);
        gain /= response.max(0.25);
        if self.tones.now.is_sounding() && !self.tones.now.can_strike_again(&params, f0) {
            // A damped note, or one laid out by other settings, cannot take
            // the strike on what it rings with: it fades out alongside the
            // new strike, as a note taken over does.
            self.tones.choke();
        }
        self.tones
            .now
            .strike(&params, f0, ratio, gain, contact, seed);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.tones.now.damp();
    }

    fn choke(&mut self) {
        self.tones.choke();
        self.tuning = 0.0;
        self.brightness = 0.5;
        if self.volume != 1.0 || self.vibrato != 1.0 {
            self.volume = 1.0;
            self.vibrato = 1.0;
            self.configure();
        }
    }

    fn is_active(&self) -> bool {
        self.tones.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        if !self.tones.is_active() {
            return;
        }
        let tones = &mut self.tones;
        add_to_every_channel(buffer, || tones.next(MalletTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if !value.is_finite() {
            return;
        }
        match expression {
            Expression::Tuning => {
                self.tuning = value.clamp(-48.0, 48.0);
                let ratio = self.ratio();
                self.tones.now.bend(ratio);
            }
            Expression::Brightness => self.brightness = value.clamp(0.0, 1.0),
            Expression::Volume => {
                self.volume = value.clamp(0.0, 4.0);
                self.configure();
            }
            Expression::Vibrato => {
                self.vibrato = 2.0 * value.clamp(0.0, 1.0);
                self.configure();
            }
            _ => {}
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            let ratio = self.ratio();
            self.tones.now.bend(ratio);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::measure::{
        self, Act, RATE, accented, amplitude_at, between, centroid, cents, decibels,
        frequency_near, largest_jump, note, off, on, play, rms, synth_of, t60_at,
    };

    fn presets() -> [(&'static str, MalletParams, [u8; 3]); 8] {
        [
            ("marimba", MalletParams::marimba(), [36, 60, 90]),
            ("xylophone", MalletParams::xylophone(), [65, 79, 96]),
            ("vibraphone", MalletParams::vibraphone(), [53, 67, 89]),
            ("glockenspiel", MalletParams::glockenspiel(), [79, 91, 103]),
            ("celesta", MalletParams::celesta(), [60, 79, 100]),
            ("kalimba", MalletParams::kalimba(), [60, 72, 88]),
            ("steel pan", MalletParams::steel_pan(), [60, 72, 86]),
            ("tubular bells", MalletParams::tubular_bells(), [60, 67, 77]),
        ]
    }

    fn voice(params: MalletParams) -> [MalletVoice; 1] {
        [MalletVoice::new(params)]
    }

    /// Without the tube, the body and the coupling, which ring at
    /// frequencies of their own near the modes'.
    fn bare(params: MalletParams) -> MalletParams {
        MalletParams {
            tube: 0.0,
            tube_third: 0.0,
            coupling: 0.0,
            body: [BodyMode::NONE; 2],
            knock: 0.0,
            ..params
        }
    }

    #[test]
    fn the_beam_shapes_have_their_nodes_where_they_should() {
        // A free bar's first mode has its nodes at 0.224 of its length from
        // each end, its second at 0.132 and the centre; a cantilever's first
        // mode is still at the clamp and swings most at the free end.
        assert!(beam_share(0, 0.2242, false) < 1e-3);
        assert!(beam_share(1, 0.1321, false) < 1e-3);
        assert!(beam_share(1, 0.5, false) < 1e-5);
        assert!(beam_share(3, 0.5, false) < 1e-5);
        assert!((beam_share(0, 0.0, false) - 1.0).abs() < 1e-9);
        assert!((beam_share(7, 0.0, false) - 1.0).abs() < 1e-6);
        assert!(beam_share(0, 0.0, true) < 1e-12);
        for n in 0..8 {
            assert!((beam_share(n, 1.0, true) - 1.0).abs() < 1e-4, "{n}");
            // Symmetric or antisymmetric about the centre.
            let (a, b) = (beam_share(n, 0.3, false), beam_share(n, 0.7, false));
            assert!((a - b).abs() < 1e-6, "{n}");
        }
        // The second free mode over the first is the uniform bar's 2.756.
        let ratio = (beam_root(1, false) / beam_root(0, false)).powi(2);
        assert!((ratio - FREE_BAR[1]).abs() < 1e-3);
        let ratio = (beam_root(1, true) / beam_root(0, true)).powi(2);
        assert!((ratio - 6.267).abs() < 1e-3);
    }

    #[test]
    fn every_mode_rings_within_half_a_percent_of_its_model() {
        let mut worst = 0.0f64;
        for (name, params, keys) in presets() {
            let params = bare(params);
            for key in keys {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(params), on(key, 1.0), 2.0, 1.2);
                for (n, mode) in params.modes.iter().enumerate() {
                    let hertz = f0 * mode.ratio;
                    if mode.ratio <= 0.0 || hertz > FADE_FROM * RATE {
                        continue;
                    }
                    // Modes nearer each other than a few window widths
                    // cannot be told apart: the steel pan's twin.
                    let crowded = params.modes.iter().enumerate().any(|(m, other)| {
                        m != n && other.ratio > 0.0 && (other.ratio / mode.ratio - 1.0).abs() < 0.05
                    });
                    if crowded {
                        continue;
                    }
                    // Over a window that the mode is still loud in.
                    let t60 = params.decay_at(f0) * mode.decay;
                    let length = (0.25 * t60).clamp(0.05, 0.8);
                    let measured = frequency_near(between(&signal, 0.01, 0.01 + length), hertz);
                    let error = 100.0 * (measured / hertz - 1.0);
                    worst = worst.max(error.abs());
                    assert!(
                        error.abs() < 0.5,
                        "{name} key {key} mode {n}: {measured:.2} Hz against {hertz:.2} ({error:.3}%)"
                    );
                }
            }
        }
        println!("worst mode error {worst:.4}%");
    }

    #[test]
    fn every_note_is_in_tune_across_its_range() {
        let mut worst = 0.0f64;
        for (name, params, keys) in presets() {
            // Without the steel pan's twin, which beats with the
            // fundamental.
            let mut params = bare(params);
            for mode in &mut params.modes {
                if mode.ratio != 1.0 && (mode.ratio - 1.0).abs() < 0.05 {
                    *mode = MalletMode::NONE;
                }
            }
            // The tubular bell's pitch is an octave below its fourth mode.
            let (index, ratio) = if name == "tubular bells" {
                (3, 2.0)
            } else {
                (0, 1.0)
            };
            for key in keys.into_iter().chain([keys[0] + 5, keys[1] + 7]) {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(params), on(key, 0.8), 2.0, 1.0);
                let t60 = params.decay_at(f0) * params.modes[index].decay;
                let length = (0.3 * t60).clamp(0.1, 0.9);
                let measured =
                    frequency_near(between(&signal, 0.02, 0.02 + length), f0 * ratio) / ratio;
                let error = cents(measured, f0);
                worst = worst.max(error.abs());
                assert!(error.abs() < 0.1, "{name} key {key}: {error:.4} cents");
            }
        }
        println!("worst tuning error {worst:.5} cents");
    }

    #[test]
    fn the_decays_follow_the_settings() {
        for (name, params, keys) in presets() {
            let params = bare(params);
            let key = keys[1];
            let f0 = key_hz(f64::from(key));
            let expected = params.decay_at(f0);
            let seconds = (0.8 * expected).clamp(0.6, 6.0);
            let signal = note(voice(params), on(key, 1.0), seconds + 1.0, seconds + 0.1);
            let index = if name == "tubular bells" { 3 } else { 0 };
            let frequency = f0 * params.modes[index].ratio;
            let expected = expected * params.modes[index].decay;
            let measured = t60_at(&signal, frequency, 0.1, seconds);
            println!("{name}: {measured:.2} s against {expected:.2} s");
            assert!(
                (measured / expected - 1.0).abs() < 0.1,
                "{name}: {measured} s against {expected} s"
            );
        }
    }

    #[test]
    fn a_damper_stops_a_released_note_and_a_marimba_rings_on() {
        let vibraphone = bare(MalletParams::vibraphone());
        let f0 = key_hz(67.0);
        let signal = note(voice(vibraphone), on(67, 1.0), 0.5, 1.2);
        let measured = t60_at(&signal, f0, 0.55, 0.75);
        assert!(
            (measured / vibraphone.release - 1.0).abs() < 0.2,
            "{measured} s"
        );
        // The marimba has no damper: released or held, the same.
        let marimba = MalletParams::marimba();
        let released = note(voice(marimba), on(60, 1.0), 0.2, 1.0);
        let held = note(voice(marimba), on(60, 1.0), 2.0, 1.0);
        assert_eq!(released, held);
    }

    #[test]
    fn velocity_makes_a_note_louder_and_brighter() {
        for (name, params, keys) in presets() {
            let key = keys[1];
            let loud = note(voice(params), on(key, 1.0), 2.0, 0.5);
            let soft = note(voice(params), on(key, 0.4), 2.0, 0.5);
            let f0 = key_hz(f64::from(key));
            // The level of the fundamental, which the contact hardly
            // changes, falls by what the range says.
            let index = if name == "tubular bells" { 3 } else { 0 };
            let frequency = f0 * params.modes[index].ratio;
            let difference = decibels(
                amplitude_at(between(&loud, 0.2, 0.4), frequency)
                    / amplitude_at(between(&soft, 0.2, 0.4), frequency),
            );
            let expected = 0.6 * params.velocity_range_db;
            let (bright, dark) = (centroid(&loud, 8_192), centroid(&soft, 8_192));
            println!("{name}: {difference:.1} dB, centroid {bright:.0} Hz against {dark:.0} Hz");
            assert!(
                (difference - expected).abs() < 1.5,
                "{name}: {difference} dB against {expected}"
            );
            assert!(
                bright > 1.05 * dark,
                "{name}: {bright} Hz against {dark} Hz"
            );
        }
    }

    #[test]
    fn an_accent_is_louder_and_harder() {
        for (name, params, keys) in presets() {
            let key = keys[1];
            let plain = note(voice(params), on(key, 0.7), 2.0, 0.5);
            let strong = note(voice(params), accented(key, 0.7), 2.0, 0.5);
            let f0 = key_hz(f64::from(key));
            let index = if name == "tubular bells" { 3 } else { 0 };
            let frequency = f0 * params.modes[index].ratio;
            let difference = decibels(
                amplitude_at(between(&strong, 0.2, 0.4), frequency)
                    / amplitude_at(between(&plain, 0.2, 0.4), frequency),
            );
            let (bright, dark) = (centroid(&strong, 8_192), centroid(&plain, 8_192));
            println!("{name}: accent {difference:.1} dB, {bright:.0} Hz against {dark:.0} Hz");
            assert!(
                (difference - params.accent_db).abs() < 1.0,
                "{name}: {difference}"
            );
            assert!(bright > 1.03 * dark, "{name}: {bright} against {dark}");
        }
    }

    #[test]
    fn the_tube_makes_the_fundamental_bloom() {
        let marimba = MalletParams {
            knock: 0.0,
            ..MalletParams::marimba()
        };
        let without = MalletParams {
            tube: 0.0,
            ..marimba
        };
        let f0 = key_hz(48.0);
        let with = note(voice(marimba), on(48, 1.0), 2.0, 1.0);
        let dry = note(voice(without), on(48, 1.0), 2.0, 1.0);
        // Over the first few milliseconds the tube has not built up yet;
        // later the fundamental is louder by about the tube's level.
        let early = decibels(
            amplitude_at(between(&with, 0.0, 0.01), f0)
                / amplitude_at(between(&dry, 0.0, 0.01), f0),
        );
        let late = decibels(
            amplitude_at(between(&with, 0.3, 0.5), f0) / amplitude_at(between(&dry, 0.3, 0.5), f0),
        );
        let expected = decibels(1.0 + marimba.tube);
        println!("tube: {early:.1} dB at first, {late:.1} dB later (expected {expected:.1})");
        assert!(early < 3.0, "{early}");
        assert!((late - expected).abs() < 0.5, "{late} against {expected}");
    }

    #[test]
    fn the_motor_makes_the_vibraphone_tremble_at_its_rate() {
        let params = MalletParams {
            knock: 0.0,
            decay: 30.0,
            ..MalletParams::vibraphone()
        };
        let key = 60;
        let f0 = key_hz(f64::from(key));
        let signal = note(voice(params), on(key, 1.0), 4.0, 3.0);
        // The level of the fundamental every 10 ms.
        let levels: Vec<f64> = (0..250)
            .map(|step| {
                let t = 0.4 + 0.01 * f64::from(step);
                amplitude_at(between(&signal, t, t + 0.02), f0)
            })
            .collect();
        let mean = levels.iter().sum::<f64>() / levels.len() as f64;
        let crossings = levels
            .windows(2)
            .filter(|pair| (pair[0] - mean) * (pair[1] - mean) < 0.0)
            .count();
        let rate = crossings as f64 / (2.0 * 2.49);
        let (low, high) = levels
            .iter()
            .fold((f64::MAX, 0.0f64), |(l, h), &x| (l.min(x), h.max(x)));
        let swing = decibels(high / low);
        println!("motor: {rate:.2} Hz, {swing:.1} dB");
        assert!((rate / params.motor_rate - 1.0).abs() < 0.06, "{rate} Hz");
        assert!(swing > 6.0, "{swing} dB");

        // Stopped, it does not tremble.
        let still = MalletParams {
            motor_rate: 0.0,
            ..params
        };
        let signal = note(voice(still), on(key, 1.0), 4.0, 1.5);
        let early = amplitude_at(between(&signal, 0.5, 0.6), f0);
        let later = amplitude_at(between(&signal, 0.6, 0.7), f0);
        assert!(decibels(early / later).abs() < 0.5);
    }

    #[test]
    fn the_steel_pans_octave_grows_more_from_a_harder_strike() {
        let params = MalletParams {
            knock: 0.0,
            ..MalletParams::steel_pan()
        };
        let f0 = key_hz(72.0);
        // The octave over the fundamental, a tenth of a second after the
        // strike, with and without the coupling, loud and soft.
        let octave = |params: MalletParams, velocity: f32| {
            let signal = note(voice(params), on(72, velocity), 2.0, 0.4);
            let window = between(&signal, 0.1, 0.2);
            decibels(amplitude_at(window, 2.0 * f0) / amplitude_at(window, f0))
        };
        let uncoupled = MalletParams {
            coupling: 0.0,
            ..params
        };
        let (loud, soft) = (octave(params, 1.0), octave(params, 0.3));
        let (loud_bare, soft_bare) = (octave(uncoupled, 1.0), octave(uncoupled, 0.3));
        println!(
            "octave: {loud:.1} dB loud, {soft:.1} dB soft; uncoupled {loud_bare:.1}, {soft_bare:.1}"
        );
        assert!(loud > loud_bare + 3.0, "{loud} against {loud_bare}");
        // Struck harder, the coupled octave grows by more than the
        // fundamental does.
        assert!(loud - soft > loud_bare - soft_bare + 3.0);
    }

    #[test]
    fn notes_start_and_are_taken_over_without_a_click() {
        for (name, params, keys) in presets() {
            let key = keys[1];
            let alone = note(voice(params), on(key, 1.0), 2.0, 0.6);
            // The note rises from silence: the force of the contact starts
            // from nothing, and the modes from rest; only the knock of the
            // hardest mallets is heard on the first sample, faintly.
            let start = alone[0].abs();
            let loudest = measure::peak(&alone);
            assert!(start < 5e-3 * loudest, "{name}: {start} against {loudest}");
            let steady = largest_jump(between(&alone, 0.05, 0.3));

            // One voice taken over by a new note: what is left once the new
            // note alone is taken away is the old note, fading.
            let steal = (0.25 * RATE) as usize;
            let other = keys[0];
            let mut synth = synth_of(voice(params));
            let both = play(
                &mut synth,
                &[(0, on(key, 1.0)), (steal, on(other, 1.0))],
                steal * 2,
                256,
            );
            let mut fresh = synth_of(voice(params));
            let second = play(&mut fresh, &[(steal, on(other, 1.0))], steal * 2, 256);
            let residual: Vec<f64> = both.iter().zip(&second).map(|(a, b)| a - b).collect();
            let at_steal = largest_jump(&residual[steal - 64..steal + 512]);
            assert!(
                at_steal <= 1.05 * steady,
                "{name}: {at_steal} at the steal against {steady}"
            );
            assert!(measure::peak(&residual[steal + 400..]) < 1e-9, "{name}");
        }
    }

    #[test]
    fn striking_a_ringing_bar_again_adds_to_it() {
        let params = bare(MalletParams::vibraphone());
        let f0 = key_hz(60.0);
        let mut synth = synth_of(voice(params));
        let at = (0.5 * RATE) as usize;
        let twice = play(
            &mut synth,
            &[(0, on(60, 1.0)), (at, on(60, 1.0))],
            at * 2,
            256,
        );
        let once = note(voice(params), on(60, 1.0), 2.0, 1.0);
        // The second strike lands on the first's ringing: louder than one
        // strike alone, and continuous.
        let level = |s: &[f64]| amplitude_at(between(s, 0.6, 0.8), f0);
        assert!(level(&twice) > 1.2 * level(&once));
        assert!(largest_jump(&twice[at - 8..at + 8]) < 2.0 * largest_jump(&once[at - 8..at + 8]));
    }

    #[test]
    fn a_bend_retunes_every_mode_as_it_rings() {
        let params = bare(MalletParams::glockenspiel());
        let mut synth = synth_of(voice(params));
        let at = (0.3 * RATE) as usize;
        let signal = play(
            &mut synth,
            &[(0, on(84, 1.0)), (at, Act::Bend(2.0))],
            (1.0 * RATE) as usize,
            256,
        );
        let f0 = key_hz(86.0);
        for mode in &params.modes[..3] {
            let measured = frequency_near(between(&signal, 0.4, 0.9), f0 * mode.ratio);
            assert!(cents(measured, f0 * mode.ratio).abs() < 0.5);
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(60, 0.9)),
            (1_000, accented(64, 0.7)),
            (7_000, on(67, 0.5)),
            (9_001, off(60)),
            (12_000, on(60, 1.0)),
            (15_000, Act::Bend(1.0)),
            (20_000, off(64)),
        ];
        for (name, params, _) in presets() {
            let run = |block| {
                play(
                    &mut synth_of([MalletVoice::new(params), MalletVoice::new(params)]),
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
            MalletParams {
                modes: [MalletMode::new(f64::NAN, f64::INFINITY, -1.0); MALLET_MODES],
                shape: MalletShape::Tine,
                strike_position: f64::NAN,
                decay: f64::INFINITY,
                decay_per_octave: f64::NAN,
                release: -1.0,
                contact: 0.0,
                softness: f64::INFINITY,
                velocity_range_db: -5.0,
                tube: 1e9,
                tube_third: f64::NAN,
                tube_q: 0.0,
                motor_rate: 1e9,
                motor_depth: f64::NAN,
                coupling: 1e9,
                knock: f64::INFINITY,
                knock_frequency: f64::NAN,
                body: [BodyMode::new(f64::NAN, f64::INFINITY, 1e9); 2],
                accent_db: 1e9,
                accent_hardness: 0.0,
                level: f64::NAN,
            },
            MalletParams {
                modes: [MalletMode::new(0.05, 4.0, 4.0); MALLET_MODES],
                decay: 60.0,
                tube: 8.0,
                tube_third: 8.0,
                tube_q: 200.0,
                coupling: 1.0,
                accent_db: 24.0,
                level: 4.0,
                body: [BodyMode::new(20.0, 50.0, 4.0); 2],
                ..MalletParams::steel_pan()
            },
        ];
        for params in extreme {
            let mut synth = synth_of([MalletVoice::new(params), MalletVoice::new(params)]);
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
            // Ten modes at the highest level, struck with the loudest accent,
            // through the loudest tube and body, can reach tens of
            // thousands; nothing goes beyond.
            assert!(
                measure::finite_within(&signal, 1e5),
                "{}",
                measure::peak(&signal)
            );
        }
        let _ = rms(&[0.0]);
    }
}
