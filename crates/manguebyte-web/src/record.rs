//! The 32-byte event record shared with JavaScript.

use manguebyte_params::ParamId;

/// Size of one [`EventRecord`] in bytes.
pub const RECORD_SIZE: usize = 32;

/// Byte offsets of the fields inside a record.
///
/// Every multi-byte field is little endian. Bytes 9, 14 and 15 are padding:
/// a writer sets them to zero and a reader ignores them.
///
/// | Bytes  | Field      | Type |
/// |--------|------------|------|
/// | 0..8   | `frame`    | u64  |
/// | 8      | `kind`     | u8   |
/// | 9      | padding    |      |
/// | 10..12 | `part`     | u16  |
/// | 12     | `note`     | u8   |
/// | 13     | `channel`  | u8   |
/// | 14..16 | padding    |      |
/// | 16..20 | `velocity` | f32  |
/// | 20..24 | `param`    | u32  |
/// | 24..32 | `value`    | f64  |
pub mod offset {
    /// The frame the event belongs to, `u64`.
    pub const FRAME: usize = 0;
    /// The kind code, `u8`. See [`RecordKind`](super::RecordKind).
    pub const KIND: usize = 8;
    /// The part, `u16`.
    pub const PART: usize = 10;
    /// The MIDI note number, `u8`.
    pub const NOTE: usize = 12;
    /// The MIDI channel, `u8`.
    pub const CHANNEL: usize = 13;
    /// The velocity, `f32`.
    pub const VELOCITY: usize = 16;
    /// The parameter, `u32`: a [`ParamId`](manguebyte_params::ParamId).
    pub const PARAM: usize = 20;
    /// The parameter value in plain units, `f64`.
    pub const VALUE: usize = 24;
    /// The padding bytes, written as zero and never read.
    pub const PADDING: [usize; 3] = [9, 14, 15];
}

/// What a record asks for, and its numeric code in the `kind` byte.
///
/// Code 0 and codes above 5 are not kinds: a record that carries one is
/// ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// Code 1: start `note` on `channel` with `velocity`.
    NoteOn = 1,
    /// Code 2: release `note` on `channel`.
    NoteOff = 2,
    /// Code 3: set parameter `param` of the part to `value`.
    Param = 3,
    /// Code 4: every note of the part stops at once, with no release.
    Choke = 4,
    /// Code 5: the part releases every note, as if each key were let go.
    AllNotesOff = 5,
}

impl RecordKind {
    /// The kind with this code, if there is one.
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::NoteOn),
            2 => Some(Self::NoteOff),
            3 => Some(Self::Param),
            4 => Some(Self::Choke),
            5 => Some(Self::AllNotesOff),
            _ => None,
        }
    }

    /// The code written in the `kind` byte.
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// One scheduled event as it travels between threads: 32 bytes, laid out
/// as [`offset`] describes.
///
/// A record is plain data; nothing about it is checked until
/// [`decode`](Self::decode) turns it into an [`Event`]. Fields a kind does
/// not use are ignored, so they may hold anything.
///
/// ```
/// use manguebyte_web::{EventRecord, RecordKind};
///
/// let record = EventRecord::note_on(48_000, 2, 60, 0.8);
/// let bytes = record.to_bytes();
/// assert_eq!(bytes.len(), 32);
/// assert_eq!(bytes[8], RecordKind::NoteOn.code());
/// assert_eq!(EventRecord::from_bytes(&bytes), record);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EventRecord {
    /// The frame the event belongs to, counted from the engine's first frame.
    pub frame: u64,
    /// The kind code. See [`RecordKind`].
    pub kind: u8,
    /// The part the event is for.
    pub part: u16,
    /// MIDI note number, 0 to 127. Used by note on and note off.
    pub note: u8,
    /// MIDI channel, 0 to 15. Used by note on and note off.
    pub channel: u8,
    /// Velocity, 0 to 1. Used by note on.
    pub velocity: f32,
    /// The parameter, as a [`ParamId`]. Used by param.
    pub param: u32,
    /// The parameter's new value in its own units. Used by param.
    pub value: f64,
}

impl EventRecord {
    fn of_kind(kind: RecordKind, frame: u64, part: u16) -> Self {
        Self {
            frame,
            kind: kind.code(),
            part,
            ..Self::default()
        }
    }

    /// A note on, on channel 0.
    pub fn note_on(frame: u64, part: u16, note: u8, velocity: f32) -> Self {
        Self {
            note,
            velocity,
            ..Self::of_kind(RecordKind::NoteOn, frame, part)
        }
    }

    /// A note off, on channel 0.
    pub fn note_off(frame: u64, part: u16, note: u8) -> Self {
        Self {
            note,
            ..Self::of_kind(RecordKind::NoteOff, frame, part)
        }
    }

    /// A parameter change.
    pub fn param(frame: u64, part: u16, param: ParamId, value: f64) -> Self {
        Self {
            param: param.0,
            value,
            ..Self::of_kind(RecordKind::Param, frame, part)
        }
    }

    /// Every note of `part` stops at once.
    pub fn choke(frame: u64, part: u16) -> Self {
        Self::of_kind(RecordKind::Choke, frame, part)
    }

    /// `part` releases every note.
    pub fn all_notes_off(frame: u64, part: u16) -> Self {
        Self::of_kind(RecordKind::AllNotesOff, frame, part)
    }

    /// The same record on another MIDI channel.
    #[must_use]
    pub fn with_channel(mut self, channel: u8) -> Self {
        self.channel = channel;
        self
    }

