//! What makes a [`PartialBank`](crate::PartialBank) sound blown rather than
//! computed: air around each partial, an attack for each partial, and wind
//! that does not hold still. The settings are here; the bank's methods say
//! how they are used, and ADR 0026 why they are what they are.

use core::f32::consts::{FRAC_PI_2, LN_2, LN_10, PI};

/// Frames between two updates of the voicing, a power of two that divides
/// [`NORMALISE_FRAMES`](crate::PartialBank::NORMALISE_FRAMES).
pub(crate) const VOICE_FRAMES: usize = 32;
const PER_VOICE_FRAME: f32 = 1.0 / VOICE_FRAMES as f32;

/// Updates a change of air or jitter depth takes: 512 frames, 10.7 ms at
/// 48 kHz.
const DEPTH_RAMP_UPDATES: u32 = 16;

/// The bandwidth of a partial's noise until it is set, in hertz.
const DEFAULT_AIR_BANDWIDTH_HZ: f32 = 20.0;

/// The narrowest noise and the slowest wind, in hertz.
const MIN_BANDWIDTH_HZ: f32 = 0.1;
const MIN_WIND_RATE_HZ: f32 = 0.01;

/// The largest settings. Past these the small-angle and small-log series
/// the update uses stop being what the documentation says.
const MAX_PITCH_CENTS: f32 = 100.0;
const MAX_JITTER_CENTS: f32 = 25.0;
const MAX_JITTER_DB: f32 = 3.0;
/// The most the wind's unit wander reaches, in standard deviations.
const MAX_WANDER: f32 = 4.0;
/// Longest time an attack counts, so that frames stay exact in an `f32`.
const MAX_ATTACK_FRAMES: u32 = 1 << 23;
/// The largest pole, the last `f32` below 1: a filter with a pole of 1 would
/// never move again, and its gain would have no finite scale.
const LARGEST_POLE: f32 = 1.0 - f32::EPSILON / 2.0;
/// The largest level an attack starts from, more than any modulator holds.
const MAX_START_LEVEL: f32 = 8.0;

/// The noise around one partial, for
/// [`PartialBank::set_air`](crate::PartialBank::set_air).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PartialAir {
    /// The noise's level against the partial's, a linear ratio from 0 to 1:
    /// the root mean square of the partial's complex noise, whose power
    /// is `depth²` times the partial's. So 0.01 puts the whole skirt 40 dB
    /// below the partial. 0 is no noise and costs nothing.
    pub depth: f32,
    /// The skirt's full width at half power, in hertz: its power is half its
    /// peak at `bandwidth_hz / 2` from the partial, and for a narrow skirt
    /// half of the whole lies that close in as well. It falls at 6 dB an
    /// octave beyond that, the shape of a resonance driven by noise, down
    /// to what the updates can carry, a quarter of their rate from the
    /// partial or so, where it falls away faster. Between 0.1 Hz and a
    /// sixty-fourth of the sample rate (750 Hz at 48 kHz), where the straight
    /// lines between updates make it a quarter narrower than asked; within
    /// 20 % up to half of that. At a sample rate that cannot hold what is
    /// asked the top is used, and what was asked stays for the next rate.
    pub bandwidth_hz: f32,
}

impl PartialAir {
    /// No air.
    pub const OFF: Self = Self {
        depth: 0.0,
        bandwidth_hz: DEFAULT_AIR_BANDWIDTH_HZ,
    };
}

impl Default for PartialAir {
    fn default() -> Self {
        Self::OFF
    }
}

/// How one partial speaks, for
/// [`PartialBank::start_attack`](crate::PartialBank::start_attack).
///
/// Everything is relative to the partial's gain: the level below multiplies
/// it. The default speaks at once, with nothing added: a bank given it
/// behaves as if it had been given nothing.
///
/// ```text
/// level  0 ........ 0  /‾‾ 1 + overshoot ‾‾\_____ 1
///        | delay      | rise              | settle
/// ```
///
/// Rise and settling are smoothsteps, `3 p² - 2 p³`: the level goes from
/// 10 % to 90 % of its peak in 0.61 of `rise_frames`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PartialAttack {
    /// Frames the partial stays silent.
    pub delay_frames: u32,
    /// Frames from the end of the delay until the level peaks.
    pub rise_frames: u32,
    /// How far above 1 the level peaks: 0.2 is 20 % too loud. From 0 to 1.
    /// Needs `settle_frames` to mean anything.
    pub overshoot: f32,
    /// Frames from the peak until the level is 1.
    pub settle_frames: u32,
    /// Extra air at the very start, against the partial's settled level,
    /// from 0 to 1. Unlike [`PartialAir::depth`], which follows the level,
    /// this does not wait for the delay or the rise: the air comes first.
    /// It falls as `(1 - p)²`.
    pub breath: f32,
    /// Frames the breath takes to die away, counted from the first update
    /// after the start, at most 31 frames later, so that what is heard of it
    /// does not depend on where the attack falls against the updates. It is
    /// read at the updates, so one shorter than an update still sounds, for
    /// two updates at most: its first value is the whole of it.
    pub breath_frames: u32,
    /// The noise's bandwidth at the very start, in hertz: it narrows to the
    /// partial's own as the breath dies away. 0 leaves the partial's own
    /// bandwidth throughout.
    pub breath_bandwidth_hz: f32,
    /// Where the pitch starts against the partial's, in cents, from -100
    /// to 100; negative is flat.
    pub pitch_cents: f32,
    /// Frames the pitch takes to settle, from the end of the delay. A
    /// smoothstep, read at the updates: one over before the first update
    /// is not heard.
    pub pitch_frames: u32,
}

/// The wind's small unsteadiness, for
/// [`PartialBank::set_wind_jitter`](crate::PartialBank::set_wind_jitter).
///
/// One wander moves both, because one wind drives both: when it rises the
/// bank goes up in pitch and in level together.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WindJitter {
    /// The root mean square of the bank's pitch movement, in cents, from 0
    /// to 25.
    pub cents: f32,
    /// The root mean square of the bank's level movement, in decibels, from
    /// 0 to 3.
    pub db: f32,
    /// How fast it wanders: the corner, in hertz, of two one-pole low-passes
    /// in a row over white noise, so 12 dB an octave above it. From 0.01 Hz
    /// to a 256th of the sample rate; a change neither moves the wander
    /// nor deepens it.
    pub rate_hz: f32,
}

impl WindJitter {
    /// No jitter.
    pub const OFF: Self = Self {
        cents: 0.0,
        db: 0.0,
        rate_hz: 4.0,
    };
}

impl Default for WindJitter {
    fn default() -> Self {
        Self::OFF
    }
}

