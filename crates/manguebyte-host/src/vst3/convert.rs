//! Turning the framework's events and transport into VST 3's, and what a
//! plugin reports back into the framework's.
//!
//! VST 3 splits what the framework keeps in one stream. Notes, pressure and
//! note expression travel in an event list; parameter values travel as
//! automation points, one queue per parameter; MIDI controllers, pitch bend
//! and channel pressure have no event of their own and travel as points for
//! whichever parameter the plugin maps them to. The scales VST 3 gives note
//! expressions are listed with [`expression_to_vst`].
//!
//! Everything here is free of allocation and is used on the audio thread.

// The bindings give enumerations a different integer type on Windows than
// elsewhere, so a cast that is needed on one platform is redundant on the
// other.
#![allow(clippy::unnecessary_cast)]

use manguebyte_dsp::Transport;
use manguebyte_dsp::event::{Event, EventKind, Expression, NoteRef};
use manguebyte_midi::MidiMessage;
use vst3::Steinberg::Vst::ProcessContext_::StatesAndFlags_ as flags;
use vst3::Steinberg::Vst::{self as vst, ControllerNumbers_, NoteExpressionTypeIDs_ as ids};

use super::changes::NoteEvent;

/// How many controllers VST 3 numbers for one channel: the 128 of MIDI,
/// then channel pressure (128) and pitch bend (129).
pub(crate) const CONTROLLERS: usize = ControllerNumbers_::kCountCtrlNumber as usize;

/// The controller number VST 3 gives channel pressure.
const CHANNEL_PRESSURE: usize = ControllerNumbers_::kAfterTouch as usize;

/// The controller number VST 3 gives pitch bend.
const PITCH_BEND: usize = ControllerNumbers_::kPitchBend as usize;

/// The parameter a plugin maps each controller of each MIDI channel to, or
/// [`vst::kNoParamId`] for none.
pub(crate) type MidiMap = [[u32; CONTROLLERS]; 16];

/// A note as VST 3 addresses it: `(channel, pitch, note id)`, where an id of
/// -1 means the note has none. An id too large for VST 3's signed field is
/// left out, and the note is then addressed by channel and pitch.
fn address(note: NoteRef) -> (i16, i16, i32) {
    let id = note.id.and_then(|id| i32::try_from(id).ok()).unwrap_or(-1);
    (
        i16::from(note.channel & 0x0F),
        i16::from(note.key & 0x7F),
        id,
    )
}

/// `value` held to `0..=1`, with anything that is not a number as zero.
pub(crate) fn unit(value: f64) -> f64 {
    if value.is_nan() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

/// The position in `0..=1` that VST 3 uses for a note expression of the
/// framework, and the expression's VST 3 type, or `None` for pressure,
/// which VST 3 sends as an event of its own, and for a value that is not a
/// finite number.
///
/// The scales are VST 3's (`ivstnoteexpression.h`, `NoteExpressionTypeIDs`):
/// volume is a gain of `4 * position`, so a quarter of the way is unity
/// and the top is +12 dB; pan runs from left (0) through the centre (0.5)
/// to right (1); tuning is `240 * (position - 0.5)` semitones, ten octaves
/// either way; vibrato and brightness are positions already. A value
/// beyond the scale is held to its end.
///
/// The way back, [`expression_from_vst`], rounds again: tuning returns
/// within 240 × 2⁻⁵² semitones and pan within 2⁻⁵² of where it started,
/// the others exactly (volume from 2⁻¹⁰²⁰ up). A test below measures it.
pub(crate) fn expression_to_vst(expression: Expression, value: f64) -> Option<(u32, f64)> {
    if !value.is_finite() {
        return None;
    }
    let (type_id, position) = match expression {
        Expression::Volume => (ids::kVolumeTypeID, value / 4.0),
        Expression::Pan => (ids::kPanTypeID, (value + 1.0) / 2.0),
        Expression::Tuning => (ids::kTuningTypeID, value / 240.0 + 0.5),
        Expression::Vibrato => (ids::kVibratoTypeID, value),
        Expression::Brightness => (ids::kBrightnessTypeID, value),
        Expression::Pressure => return None,
    };
    Some((type_id as u32, unit(position)))
}

/// The reverse of [`expression_to_vst`], for what a plugin reports.
fn expression_from_vst(type_id: u32, position: f64) -> Option<(Expression, f64)> {
    if !position.is_finite() {
        return None;
    }
    let position = unit(position);
    Some(match type_id {
        id if id == ids::kVolumeTypeID as u32 => (Expression::Volume, 4.0 * position),
        id if id == ids::kPanTypeID as u32 => (Expression::Pan, 2.0 * position - 1.0),
        id if id == ids::kTuningTypeID as u32 => (Expression::Tuning, 240.0 * (position - 0.5)),
        id if id == ids::kVibratoTypeID as u32 => (Expression::Vibrato, position),
        id if id == ids::kBrightnessTypeID as u32 => (Expression::Brightness, position),
        _ => return None,
    })
}

/// What one of the framework's events becomes for a VST 3 plugin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Outgoing {
    /// An event for the plugin's event list.
    Note(NoteEvent),
    /// A point for one parameter's queue, with a normalized value.
    Point { id: u32, value: f64 },
    /// Nothing the plugin can receive.
    Nothing,
}

