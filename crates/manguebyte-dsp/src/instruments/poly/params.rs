//! The settings of a [`PolySynthVoice`](super::PolySynthVoice): plain data,
//! so that a preset is a constant and an interface can list every choice.

use crate::{LfoShape, NoiseColor};

use super::super::common::setting;
use super::tables;

/// How many oscillators a voice has, beside its sub oscillator and noise.
pub const OSCILLATORS: usize = 3;

/// The most unison voices one oscillator can play.
pub const MAX_UNISON: usize = 16;

/// How many filters a voice has.
pub const FILTERS: usize = 2;

/// How many envelopes a voice has. The first shapes the amplifier, the
/// second opens the filters by their [`env_amount`](FilterParams::env_amount).
pub const ENVELOPES: usize = 4;

/// How many low-frequency oscillators a voice has.
pub const LFOS: usize = 3;

/// How many slots the modulation matrix has.
pub const MOD_SLOTS: usize = 16;

/// How far apart the outermost unison voices are at a detune of 1, in
/// cents either side of the note.
pub const MAX_DETUNE_CENTS: f64 = 100.0;

/// The shape an oscillator plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Wave {
    /// A sine.
    Sine,
    /// A triangle.
    Triangle,
    /// A sawtooth: every harmonic, falling 6 dB per octave.
    #[default]
    Saw,
    /// A pulse. Its [`shape`](OscParams::shape) narrows it from a square
    /// (0) to a width of 2% (1); modulating the shape is pulse-width
    /// modulation.
    Pulse,
    /// One of the voice's [wavetables](TableShape). Its
    /// [`shape`](OscParams::shape) is the position in the table.
    Table,
}

impl Wave {
    /// Every shape, in the order of their indices.
    pub const ALL: [Self; 5] = [
        Self::Sine,
        Self::Triangle,
        Self::Saw,
        Self::Pulse,
        Self::Table,
    ];

    /// The shape's name, for an interface.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Triangle => "triangle",
            Self::Saw => "saw",
            Self::Pulse => "pulse",
            Self::Table => "wavetable",
        }
    }
}

/// The wavetables, built once from formulas. Each has sixteen frames that
/// an oscillator's [`shape`](OscParams::shape) moves through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TableShape {
    /// The classic shapes in a row: sine, triangle, sawtooth, square.
    #[default]
    Basic,
    /// A sawtooth whose harmonics are let in one octave at a time, from a
    /// sine to the full sawtooth: a filter sweep without a filter.
    Harmonics,
    /// A sawtooth hard-synced to a master from the same pitch up to eight
    /// times higher: the sync sweep.
    Sync,
    /// A sine through a wavefolder whose gain rises from 1 to 12.
    Fold,
    /// The vowels a, e, i, o and u, sung on a bright source.
    Vowel,
    /// Darker vowels, oo to ah, on a breathy source: a choir.
    Choir,
    /// A sine quantised to fewer and fewer levels: stepped digital waves.
    Digital,
    /// A sine with high, sparse partials that grow: glass and bells.
    Glass,
    /// Drawbar registrations of a tonewheel organ, from a flute to full.
    Organ,
    /// Clusters of neighbouring high harmonics that move up: metallic.
    Metal,
    /// Spectra of random harmonic levels, a new one in every frame.
    Spectral,
    /// A sawtooth with a resonant peak that sweeps from the second
    /// harmonic to the 256th.
    Resonant,
}

impl TableShape {
    /// Every table, in the order of their indices.
    pub const ALL: [Self; 12] = [
        Self::Basic,
        Self::Harmonics,
        Self::Sync,
        Self::Fold,
        Self::Vowel,
        Self::Choir,
        Self::Digital,
        Self::Glass,
        Self::Organ,
        Self::Metal,
        Self::Spectral,
        Self::Resonant,
    ];

    /// The table's name, for an interface.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Harmonics => "harmonics",
            Self::Sync => "sync",
            Self::Fold => "fold",
            Self::Vowel => "vowel",
            Self::Choir => "choir",
            Self::Digital => "digital",
            Self::Glass => "glass",
            Self::Organ => "organ",
            Self::Metal => "metal",
            Self::Spectral => "spectral",
            Self::Resonant => "resonant",
        }
    }

    /// The table's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }

    /// How many frames each table has: an oscillator's
    /// [`shape`](OscParams::shape) moves from the first (0) to the last (1).
    pub const FRAMES: usize = tables::FRAMES;

    /// One cycle of the table at `position`, between its first frame (0)
    /// and its last (1), in `samples` evenly spaced points from phase 0:
    /// the waveform with every harmonic, peaking near 1, as a low note
    /// plays it. It is for drawing; a voice reads the table itself.
    ///
    /// The first call builds every table, which takes a moment and about
    /// 14 MB, as the first voice made does; later calls only read them.
    /// It allocates, so it belongs off the audio thread.
    ///
    /// ```
    /// use manguebyte_dsp::instruments::poly::TableShape;
    ///
    /// // The basic table starts as a sine.
    /// let cycle = TableShape::Basic.cycle(0.0, 64);
    /// assert!((cycle[16] - 1.0).abs() < 1e-5 && cycle[0].abs() < 1e-5);
    /// ```
    pub fn cycle(self, position: f64, samples: usize) -> Vec<f32> {
        let table = &tables::bank()[self.index()];
        (0..samples)
            .map(|n| table.read(0, position, n as f64 / samples as f64) as f32)
            .collect()
    }

    /// Every frame of the table, first to last, as a cycle of `samples`
    /// points each, the way [`cycle`](Self::cycle) gives one.
    pub fn frames(self, samples: usize) -> Vec<Vec<f32>> {
        (0..Self::FRAMES)
            .map(|frame| self.cycle(frame as f64 / (Self::FRAMES - 1) as f64, samples))
            .collect()
    }
}

