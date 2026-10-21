//! The framework's events as an Audio Unit takes them: a parameter change
//! as an `AudioUnitParameterEvent` for `AudioUnitScheduleParameters`
//! (`AUComponent.h`), and notes and MIDI as the three bytes of
//! `MusicDeviceMIDIEvent` (`MusicDevice.h`), each with its frame in the
//! slice.

use manguebyte_core::ParamId;
use manguebyte_dsp::event::EventKind;

use super::sys::*;
use crate::delivery::midi_of;

/// What a parameter needs on the audio thread, sorted by id.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ParamSlot {
    pub(crate) id: u32,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) stepped: bool,
}

impl ParamSlot {
    /// `value` held to the range, and to whole steps for a stepped one, as
    /// the 32-bit float a unit takes.
    pub(crate) fn hold(&self, value: f64) -> f32 {
        let value = value.clamp(self.min, self.max);
        let value = if self.stepped {
            value.round().clamp(self.min, self.max)
        } else {
            value
        };
        value as f32
    }
}

pub(crate) fn find_slot(slots: &[ParamSlot], id: ParamId) -> Option<&ParamSlot> {
    slots
        .binary_search_by_key(&id.0, |slot| slot.id)
        .ok()
        .and_then(|at| slots.get(at))
}

/// One event, as the unit is to be given it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Call {
    /// `AudioUnitScheduleParameters`, with one immediate change (see
    /// [`immediate`]) of a parameter to a value.
    Param {
        /// The parameter's id.
        id: u32,
        /// Its value, held to its range.
        value: f32,
    },
    /// `MusicDeviceMIDIEvent`: status and two data bytes, the second 0 for
    /// a message of two bytes.
    Midi([u8; 3]),
    /// The unit has no way to take it.
    Undeliverable,
}

/// An immediate parameter change on `frame` of the slice.
pub(crate) fn immediate(id: u32, frame: u32, value: f32) -> AudioUnitParameterEvent {
    AudioUnitParameterEvent {
        scope: kAudioUnitScope_Global,
        element: 0,
        parameter: id,
        eventType: kParameterEvent_Immediate,
        eventValues: [frame, value.to_bits(), 0, 0],
    }
}

/// What `kind` is for a unit with these parameters, that takes MIDI or
/// not.
///
/// - [`EventKind::ParamValue`] is in the parameter's own unit, held to its
///   range, as a 32-bit float.
/// - Notes become MIDI 1.0: velocity in 7 bits, a note-on at least 1 so
///   that it is not read as a note-off, no note id. Pressure on one note
///   is polyphonic aftertouch; [`EventKind::Midi`] goes as it is.
/// - The rest (other note expression, chokes, modulation), notes for a
///   unit without `MusicDeviceMIDIEvent`, values that are not numbers and
///   parameters the unit does not have are undeliverable.
pub(crate) fn translate(kind: &EventKind, slots: &[ParamSlot], midi: bool) -> Call {
    if let EventKind::ParamValue { param, value } = *kind {
        return match find_slot(slots, param).filter(|_| !value.is_nan()) {
            Some(slot) => Call::Param {
                id: slot.id,
                value: slot.hold(value),
            },
            None => Call::Undeliverable,
        };
    }
    let Some(message) = midi_of(kind).filter(|_| midi) else {
        return Call::Undeliverable;
    };
    let raw = message.to_bytes();
    let bytes = raw.as_slice();
    match bytes {
        [status, data1, data2] => Call::Midi([*status, *data1, *data2]),
        [status, data1] => Call::Midi([*status, *data1, 0]),
        _ => Call::Undeliverable,
    }
}

#[cfg(test)]
mod tests {
    use manguebyte_dsp::event::{Expression, NoteRef};
    use manguebyte_midi::MidiMessage;

    use super::*;

    const SLOTS: [ParamSlot; 2] = [
        ParamSlot {
            id: 3,
            min: -60.0,
            max: 0.0,
            stepped: false,
        },
        ParamSlot {
            id: 9,
            min: 0.0,
            max: 1.0,
            stepped: true,
        },
    ];

    #[test]
    fn values_become_scheduled_changes() {
        let value = |param, value| EventKind::ParamValue {
            param: ParamId(param),
            value,
        };
        let param = |id, value| Call::Param { id, value };
        assert_eq!(translate(&value(3, -12.5), &SLOTS, false), param(3, -12.5));
        // Held to the range, and to whole steps.
        assert_eq!(translate(&value(3, 10.0), &SLOTS, false), param(3, 0.0));
        assert_eq!(translate(&value(9, 0.6), &SLOTS, true), param(9, 1.0));
        for undeliverable in [value(4, 0.0), value(3, f64::NAN)] {
            assert_eq!(translate(&undeliverable, &SLOTS, true), Call::Undeliverable);
        }
        let event = immediate(3, 17, -12.5);
        assert_eq!(event.eventType, kParameterEvent_Immediate);
        assert_eq!(f32::from_bits(event.eventValues[1]), -12.5);
    }

    #[test]
    fn notes_become_midi_bytes_for_a_unit_that_takes_them() {
        let note = NoteRef::with_id(2, 60, 7);
        let on = EventKind::NoteOn {
            note,
            velocity: 0.0,
        };
        assert_eq!(translate(&on, &SLOTS, true), Call::Midi([0x92, 60, 1]));
        assert_eq!(translate(&on, &SLOTS, false), Call::Undeliverable);
        let program = EventKind::Midi(MidiMessage::ProgramChange {
            channel: 1,
            program: 5,
        });
        assert_eq!(translate(&program, &SLOTS, true), Call::Midi([0xC1, 5, 0]));
        let pressure = EventKind::NoteExpression {
            note,
            expression: Expression::Pressure,
            value: 1.0,
        };
        assert_eq!(
            translate(&pressure, &SLOTS, true),
            Call::Midi([0xA2, 60, 127])
        );
        assert_eq!(
            translate(&EventKind::NoteChoke { note }, &SLOTS, true),
            Call::Undeliverable
        );
    }
}