/// Converts one of the framework's events for a VST 3 plugin.
///
/// - Notes keep their channel, key and id. A choke has no VST 3 form and is
///   sent as a note-off with no velocity.
/// - Pressure for one note is a polyphonic pressure event. The other note
///   expressions are note expression events, which VST 3 addresses by
///   note id alone: one for a note without an id is not sent.
/// - A parameter value is a point, normalized, held to `0..=1`.
/// - A controller, channel pressure or pitch bend is a point for the
///   parameter `map` says the plugin maps it to; one the plugin maps to
///   nothing is not sent. Note messages that arrive as MIDI are sent as
///   notes.
/// - Parameter modulation and program changes have no VST 3 form and are
///   not sent.
pub(crate) fn outgoing(kind: &EventKind, map: Option<&MidiMap>) -> Outgoing {
    match *kind {
        EventKind::NoteOn { note, velocity } => {
            let (channel, pitch, note_id) = address(note);
            Outgoing::Note(NoteEvent::On {
                channel,
                pitch,
                tuning: 0.0,
                velocity: unit(f64::from(velocity)) as f32,
                length: 0,
                note_id,
            })
        }
        EventKind::NoteOff { note, velocity } => {
            let (channel, pitch, note_id) = address(note);
            Outgoing::Note(NoteEvent::Off {
                channel,
                pitch,
                velocity: unit(f64::from(velocity)) as f32,
                note_id,
                tuning: 0.0,
            })
        }
        EventKind::NoteChoke { note } => {
            let (channel, pitch, note_id) = address(note);
            Outgoing::Note(NoteEvent::Off {
                channel,
                pitch,
                velocity: 0.0,
                note_id,
                tuning: 0.0,
            })
        }
        EventKind::NoteExpression {
            note,
            expression: Expression::Pressure,
            value,
        } => {
            if !value.is_finite() {
                return Outgoing::Nothing;
            }
            let (channel, pitch, note_id) = address(note);
            Outgoing::Note(NoteEvent::Pressure {
                channel,
                pitch,
                pressure: unit(value) as f32,
                note_id,
            })
        }
        EventKind::NoteExpression {
            note,
            expression,
            value,
        } => {
            let (_, _, note_id) = address(note);
            match expression_to_vst(expression, value) {
                Some((type_id, value)) if note_id != -1 => Outgoing::Note(NoteEvent::Expression {
                    type_id,
                    note_id,
                    value,
                }),
                _ => Outgoing::Nothing,
            }
        }
        EventKind::ParamValue { param, value } => {
            if value.is_nan() {
                Outgoing::Nothing
            } else {
                Outgoing::Point {
                    id: param.0,
                    value: unit(value),
                }
            }
        }
        // VST 3 has no per-note parameter values or modulation.
        EventKind::NoteParamValue { .. } | EventKind::ParamModulation { .. } => Outgoing::Nothing,
        EventKind::Midi(message) => match message {
            // Note messages carried as MIDI are notes all the same.
            MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. } => {
                let kind = Event::from_midi(0, message).kind;
                outgoing(&kind, map)
            }
            MidiMessage::PolyPressure { .. } => {
                let kind = Event::from_midi(0, message).kind;
                outgoing(&kind, map)
            }
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => mapped(
                map,
                channel,
                usize::from(controller & 0x7F),
                f64::from(value & 0x7F) / 127.0,
            ),
            MidiMessage::ChannelPressure { channel, pressure } => mapped(
                map,
                channel,
                CHANNEL_PRESSURE,
                f64::from(pressure & 0x7F) / 127.0,
            ),
            MidiMessage::PitchBend { channel, value } => {
                // -8192..=8191 onto 0..=1, with the centre on 8192 / 16383.
                let wheel = f64::from(value.clamp(-8_192, 8_191)) + 8_192.0;
                mapped(map, channel, PITCH_BEND, wheel / 16_383.0)
            }
            MidiMessage::ProgramChange { .. } => Outgoing::Nothing,
        },
    }
}

