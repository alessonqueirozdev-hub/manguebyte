//! Tuning: the frequency each key sounds.
//!
//! A [`Tuning`] maps the 128 MIDI keys to frequencies, and fractional keys
//! to frequencies between theirs. It is built once, off the audio thread,
//! from [`TuningSettings`]:
//!
//! - **a scale** ([`Scale`]): an equal division of the octave into any
//!   number of steps (12, 19, 24, 31, 53...) or of any other period; a
//!   twelve-note [`Temperament`] (Pythagorean, quarter-comma meantone,
//!   Werckmeister III, Kirnberger III, Vallotti, Young, just intonation, or
//!   any table of cents) on a chosen key; or a Scala scale
//!   ([`ScalaScale`], a `.scl` file) with a keyboard mapping
//!   ([`KeyboardMapping`], a `.kbm` file);
//! - **a reference**: which key sounds which frequency (A4 at 440 Hz, 415 Hz
//!   for baroque pitch, any key at any frequency);
//! - **a transposition** in keys and **a detune** in cents;
//! - **a [`Stretch`]**: octaves widened towards the ends of the keyboard,
//!   as pianos and some organs are tuned.
//!
//! A tuning can also come from the MIDI Tuning Standard ([`MtsMessage`]),
//! or from 128 pitches or frequencies given directly.
//!
//! Querying is cheap and allocates nothing: a key's frequency is a table
//! lookup, a fractional key's one interpolation and one `exp2`, the same
//! cost as equal temperament's formula. A tuning holds no heap memory, so
//! one can be sent to the audio thread through a queue, given to a
//! [`Synth`](crate::Synth) with [`set_tuning`](crate::Synth::set_tuning)
//! (which copies it into every voice) and dropped there:
//!
//! ```
//! use manguebyte_core::spsc;
//! use manguebyte_dsp::tuning::{Scale, Temperament, Tuning, TuningSettings};
//!
//! let (mut to_audio, mut from_ui) = spsc::channel::<Tuning>(2);
//!
//! // Off the audio thread: Werckmeister III on C, at baroque pitch.
//! let settings = TuningSettings {
//!     scale: Scale::Temperament { temperament: Temperament::werckmeister_iii(), key: 0 },
//!     reference_hz: 415.0,
//!     ..TuningSettings::default()
//! };
//! to_audio.push(settings.build()?).unwrap();
//!
//! // On the audio thread, at the start of a block.
//! if let Some(tuning) = from_ui.pop() {
//!     assert!((tuning.key_frequency(69) - 415.0).abs() < 1e-9);
//!     // synth.set_tuning(&tuning);
//! }
//! # Ok::<(), manguebyte_dsp::tuning::TuningError>(())
//! ```
//!
//! # Units
//!
//! A tuning stores each key's pitch as an equal-tempered note number: the
//! key whose 12-tone equal temperament it would be, with A4 at 440 Hz, so
//! 69.0 is 440 Hz and 69.5 is a quarter tone above. That is the unit the
//! MIDI Tuning Standard sends, and it makes the default tuning exactly the
//! key numbers themselves: [`Tuning::default`], 12-tone equal temperament
//! with A4 at 440 Hz, gives the frequency `440 · 2^((note − 69) / 12)`
//! bit for bit, for every note, whole or not.
//!
//! A fractional note is interpolated linearly in pitch between the two
//! keys around it, and beyond keys 0 and 127 the pitch carries on with the
//! slope of the last step: in 19-tone equal temperament, note 69.5 lies
//! half a nineteenth of an octave above key 69.
//!
//! The instruments of this crate and the sampler take a key's pitch from
//! the tuning ([`Tuning::key_pitch`]) and add pitch bend, glides and their
//! own transpositions on top as intervals in semitones, as synthesizers
//! that follow the MIDI Tuning Standard do, so a bend of two semitones is
//! two equal-tempered semitones in any tuning. A voice that would rather
//! bend through the tuning's keys uses [`Tuning::frequency`] of the bent
//! key. See [`Voice::set_tuning`](crate::Voice::set_tuning).

mod mts;
mod scala;
mod temperament;

use core::fmt;

pub use mts::{MtsError, MtsMessage, mts_pitch_bytes};
pub use scala::{KeyboardMapping, ScalaError, ScalaErrorKind, ScalaScale};
pub use temperament::Temperament;

/// How many keys a tuning has: the MIDI keys 0 to 127.
pub const KEYS: usize = 128;

/// Every key mapped.
const ALL_KEYS: u128 = u128::MAX;

