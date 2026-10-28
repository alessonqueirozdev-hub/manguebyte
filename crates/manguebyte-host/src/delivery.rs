//! What the LV2 and Audio Unit hosts share: both carry notes as MIDI 1.0,
//! both ways, and both move the transport on between the pieces a block
//! is run in.

use manguebyte_dsp::Transport;
use manguebyte_dsp::event::{Event, EventKind, Expression};
use manguebyte_midi::MidiMessage;

/// The MIDI 1.0 message for an event, if it has one.
pub(crate) fn midi_of(kind: &EventKind) -> Option<MidiMessage> {
    let byte = |value: f64| (value * 127.0).round().clamp(0.0, 127.0) as u8;
    Some(match *kind {
        EventKind::NoteOn { note, velocity } => MidiMessage::NoteOn {
            channel: note.channel & 0x0F,
            note: note.key & 0x7F,
            velocity: byte(f64::from(velocity)).max(1),
        },
        EventKind::NoteOff { note, velocity } => MidiMessage::NoteOff {
            channel: note.channel & 0x0F,
            note: note.key & 0x7F,
            velocity: byte(f64::from(velocity)),
        },
        EventKind::NoteExpression {
            note,
            expression: Expression::Pressure,
            value,
        } if value.is_finite() => MidiMessage::PolyPressure {
            channel: note.channel & 0x0F,
            note: note.key & 0x7F,
            pressure: byte(value),
        },
        EventKind::Midi(message) => message,
        _ => return None,
    })
}

/// Moves `transport` on by `frames` for the next piece of a block, as
/// [`Transport::advance`] does, with the position in frames stopping at
/// the end of its range instead of overflowing.
pub(crate) fn advance(transport: &mut Transport, frames: usize, sample_rate: f64) {
    let position = transport.position_samples.take();
    transport.advance(frames, sample_rate);
    transport.position_samples = match position {
        Some(position) if transport.playing => {
            Some(position.saturating_add(i64::try_from(frames).unwrap_or(i64::MAX)))
        }
        position => position,
    };
}

/// The events a hosted plugin sent during one block, as MIDI 1.0, read
/// into the framework's events for the caller.
///
/// A message becomes the richest event that says the same, as
/// [`Event::from_midi`] makes it: notes are note events with 7-bit
/// velocities and no ids, pressure on one key is a note expression, and
/// everything else is [`EventKind::Midi`]. That is what the CLAP host
/// makes of a plugin's MIDI events, and the VST 3 host of its legacy
/// controller output, so that every format reports the same thing the
/// same way. No MPE or other reading of channels is applied: what a
/// channel stands for is the receiver's business.
///
/// The room is allocated once; what does not fit is counted, never
/// allocated for.
#[derive(Debug)]
pub(crate) struct Sent {
    events: Vec<Event>,
    lost: u64,
}

impl Sent {
    /// Room for `capacity` events.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            events: Vec::with_capacity(capacity),
            lost: 0,
        }
    }

    /// Forgets the last block's events.
    pub(crate) fn clear(&mut self) {
        self.events.clear();
    }

    /// The events kept since the last [`clear`](Self::clear).
    pub(crate) fn as_slice(&self) -> &[Event] {
        &self.events
    }

    /// How many events did not fit since this was last asked, which it
    /// sets back to zero.
    pub(crate) fn take_lost(&mut self) -> u64 {
        std::mem::take(&mut self.lost)
    }

    /// Takes the MIDI 1.0 bytes the plugin sent on `frame` of a piece that
    /// starts `base` frames into the block and is `frames` long. The bytes
    /// may hold several messages, as [`messages`] reads them.
    ///
    /// A frame outside the piece is the plugin's mistake; its messages are
    /// kept, on the nearest frame it could have meant, as the CLAP and
    /// VST 3 hosts keep theirs.
    pub(crate) fn midi(&mut self, base: usize, frames: usize, frame: i64, bytes: &[u8]) {
        let last = frames.saturating_sub(1);
        let within = usize::try_from(frame.max(0))
            .unwrap_or(usize::MAX)
            .min(last);
        let offset = u32::try_from(base.saturating_add(within)).unwrap_or(u32::MAX);
        messages(bytes, |message| {
            if self.events.len() < self.events.capacity() {
                self.events.push(Event::from_midi(offset, message));
            } else {
                self.lost += 1;
            }
        });
    }
}

