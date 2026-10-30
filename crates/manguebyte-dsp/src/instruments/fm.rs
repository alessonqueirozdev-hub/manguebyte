//! Frequency modulation synthesis: four sine operators.

use core::f64::consts::TAU;

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    Handover, SILENCE, Tone, add_to_every_channel, db, key_hz, setting, t60_factor,
};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{ProcessSpec, Voice};

/// How many operators an [`FmVoice`] has.
pub const FM_OPERATORS: usize = 4;

/// How many decibels a velocity sensitivity of 1 spans, from velocity 1 to
/// velocity 0.
pub const FM_VELOCITY_RANGE_DB: f64 = 30.0;

/// The highest frequency, as a fraction of the sample rate, that an
/// operator's significant sidebands may reach before the modulation is
/// turned down. See [`FmVoice`].
const REACH_LIMIT: f64 = 0.45;

/// The largest phase feedback, in radians, at a feedback of 1: enough to
/// turn a sine into something close to a sawtooth.
const MAX_FEEDBACK: f64 = 1.6;

/// How the four operators of an [`FmVoice`] are connected.
///
/// Operators are numbered from 1, the carrier at the bottom, as on the
/// classic FM synthesisers; a modulator always has a higher number than
/// what it modulates. `a -> b` means operator `a` modulates the phase of
/// operator `b`, and the carriers are the operators that are heard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Algorithm {
    /// `4 -> 3 -> 2 -> 1`: one carrier under a stack of three modulators,
    /// the richest.
    Stack,
    /// `2 -> 1` and `4 -> 3`: two pairs, both heard, as the tine and the
    /// tone bar of an electric piano.
    TwoPairs,
    /// `2 -> 1`, `3 -> 1` and `4 -> 1`: three modulators on one carrier.
    ThreeToOne,
    /// `4 -> 3 -> 2` heard beside operator 1 alone.
    StackAndSine,
    /// `4 -> 1`, `4 -> 2` and `4 -> 3`: one modulator on three carriers.
    OneToThree,
    /// The four operators heard side by side, as an additive organ.
    Additive,
}

impl Algorithm {
    /// Every algorithm.
    pub const ALL: [Self; 6] = [
        Self::Stack,
        Self::TwoPairs,
        Self::ThreeToOne,
        Self::StackAndSine,
        Self::OneToThree,
        Self::Additive,
    ];

    /// Which operators modulate which, `[target][source]`, counted from 0,
    /// and which are heard.
    fn routing(self) -> ([[bool; FM_OPERATORS]; FM_OPERATORS], [bool; FM_OPERATORS]) {
        let mut modulates = [[false; FM_OPERATORS]; FM_OPERATORS];
        let mut connect = |source: usize, target: usize| modulates[target - 1][source - 1] = true;
        let carriers = match self {
            Self::Stack => {
                connect(4, 3);
                connect(3, 2);
                connect(2, 1);
                [true, false, false, false]
            }
            Self::TwoPairs => {
                connect(2, 1);
                connect(4, 3);
                [true, false, true, false]
            }
            Self::ThreeToOne => {
                connect(2, 1);
                connect(3, 1);
                connect(4, 1);
                [true, false, false, false]
            }
            Self::StackAndSine => {
                connect(4, 3);
                connect(3, 2);
                [true, true, false, false]
            }
            Self::OneToThree => {
                connect(4, 1);
                connect(4, 2);
                connect(4, 3);
                [true, true, true, false]
            }
            Self::Additive => [true; FM_OPERATORS],
        };
        (modulates, carriers)
    }
}