/// Which oscillator modulates another by frequency or ring modulation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OscSource {
    /// The first oscillator.
    #[default]
    Osc1,
    /// The second oscillator.
    Osc2,
    /// The third oscillator.
    Osc3,
}

impl OscSource {
    /// Every source, in the order of their indices.
    pub const ALL: [Self; 3] = [Self::Osc1, Self::Osc2, Self::Osc3];

    /// The oscillator's index, from 0.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// One of the three oscillators.
///
/// Values out of range are limited; one that is not a number takes the
/// value of [`OscParams::SAW`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OscParams {
    /// The shape.
    pub wave: Wave,
    /// The wavetable played when [`wave`](Self::wave) is
    /// [`Wave::Table`].
    pub table: TableShape,
    /// The level, 0 to 1. An oscillator at 0 is not heard, but can still
    /// modulate another.
    pub level: f64,
    /// Transposition in semitones, -48 to 48.
    pub semitones: f64,
    /// Fine tuning in cents, -100 to 100.
    pub cents: f64,
    /// 0 to 1: the width of a pulse, or the position in a wavetable.
    pub shape: f64,
    /// How many unison voices play the oscillator, 1 to [`MAX_UNISON`].
    pub unison: u8,
    /// How far the unison voices are detuned, 0 to 1. The outermost two
    /// are `±MAX_DETUNE_CENTS · detune²` from the note, and the others are
    /// spread between them more densely towards the middle.
    pub detune: f64,
    /// How widely the unison voices are spread across the stereo field, 0
    /// (all in the middle) to 1 (from hard left to hard right).
    pub spread: f64,
    /// Where the oscillator sits in the stereo field, -1 (left) to 1.
    pub pan: f64,
    /// The phase every voice starts a note at, 0 to 1 of a cycle.
    pub phase: f64,
    /// How far each voice's starting phase is moved at random, 0 (not at
    /// all: a punchy, repeatable attack) to 1 (anywhere in the cycle: free
    /// running, as analogue oscillators are).
    pub random_phase: f64,
    /// Whether the oscillator is hard-synced to oscillator 1: it starts its
    /// cycle again whenever oscillator 1 does. Oscillator 1 ignores it.
    pub sync: bool,
    /// The oscillator that [`fm`](Self::fm) and [`ring`](Self::ring) take.
    /// A later oscillator is read one sample late; an oscillator taking
    /// itself is feedback.
    pub mod_source: OscSource,
    /// Phase modulation by [`mod_source`](Self::mod_source): the peak
    /// deviation of the phase, in radians, 0 to 16 (the FM index).
    pub fm: f64,
    /// Ring modulation by [`mod_source`](Self::mod_source), 0 (none) to 1
    /// (only the product of the two is heard).
    pub ring: f64,
}

impl OscParams {
    /// A sawtooth at full level, one voice, nothing else.
    pub const SAW: Self = Self {
        wave: Wave::Saw,
        table: TableShape::Basic,
        level: 1.0,
        semitones: 0.0,
        cents: 0.0,
        shape: 0.0,
        unison: 1,
        detune: 0.0,
        spread: 0.0,
        pan: 0.0,
        phase: 0.0,
        random_phase: 0.0,
        sync: false,
        mod_source: OscSource::Osc1,
        fm: 0.0,
        ring: 0.0,
    };

    /// An oscillator that is not heard.
    pub const OFF: Self = Self {
        level: 0.0,
        ..Self::SAW
    };

    /// An oscillator of `wave` at `level`, the rest as [`SAW`](Self::SAW).
    pub const fn new(wave: Wave, level: f64) -> Self {
        Self {
            wave,
            level,
            ..Self::SAW
        }
    }

    /// A wavetable oscillator at `position` in `table`.
    pub const fn table(table: TableShape, position: f64, level: f64) -> Self {
        Self {
            wave: Wave::Table,
            table,
            shape: position,
            level,
            ..Self::SAW
        }
    }

    /// The same oscillator with `voices` unison voices, detuned and spread.
    pub const fn unison(self, voices: u8, detune: f64, spread: f64) -> Self {
        Self {
            unison: voices,
            detune,
            spread,
            random_phase: 1.0,
            ..self
        }
    }

    /// The same oscillator transposed.
    pub const fn tuned(self, semitones: f64, cents: f64) -> Self {
        Self {
            semitones,
            cents,
            ..self
        }
    }