/// A starting point for voicing a rank of pipes: how a partial's air and
/// attack follow from its harmonic number. [`FLUE_PRINCIPAL`](Self::FLUE_PRINCIPAL)
/// is the one given here.
///
/// ```
/// use manguebyte_dsp::{Partial, PartialBank, PipeVoicing};
///
/// let voicing = PipeVoicing::FLUE_PRINCIPAL;
/// let mut bank = PartialBank::new(16);
/// bank.set_sample_rate(48_000.0);
/// bank.set_len(8);
/// bank.set_voicing_seed(60); // the pipe's own, so that no two pipes share noise
/// bank.set_wind_jitter(voicing.jitter);
/// for i in 0..8 {
///     let harmonic = (i + 1) as f32;
///     let hz = 261.63 * f64::from(harmonic);
///     bank.set(i, Partial { freq_hz: hz, gain: 0.2 / harmonic, pan: 0.0 });
///     bank.set_air(i, voicing.air(harmonic, hz));
///     bank.start_attack(i, &voicing.attack(harmonic, 48_000.0));
/// }
/// let (mut left, mut right) = (vec![0.0f32; 4_800], vec![0.0f32; 4_800]);
/// bank.render_add(&mut left, &mut right);
/// assert!(left.iter().any(|&x| x != 0.0));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipeVoicing {
    /// The air of the first harmonic against its level, in decibels.
    pub air_db: f32,
    /// How much more air each octave of harmonic number has, in decibels.
    pub air_db_per_octave: f32,
    /// The most air any harmonic has, in decibels.
    pub air_top_db: f32,
    /// The bandwidth of the air at 0 Hz, in hertz.
    pub air_base_hz: f32,
    /// The quality factor that widens it with frequency: the bandwidth is
    /// `air_base_hz + frequency / air_q`.
    pub air_q: f32,
    /// The first harmonic's rise, in seconds.
    pub rise_seconds: f32,
    /// What each octave of harmonic number multiplies the rise by.
    pub rise_per_octave: f32,
    /// How long the first harmonic waits, in seconds; the wait falls to
    /// none at the sixteenth harmonic.
    pub lag_seconds: f32,
    /// The overshoot of the highest harmonics; the first has none, and
    /// harmonic `h` has `1 - 1 / h` of this.
    pub overshoot: f32,
    /// The settling time over the rise time.
    pub settle_ratio: f32,
    /// [`PartialAttack::breath`].
    pub breath: f32,
    /// How long the breath lasts, in seconds.
    pub breath_seconds: f32,
    /// [`PartialAttack::breath_bandwidth_hz`].
    pub breath_bandwidth_hz: f32,
    /// [`PartialAttack::pitch_cents`].
    pub pitch_cents: f32,
    /// How long the pitch takes to settle, in seconds.
    pub pitch_seconds: f32,
    /// The wind's unsteadiness.
    pub jitter: WindJitter,
}

impl PipeVoicing {
    /// A flue principal at 8', a judgement to start listening from, and not
    /// a measurement of any pipe. What the literature gives is the shape
    /// of the thing and some of its scale:
    ///
    /// - The whole attack: 26, 45 and 160 ms in one pipe measured for three
    ///   speeds of the pallet (proceedings of ISMA 2004, Nara), a dependence
    ///   on the rise of the pressure that Nolle and Finch (JASA 91, 1992)
    ///   study at length. 33 ms for the first harmonic to arrive, and 70 ms
    ///   for the second to arrive and come back from its overshoot, sit at
    ///   the quick end of that.
    /// - Harmonics do not start together (Fletcher, Acustica 34, 1976, for the
    ///   theory; Fletcher and Rossing, "The Physics of Musical Instruments",
    ///   chapter 17). Here the first harmonic is the last to reach its level,
    ///   at 33 ms against 25 ms for the second and 18 ms for the fourth, as
    ///   Pykett reports (below); it has no overshoot, so the upper ones are
    ///   the last to finish, at 70 and 52 ms. How much stagger is a judgement:
    ///   3 ms, because in Caecilia's listening a longer one sweeps the
    ///   brightness upwards, an artificial wah.
    /// - Overshoot, delay and stability are how Keeler (IEEE Transactions
    ///   on Audio and Electroacoustics 20, 1972) describes attacks. The 25 %
    ///   here is a judgement.
    /// - A chiff is noise close to the pipe's harmonics (Castellengo, Acta
    ///   Acustica 85, 1999), which is why the breath sits on each partial.
    ///   Its level, length and bandwidth here are judgements.
    /// - That the fundamental takes longest to appear, and that the lower
    ///   harmonics start a little flat, is reported in Colin Pykett's
    ///   measurements ("A Second in the Life of a Violone"), an essay and not
    ///   a paper; the 6 cents here is a judgement.
    /// - The air's bandwidth follows the resonator the noise drives, whose
    ///   width is frequency over Q; the floor and the Q of 60 are judgements,
    ///   and so are the air's levels and the jitter's, which have no
    ///   measured figure behind them at all. They want setting by ear.
    pub const FLUE_PRINCIPAL: Self = Self {
        air_db: -38.0,
        air_db_per_octave: 4.0,
        air_top_db: -16.0,
        air_base_hz: 6.0,
        air_q: 60.0,
        rise_seconds: 0.030,
        rise_per_octave: 0.75,
        lag_seconds: 0.003,
        overshoot: 0.25,
        settle_ratio: 2.0,
        breath: 0.2,
        breath_seconds: 0.040,
        breath_bandwidth_hz: 200.0,
        pitch_cents: -6.0,
        pitch_seconds: 0.040,
        jitter: WindJitter {
            cents: 0.5,
            db: 0.08,
            rate_hz: 4.0,
        },
    };

    /// The air for a partial of harmonic number `harmonic` (1 or more) at
    /// `frequency_hz`. Works out powers: call it when a pipe is set up.
    pub fn air(&self, harmonic: f32, frequency_hz: f64) -> PartialAir {
        let octaves = harmonic.max(1.0).log2();
        let db = (self.air_db + self.air_db_per_octave * octaves).min(self.air_top_db);
        PartialAir {
            depth: 10f32.powf(db / 20.0),
            bandwidth_hz: self.air_base_hz + (frequency_hz.max(0.0) / f64::from(self.air_q)) as f32,
        }
    }

    /// The attack for a partial of harmonic number `harmonic` (1 or more) at
    /// `sample_rate`. Works out powers: call it when a pipe is set up.
    pub fn attack(&self, harmonic: f32, sample_rate: f64) -> PartialAttack {
        let harmonic = harmonic.max(1.0);
        let octaves = harmonic.log2();
        let frames = |seconds: f32| (f64::from(seconds.max(0.0)) * sample_rate).round() as u32;
        let rise = self.rise_seconds * self.rise_per_octave.powf(octaves);
        PartialAttack {
            delay_frames: frames(self.lag_seconds * (1.0 - octaves / 4.0).max(0.0)),
            rise_frames: frames(rise),
            overshoot: self.overshoot * (1.0 - 1.0 / harmonic),
            settle_frames: frames(rise * self.settle_ratio),
            breath: self.breath,
            breath_frames: frames(self.breath_seconds),
            breath_bandwidth_hz: self.breath_bandwidth_hz,
            pitch_cents: self.pitch_cents,
            pitch_frames: frames(self.pitch_seconds),
        }
    }
}

