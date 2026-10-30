//! Bowed strings by digital waveguide: violin, viola, cello, double bass
//! and a string section, bowed or plucked.

use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_midi::MidiMessage;

use super::common::{
    Handover, Noise, SILENCE, Tone, add_to_every_channel, db, flush, key_hz, setting, t60_factor,
    velocity_gain,
};
use super::modelling::{
    Bump, DRIFT_SECONDS, DcBlock, Dynamics, Glide, Life, Loss, Switch, Vibrato, VibratoShape,
    Wander, cubic_delay_for, trip_gain,
};
use super::string::BodyMode;
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{DelayLine, ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice};

/// The most players a [`BowedString`] can be: a section of that many
/// instruments playing each note.
pub const MAX_PLAYERS: usize = 4;

/// How many resonances of the instrument's body a [`BowedParams`] names: the
/// air and wood modes that give each instrument its voice. Above them the
/// body has many more, too close together to name ([`WOOD_MODES`] of them,
/// see [`BowedParams::wood`]).
pub const BODY_MODES: usize = 8;

/// How many resonances the body has above its named ones.
pub const WOOD_MODES: usize = 24;

/// The band the unnamed resonances of the body spread over, as multiples of
/// its air resonance (the first of [`BowedParams::body`]): for the violin,
/// from about 800 Hz to 7 kHz, where its modes overlap into the bridge hill
/// (J. Woodhouse, "The acoustics of the violin: a review", Rep. Prog. Phys.
/// 77, 2014, section 4).
const WOOD_LOWEST: f64 = 3.0;
const WOOD_HIGHEST: f64 = 25.0;

/// How narrow the unnamed resonances are, at the least and over their
/// spread, as Q: the wood's damping leaves them some 30 to 60 Hz wide near
/// 1.5 kHz.
const WOOD_Q: (f64, f64) = (25.0, 20.0);

/// How loud the unnamed resonances are against the named ones at
/// [`BowedParams::wood`] 1.
const WOOD_GAIN: f64 = 1.5;

/// How long a finger takes to stop the string at a slur without portamento:
/// the string's length moves to the new note's over this long, too fast to
/// be heard as a slide.
const FINGER_SECONDS: f64 = 0.002;

/// The finger landing on the string at a slur: a small knock, a fraction of
/// the bow's speed, for half a millisecond.
const FINGER_KNOCK: f64 = 0.15;
const KNOCK_SECONDS: f64 = 0.0005;

/// The bow's scratch at the start of a note: how long it lasts (its 60 dB
/// fall), and how much it roughens the bow at [`BowedParams::scratch`] 1
/// against [`BowedParams::bow_noise`] 1.
const SCRATCH_SECONDS: f64 = 0.06;
const SCRATCH_GRAIN: f64 = 6.0;

/// A body radiates poorly below its air resonance A0, the lowest mode that
/// moves air in and out of it (N. H. Fletcher and T. D. Rossing, section
/// 10.6): two poles of high-pass at this fraction of the first of
/// [`BowedParams::body`], so that the bow's bite, its lift and a finger's
/// knock, which move the string slowly, are not heard as a thump.
const RADIATION: f64 = 0.5;

/// How many times more than the bow's speed its roughness wavers.
const NOISE_WAVER: f64 = 3.0;

/// The lowest note a string plays, in hertz (A0). Lower keys sound here.
const LOWEST_HZ: f64 = 27.5;

/// The highest note, as a fraction of the sample rate (7.2 kHz at 48 kHz).
const HIGHEST_RATIO: f64 = 0.15;

/// The friction of rosin: its coefficient when sticking, when sliding
/// fast, and the sliding speed at which it is half way between, as a
/// fraction of the bow's speed. The curve is the hyperbola of J. H. Smith
/// and J. Woodhouse, "The tribology of rosin", J. Mech. Phys. Solids 48
/// (2000), as used by J. Woodhouse, "Bowed string simulation using a
/// thermal friction model", Acta Acustica 89 (2003), in its simpler form.
///
/// The friction of rosin is not a function of the sliding speed alone (it
/// follows the temperature of the contact, which the sliding heats); here
/// the curve's speed scales with the bow's, so the string's motion has the
/// same shape at every dynamic and only its size follows the bow.
const MU_STATIC: f64 = 0.8;
const MU_DYNAMIC: f64 = 0.3;
const SLIP_RATIO: f64 = 0.17;

/// The bow's speed at full dynamics, in the waveguide's units.
const BOW_SPEED: f64 = 0.2;

/// The range of bow force in which the string keeps to Helmholtz motion,
/// one slip a period: J. C. Schelleng's limits ("The bowed string and the
/// player", JASA 53, 1973). The most is `F_max = 2 Z v / (β Δμ)`, and the
/// least grows as the loss the corner suffers in a period, over `β²`.
///
/// In the waveguide's units, where the force is divided by `2 Z v / (β
/// Δμ)` so that its theoretical maximum is 1, the force kept to is at least
/// [`FORCE_FLOOR`], at least `FORCE_LOSS · (1 - g) / β` with `g` the gain of
/// a trip round the string at 3 kHz, and at most [`FORCE_CEILING`]: found by
/// measuring the model over the instruments' ranges, where above the
/// ceiling the string goes raucous and below the floor it slips twice a
/// period (see the tests). [`BowedParams::pressure`] moves between them.
const FORCE_FLOOR: f64 = 0.64;
const FORCE_LOSS: f64 = 0.28;
const FORCE_CEILING: f64 = 0.76;
const FORCE_LOW_NOTES: f64 = 0.72;
const FORCE_FAR: f64 = 0.5;

/// The finger's grip on the string: the gain of its reflection, and the
/// pole of the low-pass it makes. A finger is soft, and absorbs a little of
/// every wave that reaches it, more at high frequencies; without that loss,
/// ripples left over from each slip ring between the bow and the finger and
/// make the string slip twice.
const FINGER_GAIN: f64 = 0.993;
const FINGER_POLE: f64 = -0.3;
const FINGER: Loss = Loss {
    b0: FINGER_GAIN * (1.0 + FINGER_POLE),
    a: FINGER_POLE,
};

/// The frequency whose decay time [`BowedParams::high_decay`] sets.
const HIGH_HZ: f64 = 3_000.0;

/// How long the bowing arm takes to follow a change of dynamics.
const ARM_SECONDS: f64 = 0.03;

/// How long the bow takes to leave the string once the note is released.
const LIFT_SECONDS: f64 = 0.04;

/// The furthest from the bridge the bow gets, as a fraction of the string
/// left to bow: high up a string a player brings the bow nearer the
/// bridge, and towards the middle of the string the Helmholtz motion gives
/// way to two slips a period.
const HIGHEST_POSITION: f64 = 0.2;

/// How far above its open string a slurred note can be and still be played
/// on it: a major sixth.
const SLUR_REACH: f64 = 1.681_8;

/// How long the firm start of a note or a slur lasts, at least, in seconds
/// and in periods.
const GRIP_SECONDS: f64 = 0.03;
const GRIP_PERIODS: f64 = 12.0;

/// How long a player's ear takes to bring a note to its pitch.
const EAR_SECONDS: f64 = 0.12;

/// How much later than the period the string slips, in samples, before
/// the ear has heard it: the slip lands on a sample, on average half a
/// sample after the moment it would have come, which lengthens every
/// period by about that much (measured; see the tests).
const SLIP_LAG: f64 = 0.5;

/// How much faster than its speed the bow starts an accented note, for each
/// unit of [`BowedParams::accent_bite`].
const BITE_SPEED: f64 = 0.25;

/// Below this dynamic level the bow rests on the string: it damps it.
const RESTING: f64 = 0.02;

/// The bow position the output's level is given at.
const REFERENCE_POSITION: f64 = 0.1;

/// How many harmonics the corner of the Helmholtz motion keeps, at the
/// reference position and half way through the pressure's range: the
/// corner is rounded as it travels, and sharpened by the bow at every slip,
/// more the more force there is (L. Cremer, "The Physics of the Violin",
/// MIT Press 1984, chapter 7). Nearer the bridge the corner is sharper in
/// proportion, and the force's range from least to most quadruples it.
const CORNER_HARMONICS: f64 = 12.0;

/// How long an accent's bite lasts.
const BITE_SECONDS: f64 = 0.15;

