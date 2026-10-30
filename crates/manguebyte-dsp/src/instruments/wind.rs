//! Wind instruments by digital waveguide: flute, clarinet, saxophone,
//! trumpet and trombone.

use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_midi::MidiMessage;

use super::common::{
    Handover, Noise, SILENCE, Tone, add_to_every_channel, db, flush, key_hz, setting, t60_factor,
    velocity_gain,
};
use super::modelling::{
    Bump, DcBlock, Dynamics, Glide, Life, Loss, Switch, Vibrato, VibratoShape, cubic_delay_for,
};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{DelayLine, ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice};

/// The lowest note a bore plays, in hertz. Lower keys sound here.
const LOWEST_HZ: f64 = 25.0;

/// The highest, as a fraction of the sample rate (6 kHz at 48 kHz).
const HIGHEST_RATIO: f64 = 0.125;

/// How long the player's ear takes to bring a note to its pitch, and how
/// long after a note starts or slurs it begins to listen.
const EAR_SECONDS: f64 = 0.1;
const EAR_WAIT: f64 = 0.2;

/// How much longer than the period a brass player's ear holds the bore, to
/// make up for the lips' pull (measured: it settles there for every note).
const BRASS_EAR: f64 = 1.0128;

/// How long the breath takes to follow a controller.
const BREATH_SECONDS: f64 = 0.008;

/// The highest resonance the reed or the lips are given, as a fraction of
/// the sample rate: there the valve's scheme is stable and accurate.
const VALVE_LIMIT: f64 = 0.15;

/// The corner of the high-pass in the loop of a bore whose reflection does
/// not invert, as a fraction of the note's frequency.
const LOOP_HIGHPASS: f64 = 0.25;

/// On a harmonic bore, the bell's cut-off is at most this many times the
/// note's frequency, and at least this many: below it the bell lets out the
/// fundamental itself, and the note does not speak.
const BELL_HARMONICS: f64 = 4.0;
const BELL_LEAST: f64 = 2.5;

/// The flute's open ends let out what is above this many times the note: a
/// low note would otherwise find its octave as strong as itself, and leap.
const FLUTE_BELL_HARMONICS: f64 = 6.0;

/// The saxophone's reed resonates at least this many times above the note.
const REED_HARMONICS: f64 = 6.0;

/// How far the reed's or the lips' channel opens, at the least embouchure
/// and over its range, in the units where the bore's impedance is 1.
const OPENING_LEAST: f64 = 0.2;
const OPENING_RANGE: f64 = 0.4;

/// The blowing pressure on a reed, over the pressure that would close it:
/// at no breath and over the breath's range. A reed on a cylinder speaks
/// above a third; on a harmonic bore, higher (measured).
const REED_LEAST: f64 = 0.3;
const REED_RANGE: f64 = 0.55;
const CONE_LEAST: f64 = 0.6;
const CONE_RANGE: f64 = 0.25;
/// How much more a reed takes for each octave above A3.
const REED_LIFT: f64 = 0.05;

/// The same for the lips, how far they are parted with no pressure, and how
/// much more pressure each octave above A3 takes.
const LIPS_LEAST: f64 = 0.3;
const LIPS_RANGE: f64 = 0.6;
const LIPS_REST: f64 = 0.1;
const LIPS_LIFT: f64 = 0.12;

/// How much the breath's turbulence moves the pressure at a reed or the
/// lips, and how much of it is heard directly, as air.
const TURBULENCE: f64 = 0.05;
const REED_AIR: f64 = 0.01;

/// The flute's jet: how long it takes to reach the edge as a fraction of the
/// period (a quarter, which with the drive's quarter period of lead puts
/// the drive in phase at the fundamental), and how sharply it switches
/// between in and out (the slope of its profile, a hyperbolic tangent, at
/// its centre).
const JET_RATIO: f64 = 0.25;
/// The corner of the jet's response, as a multiple of the note: a jet's
/// waves grow on it most near the fundamental and are damped above (N. H.
/// Fletcher and T. D. Rossing, section 16.3), so it does not cancel the
/// harmonics its own split makes.
const JET_FILTER: f64 = 0.7;
const JET_GAIN: f64 = 3.0;
/// How far the jet's aim ([`WindParams::jet_offset`]) moves it off the edge,
/// in the units of its profile, played softly and loudly: a flautist
/// centres the jet for a pure soft tone and lets it ride off the edge for a
/// strong one, which brings the even harmonics up (N. H. Fletcher and L. M.
/// Douglas, "Harmonic generation in organ pipes, recorders, and flutes",
/// JASA 68, 1980: the second harmonic grows with the jet's offset).
const JET_OFFSET: (f64, f64) = (0.3, 0.7);
/// How far above the threshold of oscillation the jet drives the pipe, at
/// the softest breath and at the loudest, for an embouchure of one half: at
/// the threshold the tone is a sine, and the further above it the harder
/// the jet switches between in and out, and the richer the tone. The
/// drive is set against what the pipe loses in a trip at the note, so every
/// note of the range answers the breath alike; a wider embouchure drives it
/// up to half as hard again, a narrower one down to a half less.
const JET_DRIVE: (f64, f64) = (1.4, 2.4);
/// How fast the jet's travel time follows the note: the player's lips and
/// breath, not a key, set it.
const JET_FOLLOW_SECONDS: f64 = 0.005;
/// How much of the breath's turbulence disturbs the jet, which the jet then
/// amplifies and the pipe colours (P. R. Cook's flute puts the breath's
/// noise there, "A meta-wind-instrument physical model", ICMC 1992), and
/// how much is heard directly from the embouchure.
const JET_NOISE: f64 = 0.065;
const FLUTE_BREATH: f64 = 0.075;
/// How much of the jet's drive the embouchure radiates directly, against
/// what the pipe's open end lets out.
const EMBOUCHURE: f64 = 1.0;
/// How hard the tongue's release kicks a flute's pipe at the start of a note,
/// at full articulation, against the swing the pipe settles to: the jet
/// reaching the edge at once is a step of flow, which sets the pipe ringing
/// before the jet has taken it over, so the note speaks in a few tens of
/// milliseconds rather than growing out of the breath's noise.
const TONGUE_KICK: f64 = 2.0;
/// How much energy the vortices shed at the embouchure's edge take, in the
/// units where the jet's own drive at the threshold is 1.
const VORTEX: f64 = 0.7;
/// The breath's noise is turbulence: broad, between a high-pass at
/// [`BREATH_LOW`] and a low-pass whose corner rises with the jet's speed
/// (the eddies of a faster jet are smaller and quicker), from the first of
/// [`BREATH_HIGH`] at no breath by the second at a breath of 1.
const BREATH_LOW: f64 = 500.0;
const BREATH_HIGH: (f64, f64) = (3_000.0, 7_000.0);
/// The flute's radiated tilt, as [`BRASS_TILT`] is the brass's: soft, the
/// tone is little more than its first harmonics; loud, the jet's switching
/// reaches the bell.
const FLUTE_TILT: (f64, f64) = (3.0, 10.0);

/// How long a key or a tone hole takes to change the bore's length at a slur
/// (a [`Switch`]), and a valve, which travels for longer: for a moment the
/// bore is two lengths at once.
const KEY_SECONDS: f64 = 0.003;
const VALVE_SECONDS: f64 = 0.004;

/// The break in the air at a slur without portamento: how long it lasts,
/// and how far it lowers the breath at the least articulation and over the
/// articulation's range. A player's breath carries on through a slur, but
/// the bore's new length takes a few trips to speak, and the breath dips
/// as the fingers move.
const BREAK_SECONDS: f64 = 0.03;
const BREAK_LEAST: f64 = 0.1;
const BREAK_RANGE: f64 = 0.25;

/// The tongue: how far above its level the breath is pushed at the start of
/// a note at full articulation and velocity (falling away over
/// [`PUSH_SECONDS`]), how long the burst of air at the start lasts (its 60 dB
/// fall: the flute's chiff, the air before a reed or the lips speak), and how
/// many times louder it makes the breath's noise at its start.
const TONGUE_PUSH: f64 = 0.3;
const CHIFF_SECONDS: f64 = 0.08;
/// How long the burst of air takes to rise: quick, but not a click.
const CHIFF_RISE: f64 = 0.002;
const CHIFF_GAIN: f64 = 4.0;

/// The lips' buzz before a brass note speaks: how far below the note it
/// starts at full articulation and velocity, in cents, and how long it takes
/// to rise into the note (its 60 dB fall).
const SCOOP_CENTS: f64 = 40.0;
const SCOOP_SECONDS: f64 = 0.08;

/// How many times more than the breath its noise wavers.
const NOISE_WAVER: f64 = 3.0;

/// How long the extra pressure a high note takes moves to a slurred note's,
/// and the level of a slurred note to its velocity's.
const LIFT_SECONDS: f64 = 0.01;
const GAIN_SECONDS: f64 = 0.01;

/// How long after a slur the ear starts to listen again: soon, so that it
/// has done its work while the note is still speaking.
const EAR_WAIT_SLUR: f64 = 0.04;

/// The most the dynamics may push the breath past its loudest, and how much
/// faster an accented note's breath builds.
const BREATH_MOST: f64 = 1.5;
const ACCENT_ATTACK: f64 = 0.5;

/// How long the tongue's push takes to fall away.
const PUSH_SECONDS: f64 = 0.08;

/// How far the crests of a blaring brass note can run ahead, as a fraction
/// of the period and at most in samples, and the bore's pressure at which
/// they run half that far.
const STEEPENING: f64 = 0.04;
const STEEP_MOST: f64 = 120.0;
const BLARE_SCALE: f64 = 12.0;

/// The tilt of the sound a brass instrument or a reed radiates, as the
/// cut-off of a low-pass in multiples of the note, at no breath and at
/// full: softly blown, little reaches the bell above the first harmonics
/// (the steepening that makes a loud note blare is the other end of it).
const BRASS_TILT: (f64, f64) = (2.0, 14.0);
/// Below its cut-off a bell or an open end radiates less the lower the
/// frequency (N. H. Fletcher and T. D. Rossing, sections 8.7 and 14.5): a
/// high-pass at this multiple of the note, of the first order for the
/// saxophone's open holes and bell, of the second for a brass bell, which
/// reflects nearly all of the lowest harmonics, so that a brass
/// instrument's fundamental is heard below the harmonics above it, as it
/// is. None for the clarinet, whose open holes radiate its low register
/// well, nor for the flute, heard from its embouchure.
const BRASS_RADIATION: f64 = 2.0;
const SAXOPHONE_RADIATION: f64 = 1.0;
const SAXOPHONE_TILT: (f64, f64) = (3.0, 20.0);
const CLARINET_TILT: (f64, f64) = (3.0, 16.0);

/// What blows a [`WindVoice`]: the mechanism at the mouth and the shape of
/// the bore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindKind {
    /// An air jet across an open hole, into a pipe open at both ends: every
    /// harmonic, a breathy tone.
    Flute,
    /// A single reed on a cylindrical bore, closed at the reed: the odd
    /// harmonics, strong in the low register.
    Clarinet,
    /// A single reed on a conical bore: every harmonic, a reedy, vocal tone.
    Saxophone,
    /// The lips, buzzing into a bore flared so that its resonances fall on a
    /// harmonic series: brass.
    Brass,
}