/// One operator of an [`FmVoice`]: a sine with its own envelope.
///
/// What its level means depends on where the algorithm puts it: a carrier's
/// is its amplitude, a modulator's the peak phase deviation it gives what it
/// modulates, in radians (the modulation index). Ranges are given for each
/// field; values outside them are limited, and a value that is not a number
/// is taken as 0, or 1 for the ratio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FmOperator {
    /// The operator's frequency as a multiple of the note's, 0.01 to 32.
    pub ratio: f64,
    /// A detune in cents, -100 to 100.
    pub detune: f64,
    /// 0 to 16: for a carrier its amplitude, 1 being full scale; for a
    /// modulator the modulation index, in radians.
    pub level: f64,
    /// Seconds to rise from silence to the peak, 0 to 10.
    pub attack: f64,
    /// Seconds to fall 60 dB from the peak, on the way to the sustain
    /// level, 0.001 to 100. The fall is even in decibels, as an analogue
    /// envelope's is not but a classic FM synthesiser's is.
    pub decay: f64,
    /// The level held while the key is down, as a fraction of the peak, 0
    /// to 1.
    pub sustain: f64,
    /// Seconds to fall 60 dB once the key is released, 0.001 to 100.
    pub release: f64,
    /// How much the level follows the velocity, 0 to 1: at 1 the lowest
    /// velocity is [`FM_VELOCITY_RANGE_DB`] down. On a modulator this makes
    /// a note played harder brighter.
    pub velocity: f64,
    /// How the level changes up the keyboard, in decibels per octave above
    /// middle C, -24 to 24. The change is held between -48 and +12 dB.
    pub key_level: f64,
    /// How much faster the envelope runs up the keyboard, 0 to 2: its times
    /// are divided by `2^(key_rate × octaves above middle C)`, as a real
    /// instrument's higher notes die away sooner.
    pub key_rate: f64,
    /// How much of its own output the operator feeds back into its phase,
    /// 0 to 1, from a sine at 0 towards a sawtooth.
    pub feedback: f64,
}

impl FmOperator {
    /// An operator at the given ratio and level, with a quick attack, a
    /// slow decay to full sustain and a short release, and no velocity,
    /// scaling or feedback.
    pub const fn new(ratio: f64, level: f64) -> Self {
        Self {
            ratio,
            detune: 0.0,
            level,
            attack: 0.002,
            decay: 1.0,
            sustain: 1.0,
            release: 0.1,
            velocity: 0.0,
            key_level: 0.0,
            key_rate: 0.0,
            feedback: 0.0,
        }
    }

    /// A silent operator.
    pub const OFF: Self = Self::new(1.0, 0.0);

    fn limited(self) -> Self {
        Self {
            ratio: setting(self.ratio, 0.01, 32.0, 1.0),
            detune: setting(self.detune, -100.0, 100.0, 0.0),
            level: setting(self.level, 0.0, 16.0, 0.0),
            attack: setting(self.attack, 0.0, 10.0, 0.0),
            decay: setting(self.decay, 0.001, 100.0, 1.0),
            sustain: setting(self.sustain, 0.0, 1.0, 0.0),
            release: setting(self.release, 0.001, 100.0, 0.1),
            velocity: setting(self.velocity, 0.0, 1.0, 0.0),
            key_level: setting(self.key_level, -24.0, 24.0, 0.0),
            key_rate: setting(self.key_rate, 0.0, 2.0, 0.0),
            feedback: setting(self.feedback, 0.0, 1.0, 0.0),
        }
    }
}