/// `3 p² - 2 p³` for `p` in `0..=1`.
#[inline]
fn smooth(p: f32) -> f32 {
    p * p * (3.0 - 2.0 * p)
}

/// `e^x - 1` for small `x`: within `x⁴ / 24`.
#[inline]
fn exp_minus_one(x: f32) -> f32 {
    x * (1.0 + x * (0.5 + x / 6.0))
}

/// The cosine of `w` from 0 to `π/2`, within 3e-5. A series rather than the
/// platform's, so that the same settings give the same bits everywhere.
fn cos_series(w: f64) -> f64 {
    let w2 = w * w;
    1.0 - w2 / 2.0 * (1.0 - w2 / 12.0 * (1.0 - w2 / 30.0 * (1.0 - w2 / 56.0)))
}

/// The pole of the one-pole low-pass whose power is half at `corner_hz`,
/// updating at `update_rate` hertz. Exact, not the usual `exp(-w)`, because
/// the corners here go up to a quarter of the update rate. Always above 0 and
/// below 1, whatever the rates.
fn pole_for(corner_hz: f64, update_rate: f64) -> f32 {
    let w = (core::f64::consts::TAU * corner_hz / update_rate).min(core::f64::consts::FRAC_PI_2);
    let a = cos_series(w);
    let b = 2.0 - a;
    ((b - ((1.0 - a) * (3.0 - a)).sqrt()) as f32).min(LARGEST_POLE)
}

/// What scales uniform noise from -1 to 1, in two parts, into a one-pole
/// filter with this `pole`, for the filter's output, read through straight
/// lines between its values, to have a mean power of 1. The input's power
/// is 2/3 and the output's `2/3 · drive² / (1 - pole²)`; straight lines
/// between values whose correlation is `pole` multiply that by
/// `2/3 + pole/3`.
#[inline]
fn drive_for(pole: f32) -> f32 {
    (1.5 * (1.0 - pole * pole).max(0.0) / (2.0 / 3.0 + pole / 3.0)).sqrt()
}

/// The next value of a generator of 32-bit states, never 0.
#[inline(always)]
fn xorshift(mut state: u32) -> u32 {
    state ^= state << 13;
    state ^= state >> 17;
    state ^= state << 5;
    state
}