    /// The record's 32 bytes, padding set to zero.
    pub fn to_bytes(&self) -> [u8; RECORD_SIZE] {
        let mut bytes = [0u8; RECORD_SIZE];
        bytes[offset::FRAME..offset::FRAME + 8].copy_from_slice(&self.frame.to_le_bytes());
        bytes[offset::KIND] = self.kind;
        bytes[offset::PART..offset::PART + 2].copy_from_slice(&self.part.to_le_bytes());
        bytes[offset::NOTE] = self.note;
        bytes[offset::CHANNEL] = self.channel;
        bytes[offset::VELOCITY..offset::VELOCITY + 4]
            .copy_from_slice(&self.velocity.to_le_bytes());
        bytes[offset::PARAM..offset::PARAM + 4].copy_from_slice(&self.param.to_le_bytes());
        bytes[offset::VALUE..offset::VALUE + 8].copy_from_slice(&self.value.to_le_bytes());
        bytes
    }

    /// Reads a record from its 32 bytes. The padding is not looked at.
    pub fn from_bytes(bytes: &[u8; RECORD_SIZE]) -> Self {
        let u16_at = |at: usize| u16::from_le_bytes([bytes[at], bytes[at + 1]]);
        let u32_at = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let u64_at = |at: usize| {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(word)
        };
        Self {
            frame: u64_at(offset::FRAME),
            kind: bytes[offset::KIND],
            part: u16_at(offset::PART),
            note: bytes[offset::NOTE],
            channel: bytes[offset::CHANNEL],
            velocity: f32::from_bits(u32_at(offset::VELOCITY)),
            param: u32_at(offset::PARAM),
            value: f64::from_bits(u64_at(offset::VALUE)),
        }
    }

    /// The record as four little-endian 64-bit words, the way the ring
    /// stores it: word `n` is bytes `8n..8n + 8`.
    pub(crate) fn to_words(self) -> [u64; 4] {
        let bytes = self.to_bytes();
        let mut words = [0u64; 4];
        for (word, chunk) in words.iter_mut().zip(bytes.chunks_exact(8)) {
            let mut eight = [0u8; 8];
            eight.copy_from_slice(chunk);
            *word = u64::from_le_bytes(eight);
        }
        words
    }

    /// The inverse of [`to_words`](Self::to_words).
    pub(crate) fn from_words(words: [u64; 4]) -> Self {
        let mut bytes = [0u8; RECORD_SIZE];
        for (chunk, word) in bytes.chunks_exact_mut(8).zip(words) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        Self::from_bytes(&bytes)
    }

    /// Checks the record and says what it asks for, or `None` if it is to
    /// be ignored.
    ///
    /// A record is ignored when its kind code is unknown, or when a field
    /// its kind uses is out of range: a note above 127, a channel above 15,
    /// a note-on velocity that is not a number between 0 and 1, or a
    /// parameter value that is not finite. A note on with velocity 0 is a
    /// note off, as in MIDI. Whether the part and the parameter exist is
    /// only known when the event is applied.
    ///
    /// ```
    /// use manguebyte_web::{Event, EventRecord};
    ///
    /// let silent = EventRecord::note_on(0, 1, 60, 0.0);
    /// assert_eq!(silent.decode(), Some(Event::NoteOff { part: 1, channel: 0, note: 60 }));
    /// assert_eq!(EventRecord::note_on(0, 1, 128, 0.5).decode(), None);
    /// assert_eq!(EventRecord { kind: 9, ..silent }.decode(), None);
    /// ```
    pub fn decode(&self) -> Option<Event> {
        let part = self.part;
        let note_fields_valid = self.note <= 127 && self.channel <= 15;
        match RecordKind::from_code(self.kind)? {
            RecordKind::NoteOn => {
                let velocity = self.velocity;
                if !note_fields_valid || !(0.0..=1.0).contains(&velocity) {
                    return None;
                }
                Some(if velocity == 0.0 {
                    Event::NoteOff {
                        part,
                        channel: self.channel,
                        note: self.note,
                    }
                } else {
                    Event::NoteOn {
                        part,
                        channel: self.channel,
                        note: self.note,
                        velocity,
                    }
                })
            }
            RecordKind::NoteOff => note_fields_valid.then_some(Event::NoteOff {
                part,
                channel: self.channel,
                note: self.note,
            }),
            RecordKind::Param => self.value.is_finite().then_some(Event::Param {
                part,
                param: ParamId(self.param),
                value: self.value,
            }),
            RecordKind::Choke => Some(Event::Choke { part }),
            RecordKind::AllNotesOff => Some(Event::AllNotesOff { part }),
        }
    }
}

/// A record that has passed [`EventRecord::decode`]: every field in range.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// Start a note.
    NoteOn {
        /// The part.
        part: u16,
        /// MIDI channel, 0 to 15.
        channel: u8,
        /// MIDI note number, 0 to 127.
        note: u8,
        /// Above 0, at most 1.
        velocity: f32,
    },
    /// Release a note.
    NoteOff {
        /// The part.
        part: u16,
        /// MIDI channel, 0 to 15.
        channel: u8,
        /// MIDI note number, 0 to 127.
        note: u8,
    },
    /// Set a parameter. The part clamps the value to the parameter's range.
    Param {
        /// The part.
        part: u16,
        /// The parameter.
        param: ParamId,
        /// A finite value in the parameter's own units.
        value: f64,
    },
    /// Stop every note of the part at once.
    Choke {
        /// The part.
        part: u16,
    },
    /// Release every note of the part.
    AllNotesOff {
        /// The part.
        part: u16,
    },
}

impl Event {
    /// The part the event is for.
    pub fn part(&self) -> u16 {
        match *self {
            Self::NoteOn { part, .. }
            | Self::NoteOff { part, .. }
            | Self::Param { part, .. }
            | Self::Choke { part }
            | Self::AllNotesOff { part } => part,
        }
    }
}