/// The frequency of a pitch given as an equal-tempered note number.
#[inline]
fn pitch_hz(pitch: f64) -> f64 {
    440.0 * ((pitch - 69.0) / 12.0).exp2()
}

/// The equal-tempered note number of a frequency.
#[inline]
fn hz_pitch(hz: f64) -> f64 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// Which frequency each of the 128 MIDI keys sounds, and which keys sound
/// at all.
///
/// Built off the audio thread ([`TuningSettings::build`],
/// [`from_pitches`](Self::from_pitches), [`MtsMessage::apply`]) and never
/// changed after: to retune, build another. It owns no heap memory, so
/// cloning, sending and dropping one is real-time safe. Every query is
/// real-time safe. See the [module documentation](self) for the units and
/// how fractional notes are treated.
///
/// ```
/// use manguebyte_dsp::tuning::{Scale, Tuning, TuningSettings};
///
/// let standard = Tuning::default();
/// assert_eq!(standard.key_frequency(69), 440.0);
/// assert_eq!(standard.frequency(60.0), 440.0 * ((60.0 - 69.0) / 12.0f64).exp2());
///
/// // 19-tone equal temperament: each key is a nineteenth of an octave.
/// let nineteen = TuningSettings { scale: Scale::equal(19), ..TuningSettings::default() }.build()?;
/// assert!((nineteen.key_frequency(69 + 19) - 880.0).abs() < 1e-9);
/// # Ok::<(), manguebyte_dsp::tuning::TuningError>(())
/// ```
#[derive(Clone, PartialEq)]
pub struct Tuning {
    /// Each key's pitch, as an equal-tempered note number.
    pitch: [f64; KEYS],
    /// Each key's frequency, in hertz: `pitch_hz` of its pitch.
    hz: [f64; KEYS],
    /// Bit `k` set when key `k` sounds.
    mapped: u128,
}

impl Default for Tuning {
    /// 12-tone equal temperament with A4 (key 69) at 440 Hz, every key
    /// mapped.
    fn default() -> Self {
        let mut pitch = [0.0; KEYS];
        for (key, value) in pitch.iter_mut().enumerate() {
            *value = key as f64;
        }
        Self::from_parts(pitch, ALL_KEYS)
    }
}

impl fmt::Debug for Tuning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tuning")
            .field("c4_hz", &self.hz[60])
            .field("a4_hz", &self.hz[69])
            .field("mapped_keys", &self.mapped.count_ones())
            .finish_non_exhaustive()
    }
}

impl Tuning {
    /// A tuning from each key's pitch as an equal-tempered note number (see
    /// [the units](self#units)), every key mapped.
    pub fn from_pitches(pitches: &[f64; KEYS]) -> Result<Self, TuningError> {
        Self::checked(*pitches, ALL_KEYS)
    }

    /// A tuning from each key's frequency in hertz, every key mapped.
    pub fn from_frequencies(frequencies: &[f64; KEYS]) -> Result<Self, TuningError> {
        let mut pitch = [0.0; KEYS];
        for (key, (value, &hz)) in pitch.iter_mut().zip(frequencies).enumerate() {
            if !(hz.is_finite() && hz > 0.0) {
                return Err(TuningError::NotFinite { key: key as u8 });
            }
            *value = hz_pitch(hz);
        }
        Self::checked(pitch, ALL_KEYS)
    }

    /// The frequency of a note in hertz. A whole note is its key's; a
    /// fractional one lies between, interpolated in pitch (see
    /// [the units](self#units)). Real-time safe.
    #[inline]
    pub fn frequency(&self, note: f64) -> f64 {
        pitch_hz(self.pitch(note))
    }

    /// The pitch of a note, as an equal-tempered note number: what
    /// [`frequency`](Self::frequency) gives, before it becomes hertz. Equal
    /// temperament with A4 at 440 Hz gives `note` itself. Real-time safe.
    #[inline]
    pub fn pitch(&self, note: f64) -> f64 {
        let pitch = &self.pitch;
        if (0.0..127.0).contains(&note) {
            let index = note as usize;
            let fraction = note - index as f64;
            pitch[index] + (pitch[index + 1] - pitch[index]) * fraction
        } else if note >= 127.0 {
            pitch[127] + (note - 127.0) * (pitch[127] - pitch[126])
        } else {
            // Below key 0, and not a number.
            pitch[0] + note * (pitch[1] - pitch[0])
        }
    }

    /// The frequency of a key in hertz, from the table. Keys above 127 are
    /// taken as 127. Real-time safe.
    #[inline]
    pub fn key_frequency(&self, key: u8) -> f64 {
        self.hz[usize::from(key.min(127))]
    }

