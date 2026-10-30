//! Twelve-note temperaments of the octave.

/// The pure fifth, 3/2, in cents.
fn pure_fifth() -> f64 {
    1_200.0 * 1.5f64.log2()
}

/// The Pythagorean comma, the amount by which twelve pure fifths overshoot
/// seven octaves (3¹² / 2¹⁹), in cents: about 23.460.
pub(crate) fn pythagorean_comma() -> f64 {
    12.0 * pure_fifth() - 7.0 * 1_200.0
}

/// The syntonic comma, 81/80, by which four pure fifths overshoot a pure
/// major third two octaves up, in cents: about 21.506.
pub(crate) fn syntonic_comma() -> f64 {
    1_200.0 * (81.0f64 / 80.0).log2()
}

/// The schisma, the Pythagorean comma less the syntonic comma, in cents:
/// about 1.954.
pub(crate) fn schisma() -> f64 {
    pythagorean_comma() - syntonic_comma()
}

/// How a temperament divides the octave into twelve notes: the cents of
/// each degree above the root, the root itself being 0.
///
/// A temperament says nothing about absolute pitch or which note is the
/// root. [`TuningSettings`](super::TuningSettings) puts the root on a pitch
/// class (its key) and the whole on a reference pitch. Degrees count
/// semitones up from the root, so with the root on C, degree 9 is A.
///
/// The historical temperaments are built here from their definitions, as
/// a chain of eleven fifths each narrowed from pure by a stated fraction of
/// a comma, so the numbers are exact rather than copied from a table to a
/// few decimals. The tests compare them with the published cent values.
///
/// ```
/// use manguebyte_dsp::tuning::Temperament;
///
/// let werckmeister = Temperament::werckmeister_iii();
/// // E, four steps up from C: 390.225 cents, not equal temperament's 400.
/// assert!((werckmeister.cents()[4] - 390.225).abs() < 0.001);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Temperament {
    cents: [f64; 12],
}

/// Pitch classes the chains of fifths start from.
const C: usize = 0;
const E_FLAT: usize = 3;
const F: usize = 5;

impl Temperament {
    /// Twelve-tone equal temperament: every semitone 100 cents.
    pub fn equal() -> Self {
        let mut cents = [0.0; 12];
        for (degree, value) in cents.iter_mut().enumerate() {
            *value = 100.0 * degree as f64;
        }
        Self { cents }
    }

    /// Pythagorean tuning: eleven pure fifths from E♭ to G♯, so the fifth
    /// from G♯ back to E♭ is narrow by the whole Pythagorean comma (the
    /// wolf). The usual chain of medieval and early Renaissance keyboards;
    /// see J. Murray Barbour, *Tuning and Temperament: A Historical Survey*
    /// (1951), for this and the temperaments below.
    pub fn pythagorean() -> Self {
        Self::from_fifths(E_FLAT, [pure_fifth(); 11])
    }

    /// Quarter-comma meantone (Pietro Aron, 1523): eleven fifths from E♭ to
    /// G♯, each narrowed by a quarter of the syntonic comma, so that four of
    /// them make a pure major third (5/4). The wolf falls between G♯ and
    /// E♭.
    pub fn quarter_comma_meantone() -> Self {
        Self::from_fifths(E_FLAT, [pure_fifth() - syntonic_comma() / 4.0; 11])
    }

    /// Werckmeister III (Andreas Werckmeister, *Musicalische Temperatur*,
    /// 1691), the first of his "correct" temperaments: the fifths C–G, G–D,
    /// D–A and B–F♯ narrowed by a quarter of the Pythagorean comma each,
    /// every other fifth pure.
    pub fn werckmeister_iii() -> Self {
        let quarter = pure_fifth() - pythagorean_comma() / 4.0;
        let pure = pure_fifth();
        // C-G G-D D-A A-E E-B B-F# F#-C# C#-G# G#-Eb Eb-Bb Bb-F
        Self::from_fifths(
            C,
            [
                quarter, quarter, quarter, pure, pure, quarter, pure, pure, pure, pure, pure,
            ],
        )
    }

    /// Kirnberger III (Johann Philipp Kirnberger, letter to Forkel, 1779;
    /// the form most often played): the fifths C–G, G–D, D–A and A–E
    /// narrowed by a quarter of the syntonic comma each, so C–E is a pure
    /// major third, the fifth F♯–C♯ narrowed by the schisma, every other
    /// fifth pure.
    pub fn kirnberger_iii() -> Self {
        let quarter = pure_fifth() - syntonic_comma() / 4.0;
        let pure = pure_fifth();
        let schismatic = pure_fifth() - schisma();
        // C-G G-D D-A A-E E-B B-F# F#-C# C#-G# G#-Eb Eb-Bb Bb-F
        Self::from_fifths(
            C,
            [
                quarter, quarter, quarter, quarter, pure, pure, schismatic, pure, pure, pure, pure,
            ],
        )
    }