/// A first state for generator `stream` of a seed, never 0 (SplitMix64).
fn seeded(seed: u64, stream: u64) -> u32 {
    let mut z = seed.wrapping_add(stream.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (((z ^ (z >> 31)) >> 32) as u32).max(1)
}

/// A rotation as an angle, `atan2(sin, cos)` with no platform's maths, for a
/// rotation on the unit circle. The angle is brought into the first eighth
/// of a turn by the symmetries, so that the tangent of its half is at most
/// `tan(π/8)`, and the series for that arctangent stops below `f32`'s
/// precision there.
#[inline(always)]
fn rotation_angle(cos: f32, sin: f32) -> f32 {
    let (across, up) = (cos.abs(), sin.abs());
    let steep = up > across;
    let t = if steep {
        across / (1.0 + up)
    } else {
        up / (1.0 + across)
    };
    let t2 = t * t;
    let half = t
        * (1.0
            - t2 * (1.0 / 3.0
                - t2 * (1.0 / 5.0 - t2 * (1.0 / 7.0 - t2 * (1.0 / 9.0 - t2 / 11.0)))));
    let eighth = 2.0 * half;
    let quarter = if steep { FRAC_PI_2 - eighth } else { eighth };
    let angle = if cos < 0.0 { PI - quarter } else { quarter };
    angle.copysign(sin)
}

/// An age no attack reaches, whose level is 1 and whose breath and pitch are
/// nothing: what a partial without an attack holds.
const NO_ATTACK_AGE: f32 = (1u32 << 25) as f32;

/// The attacks in progress, a slot per partial, kept so that the same
/// sums with nothing in a slot give a level of 1, no breath and no shift:
/// see [`update_attacks`].
#[derive(Debug, Clone)]
struct Attacks {
    /// Frames since each start, at the update the bank makes next.
    age: Vec<f32>,
    delay: Vec<f32>,
    rise: Vec<f32>,
    per_rise: Vec<f32>,
    per_settle: Vec<f32>,
    peak: Vec<f32>,
    /// The level before the rise: 0, or where a restarted attack found it.
    from: Vec<f32>,
    breath: Vec<f32>,
    /// The age at the first update of the attack, where the breath starts.
    breath_start: Vec<f32>,
    per_breath: Vec<f32>,
    /// The pole of the noise at the start of the breath, and how much of
    /// it counts: 1, or 0 for the partial's own pole throughout. A pole
    /// and not a distance from the partial's, which can change meanwhile.
    breath_pole: Vec<f32>,
    breath_share: Vec<f32>,
    /// The natural logarithm of the starting pitch ratio.
    pitch_log: Vec<f32>,
    per_pitch: Vec<f32>,
    /// The age at which there is nothing left of each, infinite if none.
    ends: Vec<f32>,
}

impl Attacks {
    fn new(room: usize) -> Self {
        Self {
            age: vec![NO_ATTACK_AGE; room],
            delay: vec![0.0; room],
            rise: vec![0.0; room],
            per_rise: vec![1.0; room],
            per_settle: vec![1.0; room],
            peak: vec![1.0; room],
            from: vec![0.0; room],
            breath: vec![0.0; room],
            breath_start: vec![0.0; room],
            per_breath: vec![1.0; room],
            breath_pole: vec![0.0; room],
            breath_share: vec![0.0; room],
            pitch_log: vec![0.0; room],
            per_pitch: vec![1.0; room],
            ends: vec![f32::INFINITY; room],
        }
    }

    fn is_active(&self, i: usize) -> bool {
        self.ends[i] != f32::INFINITY
    }

    fn clear(&mut self, i: usize) {
        self.age[i] = NO_ATTACK_AGE;
        self.delay[i] = 0.0;
        self.rise[i] = 0.0;
        self.per_rise[i] = 1.0;
        self.per_settle[i] = 1.0;
        self.peak[i] = 1.0;
        self.from[i] = 0.0;
        self.breath[i] = 0.0;
        self.breath_start[i] = 0.0;
        self.per_breath[i] = 1.0;
        self.breath_pole[i] = 0.0;
        self.breath_share[i] = 0.0;
        self.pitch_log[i] = 0.0;
        self.per_pitch[i] = 1.0;
        self.ends[i] = f32::INFINITY;
    }

    /// The level of attack `i` at its present age.
    fn level_now(&self, i: usize) -> f32 {
        attack_level(
            self.age[i],
            self.delay[i],
            self.rise[i],
            self.per_rise[i],
            self.per_settle[i],
            self.peak[i],
            self.from[i],
        )
    }

    /// The pitch of attack `i` at its present age.
    fn shift_now(&self, i: usize) -> f32 {
        attack_shift(
            self.age[i],
            self.delay[i],
            self.pitch_log[i],
            self.per_pitch[i],
        )
    }
}

/// An attack's level at `age` frames, without a branch: the rise from `from`
/// up to the peak, less the settling down from there.
#[inline(always)]
fn attack_level(
    age: f32,
    delay: f32,
    rise: f32,
    per_rise: f32,
    per_settle: f32,
    peak: f32,
    from: f32,
) -> f32 {
    let after = age - delay;
    let rising = (after * per_rise).clamp(0.0, 1.0);
    let settling = ((after - rise) * per_settle).clamp(0.0, 1.0);
    from + (peak - from) * smooth(rising) - (peak - 1.0) * smooth(settling)
}

/// An attack's pitch at `age` frames, as a ratio less 1.
#[inline(always)]
fn attack_shift(age: f32, delay: f32, pitch_log: f32, per_pitch: f32) -> f32 {
    let settled = ((age - delay) * per_pitch).clamp(0.0, 1.0);
    exp_minus_one(pitch_log * (1.0 - smooth(settled)))
}

/// The bank's wind: one wander, two depths.
#[derive(Debug, Clone)]
struct Wind {
    settings: WindJitter,
    rng: u32,
    first: f32,
    second: f32,
    pole: f32,
    scale: f32,
    cents: f32,
    db: f32,
    cents_step: f32,
    db_step: f32,
    ramp_left: u32,
}

impl Wind {
    fn new(update_rate: f64, seed: u64) -> Self {
        let mut wind = Self {
            settings: WindJitter::OFF,
            rng: seeded(seed, u64::MAX),
            first: 0.0,
            second: 0.0,
            pole: 0.0,
            scale: 0.0,
            cents: 0.0,
            db: 0.0,
            cents_step: 0.0,
            db_step: 0.0,
            ramp_left: 0,
        };
        wind.set_filter(update_rate);
        wind
    }

    /// The filter for the rate set. Two one-poles in a row with
    /// `y += (1 - p)(x - y)`: the output's power is the input's times
    /// `(1 - p)(1 + p²) / (1 + p)³`, and uniform noise's is 1/3. The scale is
    /// on the input, so that the output, which is the wander, has a standard
    /// deviation of 1 at any rate, and a new rate does not move it.
    fn set_filter(&mut self, update_rate: f64) {
        let top = (update_rate / 8.0) as f32;
        let rate = self.settings.rate_hz.max(MIN_WIND_RATE_HZ).min(top);
        let pole = pole_for(f64::from(rate), update_rate);
        let p = f64::from(pole);
        let power = (1.0 - p) * (1.0 + p * p) / ((1.0 + p) * (1.0 + p) * (1.0 + p)) / 3.0;
        self.pole = pole;
        self.scale = (1.0 / power).sqrt() as f32;
    }

    fn set(&mut self, jitter: WindJitter, update_rate: f64) {
        let cents = clean(jitter.cents, MAX_JITTER_CENTS);
        let db = clean(jitter.db, MAX_JITTER_DB);
        let rate_hz = if jitter.rate_hz.is_nan() {
            WindJitter::OFF.rate_hz
        } else {
            jitter.rate_hz
        };
        let same_depths = cents == self.settings.cents && db == self.settings.db;
        self.settings = WindJitter { cents, db, rate_hz };
        self.set_filter(update_rate);
        if same_depths {
            return;
        }
        if cents == self.cents && db == self.db {
            self.ramp_left = 0;
            self.cents_step = 0.0;
            self.db_step = 0.0;
        } else {
            let per_update = 1.0 / DEPTH_RAMP_UPDATES as f32;
            self.cents_step = (cents - self.cents) * per_update;
            self.db_step = (db - self.db) * per_update;
            self.ramp_left = DEPTH_RAMP_UPDATES;
        }
    }

    fn is_active(&self) -> bool {
        self.ramp_left != 0 || self.cents != 0.0 || self.db != 0.0
    }

    /// One update: the gain and the frequency shift (a ratio less 1) to use.
    #[inline]
    fn update(&mut self) -> (f32, f32) {
        if !self.is_active() {
            return (1.0, 0.0);
        }
        if self.ramp_left != 0 {
            self.ramp_left -= 1;
            if self.ramp_left == 0 {
                self.cents = self.settings.cents;
                self.db = self.settings.db;
            } else {
                self.cents += self.cents_step;
                self.db += self.db_step;
            }
        }
        self.rng = xorshift(self.rng);
        let noise = (self.rng as i32) as f32 * (1.0 / 2_147_483_648.0);
        self.first += (1.0 - self.pole) * (noise * self.scale - self.first);
        self.second += (1.0 - self.pole) * (self.first - self.second);
        if !self.second.is_finite() {
            self.first = 0.0;
            self.second = 0.0;
        }
        let wander = self.second.clamp(-MAX_WANDER, MAX_WANDER);
        let gain_log = self.db * wander * (LN_10 / 20.0);
        let pitch_log = self.cents * wander * (LN_2 / 1_200.0);
        (1.0 + exp_minus_one(gain_log), exp_minus_one(pitch_log))
    }
}

/// `x` between 0 and `top`, a NaN being 0.
#[inline]
fn clean(x: f32, top: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, top) }
}

/// The state behind the bank's voicing, in arrays with one element per
/// partial, as the bank's own are.
#[derive(Debug, Clone)]
pub(crate) struct Voicing {
    update_rate: f64,
    seed: u64,
    // What each update reads and writes for every partial.
    rng: Vec<u32>,
    noise_re: Vec<f32>,
    noise_im: Vec<f32>,
    pole: Vec<f32>,
    drive: Vec<f32>,
    air: Vec<f32>,
    level: Vec<f32>,
    breath: Vec<f32>,
    pitch: Vec<f32>,
    /// What the kernel multiplies each phasor by.
    pub(crate) mod_re: Vec<f32>,
    pub(crate) mod_im: Vec<f32>,
    pub(crate) mod_re_step: Vec<f32>,
    pub(crate) mod_im_step: Vec<f32>,
    /// The rotations the kernel uses if [`shifting`](Self::shifting).
    pub(crate) cos_turn: Vec<f32>,
    pub(crate) sin_turn: Vec<f32>,
    // What changes now and then.
    air_target: Vec<f32>,
    air_step: Vec<f32>,
    air_ramp_left: Vec<u32>,
    bandwidth: Vec<f32>,
    rest_pole: Vec<f32>,
    attacks: Attacks,
    /// Partials with air now or to come.
    airy: usize,
    air_ramping: usize,
    attacking: usize,
    /// Attacks with a pitch to settle.
    bending: usize,
    /// Updates before the next attack can have ended.
    updates_to_attack_end: u32,
    wind: Wind,
    /// Whether the kernel with modulators runs.
    pub(crate) on: bool,
    /// Whether the rotations are shifted.
    pub(crate) shifting: bool,
    /// Whether the last update had nothing left to do.
    idle: bool,
}