    /// The pitch of a key as an equal-tempered note number. Keys above 127
    /// are taken as 127. Real-time safe.
    #[inline]
    pub fn key_pitch(&self, key: u8) -> f64 {
        self.pitch[usize::from(key.min(127))]
    }

    /// How many cents a key lies from its own pitch in 12-tone equal
    /// temperament with A4 at 440 Hz.
    pub fn key_cents_from_standard(&self, key: u8) -> f64 {
        let key = key.min(127);
        100.0 * (self.pitch[usize::from(key)] - f64::from(key))
    }

    /// Whether a key sounds. A keyboard mapping can leave keys unmapped
    /// (`x` in a `.kbm` file), and [`Synth`](crate::Synth) does not start
    /// notes on them. Their frequency is still defined, between the mapped
    /// keys around them, so that a bend across one stays continuous. Keys
    /// above 127 are taken as 127.
    #[inline]
    pub fn is_mapped(&self, key: u8) -> bool {
        self.mapped & (1 << key.min(127)) != 0
    }

    /// How many of the 128 keys sound.
    pub fn mapped_keys(&self) -> usize {
        self.mapped.count_ones() as usize
    }

    /// Every key's frequency in hertz, key 0 first.
    pub fn frequencies(&self) -> &[f64; KEYS] {
        &self.hz
    }

    /// Every key's pitch as an equal-tempered note number, key 0 first.
    pub fn pitches(&self) -> &[f64; KEYS] {
        &self.pitch
    }

    /// This tuning with `stretch` added on top. See [`Stretch`].
    pub fn stretched(&self, stretch: &Stretch) -> Result<Self, TuningError> {
        let mut pitch = self.pitch;
        for (key, value) in pitch.iter_mut().enumerate() {
            *value += stretch.cents(key as f64) / 100.0;
        }
        Self::checked(pitch, self.mapped)
    }

    /// Checks that every key has a finite pitch and frequency.
    fn checked(pitch: [f64; KEYS], mapped: u128) -> Result<Self, TuningError> {
        let tuning = Self::from_parts(pitch, mapped);
        for (key, &hz) in tuning.hz.iter().enumerate() {
            if !(hz.is_finite() && hz > 0.0) {
                return Err(TuningError::NotFinite { key: key as u8 });
            }
        }
        Ok(tuning)
    }

    fn from_parts(pitch: [f64; KEYS], mapped: u128) -> Self {
        Self {
            hz: pitch.map(pitch_hz),
            pitch,
            mapped,
        }
    }

    /// Gives one key a pitch and maps it.
    fn set_key_pitch(&mut self, key: usize, pitch: f64) {
        self.pitch[key] = pitch;
        self.hz[key] = pitch_hz(pitch);
        self.mapped |= 1 << key;
    }
}

/// Octaves widened towards the ends of the keyboard: bass keys a little
/// flat, treble keys a little sharp, by an amount that grows in proportion
/// to the distance from a pivot key.
///
/// A key `d` octaves above the pivot (12 keys an octave, whatever the
/// scale) moves `treble_cents_per_octave × d` cents; one `d` octaves below
/// moves down `bass_cents_per_octave × d`. The pivot stays where it is. So
/// each octave above the pivot is wider by the treble amount, each below by
/// the bass amount, a straight-line stand-in for the Railsback curve of a
/// piano's tuning, as Caecilia 1.x had it. It is not a model of string
/// inharmonicity.
///
/// The stretch follows the key played, before any transposition, and is
/// added after everything else; a reference key away from the pivot moves
/// with it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stretch {
    /// Cents each octave below the pivot is wider by; positive makes the
    /// bass flatter.
    pub bass_cents_per_octave: f64,
    /// Cents each octave above the pivot is wider by; positive makes the
    /// treble sharper.
    pub treble_cents_per_octave: f64,
    /// The key that does not move.
    pub pivot: u8,
}

impl Stretch {
    /// No stretch.
    pub const NONE: Self = Self {
        bass_cents_per_octave: 0.0,
        treble_cents_per_octave: 0.0,
        pivot: 69,
    };

    /// The cents a note moves.
    pub fn cents(&self, note: f64) -> f64 {
        let octaves = (note - f64::from(self.pivot)) / 12.0;
        if octaves < 0.0 {
            self.bass_cents_per_octave * octaves
        } else {
            self.treble_cents_per_octave * octaves
        }
    }

    /// Whether this stretch moves nothing.
    pub fn is_none(&self) -> bool {
        self.bass_cents_per_octave == 0.0 && self.treble_cents_per_octave == 0.0
    }
}

