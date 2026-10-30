//! The MIDI Tuning Standard: tunings sent as system exclusive messages.

use core::fmt;

use super::{KEYS, Tuning};

/// A key's pitch as MIDI tuning messages give it: a semitone (an
/// equal-tempered note number), then a 14-bit fraction of a semitone, most
/// significant 7 bits first. `7F 7F 7F` means "leave this key alone".
const NO_CHANGE: [u8; 3] = [0x7F, 0x7F, 0x7F];

/// The universal system exclusive identifiers: non-real-time and real-time.
const NON_REAL_TIME: u8 = 0x7E;
const REAL_TIME: u8 = 0x7F;
/// The sub-identifier of the MIDI Tuning Standard.
const TUNING: u8 = 0x08;
/// Its messages read here.
const BULK_DUMP: u8 = 0x01;
const NOTE_CHANGE: u8 = 0x02;
const BANK_NOTE_CHANGE: u8 = 0x07;

/// Bytes of a bulk dump from the universal identifier to the checksum:
/// identifier, device, `08 01`, program, a name of 16, 128 keys of 3, and
/// the checksum.
const BULK_DUMP_LEN: usize = 1 + 1 + 2 + 1 + 16 + KEYS * 3 + 1;

/// A system exclusive message that could not be read as a tuning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MtsError {
    /// Not a universal system exclusive message of the MIDI Tuning
    /// Standard (`7E` or `7F`, then a device, then `08`).
    NotTuning,
    /// A message of the standard that is not read here, by its second
    /// sub-identifier, such as `03` (a tuning dump request) or `08` (scale
    /// and octave tuning).
    Unsupported(u8),
    /// The message is not as long as its kind and count say.
    Length {
        /// The bytes the message should have, from the universal identifier
        /// to its end, without `F0` and `F7`.
        expected: usize,
        /// The bytes it has.
        found: usize,
    },
    /// A byte at or above `0x80` inside the message.
    NotData,
    /// The checksum of a bulk dump does not match its contents.
    Checksum {
        /// The checksum the contents give.
        expected: u8,
        /// The one the message carries.
        found: u8,
    },
}

impl fmt::Display for MtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotTuning => f.write_str("not a MIDI tuning message"),
            Self::Unsupported(kind) => write!(f, "MIDI tuning message 08 {kind:02X} is not read"),
            Self::Length { expected, found } => write!(
                f,
                "a MIDI tuning message of {found} bytes where {expected} were expected"
            ),
            Self::NotData => f.write_str("a status byte inside a MIDI tuning message"),
            Self::Checksum { expected, found } => write!(
                f,
                "bulk tuning dump checksum is {found:02X}, the contents give {expected:02X}"
            ),
        }
    }
}

impl std::error::Error for MtsError {}

/// A message of the MIDI Tuning Standard, read from system exclusive bytes
/// without copying them.
///
/// Two kinds are read: the bulk tuning dump (`F0 7E <device> 08 01`), which
/// carries a pitch for all 128 keys, and the single note tuning change, in
/// real time (`F0 7F <device> 08 02`) or with a bank (`08 07`, real time or
/// not), which retunes a few keys. A pitch is an equal-tempered note
/// number and a fraction of a semitone in 14 bits, about 0.006 cent.
///
/// Reading and [applying](Self::apply) allocate nothing, so a message
/// may be handled on any thread. Which device and program a message
/// addresses is for the caller to check.
///
/// ```
/// use manguebyte_dsp::tuning::{MtsMessage, Tuning};
///
/// // Key 69 (A4) to semitone 69 and half a semitone: 50 cents sharp.
/// let sysex = [0xF0, 0x7F, 0x7F, 0x08, 0x02, 0x00, 0x01, 69, 69, 0x40, 0x00, 0xF7];
/// let message = MtsMessage::parse(&sysex)?;
/// let tuning = message.apply(&Tuning::default());
/// assert!((tuning.key_frequency(69) - 440.0 * 2f64.powf(0.5 / 12.0)).abs() < 1e-9);
/// assert_eq!(tuning.key_frequency(70), Tuning::default().key_frequency(70));
/// # Ok::<(), manguebyte_dsp::tuning::MtsError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MtsMessage<'a> {
    /// A bulk tuning dump: a pitch for every key.
    BulkDump {
        /// The device addressed; `7F` is every device.
        device: u8,
        /// The tuning program the dump is for.
        program: u8,
        /// The tuning's name, 16 bytes of ASCII.
        name: [u8; 16],
        /// Three bytes per key, keys 0 to 127.
        data: &'a [u8],
    },
    /// A single note tuning change: new pitches for some keys.
    NoteChanges {
        /// The device addressed; `7F` is every device.
        device: u8,
        /// The tuning bank, for the form that has one (`08 07`).
        bank: Option<u8>,
        /// The tuning program changed.
        program: u8,
        /// Whether it was sent as real time, to take effect at once on
        /// notes already sounding.
        real_time: bool,
        /// Four bytes per change: the key, then its pitch.
        changes: &'a [u8],
    },
}