/// A point for the parameter a controller is mapped to.
fn mapped(map: Option<&MidiMap>, channel: u8, controller: usize, value: f64) -> Outgoing {
    let id = map
        .and_then(|map| map.get(usize::from(channel & 0x0F)))
        .and_then(|controllers| controllers.get(controller))
        .copied()
        .unwrap_or(vst::kNoParamId);
    if id == vst::kNoParamId {
        Outgoing::Nothing
    } else {
        Outgoing::Point {
            id,
            value: unit(value),
        }
    }
}

/// The note a plugin's event is about.
fn note_ref(channel: i16, pitch: i16, note_id: i32) -> NoteRef {
    let channel = u8::try_from(channel).unwrap_or(0) & 0x0F;
    let key = u8::try_from(pitch).unwrap_or(0) & 0x7F;
    match u32::try_from(note_id) {
        Ok(id) => NoteRef::with_id(channel, key, id),
        Err(_) => NoteRef::new(channel, key),
    }
}

/// Converts an event a plugin put in its output list, or returns `None` for
/// one this host has no use for: a note expression it does not convert, or
/// a value that is not a finite number.
pub(crate) fn incoming(event: NoteEvent) -> Option<EventKind> {
    Some(match event {
        NoteEvent::On {
            channel,
            pitch,
            velocity,
            note_id,
            ..
        } => EventKind::NoteOn {
            note: note_ref(channel, pitch, note_id),
            velocity: unit(f64::from(velocity)) as f32,
        },
        NoteEvent::Off {
            channel,
            pitch,
            velocity,
            note_id,
            ..
        } => EventKind::NoteOff {
            note: note_ref(channel, pitch, note_id),
            velocity: unit(f64::from(velocity)) as f32,
        },
        NoteEvent::Pressure {
            channel,
            pitch,
            pressure,
            note_id,
        } => {
            if !pressure.is_finite() {
                return None;
            }
            EventKind::NoteExpression {
                note: note_ref(channel, pitch, note_id),
                expression: Expression::Pressure,
                value: unit(f64::from(pressure)),
            }
        }
        NoteEvent::Expression {
            type_id,
            note_id,
            value,
        } => {
            let (expression, value) = expression_from_vst(type_id, value)?;
            EventKind::NoteExpression {
                note: note_ref(0, 0, note_id),
                expression,
                value,
            }
        }
        NoteEvent::MidiOut {
            control,
            channel,
            value,
            value2,
        } => {
            let channel = u8::try_from(channel).unwrap_or(0) & 0x0F;
            let low = u8::try_from(value).unwrap_or(0) & 0x7F;
            let high = u8::try_from(value2).unwrap_or(0) & 0x7F;
            let message = match usize::from(control) {
                PITCH_BEND => MidiMessage::PitchBend {
                    channel,
                    value: ((i16::from(high) << 7) | i16::from(low)) - 8_192,
                },
                CHANNEL_PRESSURE => MidiMessage::ChannelPressure {
                    channel,
                    pressure: low,
                },
                controller if controller < 128 => MidiMessage::ControlChange {
                    channel,
                    controller: controller as u8,
                    value: low,
                },
                _ => return None,
            };
            EventKind::Midi(message)
        }
    })
}