impl Default for Stretch {
    fn default() -> Self {
        Self::NONE
    }
}

/// The scale a [`Tuning`] is built on.
#[derive(Debug, Clone, PartialEq)]
pub enum Scale {
    /// `divisions` equal steps to a period of `period_cents`, one step per
    /// key. A period of 1 200 cents is the octave: 12 divisions are the
    /// usual equal temperament, 19, 24, 31 and 53 the common microtonal
    /// ones. Another period gives a non-octave scale, such as the
    /// Bohlen-Pierce scale's 13 steps to the tritave (1 901.955 cents).
    Equal {
        /// Steps to a period, at least 1.
        divisions: u32,
        /// The period, in cents, above 0.
        period_cents: f64,
    },
    /// A twelve-note temperament with its root on pitch class `key`
    /// (0 = C, 1 = C♯, ... 11 = B), repeating every octave.
    Temperament {
        /// The temperament.
        temperament: Temperament,
        /// The pitch class of its root, 0 to 11.
        key: u8,
    },
    /// A Scala scale played through a keyboard mapping.
    ///
    /// A mapping brings its own reference key and frequency, which take the
    /// place of [`TuningSettings::reference_key`] and
    /// [`TuningSettings::reference_hz`]. Without one, the scale is mapped
    /// linearly with degree 0 on key 60 and those settings as the
    /// reference: [`KeyboardMapping::linear`].
    Scala {
        /// The scale.
        scale: ScalaScale,
        /// How the keys play it.
        mapping: Option<KeyboardMapping>,
    },
}

impl Scale {
    /// The octave divided into `divisions` equal steps.
    pub fn equal(divisions: u32) -> Self {
        Self::Equal {
            divisions,
            period_cents: 1_200.0,
        }
    }
}

impl Default for Scale {
    /// 12-tone equal temperament.
    fn default() -> Self {
        Self::equal(12)
    }
}

/// Everything a [`Tuning`] is built from. The default is 12-tone equal
/// temperament with A4 at 440 Hz.
#[derive(Debug, Clone, PartialEq)]
pub struct TuningSettings {
    /// The scale.
    pub scale: Scale,
    /// The key that sounds [`reference_hz`](Self::reference_hz) (before
    /// transposition, detune and stretch). 69, A4, by default.
    pub reference_key: u8,
    /// The frequency of the reference key, in hertz. 440 by default.
    pub reference_hz: f64,
    /// Keys to shift the keyboard by: key `k` plays what key
    /// `k + transpose` would. The scale moves with the keys, as on a
    /// transposing keyboard. 0 by default.
    pub transpose: i32,
    /// Cents to move every key by. 0 by default.
    pub detune_cents: f64,
    /// Octave stretch, added last. None by default.
    pub stretch: Stretch,
}

impl Default for TuningSettings {
    fn default() -> Self {
        Self {
            scale: Scale::default(),
            reference_key: 69,
            reference_hz: 440.0,
            transpose: 0,
            detune_cents: 0.0,
            stretch: Stretch::NONE,
        }
    }
}

/// Why a [`Tuning`] could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TuningError {
    /// The reference frequency is not a finite number above zero, or the
    /// reference key is above 127.
    Reference,
    /// An equal division with no steps, or a period that is not a finite
    /// number of cents above zero.
    Division,
    /// A temperament's key is not a pitch class from 0 to 11.
    Key,
    /// A key's pitch or frequency came out as something other than a
    /// finite, positive number: an extreme stretch or detune, or a
    /// temperament or frequency table holding a NaN.
    NotFinite {
        /// The first such key.
        key: u8,
    },
}

impl fmt::Display for TuningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reference => f.write_str("the reference is not a key and a positive frequency"),
            Self::Division => f.write_str("an equal division needs steps and a positive period"),
            Self::Key => f.write_str("a temperament's key is a pitch class from 0 to 11"),
            Self::NotFinite { key } => write!(f, "key {key} has no finite, positive frequency"),
        }
    }
}

impl std::error::Error for TuningError {}

