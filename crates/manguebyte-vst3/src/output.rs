//! What a plugin sends out, as events on its VST 3 output event bus.
//!
//! - Notes become note-on and note-off events with the plugin's note ids,
//!   and a choke a note-off with velocity zero.
//! - [`Expression::Pressure`] becomes a polyphonic pressure event, and the
//!   other expressions note expression value events (volume, pan, tuning,
//!   vibrato and brightness), in VST 3's normalised units, the inverse of
//!   how the wrapper reads them.
//! - MIDI 2.0 and MIDI 1.0 from the plugin are read into notes and
//!   expressions, as the framework's input reads them, and sent as above;
//!   what applies to a whole channel (controllers, channel pressure, pitch
//!   bend, program change) goes out as `LegacyMIDICCOutEvent`, MIDI 2.0's
//!   by the default translation to MIDI 1.0.
//!
//! VST 3 names the note an expression shapes by its id alone. A note the
//! plugin gave no id, or one read from MIDI, gets one made from its channel
//! and key, from [`MADE_UP_IDS`] up, so that its expressions find it.
//! An event later than the block is sent on its last frame.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use manguebyte_dsp::event::{Event, EventKind, Expression, NoteRef};
use manguebyte_dsp::note_input::{Mpe, NoteInput};
use manguebyte_midi::MidiMessage;
use manguebyte_plugin::output::{NoteIds, OutputBuffer, OutputKind};
use vst3::ComRef;
use vst3::Steinberg::Vst::{self as vst, IEventList, IEventListTrait};

use crate::wrapper::expression_to_vst;

/// The first of the 2,048 note ids made up for notes that have none: the
/// note on key `k` of channel `ch` is `MADE_UP_IDS + ch * 128 + k`. They sit
/// at the top of the ids VST 3 allows, where hosts' own counters do not
/// reach in a session.
pub(crate) const MADE_UP_IDS: i32 = 0x7FFF_F800;

/// How many of the plugin's identified notes are followed at once.
const IDENTIFIED_NOTES: usize = 256;

/// The audio thread's side of the output event bus.
pub(crate) struct NoteSender {
    /// What the plugin writes during a block.
    pub(crate) output: OutputBuffer,
    /// The channel and key of the plugin's identified notes.
    ids: NoteIds,
    /// Reads the plugin's MIDI into notes and expressions.
    raise: NoteInput,
}

impl NoteSender {
    /// A sender with room for `capacity` events a block, or none.
    /// Allocates.
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            output: if capacity > 0 {
                OutputBuffer::with_capacity(capacity)
            } else {
                OutputBuffer::new()
            },
            ids: NoteIds::with_capacity(if capacity > 0 { IDENTIFIED_NOTES } else { 0 }),
            raise: NoteInput::new(Mpe::Off),
        }
    }

    /// Forgets the notes being followed.
    pub(crate) fn reset(&mut self) {
        self.output.clear();
        self.ids.clear();
        self.raise.reset();
    }

    /// Sends what the plugin wrote during a block of `frames` frames to the
    /// host's `list`, and empties the buffer. Real-time safe.
    ///
    /// # Safety
    ///
    /// `list` must be the host's output event list, valid for the duration
    /// of the call, or null.
    pub(crate) unsafe fn send(&mut self, frames: usize, list: *mut IEventList) {
        // SAFETY: the host's object, valid for this call, or null.
        let list = unsafe { ComRef::from_raw(list) };
        let last = frames.saturating_sub(1).min(i32::MAX as usize) as u32;
        let Self { output, ids, raise } = self;
        for event in output.iter() {
            let offset = event.offset.min(last) as i32;
            let Some(list) = list else {
                continue;
            };
            let mut add = |mut event: vst::Event| {
                event.sampleOffset = offset;
                // SAFETY: the host's own list, which copies the event.
                unsafe {
                    let _ = list.addEvent(&mut event);
                }
            };
            match event.kind {
                OutputKind::NoteOn { note, velocity } => {
                    ids.start(note);
                    add(note_on(note, velocity));
                }
                OutputKind::NoteOff { note, velocity } => add(note_off(ids.end(note), velocity)),
                OutputKind::NoteChoke { note } => add(note_off(ids.end(note), 0.0)),
                OutputKind::NoteExpression {
                    note,
                    expression,
                    value,
                } => {
                    if let Some(event) = shape(ids.resolve(note), expression, value) {
                        add(event);
                    }
                }
                OutputKind::Midi2 { message, .. } => {
                    raise.midi2(0, message, |raised: Event| {
                        raised_event(raised.kind, &mut add)
                    });
                }
                OutputKind::Midi1 { message, .. } => {
                    raise.midi1(0, message, |raised: Event| {
                        raised_event(raised.kind, &mut add)
                    });
                }
            }
        }
        output.clear();
    }
}

/// The id VST 3 knows `note` by: the plugin's own, or one made up from its
/// channel and key.
fn note_id(note: NoteRef) -> i32 {
    note.id
        .and_then(|id| i32::try_from(id).ok())
        .unwrap_or(MADE_UP_IDS + i32::from(note.channel & 0x0F) * 128 + i32::from(note.key & 0x7F))
}

