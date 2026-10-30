//! Voices and choirs by formant synthesis: sung vowels, from one singer to a
//! section.

use core::f64::consts::{PI, TAU};

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_midi::MidiMessage;

use super::common::{
    Handover, Noise, SILENCE, Tone, add_to_every_channel, db, flush, key_hz, setting, t60_factor,
    velocity_gain,
};
use super::modelling::{Dynamics, Glide, Vibrato, VibratoShape};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{ProcessSpec, Voice};

/// The most singers a [`ChoirVoice`] can be.
pub const MAX_SINGERS: usize = 6;

/// How many formants the vocal tract is given.
pub const FORMANTS: usize = 5;

/// How many samples apart the formants are retuned while a vowel moves.
const RETUNE_HOP: u32 = 32;

/// How long the singers' level takes to follow a controller.
const VOICE_SECONDS: f64 = 0.02;

/// The vowels, as [`ChoirParams::vowel`] numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vowel {
    /// "a", as in "father", 0.
    A,
    /// "e", as in "bed", 1.
    E,
    /// "i", as in "see", 2.
    I,
    /// "o", as in "law", 3.
    O,
    /// "u", as in "food", 4.
    U,
}

impl Vowel {
    /// The vowel's place on [`ChoirParams::vowel`].
    pub const fn position(self) -> f64 {
        match self {
            Self::A => 0.0,
            Self::E => 1.0,
            Self::I => 2.0,
            Self::O => 3.0,
            Self::U => 4.0,
        }
    }
}

/// A vowel's formants, `(frequency, bandwidth)` in hertz, for a man's and a
/// woman's voice, in the order of [`Vowel`].
///
/// The first three formants are the averages G. E. Peterson and H. L.
/// Barney measured for men and women ("Control methods used in a study of
/// the vowels", JASA 24, 1952) for the vowels of "father", "bed", "heed",
/// "law" and "who'd"; the fourth and fifth, and the bandwidths, are typical
/// values (D. H. Klatt, "Software for a cascade/parallel formant
/// synthesizer", JASA 67, 1980), the men's fourth drawn down near the third
/// as in a trained singer's voice, the singer's formant around 2.8 kHz (J.
/// Sundberg, "The Science of the Singing Voice", 1987).
const MEN: [[(f64, f64); FORMANTS]; 5] = [
    [
        (730.0, 80.0),
        (1_090.0, 90.0),
        (2_440.0, 120.0),
        (2_900.0, 150.0),
        (3_400.0, 200.0),
    ],
    [
        (530.0, 60.0),
        (1_840.0, 100.0),
        (2_480.0, 120.0),
        (2_900.0, 150.0),
        (3_400.0, 200.0),
    ],
    [
        (270.0, 60.0),
        (2_290.0, 100.0),
        (3_010.0, 120.0),
        (3_400.0, 150.0),
        (3_900.0, 200.0),
    ],
    [
        (570.0, 70.0),
        (840.0, 80.0),
        (2_410.0, 120.0),
        (2_850.0, 150.0),
        (3_400.0, 200.0),
    ],
    [
        (300.0, 60.0),
        (870.0, 80.0),
        (2_240.0, 120.0),
        (2_800.0, 150.0),
        (3_400.0, 200.0),
    ],
];
const WOMEN: [[(f64, f64); FORMANTS]; 5] = [
    [
        (850.0, 90.0),
        (1_220.0, 100.0),
        (2_810.0, 130.0),
        (3_800.0, 160.0),
        (4_500.0, 200.0),
    ],
    [
        (610.0, 70.0),
        (2_330.0, 110.0),
        (2_990.0, 130.0),
        (3_900.0, 160.0),
        (4_500.0, 200.0),
    ],
    [
        (310.0, 60.0),
        (2_790.0, 110.0),
        (3_310.0, 130.0),
        (4_100.0, 160.0),
        (4_700.0, 200.0),
    ],
    [
        (590.0, 80.0),
        (920.0, 90.0),
        (2_710.0, 130.0),
        (3_800.0, 160.0),
        (4_500.0, 200.0),
    ],
    [
        (370.0, 70.0),
        (950.0, 90.0),
        (2_670.0, 130.0),
        (3_800.0, 160.0),
        (4_500.0, 200.0),
    ],
];

/// Below this key the voices are men's, above the next women's, and
/// between they blend: a choir across the keyboard, basses to sopranos.
const MEN_UNTIL: f64 = 55.0;
const WOMEN_FROM: f64 = 67.0;

/// A singer raises the first formant to just above the note when the note
/// would otherwise be above it (Sundberg), so a high note keeps its power.
const F1_ABOVE_PITCH: f64 = 1.1;

/// How high the first formant can be raised: about where a soprano's goes
/// at the top of her range.
const F1_HIGHEST: f64 = 1_200.0;

/// How many octaves darker the source is at the lowest velocity than at
/// the highest, and how much more open the glottis.
const SOFT_OCTAVES: f64 = 3.0;
const SOFT_OPENING: f64 = 0.2;

/// The frequency at which the source's level is kept as the tilt changes
/// with effort: the low harmonics', which carry most of a sung vowel's
/// power, so velocity changes the colour without changing the level twice.
const TILT_REFERENCE: f64 = 300.0;

