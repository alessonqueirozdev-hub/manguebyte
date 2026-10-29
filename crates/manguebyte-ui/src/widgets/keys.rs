//! Where the keys of a piano keyboard are, shared by the keyboard and by
//! anything laid out above or below it, and what notes are called.

use manguebyte_gfx::Rect;

/// Where each white key starts within an octave, in semitone columns from
/// the C, and where the next starts: C, D and E share the first five
/// columns, F, G, A and B the other seven, as on a piano.
const WHITE_EDGES: [(u8, f32, f32); 7] = [
    (0, 0.0, 5.0 / 3.0),
    (2, 5.0 / 3.0, 10.0 / 3.0),
    (4, 10.0 / 3.0, 5.0),
    (5, 5.0, 6.75),
    (7, 6.75, 8.5),
    (9, 8.5, 10.25),
    (11, 10.25, 12.0),
];

/// Whether `note` is a black key: C#, D#, F#, G# or A#.
pub fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

/// The English name of `note`, a MIDI note number, with middle C (60) as
/// "C4" and sharps for the black keys: "C#4", "A0", "C-1".
///
/// ```
/// use manguebyte_ui::note_name;
///
/// assert_eq!(note_name(60), "C4");
/// assert_eq!(note_name(61), "C#4");
/// assert_eq!(note_name(21), "A0");
/// assert_eq!(note_name(0), "C-1");
/// ```
pub fn note_name(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note % 12)],
        i32::from(note / 12) - 1
    )
}

/// The Portuguese name of `note`, with the same octave numbers as
/// [`note_name`]: "Dó4" for middle C, "Fá#2", "Lá0". For
/// [`PianoKeyboard::note_names`](crate::PianoKeyboard::note_names).
///
/// ```
/// use manguebyte_ui::note_name_portuguese;
///
/// assert_eq!(note_name_portuguese(60), "Dó4");
/// assert_eq!(note_name_portuguese(42), "Fá#2");
/// ```
pub fn note_name_portuguese(note: u8) -> String {
    const NAMES: [&str; 12] = [
        "Dó", "Dó#", "Ré", "Ré#", "Mi", "Fá", "Fá#", "Sol", "Sol#", "Lá", "Lá#", "Si",
    ];
    format!(
        "{}{}",
        NAMES[usize::from(note % 12)],
        i32::from(note / 12) - 1
    )
}

/// A run of notes laid out across a rectangle the way a
/// [`PianoKeyboard`](crate::PianoKeyboard) lays its keys out: each note,
/// black or white, has a column of the same width, and the white keys
/// share the width of an octave as a piano's do. Anything that should
/// line up with a keyboard's keys, such as a
/// [`ZoneMap`](crate::ZoneMap) above it, uses the same span over a
/// rectangle of the same horizontal extent.
///
/// ```
/// use manguebyte_gfx::Rect;
/// use manguebyte_ui::KeySpan;
///
/// // Two octaves from C3, across 240 pixels: 10 pixels a note.
/// let span = KeySpan::new(48, 71);
/// let rect = Rect::new(0.0, 0.0, 240.0, 80.0);
/// assert_eq!(span.len(), 24);
/// assert_eq!(span.column(rect, 49).map(|c| (c.x, c.width)), Some((10.0, 10.0)));
/// assert_eq!(span.note_at(rect, 125.0), Some(60));
/// // The white C takes the first five thirds of a column of its octave.
/// let c = span.white_key(rect, 48).expect("C3 is white");
/// assert!((c.width - 50.0 / 3.0).abs() < 1e-4);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeySpan {
    low: u8,
    high: u8,
}

impl KeySpan {
    /// The 88 keys of a piano: A0 (21) to C8 (108).
    pub const PIANO: KeySpan = KeySpan { low: 21, high: 108 };

    /// The 61 keys of a common controller: C2 (36) to C7 (96).
    pub const KEYS_61: KeySpan = KeySpan { low: 36, high: 96 };

    /// The notes from `low` to `high`, both included, in either order,
    /// within MIDI's 0 to 127.
    pub fn new(low: u8, high: u8) -> KeySpan {
        let (low, high) = (low.min(127), high.min(127));
        KeySpan {
            low: low.min(high),
            high: low.max(high),
        }
    }

    /// `count` notes from `lowest` up, as many as fit below 128.
    pub fn from_lowest(lowest: u8, count: u8) -> KeySpan {
        let lowest = lowest.min(127);
        let highest = u16::from(lowest) + u16::from(count.max(1)) - 1;
        KeySpan::new(lowest, highest.min(127) as u8)
    }

    /// The lowest note.
    pub fn low(&self) -> u8 {
        self.low
    }