fn event(kind: u32) -> vst::Event {
    // SAFETY: every field of an event, its union included, is a number, so
    // all zeroes is a valid value.
    let mut event: vst::Event = unsafe { std::mem::zeroed() };
    event.r#type = kind as u16;
    event
}

fn note_on(note: NoteRef, velocity: f32) -> vst::Event {
    let mut on = event(vst::Event_::EventTypes_::kNoteOnEvent as u32);
    on.__field0.noteOn = vst::NoteOnEvent {
        channel: i16::from(note.channel & 0x0F),
        pitch: i16::from(note.key & 0x7F),
        tuning: 0.0,
        velocity: velocity.clamp(0.0, 1.0),
        length: 0,
        noteId: note_id(note),
    };
    on
}

fn note_off(note: NoteRef, velocity: f32) -> vst::Event {
    let mut off = event(vst::Event_::EventTypes_::kNoteOffEvent as u32);
    off.__field0.noteOff = vst::NoteOffEvent {
        channel: i16::from(note.channel & 0x0F),
        pitch: i16::from(note.key & 0x7F),
        velocity: velocity.clamp(0.0, 1.0),
        noteId: note_id(note),
        tuning: 0.0,
    };
    off
}

/// The event that shapes `note`, or `None` for an expression VST 3 has no
/// type for.
fn shape(note: NoteRef, expression: Expression, value: f64) -> Option<vst::Event> {
    use vst::NoteExpressionTypeIDs_ as ids;
    if expression == Expression::Pressure {
        let mut pressure = event(vst::Event_::EventTypes_::kPolyPressureEvent as u32);
        pressure.__field0.polyPressure = vst::PolyPressureEvent {
            channel: i16::from(note.channel & 0x0F),
            pitch: i16::from(note.key & 0x7F),
            pressure: value.clamp(0.0, 1.0) as f32,
            noteId: note_id(note),
        };
        return Some(pressure);
    }
    let type_id = match expression {
        Expression::Volume => ids::kVolumeTypeID,
        Expression::Pan => ids::kPanTypeID,
        Expression::Tuning => ids::kTuningTypeID,
        Expression::Vibrato => ids::kVibratoTypeID,
        Expression::Brightness => ids::kBrightnessTypeID,
        Expression::Pressure => return None,
    } as u32;
    let mut shaped = event(vst::Event_::EventTypes_::kNoteExpressionValueEvent as u32);
    shaped.__field0.noteExpressionValue = vst::NoteExpressionValueEvent {
        typeId: type_id,
        noteId: note_id(note),
        value: expression_to_vst(type_id, value)?,
    };
    Some(shaped)
}

/// A MIDI message that applies to a whole channel, as VST 3's legacy
/// controller output event: controllers, channel pressure, pitch bend and
/// program change. Anything else has no such form.
fn controller(message: MidiMessage) -> Option<vst::Event> {
    use vst::ControllerNumbers_ as numbers;
    let (channel, number, value, value2) = match message {
        MidiMessage::ControlChange {
            channel,
            controller,
            value,
        } => (channel, u32::from(controller & 0x7F), value, 0),
        MidiMessage::ChannelPressure { channel, pressure } => {
            (channel, numbers::kAfterTouch as u32, pressure, 0)
        }
        MidiMessage::PitchBend { channel, value } => {
            // Fourteen bits, low seven first, as in the MIDI bytes.
            let wide = (i32::from(value) + 0x2000).clamp(0, 0x3FFF) as u16;
            (
                channel,
                numbers::kPitchBend as u32,
                (wide & 0x7F) as u8,
                (wide >> 7) as u8,
            )
        }
        MidiMessage::ProgramChange { channel, program } => {
            (channel, numbers::kCtrlProgramChange as u32, program, 0)
        }
        _ => return None,
    };
    let mut out = event(vst::Event_::EventTypes_::kLegacyMIDICCOutEvent as u32);
    out.__field0.midiCCOut = vst::LegacyMIDICCOutEvent {
        controlNumber: number as u8,
        // `int8` is bound as `c_char`, which is unsigned on some targets.
        channel: (channel & 0x0F) as std::ffi::c_char,
        value: (value & 0x7F) as std::ffi::c_char,
        value2: (value2 & 0x7F) as std::ffi::c_char,
    };
    Some(out)
}

/// Sends what the framework's input made of the plugin's MIDI.
fn raised_event(kind: EventKind, add: &mut impl FnMut(vst::Event)) {
    let event = match kind {
        EventKind::NoteOn { note, velocity } => Some(note_on(note, velocity)),
        EventKind::NoteOff { note, velocity } => Some(note_off(note, velocity)),
        EventKind::NoteChoke { note } => Some(note_off(note, 0.0)),
        EventKind::NoteExpression {
            note,
            expression,
            value,
        } => shape(note, expression, value),
        EventKind::Midi(message) => controller(message),
        _ => None,
    };
    if let Some(event) = event {
        add(event);
    }
}