/// The settings of a [`WindVoice`].
///
/// Times are in seconds, frequencies in hertz and levels in decibels
/// unless said otherwise. Out-of-range values are limited when the
/// settings are taken (see [`WindVoice::set_params`]), and a value that is
/// not a number takes the default of [`clarinet`](Self::clarinet).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindParams {
    /// What blows it.
    pub kind: WindKind,
    /// The breath at the softest, 0 to 1 of the range in which the
    /// instrument speaks: velocity, pressure on the key and the breath
    /// controller move between this and [`breath_loud`](Self::breath_loud).
    pub breath_soft: f64,
    /// The breath at the loudest, 0 to 1. More breath is louder and
    /// brighter: a reed beats against its lay, the lips close harder.
    pub breath_loud: f64,
    /// How far the reed or the lips open, 0 to 1: the embouchure. A wider
    /// opening lets more air through, louder and brighter. For the flute,
    /// how strongly the jet drives the pipe.
    pub opening: f64,
    /// The reed's resonance, 500 to 6 000 (the saxophonist's reed is kept at
    /// least six times above the note); for brass, the lips' as a multiple
    /// of the note's frequency, 0.5 to 1.5; unused by the flute.
    pub reed_frequency: f64,
    /// The damping of that resonance, the inverse of its Q, 0.05 to 2.
    pub reed_damping: f64,
    /// The gain of a trip down the bore and back at low frequencies, 0.5
    /// to 0.999: the bore's wall losses.
    pub bore_gain: f64,
    /// Above this frequency the bell (or the open holes) lets the sound out
    /// instead of reflecting it, 200 to 10 000: a lower one is darker. On
    /// a saxophone and on brass it is at most four times the note.
    pub bell: f64,
    /// For the flute, how far off the edge the jet points, -1 to 1: off
    /// centre the jet gives even harmonics as well as odd.
    pub jet_offset: f64,
    /// How much noise the breath carries, 0 to 1: turbulence at the reed or
    /// the lips, heard as breathiness; for the flute, the turbulence of the
    /// jet, which the jet amplifies and the pipe colours, and the breath
    /// heard from the embouchure. It grows with the breath, more slowly
    /// than the tone, so a soft note is breathier.
    pub noise: f64,
    /// How long the breath takes to build at the start of a note, 0.002 to
    /// 1.
    pub attack: f64,
    /// How long it takes to stop once the note is released, 0.005 to 2.
    pub release: f64,
    /// The vibrato's rate, 0 to 12.
    pub vibrato_rate: f64,
    /// Its depth in pitch, the swing either way in cents, 0 to 100.
    pub vibrato_depth: f64,
    /// How much it also swings the breath, 0 to 1: a flute's vibrato is as
    /// much in the breath as in the pitch.
    pub tremolo: f64,
    /// How long after the start of a note the vibrato begins, 0 to 4.
    pub vibrato_delay: f64,
    /// How long it then takes to grow, 0.01 to 4.
    pub vibrato_rise: f64,
    /// How long the pitch takes to slide to a slurred note when
    /// [`portamento`](Self::portamento) is on, 0 to 1.
    pub legato_time: f64,
    /// Whether slurred notes slide into each other, the bore's length
    /// gliding over [`legato_time`](Self::legato_time), as a trombone's
    /// slide can. Off, a slur changes the bore's length at once, as a key or
    /// a valve does (and as a trombonist does, tonguing softly as the slide
    /// moves), with a short break in the air while the new note speaks.
    pub portamento: bool,
    /// How hard the tongue starts a note, 0 to 1: the breath's push and the
    /// burst of air before the tone speaks (the flute's chiff), more at a
    /// higher velocity and on an accent; for brass, also how far below the
    /// note the lips start, buzzing up into it. At a slur, how much the air
    /// breaks.
    pub articulation: f64,
    /// How far the pitch drifts, slowly, as a player's does, in cents (its
    /// RMS), 0 to 20.
    pub drift: f64,
    /// How much the breath wavers, slowly, as a fraction of it (its RMS), 0
    /// to 0.3; the breath's noise wavers three times as much. A louder
    /// breath is brighter, so the tone's colour wavers with it.
    pub fluctuation: f64,
    /// How much a held note swells after its attack, at its height about
    /// 0.9 s in, and then eases back over a few seconds, as a fraction of
    /// the breath, 0 to 0.5.
    pub swell: f64,
    /// For brass, how much the sound steepens as it travels down the bore
    /// at high levels, 0 to 1: the blare of a loud trumpet.
    pub brassiness: f64,
    /// The cut-off of the final low-pass, 500 to 20 000.
    pub tone: f64,
    /// How much quieter the lowest velocity is than the highest, 0 to 60,
    /// on top of what the breath does.
    pub velocity_range_db: f64,
    /// How much louder an accented note is, 0 to 24.
    pub accent_db: f64,
    /// How much an accent's tongue pushes the breath above its level at
    /// the start, 0 to 2, falling away over 80 ms; the breath also builds
    /// twice as fast.
    pub accent_push: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl WindParams {
    /// A clarinet: a cane reed resonating near 2.5 kHz on a cylindrical bore
    /// whose open holes and bell let out what is above 1.5 kHz (Ph.
    /// Guillemain, J. Kergomard and T. Voinier, JASA 118, 2005; N. H.
    /// Fletcher and T. D. Rossing, "The Physics of Musical Instruments",
    /// chapter 15). No vibrato, as clarinettists play.
    pub fn clarinet() -> Self {
        Self {
            kind: WindKind::Clarinet,
            breath_soft: 0.15,
            breath_loud: 0.8,
            opening: 0.5,
            reed_frequency: 2_500.0,
            reed_damping: 0.4,
            bore_gain: 0.95,
            bell: 1_500.0,
            jet_offset: 0.0,
            noise: 0.15,
            attack: 0.03,
            release: 0.06,
            vibrato_rate: 5.0,
            vibrato_depth: 0.0,
            tremolo: 0.0,
            vibrato_delay: 0.4,
            vibrato_rise: 0.4,
            legato_time: 0.015,
            portamento: false,
            articulation: 0.5,
            drift: 1.5,
            fluctuation: 0.02,
            swell: 0.08,
            brassiness: 0.0,
            tone: 8_000.0,
            velocity_range_db: 12.0,
            accent_db: 3.0,
            accent_push: 0.6,
            level: 1.0,
        }
    }

    /// An alto saxophone: a reed on a conical bore, whose resonances fall on
    /// every harmonic of the note, brighter than the clarinet, with a jaw
    /// vibrato.
    pub fn saxophone() -> Self {
        Self {
            kind: WindKind::Saxophone,
            opening: 0.55,
            reed_frequency: 2_000.0,
            reed_damping: 0.35,
            bell: 2_500.0,
            noise: 0.2,
            vibrato_rate: 5.2,
            vibrato_depth: 15.0,
            vibrato_delay: 0.3,
            legato_time: 0.02,
            drift: 3.0,
            fluctuation: 0.04,
            swell: 0.1,
            breath_soft: 0.2,
            level: 1.1,
            ..Self::clarinet()
        }
    }

    /// A concert flute: a jet aimed a little off the edge of the
    /// embouchure hole, into a pipe open at both ends, breathy, with a
    /// vibrato mostly in the breath.
    pub fn flute() -> Self {
        Self {
            kind: WindKind::Flute,
            breath_soft: 0.45,
            breath_loud: 0.9,
            opening: 0.5,
            bore_gain: 0.96,
            bell: 3_000.0,
            jet_offset: 1.0,
            noise: 0.35,
            attack: 0.05,
            release: 0.08,
            vibrato_rate: 5.0,
            vibrato_depth: 8.0,
            tremolo: 0.25,
            vibrato_delay: 0.25,
            articulation: 0.6,
            drift: 3.0,
            fluctuation: 0.035,
            swell: 0.12,
            level: 0.21,
            ..Self::clarinet()
        }
    }

    /// A trumpet: lips buzzing below the note into a bright bell, blaring
    /// when loud.
    pub fn trumpet() -> Self {
        Self {
            kind: WindKind::Brass,
            breath_soft: 0.4,
            breath_loud: 0.85,
            opening: 0.65,
            reed_frequency: 0.8,
            reed_damping: 0.25,
            bore_gain: 0.98,
            bell: 1_800.0,
            noise: 0.08,
            attack: 0.025,
            release: 0.06,
            vibrato_rate: 5.5,
            vibrato_depth: 6.0,
            vibrato_delay: 0.4,
            legato_time: 0.02,
            articulation: 0.6,
            drift: 2.5,
            fluctuation: 0.03,
            swell: 0.1,
            brassiness: 0.6,
            level: 1.4,
            ..Self::clarinet()
        }
    }

    /// A trombone: a darker bell and a slower attack. Its slide can glide
    /// between slurred notes over 150 ms: turn
    /// [`portamento`](Self::portamento) on for that.
    pub fn trombone() -> Self {
        Self {
            bell: 900.0,
            attack: 0.04,
            legato_time: 0.15,
            brassiness: 0.45,
            vibrato_depth: 4.0,
            tone: 6_000.0,
            level: 1.55,
            ..Self::trumpet()
        }
    }

    /// The settings with every value limited to its range.
    fn limited(self) -> Self {
        let default = Self::clarinet();
        let reed = if self.kind == WindKind::Brass {
            setting(self.reed_frequency, 0.5, 1.5, 0.8)
        } else {
            setting(self.reed_frequency, 500.0, 6_000.0, default.reed_frequency)
        };
        Self {
            kind: self.kind,
            breath_soft: setting(self.breath_soft, 0.0, 1.0, default.breath_soft),
            breath_loud: setting(self.breath_loud, 0.0, 1.0, default.breath_loud),
            opening: setting(self.opening, 0.0, 1.0, default.opening),
            reed_frequency: reed,
            reed_damping: setting(self.reed_damping, 0.05, 2.0, default.reed_damping),
            bore_gain: setting(self.bore_gain, 0.5, 0.999, default.bore_gain),
            bell: setting(self.bell, 200.0, 10_000.0, default.bell),
            jet_offset: setting(self.jet_offset, -1.0, 1.0, 0.0),
            noise: setting(self.noise, 0.0, 1.0, default.noise),
            attack: setting(self.attack, 0.002, 1.0, default.attack),
            release: setting(self.release, 0.005, 2.0, default.release),
            vibrato_rate: setting(self.vibrato_rate, 0.0, 12.0, default.vibrato_rate),
            vibrato_depth: setting(self.vibrato_depth, 0.0, 100.0, 0.0),
            tremolo: setting(self.tremolo, 0.0, 1.0, 0.0),
            vibrato_delay: setting(self.vibrato_delay, 0.0, 4.0, default.vibrato_delay),
            vibrato_rise: setting(self.vibrato_rise, 0.01, 4.0, default.vibrato_rise),
            legato_time: setting(self.legato_time, 0.0, 1.0, default.legato_time),
            portamento: self.portamento,
            articulation: setting(self.articulation, 0.0, 1.0, default.articulation),
            drift: setting(self.drift, 0.0, 20.0, 0.0),
            fluctuation: setting(self.fluctuation, 0.0, 0.3, 0.0),
            swell: setting(self.swell, 0.0, 0.5, 0.0),
            brassiness: setting(self.brassiness, 0.0, 1.0, 0.0),
            tone: setting(self.tone, 500.0, 20_000.0, default.tone),
            velocity_range_db: setting(
                self.velocity_range_db,
                0.0,
                60.0,
                default.velocity_range_db,
            ),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            accent_push: setting(self.accent_push, 0.0, 2.0, default.accent_push),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    fn vibrato(&self) -> VibratoShape {
        VibratoShape {
            rate: self.vibrato_rate,
            depth_cents: 1.0,
            delay: self.vibrato_delay,
            rise: self.vibrato_rise,
            wander: 0.8,
            develop: 1.0,
        }
    }
}

impl Default for WindParams {
    fn default() -> Self {
        Self::clarinet()
    }
}

/// The flow through a valve whose opening is `opening` when the pressure
/// across it, with no flow, would be `drop`: the `u` that solves
/// `u = opening * sqrt(|drop - u|) * sign(drop - u)`.
///
/// This is the quasi-static flow of Bernoulli's law through a reed's or
/// the lips' channel, in units where the bore's characteristic impedance
/// is 1, so that the flow lowers the pressure across the valve by itself.
/// With the opening known from the past (the valve's motion is computed
/// one sample behind the flow), the equation is a quadratic in `u` and the
/// root is explicit: the scheme of Ph. Guillemain, J. Kergomard and T.
/// Voinier, "Real-time synthesis of clarinet-like instruments using
/// digital impedance models", JASA 118 (2005).
#[inline]
fn valve_flow(opening: f64, drop: f64) -> f64 {
    let w2 = opening * opening;
    if w2.is_nan() || w2 <= 0.0 || !drop.is_finite() {
        return 0.0;
    }
    let q = drop.abs();
    // u² + w²u - w²q = 0, written so that nothing cancels.
    let u = 2.0 * w2 * q / (w2 + (w2 * w2 + 4.0 * w2 * q).sqrt());
    if drop < 0.0 { -u } else { u }
}

/// A valve that moves: a reed, or the lips, as a mass on a spring driven
/// by the pressure across it, `x'' / w² + d x' / w + x = drive`.
///
/// Discretised by centred differences, so that the position at a sample
/// depends only on the drive of the sample before and the flow can be
/// solved explicitly ([`valve_flow`]), as Guillemain, Kergomard and
/// Voinier do.
#[derive(Debug, Clone, Copy, Default)]
struct Valve {
    x1: f64,
    x2: f64,
    drive1: f64,
    /// `x[n] = c_drive drive[n-1] + c1 x[n-1] - c2 x[n-2]`.
    c_drive: f64,
    c1: f64,
    c2: f64,
}

impl Valve {
    /// Tunes the valve to `frequency` hertz with damping `damping` (the
    /// inverse of its Q) at `rate`, at most [`VALVE_LIMIT`] of the rate.
    fn tune(&mut self, frequency: f64, damping: f64, rate: f64) {
        let wt = (TAU * frequency / rate).clamp(1e-4, TAU * VALVE_LIMIT);
        let damping = damping.clamp(0.01, 4.0);
        let a = 1.0 / (wt * wt);
        let b = damping / (2.0 * wt);
        self.c_drive = 1.0 / (a + b);
        self.c1 = (2.0 * a - 1.0) / (a + b);
        self.c2 = (a - b) / (a + b);
    }

    /// The position for this sample, from the drive of the last.
    #[inline]
    fn position(&mut self) -> f64 {
        let x = flush(self.c_drive * self.drive1 + self.c1 * self.x1 - self.c2 * self.x2);
        self.x2 = self.x1;
        self.x1 = x;
        x
    }

    /// The drive of this sample, which moves the valve on the next.
    #[inline]
    fn drive(&mut self, drive: f64) {
        self.drive1 = setting(drive, -1e3, 1e3, 0.0);
    }

    fn reset(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.drive1 = 0.0;
    }
}

/// One note: the bore, what blows it, and the player.
#[derive(Debug, Clone)]
struct WindTone {
    rate: f64,
    kind: WindKind,
    /// The bore, and the jet (flute).
    bore: DelayLine<f64>,
    jet: DelayLine<f64>,
    /// The bell's reflection and its memory; the loop's high-pass.
    bell: Loss,
    bell_memory: f64,
    /// The jet's low-pass, its memory, and the jet's delay at the pitch
    /// aimed at, that low-pass's taken off.
    jet_filter: Loss,
    jet_memory: [f64; 2],
    jet_offset_delay: f64,
    /// The jet's travel time, in periods' worth of samples, following the
    /// note; how fast; and how strongly the jet drives the pipe.
    jet_period: f64,
    jet_follow: f64,
    jet_strength: f64,
    jet_over: f64,
    jet_unit: f64,
    kick: Bump,
    kick_level: f64,
    vortex: f64,
    embouchure: f64,
    dc: DcBlock,
    radiation: [DcBlock; 2],
    valve: Valve,
    /// The embouchure, the flute's jet's aim, the bore's gain, and the
    /// pressure a high note adds, as aimed at and as it follows.
    opening: f64,
    jet_offset: f64,
    bore_gain: f64,
    lift: f64,
    lift_target: f64,
    lift_follow: f64,
    /// The pitch, gliding between slurred notes with portamento, the bend,
    /// and the ear's correction of the bore's length.
    glide: Glide,
    bend: f64,
    ear: f64,
    /// What the bore's read needs on top of its share of the period, at the
    /// pitch aimed at.
    bore_offset: f64,
    /// A slur's change of length: the fade from the old length's read, the
    /// key it was at and its read's offset.
    switch: Switch,
    from_key: f64,
    from_offset: f64,
    /// The ear: what it hears (the pressure at the mouth, its mean taken
    /// out and its harmonics turned down by two low-passes at the note, so
    /// that it crosses zero once a period), the last of it, and how long
    /// since its last mark.
    ear_dc: DcBlock,
    ear_filter: Loss,
    ear_memory: [f64; 2],
    /// How many samples the note has sounded, and how many the ear waits,
    /// so that it does not chase the attack's own transient.
    age: u32,
    ear_wait: u32,
    heard_last: f64,
    since_mark: f64,
    /// The breath: how far it has built, 0 to 1, how fast it comes and
    /// goes, whether the key is held, and the tongue's push.
    breath: f64,
    breath_rise: f64,
    breath_fall: f64,
    held: bool,
    push: f64,
    push_fall: f64,
    /// The breath's range.
    breath_soft: f64,
    breath_loud: f64,
    dynamics: Dynamics,
    /// The vibrato, swinging by a cent, and how far that is taken in pitch
    /// and in breath.
    vibrato: Vibrato,
    vibrato_depth: f64,
    tremolo: f64,
    /// The player's drift, waver and swell, and how much of each.
    life: Life,
    drift: f64,
    fluctuation: f64,
    swell: f64,
    /// The tongue: how hard, the burst of air at the start and how fast it
    /// falls, the lips' buzz below the note (brass) and how fast it rises.
    articulation: f64,
    chiff: f64,
    chiff_fall: f64,
    onset: f64,
    onset_rise: f64,
    scoop: f64,
    scoop_fall: f64,
    /// A slur's break in the air, and how deep it goes.
    gap: Bump,
    gap_depth: f64,
    /// The breath's turbulence: noise, its filters' memories, and how much.
    noise: Noise,
    turbulence: f64,
    noise_high: f64,
    noise_low: f64,
    noise_low_pole: f64,
    noise_level: f64,
    /// The jet's last flow, and the scale that makes its rate of change
    /// as strong as itself at the fundamental.
    jet_last: f64,
    jet_scale: f64,
    /// The bell's steepening at high levels (brass).
    steep: DelayLine<f64>,
    steep_most: f64,
    brassiness: f64,
    /// The radiated sound's tilt with the breath: the cut-offs at no breath
    /// and at full, in hertz, and the low-pass's memory.
    tilt: (f64, f64),
    tilt_memory: f64,
    tilt_second: f64,
    tone: StateVariableFilter<f64>,
    out_dc: DcBlock,
    level: f64,
    /// The level the velocity and an accent give, and the one it moves to
    /// at a slur, smoothly: a step would be a click.
    gain: f64,
    gain_target: f64,
    gain_follow: f64,
    follower: f64,
    follower_fall: f64,
    sounding: bool,
}

impl WindTone {
    fn new() -> Self {
        Self {
            rate: 48_000.0,
            kind: WindKind::Clarinet,
            bore: DelayLine::new(4),
            jet: DelayLine::new(4),
            bell: Loss::NONE,
            bell_memory: 0.0,
            jet_filter: Loss::NONE,
            jet_memory: [0.0; 2],
            jet_offset_delay: 0.0,
            jet_period: 100.0,
            jet_follow: 1.0,
            jet_strength: 1.0,
            jet_over: 1.0,
            jet_unit: 1.0,
            kick: Bump::new(),
            kick_level: 0.0,
            vortex: VORTEX,
            embouchure: EMBOUCHURE,
            dc: DcBlock::default(),
            radiation: [DcBlock::default(); 2],
            valve: Valve::default(),
            opening: 0.5,
            jet_offset: 0.0,
            bore_gain: 0.95,
            lift: 0.0,
            lift_target: 0.0,
            lift_follow: 1.0,
            glide: Glide::new(),
            bend: 0.0,
            ear: 1.0,
            bore_offset: 0.0,
            switch: Switch::new(),
            from_key: 60.0,
            from_offset: 0.0,
            ear_dc: DcBlock::default(),
            ear_filter: Loss::NONE,
            ear_memory: [0.0; 2],
            age: 0,
            ear_wait: 0,
            heard_last: 0.0,
            since_mark: 0.0,
            breath: 0.0,
            breath_rise: 1.0,
            breath_fall: 1.0,
            held: false,
            push: 0.0,
            push_fall: 0.0,
            breath_soft: 0.2,
            breath_loud: 0.8,
            dynamics: Dynamics::new(),
            vibrato: Vibrato::new(),
            vibrato_depth: 0.0,
            tremolo: 0.0,
            life: Life::new(),
            drift: 0.0,
            fluctuation: 0.0,
            swell: 0.0,
            articulation: 0.0,
            chiff: 0.0,
            chiff_fall: 0.0,
            onset: 0.0,
            onset_rise: 1.0,
            scoop: 0.0,
            scoop_fall: 0.0,
            gap: Bump::new(),
            gap_depth: 0.0,
            noise: Noise::for_note(0, 0.0, 0),
            turbulence: 0.0,
            noise_high: 0.0,
            noise_low: 0.0,
            noise_low_pole: 0.0,
            noise_level: 0.0,
            jet_last: 0.0,
            jet_scale: 1.0,
            steep: DelayLine::new(16),
            steep_most: 0.0,
            brassiness: 0.0,
            tilt: (0.0, 0.0),
            tilt_memory: 0.0,
            tilt_second: 0.0,
            tone: StateVariableFilter::new(SvfMode::Lowpass, 8_000.0, 0.707),
            out_dc: DcBlock::default(),
            level: 0.0,
            gain: 1.0,
            gain_target: 1.0,
            gain_follow: 1.0,
            follower: 0.0,
            follower_fall: 0.0,
            sounding: false,
        }
    }

    fn prepare(&mut self, rate: f64) {
        self.rate = rate;
        let longest = (rate / LOWEST_HZ).ceil() as usize + 8;
        self.bore = DelayLine::new(longest);
        self.jet = DelayLine::new(longest / 2 + 8);
        self.steep = DelayLine::new(2 * STEEP_MOST as usize + 8);
        Processor::<f64>::prepare(
            &mut self.tone,
            &ProcessSpec {
                sample_rate: rate,
                max_block_size: 1,
                num_channels: 1,
            },
        );
        self.dc = DcBlock::new(LOOP_HIGHPASS * 440.0, rate);
        self.out_dc = DcBlock::new(20.0, rate);
        self.ear_dc = DcBlock::new(20.0, rate);
        self.dynamics.prepare(rate, BREATH_SECONDS);
        self.push_fall = t60_factor(PUSH_SECONDS, rate);
        self.chiff_fall = t60_factor(CHIFF_SECONDS, rate);
        self.onset_rise = 1.0 - (-1.0 / (CHIFF_RISE * rate)).exp();
        self.scoop_fall = t60_factor(SCOOP_SECONDS, rate);
        self.lift_follow = 1.0 - (-1.0 / (LIFT_SECONDS * rate)).exp();
        self.gain_follow = 1.0 - (-1.0 / (GAIN_SECONDS * rate)).exp();
        self.jet_follow = 1.0 - (-1.0 / (JET_FOLLOW_SECONDS * rate)).exp();
        self.noise_low_pole = (-TAU * BREATH_LOW / rate).exp();
        self.ear_wait = (EAR_WAIT * rate) as u32;
        self.follower_fall = t60_factor(0.3, rate);
        self.silence();
    }

    fn configure(&mut self, params: &WindParams) {
        self.kind = params.kind;
        self.opening = params.opening;
        self.jet_offset = params.jet_offset;
        self.bore_gain = params.bore_gain;
        self.breath_soft = params.breath_soft;
        self.breath_loud = params.breath_loud;
        self.vibrato_depth = params.vibrato_depth;
        self.tremolo = params.tremolo;
        self.noise_level = params.noise;
        self.drift = params.drift;
        self.fluctuation = params.fluctuation;
        self.swell = params.swell;
        self.articulation = params.articulation;
        self.brassiness = if params.kind == WindKind::Brass {
            params.brassiness
        } else {
            0.0
        };
        self.tone.set_frequency(params.tone.min(0.45 * self.rate));
        self.level = params.level;
        self.breath_fall = 1.0 - (-3.0 / (params.release * self.rate)).exp();
    }

    /// The bore's reflection and the valve's tuning for a note at `key`,
    /// and the delay the bore's read needs so that the loop is in tune at
    /// the fundamental.
    fn aim(&mut self, params: &WindParams, key: f64) {
        let rate = self.rate;
        let f0 = key_hz(key + self.bend).clamp(LOWEST_HZ, HIGHEST_RATIO * rate);
        let w0 = TAU * f0 / rate;
        let period = rate / f0;
        let harmonic = matches!(params.kind, WindKind::Saxophone | WindKind::Brass);
        // The bell reflects what is below its cut-off and lets out what is
        // above: a one-pole low-pass. On a harmonic bore the cut-off follows
        // the note as well, so that the lowest resonances stand tallest and
        // the reed or the lips do not leap to a higher one.
        let mut bell = params.bell;
        if harmonic {
            bell = bell.clamp(BELL_LEAST * f0, BELL_HARMONICS * f0);
        } else if params.kind == WindKind::Flute {
            bell = bell.clamp(BELL_LEAST * f0, FLUTE_BELL_HARMONICS * f0);
        }
        // High notes take more pressure, as a player's do; the pressure
        // moves there smoothly at a slur.
        let octaves = (f0 / 220.0).log2().max(0.0);
        self.lift_target = match params.kind {
            WindKind::Brass => LIPS_LIFT * octaves,
            WindKind::Clarinet | WindKind::Saxophone => REED_LIFT * octaves,
            WindKind::Flute => 0.0,
        };
        let pole = -(-TAU * bell.min(0.45 * rate) / rate).exp();
        self.bell = Loss {
            b0: params.bore_gain * (1.0 + pole),
            a: pole,
        };
        // A bore whose reflection does not invert keeps its mean out with a
        // high-pass in the loop. A fixed corner leaves a slow loop of nearly
        // full gain between the reed's flow and the bore's mean pressure,
        // which a reed on a harmonic bore runs as a low throb; a corner that
        // follows the note leaves that loop no gain, for a little lead at
        // the fundamental, which is taken off with the bell's delay.
        self.dc.set_frequency(LOOP_HIGHPASS * f0, rate);
        let radiation = match params.kind {
            WindKind::Brass => BRASS_RADIATION,
            WindKind::Saxophone => SAXOPHONE_RADIATION,
            WindKind::Clarinet | WindKind::Flute => 0.0,
        };
        for stage in &mut self.radiation {
            stage.set_frequency(radiation * f0, rate);
        }
        let mut filters = self.bell.delay(w0);
        if params.kind != WindKind::Clarinet {
            filters += self.dc.delay(w0);
        }
        let bore = (self.length(period) - filters).max(1.0);
        self.bore_offset = cubic_delay_for(bore, w0) - bore - filters;
        let pole = -(-TAU * f0 / rate).exp();
        self.ear_filter = Loss {
            b0: 1.0 + pole,
            a: pole,
        };
        if params.kind == WindKind::Flute {
            let pole = -(-TAU * (JET_FILTER * f0).min(0.45 * rate) / rate).exp();
            self.jet_filter = Loss {
                b0: 1.0 + pole,
                a: pole,
            };
            self.jet_offset_delay = -2.0 * self.jet_filter.delay(w0);
            self.jet_scale = 1.0 / (2.0 * (0.5 * w0).sin()).max(1e-6);
            // The jet's drive is set against what the pipe loses in a trip
            // at the note, over the jet's gain there for small deflections
            // (its profile's slope, taken where it is aimed as it plays, and
            // its filter).
            let trip = self.bell.gain(w0) * self.dc.passive_gain(w0);
            let loss = (1.0 - trip).max(1e-3);
            let slope = JET_GAIN * self.jet_filter.gain(w0).powi(2);
            self.jet_strength = loss / slope.max(1e-3);
            self.jet_unit = slope.max(1e-3);
            self.jet_over = 0.5 + params.opening;
        }
        let tilt = match params.kind {
            WindKind::Brass => BRASS_TILT,
            WindKind::Saxophone => SAXOPHONE_TILT,
            WindKind::Clarinet => CLARINET_TILT,
            WindKind::Flute => FLUTE_TILT,
        };
        self.tilt = (tilt.0 * f0, tilt.1 * f0);
        match params.kind {
            WindKind::Brass => {
                let lips = (params.reed_frequency * f0).min(VALVE_LIMIT * rate);
                self.valve.tune(lips, params.reed_damping, rate);
            }
            WindKind::Saxophone => {
                // A saxophonist firms the embouchure as the notes rise, which
                // keeps the reed's resonance above the harmonics that matter.
                let reed = params.reed_frequency.max(REED_HARMONICS * f0);
                self.valve.tune(reed, params.reed_damping, rate);
            }
            WindKind::Clarinet => {
                self.valve
                    .tune(params.reed_frequency, params.reed_damping, rate);
            }
            WindKind::Flute => {}
        }
    }

    /// The bore's round trip for a period of `period` samples.
    fn length(&self, period: f64) -> f64 {
        match self.kind {
            // Closed at the reed: the wave comes back inverted, so it takes
            // two trips to come round.
            WindKind::Clarinet => 0.5 * period,
            WindKind::Saxophone | WindKind::Flute | WindKind::Brass => period,
        }
    }

    /// How long this bore takes to change its length at a slur.
    fn change_seconds(&self) -> f64 {
        if self.kind == WindKind::Brass {
            VALVE_SECONDS
        } else {
            KEY_SECONDS
        }
    }

    /// The bore jumps to `key`: the old length's read fades out over a few
    /// milliseconds, and the breath breaks as the new note speaks.
    fn change_to(&mut self, params: &WindParams, key: f64) {
        self.from_key = self.glide.key;
        self.from_offset = self.bore_offset;
        self.glide.set(key);
        self.aim(params, key);
        self.switch.start(self.change_seconds(), self.rate);
        self.gap.start(BREAK_SECONDS, self.rate);
        self.gap_depth = BREAK_LEAST + BREAK_RANGE * self.articulation;
        // The new note speaks through a breath of air and a soft tongue.
        self.chiff = self.chiff.max(0.15 * self.articulation);
        self.push = self.push.max(0.3 * TONGUE_PUSH * self.articulation);
        self.ear_wait = (EAR_WAIT_SLUR * self.rate) as u32;
    }

    /// How hard the tongue starts a note at `velocity`, with an accent.
    fn tongue(&self, velocity: f64, accent: bool) -> f64 {
        let tongue = self.articulation * (0.4 + 0.6 * velocity.clamp(0.0, 1.0));
        if accent { 1.5 * tongue } else { tongue }
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        params: &WindParams,
        key: f64,
        level: f64,
        velocity: f64,
        accent: bool,
        seed: Noise,
    ) {
        let rate = self.rate;
        let fresh = !self.sounding;
        if fresh {
            self.glide.set(key);
            self.switch.finish();
            self.gap.stop();
            self.aim(params, key);
            self.lift = self.lift_target;
            self.steep_most = (STEEPENING * rate / key_hz(key + self.bend)).min(STEEP_MOST);
            self.jet_period = rate / key_hz(key + self.bend).clamp(LOWEST_HZ, HIGHEST_RATIO * rate);
            self.ear_wait = (EAR_WAIT * rate) as u32;
        } else {
            // Tongued again while it sounds: the bore jumps to the note.
            self.change_to(params, key);
        }
        self.dynamics.strike(level, fresh);
        self.held = true;
        let attack = if accent {
            ACCENT_ATTACK * params.attack
        } else {
            params.attack
        };
        self.breath_rise = 1.0 - (-3.0 / (attack * rate)).exp();
        let tongue = self.tongue(velocity, accent);
        self.push = TONGUE_PUSH * tongue;
        if accent {
            self.push = self.push.max(params.accent_push);
        }
        self.chiff = self.chiff.max(tongue);
        if fresh {
            if params.kind == WindKind::Flute {
                let period = rate / key_hz(key + self.bend).clamp(LOWEST_HZ, HIGHEST_RATIO * rate);
                self.kick.start(0.5 * period / rate, rate);
                self.kick_level = TONGUE_KICK * tongue / self.jet_unit;
            }
            self.noise = seed;
            self.vibrato.start(&params.vibrato(), rate, seed);
            self.life.start(seed, rate);
            self.scoop = if params.kind == WindKind::Brass {
                tongue.min(1.5)
            } else {
                0.0
            };
            // The ear starts where it settles: the lips pull a brass note
            // sharp by a constant fraction (measured).
            self.ear = if params.kind == WindKind::Brass {
                BRASS_EAR
            } else {
                1.0
            };
            self.since_mark = 0.0;
        }
        self.age = 0;
        self.sounding = true;
    }

    fn slur(&mut self, params: &WindParams, key: f64, level: f64) {
        self.age = 0;
        if params.portamento {
            self.glide.to(key, params.legato_time, self.rate);
            self.aim(params, key);
            self.ear_wait = (EAR_WAIT * self.rate) as u32;
        } else {
            self.change_to(params, key);
        }
        self.dynamics.strike(level, false);
        self.held = true;
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let rate = self.rate;
        let wave = self.vibrato.next(self.dynamics.vibrato);
        let life = self.life.next();
        self.scoop = flush(self.scoop * self.scoop_fall);
        // Everything that moves the pitch from the note's: the bend, the
        // vibrato, the drift, the lips' buzz rising into a brass note.
        let wobble = self.bend
            + 0.01
                * (self.vibrato_depth * wave + self.drift * life.drift - SCOOP_CENTS * self.scoop);
        let key = self.glide.next() + wobble;
        let period = rate / key_hz(key);

        // The breath, with the tongue's push, the break of a slur, the
        // player's swell and waver, and the breath's own vibrato.
        let dynamics = self.dynamics.next();
        if self.held {
            self.breath += (1.0 - self.breath) * self.breath_rise;
        } else {
            self.breath = flush(self.breath * (1.0 - self.breath_fall));
        }
        self.push = flush(self.push * self.push_fall);
        self.chiff = flush(self.chiff * self.chiff_fall);
        self.onset += (1.0 - self.onset) * self.onset_rise;
        let dip = self.gap_depth * self.gap.next();
        let range = self.breath_loud - self.breath_soft;
        // Under a tenth of full dynamics the breath fades out, so that the
        // breath controller or the expression pedal at nothing is silence.
        let fade = (dynamics * 10.0).min(1.0);
        // The breath the player asks for, and how much of it the mouth
        // passes on now: the attack and release, the tongue's push, the
        // break, the breath's vibrato.
        let player = (1.0 + self.swell * life.swell + self.fluctuation * life.waver).max(0.0);
        let level = (self.breath_soft + range * dynamics.min(BREATH_MOST)) * player;
        let envelope = fade
            * self.breath
            * (1.0 + self.push)
            * (1.0 - dip)
            * (1.0 + 0.5 * self.tremolo * wave);
        let blow = level * envelope;
        // The breath's noise: louder at the tongue's burst and at a slur,
        // wavering more than the breath.
        let waver = (1.0 + NOISE_WAVER * self.fluctuation * life.waver).max(0.0);
        let chiff = fade * CHIFF_GAIN * self.chiff * self.onset;
        let air = self.noise_level * level * (envelope + chiff) * waver;
        self.lift += (self.lift_target - self.lift) * self.lift_follow;
        self.gain += (self.gain_target - self.gain) * self.gain_follow;

        let bore_delay = (self.length(period * self.ear) + self.bore_offset).max(1.0);
        let mut inc = self.bore.read_cubic(bore_delay);
        if self.switch.fading() {
            // The old length's read, fading out.
            let old = rate / key_hz(self.from_key + wobble);
            let old_delay = (self.length(old * self.ear) + self.from_offset).max(1.0);
            let share = self.switch.next();
            inc = share * inc + (1.0 - share) * self.bore.read_cubic(old_delay);
        }
        let reflected = self.bell.step(&mut self.bell_memory, inc);
        // What the bell lets out: the incident wave less what the bell
        // itself reflects. The bore's wall losses are in the reflection's
        // gain, and are not sound: taken out, the bell radiates little of the
        // fundamental, and nothing of the bore's mean pressure, which a step
        // of the breath or a slur would otherwise let out as a thump.
        let radiated = inc - reflected / self.bore_gain.max(0.5);
        let opening = OPENING_LEAST + OPENING_RANGE * self.opening;

        // The pressure the ear hears, the sound the bell lets out (the
        // incident wave less what it reflects), and the air heard directly.
        let (pressure, out, heard) = match self.kind {
            WindKind::Clarinet | WindKind::Saxophone => {
                self.turbulence += (self.noise.next() - self.turbulence) * 0.3;
                let turbulence = air * self.turbulence;
                // The reed. Closed at the reed end, the cylinder's wave comes
                // back inverted (clarinet); the harmonic bore's does not
                // (saxophone).
                let incoming = if self.kind == WindKind::Clarinet {
                    -reflected
                } else {
                    self.dc.step_passive(reflected)
                };
                let x = self.valve.position();
                let (least, range) = if self.kind == WindKind::Clarinet {
                    (REED_LEAST, REED_RANGE)
                } else {
                    (CONE_LEAST, CONE_RANGE)
                };
                let gamma =
                    (least + self.lift + range * level) * envelope + TURBULENCE * turbulence;
                // The reed closes as the pressure across it rises.
                let u = valve_flow(opening * (1.0 - x).max(0.0), gamma - 2.0 * incoming);
                let p = 2.0 * incoming + u;
                self.valve.drive(gamma - p);
                self.bore.push(flush(incoming + u));
                (p, radiated, REED_AIR * turbulence)
            }
            WindKind::Brass => {
                self.turbulence += (self.noise.next() - self.turbulence) * 0.3;
                let turbulence = air * self.turbulence;
                let incoming = self.dc.step_passive(reflected);
                let x = self.valve.position();
                let gamma = (LIPS_LEAST + self.lift + LIPS_RANGE * level) * envelope
                    + TURBULENCE * turbulence;
                // The lips open outwards as the pressure behind them rises.
                let lips = opening * (LIPS_REST + x).max(0.0);
                let u = valve_flow(lips, gamma - 2.0 * incoming);
                let p = 2.0 * incoming + u;
                self.valve.drive(gamma - p);
                self.bore.push(flush(incoming + u));
                (p, radiated, REED_AIR * turbulence)
            }
            WindKind::Flute => {
                // The breath's turbulence: white noise between a high-pass
                // and a low-pass whose corner rises with the jet's speed,
                // which by Bernoulli's law is the square root of the
                // blowing pressure.
                let speed = blow.max(0.0).sqrt();
                let high = BREATH_HIGH.0 + (BREATH_HIGH.1 - BREATH_HIGH.0) * speed.min(1.5);
                let x = (core::f64::consts::PI * high / rate).min(0.9);
                // The pole of a one-pole low-pass at `high`, by the bilinear
                // transform: no exponential a sample.
                let pole = (1.0 - x) / (1.0 + x);
                let white = self.noise.next();
                self.noise_high = flush(white + pole * (self.noise_high - white));
                self.noise_low = flush(
                    self.noise_high + self.noise_low_pole * (self.noise_low - self.noise_high),
                );
                let turbulence = air * (self.noise_high - self.noise_low);

                let incoming = self.dc.step_passive(reflected);
                // The jet is deflected by the flow in and out of the hole,
                // which the wave coming back sets, and by its own
                // turbulence, and reaches the edge a quarter of a period
                // later. It splits there, part in and part out, as the
                // hyperbolic tangent of its deflection (the profile of a
                // Bickley jet), and drives the pipe by the rate at which the
                // flow it puts in changes: the jet-drive model of N. H.
                // Fletcher ("Jet drive mechanism in organ pipes", JASA 60,
                // 1976) and of M.-P. Verge, B. Fabre, A. Hirschberg and A.
                // P. J. Wijnands ("Sound production in recorderlike
                // instruments", JASA 101, 1997), with the jet's speed from
                // the blowing pressure. Blown harder, the jet switches
                // harder and the tone grows richer, as a flute's does (N. H.
                // Fletcher, "Acoustical correlates of flute performance
                // technique", JASA 57, 1975). Off centre, part of it enters
                // the pipe at rest, which gives the even harmonics.
                let [first, second] = &mut self.jet_memory;
                let deflection = self
                    .jet_filter
                    .step(first, incoming + JET_NOISE * turbulence);
                let deflection = self.jet_filter.step(second, deflection);
                self.jet.push(deflection);
                self.jet_period += (period * self.ear - self.jet_period) * self.jet_follow;
                let eta = self
                    .jet
                    .read_cubic((JET_RATIO * self.jet_period + self.jet_offset_delay).max(1.0));
                // How loud the player is blowing, from softest to loudest,
                // sets how hard the jet drives the pipe and where it is
                // aimed; the breath's envelope brings it in and out.
                let loud = ((level - self.breath_soft) / range.max(1e-3)).clamp(0.0, 1.0);
                let aim = self.jet_offset * (JET_OFFSET.0 + (JET_OFFSET.1 - JET_OFFSET.0) * loud);
                let centre = aim.tanh();
                let slope = (1.0 - centre * centre).max(0.05);
                let over = self.jet_over * (JET_DRIVE.0 + (JET_DRIVE.1 - JET_DRIVE.0) * loud);
                let flow =
                    self.jet_strength / slope * over * envelope * (JET_GAIN * eta + aim).tanh();
                // The pipe is driven by how fast that flow changes (the
                // momentum it brings), scaled to be as strong at the
                // fundamental as the flow itself.
                let drive = (flow - self.jet_last) * self.jet_scale;
                self.jet_last = flow;
                // Vortices shed at the edge of the hole take energy out of the
                // flow in and out of it, as its square: the loss that holds
                // a flue's swing down before its jet saturates (B. Fabre, A.
                // Hirschberg and A. P. J. Wijnands, "Vortex shedding in
                // steady oscillation of a flue organ pipe", Acustica 82,
                // 1996), and keeps a soft note pure.
                let x = self.jet_unit * incoming;
                let shed = self.vortex * self.jet_strength * x * x.abs();
                let kick = self.kick_level * self.kick.next();
                self.bore.push(flush(incoming + drive - shed + kick));
                (
                    incoming,
                    radiated + self.embouchure * drive,
                    FLUTE_BREATH * turbulence,
                )
            }
        };

        // The ear marks the period at the first upward crossing of zero by
        // the pressure at the mouth after three quarters of a period, to a
        // fraction of a sample, and nudges the bore's length towards the
        // note's, slowly and a little at a time, as a bowed string's does.
        let pressure = self.ear_dc.step(pressure);
        let [first, second] = &mut self.ear_memory;
        let pressure = self.ear_filter.step(first, pressure);
        let pressure = self.ear_filter.step(second, pressure);
        self.since_mark += 1.0;
        self.age = self.age.saturating_add(1);
        if self.held && self.heard_last < 0.0 && pressure >= 0.0 {
            let fraction = self.heard_last / (self.heard_last - pressure);
            let heard = self.since_mark - 1.0 + fraction;
            if heard > 0.75 * period {
                let error = heard / period - 1.0;
                let settled = self.age >= self.ear_wait
                    && !self.glide.moving()
                    && !self.switch.fading()
                    && !self.gap.running();
                if settled && error.abs() < 0.1 && self.breath > 0.9 {
                    let gain = (period / (EAR_SECONDS * rate)).clamp(1e-3, 0.05);
                    let limit = (1.5 / period).max(0.01);
                    self.ear = (self.ear - gain * error.clamp(-limit, limit)).clamp(0.8, 1.25);
                }
                self.since_mark = 1.0 - fraction;
            }
        }
        self.heard_last = pressure;

        // The radiation's first pole; a brass bell's second is taken after
        // the steepening, which bends the wave as it travels and would
        // otherwise let the slow swell of its own asymmetry out as a thump.
        let mut out = match self.kind {
            WindKind::Brass | WindKind::Saxophone => self.radiation[0].step(out),
            WindKind::Clarinet | WindKind::Flute => out,
        };
        if self.tilt.1 > 0.0 {
            let breath = blow.clamp(0.0, 1.0);
            let cutoff = self.tilt.0 + (self.tilt.1 - self.tilt.0) * breath * breath;
            let pole = (-TAU * cutoff.min(0.45 * rate) / rate).exp();
            self.tilt_memory = flush(out + pole * (self.tilt_memory - out));
            out = self.tilt_memory;
            if self.kind == WindKind::Saxophone {
                // The saxophone's tilt is two poles: with one, its reed's
                // beating brightens a soft note about as much as a loud one
                // (measured).
                self.tilt_second = flush(out + pole * (self.tilt_second - out));
                out = self.tilt_second;
            }
        }
        if self.brassiness > 0.0 {
            // Crests travel faster than troughs, so at high levels the wave
            // steepens on its way down the bore (A. Hirschberg, J. Gilbert,
            // R. Msallam and A. P. J. Wijnands, "Shock waves in trombones",
            // JASA 99, 1996): a delay that is shorter for the crests.
            // The crests run ahead by up to a fraction of the period: the
            // bore is about as long as a period's travel.
            // How far the crests can run ahead follows the period smoothly:
            // a jump at a slur would read the line from another moment, a
            // click.
            let most = (STEEPENING * period).min(STEEP_MOST);
            self.steep_most += (most - self.steep_most) * self.lift_follow;
            let most = self.steep_most;
            self.steep.push(out);
            let lead = self.brassiness * most * (BLARE_SCALE * out).tanh();
            out = self.steep.read_cubic(most + 2.0 - lead);
        }
        if self.kind == WindKind::Brass {
            out = self.radiation[1].step(out);
        }
        let out = self.out_dc.step(self.tone.process_sample(0, out + heard));
        // A tongued note is heard louder at its start.
        // A beating reed or buzzing lips hardly grow louder with the breath
        // alone (a player swells with the embouchure too), so the player's
        // swell and waver are heard in their level as well as their colour;
        // a flute's tone follows its breath by itself.
        let swaying = if self.kind == WindKind::Flute {
            1.0
        } else {
            player
        };
        let out =
            out * self.level * self.gain * self.dynamics.volume * swaying * (1.0 + 0.5 * self.push);
        self.follower = (self.follower * self.follower_fall).max(out.abs());
        if !self.held && self.breath < 1e-4 && self.follower < SILENCE {
            self.silence();
        }
        out
    }
}

impl Tone for WindTone {
    fn is_sounding(&self) -> bool {
        self.sounding
    }

    fn silence(&mut self) {
        self.bore.clear();
        self.jet.clear();
        self.steep.clear();
        self.bell_memory = 0.0;
        self.tilt_memory = 0.0;
        self.tilt_second = 0.0;
        self.jet_memory = [0.0; 2];
        self.jet_last = 0.0;
        self.dc.reset();
        for stage in &mut self.radiation {
            stage.reset();
        }
        self.out_dc.reset();
        self.ear_dc.reset();
        self.ear_memory = [0.0; 2];
        self.valve.reset();
        Processor::<f64>::reset(&mut self.tone);
        self.breath = 0.0;
        self.push = 0.0;
        self.chiff = 0.0;
        self.onset = 0.0;
        self.scoop = 0.0;
        self.switch.finish();
        self.gap.stop();
        self.kick.stop();
        self.turbulence = 0.0;
        self.noise_high = 0.0;
        self.noise_low = 0.0;
        self.heard_last = 0.0;
        self.since_mark = 0.0;
        self.ear = 1.0;
        self.held = false;
        self.follower = 0.0;
        self.sounding = false;
    }
}

/// A wind instrument by digital waveguide: a voice for
/// [`Synth`](crate::Synth) that plays flute, clarinet, saxophone, trumpet
/// and trombone.
///
/// The bore is a delay line holding the pressure wave's trip down it and
/// back, with the bell's reflection, a low-pass, at the far end (J. O.
/// Smith, "Efficient simulation of the reed-bore and bow-string
/// mechanisms", ICMC 1986). At the mouth end what blows it:
///
/// ```text
///  breath (velocity, pressure, breath controller) + turbulence
///        |
///   [reed / lips / jet] <-- incoming wave ---[bell: reflection low-pass]--+
///        |                                                               |
///        +---- outgoing wave --->[delay: one trip, or two for the clarinet]
///                                                                        +--> what the bell lets out --> out
/// ```
///
/// - **The reed** (clarinet, saxophone) is a valve that closes as the
///   pressure across it rises, a mass on a spring resonating near 2 kHz,
///   with the flow through it from Bernoulli's law solved exactly every
///   sample (Ph. Guillemain, J. Kergomard and T. Voinier, JASA 2005). On
///   the clarinet's cylinder, closed at the reed, the wave comes back
///   inverted after a trip: the period is two trips, and the odd harmonics
///   dominate. A cone resonates on every harmonic, like a pipe open at both
///   ends as long as the whole cone; the saxophone's bore is that harmonic
///   series, a trip of a whole period with a reflection that does not
///   invert, and its high-pass and its bell follow the note so that it
///   keeps to its lowest resonance.
/// - **The lips** (brass) are a valve that opens as the pressure behind them
///   rises, tuned below the note as a player's are, buzzing into a bore
///   whose resonances fall on a harmonic series, as a trumpet's bell and
///   mouthpiece make them. A loud note steepens on its way down the bore
///   and blares ([`WindParams::brassiness`]).
/// - **The jet** (flute) is deflected by the flow in and out of the
///   embouchure hole and by its own turbulence, and reaches the edge a
///   quarter of a period later, where it splits as the hyperbolic tangent
///   of its deflection; it drives the pipe by how fast the flow it puts in
///   changes, the jet-drive model of N. H. Fletcher and of M.-P. Verge, B.
///   Fabre and A. Hirschberg. The vortices shed at the edge hold its swing
///   down, so a soft note is nearly a sine; blown harder the jet is driven
///   further above its threshold and aimed further off the edge
///   ([`WindParams::jet_offset`]), and the tone grows richer, the second
///   harmonic first, as a flute's does. The embouchure radiates the jet's
///   drive as well as the pipe's open end its wave. The breath's
///   turbulence, shaped and growing with the blowing pressure, is heard
///   through the jet and directly: a soft continuous breath, and a burst of
///   it (the chiff) as the tongue releases the note, which speaks some
///   80 ms later.
/// - **The breath** is velocity, then pressure on the key, the breath
///   controller (controller 2) and the expression pedal (controller 11) as
///   the note sounds: it moves the blowing pressure between
///   [`WindParams::breath_soft`] and [`WindParams::breath_loud`], so a
///   louder note is also brighter. It carries turbulence, heard as
///   breathiness. The tongue starts every note ([`WindParams::articulation`]):
///   a push of breath and a burst of air, harder at a higher velocity; the
///   lips of a brass player start below the note and buzz up into it. An
///   accent tongues the note harder still: its breath builds twice as fast
///   and overshoots ([`WindParams::accent_push`]).
/// - **The player** is never quite still: the pitch drifts by a few cents
///   ([`WindParams::drift`]), the breath and its noise waver
///   ([`WindParams::fluctuation`]), and a held note swells after its attack
///   and eases back ([`WindParams::swell`]), each from slow noise seeded by
///   the note.
/// - **In tune.** The bore's read is tuned at the fundamental, the bell's
///   and the high-pass's delays taken off. The reed, the lips and the jet
///   pull the pitch (the lips by tens of cents), so the player has an ear:
///   the time between the upward crossings of the pressure at the mouth is
///   the period played, and the bore is lengthened or shortened, slowly,
///   until it is the note's.
/// - **Vibrato** waits, then grows, quickening to its rate as it does, and
///   wanders, in pitch and, for the flute, in the breath
///   ([`WindParams::tremolo`]); [`Expression::Vibrato`] and the modulation
///   wheel scale it.
/// - **Legato**: in a [`Synth`](crate::Synth) set to legato, a note struck
///   while another is held slurs to it: the breath goes on, and the bore
///   changes its length at once, as a key or a valve does: the old length's
///   read fades into the new one's in 3 ms (4 ms for a valve), the air
///   breaks briefly and the new note speaks through a soft tongue, without
///   gliding through the pitches between. With
///   [`WindParams::portamento`] the bore glides instead
///   ([`WindParams::legato_time`]), as a trombone's slide can.
///
/// Releasing the key stops the breath ([`WindParams::release`]) and the bore
/// rings down. Every source of noise is seeded by the note.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{WindParams, WindVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut clarinet = Synth::with_voices(1, || WindVoice::new(WindParams::clarinet()));
/// clarinet.set_legato(true);
/// clarinet.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// clarinet.note_on(NoteRef::new(0, 55), 0.8);
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// for _ in 0..20 {
///     clarinet.process(&mut block);
/// }
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct WindVoice {
    params: WindParams,
    tones: Handover<WindTone>,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
    key: u8,
}

impl WindVoice {
    /// A voice with these settings. Until it is prepared it assumes 48 kHz
    /// and has no room to sound.
    pub fn new(params: WindParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            tones: Handover::new(WindTone::new(), WindTone::new()),
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
            key: 60,
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note takes them at once, but for its bore's tuning, which
    /// it takes at its next note; a change of [`WindParams::kind`] is heard
    /// from the next note.
    pub fn set_params(&mut self, params: WindParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &WindParams {
        &self.params
    }

    fn configure(&mut self) {
        let params = self.params;
        for tone in [&mut self.tones.now, &mut self.tones.old] {
            // A sounding note keeps the bore it was blown on.
            let kind = tone.kind;
            tone.configure(&params);
            if tone.sounding {
                tone.kind = kind;
            }
        }
    }

    /// The breath a velocity asks for, and an accent's more: the softest
    /// velocity still plays, softly.
    fn breath(velocity: f32, accent: bool) -> f64 {
        let breath = 0.1 + 0.9 * f64::from(velocity).clamp(0.0, 1.0);
        if accent { breath * 1.15 } else { breath }
    }

    /// The level a velocity and an accent add on top of what the breath
    /// does.
    fn gain(&self, velocity: f32, accent: bool) -> f64 {
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let gain = velocity_gain(velocity, self.params.velocity_range_db);
        if accent {
            gain * db(self.params.accent_db)
        } else {
            gain
        }
    }

    fn retune(&mut self) {
        let bend = self.bend + self.tuning;
        self.tones.now.bend = bend;
        if self.tones.now.sounding {
            let params = self.params;
            let target = self.tones.now.glide.target;
            self.tones.now.aim(&params, target);
        }
    }
}

impl<S: Sample> Voice<S> for WindVoice {
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
        let seed = Noise::for_note(self.key, 0.0, 41);
        let gain = self.gain(velocity, accent);
        let bend = self.bend + self.tuning;
        let tone = &mut self.tones.now;
        if !tone.sounding {
            tone.kind = params.kind;
            tone.gain = gain;
        }
        tone.bend = bend;
        tone.gain_target = gain;
        tone.start(
            &params,
            self.key_tuning.key_pitch(self.key),
            Self::breath(velocity, accent),
            f64::from(velocity),
            accent,
            seed,
        );
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.tones.now.sounding {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        let params = self.params;
        self.key = note.key.min(127);
        self.tones.now.gain_target = self.gain(velocity, accent);
        self.tones.now.slur(
            &params,
            self.key_tuning.key_pitch(self.key),
            Self::breath(velocity, accent),
        );
    }

    fn note_off(&mut self, _velocity: f32) {
        self.tones.now.held = false;
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
        add_to_every_channel(buffer, || tones.next(WindTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            self.retune();
        } else {
            self.tones.now.dynamics.expression(expression, value);
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
    use crate::instruments::common::db;
    use crate::instruments::measure::{
        self, Act, RATE, accented, amplitude_at, band_passed, between, cents, decibels,
        frequency_near, inharmonic_db, largest_jump, note, off, on, play, rms, synth_of, t60_at,
    };
    use crate::instruments::modelling::probe::{
        harmonic_centroid, harmonics, high_band_levels, levels, modulation, period_track,
        pitch_track,
    };

    /// The presets and the range each plays.
    fn presets() -> [(&'static str, WindParams, u8, u8); 5] {
        [
            ("flute", WindParams::flute(), 60, 96),
            ("clarinet", WindParams::clarinet(), 50, 89),
            ("saxophone", WindParams::saxophone(), 49, 81),
            ("trumpet", WindParams::trumpet(), 54, 82),
            ("trombone", WindParams::trombone(), 40, 72),
        ]
    }

    fn voice(params: WindParams) -> [WindVoice; 1] {
        [WindVoice::new(params)]
    }

    /// The same settings with no vibrato.
    fn still(params: WindParams) -> WindParams {
        WindParams {
            vibrato_depth: 0.0,
            tremolo: 0.0,
            ..params
        }
    }

    /// The same settings with none of the player's drift, waver or swell.
    fn lifeless(params: WindParams) -> WindParams {
        WindParams {
            drift: 0.0,
            fluctuation: 0.0,
            swell: 0.0,
            ..params
        }
    }

    /// The same settings with no vibrato and none of the player's drift,
    /// waver or swell: the model alone, for what they would blur. A drift of
    /// a few cents moves the upper harmonics further than the width of a
    /// spectrum's bin over most of a second, so the harmonics are measured
    /// without it.
    fn steady(params: WindParams) -> WindParams {
        lifeless(still(params))
    }

    /// How long after `at` seconds the pitch of `signal` settles within
    /// `tolerance` cents of `target` hertz and stays there for the next
    /// 100 ms, period by period, and how many periods before that lay
    /// between the old note, `from` hertz, and the new one, more than
    /// `tolerance` from both: a glide passes through them, a change of
    /// length jumps over them.
    fn settling(signal: &[f64], at: f64, from: f64, target: f64, tolerance: f64) -> (f64, usize) {
        let (low, high) = (from.min(target), from.max(target));
        let track = period_track(between(signal, at - 0.05, at + 0.3), 0.7 * low, 1.4 * high);
        let start = 0.05;
        let mut settled = f64::INFINITY;
        for (n, &(t, _)) in track.iter().enumerate() {
            if t < start {
                continue;
            }
            let stays = track[n..]
                .iter()
                .take_while(|&&(u, _)| u < t + 0.1)
                .all(|&(_, f)| cents(f, target).abs() < tolerance);
            if stays {
                settled = t - start;
                break;
            }
        }
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

    /// A slur from `first` to `second` at 1 s on a fresh legato synth, the
    /// first note released just after.
    fn slurred(params: WindParams, first: u8, second: u8, velocity: f32) -> Vec<f64> {
        let mut synth = synth_of(voice(params));
        synth.set_legato(true);
        let at = (1.0 * RATE) as usize;
        play(
            &mut synth,
            &[
                (0, on(first, velocity)),
                (at, on(second, velocity)),
                (at + 10, off(first)),
            ],
            (2.0 * RATE) as usize,
            256,
        )
    }

    #[test]
    fn the_valve_flow_solves_its_equation() {
        for opening in [0.0, 0.05, 0.3, 1.0, 4.0] {
            for drop in [-2.0, -0.3, -1e-6, 0.0, 1e-6, 0.2, 0.9, 3.0] {
                let u: f64 = valve_flow(opening, drop);
                let across: f64 = drop - u;
                let expected = if across == 0.0 {
                    0.0
                } else {
                    opening * across.abs().sqrt() * across.signum()
                };
                assert!((u - expected).abs() < 1e-12, "{opening} {drop}: {u}");
                // The flow never reverses the pressure across the valve.
                assert!(u * drop >= 0.0 && u.abs() <= drop.abs());
            }
        }
        assert_eq!(valve_flow(f64::NAN, 1.0), 0.0);
        assert_eq!(valve_flow(1.0, f64::INFINITY), 0.0);
    }

    #[test]
    fn the_valve_rings_at_its_frequency_and_settles_at_its_drive() {
        let mut valve = Valve::default();
        valve.tune(2_000.0, 0.05, RATE);
        let mut positions = Vec::new();
        for _ in 0..48_000 {
            positions.push(valve.position());
            valve.drive(0.5);
        }
        assert!((positions[47_999] - 0.5).abs() < 1e-6);
        let crossings = positions[..2_400]
            .windows(2)
            .filter(|pair| (pair[0] - 0.5) * (pair[1] - 0.5) < 0.0)
            .count();
        // 2 kHz for 50 ms: a hundred periods, two hundred crossings.
        assert!((crossings as f64 - 200.0).abs() <= 4.0, "{crossings}");
        valve.reset();
        assert_eq!(valve.position(), 0.0);
    }

    #[test]
    fn every_note_is_in_tune_across_its_range() {
        // The player's drift is on: a note's pitch wanders by its drift's
        // RMS, slowly, and its tuning is the average over two seconds of its
        // steady part. A drift on a time scale of 0.35 s averaged over 2 s
        // keeps about 0.6 of its RMS (the averaging window is six of its
        // time constants), so a note is held to the model's own 3 cents plus
        // three times that; the notes' mean error, to the model's own 3
        // cents.
        let mut worst = 0.0f64;
        for (name, params, low, high) in presets() {
            let (mut total, mut count) = (0.0, 0.0);
            for key in (low..=high).step_by(2) {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(still(params)), on(key, 0.7), 3.0, 2.8);
                let error = cents(frequency_near(between(&signal, 0.8, 2.8), f0), f0);
                worst = worst.max(error.abs());
                total += error.abs();
                count += 1.0;
                let bound = 3.0 + 3.0 * 0.6 * params.drift;
                assert!(error.abs() < bound, "{name} key {key}: {error:.2} cents");
            }
            let mean = total / count;
            println!(
                "{name}: mean tuning error {mean:.2} cents with a drift of {}",
                params.drift
            );
            assert!(mean < 3.0, "{name}: {mean:.2} cents on average");
        }
        println!("winds: worst tuning {worst:.2} cents");
    }

    #[test]
    fn blown_notes_sustain_at_every_breath() {
        let (mut worst_swing, mut worst_cents) = (0.0f64, 0.0f64);
        for (name, params, low, high) in presets() {
            for velocity in [0.0f32, 0.5, 1.0] {
                for key in (low..=high).step_by(4) {
                    let f0 = key_hz(f64::from(key));
                    // The model alone: the player's swell and waver are
                    // measured on their own.
                    let signal = note(voice(steady(params)), on(key, velocity), 2.0, 1.6);
                    let windows = levels(between(&signal, 0.6, 1.6), 0.1);
                    let (quiet, loud) = windows
                        .iter()
                        .fold((f64::MAX, 0.0f64), |(q, l), &x| (q.min(x), l.max(x)));
                    let swing = decibels(loud / quiet);
                    let error = cents(frequency_near(between(&signal, 0.6, 1.6), f0), f0);
                    worst_swing = worst_swing.max(swing);
                    worst_cents = worst_cents.max(error.abs());
                    assert!(
                        rms(between(&signal, 0.6, 1.6)) > 1e-3,
                        "{name} v{velocity} key {key}"
                    );
                    assert!(swing < 1.0, "{name} v{velocity} key {key}: {swing:.2} dB");
                    assert!(
                        error.abs() < 3.0,
                        "{name} v{velocity} key {key}: {error:.2} cents"
                    );
                    // At its pitch, not at another register's.
                    let h = harmonics(between(&signal, 0.6, 1.6), f0, 1)[0];
                    assert!(
                        h > 0.1 * rms(between(&signal, 0.6, 1.6)),
                        "{name} v{velocity} key {key}: no fundamental"
                    );
                }
            }
        }
        println!(
            "winds: worst level swing {worst_swing:.2} dB, worst tuning {worst_cents:.2} cents"
        );
    }

    #[test]
    fn the_clarinet_sounds_its_odd_harmonics_and_the_others_every_one() {
        for (name, params, low, _) in presets() {
            for key in [low, low + 5, low + 10] {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(steady(params)), on(key, 0.8), 2.0, 1.2);
                let h: Vec<f64> = harmonics(between(&signal, 0.6, 1.2), f0, 4)
                    .into_iter()
                    .map(decibels)
                    .collect();
                let odd = 0.5 * (h[0] + h[2]);
                let even = 0.5 * (h[1] + h[3]);
                println!("{name} key {key}: odd {odd:.1} dB, even {even:.1} dB");
                if name == "clarinet" {
                    // A cylinder closed at one end resonates at the odd
                    // harmonics only, in the low register.
                    assert!(even < odd - 12.0, "{name} key {key}");
                } else if name == "flute" {
                    assert!(even > odd - 25.0, "{name} key {key}");
                } else {
                    assert!(even > odd - 6.0, "{name} key {key}");
                }
            }
        }
    }

    #[test]
    fn velocity_makes_a_note_louder_and_brighter() {
        for (name, params, low, high) in presets() {
            let key = (low + high) / 2;
            let f0 = key_hz(f64::from(key));
            let loud = note(voice(steady(params)), on(key, 1.0), 2.0, 1.2);
            let soft = note(voice(steady(params)), on(key, 0.4), 2.0, 1.2);
            let (loud, soft) = (between(&loud, 0.6, 1.2), between(&soft, 0.6, 1.2));
            let difference = decibels(rms(loud) / rms(soft));
            let (bright, dark) = (harmonic_centroid(loud, f0), harmonic_centroid(soft, f0));
            println!("{name}: {difference:.1} dB, centroid {bright:.0} Hz against {dark:.0} Hz");
            // The level by at least what the range adds, and the breath more.
            assert!(
                difference > 0.6 * params.velocity_range_db,
                "{name}: {difference} dB"
            );
            assert!(bright > 1.05 * dark, "{name}: {bright} against {dark}");
        }
    }

    #[test]
    fn an_accent_is_louder_and_tongued() {
        for (name, params, low, high) in presets() {
            let key = (low + high) / 2;
            let plain = note(voice(still(params)), on(key, 0.6), 2.0, 1.0);
            let strong = note(voice(still(params)), accented(key, 0.6), 2.0, 1.0);
            let sustain =
                decibels(rms(between(&strong, 0.5, 1.0)) / rms(between(&plain, 0.5, 1.0)));
            let attack =
                decibels(rms(between(&strong, 0.0, 0.05)) / rms(between(&plain, 0.0, 0.05)));
            println!("{name}: accent {sustain:.1} dB held, {attack:.1} dB in the attack");
            assert!(sustain > params.accent_db - 0.5, "{name}: {sustain}");
            assert!(attack > sustain + 1.0, "{name}: {attack} against {sustain}");
        }
    }

    #[test]
    fn the_vibrato_swings_the_pitch_and_the_flutes_breath() {
        // The saxophone's jaw vibrato, in pitch.
        let params = WindParams {
            noise: 0.0,
            ..lifeless(WindParams::saxophone())
        };
        let key = 61;
        let f0 = key_hz(f64::from(key));
        let signal = note(voice(params), on(key, 0.8), 3.0, 3.0);
        let hop = 0.005;
        let track = pitch_track(&signal, f0, 1.2, 2.9, 0.03, hop);
        let (rate, depth) = modulation(&track, f0, hop);
        println!("saxophone vibrato: {rate:.2} Hz, {depth:.1} cents");
        assert!((rate / params.vibrato_rate - 1.0).abs() < 0.12, "{rate}");
        assert!((depth / params.vibrato_depth - 1.0).abs() < 0.25, "{depth}");

        // The flute's, in the breath as well: its level swings at the rate.
        let params = WindParams {
            noise: 0.0,
            ..lifeless(WindParams::flute())
        };
        let key = 72;
        let f0 = key_hz(f64::from(key));
        let signal = note(voice(params), on(key, 0.8), 3.0, 3.0);
        let levels: Vec<f64> = (0..170)
            .map(|step| {
                let t = 1.2 + 0.01 * f64::from(step);
                decibels(amplitude_at(between(&signal, t, t + 0.03), f0))
            })
            .collect();
        let (low, high) = levels
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), &x| (l.min(x), h.max(x)));
        let still_flute = note(voice(still(params)), on(key, 0.8), 3.0, 3.0);
        let still_levels: Vec<f64> = (0..170)
            .map(|step| {
                let t = 1.2 + 0.01 * f64::from(step);
                decibels(amplitude_at(between(&still_flute, t, t + 0.03), f0))
            })
            .collect();
        let (still_low, still_high) = still_levels
            .iter()
            .fold((f64::MAX, f64::MIN), |(l, h), &x| (l.min(x), h.max(x)));
        println!(
            "flute tremolo: {:.1} dB, {:.1} dB without",
            high - low,
            still_high - still_low
        );
        assert!(high - low > 1.0 && high - low > 4.0 * (still_high - still_low));
    }

    #[test]
    fn legato_changes_the_note_at_once_and_only_the_slide_glides() {
        for (name, params, low, high) in presets() {
            let params = steady(params);
            let middle = (low + high) / 2;
            for (first, second) in [(middle, middle + 2), (high - 7, high - 12)] {
                let signal = slurred(params, first, second, 0.8);
                let before = rms(between(&signal, 0.8, 1.0));
                let after = rms(between(&signal, 1.6, 1.8));
                let through = levels(between(&signal, 1.0, 1.2), 0.01);
                let dip =
                    decibels(through.iter().fold(f64::MAX, |m, &x| m.min(x)) / before.min(after));
                let (f0, f1) = (key_hz(f64::from(first)), key_hz(f64::from(second)));
                let error = cents(frequency_near(between(&signal, 1.5, 2.0), f1), f1);
                // A key or a valve changes the length at once: the pitch is
                // the new note's (within 20 cents, a tenth of a whole tone)
                // within 10 ms, with at most two periods in between, for a
                // key; the lips take up to 40 ms and eight periods to move
                // to the new resonance as the valve travels and the old
                // standing wave dies away, a brass slur's break, not a glide
                // (a glide spends dozens of periods between the notes; see
                // below).
                let (settled, gliding) = settling(&signal, 1.0, f0, f1, 20.0);
                let (most, soonest) = if params.kind == WindKind::Brass {
                    (8, 0.04)
                } else {
                    (2, 0.01)
                };
                println!(
                    "{name} {first} to {second}: dip {dip:.1} dB, at the new pitch after {:.1} ms with {gliding} periods between, then {error:.2} cents",
                    1000.0 * settled
                );
                // The breath breaks as the new note speaks (for 30 ms, by at
                // most 35% of the breath), but the note goes on.
                assert!(dip > -9.0, "{name}: {dip} dB");
                assert!(error.abs() < 3.0, "{name}: {error} cents");
                assert!(settled < soonest, "{name}: {settled} s");
                assert!(
                    gliding <= most,
                    "{name}: {gliding} periods between the notes"
                );
            }
            let mut synth = synth_of(voice(params));
            synth.set_legato(true);
            play(
                &mut synth,
                &[(0, on(low, 0.8)), (4_800, on(low + 2, 0.8))],
                9_600,
                256,
            );
            assert_eq!(synth.active_voices(), 1);
        }
        // The trombone's slide glides only when asked to: with portamento
        // on, 25 ms into a slide of a fourth the pitch is between the notes,
        // and it passes through them for tens of milliseconds.
        for portamento in [true, false] {
            let params = WindParams {
                portamento,
                ..steady(WindParams::trombone())
            };
            let signal = slurred(params, 50, 55, 0.8);
            let middle = frequency_near(between(&signal, 1.01, 1.04), key_hz(52.5));
            let semitones = 12.0 * (middle / key_hz(50.0)).log2();
            let (settled, gliding) = settling(&signal, 1.0, key_hz(50.0), key_hz(55.0), 20.0);
            println!(
                "trombone, portamento {portamento}: {semitones:.2} semitones up 25 ms in, settled after {:.0} ms with {gliding} periods between",
                1000.0 * settled
            );
            if portamento {
                assert!(semitones > 0.5 && semitones < 4.0, "{semitones}");
                assert!(gliding >= 5 && settled > 0.05, "{gliding} {settled}");
            } else {
                assert!(semitones > 4.5, "{semitones}");
                assert!(gliding <= 8 && settled < 0.04, "{gliding} {settled}");
            }
        }
    }

    #[test]
    fn a_note_change_has_no_click_thump_or_burst() {
        // At a slur the bore's read fades from the old length to the new in
        // a few milliseconds and the breath breaks smoothly: no sample jumps
        // further than the notes' own largest steps by more than half again,
        // no 2 ms window above 8 kHz is more than 10 dB over the notes' own
        // level there (the air of the slur is a few decibels), and nothing
        // below half the lower note (a thump) comes within 20 dB of the
        // tone. Also for a slur down a fifth onto a louder, accented note,
        // whose level the slur takes over 10 ms.
        for (name, params, low, high) in presets() {
            let middle = (low + high) / 2;
            for (first, second, accent) in [
                (middle, middle + 2, false),
                (middle, middle - 5, false),
                (middle, middle - 7, true),
            ] {
                let signal = if accent {
                    let mut synth = synth_of(voice(still(params)));
                    synth.set_legato(true);
                    let at = (1.0 * RATE) as usize;
                    play(
                        &mut synth,
                        &[
                            (0, on(first, 0.6)),
                            (at, accented(second, 0.9)),
                            (at + 10, off(first)),
                        ],
                        (2.0 * RATE) as usize,
                        256,
                    )
                } else {
                    slurred(still(params), first, second, 0.8)
                };
                let steps = largest_jump(between(&signal, 0.6, 0.98))
                    .max(largest_jump(between(&signal, 1.3, 1.9)));
                let change = largest_jump(between(&signal, 0.995, 1.06));
                let hf = high_band_levels(&signal, 8_000.0, 0.002);
                let quiet: Vec<f64> = hf[300..490].iter().chain(&hf[650..950]).copied().collect();
                let reference = quiet.iter().sum::<f64>() / quiet.len() as f64;
                let burst = hf[495..530].iter().fold(0.0f64, |m, &x| m.max(x));
                let lowest = key_hz(f64::from(first.min(second)));
                let thump = levels(&band_passed(&signal, 10.0, 0.5 * lowest), 0.005);
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
                assert!(decibels(thump / tone) < -20.0, "{name}");
            }
        }
    }

    /// The flute's tone and breath at `velocity`, held from the start for
    /// 1.5 s at D5, the model alone (no vibrato, drift, waver or swell).
    fn flute_note(velocity: f32) -> Vec<f64> {
        note(
            voice(steady(WindParams::flute())),
            on(74, velocity),
            1.5,
            1.5,
        )
    }

    /// The share of `signal`'s energy that lies off the harmonics of `f0`
    /// (by more than 20 Hz), in decibels: for a flute, its breath.
    fn breathiness(signal: &[f64], f0: f64) -> f64 {
        let len = if signal.len() >= 32_768 {
            32_768
        } else {
            2_048
        };
        inharmonic_db(signal, f0, 20.0, len)
    }

    #[test]
    fn the_flute_breathes_into_its_tone() {
        // A real flute's breath sits some 30 to 40 dB under its tone, always
        // heard, and more in a soft note than a loud one (N. H. Fletcher and
        // T. D. Rossing, section 16.10; the breath is the turbulence of the
        // jet). Measured as the share of the energy off the harmonics over
        // the held note.
        let f0 = key_hz(74.0);
        let shares: Vec<f64> = [0.2f32, 0.6, 1.0]
            .iter()
            .map(|&velocity| breathiness(between(&flute_note(velocity), 0.6, 1.3), f0))
            .collect();
        println!(
            "flute breath: {:.1} dB soft, {:.1} dB mezzo-forte, {:.1} dB loud",
            shares[0], shares[1], shares[2]
        );
        assert!(shares[1] < -28.0 && shares[1] > -40.0, "{}", shares[1]);
        assert!(shares.iter().all(|&x| x < -25.0 && x > -45.0), "{shares:?}");
        assert!(shares[0] > shares[2] + 2.0, "{shares:?}");

        // In the attack the breath is heard before the tone (the chiff):
        // over the first 40 ms it is at least 15 dB more of the sound than
        // once the note is held, and the tone is within 6 dB of its held
        // level by 120 ms (a flute speaks in some 30 to 80 ms at
        // mezzo-forte; measured about 80).
        let signal = flute_note(0.6);
        let attack = breathiness(between(&signal, 0.0, 0.0427), f0);
        let held = rms(between(&signal, 0.6, 1.3));
        let spoken = (0..30)
            .map(|k| 0.01 * f64::from(k))
            .find(|&t| rms(between(&signal, t, t + 0.01)) > held * db(-6.0))
            .unwrap_or(1.0);
        println!(
            "flute attack: breath {attack:.1} dB of the sound against {:.1} held; tone within 6 dB after {:.0} ms",
            shares[1],
            1000.0 * spoken
        );
        assert!(attack > shares[1] + 15.0, "{attack}");
        assert!(spoken > 0.02 && spoken < 0.12, "{spoken}");
    }

    #[test]
    fn the_flutes_harmonics_follow_its_dynamics() {
        // The second harmonic is strong at mezzo-forte, some 6 to 15 dB
        // under the fundamental; soft, the tone is purer and loud richer
        // (N. H. Fletcher, "Acoustical correlates of flute performance
        // technique", JASA 57, 1975); and the upper harmonics fall away
        // steadily, by more than 10 dB an octave, not as a flat comb.
        let f0 = key_hz(74.0);
        let spectrum = |velocity: f32| {
            let signal = flute_note(velocity);
            let h = harmonics(between(&signal, 0.6, 1.3), f0, 12);
            h.iter().map(|&a| decibels(a / h[0])).collect::<Vec<f64>>()
        };
        let (soft, mezzo, loud) = (spectrum(0.2), spectrum(0.6), spectrum(1.0));
        let centroid =
            |velocity: f32| harmonic_centroid(between(&flute_note(velocity), 0.6, 1.3), f0);
        let (dark, bright) = (centroid(0.2), centroid(1.0));
        // The fall of harmonics 2 to 10, by a straight line through their
        // levels against the octave they are in.
        let fall = |levels: &[f64]| {
            let points: Vec<(f64, f64)> = (2..=10)
                .map(|n| ((n as f64).log2(), levels[n - 1]))
                .collect();
            let count = points.len() as f64;
            let (mx, my) = (
                points.iter().map(|p| p.0).sum::<f64>() / count,
                points.iter().map(|p| p.1).sum::<f64>() / count,
            );
            points.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>()
                / points.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>()
        };
        println!(
            "flute: second harmonic {:.1} dB soft, {:.1} mezzo-forte, {:.1} loud; third {:.1}, {:.1}, {:.1}; centroid {dark:.0} Hz soft, {bright:.0} loud; harmonics fall {:.1} dB an octave at mezzo-forte",
            soft[1],
            mezzo[1],
            loud[1],
            soft[2],
            mezzo[2],
            loud[2],
            fall(&mezzo)
        );
        assert!(mezzo[1] > -15.0 && mezzo[1] < -5.0, "{}", mezzo[1]);
        assert!(soft[1] < mezzo[1] - 2.0 && loud[1] > mezzo[1] + 2.0);
        assert!(bright > 1.25 * dark, "{bright} against {dark}");
        assert!(fall(&mezzo) < -10.0, "{}", fall(&mezzo));
    }

    #[test]
    fn a_held_note_drifts_wavers_and_swells_as_designed() {
        for (name, params, low, high) in presets() {
            let key = (low + high) / 2;
            let f0 = key_hz(f64::from(key));
            let params = still(params);
            // The drift: the pitch over windows of 50 ms, about its mean,
            // has the drift's RMS, within a factor of two either way (a few
            // seconds of a slow random motion), and none without it.
            let drifting = note(voice(params), on(key, 0.7), 6.0, 6.0);
            let track = pitch_track(&drifting, f0, 1.0, 5.9, 0.05, 0.025);
            let cents_track: Vec<f64> = track.iter().map(|&f| cents(f, f0)).collect();
            let mean = cents_track.iter().sum::<f64>() / cents_track.len() as f64;
            let drift = (cents_track.iter().map(|c| (c - mean).powi(2)).sum::<f64>()
                / cents_track.len() as f64)
                .sqrt();
            let plain = note(voice(steady(params)), on(key, 0.7), 6.0, 6.0);
            let track = pitch_track(&plain, f0, 1.0, 5.9, 0.05, 0.025);
            let still_drift = track
                .iter()
                .map(|&f| cents(f, f0).abs())
                .fold(0.0f64, f64::max);
            // The waver: the level over windows of 50 ms moves, slowly, and
            // the swell lifts a long note's middle above its end.
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
                voice(WindParams {
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
                "{name}: drift {drift:.2} cents (set {}), {still_drift:.2} without; waver {waver:.2} dB; swell {swell:.2} dB",
                params.drift
            );
            assert!(
                drift > 0.5 * params.drift && drift < 2.0 * params.drift,
                "{name}: {drift}"
            );
            assert!(still_drift < 0.5, "{name}: {still_drift}");
            assert!(waver > 0.05 && waver < 2.0, "{name}: {waver}");
            assert!(swell > 0.2 && swell < 4.0, "{name}: {swell}");
        }
    }

    #[test]
    fn a_released_note_stops_without_a_click() {
        for (name, params, low, high) in presets() {
            let key = (low + high) / 2;
            let f0 = key_hz(f64::from(key));
            let signal = note(voice(still(params)), on(key, 0.8), 0.8, 1.6);
            let steady = largest_jump(between(&signal, 0.5, 0.8));
            let release = largest_jump(between(&signal, 0.8, 1.6));
            // As the breath falls, a reed that beat stops beating and its
            // edges can sharpen a little; nothing jumps.
            assert!(
                release <= 1.25 * steady,
                "{name}: {release} against {steady}"
            );
            let measured = t60_at(&signal, f0, 0.82, 0.82 + (2.0 * params.release).max(0.12));
            println!("{name}: dies in {measured:.3} s");
            assert!(measured < 0.6, "{name}: {measured}");
            let mut synth = synth_of(voice(params));
            play(
                &mut synth,
                &[(0, on(key, 0.8)), (4_800, off(key))],
                2 * 48_000,
                256,
            );
            assert_eq!(synth.active_voices(), 0, "{name}");
        }
    }

    #[test]
    fn the_breath_follows_the_controllers() {
        let params = steady(WindParams::clarinet());
        let key = 58;
        let run = |shape: &dyn Fn(&mut WindVoice)| {
            let mut synth = synth_of(voice(params));
            synth.note_on(NoteRef::new(0, key), 1.0);
            play(&mut synth, &[], (0.5 * RATE) as usize, 256);
            for voice in synth.voices_mut() {
                shape(voice);
            }
            let signal = play(&mut synth, &[], (0.8 * RATE) as usize, 256);
            rms(between(&signal, 0.4, 0.8))
        };
        let full = run(&|_| {});
        let soft = run(&|voice| Voice::<f64>::expression(voice, Expression::Pressure, 0.3));
        let breath = run(&|voice| {
            Voice::<f64>::midi(
                voice,
                MidiMessage::ControlChange {
                    channel: 0,
                    controller: 2,
                    value: 0,
                },
            );
        });
        println!(
            "pressure 0.3: {:.1} dB; no breath: {:.1} dB",
            decibels(soft / full),
            decibels(breath / full)
        );
        assert!(decibels(soft / full) < -3.0);
        assert!(decibels(breath / full) < -60.0);
    }

    #[test]
    fn a_loud_brass_note_blares() {
        let f0 = key_hz(60.0);
        let upper = |brassiness: f64| {
            let params = WindParams {
                brassiness,
                ..steady(WindParams::trumpet())
            };
            let signal = note(voice(params), on(60, 1.0), 2.0, 1.2);
            let h = harmonics(between(&signal, 0.6, 1.2), f0, 10);
            decibels(h[5..].iter().sum::<f64>() / h[0])
        };
        let (blaring, plain) = (upper(1.0), upper(0.0));
        println!("trumpet's 6th to 10th harmonics: {blaring:.1} dB blaring, {plain:.1} dB without");
        assert!(blaring > plain + 2.0);
    }

    #[test]
    fn notes_start_and_are_taken_over_without_a_click() {
        for (name, params, low, high) in presets() {
            let key = (low + high) / 2;
            let alone = note(voice(params), on(key, 1.0), 2.0, 0.6);
            let steady = largest_jump(between(&alone, 0.3, 0.6));
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
            (0, on(60, 0.9)),
            (1_000, accented(64, 0.7)),
            (7_000, on(67, 0.5)),
            (9_001, off(60)),
            (12_000, on(60, 1.0)),
            (15_000, Act::Bend(1.0)),
            (20_000, off(64)),
        ];
        for (name, params, _, _) in presets() {
            let run = |block| {
                play(
                    &mut synth_of([WindVoice::new(params), WindVoice::new(params)]),
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
        for kind in [
            WindKind::Flute,
            WindKind::Clarinet,
            WindKind::Saxophone,
            WindKind::Brass,
        ] {
            let extreme = [
                WindParams {
                    kind,
                    breath_soft: f64::NAN,
                    breath_loud: f64::INFINITY,
                    opening: 1e9,
                    reed_frequency: f64::NAN,
                    reed_damping: 0.0,
                    bore_gain: 2.0,
                    bell: f64::INFINITY,
                    jet_offset: f64::NAN,
                    noise: 1e9,
                    attack: 0.0,
                    release: f64::NAN,
                    vibrato_rate: 1e9,
                    vibrato_depth: f64::INFINITY,
                    tremolo: 1e9,
                    vibrato_delay: -1.0,
                    vibrato_rise: 0.0,
                    legato_time: f64::NAN,
                    portamento: true,
                    articulation: f64::INFINITY,
                    drift: 1e9,
                    fluctuation: f64::NAN,
                    swell: -1e9,
                    brassiness: f64::INFINITY,
                    tone: 0.0,
                    velocity_range_db: -5.0,
                    accent_db: 1e9,
                    accent_push: f64::INFINITY,
                    level: f64::NAN,
                },
                WindParams {
                    kind,
                    breath_soft: 1.0,
                    breath_loud: 1.0,
                    opening: 1.0,
                    bore_gain: 0.999,
                    noise: 1.0,
                    accent_db: 24.0,
                    accent_push: 2.0,
                    level: 4.0,
                    brassiness: 1.0,
                    vibrato_depth: 100.0,
                    tremolo: 1.0,
                    articulation: 1.0,
                    drift: 20.0,
                    fluctuation: 0.3,
                    swell: 0.5,
                    ..WindParams::trumpet()
                },
            ];
            for params in extreme {
                let mut synth = synth_of([WindVoice::new(params), WindVoice::new(params)]);
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
                    "{kind:?}: {}",
                    measure::peak(&signal)
                );
            }
        }
    }

    #[test]
    fn a_released_note_dies_away_at_every_sample_rate() {
        // At 3 kHz the flute's G4, released, went on sounding by itself at
        // -25 dBFS: the bore's high-pass gained up to 10% above its corner,
        // more than the bell lost, and the vortices' loss held the swing.
        for rate in [3_000.0, 4_000.0, 6_000.0, 8_000.0, 11_025.0, 22_050.0] {
            for (name, params, low, high) in presets() {
                for key in [low, (low + high) / 2, 67, 79, high] {
                    let mut synth = crate::Synth::new(voice(params));
                    synth.prepare(&ProcessSpec {
                        sample_rate: rate,
                        max_block_size: 512,
                        num_channels: 1,
                    });
                    let released = (0.3 * rate) as usize;
                    let signal = play(
                        &mut synth,
                        &[(0, accented(key, 1.0)), (released, off(key))],
                        (2.0 * rate) as usize,
                        256,
                    );
                    let label = format!("{name} {key} at {rate} Hz");
                    assert!(measure::peak(&signal[..released]) > 1e-3, "{label}");
                    assert_eq!(synth.active_voices(), 0, "{label}: still sounding");
                    let tail = &signal[(1.5 * rate) as usize..];
                    assert_eq!(measure::peak(tail), 0.0, "{label}");
                }
            }
        }
    }
}