    /// Vallotti's temperament (Francescantonio Vallotti, described in
    /// *Della scienza teorica e pratica della moderna musica*, 1779): the
    /// six fifths F–C, C–G, G–D, D–A, A–E and E–B narrowed by a sixth of
    /// the Pythagorean comma each, the other six pure.
    pub fn vallotti() -> Self {
        let sixth = pure_fifth() - pythagorean_comma() / 6.0;
        let pure = pure_fifth();
        // F-C C-G G-D D-A A-E E-B B-F# F#-C# C#-G# G#-Eb Eb-Bb
        Self::from_fifths(
            F,
            [
                sixth, sixth, sixth, sixth, sixth, sixth, pure, pure, pure, pure, pure,
            ],
        )
    }

    /// Thomas Young's second temperament (*Outlines of experiments and
    /// inquiries respecting sound and light*, Philosophical Transactions of
    /// the Royal Society, 1800): the six fifths C–G, G–D, D–A, A–E, E–B and
    /// B–F♯ narrowed by a sixth of the Pythagorean comma each, the other six
    /// pure. It is Vallotti's moved up a fifth; this is the "Young" of
    /// Caecilia 1.x.
    pub fn young() -> Self {
        let sixth = pure_fifth() - pythagorean_comma() / 6.0;
        let pure = pure_fifth();
        // C-G G-D D-A A-E E-B B-F# F#-C# C#-G# G#-Eb Eb-Bb Bb-F
        Self::from_fifths(
            C,
            [
                sixth, sixth, sixth, sixth, sixth, sixth, pure, pure, pure, pure, pure,
            ],
        )
    }

    /// Five-limit just intonation on the root: the ratios 1/1, 16/15, 9/8,
    /// 6/5, 5/4, 4/3, 45/32, 3/2, 8/5, 5/3, 9/5 and 15/8.
    ///
    /// The seven notes of the major scale are Ptolemy's intense diatonic,
    /// the notes of pure major triads on the root, its fifth and its fourth
    /// (1/1, 9/8, 5/4, 4/3, 3/2, 5/3, 15/8); the other five are the 5-limit
    /// intervals usually given for them: the minor second 16/15, minor third
    /// 6/5, augmented fourth 45/32 (a major third over the second), minor
    /// sixth 8/5 and minor seventh 9/5 (a minor third over the fifth). Any
    /// other choice is a [`from_cents`](Self::from_cents) or a Scala file
    /// away.
    pub fn just() -> Self {
        const RATIOS: [(f64, f64); 12] = [
            (1.0, 1.0),
            (16.0, 15.0),
            (9.0, 8.0),
            (6.0, 5.0),
            (5.0, 4.0),
            (4.0, 3.0),
            (45.0, 32.0),
            (3.0, 2.0),
            (8.0, 5.0),
            (5.0, 3.0),
            (9.0, 5.0),
            (15.0, 8.0),
        ];
        let mut cents = [0.0; 12];
        for (value, (numerator, denominator)) in cents.iter_mut().zip(RATIOS) {
            *value = 1_200.0 * (numerator / denominator).log2();
        }
        Self { cents }
    }

    /// A temperament given by the cents of each degree above the root. The
    /// first entry is the root's own and is taken off every entry, so it
    /// may be anything.
    pub fn from_cents(cents: [f64; 12]) -> Self {
        let root = cents[0];
        Self {
            cents: cents.map(|value| value - root),
        }
    }

    /// A temperament given by how far each degree lies from equal
    /// temperament, in cents, the root first: the form Caecilia 1.x's
    /// tables and many published ones take.
    pub fn from_deviations(deviations: [f64; 12]) -> Self {
        let mut cents = [0.0; 12];
        for (degree, (value, deviation)) in cents.iter_mut().zip(deviations).enumerate() {
            *value = 100.0 * degree as f64 + deviation;
        }
        Self::from_cents(cents)
    }

    /// The cents of each degree above the root, the root (0) first.
    pub fn cents(&self) -> [f64; 12] {
        self.cents
    }

    /// How far each degree lies from equal temperament, in cents, the root
    /// (0) first.
    pub fn deviations(&self) -> [f64; 12] {
        let mut deviations = self.cents;
        for (degree, value) in deviations.iter_mut().enumerate() {
            *value -= 100.0 * degree as f64;
        }
        deviations
    }

    /// The cents of any degree above the root, octaves included: degree 12
    /// is 1 200 cents, degree -1 the seventh degree an octave down.
    pub(crate) fn degree_cents(&self, degree: i64) -> f64 {
        let octave = degree.div_euclid(12);
        let step = degree.rem_euclid(12) as usize;
        1_200.0 * octave as f64 + self.cents[step]
    }

    /// Lays out a chain of eleven fifths (`fifths`, in cents) upwards from
    /// pitch class `start`, folds every note into one octave and measures
    /// it from C.
    fn from_fifths(start: usize, fifths: [f64; 11]) -> Self {
        let mut positions = [0.0; 12];
        let mut class = start;
        let mut position = 0.0;
        for fifth in fifths {
            position += fifth;
            class = (class + 7) % 12;
            positions[class] = position;
        }
        let root = positions[C];
        let mut cents = [0.0; 12];
        for (value, position) in cents.iter_mut().zip(positions) {
            *value = (position - root).rem_euclid(1_200.0);
        }
        // The root is exactly 0, and nothing rounds to the top of the octave.
        cents[C] = 0.0;
        Self { cents }
    }
}

impl Default for Temperament {
    /// Equal temperament.
    fn default() -> Self {
        Self::equal()
    }
}