impl Voicing {
    pub(crate) fn new(room: usize, sample_rate: f64) -> Self {
        let update_rate = sample_rate / VOICE_FRAMES as f64;
        let rest = pole_for(f64::from(DEFAULT_AIR_BANDWIDTH_HZ) / 2.0, update_rate);
        const DEFAULT_SEED: u64 = 1;
        Self {
            update_rate,
            seed: DEFAULT_SEED,
            rng: (0..room).map(|i| seeded(DEFAULT_SEED, i as u64)).collect(),
            noise_re: vec![0.0; room],
            noise_im: vec![0.0; room],
            pole: vec![rest; room],
            drive: vec![drive_for(rest); room],
            air: vec![0.0; room],
            level: vec![1.0; room],
            breath: vec![0.0; room],
            pitch: vec![0.0; room],
            mod_re: vec![1.0; room],
            mod_im: vec![0.0; room],
            mod_re_step: vec![0.0; room],
            mod_im_step: vec![0.0; room],
            cos_turn: vec![1.0; room],
            sin_turn: vec![0.0; room],
            air_target: vec![0.0; room],
            air_step: vec![0.0; room],
            air_ramp_left: vec![0; room],
            bandwidth: vec![DEFAULT_AIR_BANDWIDTH_HZ; room],
            rest_pole: vec![rest; room],
            attacks: Attacks::new(room),
            airy: 0,
            air_ramping: 0,
            attacking: 0,
            bending: 0,
            updates_to_attack_end: 0,
            wind: Wind::new(update_rate, DEFAULT_SEED),
            on: false,
            shifting: false,
            idle: false,
        }
    }

    pub(crate) fn set_sample_rate(&mut self, sample_rate: f64) {
        self.update_rate = sample_rate / VOICE_FRAMES as f64;
        for i in 0..self.bandwidth.len() {
            self.apply_bandwidth(i);
        }
        self.wind.set_filter(self.update_rate);
    }