impl<'a> MtsMessage<'a> {
    /// Reads a message, given with or without the `F0` that starts system
    /// exclusive and the `F7` that ends it (a
    /// [`StreamParser`](manguebyte_midi::StreamParser) gives neither).
    pub fn parse(sysex: &'a [u8]) -> Result<Self, MtsError> {
        let body = sysex.strip_prefix(&[0xF0]).unwrap_or(sysex);
        let body = body.strip_suffix(&[0xF7]).unwrap_or(body);
        if body.iter().any(|&byte| byte >= 0x80) {
            return Err(MtsError::NotData);
        }
        let [universal, device, TUNING, kind, ..] = *body else {
            return Err(MtsError::NotTuning);
        };
        if universal != NON_REAL_TIME && universal != REAL_TIME {
            return Err(MtsError::NotTuning);
        }
        let real_time = universal == REAL_TIME;
        let length = |expected: usize| {
            if body.len() == expected {
                Ok(())
            } else {
                Err(MtsError::Length {
                    expected,
                    found: body.len(),
                })
            }
        };
        match kind {
            BULK_DUMP if !real_time => {
                length(BULK_DUMP_LEN)?;
                let found = body[BULK_DUMP_LEN - 1];
                let expected = body[..BULK_DUMP_LEN - 1]
                    .iter()
                    .fold(0, |sum, &byte| sum ^ byte)
                    & 0x7F;
                if found != expected {
                    return Err(MtsError::Checksum { expected, found });
                }
                let mut name = [0; 16];
                name.copy_from_slice(&body[5..21]);
                Ok(Self::BulkDump {
                    device,
                    program: body[4],
                    name,
                    data: &body[21..BULK_DUMP_LEN - 1],
                })
            }
            NOTE_CHANGE if real_time => {
                let count = usize::from(*body.get(5).ok_or(MtsError::Length {
                    expected: 6,
                    found: body.len(),
                })?);
                length(6 + 4 * count)?;
                Ok(Self::NoteChanges {
                    device,
                    bank: None,
                    program: body[4],
                    real_time,
                    changes: &body[6..],
                })
            }
            BANK_NOTE_CHANGE => {
                let count = usize::from(*body.get(6).ok_or(MtsError::Length {
                    expected: 7,
                    found: body.len(),
                })?);
                length(7 + 4 * count)?;
                Ok(Self::NoteChanges {
                    device,
                    bank: Some(body[4]),
                    program: body[5],
                    real_time,
                    changes: &body[7..],
                })
            }
            other => Err(MtsError::Unsupported(other)),
        }
    }

    /// The tuning this message makes of `tuning`: a bulk dump replaces the
    /// pitch of every key it does not mark "no change", a note change the
    /// pitch of each key it names (the last change of a key wins). A key
    /// given a pitch becomes mapped; the others keep what `tuning` had.
    pub fn apply(&self, tuning: &Tuning) -> Tuning {
        let mut result = tuning.clone();
        match *self {
            Self::BulkDump { data, .. } => {
                for (key, pitch) in data.chunks_exact(3).enumerate() {
                    result.set_mts_pitch(key, pitch);
                }
            }
            Self::NoteChanges { changes, .. } => {
                for change in changes.chunks_exact(4) {
                    result.set_mts_pitch(usize::from(change[0]), &change[1..]);
                }
            }
        }
        result
    }

    /// The name of a bulk dump, as text, without the spaces or zeros that
    /// pad it; `None` for a note change.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::BulkDump { name, .. } => {
                let text = core::str::from_utf8(name).unwrap_or("");
                Some(text.trim_end_matches([' ', '\0']))
            }
            Self::NoteChanges { .. } => None,
        }
    }
}

impl Tuning {
    /// Gives `key` the pitch of three MIDI tuning bytes, unless they say
    /// "no change".
    fn set_mts_pitch(&mut self, key: usize, bytes: &[u8]) {
        if key >= KEYS || bytes == NO_CHANGE {
            return;
        }
        let fraction = (u16::from(bytes[1]) << 7) | u16::from(bytes[2]);
        let pitch = f64::from(bytes[0]) + f64::from(fraction) / 16_384.0;
        self.set_key_pitch(key, pitch);
    }
}

/// The three bytes MIDI tuning messages give a pitch in: the nearest
/// semitone at or below it and the fraction above, rounded to the nearest
/// of 16 384 steps. `None` for a pitch outside what the format holds (0 to
/// just under 128), or one that would read as "no change".
pub fn mts_pitch_bytes(pitch: f64) -> Option<[u8; 3]> {
    if !(0.0..128.0).contains(&pitch) {
        return None;
    }
    let steps = (pitch * 16_384.0).round() as u32;
    let semitone = steps >> 14;
    let fraction = steps & 0x3FFF;
    let bytes = [
        semitone as u8,
        (fraction >> 7) as u8,
        (fraction & 0x7F) as u8,
    ];
    if semitone > 127 || bytes == NO_CHANGE {
        return None;
    }
    Some(bytes)
}