/// Calls `each` with every channel voice message in `bytes`, a stretch of
/// a MIDI 1.0 byte stream that starts on a message, in order.
///
/// Running status is followed. Real-time bytes, system common messages
/// and system exclusive data are passed over, and so are data bytes with
/// no status before them and a message cut short by the end or by another
/// status byte. Nothing is allocated.
pub(crate) fn messages(bytes: &[u8], mut each: impl FnMut(MidiMessage)) {
    let mut at = 0;
    let mut running: Option<u8> = None;
    while let Some(&byte) = bytes.get(at) {
        match byte {
            // Real time: one byte, anywhere, and running status holds.
            0xF8.. => at += 1,
            0xF0 => {
                // System exclusive, up to its end or the next status byte.
                at += 1;
                while bytes.get(at).is_some_and(|&data| data < 0x80) {
                    at += 1;
                }
                if bytes.get(at) == Some(&0xF7) {
                    at += 1;
                }
                running = None;
            }
            0xF1..=0xF7 => {
                // System common, with the data bytes each one has.
                at += match byte {
                    0xF1 | 0xF3 => 2,
                    0xF2 => 3,
                    _ => 1,
                };
                running = None;
            }
            _ => {
                let (status, data) = if byte >= 0x80 {
                    (byte, at + 1)
                } else if let Some(status) = running {
                    (status, at)
                } else {
                    at += 1;
                    continue;
                };
                running = Some(status);
                let length = if matches!(status & 0xF0, 0xC0 | 0xD0) {
                    1
                } else {
                    2
                };
                let Some(body) = bytes.get(data..data + length) else {
                    return;
                };
                if let Some(cut) = body.iter().position(|&data| data >= 0x80) {
                    // Cut short by a status byte, which starts what follows.
                    at = data + cut;
                    continue;
                }
                let mut message = [status, 0, 0];
                message[1..=length].copy_from_slice(body);
                if let Some(message) = MidiMessage::from_bytes(&message[..=length]) {
                    each(message);
                }
                at = data + length;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use manguebyte_dsp::event::NoteRef;

    use super::*;

    #[test]
    fn notes_become_midi_and_what_midi_cannot_carry_does_not() {
        let note = NoteRef::with_id(17, 200, 9);
        assert_eq!(
            midi_of(&EventKind::NoteOn {
                note,
                velocity: 0.0
            }),
            Some(MidiMessage::NoteOn {
                channel: 1,
                note: 72,
                velocity: 1
            })
        );
        assert_eq!(
            midi_of(&EventKind::NoteOff {
                note,
                velocity: 2.0
            }),
            Some(MidiMessage::NoteOff {
                channel: 1,
                note: 72,
                velocity: 127
            })
        );
        assert_eq!(
            midi_of(&EventKind::NoteExpression {
                note,
                expression: Expression::Pressure,
                value: 0.5
            }),
            Some(MidiMessage::PolyPressure {
                channel: 1,
                note: 72,
                pressure: 64
            })
        );
        for nothing in [
            EventKind::NoteChoke { note },
            EventKind::NoteExpression {
                note,
                expression: Expression::Tuning,
                value: 1.0,
            },
            EventKind::NoteExpression {
                note,
                expression: Expression::Pressure,
                value: f64::NAN,
            },
        ] {
            assert_eq!(midi_of(&nothing), None, "{nothing:?}");
        }
    }

    fn all(bytes: &[u8]) -> Vec<MidiMessage> {
        let mut found = Vec::new();
        messages(bytes, |message| found.push(message));
        found
    }

    #[test]
    fn a_stream_is_read_message_by_message_with_running_status() {
        let on = |channel, note, velocity| MidiMessage::NoteOn {
            channel,
            note,
            velocity,
        };
        // A real-time byte between two messages keeps running status.
        assert_eq!(
            all(&[0x92, 60, 100, 64, 90, 0xF8, 67, 80]),
            [on(2, 60, 100), on(2, 64, 90), on(2, 67, 80)]
        );
        // One data byte for a program change, two for the wheel.
        assert_eq!(
            all(&[0xC3, 5, 0xE0, 0, 0x40]),
            [
                MidiMessage::ProgramChange {
                    channel: 3,
                    program: 5
                },
                MidiMessage::PitchBend {
                    channel: 0,
                    value: 0
                }
            ]
        );
        // System exclusive and system common messages are passed over and
        // end running status, so the stray data bytes after them are too.
        assert_eq!(
            all(&[
                0x90, 60, 1, 0xF0, 1, 2, 3, 0xF7, 61, 0xF2, 1, 2, 5, 0x80, 60, 0
            ]),
            [
                on(0, 60, 1),
                MidiMessage::NoteOff {
                    channel: 0,
                    note: 60,
                    velocity: 0
                }
            ]
        );
        // A message cut short by a status byte or by the end is dropped.
        assert_eq!(
            all(&[0x90, 60, 0xB0, 7, 100, 0x90, 60]),
            [MidiMessage::ControlChange {
                channel: 0,
                controller: 7,
                value: 100
            }]
        );
        assert!(all(&[]).is_empty());
    }

    #[test]
    fn what_was_sent_lands_on_its_frame_and_what_does_not_fit_is_counted() {
        let mut sent = Sent::new(3);
        sent.midi(100, 50, 7, &[0x90, 60, 127]);
        // Outside the piece: its nearest frame.
        sent.midi(100, 50, -3, &[0xA0, 60, 64]);
        sent.midi(100, 50, 80, &[0xB0, 1, 2, 3, 4]);
        let offsets: Vec<u32> = sent.as_slice().iter().map(|event| event.offset).collect();
        assert_eq!(offsets, [107, 100, 149]);
        assert_eq!(
            sent.as_slice()[0].kind,
            EventKind::NoteOn {
                note: NoteRef::new(0, 60),
                velocity: 1.0
            }
        );
        assert_eq!(
            sent.as_slice()[1].kind,
            EventKind::NoteExpression {
                note: NoteRef::new(0, 60),
                expression: Expression::Pressure,
                value: 64.0 / 127.0
            }
        );
        // The second controller, in running status, did not fit.
        assert_eq!(sent.take_lost(), 1);
        assert_eq!(sent.take_lost(), 0);
        sent.clear();
        assert!(sent.as_slice().is_empty());
    }
}