    /// The same oscillator with another [`shape`](Self::shape).
    pub const fn shaped(self, shape: f64) -> Self {
        Self { shape, ..self }
    }

    /// The same oscillator placed in the stereo field.
    pub const fn panned(self, pan: f64) -> Self {
        Self { pan, ..self }
    }

    /// The same oscillator hard-synced to oscillator 1.
    pub const fn synced(self) -> Self {
        Self { sync: true, ..self }
    }

    /// The same oscillator phase-modulated by `source` with `index`.
    pub const fn fm_by(self, source: OscSource, index: f64) -> Self {
        Self {
            mod_source: source,
            fm: index,
            ..self
        }
    }

    /// The same oscillator ring-modulated by `source` by `amount`.
    pub const fn ring_by(self, source: OscSource, amount: f64) -> Self {
        Self {
            mod_source: source,
            ring: amount,
            ..self
        }
    }

    /// The same oscillator starting its notes at `phase`, moved at random
    /// by up to `random`.
    pub const fn starting(self, phase: f64, random: f64) -> Self {
        Self {
            phase,
            random_phase: random,
            ..self
        }
    }

    pub(super) fn limited(self) -> Self {
        let d = Self::SAW;
        Self {
            wave: self.wave,
            table: self.table,
            level: setting(self.level, 0.0, 1.0, d.level),
            semitones: setting(self.semitones, -48.0, 48.0, d.semitones),
            cents: setting(self.cents, -100.0, 100.0, d.cents),
            shape: setting(self.shape, 0.0, 1.0, d.shape),
            unison: self.unison.clamp(1, MAX_UNISON as u8),
            detune: setting(self.detune, 0.0, 1.0, d.detune),
            spread: setting(self.spread, 0.0, 1.0, d.spread),
            pan: setting(self.pan, -1.0, 1.0, d.pan),
            phase: setting(self.phase, 0.0, 1.0, d.phase),
            random_phase: setting(self.random_phase, 0.0, 1.0, d.random_phase),
            sync: self.sync,
            mod_source: self.mod_source,
            fm: setting(self.fm, 0.0, 16.0, d.fm),
            ring: setting(self.ring, 0.0, 1.0, d.ring),
        }
    }
}

impl Default for OscParams {
    fn default() -> Self {
        Self::SAW
    }
}

/// The shape of the sub oscillator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum SubWave {
    /// A sine: weight without colour.
    #[default]
    Sine,
    /// A triangle.
    Triangle,
    /// A square, as the dividers of the classic synthesisers make.
    Square,
    /// A sawtooth.
    Saw,
}

impl SubWave {
    /// Every shape, in the order of their indices.
    pub const ALL: [Self; 4] = [Self::Sine, Self::Triangle, Self::Square, Self::Saw];
}

/// The sub oscillator: one voice an octave or two below oscillator 1,
/// following its pitch without its detune.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SubParams {
    /// The shape.
    pub wave: SubWave,
    /// How many octaves below oscillator 1, 1 or 2.
    pub octaves: u8,
    /// The level, 0 to 1.
    pub level: f64,
}

impl SubParams {
    /// No sub oscillator.
    pub const OFF: Self = Self {
        wave: SubWave::Sine,
        octaves: 1,
        level: 0.0,
    };

    fn limited(self) -> Self {
        Self {
            wave: self.wave,
            octaves: self.octaves.clamp(1, 2),
            level: setting(self.level, 0.0, 1.0, 0.0),
        }
    }
}

/// The noise source, mixed in before the filters.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoiseParams {
    /// White, pink or brown.
    pub color: NoiseColor,
    /// The level, 0 to 1.
    pub level: f64,
}

impl NoiseParams {
    /// No noise.
    pub const OFF: Self = Self {
        color: NoiseColor::White,
        level: 0.0,
    };

    fn limited(self) -> Self {
        Self {
            color: self.color,
            level: setting(self.level, 0.0, 1.0, 0.0),
        }
    }
}

/// The kind of a filter. See [`PolySynthVoice`](super::PolySynthVoice)
/// for the models and why each is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FilterModel {
    /// No filter: a wire in series, nothing in parallel.
    Off,
    /// The transistor ladder, low-pass, 24 dB per octave: the classic
    /// four-pole.
    #[default]
    LadderLp24,
    /// The ladder's low-pass after two stages, 12 dB per octave.
    LadderLp12,
    /// The ladder's high-pass, 24 dB per octave.
    LadderHp24,
    /// The ladder's band-pass, 6 dB per octave each side.
    LadderBp12,
    /// The linear state-variable filter, low-pass, 12 dB per octave.
    SvfLp,
    /// The state-variable band-pass, unity gain at its centre.
    SvfBp,
    /// The state-variable high-pass, 12 dB per octave.
    SvfHp,
    /// The state-variable notch.
    SvfNotch,
    /// A state-variable filter in the manner of the SEM: its
    /// [`morph`](FilterParams::morph) goes from low-pass through notch to
    /// high-pass, and its resonance is soft and never screams.
    SemNotch,
    /// The SEM-like filter morphing from low-pass through band-pass to
    /// high-pass.
    SemBand,
    /// A two-pole Sallen–Key low-pass with a saturating feedback loop, in
    /// the manner of the MS-20: aggressive, and it screams at full
    /// resonance.
    Ms20Lp,
    /// The MS-20-like high-pass.
    Ms20Hp,
    /// A comb filter with positive feedback: resonances at every multiple
    /// of the cutoff. With key tracking at 1 it rings at the note, as a
    /// plucked string does.
    CombPlus,
    /// A comb filter with negative feedback: resonances at the odd
    /// multiples of half the cutoff, hollow.
    CombMinus,
    /// Three band-passes at the formants of a vowel, which
    /// [`morph`](FilterParams::morph) moves through a, e, i, o and u. The
    /// cutoff moves the formants (1 kHz leaves them as spoken).
    Formant,
}