/// The settings of a [`BowedString`].
///
/// Times are in seconds, frequencies in hertz and levels in decibels
/// unless said otherwise. Out-of-range values are limited when the
/// settings are taken (see [`BowedString::set_params`]), and a value that
/// is not a number takes the default of [`violin`](Self::violin).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BowedParams {
    /// The open strings, lowest first, 20 to 4 000. A note is played on the
    /// highest string at or below it, as a player in the lowest position
    /// would, which decides how much of the string is left to bow: the bow
    /// stays where it is on the string while the finger shortens it.
    pub strings: [f64; 4],
    /// Where the bow touches an open string, as a fraction of its length
    /// from the bridge, 0.02 to 0.4. On a stopped string the same spot is a
    /// larger fraction. Near the bridge (sul ponticello) the sound is
    /// bright and needs more force; over the fingerboard (sul tasto) it is
    /// soft and veiled.
    pub bow_position: f64,
    /// How hard the bow presses, 0 to 1, between the least force that
    /// keeps the string in its Helmholtz motion and the most: light is
    /// airy, heavy is dense and grainy.
    pub pressure: f64,
    /// How long the bow takes to reach its speed at the start of a note,
    /// 0.005 to 2.
    pub attack: f64,
    /// How rough the bow is, 0 to 1: the rosin's grip changes a little all
    /// the time, which is heard as noise in the tone.
    pub bow_noise: f64,
    /// How long the free string's fundamental takes to fall by 60 dB at
    /// middle C, 0.05 to 30: how long a plucked note rings, and a bowed one
    /// after the bow leaves while the finger stays down.
    pub decay: f64,
    /// How many times shorter that decay is an octave higher, 1 to 4.
    pub decay_per_octave: f64,
    /// The decay time at 3 kHz as a fraction of the fundamental's, 0.01 to
    /// 1.
    pub high_decay: f64,
    /// How long a released note takes to fall by 60 dB, 0.02 to 10: the
    /// finger lifts or the hand damps the string.
    pub release: f64,
    /// The vibrato's rate, 0 to 12.
    pub vibrato_rate: f64,
    /// Its depth, the swing either way in cents, 0 to 100.
    pub vibrato_depth: f64,
    /// How long after the start of a note it begins, 0 to 4.
    pub vibrato_delay: f64,
    /// How long it then takes to reach its depth, 0.01 to 4.
    pub vibrato_rise: f64,
    /// How long the finger takes to slide to a slurred note when
    /// [`portamento`](Self::portamento) is on, 0 to 1.
    pub legato_time: f64,
    /// Whether slurred notes slide into each other, the finger gliding along
    /// the string over [`legato_time`](Self::legato_time), as in an
    /// expressive portamento. Off, a slur is a change of finger: the string
    /// is stopped at its new length at once, with the small knock of the
    /// finger landing.
    pub portamento: bool,
    /// How far the pitch drifts, slowly, as a player's finger and ear do, in
    /// cents (its RMS), 0 to 20. Each player of a section drifts on their
    /// own.
    pub drift: f64,
    /// How much the bow's speed wavers, slowly, as a fraction of it (its
    /// RMS), 0 to 0.3; the bow's roughness wavers three times as much.
    pub fluctuation: f64,
    /// How much a held note swells after its attack, at its height about
    /// 0.9 s in, and then eases back over a few seconds, as a fraction of
    /// the bow's speed, 0 to 0.5.
    pub swell: f64,
    /// How much the bow scratches as it bites at the start of a note, 0 to
    /// 1, before the string settles into its motion: more at a higher
    /// velocity and on an accent, over some 60 ms.
    pub scratch: f64,
    /// The resonances of the instrument's body, fed by the force on the
    /// bridge.
    pub body: [BodyMode; BODY_MODES],
    /// How much the many resonances of the body above its named ones add, 0
    /// to 1: the wood's dense, uneven response, which a few resonances alone
    /// would leave smooth, like a filter.
    pub wood: f64,
    /// How much of the sound comes through the body, 0 to 1.
    pub body_mix: f64,
    /// The cut-off of the final low-pass, 500 to 20 000.
    pub tone: f64,
    /// How much quieter the lowest velocity is than the highest, 0 to 60.
    /// Velocity sets the bow's speed, which the level follows.
    pub velocity_range_db: f64,
    /// How much louder an accented note is, 0 to 24.
    pub accent_db: f64,
    /// How much harder the bow bites at the start of an accented note, 0 to
    /// 4, as a multiple of its force, falling away over 150 ms, and the bow
    /// reaches its speed three times as fast: a martelé.
    pub accent_bite: f64,
    /// How many instruments play each note, 1 to [`MAX_PLAYERS`]: one for a
    /// soloist, more for a section.
    pub players: usize,
    /// How far apart the players are tuned, in cents from the lowest to the
    /// highest, 0 to 50. They also start a little apart and each has a
    /// vibrato of their own.
    pub spread: f64,
    /// Plucked instead of bowed: the string is pulled by a finger at the
    /// bow's place and let go, and rings with its own decay.
    pub pizzicato: bool,
    /// How long a finger plucking at full velocity takes to let go of the
    /// string, 0.0002 to 0.02: shorter is brighter. Longer at lower
    /// velocities.
    pub pluck_time: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl BowedParams {
    /// A violin: strings G3, D4, A4 and E5, bowed about a ninth of the open
    /// string from the bridge. Its body: the air resonance A0 near 275 Hz,
    /// the centre bout's rotation near 405 Hz (which radiates little), the
    /// two corpus bending modes B1- and B1+ near 470 and 545 Hz (the T1 and
    /// C3 of the older nomenclature), C4 near 700 Hz, a mode near 1 kHz, the
    /// bridge's hill around 2.5 kHz, and above 800 Hz the dense, uneven
    /// modes of the wood (N. H. Fletcher and T. D. Rossing, "The Physics of
    /// Musical Instruments", 2nd ed., chapter 10; E. V. Jansson, "Acoustics
    /// for violin and guitar makers", 2002, chapter 7; G. Bissinger,
    /// "Structural acoustics of good and bad violins", JASA 124, 2008; J.
    /// Woodhouse, "The acoustics of the violin: a review", Rep. Prog. Phys.
    /// 77, 2014).
    pub fn violin() -> Self {
        Self {
            strings: [196.0, 293.66, 440.0, 659.26],
            bow_position: 0.09,
            pressure: 0.5,
            attack: 0.06,
            bow_noise: 0.3,
            decay: 1.2,
            decay_per_octave: 1.3,
            high_decay: 0.15,
            release: 0.25,
            vibrato_rate: 5.8,
            vibrato_depth: 18.0,
            vibrato_delay: 0.25,
            vibrato_rise: 0.35,
            legato_time: 0.03,
            portamento: false,
            drift: 2.0,
            fluctuation: 0.03,
            swell: 0.08,
            scratch: 0.4,
            body: [
                BodyMode::new(275.0, 12.0, 0.8),
                BodyMode::new(405.0, 15.0, 0.25),
                BodyMode::new(470.0, 18.0, 1.0),
                BodyMode::new(545.0, 16.0, 0.9),
                BodyMode::new(700.0, 12.0, 0.5),
                BodyMode::new(1_050.0, 10.0, 0.3),
                BodyMode::new(2_500.0, 1.6, 0.9),
                BodyMode::new(3_500.0, 3.0, 0.3),
            ],
            wood: 0.6,
            body_mix: 0.75,
            tone: 7_500.0,
            velocity_range_db: 24.0,
            accent_db: 3.0,
            accent_bite: 1.5,
            players: 1,
            spread: 0.0,
            pizzicato: false,
            pluck_time: 0.002,
            level: 0.15,
        }
    }

    /// A viola: strings C3, G3, D4 and A4, a larger body whose resonances
    /// sit a little lower than the violin's, and a darker tone.
    pub fn viola() -> Self {
        Self {
            strings: [130.81, 196.0, 293.66, 440.0],
            bow_position: 0.09,
            vibrato_rate: 5.5,
            vibrato_depth: 18.0,
            decay: 1.4,
            body: [
                BodyMode::new(230.0, 12.0, 0.8),
                BodyMode::new(320.0, 15.0, 0.25),
                BodyMode::new(350.0, 16.0, 1.0),
                BodyMode::new(420.0, 16.0, 0.9),
                BodyMode::new(600.0, 10.0, 0.4),
                BodyMode::new(880.0, 10.0, 0.3),
                BodyMode::new(2_000.0, 1.6, 0.8),
                BodyMode::new(3_000.0, 3.0, 0.25),
            ],
            tone: 6_500.0,
            ..Self::violin()
        }
    }

    /// A cello: strings C2, G2, D3 and A3, bowed a little nearer the bridge
    /// relative to its long strings, a body whose air resonance is near
    /// 100 Hz, and a slower vibrato.
    pub fn cello() -> Self {
        Self {
            strings: [65.41, 98.0, 146.83, 220.0],
            bow_position: 0.14,
            attack: 0.08,
            vibrato_rate: 5.2,
            vibrato_depth: 20.0,
            vibrato_delay: 0.3,
            decay: 2.0,
            release: 0.3,
            body: [
                BodyMode::new(100.0, 10.0, 0.8),
                BodyMode::new(145.0, 12.0, 0.25),
                BodyMode::new(175.0, 12.0, 0.9),
                BodyMode::new(215.0, 14.0, 0.9),
                BodyMode::new(290.0, 12.0, 0.5),
                BodyMode::new(440.0, 10.0, 0.3),
                BodyMode::new(1_200.0, 1.5, 0.8),
                BodyMode::new(2_400.0, 3.0, 0.3),
            ],
            tone: 5_500.0,
            level: 0.15,
            ..Self::violin()
        }
    }

    /// A double bass: strings E1, A1, D2 and G2 tuned in fourths, a large
    /// body resonating near 60 Hz, a slow attack and a slow, narrow vibrato.
    pub fn contrabass() -> Self {
        Self {
            strings: [41.2, 55.0, 73.42, 98.0],
            bow_position: 0.16,
            attack: 0.1,
            vibrato_rate: 4.8,
            vibrato_depth: 16.0,
            vibrato_delay: 0.35,
            decay: 2.5,
            release: 0.35,
            body: [
                BodyMode::new(60.0, 8.0, 0.8),
                BodyMode::new(88.0, 10.0, 0.25),
                BodyMode::new(110.0, 10.0, 0.8),
                BodyMode::new(145.0, 12.0, 0.7),
                BodyMode::new(210.0, 10.0, 0.5),
                BodyMode::new(320.0, 10.0, 0.3),
                BodyMode::new(800.0, 1.5, 0.7),
                BodyMode::new(1_600.0, 3.0, 0.3),
            ],
            tone: 4_000.0,
            level: 0.17,
            ..Self::violin()
        }
    }

    /// A string section: four players to a note, tuned up to 12 cents
    /// apart, starting a few milliseconds apart, each with a vibrato of
    /// their own, bowing softly, with a body between the violin's and the
    /// viola's.
    pub fn string_ensemble() -> Self {
        Self {
            strings: [65.41, 130.81, 293.66, 440.0],
            bow_position: 0.12,
            pressure: 0.4,
            attack: 0.14,
            bow_noise: 0.2,
            vibrato_rate: 5.5,
            vibrato_depth: 14.0,
            vibrato_delay: 0.2,
            legato_time: 0.05,
            body: [
                BodyMode::new(250.0, 10.0, 0.8),
                BodyMode::new(360.0, 14.0, 0.25),
                BodyMode::new(410.0, 14.0, 1.0),
                BodyMode::new(500.0, 14.0, 0.9),
                BodyMode::new(680.0, 10.0, 0.4),
                BodyMode::new(950.0, 10.0, 0.3),
                BodyMode::new(2_200.0, 1.5, 0.7),
                BodyMode::new(3_200.0, 3.0, 0.2),
            ],
            tone: 6_000.0,
            players: MAX_PLAYERS,
            spread: 12.0,
            level: 0.08,
            ..Self::violin()
        }
    }

    /// A violin plucked: a short, bright ring.
    pub fn violin_pizzicato() -> Self {
        Self {
            pizzicato: true,
            bow_position: 0.22,
            decay: 0.6,
            release: 0.15,
            pluck_time: 0.0015,
            level: 0.4,
            ..Self::violin()
        }
    }

    /// A cello plucked: round, ringing longer.
    pub fn cello_pizzicato() -> Self {
        Self {
            pizzicato: true,
            bow_position: 0.22,
            decay: 1.6,
            release: 0.2,
            pluck_time: 0.0025,
            level: 0.4,
            ..Self::cello()
        }
    }

    /// A double bass plucked by the side of a finger over the end of the
    /// fingerboard, as in jazz: deep, with a long ring.
    pub fn contrabass_pizzicato() -> Self {
        Self {
            pizzicato: true,
            bow_position: 0.25,
            decay: 2.6,
            release: 0.2,
            pluck_time: 0.004,
            level: 0.45,
            ..Self::contrabass()
        }
    }

    /// The settings with every value limited to its range.
    fn limited(self) -> Self {
        let default = Self::violin();
        let body_default = BodyMode::NONE;
        let mut strings = [0.0; 4];
        for (n, (slot, &open)) in strings.iter_mut().zip(&self.strings).enumerate() {
            *slot = setting(open, 20.0, 4_000.0, default.strings[n]);
        }
        Self {
            strings,
            bow_position: setting(self.bow_position, 0.02, 0.4, default.bow_position),
            pressure: setting(self.pressure, 0.0, 1.0, default.pressure),
            attack: setting(self.attack, 0.005, 2.0, default.attack),
            bow_noise: setting(self.bow_noise, 0.0, 1.0, default.bow_noise),
            decay: setting(self.decay, 0.05, 30.0, default.decay),
            decay_per_octave: setting(self.decay_per_octave, 1.0, 4.0, default.decay_per_octave),
            high_decay: setting(self.high_decay, 0.01, 1.0, default.high_decay),
            release: setting(self.release, 0.02, 10.0, default.release),
            vibrato_rate: setting(self.vibrato_rate, 0.0, 12.0, default.vibrato_rate),
            vibrato_depth: setting(self.vibrato_depth, 0.0, 100.0, default.vibrato_depth),
            vibrato_delay: setting(self.vibrato_delay, 0.0, 4.0, default.vibrato_delay),
            vibrato_rise: setting(self.vibrato_rise, 0.01, 4.0, default.vibrato_rise),
            legato_time: setting(self.legato_time, 0.0, 1.0, default.legato_time),
            portamento: self.portamento,
            drift: setting(self.drift, 0.0, 20.0, 0.0),
            fluctuation: setting(self.fluctuation, 0.0, 0.3, 0.0),
            swell: setting(self.swell, 0.0, 0.5, 0.0),
            scratch: setting(self.scratch, 0.0, 1.0, default.scratch),
            body: self.body.map(|mode| BodyMode {
                frequency: setting(mode.frequency, 20.0, 10_000.0, body_default.frequency),
                q: setting(mode.q, 0.5, 50.0, body_default.q),
                gain: setting(mode.gain, 0.0, 4.0, 0.0),
            }),
            wood: setting(self.wood, 0.0, 1.0, default.wood),
            body_mix: setting(self.body_mix, 0.0, 1.0, default.body_mix),
            tone: setting(self.tone, 500.0, 20_000.0, default.tone),
            velocity_range_db: setting(
                self.velocity_range_db,
                0.0,
                60.0,
                default.velocity_range_db,
            ),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            accent_bite: setting(self.accent_bite, 0.0, 4.0, default.accent_bite),
            players: self.players.clamp(1, MAX_PLAYERS),
            spread: setting(self.spread, 0.0, 50.0, 0.0),
            pizzicato: self.pizzicato,
            pluck_time: setting(self.pluck_time, 0.000_2, 0.02, default.pluck_time),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    /// The decay time of the fundamental of a note `f0` hertz high.
    fn decay_at(&self, f0: f64) -> f64 {
        let octaves = (f0 / key_hz(60.0)).log2();
        self.decay / self.decay_per_octave.powf(octaves)
    }

    /// The open string a note `f0` hertz high is played on: the highest at
    /// or below it, or, for a slur from a note on `current`, that string
    /// still if the note lies within a sixth above it, as a slurring player
    /// keeps to a string rather than cross to another.
    fn string_for(&self, f0: f64, current: Option<f64>) -> f64 {
        if let Some(open) = current {
            if f0 >= open * 0.999_9 && f0 <= open * SLUR_REACH {
                return open;
            }
        }
        self.strings
            .iter()
            .copied()
            .filter(|&open| open <= f0 * 1.000_1)
            .fold(self.strings[0], f64::max)
    }

    /// Where the bow is, as a fraction of the string left to bow, for a
    /// note `f0` hertz high on the string `open`, and `scale` times
    /// further from the bridge.
    fn position_at(&self, f0: f64, open: f64, scale: f64) -> f64 {
        (self.bow_position * scale * f0 / open).clamp(0.02, HIGHEST_POSITION)
    }

    fn vibrato(&self) -> VibratoShape {
        VibratoShape {
            rate: self.vibrato_rate,
            depth_cents: self.vibrato_depth,
            delay: self.vibrato_delay,
            rise: self.vibrato_rise,
            wander: 0.8,
            develop: 1.0,
        }
    }
}

impl Default for BowedParams {
    fn default() -> Self {
        Self::violin()
    }
}

/// The velocity the bow gives the string where it touches it.
///
/// `drop` is the bow's velocity less the string's as the waves arriving
/// there would make it with no friction, and `force` the bow's normal
/// force over twice the string's impedance, in the same units. Sticking,
/// the string moves with the bow: the bow gives it all of `drop`, as long
/// as that takes no more force than static friction allows. Slipping, the
/// friction force `2Z (drop - slip)` must equal `force · μ(slip)` with the
/// hyperbolic friction curve `μ(s) = μd + (μs - μd) s0 / (s0 + s)`: a
/// quadratic in the slip speed `s`, solved exactly.
///
/// Between the two lies the hysteresis that M. E. McIntyre, R. T.
/// Schumacher and J. Woodhouse describe ("On the oscillations of musical
/// instruments", JASA 74, 1983, after F. G. Friedlander's construction): a
/// string that slips goes on slipping as long as a slipping solution
/// exists, and one that sticks goes on sticking as long as static friction
/// can hold it. `sticking` carries which.
#[inline]
fn friction(drop: f64, force: f64, slip_speed: f64, sticking: &mut bool) -> f64 {
    if force <= 0.0 || drop.is_nan() {
        *sticking = false;
        return 0.0;
    }
    let x = drop.abs();
    let limit = force * MU_STATIC;
    if *sticking && x <= limit {
        return drop;
    }
    // s² + b s + c = 0.
    let b = slip_speed + force * MU_DYNAMIC - x;
    let c = slip_speed * (limit - x);
    let discriminant = b * b - 4.0 * c;
    let slip = if c <= 0.0 {
        // Static friction cannot hold: the one root that slips forward,
        // written so that nothing cancels.
        let root = discriminant.max(0.0).sqrt();
        if b > 0.0 {
            -2.0 * c / (b + root)
        } else {
            0.5 * (root - b)
        }
    } else if !*sticking && b < 0.0 && discriminant >= 0.0 {
        // Slipping, and a slipping solution exists: the faster one.
        0.5 * (discriminant.sqrt() - b)
    } else {
        *sticking = true;
        return drop;
    };
    *sticking = false;
    (x - slip).max(0.0).copysign(drop)
}

/// One of the body's unnamed resonances: a two-pole band-pass whose peak
/// gain is 1, `y[n] = b0 (x[n] - x[n-2]) + a1 y[n-1] - a2 y[n-2]` with its
/// poles at radius `r = e^(-π f / (Q fs))`, the resonator of J. O. Smith
/// and J. B. Angell, "A constant-gain digital resonator tuned by a single
/// coefficient", Computer Music Journal 6, 1982.
#[derive(Debug, Clone, Copy, Default)]
struct Resonator {
    a1: f64,
    a2: f64,
    b0: f64,
    x1: f64,
    x2: f64,
    y1: f64,
    y2: f64,
}

impl Resonator {
    fn tune(&mut self, frequency: f64, q: f64, rate: f64) {
        let r = (-core::f64::consts::PI * frequency / (q * rate)).exp();
        self.a1 = 2.0 * r * (TAU * frequency / rate).cos();
        self.a2 = r * r;
        self.b0 = 0.5 * (1.0 - r * r);
    }

    #[inline]
    fn step(&mut self, x: f64) -> f64 {
        let y = flush(self.b0 * (x - self.x2) + self.a1 * self.y1 - self.a2 * self.y2);
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// One player's string: the two stretches of string either side of the
/// bow, each a delay line holding the waves going out and back.
#[derive(Debug, Clone)]
struct Player {
    /// From the bow to the bridge and back, and from the bow to the finger
    /// and back.
    bridge: DelayLine<f64>,
    nut: DelayLine<f64>,
    /// The memory of the bridge's loss filter, and of the finger's.
    loss: f64,
    finger: f64,
    sticking: bool,
    vibrato: Vibrato,
    /// The player's own drift of pitch.
    drift: Wander,
    /// The bow's roughness: noise, and a low-pass of it.
    noise: Noise,
    rough: f64,
    /// This player's tuning, in cents, and how many samples it waits before
    /// it starts.
    detune: f64,
    wait: u32,
    /// The player's ear: how much longer or shorter than the period's the
    /// string is held, to make up for what the bow does to the pitch, and
    /// how many samples have passed since the string last slipped.
    ear: f64,
    since_slip: u32,
    /// Whether the string has slipped more than once since the last mark.
    cracked: bool,
    /// The finger's pull for a pizzicato: samples left, length, level.
    pluck_left: u32,
    pluck_len: u32,
    pluck_scale: f64,
    /// How many times the string has slipped, for the tests.
    #[cfg(test)]
    slips: u64,
}

impl Player {
    fn new() -> Self {
        Self {
            bridge: DelayLine::new(4),
            nut: DelayLine::new(4),
            loss: 0.0,
            finger: 0.0,
            sticking: false,
            vibrato: Vibrato::new(),
            drift: Wander::new(),
            noise: Noise::for_note(0, 0.0, 0),
            rough: 0.0,
            detune: 0.0,
            wait: 0,
            ear: 1.0,
            since_slip: 0,
            cracked: false,
            pluck_left: 0,
            pluck_len: 1,
            pluck_scale: 0.0,
            #[cfg(test)]
            slips: 0,
        }
    }

    fn silence(&mut self) {
        self.bridge.clear();
        self.nut.clear();
        self.loss = 0.0;
        self.sticking = false;
        self.rough = 0.0;
        self.wait = 0;
        self.ear = 1.0;
        self.since_slip = 0;
        self.cracked = false;
        self.pluck_left = 0;
    }

    /// The finger's pull, a raised cosine.
    #[inline]
    fn pluck(&mut self) -> f64 {
        if self.pluck_left == 0 {
            return 0.0;
        }
        let k = self.pluck_len - self.pluck_left;
        self.pluck_left -= 1;
        let phase = TAU * (f64::from(k) + 0.5) / f64::from(self.pluck_len);
        self.pluck_scale * (1.0 - phase.cos())
    }
}

/// One note: the players' strings and the body they share.
#[derive(Debug, Clone)]
struct BowedTone {
    rate: f64,
    players: [Player; MAX_PLAYERS],
    count: usize,
    pizzicato: bool,
    /// The pitch, in keys, gliding between slurred notes with portamento,
    /// and the bend.
    glide: Glide,
    bend: f64,
    /// A change of finger: the fade from the old length's reads, the key
    /// they were at and their offsets, and the finger's knock.
    switch: Switch,
    from_key: f64,
    from_bridge_offset: f64,
    from_nut_offset: f64,
    knock: Bump,
    /// The player's swell and waver, and how much of each and of the drift.
    life: Life,
    drift: f64,
    fluctuation: f64,
    swell: f64,
    /// The bow's scratch at the start of a note: how much, now, and how fast
    /// it falls.
    scratch: f64,
    scratch_now: f64,
    scratch_fall: f64,
    /// The open string the note is played on, in hertz.
    open: f64,
    /// The bow's place on the string, as asked and as it moves there.
    position: f64,
    position_now: f64,
    position_follow: f64,
    /// What the read delays need on top of the period's share, at the
    /// pitch aimed at: the loss filter's delay and the interpolators'.
    bridge_offset: f64,
    nut_offset: f64,
    /// The loss of a held string and of a released one, and how far the
    /// string has gone from one to the other.
    held_loss: Loss,
    released_loss: Loss,
    damping: f64,
    damping_step: f64,
    /// The bow: how far it is on the string, 0 to 1, and how fast it
    /// comes and goes; the accent's bite.
    bow: f64,
    bow_rise: f64,
    bow_fall: f64,
    bite: f64,
    bite_fall: f64,
    /// The firm start: 1 when a note starts or slurs, falling to 0, while
    /// the force moves from the top of its window to where the pressure
    /// puts it, and how fast.
    grip: f64,
    grip_fall: f64,
    grip_seconds: f64,
    held: bool,
    /// The pitch aimed at, in hertz, and the level the note was struck at:
    /// a note played louder is bowed harder, so its corner is sharper.
    target_f0: f64,
    struck: f64,
    /// The force, between Schelleng's limits, 0 to 1, and those limits at
    /// the pitch aimed at, as fractions of the theoretical maximum.
    pressure: f64,
    floor: f64,
    ceiling: f64,
    dynamics: Dynamics,
    roughness: f64,
    body: [StateVariableFilter<f64>; BODY_MODES],
    body_gain: [f64; BODY_MODES],
    wood: [Resonator; WOOD_MODES],
    wood_gain: [f64; WOOD_MODES],
    body_mix: f64,
    /// The corner's low-pass: its pole and its memory.
    corner: f64,
    corner_memory: f64,
    tone: StateVariableFilter<f64>,
    dc: DcBlock,
    /// The body's radiation below its air resonance: two poles of
    /// high-pass.
    radiation: [DcBlock; 2],
    level: f64,
    gain: f64,
    follower: f64,
    follower_fall: f64,
    sounding: bool,
}

impl BowedTone {
    fn new() -> Self {
        let band = || StateVariableFilter::new(SvfMode::Bandpass, 1_000.0, 1.0);
        Self {
            rate: 48_000.0,
            players: core::array::from_fn(|_| Player::new()),
            count: 1,
            pizzicato: false,
            glide: Glide::new(),
            bend: 0.0,
            switch: Switch::new(),
            from_key: 60.0,
            from_bridge_offset: 0.0,
            from_nut_offset: 0.0,
            knock: Bump::new(),
            life: Life::new(),
            drift: 0.0,
            fluctuation: 0.0,
            swell: 0.0,
            scratch: 0.0,
            scratch_now: 0.0,
            scratch_fall: 0.0,
            open: 196.0,
            position: 0.1,
            position_now: 0.1,
            position_follow: 1.0,
            bridge_offset: 0.0,
            nut_offset: 0.0,
            held_loss: Loss::NONE,
            released_loss: Loss::NONE,
            damping: 0.0,
            damping_step: 0.0,
            bow: 0.0,
            bow_rise: 0.0,
            bow_fall: 0.0,
            bite: 0.0,
            bite_fall: 0.0,
            grip: 0.0,
            grip_fall: 0.0,
            grip_seconds: GRIP_SECONDS,
            held: false,
            pressure: 0.5,
            target_f0: 440.0,
            struck: 1.0,
            floor: FORCE_FLOOR,
            ceiling: FORCE_CEILING,
            dynamics: Dynamics::new(),
            roughness: 0.0,
            body: core::array::from_fn(|_| band()),
            body_gain: [0.0; BODY_MODES],
            wood: [Resonator::default(); WOOD_MODES],
            wood_gain: [0.0; WOOD_MODES],
            body_mix: 0.0,
            corner: 0.0,
            corner_memory: 0.0,
            tone: StateVariableFilter::new(SvfMode::Lowpass, 8_000.0, 0.707),
            dc: DcBlock::default(),
            radiation: [DcBlock::default(); 2],
            level: 0.0,
            gain: 1.0,
            follower: 0.0,
            follower_fall: 0.0,
            sounding: false,
        }
    }

    fn prepare(&mut self, rate: f64) {
        self.rate = rate;
        let longest = (rate / LOWEST_HZ).ceil() as usize + 8;
        for player in &mut self.players {
            player.bridge = DelayLine::new((0.46 * longest as f64) as usize + 8);
            player.nut = DelayLine::new(longest);
        }
        let spec = ProcessSpec {
            sample_rate: rate,
            max_block_size: 1,
            num_channels: 1,
        };
        for filter in self.body.iter_mut().chain([&mut self.tone]) {
            Processor::<f64>::prepare(filter, &spec);
        }
        self.dc = DcBlock::new(20.0, rate);
        self.dynamics.prepare(rate, ARM_SECONDS);
        self.position_follow = 1.0 - (-1.0 / (0.02 * rate)).exp();
        self.bow_fall = 1.0 - (-3.0 / (LIFT_SECONDS * rate)).exp();
        self.bite_fall = t60_factor(BITE_SECONDS, rate);
        self.scratch_fall = t60_factor(SCRATCH_SECONDS, rate);
        self.damping_step = 1.0 / (LIFT_SECONDS * rate);
        self.follower_fall = t60_factor(0.3, rate);
        self.silence();
    }

    /// Takes the settings that apply at once.
    fn configure(&mut self, params: &BowedParams) {
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
        // The unnamed resonances: spread evenly in log frequency over the
        // band above the air resonance, each nudged off its place, its Q and
        // its share drawn from the same noise for every voice (the
        // instrument's own, not a note's), falling gently with frequency.
        let air = params.body[0].frequency;
        let (low, high) = (WOOD_LOWEST * air, WOOD_HIGHEST * air);
        let mut noise = Noise::for_note(0, 0.0, 0xB0D1);
        for (n, (mode, gain)) in self.wood.iter_mut().zip(&mut self.wood_gain).enumerate() {
            let place = (n as f64 + 0.5 + 0.8 * noise.next() * 0.5) / WOOD_MODES as f64;
            let frequency = (low * (high / low).powf(place)).min(0.45 * self.rate);
            let q = WOOD_Q.0 + WOOD_Q.1 * noise.unit();
            mode.tune(frequency, q, self.rate);
            let share = 0.3 + 0.7 * noise.unit();
            *gain = WOOD_GAIN * params.wood * share / (1.0 + frequency / (10.0 * air));
        }
        for stage in &mut self.radiation {
            stage.set_frequency(RADIATION * air, self.rate);
        }
        self.body_mix = params.body_mix;
        self.drift = params.drift;
        self.fluctuation = params.fluctuation;
        self.swell = params.swell;
        self.scratch = params.scratch;
        self.tone.set_frequency(params.tone.min(0.45 * self.rate));
        self.pressure = params.pressure;
        self.shape_corner();
        self.roughness = params.bow_noise;
        self.level = params.level;
        self.bow_rise = 1.0 - (-3.0 / (params.attack * self.rate)).exp();
        self.grip_seconds = (0.5 * params.attack).max(GRIP_SECONDS);
    }

    /// Aims the string at `key` (bent by the tone's bend): its loss filters
    /// and the delays the reads need, worked out at that pitch, and the
    /// bow's place on the string left to bow.
    fn aim(&mut self, params: &BowedParams, key: f64, scale: f64, slur: bool) {
        let rate = self.rate;
        let f0 = key_hz(key + self.bend).clamp(LOWEST_HZ, HIGHEST_RATIO * rate);
        let w0 = TAU * f0 / rate;
        let period = rate / f0;
        let wh = (TAU * HIGH_HZ.max(3.0 * f0) / rate).min(0.9 * core::f64::consts::PI);
        // The bridge's loss makes up what the finger does not lose of the
        // trip's loss.
        let (finger_low, finger_high) = (FINGER.gain(w0), FINGER.gain(wh));
        let design = |seconds: f64, high: f64| {
            Loss::design(
                trip_gain(seconds, period, rate) / finger_low,
                trip_gain(high.min(seconds), period, rate) / finger_high,
                w0,
                wh,
            )
        };
        let decay = params.decay_at(f0);
        self.held_loss = design(decay, decay * params.high_decay);
        self.released_loss = design(
            params.release.min(decay),
            0.4 * params.release.min(decay) * params.high_decay.sqrt(),
        );
        self.open = params.string_for(f0, if slur { Some(self.open) } else { None });
        self.position = params.position_at(f0, self.open, scale);

        // Schelleng's window for this pitch and place, and the force in it.
        let trip = self.held_loss.gain(wh) * finger_high;
        // Below about 200 Hz the window narrows towards the force that
        // suits every low note, which settle into their motion slowly and
        // are the easiest to crack; far from the bridge the most force the
        // string takes falls below its theoretical value. Both measured.
        let low = ((f0 - 50.0) / 150.0).clamp(0.0, 1.0);
        let floor = FORCE_LOW_NOTES - (FORCE_LOW_NOTES - FORCE_FLOOR) * low;
        let ceiling = FORCE_LOW_NOTES + (FORCE_CEILING - FORCE_LOW_NOTES) * low;
        let far = (self.position - 0.12).max(0.0);
        self.floor = (FORCE_LOSS * (1.0 - trip) / self.position).max(floor);
        self.ceiling = (ceiling - FORCE_FAR * far).max(self.floor);
        self.target_f0 = f0;
        self.shape_corner();
        // A low string takes more periods to settle into its motion: the
        // firm start lasts at least a dozen of them.
        let grip = self.grip_seconds.max(GRIP_PERIODS / f0);
        self.grip_fall = t60_factor(grip, rate);

        // Each stretch holds its share of the period less what its filter
        // delays; each read asks for a little more or less, so that the
        // cubic interpolator's delay at the fundamental is exact.
        let bridge_filter = self.held_loss.delay(w0);
        let nut_filter = FINGER.delay(w0);
        let bridge = (self.position * period - bridge_filter).max(1.0);
        let nut = ((1.0 - self.position) * period - nut_filter).max(1.0);
        self.bridge_offset = cubic_delay_for(bridge, w0) - bridge - bridge_filter;
        self.nut_offset = cubic_delay_for(nut, w0) - nut - nut_filter;
    }

    /// The corner's low-pass for the pitch aimed at, the place and the
    /// pressure.
    fn shape_corner(&mut self) {
        let harmonics = CORNER_HARMONICS
            * (REFERENCE_POSITION / self.position)
            * (2.0 * self.pressure - 1.0).exp2()
            * self.struck.powf(0.3);
        let cutoff = (harmonics * self.target_f0).clamp(100.0, 0.45 * self.rate);
        self.corner = if self.pizzicato {
            0.0
        } else {
            (-TAU * cutoff / self.rate).exp()
        };
    }

    /// Starts a note, or bows a sounding one again.
    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        params: &BowedParams,
        key: f64,
        level: f64,
        velocity: f64,
        accent: bool,
        scale: f64,
        seed: u64,
    ) {
        let rate = self.rate;
        let fresh = !self.sounding;
        self.pizzicato = params.pizzicato;
        self.glide.set(key);
        self.switch.finish();
        // The bow bites: it scratches before the string settles, more the
        // harder it is played.
        let bite = params.scratch * (0.4 + 0.6 * velocity.clamp(0.0, 1.0));
        self.scratch_now = if params.pizzicato {
            0.0
        } else if accent {
            1.5 * bite
        } else {
            bite
        };
        if fresh {
            self.life
                .start(Noise::for_note(key as u8, level, seed + 9), rate);
        }
        self.struck = setting(level, 0.0, 16.0, 1.0);
        self.aim(params, key, scale, false);
        if fresh {
            self.position_now = self.position;
            self.damping = 0.0;
            self.bow = 0.0;
            self.gain = 1.0;
        }
        self.dynamics.strike(level, fresh);
        self.held = true;
        self.damping = 0.0;
        self.bite = if accent { params.accent_bite } else { 0.0 };
        self.grip = 1.0;
        // An accented note is struck with the bow: it reaches its speed in
        // a third of the time.
        let attack = if accent {
            params.attack / 3.0
        } else {
            params.attack
        };
        self.bow_rise = 1.0 - (-3.0 / (attack * rate)).exp();
        self.count = params.players;
        let f0 = key_hz(key);
        let spread = params.spread;
        let count = self.count;
        for (n, player) in self.players.iter_mut().take(count).enumerate() {
            let mut noise = Noise::for_note(key as u8, level, seed + n as u64);
            if fresh {
                // Spread evenly across the players, nudged a little.
                let place = if count > 1 {
                    n as f64 / (count - 1) as f64 - 0.5
                } else {
                    0.0
                };
                player.detune = spread * (place + 0.1 * noise.next());
                player.wait = if n == 0 {
                    0
                } else {
                    ((0.004 + 0.022 * noise.unit()) * rate) as u32
                };
                player.vibrato.start(&params.vibrato(), rate, noise);
                player.drift.start(
                    Noise::for_note(key as u8, level, seed + 5 + n as u64),
                    DRIFT_SECONDS,
                    rate,
                );
                player.ear = if params.pizzicato {
                    1.0
                } else {
                    1.0 - SLIP_LAG * f0 / rate
                };
            }
            player.noise = noise;
            if params.pizzicato {
                // The finger lets go faster the harder it plucks, which is
                // brighter. The pulse's level is set so that every pluck
                // carries the same energy whatever its length, before the
                // velocity's gain.
                let seconds = params.pluck_time * (1.0 - level.max(1e-3).log10() / 3.0);
                let len = (seconds * rate).round().clamp(1.0, 4_096.0) as u32;
                player.pluck_len = len;
                player.pluck_left = len;
                let mut gain = level * BOW_SPEED * (200.0 / f64::from(len)).sqrt();
                if accent {
                    gain *= db(params.accent_db);
                }
                player.pluck_scale = gain / (1.0 + f0 / 2_000.0);
            }
        }
        self.sounding = true;
    }

    /// A slurred note: the finger moves to `key`, the bow carries on.
    fn slur(&mut self, params: &BowedParams, key: f64, level: f64, scale: f64) {
        let rate = self.rate;
        self.struck = setting(level, 0.0, 16.0, 1.0);
        if params.portamento {
            self.glide.to(key, params.legato_time, rate);
            self.aim(params, key, scale, true);
        } else {
            // A change of finger: the string is stopped at its new length at
            // once, and the finger knocks as it lands.
            self.from_key = self.glide.key;
            self.from_bridge_offset = self.bridge_offset;
            self.from_nut_offset = self.nut_offset;
            self.glide.set(key);
            self.aim(params, key, scale, true);
            self.switch.start(FINGER_SECONDS, rate);
            self.knock.start(KNOCK_SECONDS, rate);
        }
        self.dynamics.strike(level, false);
        self.held = true;
        self.damping = 0.0;
        self.grip = self.grip.max(0.5);
        if self.pizzicato {
            // Nothing slurs on a plucked string: it is plucked again.
            for player in self.players.iter_mut().take(self.count) {
                player.pluck_left = player.pluck_len;
            }
        }
    }

    fn release(&mut self) {
        self.held = false;
    }

    /// The force on the bridge `x` through the body: its named resonances
    /// and the wood's many, mixed with what reaches the air directly.
    #[inline]
    fn resonate(&mut self, x: f64) -> f64 {
        let mut body = 0.0;
        for (filter, gain) in self.body.iter_mut().zip(&self.body_gain) {
            if *gain > 0.0 {
                body += gain * filter.process_sample(0, x);
            }
        }
        for (mode, gain) in self.wood.iter_mut().zip(&self.wood_gain) {
            if *gain > 0.0 {
                body += gain * mode.step(x);
            }
        }
        (1.0 - self.body_mix) * x + self.body_mix * body
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let rate = self.rate;
        let key = self.glide.next() + self.bend;
        let life = self.life.next();
        self.scratch_now = flush(self.scratch_now * self.scratch_fall);
        // The bow arm's swell and waver.
        let sway = (1.0 + self.swell * life.swell + self.fluctuation * life.waver).max(0.0);
        self.position_now += (self.position - self.position_now) * self.position_follow;
        let position = self.position_now;

        // The bow comes and goes; a released string takes the released loss
        // as the bow leaves it.
        let dynamics = self.dynamics.next();
        if self.held && !self.pizzicato {
            self.bow += (1.0 - self.bow) * self.bow_rise;
        } else {
            self.bow = flush(self.bow * (1.0 - self.bow_fall));
            if self.bow < 1e-6 {
                self.bow = 0.0;
            }
        }
        // The string is damped once the note is released, and while the
        // bow rests on it without moving: the hair holds it still.
        let resting = !self.held || (!self.pizzicato && dynamics < RESTING);
        self.damping = if resting {
            (self.damping + self.damping_step).min(1.0)
        } else {
            (self.damping - self.damping_step).max(0.0)
        };
        let loss = Loss {
            b0: self.held_loss.b0 + (self.released_loss.b0 - self.held_loss.b0) * self.damping,
            a: self.held_loss.a + (self.released_loss.a - self.held_loss.a) * self.damping,
        };
        self.bite *= self.bite_fall;
        self.grip = flush(self.grip * self.grip_fall);

        // The bow's speed, and the force, which sits between Schelleng's
        // limits, both proportional to the speed.
        let speed = BOW_SPEED * dynamics * sway * self.bow * (1.0 + BITE_SPEED * self.bite);
        let maximum = BOW_SPEED * dynamics * sway / (position * (MU_STATIC - MU_DYNAMIC));
        // The force: where the pressure puts it in Schelleng's window, from
        // the top of the window at the start of a note or a slur. A string
        // falls into Helmholtz motion most surely with a firm start, and the
        // friction's hysteresis keeps it there as the force eases off.
        let pressure = self.pressure + (1.0 - self.pressure) * self.grip;
        let window = self.floor + (self.ceiling - self.floor) * pressure;
        let force = window * maximum * self.bow * (1.0 + self.bite);

        let mut out = 0.0;
        let count = self.count;
        // The bow's roughness wavers with the arm, and scratches as it bites.
        let waver = (1.0 + NOISE_WAVER * self.fluctuation * life.waver).max(0.0);
        let roughness = self.roughness * waver + SCRATCH_GRAIN * self.scratch_now;
        // A change of finger: the string's length moves to the new note's in
        // a couple of milliseconds, as the finger lands. A fade between the
        // two lengths' reads would carry two corners round the string, each
        // slipping on its own for a dozen periods; moved this fast, the one
        // corner is only squeezed, and the string goes on in its motion.
        let (key, bridge_offset, nut_offset, landing) = if self.switch.fading() {
            let share = self.switch.next();
            let rest = 1.0 - share;
            (
                key + rest * (self.from_key - self.glide.key),
                self.bridge_offset + rest * (self.from_bridge_offset - self.bridge_offset),
                self.nut_offset + rest * (self.from_nut_offset - self.nut_offset),
                true,
            )
        } else {
            (key, self.bridge_offset, self.nut_offset, false)
        };
        let knock = FINGER_KNOCK * speed * self.knock.next();
        // The ear listens once the note has settled, and not while the
        // finger slides or lands.
        let listening = self.grip < 0.2 && !self.glide.moving() && !landing;
        let drift = self.drift;
        for player in self.players.iter_mut().take(count) {
            let vibrato = player.vibrato.next(self.dynamics.vibrato);
            let wobble = 0.01 * (player.detune + vibrato + drift * player.drift.next());
            let f = key_hz(key + wobble);
            let period = rate / f;
            let length = period * player.ear;
            let bridge_delay = (position * length + bridge_offset).max(1.0);
            let nut_delay = ((1.0 - position) * length + nut_offset).max(1.0);
            let at_bridge = player.bridge.read_cubic(bridge_delay);
            let at_nut = player.nut.read_cubic(nut_delay);
            let from_bridge = -loss.step(&mut player.loss, at_bridge);
            let from_nut = -FINGER.step(&mut player.finger, at_nut);
            out += at_bridge;

            let mut jump = knock;
            if player.wait > 0 {
                player.wait -= 1;
            } else if speed > 0.0 {
                player.rough += (player.noise.next() - player.rough) * 0.25;
                let grain = 1.0 + 0.2 * roughness * player.rough;
                let drop = speed * grain - (from_bridge + from_nut);
                let was = player.sticking;
                jump = friction(
                    drop,
                    force * grain,
                    SLIP_RATIO * speed,
                    &mut player.sticking,
                );
                player.since_slip = player.since_slip.saturating_add(1);
                if was && !player.sticking {
                    #[cfg(test)]
                    {
                        player.slips += 1;
                    }
                    // The first slip of a period marks it; the time since the
                    // last mark is the period the string plays, and the ear
                    // moves the finger a little towards the right one. A
                    // period in which the string slipped more than once
                    // tells the ear nothing.
                    let heard = f64::from(player.since_slip);
                    if heard > 0.75 * period {
                        let error = heard / period - 1.0;
                        if listening && !player.cracked && error.abs() < 0.1 {
                            // A period is heard to the sample, so one period
                            // may not move the ear by more than a sample and a
                            // half's worth, nor by more than a percent where
                            // that is more: the limit must not clip the
                            // sample's own jitter, or it would bias the ear.
                            let gain = (period / (EAR_SECONDS * rate)).clamp(1e-3, 0.05);
                            let limit = (1.5 / period).max(0.01);
                            player.ear =
                                (player.ear - gain * error.clamp(-limit, limit)).clamp(0.9, 1.05);
                        }
                        player.since_slip = 0;
                        player.cracked = false;
                    } else {
                        player.cracked = true;
                    }
                }
            } else {
                player.sticking = false;
            }
            let pull = player.pluck();
            if pull != 0.0 {
                // Until the pluck has reached the bridge, the follower
                // cannot hear it.
                self.follower = self.follower.max(1e-3);
            }
            player.bridge.push(flush(from_nut + jump + pull));
            player.nut.push(flush(from_bridge + jump + pull));
        }

        // The level follows the bow's speed alone: nearer the bridge the
        // string swings more for the same speed, which a player hears as
        // brightness, not loudness.
        let out = if self.pizzicato {
            out
        } else {
            out * position / REFERENCE_POSITION
        };
        // The corner of the Helmholtz motion, sharper for a heavier bow and a
        // bow nearer the bridge.
        self.corner_memory = flush(out + self.corner * (self.corner_memory - out));
        let mixed = self.resonate(self.corner_memory);
        let toned = self.tone.process_sample(0, mixed);
        let [first, second] = &mut self.radiation;
        let radiated = second.step(first.step(self.dc.step(toned)));
        let out = radiated * self.level * self.dynamics.volume;
        self.follower = (self.follower * self.follower_fall).max(out.abs());
        let quiet = self.follower < SILENCE;
        let bowed_out = !self.held && self.bow == 0.0;
        let plucked_out = self.pizzicato && self.players[0].pluck_left == 0;
        if quiet && (bowed_out || plucked_out) {
            self.silence();
        }
        out
    }
}

impl Tone for BowedTone {
    fn is_sounding(&self) -> bool {
        self.sounding
    }

    fn silence(&mut self) {
        for player in &mut self.players {
            player.silence();
        }
        for filter in self.body.iter_mut().chain([&mut self.tone]) {
            Processor::<f64>::reset(filter);
        }
        for mode in &mut self.wood {
            mode.reset();
        }
        self.switch.finish();
        self.knock.stop();
        self.scratch_now = 0.0;
        self.dc.reset();
        for stage in &mut self.radiation {
            stage.reset();
        }
        self.corner_memory = 0.0;
        self.bow = 0.0;
        self.bite = 0.0;
        self.damping = 0.0;
        self.held = false;
        self.follower = 0.0;
        self.sounding = false;
    }
}

/// A bowed string by digital waveguide: a voice for
/// [`Synth`](crate::Synth) that plays violin, viola, cello, double bass and
/// a string section, bowed or plucked.
///
/// The string is two delay lines, one either side of the bow, after J. O.
/// Smith, "Efficient simulation of the reed-bore and bow-string
/// mechanisms" (ICMC 1986), and the bow meets it through the friction of
/// rosin, solved exactly at every sample with the hysteresis of M. E.
/// McIntyre, R. T. Schumacher and J. Woodhouse, "On the oscillations of
/// musical instruments" (JASA 1983):
///
/// ```text
///            bridge                bow                 finger
///  body <--- [loss]--[delay  β·P]---(friction)---[delay (1-β)·P]---(-1)
///                     ^  bridge's stretch  |  finger's stretch  ^
///                     +-------------------+--------------------+
/// ```
///
/// - **Helmholtz motion.** A corner travels round the string; while it is
///   on the far side the string sticks to the bow and is dragged along,
///   and when it passes the bow the string slips back, once a period. The
///   force the bow needs lies between J. C. Schelleng's limits;
///   [`BowedParams::pressure`] places it inside the part of that window
///   where the string keeps to Helmholtz motion (measured over every
///   preset's range), and every note starts at its top, with a firm grip,
///   which is where the motion establishes itself most surely, then eases
///   to where the pressure puts it. A heavier bow, or one nearer the
///   bridge, sharpens the corner, which is heard as brightness (L. Cremer,
///   "The Physics of the Violin").
/// - **In tune.** The period's delay is shared between the two stretches by
///   the bow's place, and each is read by a cubic interpolator, so the
///   pitch, the vibrato and the slurs move smoothly; what the loss filters
///   and the interpolators delay at the fundamental is worked out and
///   taken off. The bow itself lengthens every period a little (a slip
///   lands on a sample, half a sample late on average, which flattens a
///   high note by tens of cents), so each player has an ear: the time
///   between slips is the period played, and the string is shortened or
///   lengthened, slowly, until it is the note's, as a player corrects by
///   ear. The ear listens only to periods with a single slip, and is
///   limited per period so that the sample's own jitter cannot bias it.
/// - **The bow's place** is fixed on the string while the finger shortens
///   it: a note is played on the highest open string at or below it
///   ([`BowedParams::strings`]), and higher up a string the bow is a
///   larger fraction of what is left, as on the instrument.
/// - **Dynamics** are the bow's speed: velocity sets it, and pressure on
///   the key, the breath controller or the expression pedal take over
///   while the note sounds. The level follows the speed.
/// - **The bow's bite**: a note starts with the bow scratching before the
///   string settles into its motion ([`BowedParams::scratch`]), more at a
///   higher velocity and on an accent.
/// - **The player** is never quite still: each player's pitch drifts by a
///   few cents ([`BowedParams::drift`]), the bow's speed and roughness
///   waver ([`BowedParams::fluctuation`]), and a held note swells after its
///   attack and eases back ([`BowedParams::swell`]), each from slow noise
///   seeded by the note.
/// - **Vibrato** waits, then grows, quickening to its rate as it does, and
///   wanders, as a player's does; [`Expression::Vibrato`] and the
///   modulation wheel scale it.
/// - **Legato**: in a [`Synth`](crate::Synth) set to legato, a note
///   struck while another is held slurs to it: the bow goes on, and the
///   finger stops the string at its new length within 2 ms, with the
///   small knock of its landing, rather than sliding through the pitches
///   between; with [`BowedParams::portamento`] it slides
///   ([`BowedParams::legato_time`]). The note stays on its string if it
///   lies within a sixth above it, as a slurring player keeps to a string.
/// - **The body** is fed by the force on the bridge: its named air and
///   wood resonances ([`BowedParams::body`]: for the violin A0, the centre
///   bout's rotation, B1-, B1+, C4 and the bridge hill) and, above them,
///   [`WOOD_MODES`] narrow, unevenly spread resonances of the wood
///   ([`BowedParams::wood`]), whose dense, irregular response is what makes
///   a body sound like wood rather than a filter.
/// - **A section** ([`BowedParams::players`]) is several strings to a note,
///   tuned a little apart, starting a little apart, each with its own
///   vibrato.
/// - **Pizzicato** ([`BowedParams::pizzicato`]): the string is pulled by a
///   finger at the bow's place and let go.
///
/// An accent is louder ([`BowedParams::accent_db`]) and the bow bites
/// harder at its start ([`BowedParams::accent_bite`]).
/// [`Expression::Brightness`] moves the bow towards the bridge (1) or the
/// fingerboard (0). Striking a sounding key again bows it again from where
/// the string is. Releasing the key lifts the bow, and the string is
/// damped ([`BowedParams::release`]). Every source of noise is seeded by
/// the note.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{BowedParams, BowedString};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut cello = Synth::with_voices(1, || BowedString::new(BowedParams::cello()));
/// cello.set_legato(true);
/// cello.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// cello.note_on(NoteRef::new(0, 48), 0.8);
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// for _ in 0..20 {
///     cello.process(&mut block);
/// }
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct BowedString {
    params: BowedParams,
    tones: Handover<BowedTone>,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
    key: u8,
    /// The velocity of the note, 0 to 1.
    velocity: f64,
}

impl BowedString {
    /// A string with these settings. Until it is prepared it assumes
    /// 48 kHz and has no room to sound.
    pub fn new(params: BowedParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            tones: Handover::new(BowedTone::new(), BowedTone::new()),
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
            key: 60,
            velocity: 0.8,
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note takes the body, the tone, the pressure, the bow's
    /// roughness and the level at once, and the rest at its next note.
    pub fn set_params(&mut self, params: BowedParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &BowedParams {
        &self.params
    }

    fn configure(&mut self) {
        let params = self.params;
        self.tones.now.configure(&params);
        self.tones.old.configure(&params);
    }

    /// How much further from the bridge the bow is than its setting: a
    /// player plays louder nearer the bridge, where the string takes more
    /// force (0.8 times as far at full velocity, 1.2 at none), and
    /// [`Expression::Brightness`] moves it twice as far at 0 and half at 1.
    fn position_scale(&self) -> f64 {
        let brightness = self.tones.now.dynamics.brightness.unwrap_or(0.5);
        (1.0 - 2.0 * brightness + 0.6 * (0.5 - self.velocity)).exp2()
    }

    fn level(&self, velocity: f32, accent: bool) -> f64 {
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let level = velocity_gain(velocity, self.params.velocity_range_db);
        if accent {
            level * db(self.params.accent_db)
        } else {
            level
        }
    }

    fn retune(&mut self) {
        let bend = self.bend + self.tuning;
        let tone = &mut self.tones.now;
        tone.bend = bend;
        if tone.sounding {
            let scale = self.position_scale();
            let target = self.tones.now.glide.target;
            let params = self.params;
            self.tones.now.aim(&params, target, scale, true);
        }
    }
}

impl<S: Sample> Voice<S> for BowedString {
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
        self.velocity = f64::from(velocity).clamp(0.0, 1.0);
        let level = self.level(velocity, accent);
        let scale = self.position_scale();
        self.tones.now.bend = self.bend + self.tuning;
        self.tones.now.start(
            &params,
            self.key_tuning.key_pitch(self.key),
            level,
            self.velocity,
            accent,
            scale,
            u64::from(self.key) * 16,
        );
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.tones.now.sounding {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        let params = self.params;
        self.key = note.key.min(127);
        self.velocity = f64::from(velocity).clamp(0.0, 1.0);
        let level = self.level(velocity, accent);
        let scale = self.position_scale();
        self.tones
            .now
            .slur(&params, self.key_tuning.key_pitch(self.key), level, scale);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.tones.now.release();
    }

    fn choke(&mut self) {
        self.tones.choke();
        self.tones.now.dynamics.reset();
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
        add_to_every_channel(buffer, || tones.next(BowedTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            self.retune();
        } else if self.tones.now.dynamics.expression(expression, value)
            && expression == Expression::Brightness
        {
            self.retune();
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            self.retune();
        }
    }

    fn midi(&mut self, message: MidiMessage) {
        self.tones.now.dynamics.midi(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::measure::{
        self, Act, RATE, accented, amplitude_at, band_passed, between, cents, decibels,
        frequency_near, inharmonic_db, largest_jump, note, off, on, play, rms, synth_of, t60_at,
    };
    use crate::instruments::modelling::probe::{
        harmonic_centroid, high_band_levels, levels, modulation, period_track, pitch_track,
    };

    /// The bowed presets and the range each plays.
    fn bowed() -> [(&'static str, BowedParams, u8, u8); 5] {
        [
            ("violin", BowedParams::violin(), 55, 100),
            ("viola", BowedParams::viola(), 48, 88),
            ("cello", BowedParams::cello(), 36, 79),
            ("contrabass", BowedParams::contrabass(), 28, 67),
            ("ensemble", BowedParams::string_ensemble(), 36, 96),
        ]
    }

    /// The plucked presets and their ranges.
    fn plucked() -> [(&'static str, BowedParams, u8, u8); 3] {
        [
            ("violin", BowedParams::violin_pizzicato(), 55, 93),
            ("cello", BowedParams::cello_pizzicato(), 36, 76),
            ("contrabass", BowedParams::contrabass_pizzicato(), 28, 60),
        ]
    }

    fn voice(params: BowedParams) -> [BowedString; 1] {
        [BowedString::new(params)]
    }

    /// The same settings with none of the player's drift, waver, swell or
    /// scratch.
    fn lifeless(params: BowedParams) -> BowedParams {
        BowedParams {
            drift: 0.0,
            fluctuation: 0.0,
            swell: 0.0,
            scratch: 0.0,
            ..params
        }
    }

    /// The same settings with no vibrato, one player, no roughness and none
    /// of the player's drift, waver, swell or scratch: the model alone.
    fn plain(params: BowedParams) -> BowedParams {
        BowedParams {
            vibrato_depth: 0.0,
            players: 1,
            bow_noise: 0.0,
            ..lifeless(params)
        }
    }

    /// The same settings with no vibrato, one player and no roughness, but
    /// the player's drift, waver and swell.
    fn held(params: BowedParams) -> BowedParams {
        BowedParams {
            vibrato_depth: 0.0,
            players: 1,
            bow_noise: 0.0,
            ..params
        }
    }

    /// A slur from `first` to `second` at 1 s on a fresh legato synth, the
    /// first note released just after.
    fn slurred(params: BowedParams, first: u8, second: u8) -> Vec<f64> {
        let mut synth = synth_of(voice(params));
        synth.set_legato(true);
        let at = (1.0 * RATE) as usize;
        play(
            &mut synth,
            &[
                (0, on(first, 0.8)),
                (at, on(second, 0.8)),
                (at + 10, off(first)),
            ],
            (2.0 * RATE) as usize,
            256,
        )
    }

    /// How long after `at` seconds the pitch settles within `tolerance`
    /// cents of `target` and stays there for 100 ms, and how many periods
    /// before that lay between `from` and `target`, more than `tolerance`
    /// from both.
    fn settling(signal: &[f64], at: f64, from: f64, target: f64, tolerance: f64) -> (f64, usize) {
        let (low, high) = (from.min(target), from.max(target));
        // A narrow band, as a bowed string's harmonics are strong; and the
        // median of seven periods, as each slip lands on a sample and a
        // single period of a high note is out by up to a sample's worth
        // (some 25 cents at 700 Hz).
        let raw = period_track(between(signal, at - 0.05, at + 0.3), 0.85 * low, 1.2 * high);
        let track: Vec<(f64, f64)> = (0..raw.len())
            .map(|n| {
                let mut near: Vec<f64> = raw[n.saturating_sub(3)..(n + 4).min(raw.len())]
                    .iter()
                    .map(|p| p.1)
                    .collect();
                near.sort_by(f64::total_cmp);
                (raw[n].0, near[near.len() / 2])
            })
            .collect();
        let start = 0.05;
        let settled = track
            .iter()
            .enumerate()
            .find(|&(n, &(t, _))| {
                t >= start
                    && track[n..]
                        .iter()
                        .take_while(|&&(u, _)| u < t + 0.1)
                        .all(|&(_, f)| cents(f, target).abs() < tolerance)
            })
            .map_or(f64::INFINITY, |(_, &(t, _))| t - start);
        let between_notes = track
            .iter()
            .filter(|&&(t, f)| {
                t >= start
                    && t - start < settled
                    && f > low
                    && f < high
                    && cents(f, from).abs() > tolerance
                    && cents(f, target).abs() > tolerance
            })
            .count();
        (settled, between_notes)
    }

    /// Bows `key` on a fresh string for `seconds` and returns the sound and
    /// how many times a period the string slipped over the second half.
    fn bow(params: BowedParams, key: u8, seconds: f64) -> (Vec<f64>, f64) {
        let mut synth = synth_of(voice(params));
        let half = (0.5 * seconds * RATE) as usize;
        let mut signal = play(&mut synth, &[(0, on(key, 0.8))], half, 256);
        for voice in synth.voices_mut() {
            voice.tones.now.players[0].slips = 0;
        }
        signal.extend(play(&mut synth, &[], half, 256));
        let slips = synth.voices_mut().next().unwrap().tones.now.players[0].slips;
        let periods = 0.5 * seconds * key_hz(f64::from(key));
        (signal, slips as f64 / periods)
    }

    #[test]
    fn the_friction_follows_its_curve_and_its_hysteresis() {
        const V0: f64 = 0.02;
        let force = 1.0;
        // Stuck: the bow gives the string all the velocity it lacks.
        let mut sticking = true;
        assert_eq!(friction(0.5, force, V0, &mut sticking), 0.5);
        assert!(sticking);
        // Pulled past static friction it slips, and the friction force
        // given is the curve's at the slip speed.
        for drop in [0.81, 1.0, 3.0, 20.0] {
            let mut sticking = true;
            let given = friction(drop, force, V0, &mut sticking);
            assert!(!sticking);
            let slip = drop - given;
            let mu = MU_DYNAMIC + (MU_STATIC - MU_DYNAMIC) * V0 / (V0 + slip);
            assert!((given - force * mu).abs() < 1e-12, "{drop}: {given}");
            // And the same the other way.
            let mut sticking = true;
            assert_eq!(friction(-drop, force, V0, &mut sticking), -given);
        }
        // Below static friction, a string that slips keeps slipping while a
        // slipping solution exists, and one that sticks keeps sticking.
        let drop = 0.7;
        let mut slipping = false;
        assert!(friction(drop, force, V0, &mut slipping) < drop);
        assert!(!slipping);
        let mut sticking = true;
        assert_eq!(friction(drop, force, V0, &mut sticking), drop);
        // Slow enough, a slipping string sticks again.
        let mut slipping = false;
        assert_eq!(friction(0.05, force, V0, &mut slipping), 0.05);
        assert!(slipping);
        // No force, no friction; nothing breaks it.
        let mut state = true;
        assert_eq!(friction(0.5, 0.0, V0, &mut state), 0.0);
        assert_eq!(friction(f64::NAN, 1.0, V0, &mut state), 0.0);
        assert!(friction(f64::INFINITY, 1.0, V0, &mut state).is_finite());
    }

    #[test]
    fn bowed_notes_sustain_in_helmholtz_motion_at_every_pressure() {
        let mut worst_swing = 0.0f64;
        let mut worst_cents = 0.0f64;
        for (name, params, low, high) in bowed() {
            for pressure in [0.0, 0.5, 1.0] {
                let params = BowedParams {
                    pressure,
                    players: 1,
                    ..lifeless(params)
                };
                let (mut notes, mut helmholtz) = (0, 0);
                for key in (low..=high).step_by(3) {
                    let f0 = key_hz(f64::from(key));
                    let (signal, slips) = bow(params, key, 1.6);
                    notes += 1;
                    if (slips - 1.0).abs() <= 0.1 {
                        helmholtz += 1;
                    }
                    // Sustained: no window of the last second is more than
                    // 3 dB from another, though the vibrato sways the
                    // level through the body's resonances.
                    let windows = levels(between(&signal, 0.6, 1.6), 0.1);
                    let (quiet, loud) = windows
                        .iter()
                        .fold((f64::MAX, 0.0f64), |(q, l), &x| (q.min(x), l.max(x)));
                    let swing = decibels(loud / quiet);
                    worst_swing = worst_swing.max(swing);
                    assert!(swing < 3.0, "{name} p{pressure} key {key}: {swing:.2} dB");
                    // And at its pitch.
                    let error = cents(frequency_near(between(&signal, 0.6, 1.6), f0), f0);
                    worst_cents = worst_cents.max(error.abs());
                    assert!(
                        error.abs() < 5.0,
                        "{name} p{pressure} key {key}: {error:.2} cents"
                    );
                }
                let share = f64::from(helmholtz) / f64::from(notes);
                println!(
                    "{name} pressure {pressure}: {helmholtz} of {notes} notes in Helmholtz motion"
                );
                assert!(share >= 0.85, "{name} p{pressure}: {share}");
            }
        }
        println!("worst level swing {worst_swing:.2} dB, worst tuning {worst_cents:.2} cents");
    }

    #[test]
    fn every_bowed_note_is_in_tune_across_its_range() {
        // The player's drift is on, so a note's tuning is its average over
        // two seconds of its steady part: the drift keeps about 0.6 of its
        // RMS there (see the winds' test). Every note is held to the
        // model's own 5 cents plus three times that, and 95% of them to its
        // own 2 cents plus that once.
        let mut errors = Vec::new();
        for (name, params, low, high) in bowed() {
            for key in (low..=high).step_by(2) {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(held(params)), on(key, 0.8), 3.0, 2.8);
                let error = cents(frequency_near(between(&signal, 0.8, 2.8), f0), f0);
                errors.push((name, key, error, params.drift));
            }
        }
        let worst = errors.iter().fold(0.0f64, |m, e| m.max(e.2.abs()));
        let within = errors
            .iter()
            .filter(|e| e.2.abs() < 2.0 + 0.6 * e.3)
            .count();
        println!(
            "{within} of {} bowed notes within 2 cents and their drift, worst {worst:.2}",
            errors.len()
        );
        for (name, key, error, drift) in &errors {
            if error.abs() >= 2.0 + 0.6 * drift {
                println!("{name} key {key}: {error:.2} cents");
            }
        }
        for (name, key, error, drift) in &errors {
            assert!(
                error.abs() < 5.0 + 1.8 * drift,
                "{name} key {key}: {error:.2} cents"
            );
        }
        assert!(within as f64 >= 0.95 * errors.len() as f64);
    }

    #[test]
    fn plucked_notes_ring_in_tune_and_decay_as_set() {
        let mut worst = 0.0f64;
        for (name, params, low, high) in plucked() {
            let params = plain(params);
            for key in (low..=high).step_by(3) {
                let f0 = key_hz(f64::from(key));
                // As set, unless the finger alone damps the string sooner: it
                // loses a little of every trip, most for a high note.
                let finger = -3.0 / (f0 * FINGER.gain(TAU * f0 / RATE).log10());
                let decay = params.decay_at(f0).min(finger);
                // Over the first 30 dB, well above where the voice stops.
                let seconds = (0.5 * decay).clamp(0.16, 2.0) + 0.05;
                let signal = note(voice(params), on(key, 0.8), 3.0, seconds + 0.05);
                let error = cents(
                    frequency_near(between(&signal, 0.02, 0.02 + 0.5 * seconds), f0),
                    f0,
                );
                worst = worst.max(error.abs());
                assert!(error.abs() < 0.5, "{name} key {key}: {error:.3} cents");
                let measured = t60_at(&signal, f0, 0.05, seconds);
                assert!(
                    (measured / decay - 1.0).abs() < 0.2,
                    "{name} key {key}: {measured:.2} s against {decay:.2}"
                );
            }
        }
        println!("plucked: worst tuning {worst:.4} cents");
    }

    #[test]
    fn the_vibrato_waits_then_swings_at_its_rate_and_depth() {
        let params = BowedParams {
            bow_noise: 0.0,
            players: 1,
            ..lifeless(BowedParams::violin())
        };
        let key = 69;
        let f0 = key_hz(f64::from(key));
        let signal = note(voice(params), on(key, 0.8), 3.0, 3.0);
        let hop = 0.005;
        // Before the vibrato starts, the pitch is steady.
        let early = pitch_track(&signal, f0, 0.1, 0.24, 0.03, hop);
        let (_, still) = modulation(&early, f0, hop);
        // Once it has grown, it swings at its rate and depth.
        let late = pitch_track(&signal, f0, 1.0, 2.9, 0.03, hop);
        let (rate, depth) = modulation(&late, f0, hop);
        println!(
            "vibrato: {still:.2} cents before, then {rate:.2} Hz and {depth:.1} cents (set {} Hz, {} cents)",
            params.vibrato_rate, params.vibrato_depth
        );
        assert!(still < 0.25 * depth, "{still} against {depth}");
        assert!((rate / params.vibrato_rate - 1.0).abs() < 0.1, "{rate}");
        assert!((depth / params.vibrato_depth - 1.0).abs() < 0.2, "{depth}");
        // The vibrato expression scales it.
        let mut synth = synth_of(voice(params));
        synth.note_on(NoteRef::new(0, key), 0.8);
        for voice in synth.voices_mut() {
            Voice::<f64>::expression(voice, Expression::Vibrato, 0.0);
        }
        let signal = play(&mut synth, &[], (2.0 * RATE) as usize, 256);
        let track = pitch_track(&signal, f0, 1.0, 1.9, 0.03, hop);
        assert!(modulation(&track, f0, hop).1 < 2.0);
    }

    #[test]
    fn velocity_sets_the_bow_speed_and_the_level_follows() {
        for (name, params, low, high) in bowed() {
            let key = (low + high) / 2;
            let params = plain(params);
            let loud = note(voice(params), on(key, 1.0), 2.0, 1.2);
            let soft = note(voice(params), on(key, 0.4), 2.0, 1.2);
            let (loud, soft) = (between(&loud, 0.6, 1.2), between(&soft, 0.6, 1.2));
            let difference = decibels(rms(loud) / rms(soft));
            let expected = 0.6 * params.velocity_range_db;
            let f0 = key_hz(f64::from(key));
            let (bright, dark) = (harmonic_centroid(loud, f0), harmonic_centroid(soft, f0));
            println!("{name}: {difference:.1} dB, centroid {bright:.0} Hz against {dark:.0} Hz");
            assert!(
                (difference - expected).abs() < 2.0,
                "{name}: {difference} dB against {expected}"
            );
            // Played louder nearer the bridge, it is brighter.
            assert!(bright > 1.03 * dark, "{name}: {bright} against {dark}");
        }
    }

    #[test]
    fn an_accent_is_louder_and_bites() {
        for (name, params, low, high) in bowed() {
            let key = (low + high) / 2;
            let params = plain(params);
            // The string's own sound: the body's resonances build up over
            // tens of milliseconds and would hide the attack.
            let params = BowedParams {
                body_mix: 0.0,
                ..params
            };
            let plain_note = note(voice(params), on(key, 0.7), 2.0, 1.2);
            let strong = note(voice(params), accented(key, 0.7), 2.0, 1.2);
            let sustain =
                decibels(rms(between(&strong, 0.6, 1.2)) / rms(between(&plain_note, 0.6, 1.2)));
            // The bite: the attack's first 60 ms, against the plain one's,
            // beyond what the level alone gives.
            let attack =
                decibels(rms(between(&strong, 0.0, 0.06)) / rms(between(&plain_note, 0.0, 0.06)));
            println!("{name}: accent {sustain:.1} dB sustained, {attack:.1} dB in the attack");
            assert!(
                (sustain - params.accent_db).abs() < 1.5,
                "{name}: {sustain}"
            );
            assert!(attack > sustain + 0.5, "{name}: {attack} against {sustain}");
        }
    }

    #[test]
    fn pressure_and_brightness_change_the_timbre() {
        let params = plain(BowedParams::violin());
        let key = 67;
        let centroid_of = |params: BowedParams, brightness: Option<f64>| {
            let mut synth = synth_of(voice(params));
            synth.note_on(NoteRef::new(0, key), 0.8);
            if let Some(value) = brightness {
                for voice in synth.voices_mut() {
                    Voice::<f64>::expression(voice, Expression::Brightness, value);
                }
            }
            let signal = play(&mut synth, &[], (1.2 * RATE) as usize, 256);
            harmonic_centroid(between(&signal, 0.6, 1.2), key_hz(f64::from(key)))
        };
        let light = centroid_of(
            BowedParams {
                pressure: 0.0,
                ..params
            },
            None,
        );
        let heavy = centroid_of(
            BowedParams {
                pressure: 1.0,
                ..params
            },
            None,
        );
        let tasto = centroid_of(params, Some(0.0));
        let ponticello = centroid_of(params, Some(1.0));
        println!(
            "pressure: {light:.0} Hz light, {heavy:.0} Hz heavy; {tasto:.0} Hz sul tasto, {ponticello:.0} Hz sul ponticello"
        );
        assert!(heavy > light, "{heavy} against {light}");
        assert!(ponticello > 1.2 * tasto, "{ponticello} against {tasto}");
    }

    #[test]
    fn legato_is_a_change_of_finger_and_only_portamento_slides() {
        for (name, params, low, high) in bowed() {
            let params = plain(params);
            let middle = (low + high) / 2;
            for (first, second) in [(middle, middle + 2), (middle + 4, middle - 1)] {
                let signal = slurred(params, first, second);
                // The level through the slur stays within a few decibels of
                // the quieter of the two notes (the body favours some
                // pitches).
                let before = rms(between(&signal, 0.8, 1.0));
                let after = rms(between(&signal, 1.4, 1.6));
                let through = levels(between(&signal, 1.0, 1.2), 0.01);
                let dip = through.iter().fold(f64::MAX, |m, &x| m.min(x));
                let dip = decibels(dip / before.min(after));
                // A finger stops the string at once: the string's pitch
                // (heard without the body, whose resonances ring on at the
                // old note for a few tens of milliseconds, as an
                // instrument's do) is the new note's within 30 cents, as the
                // slips' own jitter allows, within 20 ms, with at most three
                // irregular periods between the notes as it reorganises (a
                // slide passes steadily through a dozen or more; see
                // below).
                let (f0, f1) = (key_hz(f64::from(first)), key_hz(f64::from(second)));
                let string = slurred(
                    BowedParams {
                        body_mix: 0.0,
                        ..params
                    },
                    first,
                    second,
                );
                let (settled, gliding) = settling(&string, 1.0, f0, f1, 30.0);
                let error = cents(frequency_near(between(&signal, 1.3, 1.8), f1), f1);
                println!(
                    "{name} {first} to {second}: dip {dip:.1} dB, at the new pitch after {:.1} ms with {gliding} periods between, then {error:.2} cents",
                    1000.0 * settled
                );
                assert!(dip > -6.0, "{name}: {dip} dB");
                assert!(error.abs() < 5.0, "{name}: {error} cents");
                assert!(settled < 0.02, "{name}: {settled} s");
                assert!(gliding <= 3, "{name}: {gliding} periods between");
            }
            // One voice played both.
            let mut synth = synth_of(voice(params));
            synth.set_legato(true);
            play(
                &mut synth,
                &[(0, on(middle, 0.8)), (4_800, on(middle + 2, 0.8))],
                9_600,
                256,
            );
            assert_eq!(synth.active_voices(), 1);
        }
        // With portamento the finger slides: 15 ms into a slide of a fourth
        // over 30 ms the pitch is between the notes.
        let params = BowedParams {
            portamento: true,
            ..plain(BowedParams::violin())
        };
        let signal = slurred(params, 69, 74);
        let middle = frequency_near(between(&signal, 1.005, 1.02), key_hz(71.5));
        let semitones = 12.0 * (middle / key_hz(69.0)).log2();
        let (settled, gliding) = settling(&signal, 1.0, key_hz(69.0), key_hz(74.0), 30.0);
        println!(
            "violin portamento: {semitones:.2} semitones up 12 ms in, settled after {:.0} ms with {gliding} periods between",
            1000.0 * settled
        );
        assert!(semitones > 0.5 && semitones < 4.5, "{semitones}");
        assert!(gliding >= 8, "{gliding}");
    }

    #[test]
    fn a_change_of_finger_has_no_click_thump_or_burst() {
        // The finger's knock and the fade from the old length to the new
        // leave no sample stepping further than the notes' own largest
        // steps by more than half again, no 2 ms window above 8 kHz more
        // than 10 dB over the notes' own level there, and nothing below
        // half the lower note and the body's air resonance within 20 dB of
        // the tone.
        for (name, params, low, high) in bowed() {
            let middle = (low + high) / 2;
            for (first, second) in [(middle, middle + 2), (middle + 4, middle - 1)] {
                let signal = slurred(held(params), first, second);
                let steps = largest_jump(between(&signal, 0.6, 0.98))
                    .max(largest_jump(between(&signal, 1.3, 1.9)));
                let change = largest_jump(between(&signal, 0.995, 1.06));
                let hf = high_band_levels(&signal, 8_000.0, 0.002);
                let quiet: Vec<f64> = hf[300..490].iter().chain(&hf[650..950]).copied().collect();
                let reference = quiet.iter().sum::<f64>() / quiet.len() as f64;
                let burst = hf[495..530].iter().fold(0.0f64, |m, &x| m.max(x));
                // Below the body's air resonance, which a change of finger
                // rings, as on the instrument.
                let lowest = key_hz(f64::from(first.min(second)));
                let below = (0.5 * lowest).min(0.8 * params.body[0].frequency);
                let thump = levels(&band_passed(&signal, 10.0, below), 0.005);
                let thump = thump[195..230].iter().fold(0.0f64, |m, &x| m.max(x));
                let tone = rms(between(&signal, 0.6, 0.98));
                println!(
                    "{name} {first} to {second}: largest step {:.2} of the notes', high band {:.1} dB, low band {:.1} dB under the tone",
                    change / steps,
                    decibels(burst / reference),
                    -decibels(thump / tone)
                );
                assert!(change < 1.5 * steps, "{name}: {change} against {steps}");
                assert!(decibels(burst / reference) < 10.0, "{name}");
                assert!(decibels(thump / tone) < -18.0, "{name}");
            }
        }
    }

    #[test]
    fn an_accented_attack_bites_without_a_thump() {
        // A martelé drags the string hard at its start, which moves it
        // slowly as well as fast; the body does not radiate that, so
        // nothing below half its air resonance comes within 18 dB of the
        // held tone (before the body's radiation was modelled it came
        // within 8 dB on the violin).
        for (name, params, low, high) in bowed() {
            let key = (low + high) / 2;
            let signal = note(voice(params), accented(key, 0.9), 1.0, 1.0);
            let below = 0.5 * params.body[0].frequency;
            let thump = levels(&band_passed(&signal, 10.0, below), 0.005);
            let thump = thump[..20].iter().fold(0.0f64, |m, &x| m.max(x));
            let tone = rms(between(&signal, 0.5, 0.9));
            println!(
                "{name}: accented attack {:.1} dB under the tone below {below:.0} Hz",
                -decibels(thump / tone)
            );
            assert!(decibels(thump / tone) < -18.0, "{name}");
        }
    }

    #[test]
    fn the_violins_body_resonates_at_its_modes_and_its_wood() {
        // The body's response to a tap on the bridge: a peak at each named
        // resonance, within 3% of where it was put (A0, the centre bout,
        // B1-, B1+ and C4; above them the wood's modes crowd the named
        // ones, and the broad bridge hill is too wide to peak), and between 800 Hz and 7 kHz the many uneven peaks
        // of the wood: at least a dozen more than the named modes give.
        let params = BowedParams::violin();
        let peaks_of = |params: BowedParams| {
            let mut tone = BowedTone::new();
            tone.prepare(RATE);
            tone.configure(&params);
            let len = 1 << 16;
            let mut response = vec![0.0; len];
            for (n, slot) in response.iter_mut().enumerate() {
                *slot = tone.resonate(if n == 0 { 1.0 } else { 0.0 });
            }
            let mut fft = crate::fft::RealFft::<f64>::new(len);
            let mut spectrum = vec![crate::fft::Complex::<f64>::ZERO; fft.spectrum_len()];
            fft.forward(&response, &mut spectrum);
            let magnitude: Vec<f64> = spectrum.iter().map(|c| c.norm()).collect();
            (magnitude, RATE / len as f64)
        };
        let (magnitude, bin) = peaks_of(params);
        let level_at = |hz: f64| magnitude[(hz / bin).round() as usize];
        for mode in &params.body[..5] {
            let (low, high) = (
                (0.97 * mode.frequency / bin) as usize,
                (1.03 * mode.frequency / bin) as usize,
            );
            let (at, peak) = (low..=high).fold((low, 0.0f64), |b, k| {
                if magnitude[k] > b.1 {
                    (k, magnitude[k])
                } else {
                    b
                }
            });
            let local = magnitude[at - 1] < peak && magnitude[at + 1] < peak;
            println!(
                "violin body: {:.0} Hz peaks at {:.0} Hz, {:.1} dB",
                mode.frequency,
                at as f64 * bin,
                decibels(peak)
            );
            assert!(local && at > low && at < high, "{} Hz", mode.frequency);
        }
        let count = |magnitude: &[f64]| {
            let (from, to) = ((800.0 / bin) as usize, (7_000.0 / bin) as usize);
            (from..to)
                .filter(|&k| {
                    // A peak that stands at least 1 dB over the response
                    // 2% either side of it.
                    let spread = ((k as f64 * 0.02) as usize).max(2);
                    magnitude[k] > magnitude[k - 1]
                        && magnitude[k] > magnitude[k + 1]
                        && magnitude[k] > 1.12 * magnitude[k - spread]
                        && magnitude[k] > 1.12 * magnitude[k + spread]
                })
                .count()
        };
        let woody = count(&magnitude);
        let (smooth, _) = peaks_of(BowedParams {
            wood: 0.0,
            ..params
        });
        let named = count(&smooth);
        println!(
            "violin body: {woody} peaks between 800 Hz and 7 kHz, {named} without the wood; {:.1} dB at 275 Hz against {:.1} dB at 1.5 kHz",
            decibels(level_at(275.0)),
            decibels(level_at(1_500.0))
        );
        assert!(woody >= named + 12, "{woody} against {named}");
    }

    #[test]
    fn the_bow_scratches_as_it_bites() {
        // Bowed harder, or accented, the first tens of milliseconds carry
        // more noise against the tone, as the bow bites before the string
        // settles into its motion; held, the scratch is gone.
        let key = 67;
        let f0 = key_hz(f64::from(key));
        let noise_in = |params: BowedParams, act: Act, from: f64, to: f64| {
            let signal = note(voice(params), act, 1.0, 1.0);
            inharmonic_db(between(&signal, from, to), f0, 30.0, 2_048)
        };
        let params = BowedParams {
            body_mix: 0.0,
            ..held(BowedParams::violin())
        };
        let smooth = BowedParams {
            scratch: 0.0,
            ..params
        };
        let biting = noise_in(params, on(key, 1.0), 0.01, 0.0527);
        let soft = noise_in(params, on(key, 0.3), 0.01, 0.0527);
        let without = noise_in(smooth, on(key, 1.0), 0.01, 0.0527);
        let accent = noise_in(params, accented(key, 1.0), 0.01, 0.0527);
        let settled = noise_in(params, on(key, 1.0), 0.6, 0.6427);
        let settled_without = noise_in(smooth, on(key, 1.0), 0.6, 0.6427);
        println!(
            "violin attack noise: {biting:.1} dB at full velocity, {soft:.1} soft, {accent:.1} accented, {without:.1} without the scratch; held {settled:.1} dB, {settled_without:.1} without"
        );
        assert!(biting > without + 3.0, "{biting} against {without}");
        assert!(biting > soft + 1.0, "{biting} against {soft}");
        assert!(accent > biting, "{accent} against {biting}");
        assert!(
            (settled - settled_without).abs() < 1.5,
            "{settled} {settled_without}"
        );
    }

    #[test]
    fn a_held_note_drifts_wavers_and_swells_as_designed() {
        for (name, params, low, high) in bowed() {
            let key = (low + high) / 2;
            let f0 = key_hz(f64::from(key));
            let params = held(params);
            let drifting = note(voice(params), on(key, 0.7), 6.0, 6.0);
            let track = pitch_track(&drifting, f0, 1.0, 5.9, 0.05, 0.025);
            let cents_track: Vec<f64> = track.iter().map(|&f| cents(f, f0)).collect();
            let mean = cents_track.iter().sum::<f64>() / cents_track.len() as f64;
            let drift = (cents_track.iter().map(|c| (c - mean).powi(2)).sum::<f64>()
                / cents_track.len() as f64)
                .sqrt();
            let swaying = levels(between(&drifting, 1.0, 5.9), 0.05);
            let level_mean =
                swaying.iter().map(|&x| decibels(x)).sum::<f64>() / swaying.len() as f64;
            let waver = (swaying
                .iter()
                .map(|&x| (decibels(x) - level_mean).powi(2))
                .sum::<f64>()
                / swaying.len() as f64)
                .sqrt();
            let swelling = note(
                voice(BowedParams {
                    drift: 0.0,
                    fluctuation: 0.0,
                    ..params
                }),
                on(key, 0.7),
                6.0,
                6.0,
            );
            let swell =
                decibels(rms(between(&swelling, 0.7, 1.1)) / rms(between(&swelling, 5.0, 5.9)));
            println!(
                "{name}: drift {drift:.2} cents (set {}), waver {waver:.2} dB, swell {swell:.2} dB",
                params.drift
            );
            // The drift's RMS within a factor of two either way, over a few
            // seconds of a slow random motion; a waver of the level and a
            // swell of the long note, heard but not a tremolo.
            assert!(
                drift > 0.5 * params.drift && drift < 2.0 * params.drift,
                "{name}: {drift}"
            );
            assert!(waver > 0.05 && waver < 2.0, "{name}: {waver}");
            assert!(swell > 0.2 && swell < 4.0, "{name}: {swell}");
        }
    }

    #[test]
    fn a_released_note_is_damped_without_a_click() {
        for (name, params, low, high) in bowed() {
            let key = (low + high) / 2;
            let params = plain(params);
            let f0 = key_hz(f64::from(key));
            let signal = note(voice(params), on(key, 0.8), 0.8, 1.6);
            let steady = largest_jump(between(&signal, 0.5, 0.8));
            let release = largest_jump(between(&signal, 0.8, 1.6));
            assert!(
                release <= 1.05 * steady,
                "{name}: {release} against {steady}"
            );
            let measured = t60_at(&signal, f0, 0.9, 0.9 + params.release);
            println!(
                "{name}: released in {measured:.2} s against {:.2}",
                params.release
            );
            assert!(
                (measured / params.release - 1.0).abs() < 0.35,
                "{name}: {measured}"
            );
            // And it falls silent: the voice is free again.
            let mut synth = synth_of(voice(params));
            play(
                &mut synth,
                &[(0, on(key, 0.8)), (4_800, off(key))],
                4 * 48_000,
                256,
            );
            assert_eq!(synth.active_voices(), 0, "{name}");
        }
    }

    #[test]
    fn a_section_is_several_players_apart() {
        let section = BowedParams {
            bow_noise: 0.0,
            ..BowedParams::string_ensemble()
        };
        let soloist = BowedParams {
            players: 1,
            ..section
        };
        let key = 60;
        let f0 = key_hz(f64::from(key));
        // The players' tunings beat: the fundamental's level moves more.
        let sway = |params: BowedParams| {
            let signal = note(voice(params), on(key, 0.8), 3.0, 2.5);
            let levels: Vec<f64> = (0..40)
                .map(|step| {
                    let t = 0.5 + 0.05 * f64::from(step);
                    decibels(amplitude_at(between(&signal, t, t + 0.05), f0))
                })
                .collect();
            let mean = levels.iter().sum::<f64>() / levels.len() as f64;
            (levels.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / levels.len() as f64).sqrt()
        };
        let (many, one) = (sway(section), sway(soloist));
        println!("section sways {many:.2} dB, a soloist {one:.2} dB");
        assert!(many > 2.0 * one && many > 1.0, "{many} against {one}");
    }

    #[test]
    fn the_controllers_shape_a_sounding_note() {
        let params = plain(BowedParams::cello());
        let key = 48;
        let run = |shape: &dyn Fn(&mut BowedString)| {
            let mut synth = synth_of(voice(params));
            synth.note_on(NoteRef::new(0, key), 1.0);
            play(&mut synth, &[], (0.5 * RATE) as usize, 256);
            for voice in synth.voices_mut() {
                shape(voice);
            }
            let signal = play(&mut synth, &[], (1.0 * RATE) as usize, 256);
            rms(between(&signal, 0.5, 1.0))
        };
        let full = run(&|_| {});
        let half = run(&|voice| Voice::<f64>::expression(voice, Expression::Pressure, 0.5));
        let pedal = run(&|voice| {
            Voice::<f64>::midi(
                voice,
                MidiMessage::ControlChange {
                    channel: 0,
                    controller: 11,
                    value: 0,
                },
            );
        });
        println!(
            "pressure 0.5: {:.1} dB; expression pedal down: {:.1} dB",
            decibels(half / full),
            decibels(pedal / full)
        );
        assert!((decibels(half / full) + 6.0).abs() < 1.5);
        assert!(decibels(pedal / full) < -40.0);
    }

    #[test]
    fn notes_start_and_are_taken_over_without_a_click() {
        for (name, params, low, high) in bowed().into_iter().chain(plucked()) {
            let key = (low + high) / 2;
            let alone = note(voice(params), on(key, 1.0), 2.0, 0.6);
            let steady = largest_jump(between(&alone, 0.3, 0.6));
            // The note rises from silence.
            assert!(alone[0].abs() < 1e-3 * measure::peak(&alone), "{name}");
            let steal = (0.4 * RATE) as usize;
            let other = key - 5;
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
            let attack = largest_jump(between(&alone, 0.0, 0.3));
            assert!(
                at_steal <= 1.05 * steady.max(attack),
                "{name}: {at_steal} at the steal against {steady}"
            );
            assert!(measure::peak(&residual[steal + 400..]) < 1e-9, "{name}");
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(48, 0.9)),
            (1_000, accented(55, 0.7)),
            (7_000, on(60, 0.5)),
            (9_001, off(48)),
            (12_000, on(48, 1.0)),
            (15_000, Act::Bend(1.0)),
            (20_000, off(55)),
        ];
        for (name, params, _, _) in bowed().into_iter().chain(plucked()) {
            let run = |block| {
                play(
                    &mut synth_of([BowedString::new(params), BowedString::new(params)]),
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
            BowedParams {
                strings: [f64::NAN, 0.0, 1e9, -5.0],
                bow_position: f64::NAN,
                pressure: f64::INFINITY,
                attack: 0.0,
                bow_noise: 1e9,
                decay: f64::INFINITY,
                decay_per_octave: f64::NAN,
                high_decay: -1.0,
                release: f64::NAN,
                vibrato_rate: 1e9,
                vibrato_depth: f64::INFINITY,
                vibrato_delay: -1.0,
                vibrato_rise: 0.0,
                legato_time: f64::NAN,
                portamento: true,
                drift: f64::INFINITY,
                fluctuation: 1e9,
                swell: f64::NAN,
                scratch: -1e9,
                body: [BodyMode::new(f64::NAN, f64::INFINITY, 1e9); BODY_MODES],
                wood: f64::INFINITY,
                body_mix: f64::NAN,
                tone: 0.0,
                velocity_range_db: -5.0,
                accent_db: 1e9,
                accent_bite: f64::INFINITY,
                players: 1_000,
                spread: f64::NAN,
                pizzicato: false,
                pluck_time: 0.0,
                level: f64::NAN,
            },
            BowedParams {
                pressure: 1.0,
                bow_noise: 1.0,
                accent_db: 24.0,
                accent_bite: 4.0,
                level: 4.0,
                body_mix: 1.0,
                vibrato_depth: 100.0,
                body: [BodyMode::new(20.0, 50.0, 4.0); BODY_MODES],
                wood: 1.0,
                drift: 20.0,
                fluctuation: 0.3,
                swell: 0.5,
                scratch: 1.0,
                players: MAX_PLAYERS,
                spread: 50.0,
                ..BowedParams::contrabass()
            },
            BowedParams {
                pizzicato: true,
                ..BowedParams::violin()
            },
        ];
        for params in extreme {
            let mut synth = synth_of([BowedString::new(params), BowedString::new(params)]);
            synth.set_legato(true);
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
                measure::finite_within(&signal, 1e3),
                "{}",
                measure::peak(&signal)
            );
        }
    }
}