/// The transport as VST 3 describes it.
///
/// A field the framework's transport does not know is left at zero with
/// its flag clear. VST 3 has no flag for the position in frames, so an
/// unknown one is given as zero. `continuous` is the number of frames
/// processed since activation.
pub(crate) fn process_context(
    transport: &Transport,
    sample_rate: f64,
    continuous: i64,
) -> vst::ProcessContext {
    // SAFETY: every field is a number, or a struct of numbers, for which all
    // zeros is a value.
    let mut context: vst::ProcessContext = unsafe { std::mem::zeroed() };
    let mut state = 0u32;
    if transport.playing {
        state |= flags::kPlaying as u32;
    }
    if transport.recording {
        state |= flags::kRecording as u32;
    }
    if let Some(tempo) = transport.tempo_bpm.filter(|tempo| tempo.is_finite()) {
        state |= flags::kTempoValid as u32;
        context.tempo = tempo;
    }
    if let Some((numerator, denominator)) = transport.time_signature {
        state |= flags::kTimeSigValid as u32;
        context.timeSigNumerator = i32::from(numerator);
        context.timeSigDenominator = i32::from(denominator);
    }
    if let Some(beats) = transport.position_beats.filter(|beats| beats.is_finite()) {
        state |= flags::kProjectTimeMusicValid as u32;
        context.projectTimeMusic = beats;
    }
    if let Some(bar) = transport.bar_start_beats.filter(|bar| bar.is_finite()) {
        state |= flags::kBarPositionValid as u32;
        context.barPositionMusic = bar;
    }
    if let Some((start, end)) = transport.loop_beats {
        if start.is_finite() && end.is_finite() {
            state |= (flags::kCycleActive | flags::kCycleValid) as u32;
            context.cycleStartMusic = start;
            context.cycleEndMusic = end;
        }
    }
    state |= flags::kContTimeValid as u32;
    context.state = state;
    context.sampleRate = sample_rate;
    context.projectTimeSamples = transport.position_samples.unwrap_or(0);
    context.continousTimeSamples = continuous;
    context
}