impl FilterModel {
    /// Every model, in the order of their indices.
    pub const ALL: [Self; 16] = [
        Self::Off,
        Self::LadderLp24,
        Self::LadderLp12,
        Self::LadderHp24,
        Self::LadderBp12,
        Self::SvfLp,
        Self::SvfBp,
        Self::SvfHp,
        Self::SvfNotch,
        Self::SemNotch,
        Self::SemBand,
        Self::Ms20Lp,
        Self::Ms20Hp,
        Self::CombPlus,
        Self::CombMinus,
        Self::Formant,
    ];

    /// The model's name, for an interface.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::LadderLp24 => "ladder LP24",
            Self::LadderLp12 => "ladder LP12",
            Self::LadderHp24 => "ladder HP24",
            Self::LadderBp12 => "ladder BP12",
            Self::SvfLp => "SVF LP",
            Self::SvfBp => "SVF BP",
            Self::SvfHp => "SVF HP",
            Self::SvfNotch => "SVF notch",
            Self::SemNotch => "SEM LP-notch-HP",
            Self::SemBand => "SEM LP-BP-HP",
            Self::Ms20Lp => "MS-20 LP",
            Self::Ms20Hp => "MS-20 HP",
            Self::CombPlus => "comb +",
            Self::CombMinus => "comb -",
            Self::Formant => "formant",
        }
    }
}

/// One of the two filters.
///
/// Values out of range are limited; one that is not a number takes the
/// value of [`FilterParams::LOWPASS`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FilterParams {
    /// The kind of filter.
    pub model: FilterModel,
    /// The cutoff in hertz, 20 to 20 000: for the combs the frequency of
    /// their first resonance, for the formant filter the shift of the
    /// formants (1 kHz leaves them where they are).
    pub cutoff: f64,
    /// The resonance, 0 to 1. The ladder and the MS-20-like filter reach
    /// self-oscillation at 1; the state-variable filters reach a Q of 20,
    /// the SEM-like one of 10; the combs' feedback reaches 0.985.
    pub resonance: f64,
    /// The gain into the filter's saturation, in decibels, 0 to 36. The
    /// ladder and the MS-20-like filter saturate in their loops; the
    /// others saturate their input, and at 0 dB are linear.
    pub drive: f64,
    /// How far the cutoff follows the key, 0 to 1: at 1 it rises an octave
    /// for every octave above middle C.
    pub key_track: f64,
    /// How far envelope 2 moves the cutoff, in octaves, -8 to 8.
    pub env_amount: f64,
    /// 0 to 1: the mode of the SEM-like filters, the vowel of the formant
    /// filter, the damping of the combs. Ignored by the others.
    pub morph: f64,
}

impl FilterParams {
    /// A ladder low-pass at 2 kHz with a little resonance.
    pub const LOWPASS: Self = Self {
        model: FilterModel::LadderLp24,
        cutoff: 2_000.0,
        resonance: 0.1,
        drive: 0.0,
        key_track: 0.0,
        env_amount: 0.0,
        morph: 0.0,
    };

    /// No filter.
    pub const OFF: Self = Self {
        model: FilterModel::Off,
        ..Self::LOWPASS
    };

    /// A filter of `model` at `cutoff` hertz with `resonance`.
    pub const fn new(model: FilterModel, cutoff: f64, resonance: f64) -> Self {
        Self {
            model,
            cutoff,
            resonance,
            ..Self::LOWPASS
        }
    }

    /// The same filter opened by envelope 2 by `octaves`.
    pub const fn env(self, octaves: f64) -> Self {
        Self {
            env_amount: octaves,
            ..self
        }
    }

    /// The same filter following the key by `amount`.
    pub const fn track(self, amount: f64) -> Self {
        Self {
            key_track: amount,
            ..self
        }
    }

    /// The same filter driven by `decibels`.
    pub const fn driven(self, decibels: f64) -> Self {
        Self {
            drive: decibels,
            ..self
        }
    }

    /// The same filter with another [`morph`](Self::morph).
    pub const fn morphed(self, morph: f64) -> Self {
        Self { morph, ..self }
    }