/// The settings of an [`FmVoice`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FmParams {
    /// How the operators are connected.
    pub algorithm: Algorithm,
    /// The operators, numbered 1 to 4 from the first.
    pub operators: [FmOperator; FM_OPERATORS],
    /// How much louder an accented note's carriers are, in decibels, 0 to
    /// 24.
    pub accent_db: f64,
    /// How much an accent multiplies the modulators' indices, 1 to 4: an
    /// accented note is struck harder, so brighter.
    pub accent_index: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl FmParams {
    /// An electric piano in the manner of the classic FM synthesisers:
    /// two pairs, a tone bar (a carrier modulated at its own frequency,
    /// whose index falls as the note sounds) and a tine (a carrier
    /// modulated fourteen times higher, a metallic ping that dies in a
    /// quarter of a second). Velocity raises both indices, so a note
    /// played harder barks, and higher notes are softer and shorter.
    pub fn electric_piano() -> Self {
        Self {
            algorithm: Algorithm::TwoPairs,
            operators: [
                FmOperator {
                    attack: 0.001,
                    decay: 5.0,
                    sustain: 0.0,
                    release: 0.4,
                    velocity: 0.7,
                    key_level: -1.5,
                    key_rate: 0.6,
                    ..FmOperator::new(1.0, 0.55)
                },
                FmOperator {
                    attack: 0.0005,
                    decay: 2.4,
                    sustain: 0.0,
                    release: 0.4,
                    velocity: 0.85,
                    key_level: -3.0,
                    key_rate: 0.6,
                    ..FmOperator::new(1.0, 1.1)
                },
                FmOperator {
                    detune: 4.0,
                    attack: 0.0005,
                    decay: 1.6,
                    sustain: 0.0,
                    release: 0.3,
                    velocity: 0.9,
                    key_level: -1.0,
                    key_rate: 0.6,
                    ..FmOperator::new(1.0, 0.3)
                },
                FmOperator {
                    attack: 0.0003,
                    decay: 0.25,
                    sustain: 0.0,
                    release: 0.2,
                    velocity: 0.9,
                    key_level: -4.0,
                    key_rate: 0.8,
                    ..FmOperator::new(14.0, 1.2)
                },
            ],
            accent_db: 2.5,
            accent_index: 1.5,
            level: 0.33,
        }
    }

    /// A tubular bell: a carrier modulated at 3.5 times its frequency,
    /// whose sidebands fall between the harmonics and make the sound
    /// metallic, and a second pair an octave up and a few cents sharp, which
    /// beats against the first. The indices fall more slowly than the
    /// level, so the bell mellows as it rings for many seconds. The
    /// inharmonic ratio and the falling index are J. M. Chowning's, "The
    /// synthesis of complex audio spectra by means of frequency
    /// modulation", JAES 1973.
    pub fn tubular_bell() -> Self {
        Self {
            algorithm: Algorithm::TwoPairs,
            operators: [
                FmOperator {
                    attack: 0.0005,
                    decay: 9.0,
                    sustain: 0.0,
                    release: 3.0,
                    velocity: 0.6,
                    key_level: -1.0,
                    key_rate: 0.4,
                    ..FmOperator::new(1.0, 0.6)
                },
                FmOperator {
                    attack: 0.0005,
                    decay: 6.0,
                    sustain: 0.0,
                    release: 3.0,
                    velocity: 0.7,
                    key_level: -2.0,
                    key_rate: 0.4,
                    ..FmOperator::new(3.5, 1.8)
                },
                FmOperator {
                    detune: 6.0,
                    attack: 0.0005,
                    decay: 5.0,
                    sustain: 0.0,
                    release: 2.5,
                    velocity: 0.6,
                    key_level: -1.0,
                    key_rate: 0.4,
                    ..FmOperator::new(2.0, 0.25)
                },
                FmOperator {
                    attack: 0.0005,
                    decay: 1.5,
                    sustain: 0.0,
                    release: 1.5,
                    velocity: 0.8,
                    key_level: -3.0,
                    key_rate: 0.5,
                    ..FmOperator::new(1.4, 1.2)
                },
            ],
            accent_db: 3.0,
            accent_index: 1.4,
            level: 0.26,
        }
    }

    /// A punchy FM bass: a carrier with three modulators, the first at the
    /// note's frequency with feedback, which gives a bright, nearly
    /// sawtooth attack that falls to a rounder sustain, and two quicker
    /// ones at two and three times the frequency for the bite of the
    /// attack.
    pub fn fm_bass() -> Self {
        Self {
            algorithm: Algorithm::ThreeToOne,
            operators: [
                FmOperator {
                    attack: 0.001,
                    decay: 3.0,
                    sustain: 0.6,
                    release: 0.08,
                    velocity: 0.5,
                    key_rate: 0.3,
                    ..FmOperator::new(1.0, 0.7)
                },
                FmOperator {
                    attack: 0.001,
                    decay: 0.5,
                    sustain: 0.3,
                    release: 0.08,
                    velocity: 0.7,
                    key_level: -2.0,
                    key_rate: 0.3,
                    feedback: 0.3,
                    ..FmOperator::new(1.0, 2.2)
                },
                FmOperator {
                    attack: 0.001,
                    decay: 0.15,
                    sustain: 0.0,
                    release: 0.08,
                    velocity: 0.8,
                    key_level: -3.0,
                    key_rate: 0.3,
                    ..FmOperator::new(2.0, 0.6)
                },
                FmOperator {
                    attack: 0.0005,
                    decay: 0.08,
                    sustain: 0.0,
                    release: 0.05,
                    velocity: 0.8,
                    key_level: -3.0,
                    ..FmOperator::new(3.0, 0.3)
                },
            ],
            accent_db: 3.0,
            accent_index: 1.5,
            level: 0.75,
        }
    }

    fn limited(self) -> Self {
        Self {
            algorithm: self.algorithm,
            operators: self.operators.map(FmOperator::limited),
            accent_db: setting(self.accent_db, 0.0, 24.0, 0.0),
            accent_index: setting(self.accent_index, 1.0, 4.0, 1.0),
            level: setting(self.level, 0.0, 4.0, 0.5),
        }
    }
}