impl TuningSettings {
    /// Builds the tuning. Allocates nothing, but takes a few hundred
    /// `exp2` and `log2` calls: do it off the audio thread.
    pub fn build(&self) -> Result<Tuning, TuningError> {
        let linear;
        let (reference_key, reference_hz, scale): (u8, f64, Pitches<'_>) = match &self.scale {
            Scale::Equal {
                divisions,
                period_cents,
            } => {
                if *divisions == 0 || !(period_cents.is_finite() && *period_cents > 0.0) {
                    return Err(TuningError::Division);
                }
                (
                    self.reference_key,
                    self.reference_hz,
                    Pitches::Equal {
                        divisions: f64::from(*divisions),
                        period_cents: *period_cents,
                    },
                )
            }
            Scale::Temperament { temperament, key } => {
                if *key > 11 {
                    return Err(TuningError::Key);
                }
                (
                    self.reference_key,
                    self.reference_hz,
                    Pitches::Temperament {
                        temperament,
                        key: i64::from(*key),
                    },
                )
            }
            Scale::Scala { scale, mapping } => {
                let mapping = match mapping {
                    Some(mapping) => mapping,
                    None => {
                        linear = KeyboardMapping::linear(60, self.reference_key, self.reference_hz);
                        &linear
                    }
                };
                (
                    mapping.reference_key(),
                    mapping.reference_hz(),
                    Pitches::Scala { scale, mapping },
                )
            }
        };
        if reference_key > 127 || !(reference_hz.is_finite() && reference_hz > 0.0) {
            return Err(TuningError::Reference);
        }
        let reference_key = i64::from(reference_key);
        let reference_pitch = hz_pitch(reference_hz);
        // Scala mappings always map their reference key; see
        // `KeyboardMapping::parse`.
        let reference_cents = scale.cents(reference_key).unwrap_or(0.0);

        let mut pitch = [0.0; KEYS];
        let mut mapped = 0_u128;
        for (key, value) in pitch.iter_mut().enumerate() {
            let source = key as i64 + i64::from(self.transpose);
            if let Some(standard) = scale.unretuned(source) {
                *value = standard;
                mapped |= 1 << key;
            } else if let Some(cents) = scale.cents(source) {
                *value = reference_pitch + (cents - reference_cents) / 100.0;
                mapped |= 1 << key;
            }
        }
        // Never empty: a mapping maps its reference key, which is on the
        // keyboard, and does not retune keys it leaves out of its range.
        fill_unmapped(&mut pitch, mapped);
        for (key, value) in pitch.iter_mut().enumerate() {
            *value += (self.detune_cents + self.stretch.cents(key as f64)) / 100.0;
        }
        Tuning::checked(pitch, mapped)
    }
}

/// A scale ready to give the pitch of any key.
enum Pitches<'a> {
    Equal {
        divisions: f64,
        period_cents: f64,
    },
    Temperament {
        temperament: &'a Temperament,
        key: i64,
    },
    Scala {
        scale: &'a ScalaScale,
        mapping: &'a KeyboardMapping,
    },
}

impl Pitches<'_> {
    /// The pitch of a key in cents, measured from a point of the scale's
    /// own (only differences between keys matter), or `None` if the key is
    /// unmapped.
    fn cents(&self, key: i64) -> Option<f64> {
        match *self {
            Self::Equal {
                divisions,
                period_cents,
            } => Some(key as f64 * period_cents / divisions),
            Self::Temperament {
                temperament,
                key: root,
            } => Some(temperament.degree_cents(key - root)),
            Self::Scala { scale, mapping } => mapping.key_cents(scale, key),
        }
    }

    /// The pitch of a key the mapping does not retune: its own, in equal
    /// temperament with A4 at 440 Hz. `None` for a key it retunes.
    fn unretuned(&self, key: i64) -> Option<f64> {
        match *self {
            Self::Scala { mapping, .. } if !mapping.retunes(key) => Some(key as f64),
            _ => None,
        }
    }
}

/// Gives each unmapped key a pitch between the mapped keys on either side,
/// in proportion to its place between them, or, beyond the last mapped key
/// on one side, the nearest one's pitch moved a semitone per key.
fn fill_unmapped(pitch: &mut [f64; KEYS], mapped: u128) {
    let is_mapped = |key: usize| mapped & (1 << key) != 0;
    let mut previous: Option<usize> = None;
    for key in 0..KEYS {
        if !is_mapped(key) {
            continue;
        }
        match previous {
            None => {
                for gap in 0..key {
                    pitch[gap] = pitch[key] - (key - gap) as f64;
                }
            }
            Some(before) => {
                let span = (key - before) as f64;
                for gap in before + 1..key {
                    let share = (gap - before) as f64 / span;
                    pitch[gap] = pitch[before] + (pitch[key] - pitch[before]) * share;
                }
            }
        }
        previous = Some(key);
    }
    if let Some(last) = previous {
        for gap in last + 1..KEYS {
            pitch[gap] = pitch[last] + (gap - last) as f64;
        }
    }
}

#[cfg(test)]
mod tests;