    pub(super) fn limited(self) -> Self {
        let d = Self::LOWPASS;
        Self {
            model: self.model,
            cutoff: setting(self.cutoff, 20.0, 20_000.0, d.cutoff),
            resonance: setting(self.resonance, 0.0, 1.0, d.resonance),
            drive: setting(self.drive, 0.0, 36.0, d.drive),
            key_track: setting(self.key_track, 0.0, 1.0, d.key_track),
            env_amount: setting(self.env_amount, -8.0, 8.0, d.env_amount),
            morph: setting(self.morph, 0.0, 1.0, d.morph),
        }
    }
}

impl Default for FilterParams {
    fn default() -> Self {
        Self::LOWPASS
    }
}

/// How the two filters are connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FilterRouting {
    /// The mix goes through filter 1 and then filter 2.
    #[default]
    Serial,
    /// The mix goes through both filters side by side, and their outputs
    /// are added.
    Parallel,
}

/// An envelope: attack, decay, sustain and release, each segment with a
/// curve.
///
/// A curve of 0 is a straight line; towards 1 the segment moves quickly at
/// first and slowly at the end, as the charging and discharging of a
/// capacitor do (an analogue envelope); towards -1 it starts slowly and
/// ends quickly. Every segment takes the time it is given whatever its
/// curve. Values out of range are limited; one that is not a number takes
/// the value of [`EnvParams::AMP`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EnvParams {
    /// Seconds to rise from silence to the peak, 0 to 20.
    pub attack: f64,
    /// Seconds to fall from the peak to the sustain level, 0 to 20.
    pub decay: f64,
    /// The level held while the key is down, 0 to 1.
    pub sustain: f64,
    /// Seconds to fall to silence once the key is released, 0 to 20.
    pub release: f64,
    /// The attack's curve, -1 to 1.
    pub attack_curve: f64,
    /// The decay's curve, -1 to 1.
    pub decay_curve: f64,
    /// The release's curve, -1 to 1.
    pub release_curve: f64,
}

impl EnvParams {
    /// A quick attack, full sustain and a short release, with analogue
    /// curves.
    pub const AMP: Self = Self {
        attack: 0.003,
        decay: 0.3,
        sustain: 1.0,
        release: 0.15,
        attack_curve: 0.3,
        decay_curve: 0.7,
        release_curve: 0.7,
    };

    /// An envelope with these times and analogue curves.
    pub const fn adsr(attack: f64, decay: f64, sustain: f64, release: f64) -> Self {
        Self {
            attack,
            decay,
            sustain,
            release,
            ..Self::AMP
        }
    }

    pub(super) fn limited(self) -> Self {
        let d = Self::AMP;
        Self {
            attack: setting(self.attack, 0.0, 20.0, d.attack),
            decay: setting(self.decay, 0.0, 20.0, d.decay),
            sustain: setting(self.sustain, 0.0, 1.0, d.sustain),
            release: setting(self.release, 0.0, 20.0, d.release),
            attack_curve: setting(self.attack_curve, -1.0, 1.0, d.attack_curve),
            decay_curve: setting(self.decay_curve, -1.0, 1.0, d.decay_curve),
            release_curve: setting(self.release_curve, -1.0, 1.0, d.release_curve),
        }
    }
}

impl Default for EnvParams {
    fn default() -> Self {
        Self::AMP
    }
}

/// A low-frequency oscillator. Its output runs from -1 to 1.
///
/// Values out of range are limited; one that is not a number takes the
/// value of [`LfoParams::DEFAULT`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LfoParams {
    /// The shape. A sample-and-hold draws a new level, repeatably for the
    /// note, at every cycle. Steps (square, saws, sample-and-hold) are
    /// rounded over a millisecond, so they do not click.
    pub shape: LfoShape,
    /// The rate in hertz, 0.01 to 50, when [`beats`](Self::beats) is 0.
    pub rate: f64,
    /// The length of a cycle in quarter notes, 0 to 64: when above 0 the
    /// rate follows the tempo ([`PolySynthVoice::set_tempo`](super::PolySynthVoice::set_tempo)).
    /// 0.25 is a sixteenth, 1 a beat, 4 a bar of 4/4.
    pub beats: f64,
    /// Whether every note starts the cycle again at [`phase`](Self::phase).
    /// When not, the oscillator runs on from note to note while the voice
    /// sounds.
    pub retrigger: bool,
    /// Where in the cycle a note starts it, 0 to 1.
    pub phase: f64,
    /// Seconds over which the oscillator's depth rises from nothing at the
    /// start of a note, 0 to 10: a delayed vibrato.
    pub fade_in: f64,
}

impl LfoParams {
    /// A sine at 5 Hz that starts again with every note.
    pub const DEFAULT: Self = Self {
        shape: LfoShape::Sine,
        rate: 5.0,
        beats: 0.0,
        retrigger: true,
        phase: 0.0,
        fade_in: 0.0,
    };

    /// An oscillator of `shape` at `rate` hertz that starts again with
    /// every note.
    pub const fn new(shape: LfoShape, rate: f64) -> Self {
        Self {
            shape,
            rate,
            ..Self::DEFAULT
        }
    }