impl Default for FmParams {
    fn default() -> Self {
        Self::electric_piano()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Sustain,
    Release,
}

/// An operator's envelope: a straight attack, then falls even in
/// decibels.
#[derive(Debug, Clone, Copy)]
struct Envelope {
    stage: Stage,
    level: f64,
    attack_step: f64,
    decay: f64,
    sustain: f64,
    release: f64,
}

impl Envelope {
    const IDLE: Self = Self {
        stage: Stage::Idle,
        level: 0.0,
        attack_step: 1.0,
        decay: 0.0,
        sustain: 0.0,
        release: 0.0,
    };

    /// Starts from the level the envelope is at, so that a note struck
    /// again does not jump.
    fn start(&mut self, operator: &FmOperator, time_scale: f64, sample_rate: f64) {
        let samples = operator.attack * time_scale * sample_rate;
        self.attack_step = if samples < 1.0 { 1.0 } else { 1.0 / samples };
        self.decay = t60_factor(operator.decay * time_scale, sample_rate);
        self.sustain = operator.sustain;
        self.release = t60_factor(operator.release * time_scale, sample_rate);
        self.stage = Stage::Attack;
    }

    fn release(&mut self) {
        if self.stage != Stage::Idle {
            self.stage = Stage::Release;
        }
    }

    #[inline]
    fn next(&mut self) -> f64 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level *= self.decay;
                if self.level <= self.sustain {
                    self.level = self.sustain;
                    self.stage = Stage::Sustain;
                }
                if self.level < SILENCE {
                    *self = Self::IDLE;
                }
            }
            Stage::Sustain => {}
            Stage::Release => {
                self.level *= self.release;
                if self.level < SILENCE {
                    *self = Self::IDLE;
                }
            }
        }
        self.level
    }
}

/// An operator as it plays: its phase, its envelope and what it puts out.
#[derive(Debug, Clone, Copy)]
struct Oscillator {
    /// Position in the cycle, from 0 up to 1.
    phase: f64,
    increment: f64,
    envelope: Envelope,
    /// The amplitude or index after velocity, keyboard scaling, accent and
    /// the limit on sidebands.
    gain: f64,
    /// Phase feedback, in radians at full envelope, and the last two
    /// outputs it is taken from.
    feedback: f64,
    history: [f64; 2],
}

impl Oscillator {
    const NEW: Self = Self {
        phase: 0.0,
        increment: 0.0,
        envelope: Envelope::IDLE,
        gain: 0.0,
        feedback: 0.0,
        history: [0.0; 2],
    };
}

/// One FM note.
#[derive(Debug, Clone)]
struct FmTone {
    sample_rate: f64,
    operators: [Oscillator; FM_OPERATORS],
    modulates: [[bool; FM_OPERATORS]; FM_OPERATORS],
    carriers: [bool; FM_OPERATORS],
    /// Levels and feedbacks before the limit on sidebands, kept so that a
    /// bend can apply the limit again.
    gains: [f64; FM_OPERATORS],
    feedbacks: [f64; FM_OPERATORS],
    /// The frequency of each operator's own sine.
    frequencies: [f64; FM_OPERATORS],
    level: f64,
}

impl FmTone {
    fn new() -> Self {
        Self {
            sample_rate: 48_000.0,
            operators: [Oscillator::NEW; FM_OPERATORS],
            modulates: [[false; FM_OPERATORS]; FM_OPERATORS],
            carriers: [false; FM_OPERATORS],
            gains: [0.0; FM_OPERATORS],
            feedbacks: [0.0; FM_OPERATORS],
            frequencies: [0.0; FM_OPERATORS],
            level: 0.0,
        }
    }

    fn strike(&mut self, params: &FmParams, key: f64, velocity: f64, accent: bool) {
        let (modulates, carriers) = params.algorithm.routing();
        self.modulates = modulates;
        self.carriers = carriers;
        self.level = params.level;
        let octaves = (key - 60.0) / 12.0;
        for (index, operator) in params.operators.iter().enumerate() {
            let mut gain = operator.level
                * db(-FM_VELOCITY_RANGE_DB * operator.velocity * (1.0 - velocity))
                * db((operator.key_level * octaves).clamp(-48.0, 12.0));
            if accent {
                gain *= if carriers[index] {
                    db(params.accent_db)
                } else {
                    params.accent_index
                };
            }
            self.gains[index] = gain;
            self.feedbacks[index] = MAX_FEEDBACK * operator.feedback;
            let time_scale = (-operator.key_rate * octaves).exp2();
            self.operators[index]
                .envelope
                .start(operator, time_scale, self.sample_rate);
        }
        self.tune(params, key);
    }