/// The settings of a [`ChoirVoice`].
///
/// Times are in seconds, frequencies in hertz and levels in decibels
/// unless said otherwise. Out-of-range values are limited when the
/// settings are taken (see [`ChoirVoice::set_params`]), and a value that
/// is not a number takes the default of [`aah_choir`](Self::aah_choir).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChoirParams {
    /// The vowel, 0 to 4: a, e, i, o and u ([`Vowel::position`]), and
    /// between them a blend.
    pub vowel: f64,
    /// How many singers sing each note, 1 to [`MAX_SINGERS`].
    pub singers: usize,
    /// How far apart they are tuned, in cents from the lowest to the
    /// highest, 0 to 50.
    pub spread: f64,
    /// How much each voice's pitch and level wobble at random, 0 to 1: the
    /// jitter and shimmer of a real voice.
    pub jitter: f64,
    /// How much breath the voice carries, 0 to 1: noise from the glottis,
    /// strongest while it is open.
    pub breathiness: f64,
    /// The share of each period the glottis is open, 0.3 to 0.9: lower is
    /// pressed and bright, higher breathy and soft.
    pub open_quotient: f64,
    /// How long the voice takes to sound, 0.005 to 2.
    pub attack: f64,
    /// How long it takes to fade once released, 0.01 to 4.
    pub release: f64,
    /// The vibrato's rate, 0 to 12.
    pub vibrato_rate: f64,
    /// Its depth, the swing either way in cents, 0 to 200.
    pub vibrato_depth: f64,
    /// How long after the start of a note it begins, 0 to 4.
    pub vibrato_delay: f64,
    /// How long it then takes to grow, 0.01 to 4.
    pub vibrato_rise: f64,
    /// How long the pitch takes to move to a slurred note, 0 to 1.
    pub legato_time: f64,
    /// How long a change of vowel takes, 0.005 to 1.
    pub vowel_time: f64,
    /// How much longer or shorter the vocal tract is than an average one,
    /// as a factor on every formant, 0.7 to 1.4: below 1 a darker,
    /// larger voice.
    pub formant_shift: f64,
    /// The brightness of the voice at full velocity, as the cut-off of the
    /// glottal source's tilt, 500 to 12 000; softer singing is darker, by
    /// up to three octaves at the lowest velocity, and breathier.
    pub brightness: f64,
    /// How much quieter the lowest velocity is than the highest, 0 to 60.
    pub velocity_range_db: f64,
    /// How much louder an accented note is, 0 to 24.
    pub accent_db: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl ChoirParams {
    /// A choir singing "aah": six voices per note, tuned up to 18 cents
    /// apart, each with its own vibrato and wobble, a slow attack.
    pub fn aah_choir() -> Self {
        Self {
            vowel: Vowel::A.position(),
            singers: MAX_SINGERS,
            spread: 18.0,
            jitter: 0.5,
            breathiness: 0.2,
            open_quotient: 0.6,
            attack: 0.18,
            release: 0.35,
            vibrato_rate: 5.2,
            vibrato_depth: 22.0,
            vibrato_delay: 0.2,
            vibrato_rise: 0.4,
            legato_time: 0.06,
            vowel_time: 0.12,
            formant_shift: 1.0,
            brightness: 4_000.0,
            velocity_range_db: 18.0,
            accent_db: 3.0,
            level: 0.08,
        }
    }

    /// A choir singing "ooh": rounder and darker, more breath.
    pub fn ooh_choir() -> Self {
        Self {
            vowel: Vowel::U.position(),
            breathiness: 0.3,
            open_quotient: 0.7,
            brightness: 2_500.0,
            level: 0.1,
            ..Self::aah_choir()
        }
    }

    /// A single singer on a vowel, a lead line: a wider, slower-starting
    /// vibrato, a brighter, pressed voice, and no wobble between singers.
    pub fn solo_voice() -> Self {
        Self {
            vowel: Vowel::A.position(),
            singers: 1,
            spread: 0.0,
            jitter: 0.3,
            breathiness: 0.12,
            open_quotient: 0.5,
            attack: 0.06,
            release: 0.2,
            vibrato_rate: 5.6,
            vibrato_depth: 40.0,
            vibrato_delay: 0.3,
            vibrato_rise: 0.3,
            legato_time: 0.08,
            vowel_time: 0.1,
            formant_shift: 1.0,
            brightness: 6_000.0,
            velocity_range_db: 18.0,
            accent_db: 3.0,
            level: 0.12,
        }
    }

    /// The settings with every value limited to its range.
    fn limited(self) -> Self {
        let default = Self::aah_choir();
        Self {
            vowel: setting(self.vowel, 0.0, 4.0, default.vowel),
            singers: self.singers.clamp(1, MAX_SINGERS),
            spread: setting(self.spread, 0.0, 50.0, 0.0),
            jitter: setting(self.jitter, 0.0, 1.0, 0.0),
            breathiness: setting(self.breathiness, 0.0, 1.0, 0.0),
            open_quotient: setting(self.open_quotient, 0.3, 0.9, default.open_quotient),
            attack: setting(self.attack, 0.005, 2.0, default.attack),
            release: setting(self.release, 0.01, 4.0, default.release),
            vibrato_rate: setting(self.vibrato_rate, 0.0, 12.0, default.vibrato_rate),
            vibrato_depth: setting(self.vibrato_depth, 0.0, 200.0, 0.0),
            vibrato_delay: setting(self.vibrato_delay, 0.0, 4.0, default.vibrato_delay),
            vibrato_rise: setting(self.vibrato_rise, 0.01, 4.0, default.vibrato_rise),
            legato_time: setting(self.legato_time, 0.0, 1.0, default.legato_time),
            vowel_time: setting(self.vowel_time, 0.005, 1.0, default.vowel_time),
            formant_shift: setting(self.formant_shift, 0.7, 1.4, 1.0),
            brightness: setting(self.brightness, 500.0, 12_000.0, default.brightness),
            velocity_range_db: setting(
                self.velocity_range_db,
                0.0,
                60.0,
                default.velocity_range_db,
            ),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    fn vibrato(&self) -> VibratoShape {
        VibratoShape {
            rate: self.vibrato_rate,
            depth_cents: self.vibrato_depth,
            delay: self.vibrato_delay,
            rise: self.vibrato_rise,
            wander: 0.6,
            develop: 0.0,
        }
    }
}

impl Default for ChoirParams {
    fn default() -> Self {
        Self::aah_choir()
    }
}

/// The formants of a vowel at `position` (0 to 4, between vowels a blend)
/// for a voice `woman` of the way from a man's to a woman's, the vocal
/// tract scaled by `shift`, singing `f0` hertz.
fn formants(position: f64, woman: f64, shift: f64, f0: f64) -> [(f64, f64); FORMANTS] {
    let position = position.clamp(0.0, 4.0);
    let low = (position.floor() as usize).min(4);
    let high = (low + 1).min(4);
    let t = position - low as f64;
    let mut out = [(0.0, 0.0); FORMANTS];
    for (n, slot) in out.iter_mut().enumerate() {
        let blend = |table: &[[(f64, f64); FORMANTS]; 5]| {
            let (a, b) = (table[low][n], table[high][n]);
            (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
        };
        let (man, wife) = (blend(&MEN), blend(&WOMEN));
        let frequency = (man.0 + (wife.0 - man.0) * woman) * shift;
        let bandwidth = man.1 + (wife.1 - man.1) * woman;
        *slot = (frequency, bandwidth);
    }
    out[0].0 = out[0].0.max((F1_ABOVE_PITCH * f0).min(F1_HIGHEST));
    // The formants keep their order, a little apart.
    for n in 1..FORMANTS {
        out[n].0 = out[n].0.max(out[n - 1].0 * 1.05);
    }
    out
}

/// How far a key is from a man's voice towards a woman's.
fn womanhood(key: f64) -> f64 {
    let t = ((key - MEN_UNTIL) / (WOMEN_FROM - MEN_UNTIL)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A two-pole resonance with unity gain at DC: `y = A x + B y1 + C y2`,
/// D. H. Klatt's resonator, a formant of the cascade.
#[derive(Debug, Clone, Copy, Default)]
struct Formant {
    a: f64,
    b: f64,
    c: f64,
    y1: f64,
    y2: f64,
}

impl Formant {
    fn tune(&mut self, frequency: f64, bandwidth: f64, rate: f64) {
        // A formant above what the rate can hold is left out: its
        // resonance would only be heard above the band, where it is all
        // but flat.
        if frequency > 0.45 * rate {
            *self = Self {
                a: 1.0,
                ..Self::default()
            };
            return;
        }
        let r = (-PI * bandwidth / rate).exp();
        self.c = -r * r;
        self.b = 2.0 * r * (TAU * frequency / rate).cos();
        self.a = 1.0 - self.b - self.c;
    }

    #[inline]
    fn next(&mut self, x: f64) -> f64 {
        let y = flush(self.a * x + self.b * self.y1 + self.c * self.y2);
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// The magnitude of the response at `frequency`, for the tests.
    #[cfg(test)]
    fn gain(&self, frequency: f64, rate: f64) -> f64 {
        let w = TAU * frequency / rate;
        let (re, im) = (
            1.0 - self.b * w.cos() - self.c * (2.0 * w).cos(),
            self.b * w.sin() + self.c * (2.0 * w).sin(),
        );
        self.a / re.hypot(im)
    }

    fn rest(&mut self) {
        self.y1 = 0.0;
        self.y2 = 0.0;
    }
}

/// One singer: a glottis and a vocal tract.
#[derive(Debug, Clone)]
struct Singer {
    /// The glottis's phase in the period, 0 to 1.
    phase: f64,
    /// The tilt's memory: a one-pole low-pass on the glottal flow.
    tilt: f64,
    tract: [Formant; FORMANTS],
    vibrato: Vibrato,
    /// This singer's tuning (cents), vocal tract (a factor on the
    /// formants), level, and how many samples they wait to start.
    detune: f64,
    tract_scale: f64,
    loudness: f64,
    wait: u32,
    /// The singer's own wobble: noise, followed slowly, for pitch and
    /// level.
    noise: Noise,
    wobble: f64,
    wobble_target: f64,
    shimmer: f64,
    countdown: u32,
}

impl Singer {
    fn new() -> Self {
        Self {
            phase: 0.0,
            tilt: 0.0,
            tract: [Formant::default(); FORMANTS],
            vibrato: Vibrato::new(),
            detune: 0.0,
            tract_scale: 1.0,
            loudness: 1.0,
            wait: 0,
            noise: Noise::for_note(0, 0.0, 0),
            wobble: 0.0,
            wobble_target: 0.0,
            shimmer: 0.0,
            countdown: 0,
        }
    }

    fn silence(&mut self) {
        self.phase = 0.0;
        self.tilt = 0.0;
        for formant in &mut self.tract {
            formant.rest();
        }
        self.wait = 0;
        self.wobble = 0.0;
        self.wobble_target = 0.0;
        self.shimmer = 0.0;
        self.countdown = 0;
    }
}

/// The correction a band-limited step needs `t` samples after it (`t` in
/// 0 to 1) or before it (`t` in -1 to 0): the two-sample polynomial band
/// limited step (V. Välimäki and A. Huovilainen, "Antialiasing oscillators
/// in subtractive synthesis", IEEE Signal Processing Magazine, 2007), for a
/// step of one.
#[inline]
fn blep(t: f64) -> f64 {
    if t >= 0.0 {
        -0.5 * (1.0 - t) * (1.0 - t)
    } else {
        0.5 * (1.0 + t) * (1.0 + t)
    }
}

/// One note: the singers and what they share.
#[derive(Debug, Clone)]
struct ChoirTone {
    rate: f64,
    singers: [Singer; MAX_SINGERS],
    count: usize,
    glide: Glide,
    bend: f64,
    /// The vowel, gliding where it is sent, and the voice's register.
    vowel: Glide,
    woman: f64,
    formant_shift: f64,
    retune_left: u32,
    /// The glottis: its open quotient, the tilt's pole at full effort and
    /// the effort's, the breath noise.
    open_quotient: f64,
    effort: f64,
    brightness: f64,
    tilt_pole: f64,
    breathiness: f64,
    breath: Noise,
    jitter: f64,
    /// The level: the note's envelope, rising and falling, and the
    /// dynamics.
    envelope: f64,
    rise: f64,
    fall: f64,
    held: bool,
    dynamics: Dynamics,
    gain: f64,
    level: f64,
    /// How the singers' wobble moves: samples between new targets and how
    /// fast it follows.
    wobble_period: u32,
    wobble_follow: f64,
    follower: f64,
    follower_fall: f64,
    sounding: bool,
}

impl ChoirTone {
    fn new() -> Self {
        Self {
            rate: 48_000.0,
            singers: core::array::from_fn(|_| Singer::new()),
            count: 1,
            glide: Glide::new(),
            bend: 0.0,
            vowel: Glide::new(),
            woman: 0.5,
            formant_shift: 1.0,
            retune_left: 0,
            open_quotient: 0.6,
            effort: 1.0,
            brightness: 4_000.0,
            tilt_pole: 0.0,
            breathiness: 0.0,
            breath: Noise::for_note(0, 0.0, 0),
            jitter: 0.0,
            envelope: 0.0,
            rise: 1.0,
            fall: 1.0,
            held: false,
            dynamics: Dynamics::new(),
            gain: 1.0,
            level: 0.0,
            wobble_period: 1,
            wobble_follow: 0.0,
            follower: 0.0,
            follower_fall: 0.0,
            sounding: false,
        }
    }

    fn prepare(&mut self, rate: f64) {
        self.rate = rate;
        self.dynamics.prepare(rate, VOICE_SECONDS);
        self.wobble_period = ((0.08 * rate) as u32).max(1);
        self.wobble_follow = 1.0 - (-1.0 / (0.06 * rate)).exp();
        self.follower_fall = t60_factor(0.3, rate);
        self.silence();
    }

    fn configure(&mut self, params: &ChoirParams) {
        self.open_quotient = (params.open_quotient + SOFT_OPENING * (1.0 - self.effort)).min(0.9);
        self.breathiness = params.breathiness;
        self.jitter = params.jitter;
        self.formant_shift = params.formant_shift;
        self.level = params.level;
        self.rise = 1.0 - (-3.0 / (params.attack * self.rate)).exp();
        self.fall = 1.0 - (-3.0 / (params.release * self.rate)).exp();
        self.vowel.to(params.vowel, params.vowel_time, self.rate);
        self.retune_left = 0;
    }

    /// Tunes every singer's vocal tract to the vowel where it is now, for
    /// the note at `key`.
    fn retune(&mut self, key: f64) {
        let rate = self.rate;
        let f0 = key_hz(key);
        let vowel = self.vowel.key;
        for singer in self.singers.iter_mut().take(self.count) {
            let shift = self.formant_shift * singer.tract_scale;
            for (formant, (frequency, bandwidth)) in singer
                .tract
                .iter_mut()
                .zip(formants(vowel, self.woman, shift, f0))
            {
                formant.tune(frequency, bandwidth, rate);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn start(
        &mut self,
        params: &ChoirParams,
        key: f64,
        level: f64,
        effort: f64,
        accent: bool,
        seed: u64,
    ) {
        let rate = self.rate;
        let fresh = !self.sounding;
        self.glide.set(key);
        self.woman = womanhood(key);
        self.count = params.singers;
        self.effort = effort;
        self.open_quotient = (params.open_quotient + SOFT_OPENING * (1.0 - effort)).min(0.9);
        let full = params.brightness.min(0.45 * rate);
        self.brightness = full * (SOFT_OCTAVES * (effort - 1.0)).exp2();
        self.tilt_pole = (-TAU * self.brightness / rate).exp();
        // The tilt's loss at the first formants, made up.
        let loss = |cutoff: f64| (1.0 + (TILT_REFERENCE / cutoff).powi(2)).sqrt();
        let makeup = loss(self.brightness) / loss(full);
        self.dynamics.strike(level, fresh);
        self.gain = makeup * if accent { db(params.accent_db) } else { 1.0 };
        self.held = true;
        if fresh {
            self.vowel.set(params.vowel);
            self.breath = Noise::for_note(key as u8, 0.0, seed);
            let count = self.count;
            for (n, singer) in self.singers.iter_mut().take(count).enumerate() {
                let mut noise = Noise::for_note(key as u8, 0.0, seed + 1 + n as u64);
                let place = if count > 1 {
                    n as f64 / (count - 1) as f64 - 0.5
                } else {
                    0.0
                };
                singer.detune = params.spread * (place + 0.15 * noise.next());
                singer.tract_scale = 1.0 + 0.04 * noise.next() * f64::from(u8::from(count > 1));
                singer.loudness = 1.0 + 0.15 * noise.next() * f64::from(u8::from(count > 1));
                singer.wait = if n == 0 {
                    0
                } else {
                    ((0.005 + 0.035 * noise.unit()) * rate) as u32
                };
                singer.phase = noise.unit();
                singer.vibrato.start(&params.vibrato(), rate, noise);
                singer.noise = noise;
            }
        }
        self.retune(key);
        self.sounding = true;
    }

    fn slur(&mut self, params: &ChoirParams, key: f64, level: f64) {
        self.glide.to(key, params.legato_time, self.rate);
        self.dynamics.strike(level, false);
        self.held = true;
        self.retune_left = 0;
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let rate = self.rate;
        let key = self.glide.next() + self.bend;
        // The vowel and the pitch move the formants; they are retuned every
        // few samples while either moves.
        let vowel_moving = self.vowel.moving();
        self.vowel.next();
        if vowel_moving || self.glide.moving() {
            if self.retune_left == 0 {
                self.retune_left = RETUNE_HOP;
                self.retune(key);
            }
            self.retune_left -= 1;
        }

        let dynamics = self.dynamics.next();
        let target = if self.held { 1.0 } else { 0.0 };
        let follow = if self.held { self.rise } else { self.fall };
        self.envelope = flush(self.envelope + (target - self.envelope) * follow);

        let open = self.open_quotient;
        let breath_level = self.breathiness;
        let jitter = self.jitter;
        let mut out = 0.0;
        for singer in self.singers.iter_mut().take(self.count) {
            if singer.wait > 0 {
                singer.wait -= 1;
                continue;
            }
            // The singer's own wobble: a new random target every 80 ms,
            // followed smoothly.
            if singer.countdown == 0 {
                singer.countdown = self.wobble_period;
                singer.wobble_target = singer.noise.next();
            }
            singer.countdown -= 1;
            singer.wobble += (singer.wobble_target - singer.wobble) * self.wobble_follow;
            singer.shimmer += (singer.noise.next() - singer.shimmer) * 0.01;
            let vibrato = singer.vibrato.next(self.dynamics.vibrato);
            let cents = singer.detune + vibrato + 12.0 * jitter * singer.wobble;
            let step = (key_hz(key + 0.01 * cents) / rate).min(0.45);

            // The glottis: the derivative of the flow of D. H. Klatt and L.
            // C. Klatt's model ("Analysis, synthesis, and perception of
            // voice quality variations among female and male talkers", JASA
            // 87, 1990), 2τ - 3τ² over the open phase, which ends with a
            // step back to zero as the folds close; that step is band
            // limited.
            let phase = singer.phase;
            let mut source = if phase < open {
                let tau = phase / open;
                2.0 * tau - 3.0 * tau * tau
            } else {
                0.0
            };
            let distance = (phase - open) / step;
            if distance > -1.0 && distance < 1.0 {
                source += blep(distance);
            }
            // Breath through the glottis, while it is open.
            if phase < open && breath_level > 0.0 {
                source += breath_level * 0.6 * self.breath.next();
            }
            singer.phase += step;
            if singer.phase >= 1.0 {
                singer.phase -= 1.0;
            }
            // The tilt: a softer voice is darker.
            singer.tilt = flush(source + self.tilt_pole * (singer.tilt - source));
            let mut voice = singer.tilt;
            for formant in &mut singer.tract {
                voice = formant.next(voice);
            }
            out += voice * singer.loudness * (1.0 + 0.3 * jitter * singer.shimmer);
        }

        let singers = self.count as f64;
        let out = out / singers.sqrt()
            * self.envelope
            * dynamics
            * self.gain
            * self.level
            * self.dynamics.volume;
        self.follower = (self.follower * self.follower_fall).max(out.abs());
        if !self.held && self.envelope < 1e-4 && self.follower < SILENCE {
            self.silence();
        }
        out
    }
}

impl Tone for ChoirTone {
    fn is_sounding(&self) -> bool {
        self.sounding
    }

    fn silence(&mut self) {
        for singer in &mut self.singers {
            singer.silence();
        }
        self.envelope = 0.0;
        self.held = false;
        self.follower = 0.0;
        self.retune_left = 0;
        self.sounding = false;
    }
}

/// A sung voice, or a choir, by formant synthesis: a voice for
/// [`Synth`](crate::Synth) that sings vowels.
///
/// Each singer is a source and a filter, as G. Fant described speech
/// ("Acoustic Theory of Speech Production", 1960): the glottis puts out
/// the derivative of its air flow, a pulse a period, and the vocal tract
/// shapes it with its resonances, the formants, which make the vowel.
///
/// ```text
///  glottis (Klatt's flow, band limited closure) + breath
///       -> tilt (effort) -> F1 -> F2 -> F3 -> F4 -> F5 -> singer
///  singers (each detuned, with its own vibrato, wobble and vocal tract) -> choir
/// ```
///
/// - **The vowels** a, e, i, o and u ([`Vowel`]) are given by their first
///   five formants, from Peterson and Barney's measurements of men's and
///   women's voices; [`ChoirParams::vowel`] blends between neighbours, and
///   moves smoothly when changed ([`ChoirParams::vowel_time`]).
/// - **Across the keyboard** the voices go from men's below G3 to women's
///   above G4, basses to sopranos, and a high note raises its first
///   formant to just above its pitch, as sopranos do.
/// - **A choir** ([`ChoirParams::singers`]) is several singers to a note,
///   tuned a little apart ([`ChoirParams::spread`]), starting a few
///   milliseconds apart, each with a vocal tract a few percent longer or
///   shorter, a vibrato of their own and a random wobble in pitch and level
///   ([`ChoirParams::jitter`]).
/// - **Effort**: velocity sets the level and the brightness of the source
///   (a soft voice is darker), pressure on the key, the breath controller
///   and the expression pedal the level as the note sounds;
///   [`Expression::Brightness`] moves the vowel from u (0) through o, a and
///   e to i (1), the order of their brightness.
/// - **Legato**: in a [`Synth`](crate::Synth) set to legato, a note struck
///   while another is held slurs to it ([`ChoirParams::legato_time`]).
///
/// Every source of noise is seeded by the note.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{ChoirParams, ChoirVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut choir = Synth::with_voices(8, || ChoirVoice::new(ChoirParams::aah_choir()));
/// choir.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [48, 55, 64, 67] {
///     choir.note_on(NoteRef::new(0, key), 0.8);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// for _ in 0..20 {
///     choir.process(&mut block);
/// }
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct ChoirVoice {
    params: ChoirParams,
    tones: Handover<ChoirTone>,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
    key: u8,
}

impl ChoirVoice {
    /// A voice with these settings. Until it is prepared it assumes 48 kHz.
    pub fn new(params: ChoirParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            tones: Handover::new(ChoirTone::new(), ChoirTone::new()),
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
            key: 60,
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note glides to the new vowel and takes the rest at once,
    /// but for the number of singers and their tuning, which it takes at its
    /// next note.
    pub fn set_params(&mut self, params: ChoirParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &ChoirParams {
        &self.params
    }

    fn configure(&mut self) {
        let mut params = self.params;
        if let Some(brightness) = self.tones.now.dynamics.brightness {
            params.vowel = vowel_for_brightness(brightness);
        }
        self.tones.now.configure(&params);
        self.tones.old.configure(&params);
    }

    fn level(&self, velocity: f32, accent: bool) -> f64 {
        let _ = accent;
        velocity_gain(f64::from(velocity), self.params.velocity_range_db)
    }
}

/// The vowel for a brightness from 0 to 1: u, o, a, e, i.
fn vowel_for_brightness(brightness: f64) -> f64 {
    const PATH: [f64; 5] = [4.0, 3.0, 0.0, 1.0, 2.0];
    let x = brightness.clamp(0.0, 1.0) * 4.0;
    let low = (x.floor() as usize).min(3);
    let t = x - low as f64;
    PATH[low] + (PATH[low + 1] - PATH[low]) * t
}

impl<S: Sample> Voice<S> for ChoirVoice {
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
        let level = self.level(velocity, accent);
        let effort = f64::from(velocity).clamp(0.0, 1.0);
        self.tones.now.bend = self.bend + self.tuning;
        self.tones.now.start(
            &params,
            self.key_tuning.key_pitch(self.key),
            level,
            effort,
            accent,
            u64::from(self.key) * 8,
        );
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.tones.now.sounding {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        let params = self.params;
        self.key = note.key.min(127);
        let level = self.level(velocity, accent);
        self.tones
            .now
            .slur(&params, self.key_tuning.key_pitch(self.key), level);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.tones.now.held = false;
    }

    fn choke(&mut self) {
        self.tones.choke();
        self.tones.now.dynamics.reset();
        self.tuning = 0.0;
        self.configure();
    }

    fn is_active(&self) -> bool {
        self.tones.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        if !self.tones.is_active() {
            return;
        }
        let tones = &mut self.tones;
        add_to_every_channel(buffer, || tones.next(ChoirTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            self.tones.now.bend = self.bend + self.tuning;
            self.tones.now.retune_left = 0;
        } else if self.tones.now.dynamics.expression(expression, value)
            && expression == Expression::Brightness
        {
            let vowel = vowel_for_brightness(value);
            let time = self.params.vowel_time;
            let rate = self.tones.now.rate;
            self.tones.now.vowel.to(vowel, time, rate);
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            self.tones.now.bend = self.bend + self.tuning;
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
        self, Act, RATE, accented, amplitude_at, between, cents, decibels, frequency_near,
        largest_jump, note, off, on, play, rms, synth_of,
    };
    use crate::instruments::modelling::probe::{harmonic_centroid, modulation, pitch_track};

    fn presets() -> [(&'static str, ChoirParams); 3] {
        [
            ("aah choir", ChoirParams::aah_choir()),
            ("ooh choir", ChoirParams::ooh_choir()),
            ("solo voice", ChoirParams::solo_voice()),
        ]
    }

    fn voice(params: ChoirParams) -> [ChoirVoice; 1] {
        [ChoirVoice::new(params)]
    }

    /// One singer, steady: no vibrato, no wobble, no breath.
    fn steady(params: ChoirParams) -> ChoirParams {
        ChoirParams {
            singers: 1,
            spread: 0.0,
            jitter: 0.0,
            breathiness: 0.0,
            vibrato_depth: 0.0,
            ..params
        }
    }

    /// The local maxima of the cascade's response, on a fine grid.
    fn peaks(tract: &[Formant; FORMANTS]) -> Vec<f64> {
        let response = |f: f64| {
            tract
                .iter()
                .map(|formant| formant.gain(f, RATE))
                .product::<f64>()
        };
        let grid: Vec<f64> = (1..1_200).map(|n| 5.0 * f64::from(n)).collect();
        let values: Vec<f64> = grid.iter().map(|&f| response(f)).collect();
        (1..values.len() - 1)
            .filter(|&n| values[n] > values[n - 1] && values[n] >= values[n + 1])
            .map(|n| grid[n])
            .collect()
    }

    #[test]
    fn the_formants_peak_at_the_vowels_targets() {
        for (woman, table, name) in [(0.0, &MEN, "men"), (1.0, &WOMEN, "women")] {
            for (vowel, row) in table.iter().enumerate() {
                let mut tract = [Formant::default(); FORMANTS];
                let targets = formants(vowel as f64, woman, 1.0, 100.0);
                for (formant, (frequency, bandwidth)) in tract.iter_mut().zip(targets) {
                    formant.tune(frequency, bandwidth, RATE);
                }
                let found = peaks(&tract);
                // The first three formants, the vowel's own, each have a
                // peak within 5%.
                for (n, &(target, _)) in row.iter().take(3).enumerate() {
                    let nearest = found.iter().copied().fold(f64::MAX, |best, f| {
                        if (f - target).abs() < (best - target).abs() {
                            f
                        } else {
                            best
                        }
                    });
                    let error = nearest / target - 1.0;
                    assert!(
                        error.abs() < 0.05,
                        "{name} vowel {vowel} F{}: peak {nearest} Hz against {target} Hz",
                        n + 1
                    );
                }
            }
        }
    }

    #[test]
    fn a_sung_vowel_shows_its_formants() {
        // A low voice, whose harmonics sample the vocal tract finely: the
        // loudest harmonics near F1 and F2 lie at the vowel's formants.
        for (vowel, f1, f2) in [
            (Vowel::A, 730.0, 1_090.0),
            (Vowel::I, 270.0, 2_290.0),
            (Vowel::U, 300.0, 870.0),
        ] {
            let params = ChoirParams {
                vowel: vowel.position(),
                ..steady(ChoirParams::solo_voice())
            };
            let key = 33; // 55 Hz, a bass
            let f0 = key_hz(f64::from(key));
            let signal = note(voice(params), on(key, 1.0), 2.0, 1.0);
            let part = between(&signal, 0.4, 1.0);
            let envelope_peak = |low: f64, high: f64| {
                let first = (low / f0).ceil() as usize;
                let last = (high / f0).floor() as usize;
                let levels: Vec<(f64, f64)> = (first..=last)
                    .map(|n| (n as f64 * f0, decibels(amplitude_at(part, n as f64 * f0))))
                    .collect();
                let (index, _) =
                    levels
                        .iter()
                        .enumerate()
                        .fold(
                            (0, f64::MIN),
                            |best, (i, &(_, l))| if l > best.1 { (i, l) } else { best },
                        );
                // A parabola through the loudest and its neighbours.
                if index > 0 && index + 1 < levels.len() {
                    let (a, b, c) = (levels[index - 1].1, levels[index].1, levels[index + 1].1);
                    let shift = 0.5 * (a - c) / (a - 2.0 * b + c);
                    levels[index].0 + shift * f0
                } else {
                    levels[index].0
                }
            };
            let found1 = envelope_peak(0.6 * f1, 1.4 * f1);
            let found2 = envelope_peak(0.8 * f2, 1.25 * f2);
            println!("{vowel:?}: F1 {found1:.0} Hz ({f1}), F2 {found2:.0} Hz ({f2})");
            assert!((found1 / f1 - 1.0).abs() < 0.08, "{vowel:?} F1 {found1}");
            assert!((found2 / f2 - 1.0).abs() < 0.08, "{vowel:?} F2 {found2}");
        }
    }

    #[test]
    fn every_note_is_in_tune_across_the_choir() {
        let mut worst = 0.0f64;
        for (name, params) in presets() {
            for key in (36..=84).step_by(4) {
                let f0 = key_hz(f64::from(key));
                let signal = note(voice(steady(params)), on(key, 0.8), 2.0, 1.0);
                let error = cents(frequency_near(between(&signal, 0.4, 1.0), f0), f0);
                worst = worst.max(error.abs());
                assert!(error.abs() < 0.5, "{name} key {key}: {error:.3} cents");
            }
        }
        println!("choir: worst tuning {worst:.4} cents");
    }

    #[test]
    fn the_vibrato_has_its_rate_and_depth() {
        let params = ChoirParams {
            jitter: 0.0,
            breathiness: 0.0,
            ..ChoirParams::solo_voice()
        };
        let key = 64;
        let f0 = key_hz(f64::from(key));
        let signal = note(voice(params), on(key, 0.8), 3.0, 3.0);
        let hop = 0.005;
        let track = pitch_track(&signal, f0, 1.0, 2.9, 0.03, hop);
        let (rate, depth) = modulation(&track, f0, hop);
        let early = pitch_track(&signal, f0, 0.1, 0.28, 0.03, hop);
        let (_, before) = modulation(&early, f0, hop);
        println!("voice vibrato: {before:.1} cents before, then {rate:.2} Hz and {depth:.1} cents");
        assert!((rate / params.vibrato_rate - 1.0).abs() < 0.1, "{rate}");
        assert!((depth / params.vibrato_depth - 1.0).abs() < 0.2, "{depth}");
        assert!(before < 0.25 * depth);
    }

    #[test]
    fn a_choir_is_several_singers_apart() {
        let key = 60;
        let f0 = key_hz(f64::from(key));
        let sway = |params: ChoirParams| {
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
        let choir = ChoirParams {
            vibrato_depth: 0.0,
            breathiness: 0.0,
            ..ChoirParams::aah_choir()
        };
        let one = ChoirParams {
            singers: 1,
            jitter: 0.0,
            ..choir
        };
        let (many, single) = (sway(choir), sway(one));
        println!("choir sways {many:.2} dB, one singer {single:.2} dB");
        assert!(many > 1.0 && many > 3.0 * single, "{many} against {single}");
    }

    #[test]
    fn velocity_makes_a_voice_louder_and_brighter() {
        for (name, params) in presets() {
            let key = 60;
            let f0 = key_hz(f64::from(key));
            let loud = note(voice(steady(params)), on(key, 1.0), 2.0, 1.0);
            let soft = note(voice(steady(params)), on(key, 0.4), 2.0, 1.0);
            let (loud, soft) = (between(&loud, 0.5, 1.0), between(&soft, 0.5, 1.0));
            let difference = decibels(rms(loud) / rms(soft));
            let expected = 0.6 * params.velocity_range_db;
            let (bright, dark) = (harmonic_centroid(loud, f0), harmonic_centroid(soft, f0));
            println!("{name}: {difference:.1} dB, centroid {bright:.0} Hz against {dark:.0} Hz");
            assert!((difference - expected).abs() < 2.0, "{name}: {difference}");
            assert!(bright > 1.05 * dark, "{name}: {bright} against {dark}");
        }
    }

    #[test]
    fn an_accent_is_louder() {
        for (name, params) in presets() {
            let plain = note(voice(steady(params)), on(60, 0.7), 2.0, 1.0);
            let strong = note(voice(steady(params)), accented(60, 0.7), 2.0, 1.0);
            let difference =
                decibels(rms(between(&strong, 0.5, 1.0)) / rms(between(&plain, 0.5, 1.0)));
            assert!(
                (difference - params.accent_db).abs() < 0.5,
                "{name}: {difference}"
            );
        }
    }

    #[test]
    fn the_vowel_moves_with_the_brightness_expression() {
        let params = steady(ChoirParams::solo_voice());
        let key = 45;
        let f0 = key_hz(f64::from(key));
        let mut synth = synth_of(voice(params));
        synth.note_on(NoteRef::new(0, key), 0.8);
        let a = play(&mut synth, &[], (0.6 * RATE) as usize, 256);
        for voice in synth.voices_mut() {
            Voice::<f64>::expression(voice, Expression::Brightness, 1.0);
        }
        let i = play(&mut synth, &[], (0.6 * RATE) as usize, 256);
        // "a" has its second formant near 1.1 kHz, "i" near 2.3 kHz.
        let band = |signal: &[f64], low: f64, high: f64| {
            let part = between(signal, 0.3, 0.6);
            ((low / f0).ceil() as usize..=(high / f0).floor() as usize)
                .map(|n| amplitude_at(part, n as f64 * f0).powi(2))
                .sum::<f64>()
        };
        let a_ratio = decibels((band(&a, 2_000.0, 2_600.0) / band(&a, 900.0, 1_300.0)).sqrt());
        let i_ratio = decibels((band(&i, 2_000.0, 2_600.0) / band(&i, 900.0, 1_300.0)).sqrt());
        println!("2.3 kHz against 1.1 kHz: {a_ratio:.1} dB as a, {i_ratio:.1} dB as i");
        assert!(i_ratio > a_ratio + 15.0);
    }

    #[test]
    fn legato_slurs_and_releases_fade() {
        for (name, params) in presets() {
            let params = steady(params);
            let mut synth = synth_of(voice(params));
            synth.set_legato(true);
            let at = (1.0 * RATE) as usize;
            let signal = play(
                &mut synth,
                &[(0, on(60, 0.8)), (at, on(62, 0.8)), (at + 10, off(60))],
                (2.0 * RATE) as usize,
                256,
            );
            // The slur does not dip below the quieter of the two notes.
            let before = rms(between(&signal, 0.8, 1.0));
            let after = rms(between(&signal, 1.5, 2.0));
            let through =
                crate::instruments::modelling::probe::levels(between(&signal, 1.0, 1.2), 0.02);
            let lowest = through.iter().fold(f64::MAX, |m, &x| m.min(x));
            let dip = decibels(lowest / before.min(after));
            let f1 = key_hz(62.0);
            let error = cents(frequency_near(between(&signal, 1.5, 2.0), f1), f1);
            assert!(dip > -3.0, "{name}: {dip}");
            assert!(error.abs() < 0.5, "{name}: {error}");
            assert_eq!(synth.active_voices(), 1);
            // A release fades without a click and frees the voice.
            let released = note(voice(params), on(60, 0.8), 0.8, 1.5 + params.release * 3.0);
            let steady_jump = largest_jump(between(&released, 0.5, 0.8));
            assert!(
                largest_jump(between(&released, 0.8, 1.5)) <= 1.05 * steady_jump,
                "{name}"
            );
            let mut synth = synth_of(voice(params));
            play(
                &mut synth,
                &[(0, on(60, 0.8)), (4_800, off(60))],
                4 * 48_000,
                256,
            );
            assert_eq!(synth.active_voices(), 0, "{name}");
        }
    }

    #[test]
    fn notes_start_and_are_taken_over_without_a_click() {
        for (name, params) in presets() {
            let alone = note(voice(params), on(64, 1.0), 2.0, 0.8);
            let steady_jump = largest_jump(between(&alone, 0.4, 0.8));
            assert!(alone[0].abs() < 1e-3 * measure::peak(&alone), "{name}");
            let steal = (0.5 * RATE) as usize;
            let mut synth = synth_of(voice(params));
            let both = play(
                &mut synth,
                &[(0, on(64, 1.0)), (steal, on(59, 1.0))],
                steal * 2,
                256,
            );
            let mut fresh = synth_of(voice(params));
            let second = play(&mut fresh, &[(steal, on(59, 1.0))], steal * 2, 256);
            let residual: Vec<f64> = both.iter().zip(&second).map(|(a, b)| a - b).collect();
            let at_steal = largest_jump(&residual[steal - 64..steal + 512]);
            assert!(
                at_steal <= 1.05 * steady_jump,
                "{name}: {at_steal} against {steady_jump}"
            );
            assert!(measure::peak(&residual[steal + 400..]) < 1e-9, "{name}");
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(48, 0.9)),
            (1_000, accented(55, 0.7)),
            (7_000, on(64, 0.5)),
            (9_001, off(48)),
            (12_000, on(48, 1.0)),
            (15_000, Act::Bend(1.0)),
            (20_000, off(55)),
        ];
        for (name, params) in presets() {
            let run = |block| {
                play(
                    &mut synth_of([ChoirVoice::new(params), ChoirVoice::new(params)]),
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
            ChoirParams {
                vowel: f64::NAN,
                singers: 1_000,
                spread: f64::INFINITY,
                jitter: 1e9,
                breathiness: f64::NAN,
                open_quotient: 0.0,
                attack: 0.0,
                release: f64::NAN,
                vibrato_rate: 1e9,
                vibrato_depth: f64::INFINITY,
                vibrato_delay: -1.0,
                vibrato_rise: 0.0,
                legato_time: f64::NAN,
                vowel_time: 0.0,
                formant_shift: 1e9,
                brightness: f64::NAN,
                velocity_range_db: -5.0,
                accent_db: 1e9,
                level: f64::NAN,
            },
            ChoirParams {
                breathiness: 1.0,
                jitter: 1.0,
                accent_db: 24.0,
                level: 4.0,
                vibrato_depth: 200.0,
                formant_shift: 1.4,
                brightness: 12_000.0,
                ..ChoirParams::aah_choir()
            },
        ];
        for params in extreme {
            let mut synth = synth_of([ChoirVoice::new(params), ChoirVoice::new(params)]);
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