    /// An oscillator of `shape` locked to the tempo, one cycle every
    /// `beats` quarter notes, that starts again with every note.
    pub const fn synced(shape: LfoShape, beats: f64) -> Self {
        Self {
            shape,
            beats,
            ..Self::DEFAULT
        }
    }

    /// The same oscillator with its depth rising over `seconds` from the
    /// start of a note.
    pub const fn fading_in(self, seconds: f64) -> Self {
        Self {
            fade_in: seconds,
            ..self
        }
    }

    pub(super) fn limited(self) -> Self {
        let d = Self::DEFAULT;
        Self {
            shape: self.shape,
            rate: setting(self.rate, 0.01, 50.0, d.rate),
            beats: setting(self.beats, 0.0, 64.0, d.beats),
            retrigger: self.retrigger,
            phase: setting(self.phase, 0.0, 1.0, d.phase),
            fade_in: setting(self.fade_in, 0.0, 10.0, d.fade_in),
        }
    }
}

impl Default for LfoParams {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Where a modulation comes from. Each runs from 0 to 1 or from -1 to 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModSource {
    /// Nothing: an empty slot, or, as a slot's `via`, no scaling.
    #[default]
    Off,
    /// Envelope 1, the amplifier's, 0 to 1.
    Env1,
    /// Envelope 2, the filters', 0 to 1.
    Env2,
    /// Envelope 3, 0 to 1.
    Env3,
    /// Envelope 4, 0 to 1.
    Env4,
    /// LFO 1, -1 to 1.
    Lfo1,
    /// LFO 2, -1 to 1.
    Lfo2,
    /// LFO 3, -1 to 1.
    Lfo3,
    /// The note's velocity, 0 to 1.
    Velocity,
    /// 1 for an accented note, 0 otherwise.
    Accent,
    /// Pressure on the key: channel or polyphonic aftertouch, or the note's
    /// pressure expression, whichever is highest, 0 to 1.
    Aftertouch,
    /// The modulation wheel (controller 1), or the note's vibrato
    /// expression, whichever is highest, 0 to 1.
    ModWheel,
    /// The key, -1 at key 0, 0 at middle C (60), 1 at key 120.
    Key,
    /// A random level drawn for each note, repeatably, -1 to 1.
    Random,
    /// Timbre: controller 74, or the note's brightness expression, 0 to 1.
    Brightness,
}

impl ModSource {
    /// Every source, in the order of their indices.
    pub const ALL: [Self; 15] = [
        Self::Off,
        Self::Env1,
        Self::Env2,
        Self::Env3,
        Self::Env4,
        Self::Lfo1,
        Self::Lfo2,
        Self::Lfo3,
        Self::Velocity,
        Self::Accent,
        Self::Aftertouch,
        Self::ModWheel,
        Self::Key,
        Self::Random,
        Self::Brightness,
    ];

    /// The source's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Where a modulation goes, and what its amount means there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ModDestination {
    /// Nowhere: an empty slot.
    #[default]
    Off,
    /// The pitch of every oscillator, in semitones.
    Pitch,
    /// The pitch of oscillator 1 (and of the sub oscillator), in semitones.
    Osc1Pitch,
    /// The pitch of oscillator 2, in semitones.
    Osc2Pitch,
    /// The pitch of oscillator 3, in semitones.
    Osc3Pitch,
    /// Added to the level of oscillator 1.
    Osc1Level,
    /// Added to the level of oscillator 2.
    Osc2Level,
    /// Added to the level of oscillator 3.
    Osc3Level,
    /// Added to the level of the sub oscillator.
    SubLevel,
    /// Added to the level of the noise.
    NoiseLevel,
    /// Added to the shape of oscillator 1: pulse width or table position.
    Osc1Shape,
    /// Added to the shape of oscillator 2.
    Osc2Shape,
    /// Added to the shape of oscillator 3.
    Osc3Shape,
    /// Added to the FM index of oscillator 1, in radians.
    Osc1Fm,
    /// Added to the FM index of oscillator 2, in radians.
    Osc2Fm,
    /// Added to the FM index of oscillator 3, in radians.
    Osc3Fm,
    /// Added to the unison detune of every oscillator.
    Detune,
    /// The cutoff of both filters, in octaves.
    Cutoff,
    /// The cutoff of filter 1, in octaves.
    Cutoff1,
    /// The cutoff of filter 2, in octaves.
    Cutoff2,
    /// Added to the resonance of both filters.
    Resonance,
    /// Added to the morph of both filters.
    Morph,
    /// The drive of both filters, in decibels.
    Drive,
    /// The voice's level, in decibels.
    Volume,
    /// Added to the voice's place in the stereo field.
    Pan,
    /// The rate of LFO 1, in octaves.
    Lfo1Rate,
    /// The rate of LFO 2, in octaves.
    Lfo2Rate,
    /// The rate of LFO 3, in octaves.
    Lfo3Rate,
}

impl ModDestination {
    /// Every destination, in the order of their indices.
    pub const ALL: [Self; 28] = [
        Self::Off,
        Self::Pitch,
        Self::Osc1Pitch,
        Self::Osc2Pitch,
        Self::Osc3Pitch,
        Self::Osc1Level,
        Self::Osc2Level,
        Self::Osc3Level,
        Self::SubLevel,
        Self::NoiseLevel,
        Self::Osc1Shape,
        Self::Osc2Shape,
        Self::Osc3Shape,
        Self::Osc1Fm,
        Self::Osc2Fm,
        Self::Osc3Fm,
        Self::Detune,
        Self::Cutoff,
        Self::Cutoff1,
        Self::Cutoff2,
        Self::Resonance,
        Self::Morph,
        Self::Drive,
        Self::Volume,
        Self::Pan,
        Self::Lfo1Rate,
        Self::Lfo2Rate,
        Self::Lfo3Rate,
    ];