    /// Sets every operator's frequency for a note of `key`, and turns the
    /// modulation down where its sidebands would pass the limit.
    ///
    /// An operator modulated with index `I` by a signal whose highest
    /// significant frequency is `r` has sidebands up to its own frequency
    /// plus `n r`, where `n` is the order of the last sideband that matters.
    /// Carson's rule takes `n = I + 1`, which leaves out about 2% of the
    /// power, -17 dB, too much to fold back; the Bessel functions that give
    /// the sidebands' sizes are 50 dB down by `n = I + 1 + 2√I`, which is
    /// what is used here. That is the reach of the operator's own output
    /// when it modulates something else in turn. Feedback of `β` radians
    /// makes harmonics that are 50 dB down by about the `20 β`-th. A
    /// modulator above the limit is silenced; if a carrier would still
    /// reach past it, every index and feedback is scaled down by the factor
    /// that brings it back, found by bisection.
    fn tune(&mut self, params: &FmParams, key: f64) {
        let rate = self.sample_rate;
        let note = key_hz(key);
        let limit = REACH_LIMIT * rate;
        for (index, operator) in params.operators.iter().enumerate() {
            let frequency = note * operator.ratio * (operator.detune / 1_200.0).exp2();
            self.frequencies[index] = frequency;
            self.operators[index].increment = (frequency / rate).min(0.5);
        }
        let audible = |index: usize| self.frequencies[index] < limit;
        let reach = |scale: f64| {
            let mut reach = [0.0f64; FM_OPERATORS];
            let mut widest = 0.0f64;
            for target in (0..FM_OPERATORS).rev() {
                let feedback = self.feedbacks[target] * scale;
                let mut highest = self.frequencies[target] * (1.0 + 20.0 * feedback);
                for (source, &source_reach) in reach.iter().enumerate().skip(target + 1) {
                    if self.modulates[target][source] && audible(source) {
                        let index = self.gains[source] * scale;
                        if index > 0.0 {
                            highest += (index + 1.0 + 2.0 * index.sqrt()) * source_reach;
                        }
                    }
                }
                reach[target] = highest;
                if self.carriers[target] && audible(target) {
                    widest = widest.max(highest);
                }
            }
            widest
        };
        let mut scale = 1.0;
        if reach(1.0) > limit {
            let (mut low, mut high) = (0.0, 1.0);
            for _ in 0..30 {
                let middle = 0.5 * (low + high);
                if reach(middle) > limit {
                    high = middle;
                } else {
                    low = middle;
                }
            }
            scale = low;
        }
        for index in 0..FM_OPERATORS {
            let operator = &mut self.operators[index];
            let heard = audible(index);
            operator.gain = match (self.carriers[index], heard) {
                (_, false) => 0.0,
                (true, true) => self.gains[index],
                (false, true) => self.gains[index] * scale,
            };
            operator.feedback = self.feedbacks[index] * scale;
        }
    }

    fn release(&mut self) {
        for operator in &mut self.operators {
            operator.envelope.release();
        }
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let mut outputs = [0.0; FM_OPERATORS];
        let mut sum = 0.0;
        for index in (0..FM_OPERATORS).rev() {
            let modulation: f64 = outputs
                .iter()
                .zip(&self.modulates[index])
                .skip(index + 1)
                .filter(|(_, modulates)| **modulates)
                .map(|(output, _)| output)
                .sum();
            let operator = &mut self.operators[index];
            let envelope = operator.envelope.next();
            if envelope == 0.0 || operator.gain == 0.0 {
                operator.history = [0.0; 2];
            } else {
                // Feedback FM is N. Tomisawa's (US patent 4,249,447, 1981).
                // Taking the average of the last two outputs keeps it from
                // oscillating at half the sample rate.
                let feedback =
                    operator.feedback * 0.5 * (operator.history[0] + operator.history[1]);
                let raw = (TAU * operator.phase + modulation + feedback).sin() * envelope;
                operator.history = [raw, operator.history[0]];
                outputs[index] = raw * operator.gain;
            }
            operator.phase += operator.increment;
            if operator.phase >= 1.0 {
                operator.phase -= 1.0;
            }
            if self.carriers[index] {
                sum += outputs[index];
            }
        }
        sum * self.level
    }
}

impl Tone for FmTone {
    fn is_sounding(&self) -> bool {
        self.operators
            .iter()
            .zip(&self.carriers)
            .any(|(operator, &carrier)| carrier && operator.envelope.stage != Stage::Idle)
    }