    pub(crate) fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
        for (i, state) in self.rng.iter_mut().enumerate() {
            *state = seeded(seed, i as u64);
        }
        self.wind.rng = seeded(seed, u64::MAX);
    }

    pub(crate) fn seed(&self) -> u64 {
        self.seed
    }

    /// Partial `i` back to the state of one never used. Its generator goes
    /// on from where it was.
    pub(crate) fn forget(&mut self, i: usize) {
        self.end_attack(i);
        if self.air_ramp_left[i] != 0 {
            self.air_ramp_left[i] = 0;
            self.air_ramping -= 1;
        }
        if self.air[i] != 0.0 || self.air_target[i] != 0.0 {
            self.airy -= 1;
        }
        self.air[i] = 0.0;
        self.air_target[i] = 0.0;
        self.air_step[i] = 0.0;
        self.bandwidth[i] = DEFAULT_AIR_BANDWIDTH_HZ;
        self.apply_bandwidth(i);
        self.noise_re[i] = 0.0;
        self.noise_im[i] = 0.0;
        self.level[i] = 1.0;
        self.breath[i] = 0.0;
        self.pitch[i] = 0.0;
        self.mod_re[i] = 1.0;
        self.mod_im[i] = 0.0;
        self.mod_re_step[i] = 0.0;
        self.mod_im_step[i] = 0.0;
        self.cos_turn[i] = 1.0;
        self.sin_turn[i] = 0.0;
    }

    /// The highest bandwidth this sample rate holds.
    fn top_bandwidth(&self) -> f32 {
        (self.update_rate / 2.0) as f32
    }

    /// Puts partial `i`'s noise at the bandwidth asked for, or as near as
    /// this sample rate holds. What was asked stays, in hertz, for the next.
    fn apply_bandwidth(&mut self, i: usize) {
        let used = self.bandwidth[i].min(self.top_bandwidth());
        let pole = pole_for(f64::from(used) / 2.0, self.update_rate);
        self.rest_pole[i] = pole;
        self.pole[i] = pole;
        self.drive[i] = drive_for(pole);
    }

    pub(crate) fn set_bandwidth(&mut self, i: usize, bandwidth_hz: f32) {
        let bandwidth = if bandwidth_hz.is_nan() {
            DEFAULT_AIR_BANDWIDTH_HZ
        } else {
            bandwidth_hz.max(MIN_BANDWIDTH_HZ)
        };
        if bandwidth != self.bandwidth[i] {
            self.bandwidth[i] = bandwidth;
            self.apply_bandwidth(i);
        }
    }

    pub(crate) fn set_depth(&mut self, i: usize, depth: f32) {
        let depth = clean(depth, 1.0);
        if depth == self.air_target[i] {
            return;
        }
        let was_airy = self.air[i] != 0.0 || self.air_target[i] != 0.0;
        let was_ramping = self.air_ramp_left[i] != 0;
        self.air_target[i] = depth;
        if depth == self.air[i] {
            self.air_step[i] = 0.0;
            self.air_ramp_left[i] = 0;
            if was_ramping {
                self.air_ramping -= 1;
            }
        } else {
            self.air_step[i] = (depth - self.air[i]) * (1.0 / DEPTH_RAMP_UPDATES as f32);
            self.air_ramp_left[i] = DEPTH_RAMP_UPDATES;
            if !was_ramping {
                self.air_ramping += 1;
            }
        }
        let is_airy = self.air[i] != 0.0 || depth != 0.0;
        if is_airy && !was_airy {
            self.airy += 1;
            self.wake();
        } else if was_airy && !is_airy {
            self.airy -= 1;
        }
    }

    pub(crate) fn air_of(&self, i: usize) -> crate::PartialAir {
        crate::PartialAir {
            depth: self.air_target[i],
            bandwidth_hz: self.bandwidth[i],
        }
    }

    pub(crate) fn is_attacking(&self, i: usize) -> bool {
        self.attacks.is_active(i)
    }

    /// Ends partial `i`'s attack, if it has one, for the next update to
    /// bring its level back from wherever it was.
    fn end_attack(&mut self, i: usize) {
        if self.attacks.is_active(i) {
            self.attacking -= 1;
            if self.attacks.pitch_log[i] != 0.0 {
                self.bending -= 1;
            }
            self.attacks.clear(i);
        }
    }

    /// Starts an attack on partial `i`, `to_update` frames before the next
    /// update (0 if the next frame has one): from silence, or, if `from_here`,
    /// from the level the partial has now.
    pub(crate) fn start_attack(
        &mut self,
        i: usize,
        attack: &PartialAttack,
        to_update: usize,
        from_here: bool,
    ) {
        let frames = |x: u32| x.min(MAX_ATTACK_FRAMES);
        let overshoot = clean(attack.overshoot, 1.0);
        let breath = clean(attack.breath, 1.0);
        let cents = if attack.pitch_cents.is_nan() {
            0.0
        } else {
            attack.pitch_cents.clamp(-MAX_PITCH_CENTS, MAX_PITCH_CENTS)
        };
        let (delay, rise, settle) = (
            frames(attack.delay_frames),
            frames(attack.rise_frames),
            frames(attack.settle_frames),
        );
        let breath_frames = if breath == 0.0 {
            0
        } else {
            frames(attack.breath_frames)
        };
        let pitch_frames = if cents == 0.0 {
            0
        } else {
            frames(attack.pitch_frames)
        };
        let settle = if overshoot == 0.0 { 0 } else { settle };

        self.end_attack(i);
        if delay == 0 && rise == 0 && settle == 0 && breath_frames == 0 && pitch_frames == 0 {
            self.level[i] = 1.0;
            self.breath[i] = 0.0;
            self.pitch[i] = 0.0;
            self.restore_noise(i);
            return;
        }
        let per = |frames: u32| {
            if frames == 0 {
                1.0
            } else {
                1.0 / frames as f32
            }
        };
        let (breath_pole, breath_share) = if breath_frames != 0 && attack.breath_bandwidth_hz > 0.0
        {
            let bandwidth = attack
                .breath_bandwidth_hz
                .max(MIN_BANDWIDTH_HZ)
                .min(self.top_bandwidth());
            (pole_for(f64::from(bandwidth) / 2.0, self.update_rate), 1.0)
        } else {
            (0.0, 0.0)
        };
        // The breath counts from the first update, so that what is heard of
        // it does not depend on how far off that was.
        let breath_start = to_update as u32 + VOICE_FRAMES as u32;
        let breath_ends = if breath_frames == 0 {
            0
        } else {
            breath_start + breath_frames
        };
        let ends = (delay + rise + settle)
            .max(breath_ends)
            .max(delay + pitch_frames);
        let from = if from_here {
            clean(self.mod_re[i], MAX_START_LEVEL)
        } else {
            0.0
        };
        let attacks = &mut self.attacks;
        attacks.age[i] = to_update as f32;
        attacks.delay[i] = delay as f32;
        attacks.rise[i] = rise as f32;
        attacks.per_rise[i] = per(rise);
        attacks.per_settle[i] = per(settle);
        attacks.peak[i] = 1.0 + if settle == 0 { 0.0 } else { overshoot };
        attacks.from[i] = from;
        attacks.breath[i] = if breath_frames == 0 { 0.0 } else { breath };
        attacks.breath_start[i] = breath_start as f32;
        attacks.per_breath[i] = per(breath_frames);
        attacks.breath_pole[i] = breath_pole;
        attacks.breath_share[i] = breath_share;
        attacks.pitch_log[i] = if pitch_frames == 0 {
            0.0
        } else {
            cents * (LN_2 / 1_200.0)
        };
        attacks.per_pitch[i] = per(pitch_frames);
        attacks.ends[i] = ends as f32;
        self.attacking += 1;
        if attacks.pitch_log[i] != 0.0 {
            self.bending += 1;
        }
        let updates = ends
            .saturating_sub(to_update as u32)
            .div_ceil(VOICE_FRAMES as u32);
        self.updates_to_attack_end = if self.attacking == 1 {
            updates
        } else {
            self.updates_to_attack_end.min(updates)
        };
        self.wake();
        let level = self.attacks.level_now(i);
        self.level[i] = level;
        self.breath[i] = 0.0;
        self.pitch[i] = self.attacks.shift_now(i);
        if from_here {
            // The modulator goes on where it was going until the next update
            // takes over, and that one starts from where it has got to.
            return;
        }
        // The partial is silent from this frame, and the frames up to the
        // next update bring it to where that update takes over.
        self.mod_re[i] = 0.0;
        self.mod_im[i] = 0.0;
        self.mod_re_step[i] = if to_update == 0 {
            0.0
        } else {
            level / to_update as f32
        };
        self.mod_im_step[i] = 0.0;
    }

    fn restore_noise(&mut self, i: usize) {
        self.pole[i] = self.rest_pole[i];
        self.drive[i] = drive_for(self.pole[i]);
    }

    pub(crate) fn set_wind(&mut self, jitter: WindJitter) {
        self.wind.set(jitter, self.update_rate);
        if self.wind.is_active() {
            self.wake();
        }
    }

    pub(crate) fn wind(&self) -> WindJitter {
        self.wind.settings
    }

    fn wake(&mut self) {
        self.on = true;
        self.idle = false;
    }

    /// Whether anything still has work to do.
    fn is_busy(&self) -> bool {
        self.airy != 0 || self.attacking != 0 || self.wind.is_active()
    }

    /// One update: called every [`VOICE_FRAMES`] frames while
    /// [`on`](Self::on), counted across renders, with the rotations the
    /// caller has set for the first `used` partials. Works out where each
    /// modulator should be `VOICE_FRAMES` frames from now, and how far
    /// each rotation is shifted until then.
    pub(crate) fn update(&mut self, used: usize, cos_step: &[f32], sin_step: &[f32]) {
        if self.is_busy() {
            self.idle = false;
        } else if self.idle {
            // The last update had nothing to do either, so the modulators
            // have arrived at 1 and the plain kernel takes over from here.
            self.turn_off();
            return;
        } else {
            self.idle = true;
        }
        let (wind_gain, wind_shift) = self.wind.update();
        if self.air_ramping != 0 {
            self.advance_air_ramps(used);
        }
        if self.attacking != 0 {
            self.advance_attacks(used);
        }
        add_noise(
            &mut self.rng[..used],
            &mut self.noise_re[..used],
            &mut self.noise_im[..used],
            &self.pole[..used],
            &self.drive[..used],
            &self.air[..used],
            &self.level[..used],
            &self.breath[..used],
            &mut self.mod_re[..used],
            &mut self.mod_im[..used],
            &mut self.mod_re_step[..used],
            &mut self.mod_im_step[..used],
            wind_gain,
        );
        self.shifting = wind_shift != 0.0 || self.bending != 0;
        if self.shifting {
            shift_rotations(
                &cos_step[..used],
                &sin_step[..used],
                &self.pitch[..used],
                wind_shift,
                &mut self.cos_turn[..used],
                &mut self.sin_turn[..used],
            );
        }
    }

    fn turn_off(&mut self) {
        self.on = false;
        self.shifting = false;
        self.idle = false;
        self.mod_re.fill(1.0);
        self.mod_im.fill(0.0);
        self.mod_re_step.fill(0.0);
        self.mod_im_step.fill(0.0);
    }

    fn advance_air_ramps(&mut self, used: usize) {
        for i in 0..used {
            if self.air_ramp_left[i] == 0 {
                continue;
            }
            self.air_ramp_left[i] -= 1;
            if self.air_ramp_left[i] == 0 {
                self.air_ramping -= 1;
                self.air[i] = self.air_target[i];
                self.air_step[i] = 0.0;
                if self.air[i] == 0.0 {
                    self.airy -= 1;
                }
            } else {
                self.air[i] += self.air_step[i];
            }
        }
    }

    fn advance_attacks(&mut self, used: usize) {
        let attacks = &mut self.attacks;
        update_attacks(
            &mut attacks.age[..used],
            &attacks.delay[..used],
            &attacks.rise[..used],
            &attacks.per_rise[..used],
            &attacks.per_settle[..used],
            &attacks.peak[..used],
            &attacks.from[..used],
            &attacks.breath[..used],
            &attacks.breath_start[..used],
            &attacks.per_breath[..used],
            &attacks.breath_pole[..used],
            &attacks.breath_share[..used],
            &attacks.pitch_log[..used],
            &attacks.per_pitch[..used],
            &self.rest_pole[..used],
            &mut self.level[..used],
            &mut self.breath[..used],
            &mut self.pitch[..used],
            &mut self.pole[..used],
            &mut self.drive[..used],
        );
        self.updates_to_attack_end = self.updates_to_attack_end.saturating_sub(1);
        if self.updates_to_attack_end != 0 {
            return;
        }
        let mut soonest = f32::INFINITY;
        for i in 0..used {
            if !self.attacks.is_active(i) {
                continue;
            }
            let left = self.attacks.ends[i] - self.attacks.age[i];
            if left <= 0.0 {
                self.end_attack(i);
                self.level[i] = 1.0;
                self.breath[i] = 0.0;
                self.pitch[i] = 0.0;
                self.restore_noise(i);
            } else {
                soonest = soonest.min(left);
            }
        }
        if soonest != f32::INFINITY {
            self.updates_to_attack_end = (soonest as u32).div_ceil(VOICE_FRAMES as u32);
        }
    }
}