    /// The destination's place in [`ALL`](Self::ALL).
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// One slot of the modulation matrix: `source × via × amount` is added to
/// `destination`, every sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModSlot {
    /// Where the modulation comes from.
    pub source: ModSource,
    /// A second source that scales the first, such as the modulation wheel
    /// deepening a vibrato; [`ModSource::Off`] scales by 1.
    pub via: ModSource,
    /// Where it goes.
    pub destination: ModDestination,
    /// How much, in the destination's unit, -48 to 48.
    pub amount: f64,
}

impl ModSlot {
    /// An empty slot.
    pub const EMPTY: Self = Self {
        source: ModSource::Off,
        via: ModSource::Off,
        destination: ModDestination::Off,
        amount: 0.0,
    };

    /// `source` to `destination` by `amount`.
    pub const fn new(source: ModSource, destination: ModDestination, amount: f64) -> Self {
        Self {
            source,
            via: ModSource::Off,
            destination,
            amount,
        }
    }

    /// The same slot scaled by `via`.
    pub const fn via(self, via: ModSource) -> Self {
        Self { via, ..self }
    }

    /// Whether the slot does anything.
    pub fn is_active(&self) -> bool {
        self.source != ModSource::Off
            && self.destination != ModDestination::Off
            && self.amount != 0.0
    }

    fn limited(self) -> Self {
        Self {
            amount: setting(self.amount, -48.0, 48.0, 0.0),
            ..self
        }
    }
}

impl Default for ModSlot {
    fn default() -> Self {
        Self::EMPTY
    }
}

/// A matrix with `slots` first and the rest empty; slots past
/// [`MOD_SLOTS`] are left out. For writing presets as constants.
pub const fn matrix(slots: &[ModSlot]) -> [ModSlot; MOD_SLOTS] {
    let mut out = [ModSlot::EMPTY; MOD_SLOTS];
    let mut index = 0;
    while index < slots.len() && index < MOD_SLOTS {
        out[index] = slots[index];
        index += 1;
    }
    out
}

/// How a voice plays notes that overlap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VoiceMode {
    /// Every note has a voice of its own.
    #[default]
    Poly,
    /// One voice: a new note takes it over, glides from the old pitch and
    /// strikes the envelopes again from where they are.
    Mono,
    /// One voice: a note struck while another is held glides to its pitch
    /// without striking the envelopes again.
    Legato,
}

impl VoiceMode {
    /// Every mode, in the order of their indices.
    pub const ALL: [Self; 3] = [Self::Poly, Self::Mono, Self::Legato];
}