    fn silence(&mut self) {
        self.operators.fill(Oscillator::NEW);
    }
}

/// Frequency modulation: a voice for [`Synth`](crate::Synth) made of four
/// sine operators, each with its own envelope, connected by an
/// [`Algorithm`].
///
/// The method is J. M. Chowning, "The synthesis of complex audio spectra by
/// means of frequency modulation", JAES 1973, in the phase-modulation form
/// the classic FM synthesisers use: an operator's output is added to the
/// phase of the operators it modulates. Presets:
/// [`electric_piano`](FmParams::electric_piano) (tine and tone bar),
/// [`tubular_bell`](FmParams::tubular_bell) and [`fm_bass`](FmParams::fm_bass).
///
/// - **Envelopes** are a straight attack and falls even in decibels to a
///   sustain level and, after the key is released, to silence. Their times
///   shorten up the keyboard by [`FmOperator::key_rate`].
/// - **Velocity** scales each operator's level by its own sensitivity: on a
///   carrier the note is louder, on a modulator brighter.
/// - **An accent** makes the carriers [`FmParams::accent_db`] louder and
///   multiplies the modulators' indices by [`FmParams::accent_index`].
/// - **Aliasing** is kept away by construction. Sines are band-limited, but
///   the sidebands of a modulated sine are not: each note works out how far
///   up its sidebands reach (Carson's rule, through the whole chain of
///   modulators) and, where that would pass 0.45 of the sample rate, turns
///   every index and feedback down by the factor that brings it back, as
///   the keyboard level scaling of the classic instruments does. A
///   modulator above that frequency is silenced. High notes are therefore
///   less bright than low ones; the tests measure what aliasing remains.
///
/// Striking a sounding key again restarts the envelopes from where they
/// are, with the phases running on; a voice taken over for a new note hands
/// its old note to a fade over
/// [`HANDOVER_SECONDS`](super::HANDOVER_SECONDS). Pitch bend and
/// [`Expression::Tuning`] retune every operator at once.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{FmParams, FmVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut piano = Synth::with_voices(8, || FmVoice::new(FmParams::electric_piano()));
/// piano.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [57, 60, 64] {
///     piano.note_on(NoteRef::new(0, key), 0.7);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// piano.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct FmVoice {
    params: FmParams,
    tones: Handover<FmTone>,
    key: u8,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
}

