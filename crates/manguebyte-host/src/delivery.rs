//! What the LV2 and Audio Unit hosts share: both carry notes as MIDI 1.0,
//! and both move the transport on between the pieces a block is run in.

use manguebyte_dsp::Transport;
use manguebyte_dsp::event::{EventKind, Expression};
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
}