/// Moves every attack on one update and works out what it means now. With
/// nothing in a slot ([`Attacks::clear`]) this gives a level of 1, no breath,
/// no shift and the partial's own pole, exactly. It vectorises as
/// [`add_noise`] does.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn update_attacks(
    age: &mut [f32],
    delay: &[f32],
    rise: &[f32],
    per_rise: &[f32],
    per_settle: &[f32],
    peak: &[f32],
    from: &[f32],
    breath_depth: &[f32],
    breath_start: &[f32],
    per_breath: &[f32],
    breath_pole: &[f32],
    breath_share: &[f32],
    pitch_log: &[f32],
    per_pitch: &[f32],
    rest_pole: &[f32],
    level: &mut [f32],
    breath: &mut [f32],
    pitch: &mut [f32],
    pole: &mut [f32],
    drive: &mut [f32],
) {
    let n = age.len();
    let (delay, rise, per_rise, per_settle, peak, from) = (
        &delay[..n],
        &rise[..n],
        &per_rise[..n],
        &per_settle[..n],
        &peak[..n],
        &from[..n],
    );
    let (breath_depth, breath_start, per_breath, breath_pole, breath_share) = (
        &breath_depth[..n],
        &breath_start[..n],
        &per_breath[..n],
        &breath_pole[..n],
        &breath_share[..n],
    );
    let (pitch_log, per_pitch, rest_pole) = (&pitch_log[..n], &per_pitch[..n], &rest_pole[..n]);
    let (level, breath, pitch, pole, drive) = (
        &mut level[..n],
        &mut breath[..n],
        &mut pitch[..n],
        &mut pole[..n],
        &mut drive[..n],
    );
    for i in 0..n {
        let now = age[i] + VOICE_FRAMES as f32;
        age[i] = now;
        level[i] = attack_level(
            now,
            delay[i],
            rise[i],
            per_rise[i],
            per_settle[i],
            peak[i],
            from[i],
        );
        let left = (1.0 - (now - breath_start[i]) * per_breath[i]).clamp(0.0, 1.0);
        breath[i] = breath_depth[i] * (left * left);
        pitch[i] = attack_shift(now, delay[i], pitch_log[i], per_pitch[i]);
        // Between two poles, so inside what a pole may be, whatever either
        // of them has become since the attack began.
        let rest = rest_pole[i];
        let moved = rest + (breath_pole[i] - rest) * (left * breath_share[i]);
        pole[i] = moved;
        drive[i] = drive_for(moved);
    }
}

/// Moves on one update the noise of every partial that has any, so that a
/// partial's stream depends on its own settings and on no other's, and aims
/// every modulator at `wind (level + (level air + breath) n)`, for the
/// complex noise `n`. Nothing that is not finite stays in the state. Written as one loop over slices the compiler knows are apart, so that
/// it vectorises on the baseline instruction set; every operation is exact
/// IEEE arithmetic, so the result does not depend on how it was compiled.
#[allow(clippy::too_many_arguments)]
#[inline(never)]
fn add_noise(
    rng: &mut [u32],
    noise_re: &mut [f32],
    noise_im: &mut [f32],
    pole: &[f32],
    drive: &[f32],
    air: &[f32],
    level: &[f32],
    breath: &[f32],
    mod_re: &mut [f32],
    mod_im: &mut [f32],
    mod_re_step: &mut [f32],
    mod_im_step: &mut [f32],
    wind_gain: f32,
) {
    const SCALE: f32 = 1.0 / 32_768.0;
    let n = rng.len();
    let (noise_re, noise_im) = (&mut noise_re[..n], &mut noise_im[..n]);
    let (pole, drive, air, level, breath) = (
        &pole[..n],
        &drive[..n],
        &air[..n],
        &level[..n],
        &breath[..n],
    );
    let (mod_re, mod_im) = (&mut mod_re[..n], &mut mod_im[..n]);
    let (mod_re_step, mod_im_step) = (&mut mod_re_step[..n], &mut mod_im_step[..n]);
    for i in 0..n {
        let amount = level[i] * air[i] + breath[i];
        let has_air = amount != 0.0;
        let state = if has_air { xorshift(rng[i]) } else { rng[i] };
        rng[i] = state;
        // Two numbers from one state: its high and low halves, each uniform
        // from -1 to 1, and each pair of them occurs once in the period.
        let high = ((state as i32) >> 16) as f32 * SCALE;
        let low = (((state << 16) as i32) >> 16) as f32 * SCALE;
        let next_re = pole[i] * noise_re[i] + drive[i] * high;
        let next_im = pole[i] * noise_im[i] + drive[i] * low;
        let re = if has_air { next_re } else { noise_re[i] };
        let im = if has_air { next_im } else { noise_im[i] };
        let re = if re.is_finite() { re } else { 0.0 };
        let im = if im.is_finite() { im } else { 0.0 };
        noise_re[i] = re;
        noise_im[i] = im;
        let aim_re = wind_gain * (level[i] + amount * re);
        let aim_im = wind_gain * (amount * im);
        let now_re = if mod_re[i].is_finite() {
            mod_re[i]
        } else {
            aim_re
        };
        let now_im = if mod_im[i].is_finite() {
            mod_im[i]
        } else {
            aim_im
        };
        mod_re[i] = now_re;
        mod_im[i] = now_im;
        mod_re_step[i] = (aim_re - now_re) * PER_VOICE_FRAME;
        mod_im_step[i] = (aim_im - now_im) * PER_VOICE_FRAME;
    }
}