/// Everything a [`PolySynthVoice`](super::PolySynthVoice) is set to.
///
/// The signal path: three [oscillators](OscParams), a
/// [sub oscillator](SubParams) and [noise](NoiseParams) are mixed and go
/// through two [filters](FilterParams), in series or in parallel, then a
/// saturator and the amplifier, which envelope 1 opens. Four
/// [envelopes](EnvParams), three [LFOs](LfoParams), the note and the
/// player's controllers modulate it through the sixteen slots of the
/// [matrix](ModSlot).
///
/// Every value out of range is limited and one that is not a number takes
/// a default, so no setting makes a voice put out anything but finite
/// numbers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PolySynthParams {
    /// The three oscillators.
    pub oscillators: [OscParams; OSCILLATORS],
    /// The sub oscillator.
    pub sub: SubParams,
    /// The noise.
    pub noise: NoiseParams,
    /// The two filters.
    pub filters: [FilterParams; FILTERS],
    /// How the filters are connected.
    pub routing: FilterRouting,
    /// The four envelopes: the amplifier's, the filters', and two free.
    pub envelopes: [EnvParams; ENVELOPES],
    /// The three LFOs.
    pub lfos: [LfoParams; LFOS],
    /// The modulation matrix.
    pub matrix: [ModSlot; MOD_SLOTS],
    /// How overlapping notes are played.
    pub mode: VoiceMode,
    /// How long a glide from one note to the next takes, in seconds, 0 to
    /// 5, in [mono](VoiceMode::Mono) and [legato](VoiceMode::Legato) modes.
    /// The pitch moves in a straight line in semitones and arrives in this
    /// time whatever the interval.
    pub glide: f64,
    /// How much the level follows the velocity, 0 to 1: at 1 the softest
    /// note is 24 dB down.
    pub velocity: f64,
    /// How much louder an accented note is, in decibels, 0 to 12.
    pub accent_db: f64,
    /// A saturator after the filters, 0 (none) to 1 (24 dB of drive).
    pub saturation: f64,
    /// Where the voice sits in the stereo field, -1 to 1.
    pub pan: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl PolySynthParams {
    /// The starting point: one sawtooth through an open ladder filter.
    pub const INIT: Self = Self {
        oscillators: [OscParams::SAW, OscParams::OFF, OscParams::OFF],
        sub: SubParams::OFF,
        noise: NoiseParams::OFF,
        filters: [
            FilterParams::new(FilterModel::LadderLp24, 8_000.0, 0.1),
            FilterParams::OFF,
        ],
        routing: FilterRouting::Serial,
        envelopes: [
            EnvParams::AMP,
            EnvParams::adsr(0.003, 0.4, 0.3, 0.2),
            EnvParams::adsr(0.003, 0.5, 0.0, 0.3),
            EnvParams::adsr(0.003, 1.0, 0.0, 0.5),
        ],
        lfos: [
            LfoParams::DEFAULT,
            LfoParams::new(LfoShape::Triangle, 0.5),
            LfoParams::new(LfoShape::Sine, 0.2),
        ],
        matrix: [ModSlot::EMPTY; MOD_SLOTS],
        mode: VoiceMode::Poly,
        glide: 0.0,
        velocity: 0.5,
        accent_db: 3.0,
        saturation: 0.0,
        pan: 0.0,
        level: 0.5,
    };

    /// The settings with every value limited to its range.
    pub(super) fn limited(&self) -> Self {
        let d = Self::INIT;
        Self {
            oscillators: self.oscillators.map(OscParams::limited),
            sub: self.sub.limited(),
            noise: self.noise.limited(),
            filters: self.filters.map(FilterParams::limited),
            routing: self.routing,
            envelopes: self.envelopes.map(EnvParams::limited),
            lfos: self.lfos.map(LfoParams::limited),
            matrix: self.matrix.map(ModSlot::limited),
            mode: self.mode,
            glide: setting(self.glide, 0.0, 5.0, d.glide),
            velocity: setting(self.velocity, 0.0, 1.0, d.velocity),
            accent_db: setting(self.accent_db, 0.0, 12.0, d.accent_db),
            saturation: setting(self.saturation, 0.0, 1.0, d.saturation),
            pan: setting(self.pan, -1.0, 1.0, d.pan),
            level: setting(self.level, 0.0, 4.0, d.level),
        }
    }
}

impl Default for PolySynthParams {
    fn default() -> Self {
        Self::INIT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn garbage_is_limited_and_not_a_number_takes_the_default() {
        let mut wild = PolySynthParams::INIT;
        wild.oscillators[0] = OscParams {
            level: f64::NAN,
            semitones: 1e9,
            unison: 200,
            detune: f64::NEG_INFINITY,
            fm: f64::INFINITY,
            ..OscParams::SAW
        };
        wild.filters[1].cutoff = f64::NAN;
        wild.envelopes[2].attack = -4.0;
        wild.lfos[1].rate = 1e12;
        wild.matrix[3] = ModSlot::new(ModSource::Lfo1, ModDestination::Pitch, f64::NAN);
        wild.level = f64::INFINITY;
        let limited = wild.limited();
        let osc = limited.oscillators[0];
        assert_eq!(osc.level, 1.0);
        assert_eq!(osc.semitones, 48.0);
        assert_eq!(usize::from(osc.unison), MAX_UNISON);
        assert_eq!(osc.detune, 0.0);
        assert_eq!(osc.fm, 16.0);
        assert_eq!(limited.filters[1].cutoff, FilterParams::LOWPASS.cutoff);
        assert_eq!(limited.envelopes[2].attack, 0.0);
        assert_eq!(limited.lfos[1].rate, 50.0);
        assert_eq!(limited.matrix[3].amount, 0.0);
        assert!(!limited.matrix[3].is_active());
        assert_eq!(limited.level, 4.0);
        // Limiting what is already limited changes nothing.
        assert_eq!(limited.limited(), limited);
        assert_eq!(PolySynthParams::INIT.limited(), PolySynthParams::INIT);
    }

    #[test]
    fn the_lists_are_complete_and_in_index_order() {
        for (index, source) in ModSource::ALL.into_iter().enumerate() {
            assert_eq!(source.index(), index);
        }
        for (index, destination) in ModDestination::ALL.into_iter().enumerate() {
            assert_eq!(destination.index(), index);
        }
        for (index, table) in TableShape::ALL.into_iter().enumerate() {
            assert_eq!(table.index(), index);
        }
        for (index, model) in FilterModel::ALL.into_iter().enumerate() {
            assert_eq!(model as usize, index);
        }
        let built = matrix(&[ModSlot::new(ModSource::Lfo1, ModDestination::Pitch, 0.2)]);
        assert!(built[0].is_active());
        assert!(built[1..].iter().all(|slot| !slot.is_active()));
    }
}