impl FmVoice {
    /// A voice with these settings.
    pub fn new(params: FmParams) -> Self {
        Self {
            params: params.limited(),
            tones: Handover::new(FmTone::new(), FmTone::new()),
            key: 60,
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
        }
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note keeps its envelopes and levels until it is struck
    /// again, and takes the new level at once.
    pub fn set_params(&mut self, params: FmParams) {
        self.params = params.limited();
        self.tones.now.level = self.params.level;
        self.tones.old.level = self.params.level;
    }

    /// The settings, as limited.
    pub fn params(&self) -> &FmParams {
        &self.params
    }

    fn pitch(&self) -> f64 {
        self.key_tuning.key_pitch(self.key) + self.bend + self.tuning
    }
}

impl<S: Sample> Voice<S> for FmVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.tones.prepare(spec.sample_rate);
        for tone in [&mut self.tones.now, &mut self.tones.old] {
            tone.sample_rate = spec.sample_rate;
            tone.silence();
        }
    }

    fn set_tuning(&mut self, tuning: &Tuning) {
        self.key_tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        self.key = note.key.min(127);
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let params = self.params;
        let pitch = self.pitch();
        self.tones.now.strike(&params, pitch, velocity, accent);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.tones.now.release();
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
        add_to_every_channel(buffer, || tones.next(FmTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            let (params, pitch) = (self.params, self.pitch());
            self.tones.now.tune(&params, pitch);
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            let (params, pitch) = (self.params, self.pitch());
            self.tones.now.tune(&params, pitch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::measure::{
        Act, RATE, accented, between, centroid, cents, decibels, finite_within, frequency_near,
        inharmonic_db, largest_jump, note, off, on, peak, play, rms, synth_of, t60_at,
    };

    fn presets() -> [(&'static str, FmParams); 3] {
        [
            ("piano", FmParams::electric_piano()),
            ("bell", FmParams::tubular_bell()),
            ("bass", FmParams::fm_bass()),
        ]
    }

    fn voice(params: FmParams) -> [FmVoice; 1] {
        [FmVoice::new(params)]
    }

    #[test]
    fn harmonic_presets_are_in_tune_across_the_keyboard() {
        let mut worst = 0.0f64;
        // The piano's tine is detuned by a few cents on purpose, a chorus
        // against the tone bar; here every operator is at its exact ratio.
        let mut piano = FmParams::electric_piano();
        for operator in &mut piano.operators {
            operator.detune = 0.0;
        }
        for (name, params) in [("piano", piano), presets()[2]] {
            for key in (24..=108).step_by(7) {
                let signal = note(voice(params), on(key, 0.8), 0.6, 0.6);
                let expected = key_hz(f64::from(key));
                let measured = frequency_near(between(&signal, 0.05, 0.55), expected);
                let error = cents(measured, expected);
                worst = worst.max(error.abs());
                assert!(error.abs() < 1.0, "{name} key {key}: {error:.3} cents");
            }
        }
        println!("worst tuning error {worst:.4} cents");
    }

    #[test]
    fn an_envelope_falls_in_the_time_it_is_given() {
        // One plain carrier, so the level is the envelope.
        let params = FmParams {
            algorithm: Algorithm::Additive,
            operators: [
                FmOperator {
                    decay: 4.0,
                    sustain: 0.0,
                    release: 0.3,
                    ..FmOperator::new(1.0, 0.5)
                },
                FmOperator::OFF,
                FmOperator::OFF,
                FmOperator::OFF,
            ],
            ..FmParams::electric_piano()
        };
        let signal = note(voice(params), on(60, 1.0), 1.0, 1.6);
        let decay = t60_at(&signal, key_hz(60.0), 0.1, 0.95);
        let release = t60_at(&signal, key_hz(60.0), 1.02, 1.3);
        println!("decay {decay:.4} s, release {release:.4} s");
        assert!((decay / 4.0 - 1.0).abs() < 0.01, "{decay}");
        assert!((release / 0.3 - 1.0).abs() < 0.03, "{release}");

        // The bell rings for a long time, and its higher notes die sooner.
        let bell = FmParams::tubular_bell();
        let low = note(voice(bell), on(60, 1.0), 6.0, 6.0);
        let level_at = |signal: &[f64], t: f64| decibels(rms(between(signal, t, t + 0.1)));
        let fall = level_at(&low, 0.1) - level_at(&low, 4.0);
        println!("the bell falls {fall:.1} dB in 3.9 s");
        assert!((20.0..45.0).contains(&fall), "{fall}");
        let high = note(voice(bell), on(84, 1.0), 6.0, 6.0);
        assert!(level_at(&high, 0.1) - level_at(&high, 4.0) > fall + 10.0);
    }

    #[test]
    fn velocity_and_accent_make_notes_louder_and_brighter() {
        for (name, params) in presets() {
            let loud = note(voice(params), on(57, 1.0), 0.5, 0.5);
            let soft = note(voice(params), on(57, 0.4), 0.5, 0.5);
            let strong = note(voice(params), accented(57, 0.7), 0.5, 0.5);
            let plain = note(voice(params), on(57, 0.7), 0.5, 0.5);
            let velocity_db = decibels(rms(&loud) / rms(&soft));
            let accent_db = decibels(rms(&strong) / rms(&plain));
            let velocity_brightness = centroid(&loud, 8_192) / centroid(&soft, 8_192);
            let accent_brightness = centroid(&strong, 8_192) / centroid(&plain, 8_192);
            println!(
                "{name}: velocity {velocity_db:.1} dB and {velocity_brightness:.2} times the \
                 centroid, accent {accent_db:.1} dB and {accent_brightness:.2} times"
            );
            assert!(velocity_db > 6.0, "{name}: {velocity_db}");
            assert!(velocity_brightness > 1.2, "{name}: {velocity_brightness}");
            assert!(accent_db > 0.6 * params.accent_db, "{name}: {accent_db}");
            assert!(accent_brightness > 1.05, "{name}: {accent_brightness}");
        }
    }

    /// A held, bright stack: two modulators at full index under a carrier,
    /// and feedback on top. Left alone its sidebands would reach far past
    /// half the sample rate at the higher keys.
    fn glaring() -> FmParams {
        let held = |ratio, level| FmOperator {
            sustain: 1.0,
            decay: 100.0,
            ..FmOperator::new(ratio, level)
        };
        FmParams {
            algorithm: Algorithm::Stack,
            operators: [
                held(1.0, 0.5),
                held(1.0, 6.0),
                held(2.0, 4.0),
                FmOperator {
                    feedback: 1.0,
                    ..held(1.0, 2.0)
                },
            ],
            accent_db: 0.0,
            accent_index: 1.0,
            level: 1.0,
        }
    }

    #[test]
    fn high_notes_are_kept_free_of_aliasing() {
        for (name, params) in [presets()[0], presets()[2], ("glaring", glaring())] {
            for key in [72u8, 84, 96, 103] {
                // Accented, for the widest spectrum, and after the attack,
                // whose own spread is no aliasing.
                let signal = note(voice(params), accented(key, 1.0), 0.5, 0.5);
                let f0 = key_hz(f64::from(key));
                let other = inharmonic_db(between(&signal, 0.05, 0.5), f0, 30.0, 16_384);
                println!("{name} key {key}: {other:.1} dB outside the harmonics");
                assert!(other < -50.0, "{name} key {key}: {other} dB");
            }
        }
    }

    #[test]
    fn notes_start_end_and_are_taken_over_without_a_click() {
        for (name, params) in presets() {
            let alone = note(voice(params), on(52, 1.0), 0.3, 0.8);
            // The first millisecond, while the envelopes rise, against the
            // rest of the note, whose brightest moment is just after.
            let attack = largest_jump(between(&alone, 0.0, 0.001));
            let steady = largest_jump(between(&alone, 0.001, 0.3));
            let release = largest_jump(between(&alone, 0.3, 0.8));
            assert!(alone[0].abs() < 1e-3, "{name} starts at {}", alone[0]);
            assert!(
                attack <= 1.05 * steady,
                "{name}: attack {attack} against {steady}"
            );
            assert!(
                release <= 1.05 * steady,
                "{name}: release {release} against {steady}"
            );

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
                at_steal <= 1.05 * steady,
                "{name}: {at_steal} at the steal against {steady}"
            );
            assert!(peak(&residual[steal + 400..]) < 1e-12, "{name}");

            // Struck again while sounding: no jump either.
            let mut synth = synth_of(voice(params));
            let again = play(
                &mut synth,
                &[(0, on(52, 1.0)), (steal, on(52, 1.0))],
                steal * 2,
                256,
            );
            let jump = largest_jump(&again[steal - 64..steal + 512]);
            assert!(
                jump <= 1.05 * steady.max(largest_jump(&again[steal + 512..])),
                "{name}"
            );
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(40, 0.9)),
            (1_000, accented(47, 0.7)),
            (7_000, on(52, 0.5)),
            (9_001, off(40)),
            (9_500, Act::Bend(0.7)),
            (12_000, on(40, 1.0)),
            (20_000, off(47)),
        ];
        for (name, params) in presets() {
            let run = |block| {
                let mut synth = synth_of([FmVoice::new(params), FmVoice::new(params)]);
                play(&mut synth, &script, 30_000, block)
            };
            let reference = run(256);
            for block in [1, 7, 64, 1_000] {
                assert!(run(block) == reference, "{name} with blocks of {block}");
            }
        }
    }

    #[test]
    fn extreme_settings_and_notes_stay_finite() {
        let wild = FmOperator {
            ratio: f64::INFINITY,
            detune: f64::NAN,
            level: 1e9,
            attack: -1.0,
            decay: 0.0,
            sustain: f64::NAN,
            release: f64::INFINITY,
            velocity: 1e9,
            key_level: 1e9,
            key_rate: f64::NAN,
            feedback: 1e9,
        };
        let loud = FmOperator {
            level: 16.0,
            feedback: 1.0,
            key_level: 24.0,
            ..FmOperator::new(32.0, 16.0)
        };
        for algorithm in Algorithm::ALL {
            for operator in [wild, loud] {
                let params = FmParams {
                    algorithm,
                    operators: [operator; FM_OPERATORS],
                    accent_db: f64::INFINITY,
                    accent_index: 1e9,
                    level: f64::NAN,
                };
                let mut synth = synth_of([FmVoice::new(params), FmVoice::new(params)]);
                let script = [
                    (0, accented(0, 1.0)),
                    (10, accented(127, 1.0)),
                    (20, Act::Bend(48.0)),
                    (500, accented(127, 1.0)),
                    (900, Act::Bend(-48.0)),
                    (2_000, off(0)),
                ];
                let signal = play(&mut synth, &script, 6_000, 100);
                // Four carriers at full level, the accent's 24 dB, the
                // keyboard's 12 and a level of 4.
                let bound = 4.0 * 16.0 * 16.0 * 4.0 * 4.0;
                assert!(finite_within(&signal, bound), "{algorithm:?}");
            }
        }
    }
}