/// Shifts each rotation by the wind's share and the partial's own: an angle
/// `w` becomes `w (1 + shift)`, by turning the rotation through `w · shift`.
/// The same ratio for every partial keeps harmonics harmonic.
#[inline(never)]
fn shift_rotations(
    cos_step: &[f32],
    sin_step: &[f32],
    pitch: &[f32],
    wind_shift: f32,
    cos_turn: &mut [f32],
    sin_turn: &mut [f32],
) {
    let n = cos_step.len();
    let (sin_step, pitch) = (&sin_step[..n], &pitch[..n]);
    let (cos_turn, sin_turn) = (&mut cos_turn[..n], &mut sin_turn[..n]);
    for i in 0..n {
        let (c, s) = (cos_step[i], sin_step[i]);
        let shift = wind_shift + pitch[i] + wind_shift * pitch[i];
        let delta = rotation_angle(c, s) * shift;
        let d2 = delta * delta;
        // The shift is at most two whole tones' ratio less 1 (a pitch of 100
        // cents and a jitter of 25 at four standard deviations): a turn of
        // 0.4 radians at Nyquist, where these series are good to 2e-8.
        let turn_c = 1.0 - d2 * (1.0 / 2.0 - d2 * (1.0 / 24.0 - d2 / 720.0));
        let turn_s = delta * (1.0 - d2 * (1.0 / 6.0 - d2 * (1.0 / 120.0 - d2 / 5_040.0)));
        cos_turn[i] = c * turn_c - s * turn_s;
        sin_turn[i] = c * turn_s + s * turn_c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_angle_of_a_rotation_is_its_angle() {
        // Within 2e-6 radians, where the series stops, and within that
        // share of itself for a small angle.
        let mut worst = 0.0f64;
        let mut worst_small = 0.0f64;
        for k in -4_000..=4_000 {
            let w = f64::from(k) / 4_000.0 * core::f64::consts::PI;
            let got = f64::from(rotation_angle(w.cos() as f32, w.sin() as f32));
            let error = (got - w).abs();
            worst = worst.max(error);
            if w != 0.0 && w.abs() < 1.0 {
                worst_small = worst_small.max(error / w.abs());
            }
        }
        assert!(worst < 2e-6, "{worst}");
        assert!(worst_small < 2e-6, "{worst_small}");
        assert_eq!(rotation_angle(1.0, 0.0), 0.0);
    }

    #[test]
    fn the_pole_has_its_corner_where_it_was_asked() {
        for (corner, rate) in [
            (1.0, 1_500.0),
            (10.0, 1_500.0),
            (100.0, 1_500.0),
            (375.0, 1_500.0),
        ] {
            let pole = f64::from(pole_for(corner, rate));
            let w = core::f64::consts::TAU * corner / rate;
            // |1 / (1 - p e^-iw)|² against its value at w = 0.
            let power = (1.0 - pole) * (1.0 - pole) / (1.0 - 2.0 * pole * w.cos() + pole * pole);
            assert!((power - 0.5).abs() < 2e-3, "{corner}: {power}");
        }
    }

    #[test]
    fn the_series_are_as_close_as_they_say() {
        for k in 0..=100 {
            let w = f64::from(k) / 100.0 * core::f64::consts::FRAC_PI_2;
            assert!((cos_series(w) - w.cos()).abs() < 3e-5, "{w}");
        }
        for k in -100..=100 {
            let x = k as f32 / 100.0 * 0.1;
            let exact = f64::from(x).exp_m1();
            assert!((f64::from(exp_minus_one(x)) - exact).abs() < 5e-6, "{x}");
        }
    }

    fn poles_of(voicing: &Voicing, used: usize) -> Vec<f32> {
        voicing.pole[..used].to_vec()
    }

    #[test]
    fn the_noises_pole_stays_inside_the_circle_as_settings_change_under_it() {
        let attack = |bandwidth: f32| PartialAttack {
            rise_frames: 480,
            breath: 0.2,
            breath_frames: 1_920,
            breath_bandwidth_hz: bandwidth,
            ..PartialAttack::default()
        };
        let (cos_step, sin_step) = ([1.0f32; 16], [0.0f32; 16]);
        for (first, breath, then) in [
            // The new pole less the old distance went past 1.
            (300.0, 50.0, 10.0),
            // And below 0.
            (10.0, 700.0, 750.0),
        ] {
            let mut voicing = Voicing::new(16, 48_000.0);
            voicing.set_bandwidth(0, first);
            voicing.set_depth(0, 0.02);
            voicing.start_attack(0, &attack(breath), 0, false);
            voicing.update(16, &cos_step, &sin_step);
            voicing.set_bandwidth(0, then);
            for _ in 0..80 {
                voicing.update(16, &cos_step, &sin_step);
                for pole in poles_of(&voicing, 16) {
                    assert!(pole > 0.0 && pole < 1.0, "{first} {breath} {then}: {pole}");
                }
            }
        }
    }

    #[test]
    fn nothing_that_is_not_finite_stays_in_the_state() {
        let (cos_step, sin_step) = ([1.0f32; 16], [0.0f32; 16]);
        let mut voicing = Voicing::new(16, 48_000.0);
        voicing.set_depth(0, 0.1);
        voicing.set_wind(WindJitter {
            cents: 1.0,
            db: 0.1,
            rate_hz: 4.0,
        });
        voicing.update(16, &cos_step, &sin_step);
        voicing.noise_re[0] = f32::NAN;
        voicing.noise_im[0] = f32::INFINITY;
        voicing.mod_re[0] = f32::NAN;
        voicing.mod_im[0] = f32::NEG_INFINITY;
        voicing.wind.first = f32::NAN;
        voicing.wind.second = f32::NAN;
        voicing.update(16, &cos_step, &sin_step);
        for x in [
            voicing.noise_re[0],
            voicing.noise_im[0],
            voicing.mod_re[0],
            voicing.mod_im[0],
            voicing.mod_re_step[0],
            voicing.mod_im_step[0],
            voicing.wind.first,
            voicing.wind.second,
        ] {
            assert!(x.is_finite(), "{x}");
        }
    }

    #[test]
    fn a_pole_is_below_one_at_any_rate_and_the_wind_has_a_finite_scale() {
        for rate in [1e-300, 1.0, 48_000.0, 2.2e9, 1e300] {
            for corner in [0.0, 0.01, 1.0, 1e6] {
                let pole = pole_for(corner, rate);
                assert!(pole > 0.0 && pole < 1.0, "{corner} at {rate}: {pole}");
            }
            let wind = Wind::new(rate, 1);
            assert!(wind.scale.is_finite(), "at {rate}");
        }
    }

    #[test]
    fn a_seed_gives_no_stuck_generators() {
        for seed in [0, 1, u64::MAX] {
            for stream in 0..1_000 {
                assert_ne!(seeded(seed, stream), 0);
            }
        }
    }
}