    /// The highest note.
    pub fn high(&self) -> u8 {
        self.high
    }

    /// How many notes.
    pub fn len(&self) -> usize {
        usize::from(self.high - self.low) + 1
    }

    /// Never: a span has at least one note.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Whether `note` is in the span.
    pub fn contains(&self, note: u8) -> bool {
        (self.low..=self.high).contains(&note)
    }

    /// The notes, from the lowest.
    pub fn notes(&self) -> std::ops::RangeInclusive<u8> {
        self.low..=self.high
    }

    /// The width of one note's column in `rect`.
    pub fn column_width(&self, rect: Rect) -> f32 {
        rect.width / self.len() as f32
    }

    /// The column of `note` in `rect`, as tall as `rect`: where a black
    /// key is drawn, and where anything about one note lines up with the
    /// keyboard. `None` for a note outside the span.
    pub fn column(&self, rect: Rect, note: u8) -> Option<Rect> {
        if !self.contains(note) {
            return None;
        }
        let width = self.column_width(rect);
        let x = rect.x + f32::from(note - self.low) * width;
        Some(Rect::new(x, rect.y, width, rect.height))
    }

    /// The note whose column is at `x` in `rect`, if any.
    pub fn note_at(&self, rect: Rect, x: f32) -> Option<u8> {
        let width = self.column_width(rect);
        let along = ((x - rect.x) / width).floor();
        if !(along >= 0.0 && along < self.len() as f32) {
            return None;
        }
        Some(self.low + along as u8)
    }

    /// The left and right edges of white `note` in semitone columns from
    /// the span's first column, or `None` for a black note.
    fn white_edges(&self, note: u8) -> Option<(f32, f32)> {
        let (_, from, to) = WHITE_EDGES
            .iter()
            .copied()
            .find(|&(semitone, _, _)| semitone == note % 12)?;
        let octave = f32::from(note / 12 * 12) - f32::from(self.low);
        Some((octave + from, octave + to))
    }

    /// Where white `note` is in `rect`, as tall as `rect`, cut at the
    /// span's ends: `None` for a black note or one outside the span.
    pub fn white_key(&self, rect: Rect, note: u8) -> Option<Rect> {
        if !self.contains(note) {
            return None;
        }
        let (from, to) = self.white_edges(note)?;
        let width = self.column_width(rect);
        let left = (rect.x + from * width).max(rect.x);
        let right = (rect.x + to * width).min(rect.right());
        Some(Rect::new(
            left,
            rect.y,
            (right - left).max(0.0),
            rect.height,
        ))
    }

    /// The white key at `x` in `rect`, if any: the key under a press below
    /// the black keys.
    pub fn white_at(&self, rect: Rect, x: f32) -> Option<u8> {
        let note = self.note_at(rect, x)?;
        [note, note.saturating_sub(1), note.saturating_add(1)]
            .into_iter()
            .filter(|&candidate| !is_black(candidate))
            .find(|&candidate| {
                self.white_key(rect, candidate)
                    .is_some_and(|key| key.x <= x && x < key.right())
            })
    }
}

impl Default for KeySpan {
    fn default() -> Self {
        KeySpan::KEYS_61
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn white_keys_tile_the_span_and_hold_their_own_columns() {
        let span = KeySpan::new(36, 96);
        let rect = Rect::new(10.0, 0.0, 610.0, 50.0);
        let whites: Vec<Rect> = span
            .notes()
            .filter_map(|note| span.white_key(rect, note))
            .collect();
        assert_eq!(whites.len(), 36);
        assert!((whites[0].x - rect.x).abs() < 1e-4);
        for pair in whites.windows(2) {
            assert!((pair[0].right() - pair[1].x).abs() < 1e-3, "{pair:?}");
        }
        for note in span.notes().filter(|&note| !is_black(note)) {
            let column = span.column(rect, note).expect("in the span");
            let key = span.white_key(rect, note).expect("white");
            assert!(key.x <= column.x + 1e-3 && column.right() <= key.right() + 1e-3);
            assert_eq!(span.white_at(rect, column.center().x), Some(note));
        }
    }

    #[test]
    fn a_span_keeps_within_midi() {
        assert_eq!(KeySpan::new(100, 20), KeySpan::new(20, 100));
        assert_eq!(KeySpan::from_lowest(120, 20).high(), 127);
        assert_eq!(KeySpan::PIANO.len(), 88);
        assert_eq!(KeySpan::KEYS_61.len(), 61);
        assert_eq!(
            KeySpan::new(60, 60).note_at(Rect::new(0.0, 0.0, 10.0, 10.0), 10.0),
            None
        );
    }
}