/// Moves `transport` on by `frames`, as [`Transport::advance`] does, for
/// the next piece of a block that is processed in pieces. Real-time safe.
///
/// As in the CLAP host: `Transport::advance` adds to the position in frames
/// with a plain `+`, which would panic on the audio thread in a build with
/// overflow checks for a position at the end of its range, so that part is
/// done here with an addition that stops at the end.
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
    use manguebyte_core::ParamId;

    use super::*;

    #[test]
    fn notes_keep_channel_key_and_id() {
        let note = NoteRef::with_id(3, 61, 42);
        assert_eq!(
            outgoing(
                &EventKind::NoteOn {
                    note,
                    velocity: 0.5
                },
                None
            ),
            Outgoing::Note(NoteEvent::On {
                channel: 3,
                pitch: 61,
                tuning: 0.0,
                velocity: 0.5,
                length: 0,
                note_id: 42,
            })
        );
        // No id is -1, and so is an id VST 3 cannot hold.
        for note in [NoteRef::new(0, 60), NoteRef::with_id(0, 60, u32::MAX)] {
            let Outgoing::Note(NoteEvent::Off { note_id, .. }) = outgoing(
                &EventKind::NoteOff {
                    note,
                    velocity: 2.0,
                },
                None,
            ) else {
                panic!("a note-off did not become one");
            };
            assert_eq!(note_id, -1);
        }
        // A choke is a note-off with no velocity.
        assert_eq!(
            outgoing(&EventKind::NoteChoke { note }, None),
            Outgoing::Note(NoteEvent::Off {
                channel: 3,
                pitch: 61,
                velocity: 0.0,
                note_id: 42,
                tuning: 0.0,
            })
        );
    }

    #[test]
    fn note_expressions_take_vst3_scales_and_come_back() {
        let note = NoteRef::with_id(0, 60, 7);
        let shaped = |expression, value| EventKind::NoteExpression {
            note,
            expression,
            value,
        };
        // Values whose conversion is exact both ways.
        for (expression, value, position) in [
            (Expression::Volume, 1.0, 0.25),
            (Expression::Volume, 4.0, 1.0),
            (Expression::Pan, 0.0, 0.5),
            (Expression::Pan, -1.0, 0.0),
            (Expression::Tuning, 60.0, 0.75),
            (Expression::Tuning, -120.0, 0.0),
            (Expression::Vibrato, 0.25, 0.25),
            (Expression::Brightness, 1.0, 1.0),
        ] {
            let Outgoing::Note(event) = outgoing(&shaped(expression, value), None) else {
                panic!("{expression:?} was not sent");
            };
            let NoteEvent::Expression {
                note_id, value: at, ..
            } = event
            else {
                panic!("{expression:?} did not become a note expression");
            };
            assert_eq!((note_id, at), (7, position), "{expression:?} {value}");
            // VST 3 names the note by its id alone, so that is what comes
            // back.
            let back = EventKind::NoteExpression {
                note: NoteRef::with_id(0, 0, 7),
                expression,
                value,
            };
            assert_eq!(incoming(event), Some(back));
        }
        // Beyond the scale is held to its end; not a number is not sent.
        assert_eq!(
            expression_to_vst(Expression::Volume, 9.0),
            Some((ids::kVolumeTypeID as u32, 1.0))
        );
        assert_eq!(expression_to_vst(Expression::Tuning, f64::INFINITY), None);
        // A note with no id cannot be shaped: VST 3 addresses by id.
        let anonymous = EventKind::NoteExpression {
            note: NoteRef::new(0, 60),
            expression: Expression::Tuning,
            value: 1.0,
        };
        assert_eq!(outgoing(&anonymous, None), Outgoing::Nothing);
        // Pressure is an event of its own, and needs no id.
        let pressed = EventKind::NoteExpression {
            note: NoteRef::new(2, 50),
            expression: Expression::Pressure,
            value: 0.75,
        };
        let Outgoing::Note(event) = outgoing(&pressed, None) else {
            panic!("pressure was not sent");
        };
        assert_eq!(
            event,
            NoteEvent::Pressure {
                channel: 2,
                pitch: 50,
                pressure: 0.75,
                note_id: -1
            }
        );
        assert_eq!(incoming(event), Some(pressed));
    }

    /// Values across `low..=high`: an even sweep, values from a simple
    /// pseudo-random sequence, and `extra`.
    fn across(low: f64, high: f64, extra: &[f64]) -> Vec<f64> {
        let steps = 50_000u32;
        let mut values: Vec<f64> = (0..=steps)
            .map(|step| low + (high - low) * f64::from(step) / f64::from(steps))
            .collect();
        // A 64-bit linear congruential generator (Knuth's MMIX constants).
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..50_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let fraction = (state >> 11) as f64 / (1u64 << 53) as f64;
            values.push(low + (high - low) * fraction);
        }
        values.extend_from_slice(extra);
        values
    }

    /// How far `value` is from itself after a trip to VST 3's scale and
    /// back.
    fn round_trip_error(expression: Expression, value: f64) -> f64 {
        let (type_id, position) = expression_to_vst(expression, value).unwrap();
        let (back, returned) = expression_from_vst(type_id, position).unwrap();
        assert_eq!(back, expression);
        (returned - value).abs()
    }

    #[test]
    fn note_expression_comes_back_within_its_stated_absolute_error() {
        // Values close to zero of either sign, where an absolute error is
        // many units in the last place. The powers of two are made exactly,
        // not with `powi`, which Miri lets be off in the last place.
        let near_zero: Vec<f64> = (1..=60)
            .flat_map(|power| {
                let value = 1.1 / (1u64 << power) as f64;
                [value, -value]
            })
            .collect();

        // Tuning: within 240 * 2^-52 semitones. Not exact, so the bound is
        // not idle: 0.1 is one of the values that comes back different.
        let mut extra = near_zero.clone();
        extra.extend([0.1, 1.65e-6, -1.65e-6, 120.0, -120.0]);
        let tuning = across(-120.0, 120.0, &extra)
            .into_iter()
            .map(|value| round_trip_error(Expression::Tuning, value))
            .fold(0.0, f64::max);
        assert!(
            tuning <= 240.0 * f64::EPSILON,
            "tuning is off by {tuning:e}"
        );
        assert!(round_trip_error(Expression::Tuning, 0.1) > 0.0);

        // Pan: within 2^-52.
        let mut extra = near_zero.clone();
        extra.extend([1.2e-6, -1.2e-6, 1.0, -1.0]);
        let pan = across(-1.0, 1.0, &extra)
            .into_iter()
            .map(|value| round_trip_error(Expression::Pan, value))
            .fold(0.0, f64::max);
        assert!(pan <= f64::EPSILON, "pan is off by {pan:e}");

        // Volume from 2^-1020 up, vibrato and brightness: exactly.
        let positive: Vec<f64> = near_zero.iter().map(|value| value.abs()).collect();
        let mut extra = positive.clone();
        // 2^-1020, the smallest value whose quarter is not subnormal.
        extra.extend([4.0 * f64::MIN_POSITIVE, 4.0]);
        for value in across(0.0, 4.0, &extra) {
            assert_eq!(
                round_trip_error(Expression::Volume, value),
                0.0,
                "{value:e}"
            );
        }
        for value in across(0.0, 1.0, &positive) {
            assert_eq!(round_trip_error(Expression::Vibrato, value), 0.0);
            assert_eq!(round_trip_error(Expression::Brightness, value), 0.0);
        }
    }

    #[test]
    fn controllers_go_to_the_parameter_they_are_mapped_to() {
        let mut map: MidiMap = [[vst::kNoParamId; CONTROLLERS]; 16];
        map[2][74] = 1_000;
        map[0][PITCH_BEND] = 1_001;
        map[0][CHANNEL_PRESSURE] = 1_002;
        let midi = |message| outgoing(&EventKind::Midi(message), Some(&map));
        assert_eq!(
            midi(MidiMessage::ControlChange {
                channel: 2,
                controller: 74,
                value: 127
            }),
            Outgoing::Point {
                id: 1_000,
                value: 1.0
            }
        );
        // The wheel at rest, all the way down and all the way up.
        for (wheel, position) in [(0, 8_192.0 / 16_383.0), (-8_192, 0.0), (8_191, 1.0)] {
            assert_eq!(
                midi(MidiMessage::PitchBend {
                    channel: 0,
                    value: wheel
                }),
                Outgoing::Point {
                    id: 1_001,
                    value: position
                }
            );
        }
        assert_eq!(
            midi(MidiMessage::ChannelPressure {
                channel: 0,
                pressure: 0
            }),
            Outgoing::Point {
                id: 1_002,
                value: 0.0
            }
        );
        // Unmapped, out of range, a program change, or no map at all.
        for message in [
            MidiMessage::ControlChange {
                channel: 1,
                controller: 74,
                value: 1,
            },
            MidiMessage::ProgramChange {
                channel: 0,
                program: 3,
            },
        ] {
            assert_eq!(midi(message), Outgoing::Nothing);
        }
        assert_eq!(
            outgoing(
                &EventKind::Midi(MidiMessage::ControlChange {
                    channel: 2,
                    controller: 74,
                    value: 1
                }),
                None
            ),
            Outgoing::Nothing
        );
        // A note that arrives as MIDI is a note.
        assert!(matches!(
            midi(MidiMessage::NoteOn {
                channel: 0,
                note: 60,
                velocity: 127
            }),
            Outgoing::Note(NoteEvent::On { pitch: 60, .. })
        ));
    }

    #[test]
    fn values_are_held_to_the_unit_range_and_modulation_is_not_sent() {
        let value = |value| EventKind::ParamValue {
            param: ParamId(5),
            value,
        };
        assert_eq!(
            outgoing(&value(1.5), None),
            Outgoing::Point { id: 5, value: 1.0 }
        );
        assert_eq!(
            outgoing(&value(-0.5), None),
            Outgoing::Point { id: 5, value: 0.0 }
        );
        assert_eq!(outgoing(&value(f64::NAN), None), Outgoing::Nothing);
        let modulation = EventKind::ParamModulation {
            param: ParamId(5),
            amount: 0.1,
            note: None,
        };
        assert_eq!(outgoing(&modulation, None), Outgoing::Nothing);
    }

    #[test]
    fn controller_output_becomes_midi_again() {
        let out = |control, value, value2| {
            incoming(NoteEvent::MidiOut {
                control,
                channel: 1,
                value,
                value2,
            })
        };
        assert_eq!(
            out(7, 100, 0),
            Some(EventKind::Midi(MidiMessage::ControlChange {
                channel: 1,
                controller: 7,
                value: 100
            }))
        );
        assert_eq!(
            out(129, 0, 64),
            Some(EventKind::Midi(MidiMessage::PitchBend {
                channel: 1,
                value: 0
            }))
        );
        assert_eq!(
            out(128, 5, 0),
            Some(EventKind::Midi(MidiMessage::ChannelPressure {
                channel: 1,
                pressure: 5
            }))
        );
        assert_eq!(out(200, 0, 0), None);
    }

    #[test]
    fn the_transport_is_described_field_by_field() {
        let mut transport = Transport::playing_at(97.0);
        transport.time_signature = Some((6, 8));
        transport.position_beats = Some(7.25);
        transport.bar_start_beats = Some(6.0);
        transport.position_samples = Some(-500);
        transport.loop_beats = Some((4.0, 8.0));
        let context = process_context(&transport, 44_100.0, 1_234);
        let has = |flag: u32| context.state & flag != 0;
        assert!(has(flags::kPlaying as u32) && !has(flags::kRecording as u32));
        assert!(has(flags::kTempoValid as u32) && context.tempo == 97.0);
        assert!(has(flags::kTimeSigValid as u32));
        assert_eq!(
            (context.timeSigNumerator, context.timeSigDenominator),
            (6, 8)
        );
        assert!(has(flags::kProjectTimeMusicValid as u32) && context.projectTimeMusic == 7.25);
        assert!(has(flags::kBarPositionValid as u32) && context.barPositionMusic == 6.0);
        assert!(has(flags::kCycleActive as u32) && has(flags::kCycleValid as u32));
        assert_eq!((context.cycleStartMusic, context.cycleEndMusic), (4.0, 8.0));
        assert_eq!(context.projectTimeSamples, -500);
        assert_eq!(context.continousTimeSamples, 1_234);
        assert_eq!(context.sampleRate, 44_100.0);

        // A stopped transport that knows nothing says so.
        let context = process_context(&Transport::STOPPED, 48_000.0, 0);
        assert_eq!(context.state, flags::kContTimeValid as u32);
        assert_eq!(context.projectTimeSamples, 0);
    }

    #[test]
    fn a_position_at_the_end_of_its_range_stops_there() {
        let mut transport = Transport::playing_at(120.0);
        transport.position_samples = Some(i64::MAX - 10);
        advance(&mut transport, 64, 48_000.0);
        assert_eq!(transport.position_samples, Some(i64::MAX));
        let mut ours = Transport::playing_at(97.0);
        let mut theirs = ours;
        advance(&mut ours, 4_096, 44_100.0);
        theirs.advance(4_096, 44_100.0);
        assert_eq!(ours, theirs);
    }
}
