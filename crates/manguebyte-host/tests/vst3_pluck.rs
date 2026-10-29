//! Hosts Mangue Pluck, the framework's example instrument, as a VST 3
//! plugin, through the shared library cargo builds for it, and holds the
//! result against the same plugin driven directly.
//!
//! As in the CLAP tests, the plugin is in this executable twice: once as an
//! ordinary Rust library, driven with `manguebyte_plugin::Harness`, and once
//! as the library a host loads, reached only through `GetPluginFactory` and
//! the VST 3 interfaces behind it. If the host does its job, the two cannot
//! be told apart.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "vst3", not(miri)))]

mod common;

use mangue_pluck::Pluck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_graph::Graph;
use manguebyte_host::vst3::{
    ClassId, HostEvent, ProcessFailure, ProcessStatus, Tail, Vst3Error, Vst3Module, Vst3Processor,
};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{Harness, Plugin};

use common::{assert_same_bits, for_harness_vst3, render, vst3_id};

const RATE: f64 = 48_000.0;

/// The class id the framework's wrapper derives from `org.manguebyte.pluck`,
/// as hosts print it. The wrapper's own tests pin the same number.
const CLASS: &str = "E3AC906E0FE4AF333672CEE11FA34823";

fn class() -> ClassId {
    CLASS.parse().unwrap()
}

/// A parameter's VST 3 id.
fn id(name: &str) -> ParamId {
    vst3_id(ParamId::from_name(name))
}

fn cutoff() -> ParamId {
    id("cutoff")
}
fn resonance() -> ParamId {
    id("resonance")
}
fn decay() -> ParamId {
    id("decay")
}
fn release() -> ParamId {
    id("release")
}
fn reverb() -> ParamId {
    id("reverb")
}
fn level() -> ParamId {
    id("level")
}

fn module() -> Vst3Module {
    common::load_vst3(common::built_library("mangue_pluck")).expect("Mangue Pluck loads")
}

/// A few seconds of what VST 3 can carry: notes with and without ids, two
/// notes on one key, polyphonic pressure, note expression, automation and
/// the pitch wheel, each on a frame of its own. Values are normalized, as
/// a VST 3 host has them; every parameter of this plugin is continuous, so
/// they are the same positions the CLAP tests use.
///
/// Expression values are ones whose conversion to VST 3's scale and back
/// is exact, so that the comparison can be bit for bit: a tuning of
/// 0.46875 semitones is 2^-9 of VST 3's range away from its middle.
fn song() -> Vec<(u64, EventKind)> {
    let low = NoteRef::with_id(0, 45, 1);
    let fifth = NoteRef::with_id(0, 52, 2);
    let plain = NoteRef::new(1, 57);
    let again = NoteRef::with_id(0, 45, 3);
    let on = |note, velocity| EventKind::NoteOn { note, velocity };
    let off = |note, velocity| EventKind::NoteOff { note, velocity };
    let shape = |note, expression, value| EventKind::NoteExpression {
        note,
        expression,
        value,
    };
    let value = |param, value| EventKind::ParamValue { param, value };
    vec![
        (100, on(low, 0.8)),
        (2_500, value(resonance(), 0.6)),
        (3_000, shape(low, Expression::Tuning, 0.468_75)),
        (4_000, on(fifth, 0.7)),
        (4_001, on(plain, 0.6)),
        (4_500, shape(fifth, Expression::Pressure, 0.75)),
        (5_000, on(again, 0.5)),
        (
            5_500,
            EventKind::Midi(MidiMessage::PitchBend {
                channel: 0,
                value: 2_000,
            }),
        ),
        (6_000, shape(fifth, Expression::Volume, 0.5)),
        (6_001, off(plain, 0.0)),
        (7_777, value(decay(), 0.8)),
        (9_000, value(release(), 0.1)),
        (9_100, off(low, 0.3)),
        (10_000, value(reverb(), 0.6)),
        (11_000, off(again, 0.0)),
        (12_000, value(level(), 0.9)),
        (13_000, off(fifth, 0.0)),
        (
            13_500,
            EventKind::Midi(MidiMessage::PitchBend {
                channel: 0,
                value: 0,
            }),
        ),
        (14_000, on(low, 1.0)),
        (16_000, off(low, 0.0)),
    ]
}

const SONG_FRAMES: usize = 24_000;

/// `message` on each of the 16 MIDI channels, in channel order, if it is a
/// controller move (a control change, channel pressure or the pitch wheel),
/// and otherwise `message` alone.
///
/// That is what the VST 3 wrapper delivers for a plugin that shares its
/// controllers between channels, as Pluck does with the default
/// `MidiControllers::MUSICAL`: VST 3 carries a controller move as a change
/// of the one hidden parameter every channel is mapped to, so the wrapper
/// cannot tell which channel it came from, and hands it to every channel so
/// that notes on any of them hear it.
fn as_delivered(message: MidiMessage) -> Vec<MidiMessage> {
    let on = |channel: u8| match message {
        MidiMessage::ControlChange {
            controller, value, ..
        } => Some(MidiMessage::ControlChange {
            channel,
            controller,
            value,
        }),
        MidiMessage::ChannelPressure { pressure, .. } => {
            Some(MidiMessage::ChannelPressure { channel, pressure })
        }
        MidiMessage::PitchBend { value, .. } => Some(MidiMessage::PitchBend { channel, value }),
        _ => None,
    };
    match on(0) {
        Some(_) => (0..16).filter_map(on).collect(),
        None => vec![message],
    }
}

/// The song as the plugin plays it with nothing in between, given exactly
/// the events in `schedule`.
fn played_alone(max_block: usize, pattern: &[usize], schedule: &[(u64, EventKind)]) -> AudioBuffer {
    let mut harness = Harness::<Pluck>::new(RATE, max_block).with_block_pattern(pattern);
    let fresh = Pluck::new().params();
    for &(frame, kind) in schedule {
        harness.schedule(frame, for_harness_vst3(&*fresh, kind));
    }
    harness.render(SONG_FRAMES)
}

/// The song as the plugin plays it with nothing in between, given the
/// events a VST 3 host delivers for `schedule`: each controller move on
/// every channel (see [`as_delivered`]).
fn reference(max_block: usize, pattern: &[usize], schedule: &[(u64, EventKind)]) -> AudioBuffer {
    let mut delivered = Vec::with_capacity(schedule.len());
    for &(frame, kind) in schedule {
        match kind {
            EventKind::Midi(message) => delivered.extend(
                as_delivered(message)
                    .into_iter()
                    .map(|message| (frame, EventKind::Midi(message))),
            ),
            other => delivered.push((frame, other)),
        }
    }
    played_alone(max_block, pattern, &delivered)
}

/// The song as the plugin plays it when hosted, and what hosting cost.
fn hosted(
    module: &Vst3Module,
    max_block: usize,
    pattern: &[usize],
    schedule: &[(u64, EventKind)],
) -> (AudioBuffer, common::Rendered) {
    let mut plugin = module.create(class()).expect("the plugin is created");
    let mut processor = plugin.activate(RATE, max_block).expect("it activates");
    let mut audio = AudioBuffer::<f32>::new(2, SONG_FRAMES);
    let rendered = render(
        &mut processor,
        RATE,
        schedule,
        pattern,
        max_block,
        &mut audio,
        None,
    );
    assert_eq!(processor.status(), ProcessStatus::Processed);
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0, "an event was dropped");
    assert_eq!(
        plugin.undeliverable_events(),
        0,
        "an event did not reach the plugin"
    );
    (audio, rendered)
}

/// A note-on event at `offset`.
fn note_on(offset: u32, note: NoteRef) -> Event {
    Event {
        offset,
        kind: EventKind::NoteOn {
            note,
            velocity: 1.0,
        },
    }
}

// --- What the plugin is ---------------------------------------------------

#[test]
fn the_module_lists_what_the_plugin_declares() {
    let path = common::built_library("mangue_pluck");
    let module = common::load_vst3(&path).unwrap();
    assert_eq!(module.path(), path);
    assert_eq!(module.factory_info().vendor, "Manguebyte");

    let [class_info] = module.classes() else {
        panic!("expected one class, found {:?}", module.classes());
    };
    let info = Pluck::INFO;
    assert_eq!(class_info.id, class());
    assert_eq!(class_info.id.to_string(), CLASS);
    assert_eq!(class_info.name, info.name);
    assert_eq!(class_info.category, "Audio Module Class");
    assert!(class_info.is_audio_module() && class_info.is_instrument());
    assert_eq!(class_info.sub_categories, ["Instrument", "Synth"]);
    assert_eq!(class_info.vendor, info.vendor);
    assert_eq!(class_info.version, info.version);
    assert!(class_info.sdk_version.starts_with("VST 3."));
    assert_eq!(module.class(class()), Some(class_info));

    // The same file again is the same module, and a handle can be used
    // from another thread.
    let again = common::load_vst3(&path).unwrap();
    assert_eq!(again, module);
    let listed = std::thread::spawn(move || again.classes().len())
        .join()
        .unwrap();
    assert_eq!(listed, 1);

    let plugin = module.create(class()).unwrap();
    assert_eq!(plugin.class(), class_info);
    assert!(plugin.has_controller());
    assert!(!plugin.is_active() && !plugin.has_processor());
}

/// `Vst3Module::path` is the path a handle was loaded from, as it was
/// given to `load`. A second `load` of the same file, spelled another way,
/// shares the first module but keeps its own spelling.
#[test]
fn a_module_reports_the_path_it_was_given() {
    let built = common::built_library("mangue_pluck");
    let first = common::load_vst3(&built).unwrap();
    // `Path` comparison drops `.` but keeps `..`, so this is another path
    // to the same file.
    let folder = built.parent().unwrap();
    let respelled = folder
        .join("..")
        .join(folder.file_name().unwrap())
        .join(built.file_name().unwrap());
    assert_ne!(respelled, built);
    let second = common::load_vst3(&respelled).unwrap();
    assert_eq!(second, first, "the same file is one module");
    assert_eq!(first.path(), built.as_path());
    assert_eq!(
        second.path(),
        respelled.as_path(),
        "the module reports a path other than the one it was loaded from"
    );
    // A copy of a handle is the same handle.
    assert_eq!(second.clone().path(), respelled.as_path());
}

#[test]
fn buses_follow_the_layout_the_plugin_declares() {
    let plugin = module().create(class()).unwrap();

    // An instrument: no audio in, one stereo bus out, one event bus in.
    assert!(plugin.audio_inputs().is_empty());
    let [output] = plugin.audio_outputs() else {
        panic!("expected one output, found {:?}", plugin.audio_outputs());
    };
    assert_eq!(output.channels as usize, Pluck::LAYOUT.outputs);
    assert!(output.is_main && output.default_active);
    assert_eq!(output.name, "Output");
    // Left and right, one bit each.
    assert_eq!(output.arrangement, 0b11);
    let [notes] = plugin.event_inputs() else {
        panic!(
            "expected one event input, found {:?}",
            plugin.event_inputs()
        );
    };
    assert_eq!((notes.name.as_str(), notes.channels), ("Notes", 16));
    assert!(plugin.event_outputs().is_empty());

    assert_eq!(plugin.latency(), 0);
    assert_eq!(
        plugin.tail(),
        Tail::Infinite,
        "the wrapper declares no tail"
    );

    // No bus is on before the first activation; then the ones the host
    // uses are, and they stay on.
    assert!(!output.active && !notes.active);
    let mut plugin = plugin;
    let processor = plugin.activate(RATE, 256).unwrap();
    assert!(plugin.audio_outputs()[0].active && plugin.event_inputs()[0].active);
    // Pluck's own tail is the maximum release plus the reverb decay, which is 7.2 seconds.
    let seconds = mangue_pluck::MAX_RELEASE_SECONDS + mangue_pluck::REVERB_DECAY_SECONDS;
    let frames = (seconds * RATE).ceil() as u32;
    assert_eq!(processor.tail(), Tail::Frames(frames));
    drop(processor);
    plugin.deactivate().unwrap();
    assert!(plugin.audio_outputs()[0].active && plugin.event_inputs()[0].active);
}

#[test]
fn parameters_are_the_ones_the_plugin_declares() {
    let plugin = module().create(class()).unwrap();
    let declared = Pluck::new().params();

    let mut expected: Vec<&dyn Param> = Vec::new();
    declared.visit(&mut |param| expected.push(param));
    assert_eq!(expected.len(), 6);
    // The plugin's own six, then one hidden parameter for each of the 13
    // controllers of the default `MidiControllers::MUSICAL`, shared by every
    // MIDI channel. (Before plugins declared their controllers, it was one
    // for each of the 130 VST 3 numbers on each of 16 channels: 2,086 in
    // all.)
    assert!(!Pluck::MIDI_CONTROLLERS.is_per_channel());
    assert_eq!(Pluck::MIDI_CONTROLLERS.len(), 13);
    assert_eq!(plugin.params().len(), 6 + 13);

    for (listed, declared) in plugin.params().iter().zip(expected) {
        assert_eq!(listed.id, vst3_id(declared.param_id()));
        assert_eq!(listed.title, declared.name());
        assert_eq!(listed.units, declared.info().unit());
        assert_eq!(listed.step_count, 0);
        assert_eq!(listed.default, declared.default_normalized());
        assert_eq!(listed.unit_id, 0);
        let flags = declared.info().flags();
        assert_eq!(listed.flags.automatable, flags.automatable);
        assert_eq!(listed.flags.hidden, flags.hidden);
        assert_eq!(listed.flags.bypass, flags.bypass);

        assert_eq!(plugin.param(listed.id), Some(listed));
        assert_eq!(
            plugin.param_value(listed.id),
            Some(declared.default_normalized())
        );
        // Plain values are the plugin's own unit, worked out by the plugin.
        assert_eq!(
            plugin.normalized_to_plain(listed.id, 0.25),
            Some(declared.normalized_to_plain(0.25))
        );
        let plain = declared.normalized_to_plain(0.75);
        assert_eq!(
            plugin.plain_to_normalized(listed.id, plain),
            Some(declared.plain_to_normalized(plain))
        );
    }
    for hidden in &plugin.params()[6..] {
        assert!(hidden.flags.hidden && !hidden.flags.automatable);
        assert_eq!(hidden.unit_id, 1);
    }

    // The controllers the plugin maps, which is how VST 3 delivers them.
    let bend = plugin
        .midi_mapping(0, 129)
        .expect("the pitch wheel is mapped");
    let sustain = plugin
        .midi_mapping(0, 64)
        .expect("the sustain pedal is mapped");
    assert_ne!(bend, sustain);
    assert!(plugin.param(bend).is_some() && plugin.param(sustain).is_some());
    // Every channel is mapped to the same parameter.
    for channel in 1..16 {
        assert_eq!(plugin.midi_mapping(channel, 129), Some(bend), "{channel}");
        assert_eq!(plugin.midi_mapping(channel, 64), Some(sustain), "{channel}");
    }
    // Only what the plugin declared: the modulation wheel (CC 1) is, the
    // filter cutoff (CC 74) is not.
    assert!(plugin.midi_mapping(0, 1).is_some());
    assert_eq!(plugin.midi_mapping(0, 74), None);
    assert_eq!(plugin.midi_mapping(16, 64), None);
    assert_eq!(plugin.midi_mapping(0, 130), None);
    let titles: Vec<_> = plugin.params()[6..]
        .iter()
        .map(|hidden| hidden.title.as_str())
        .collect();
    assert!(titles.contains(&"MIDI CC 64"), "{titles:?}");
    assert!(titles.contains(&"MIDI Pitch Bend"), "{titles:?}");

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(plugin.param(nothing), None);
    assert_eq!(plugin.param_value(nothing), None);
    assert_eq!(plugin.normalized_to_plain(nothing, 0.5), None);
    assert_eq!(plugin.plain_to_normalized(cutoff(), f64::NAN), None);
}

#[test]
fn text_goes_both_ways_in_the_plugins_own_words() {
    let plugin = module().create(class()).unwrap();
    let declared = Pluck::new().params();

    let mut checked = 0;
    declared.visit(&mut |param| {
        let id = vst3_id(param.param_id());
        for position in [0.0, 0.25, 0.5, 1.0] {
            // VST 3 keeps the unit in a field of its own, so the text is
            // the value alone.
            let expected = param.plain_to_text(param.normalized_to_plain(position));
            let text = plugin.param_to_text(id, position).unwrap();
            assert_eq!(text, expected);

            let back = plugin.text_to_param(id, &text).unwrap();
            let direct = param.plain_to_normalized(param.text_to_plain(&text).unwrap());
            assert_eq!(back, direct);
            checked += 1;
        }
        assert_eq!(plugin.text_to_param(id, "not a number"), None);
    });
    assert_eq!(checked, 24);

    assert_eq!(plugin.param_to_text(cutoff(), 0.0).unwrap(), "100");
    assert_eq!(plugin.param_to_text(level(), 1.0).unwrap(), "6.0");
    assert_eq!(plugin.text_to_param(level(), "6 dB"), Some(1.0));
    assert_eq!(plugin.text_to_param(level(), "-40"), Some(0.0));
    // Out of range is held to it, and not a number is no answer.
    assert_eq!(
        plugin.param_to_text(level(), 7.0),
        plugin.param_to_text(level(), 1.0)
    );
    assert_eq!(plugin.param_to_text(level(), f64::NAN), None);

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(plugin.param_to_text(nothing, 0.5), None);
    assert_eq!(plugin.text_to_param(nothing, "1"), None);
}

// --- Sound ----------------------------------------------------------------

#[test]
fn the_hosted_plugin_sounds_exactly_as_the_plugin_does_alone() {
    let module = module();
    let schedule = song();
    let cases: [(usize, &[usize]); 4] = [
        (256, &[256]),
        (64, &[64]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
        (1_024, &[997, 1_024, 3]),
    ];
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let expected = reference(max_block, pattern, &schedule);
        assert!(expected.peak() > 0.05, "{what}: the reference is silent");
        assert!(
            expected.channel(0)[..100]
                .iter()
                .all(|&sample| sample == 0.0),
            "{what}: sound before the first note"
        );

        let (actual, rendered) = hosted(&module, max_block, pattern, &schedule);
        assert_same_bits(&actual, &expected, &what);
        assert_eq!(
            rendered.heap_calls, 0,
            "{what}: the host used the heap while processing"
        );
    }
}

#[test]
fn every_kind_of_event_is_heard() {
    // Bit-identical output would also follow from the host delivering
    // nothing, if the reference were deaf too. So each kind of event, taken
    // out of the song, changes what the hosted plugin plays.
    let module = module();
    let whole = song();
    let (complete, _) = hosted(&module, 256, &[256], &whole);

    type IsKind = fn(&EventKind) -> bool;
    let kinds: [(&str, IsKind); 6] = [
        ("note on", |kind| matches!(kind, EventKind::NoteOn { .. })),
        ("note off", |kind| matches!(kind, EventKind::NoteOff { .. })),
        ("tuning", |kind| {
            matches!(
                kind,
                EventKind::NoteExpression {
                    expression: Expression::Tuning,
                    ..
                }
            )
        }),
        ("volume", |kind| {
            matches!(
                kind,
                EventKind::NoteExpression {
                    expression: Expression::Volume,
                    ..
                }
            )
        }),
        ("polyphonic pressure", |kind| {
            matches!(
                kind,
                EventKind::NoteExpression {
                    expression: Expression::Pressure,
                    ..
                }
            )
        }),
        ("the pitch wheel", |kind| matches!(kind, EventKind::Midi(_))),
    ];
    for (name, is_kind) in kinds {
        let without: Vec<_> = whole
            .iter()
            .copied()
            .filter(|(_, kind)| !is_kind(kind))
            .collect();
        assert!(without.len() < whole.len(), "the song has no {name}");
        let (partial, _) = hosted(&module, 256, &[256], &without);
        assert!(partial != complete, "{name} made no difference");
    }
    for param in [resonance(), decay(), release(), reverb(), level()] {
        let without: Vec<_> = whole
            .iter()
            .copied()
            .filter(
                |(_, kind)| !matches!(kind, EventKind::ParamValue { param: p, .. } if *p == param),
            )
            .collect();
        assert_eq!(without.len(), whole.len() - 1);
        let (partial, _) = hosted(&module, 256, &[256], &without);
        assert!(
            partial != complete,
            "automation of {param:?} made no difference"
        );
    }
}

#[test]
fn a_shared_controller_reaches_notes_on_every_channel() {
    // Pluck shares its controllers between channels, so in VST 3 a move of
    // the pitch wheel or the sustain pedal reaches notes on every channel,
    // whatever channel the host had it on: a note on channel 6 is bent by
    // the wheel on channel 1 exactly as by its own channel's wheel, and held
    // by the pedal the same way.
    let module = module();
    let note = NoteRef::new(5, 57);
    let schedule = |channel: u8| {
        vec![
            (
                100,
                EventKind::NoteOn {
                    note,
                    velocity: 0.8,
                },
            ),
            (
                2_000,
                EventKind::Midi(MidiMessage::PitchBend {
                    channel,
                    value: 4_000,
                }),
            ),
            (
                3_000,
                EventKind::Midi(MidiMessage::ControlChange {
                    channel,
                    controller: 64,
                    value: 127,
                }),
            ),
            (
                4_000,
                EventKind::NoteOff {
                    note,
                    velocity: 0.0,
                },
            ),
        ]
    };
    let own_channel = played_alone(256, &[256], &schedule(5));
    let untouched = played_alone(256, &[256], &schedule(5)[..1]);
    assert!(
        own_channel != untouched,
        "the wheel and the pedal made no difference"
    );
    for (max_block, pattern) in [(256usize, &[256usize][..]), (512, &[1, 511, 7, 300])] {
        let (hosted_audio, _) = hosted(&module, max_block, pattern, &schedule(0));
        assert_same_bits(
            &hosted_audio,
            &own_channel,
            &format!("controllers on channel 1, blocks of {pattern:?}"),
        );
    }
}

#[test]
fn automation_lands_on_its_frame_through_the_host() {
    // A point every 61 frames for the decay, which the voices follow on
    // their frame, while a note sounds: each one is heard where it is
    // placed, whichever way the host cuts the blocks.
    let mut schedule = vec![(
        0,
        EventKind::NoteOn {
            note: NoteRef::with_id(0, 40, 1),
            velocity: 1.0,
        },
    )];
    for step in 1..60u64 {
        schedule.push((
            step * 61,
            EventKind::ParamValue {
                param: decay(),
                value: (step % 7) as f64 / 7.0,
            },
        ));
    }
    let module = module();
    let expected = reference(128, &[128], &schedule);
    for (max_block, pattern) in [(128usize, &[128usize][..]), (512, &[13, 512, 1])] {
        let (actual, rendered) = hosted(&module, max_block, pattern, &schedule);
        let what = format!("blocks of {pattern:?}");
        assert_same_bits(&actual, &expected, &what);
        assert_eq!(rendered.heap_calls, 0, "{what}");
    }
    // And the points are what make the difference: with the decay left
    // where it starts, the note sounds otherwise.
    let still = reference(128, &[128], &schedule[..1]);
    assert!(still != expected, "the automation made no difference");
}

#[test]
fn the_sound_does_not_depend_on_how_the_host_cuts_time() {
    let module = module();
    // The whole song: the plugin follows every event on its frame, the
    // reverb's blend and the output level included.
    let schedule = song();
    let (reference, _) = hosted(&module, 256, &[256], &schedule);
    assert!(reference.peak() > 0.05);

    for (max_block, pattern) in [
        (64usize, &[64usize][..]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
        (997, &[997]),
        (4_096, &[4_096]),
    ] {
        let (actual, rendered) = hosted(&module, max_block, pattern, &schedule);
        assert_same_bits(
            &actual,
            &reference,
            &format!("blocks of {pattern:?}, at most {max_block}"),
        );
        assert_eq!(rendered.heap_calls, 0);
    }

    // A caller may hand over more than the plugin was activated for; the
    // host then cuts the block up itself.
    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 64).unwrap();
    assert_eq!(processor.max_block_size(), 64);
    let mut audio = AudioBuffer::<f32>::new(2, SONG_FRAMES);
    let rendered = render(
        &mut processor,
        RATE,
        &schedule,
        &[5_000, 64, 1_333],
        usize::MAX,
        &mut audio,
        None,
    );
    assert_same_bits(&audio, &reference, "blocks longer than the plugin takes");
    assert_eq!(rendered.heap_calls, 0);
    assert!(rendered.blocks < 20, "the test did not send long blocks");
}

#[test]
fn processing_never_uses_the_heap() {
    assert!(
        common::heap_calls_during(|| drop(std::hint::black_box(vec![0u8; 64]))) >= 2,
        "the heap watcher is not working"
    );

    let mut plugin = module().create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 128).unwrap();
    let transport = Transport::playing_at(120.0);

    // More points in one block than the host has room for: the rest are
    // dropped and counted, not allocated for.
    let mut flood = EventBuffer::with_capacity(6_000);
    for index in 0..6_000u32 {
        let kind = EventKind::ParamValue {
            param: resonance(),
            value: f64::from(index % 100) / 100.0,
        };
        flood
            .push(Event {
                offset: index % 128,
                kind,
            })
            .unwrap();
    }
    let mut notes = EventBuffer::with_capacity(8);
    let note = NoteRef::with_id(0, 60, 9);
    notes.push(note_on(3, note)).unwrap();
    // Beyond the end of a 128-frame block: kept for the next one.
    notes
        .push(Event {
            offset: 500,
            kind: EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
        })
        .unwrap();
    let mut midi = EventBuffer::with_capacity(8);
    for (offset, message) in [
        (
            1,
            MidiMessage::ControlChange {
                channel: 0,
                controller: 64,
                value: 127,
            },
        ),
        (
            2,
            MidiMessage::PitchBend {
                channel: 0,
                value: -100,
            },
        ),
        (
            3,
            MidiMessage::ChannelPressure {
                channel: 2,
                pressure: 9,
            },
        ),
    ] {
        midi.push(Event::from_midi(offset, message)).unwrap();
    }

    let mut block = AudioBuffer::<f32>::new(2, 128);
    let mut long = AudioBuffer::<f32>::new(2, 1_000);
    let mut empty = AudioBuffer::<f32>::new(2, 0);
    for index in 0..50 {
        plugin.set_param(cutoff(), f64::from(index) / 50.0).unwrap();
    }
    let heap_calls = common::heap_calls_during(|| {
        processor.process(&mut block);
        processor.process_with(&mut block, &ProcessContext::new(&notes, &transport));
        processor.process_with(&mut block, &ProcessContext::new(&flood, &transport));
        processor.process_with(&mut long, &ProcessContext::new(&notes, &transport));
        processor.process_with(&mut block, &ProcessContext::new(&midi, &transport));
        processor.process_with(&mut empty, &ProcessContext::new(&notes, &transport));
        processor.flush();
        processor.reset();
        processor.process(&mut block);
        let _ = (
            processor.status(),
            processor.output_events().len(),
            processor.latency_samples(),
            processor.tail(),
        );
    });
    assert_eq!(heap_calls, 0, "the host used the heap on the audio thread");
    assert_eq!(processor.status(), ProcessStatus::Processed);
    // 6,000 points in a block that has room for 4,096.
    assert_eq!(plugin.lost_events(), 6_000 - 4_096);
    assert_eq!(plugin.undeliverable_events(), 0);

    // The controller is told what the processor was sent.
    plugin.pump();
    assert_eq!(plugin.param_value(cutoff()), Some(49.0 / 50.0));
}

// --- Parameters and state -------------------------------------------------

#[test]
fn a_parameter_set_on_the_main_thread_reaches_the_plugin() {
    let module = module();
    let mut plugin = module.create(class()).unwrap();

    // Not active: the controller has the value at once, and the processor
    // will have it on the first frame after activation.
    plugin.set_param(resonance(), 0.75).unwrap();
    assert_eq!(plugin.param_value(resonance()), Some(0.75));
    // Held to the normalized range.
    plugin.set_param(reverb(), 7.0).unwrap();
    assert_eq!(plugin.param_value(reverb()), Some(1.0));
    plugin.set_param(reverb(), 0.0).unwrap();

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(
        plugin.set_param(nothing, 0.5),
        Err(Vst3Error::UnknownParam(nothing))
    );
    assert!(matches!(
        plugin.set_param(cutoff(), f64::NAN),
        Err(Vst3Error::InvalidArgument(_))
    ));

    // Active: the value crosses to the audio half and arrives with the
    // next block, as a point on its first frame.
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let note = NoteRef::new(0, 45);
    let play = [(
        0,
        EventKind::NoteOn {
            note,
            velocity: 1.0,
        },
    )];
    let mut first = AudioBuffer::<f32>::new(2, 4_096);
    render(&mut processor, RATE, &play, &[256], 256, &mut first, None);
    plugin.set_param(cutoff(), 0.95).unwrap();
    let mut second = AudioBuffer::<f32>::new(2, 4_096);
    render(&mut processor, RATE, &[], &[256], 256, &mut second, None);
    plugin.pump();
    assert_eq!(plugin.param_value(cutoff()), Some(0.95));

    // Exactly as if automation had put it there.
    let declared = Pluck::new().params();
    let plain = |id, value| {
        common::by_vst3_id(&*declared, id)
            .unwrap()
            .normalized_to_plain(value)
    };
    let mut harness = Harness::<Pluck>::new(RATE, 256);
    harness
        .params()
        .resonance
        .set(plain(resonance(), 0.75) as f32);
    harness.params().reverb.set(plain(reverb(), 0.0) as f32);
    harness.schedule(0, play[0].1);
    harness.schedule(
        4_096,
        EventKind::ParamValue {
            param: ParamId::from_name("cutoff"),
            value: plain(cutoff(), 0.95),
        },
    );
    let expected = harness.render(8_192);
    let mut actual = AudioBuffer::<f32>::new(2, 8_192);
    for channel in 0..2 {
        actual.channel_mut(channel)[..4_096].copy_from_slice(first.channel(channel));
        actual.channel_mut(channel)[4_096..].copy_from_slice(second.channel(channel));
    }
    assert_same_bits(&actual, &expected, "a value set from the main thread");
    assert!(second != first, "the value changed nothing");

    // The queue to the audio thread is bounded, and says when it is full.
    let mut accepted = 0;
    let full = loop {
        match plugin.set_param(decay(), 0.5) {
            Ok(()) => accepted += 1,
            Err(error) => break error,
        }
        assert!(accepted < 100_000, "the queue never filled");
    };
    assert_eq!(full, Vst3Error::QueueFull);
    assert_eq!(accepted, 1_024);
    processor.process(&mut AudioBuffer::<f32>::new(2, 16));
    plugin.set_param(decay(), 0.25).unwrap();

    // A value still in the queue when the audio half goes is not lost: the
    // plugin has it once it is inactive, and its processor hears it in the
    // first block after the next activation.
    drop(processor);
    plugin.deactivate().unwrap();
    assert_eq!(plugin.param_value(decay()), Some(0.25));
    let saved = String::from_utf8(component_part(&plugin.save_state().unwrap())).unwrap();
    let alone = Harness::<Pluck>::new(RATE, 64);
    alone.load_state(&saved).unwrap();
    assert_eq!(
        alone.params().decay.value(),
        plain(decay(), 0.25) as f32,
        "the component does not have the last value"
    );
}

/// The normalized value the plugin shows for a parameter once it has been
/// set to `value` and kept it as the plugin keeps values.
fn settled(id: ParamId, value: f64) -> f64 {
    let declared = Pluck::new().params();
    let param = common::by_vst3_id(&*declared, id).unwrap();
    param.set_normalized(value);
    param.normalized()
}

/// The component's part of a state saved by the host, which for this
/// plugin is the framework's own text.
fn component_part(state: &[u8]) -> Vec<u8> {
    // 8 bytes of magic, 16 of class, then the length and the bytes.
    let length = u64::from_le_bytes(state[24..32].try_into().unwrap()) as usize;
    state[32..32 + length].to_vec()
}

#[test]
fn state_survives_a_round_trip_through_the_hosts_streams() {
    let module = module();
    let mut plugin = module.create(class()).unwrap();
    plugin.set_param(cutoff(), 0.8).unwrap();
    plugin.set_param(level(), 0.3).unwrap();
    let saved = plugin.save_state().unwrap();

    // One run of bytes: the host's header, the class, the component's
    // state, which is the plugin's own and which the plugin with no host
    // reads, and the controller's, which this plugin keeps in its
    // component.
    assert_eq!(&saved[..8], b"MBVST3\0\x01");
    let text = String::from_utf8(component_part(&saved)).unwrap();
    assert!(text.starts_with("manguebyte-plugin 1 org.manguebyte.pluck\n"));
    assert_eq!(
        &saved[saved.len() - 8..],
        &[0; 8],
        "the controller saves nothing"
    );
    let alone = Harness::<Pluck>::new(RATE, 64);
    assert!(alone.load_state(&text).unwrap().is_exact());
    assert_eq!(alone.save_state(), text);

    // Streams that move a few bytes per call give the same bytes.
    for chunk in [1, 3, 7, 4_096] {
        plugin.set_stream_chunk(chunk);
        assert_eq!(
            plugin.save_state().unwrap(),
            saved,
            "{chunk} bytes per call"
        );
    }

    // Another instance takes it, whole or in crumbs.
    for chunk in [0, 1, 5] {
        let mut other = module.create(class()).unwrap();
        assert_ne!(other.param_value(cutoff()), plugin.param_value(cutoff()));
        other.set_stream_chunk(chunk);
        other.load_state(&saved).unwrap();
        for param in &plugin.params()[..6] {
            // The plugin keeps each value as a 32-bit float in its own
            // unit, and its controller shows what it keeps: close to what
            // the first instance was told, and not always equal to it.
            let told = plugin.param_value(param.id).unwrap();
            assert_eq!(
                other.param_value(param.id),
                Some(settled(param.id, told)),
                "{} after loading {chunk} bytes per call",
                param.title
            );
        }
        assert_eq!(other.save_state().unwrap(), saved);
        assert!(other.pump().is_empty());
    }

    // Damaged, cut short or meant for someone else: refused, and nothing
    // is changed.
    let mut other = module.create(class()).unwrap();
    let before = other.param_value(cutoff());
    assert!(matches!(
        other.load_state(b""),
        Err(Vst3Error::NotAState(_))
    ));
    assert!(matches!(
        other.load_state(b"not a state"),
        Err(Vst3Error::NotAState(_))
    ));
    let mut foreign = saved.clone();
    foreign[8] ^= 0xFF;
    assert!(matches!(
        other.load_state(&foreign),
        Err(Vst3Error::StateOfAnotherClass(_))
    ));
    // A state of the right class whose component part the plugin refuses.
    let mut garbled = saved.clone();
    let start = 32;
    garbled[start..start + 8].copy_from_slice(b"garbage!");
    assert_eq!(other.load_state(&garbled), Err(Vst3Error::StateLoadFailed));
    assert_eq!(other.param_value(cutoff()), before);
    // Cut anywhere, the state is refused, and nothing crashes.
    for length in 0..saved.len() {
        assert!(
            other.load_state(&saved[..length]).is_err(),
            "cut at {length}"
        );
    }
    other.load_state(&saved).unwrap();
    assert_eq!(other.param_value(cutoff()), Some(settled(cutoff(), 0.8)));

    // State restored while the plugin runs is heard from the next block,
    // exactly as when the plugin is restored with no host in between.
    let mut running = module.create(class()).unwrap();
    let mut processor = running.activate(RATE, 256).unwrap();
    let play = [(
        0,
        EventKind::NoteOn {
            note: NoteRef::new(0, 50),
            velocity: 1.0,
        },
    )];
    let mut first = AudioBuffer::<f32>::new(2, 2_048);
    render(&mut processor, RATE, &play, &[256], 256, &mut first, None);
    running.load_state(&saved).unwrap();
    let mut second = AudioBuffer::<f32>::new(2, 2_048);
    render(&mut processor, RATE, &[], &[256], 256, &mut second, None);

    let mut harness = Harness::<Pluck>::new(RATE, 256);
    harness.schedule(0, play[0].1);
    let expected_first = harness.render(2_048);
    harness.load_state(&text).unwrap();
    let expected_second = harness.render(2_048);
    assert_same_bits(&first, &expected_first, "before the state was loaded");
    assert_same_bits(&second, &expected_second, "after the state was loaded");
    assert!(processor.output_events().is_empty());
}

// --- A loaded state has the last word ---------------------------------------
//
// Parameter values are on their way between the two halves all the time.
// A state loaded while some are replaces them all: none of them may reach
// the plugin, or be told to its controller, after the state.

/// A state saved from a fresh Mangue Pluck whose cutoff was set to `value`
/// while it was inactive.
fn state_with_cutoff(module: &Vst3Module, value: f64) -> Vec<u8> {
    let mut plugin = module.create(class()).unwrap();
    plugin.set_param(cutoff(), value).unwrap();
    plugin.save_state().unwrap()
}

/// One block in which the cutoff is automated to `value` on frame 0.
fn automate_cutoff(processor: &mut Vst3Processor, value: f64) {
    let mut events = EventBuffer::with_capacity(4);
    events
        .push(Event {
            offset: 0,
            kind: EventKind::ParamValue {
                param: cutoff(),
                value,
            },
        })
        .unwrap();
    let mut block = AudioBuffer::<f32>::new(2, 256);
    processor.process_with(
        &mut block,
        &ProcessContext::new(&events, &Transport::STOPPED),
    );
}

/// `load_state` while audio runs, before the main thread has called `pump`
/// since the last block: the next `pump` must not tell the controller the
/// value the audio half delivered before the state was loaded.
#[test]
fn pump_does_not_put_back_a_value_a_loaded_state_replaced() {
    let module = module();
    let saved = state_with_cutoff(&module, 0.8);

    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    automate_cutoff(&mut processor, 0.1);
    plugin.load_state(&saved).unwrap();
    let loaded = plugin.param_value(cutoff());
    assert_eq!(
        loaded,
        Some(settled(cutoff(), 0.8)),
        "the state was not loaded"
    );

    plugin.pump();
    assert_eq!(
        plugin.param_value(cutoff()),
        loaded,
        "pump told the controller the automation value that the loaded state replaced"
    );
    drop(processor);
}

/// The same, once the audio half has stopped: the stale value must not
/// reach the plugin itself, which would then save a state other than the
/// one it was given.
#[test]
fn a_state_loaded_after_the_audio_half_stopped_is_the_state_the_plugin_keeps() {
    let module = module();
    let saved = state_with_cutoff(&module, 0.8);

    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    automate_cutoff(&mut processor, 0.1);
    drop(processor);
    plugin.load_state(&saved).unwrap();
    plugin.deactivate().unwrap();

    // The plugin's own state, as it saves it: not just what the host shows.
    assert!(
        plugin.save_state().unwrap() == saved,
        "the plugin does not keep the state it was given"
    );
    assert_eq!(
        plugin.param_value(cutoff()),
        Some(settled(cutoff(), 0.8)),
        "the loaded cutoff was replaced by automation played before the load"
    );
}

/// A value queued for an audio half that was dropped before reading it is
/// still on its way when a state is loaded, and is forgotten: `deactivate`
/// neither hands it to the plugin after the state nor keeps it for the
/// next activation.
#[test]
fn a_value_still_waiting_when_a_state_is_loaded_is_forgotten() {
    let module = module();
    let saved = state_with_cutoff(&module, 0.8);

    let mut plugin = module.create(class()).unwrap();
    let processor = plugin.activate(RATE, 256).unwrap();
    plugin.set_param(cutoff(), 0.1).unwrap();
    // The audio half goes without having run a block.
    drop(processor);
    plugin.load_state(&saved).unwrap();
    plugin.deactivate().unwrap();
    assert!(
        plugin.save_state().unwrap() == saved,
        "a value set before the state was loaded replaced the loaded one"
    );

    // Nor does the next activation deliver it.
    let mut processor = plugin.activate(RATE, 256).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 256));
    plugin.pump();
    assert_eq!(plugin.param_value(cutoff()), Some(settled(cutoff(), 0.8)));
    drop(processor);
    plugin.deactivate().unwrap();
    assert!(plugin.save_state().unwrap() == saved);
}

/// A value in the queue to a running audio half when a state is loaded is
/// not delivered after the state, though the audio half reads it later.
#[test]
fn a_value_queued_for_running_audio_is_not_delivered_after_a_loaded_state() {
    let module = module();
    let saved = state_with_cutoff(&module, 0.8);

    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 256));
    plugin.set_param(cutoff(), 0.1).unwrap();
    plugin.load_state(&saved).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 256));
    plugin.pump();
    assert_eq!(
        plugin.param_value(cutoff()),
        Some(settled(cutoff(), 0.8)),
        "the queued value was delivered after the state"
    );
    drop(processor);
    plugin.deactivate().unwrap();
    assert!(
        plugin.save_state().unwrap() == saved,
        "the plugin heard a value set before the state was loaded"
    );

    // A value set after the load reaches the running plugin as usual.
    let mut processor = plugin.activate(RATE, 256).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 256));
    plugin.set_param(cutoff(), 0.3).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 256));
    assert!(
        plugin.save_state().unwrap() == state_with_cutoff(&module, 0.3),
        "a value set after the state was loaded did not reach the plugin"
    );
    drop(processor);
}

// --- Life cycle -----------------------------------------------------------

#[test]
fn a_plugin_can_be_activated_again_and_again() {
    let module = module();
    let mut plugin = module.create(class()).unwrap();
    let note = NoteRef::new(0, 48);
    let play = [
        (
            10,
            EventKind::NoteOn {
                note,
                velocity: 0.9,
            },
        ),
        (
            3_000,
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
        ),
    ];

    let mut previous: Option<AudioBuffer> = None;
    for round in 0..4 {
        let (rate, max_block) = [
            (48_000.0, 256),
            (44_100.0, 100),
            (96_000.0, 1_024),
            (96_000.0, 1_024),
        ][round];
        let mut processor = plugin.activate(rate, max_block).unwrap();
        assert!(plugin.is_active() && plugin.has_processor());
        assert_eq!(processor.sample_rate(), rate);
        assert_eq!(processor.status(), ProcessStatus::Idle);
        assert_eq!(
            plugin.activate(rate, max_block).unwrap_err(),
            Vst3Error::AlreadyActive
        );
        assert_eq!(plugin.deactivate(), Err(Vst3Error::ProcessorAlive));

        let mut audio = AudioBuffer::<f32>::new(2, 6_000);
        let rendered = render(
            &mut processor,
            rate,
            &play,
            &[max_block],
            max_block,
            &mut audio,
            None,
        );
        assert_eq!(rendered.heap_calls, 0);
        assert!(audio.peak() > 0.05, "round {round} is silent");

        let mut harness = Harness::<Pluck>::new(rate, max_block);
        for &(frame, kind) in &play {
            harness.schedule(frame, kind);
        }
        assert_same_bits(&audio, &harness.render(6_000), &format!("round {round}"));
        if round == 3 {
            assert_same_bits(
                &audio,
                previous.as_ref().unwrap(),
                "the same settings twice",
            );
        }
        previous = Some(audio);

        drop(processor);
        assert!(plugin.is_active() && !plugin.has_processor());
        plugin.deactivate().unwrap();
        assert!(!plugin.is_active());
        plugin.deactivate().unwrap();
    }
    assert_eq!(plugin.failed_blocks(), 0);

    for (rate, max_block) in [
        (0.0, 64),
        (-48_000.0, 64),
        (f64::NAN, 64),
        (f64::INFINITY, 64),
        (48_000.0, 0),
        (48_000.0, usize::MAX),
    ] {
        assert!(
            matches!(
                plugin.activate(rate, max_block),
                Err(Vst3Error::InvalidArgument(_))
            ),
            "activated at {rate} Hz with blocks of {max_block}"
        );
        assert!(!plugin.is_active());
    }
}

#[test]
fn the_audio_half_runs_on_another_thread() {
    fn can_be_sent<T: Send>() {}
    can_be_sent::<Vst3Processor>();
    can_be_sent::<Vst3Module>();

    let module = module();
    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let schedule = song();
    let expected = reference(256, &[256], &schedule);

    let audio_thread = std::thread::spawn(move || {
        let mut audio = AudioBuffer::<f32>::new(2, SONG_FRAMES);
        let rendered = render(
            &mut processor,
            RATE,
            &schedule,
            &[256],
            256,
            &mut audio,
            None,
        );
        // Dropped here, away from the main thread, on purpose.
        drop(processor);
        (audio, rendered.heap_calls)
    });
    let mut pumped = Vec::new();
    while !audio_thread.is_finished() {
        pumped.extend(plugin.pump());
        let _ = plugin.param_value(cutoff());
        let _ = plugin.param_to_text(level(), 0.5);
        std::thread::yield_now();
    }
    let (audio, heap_calls) = audio_thread.join().unwrap();
    pumped.extend(plugin.pump());

    assert_same_bits(&audio, &expected, "played on another thread");
    assert_eq!(heap_calls, 0);
    assert_eq!(pumped, [], "the plugin had nothing to report");
    // What the audio half sent, the controller now shows.
    assert_eq!(plugin.param_value(level()), Some(0.9));
    assert!(!plugin.has_processor());
    plugin.deactivate().unwrap();

    let mut again = plugin.activate(RATE, 64).unwrap();
    again.process(&mut AudioBuffer::<f32>::new(2, 64));
    assert_eq!(again.status(), ProcessStatus::Processed);
    drop(again);
    drop(plugin);
}

#[test]
fn a_processor_asked_for_another_sample_rate_is_silent_and_says_so() {
    let mut plugin = module().create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut events = EventBuffer::with_capacity(4);
    events.push(note_on(0, NoteRef::new(0, 60))).unwrap();
    let transport = Transport::STOPPED;
    let context = ProcessContext::new(&events, &transport);
    let spec = |sample_rate| ProcessSpec {
        sample_rate,
        max_block_size: 256,
        num_channels: 2,
    };

    processor.prepare(&spec(44_100.0));
    let mut block = AudioBuffer::<f32>::new(2, 256);
    block.channel_mut(0).fill(0.5);
    processor.process_with(&mut block, &context);
    assert_eq!(block.peak(), 0.0);
    assert_eq!(
        processor.status(),
        ProcessStatus::Failed(ProcessFailure::SampleRateMismatch)
    );
    assert_eq!(plugin.failed_blocks(), 1);
    assert_eq!(
        plugin.pump(),
        [HostEvent::ProcessFailed(ProcessFailure::SampleRateMismatch)]
    );
    assert!(plugin.pump().is_empty(), "the failure was reported twice");

    // Told the right rate, it plays, the note of the silent block too.
    processor.prepare(&spec(RATE));
    let mut block = AudioBuffer::<f32>::new(2, 1_000);
    processor.process(&mut block);
    assert!(block.peak() > 0.01, "the kept note was not played");
    assert_eq!(processor.status(), ProcessStatus::Processed);
    assert_eq!(plugin.failed_blocks(), 1);
}

// --- In a graph -----------------------------------------------------------

#[test]
fn a_hosted_plugin_is_a_node_in_a_graph() {
    let module = module();
    let mut plugin = module.create(class()).unwrap();
    let spec = ProcessSpec {
        sample_rate: RATE,
        max_block_size: 256,
        num_channels: 2,
    };
    let processor = plugin
        .activate(spec.sample_rate, spec.max_block_size)
        .unwrap();

    let (mut graph, mut engine) = Graph::<f32>::new(spec);
    let node = graph.add(processor);
    graph.connect(node, Graph::<f32>::OUTPUT).unwrap();
    graph.commit().unwrap();

    let schedule = song();
    let mut audio = AudioBuffer::<f32>::new(2, SONG_FRAMES);
    let rendered = render(
        &mut engine,
        RATE,
        &schedule,
        &[256, 100, 3],
        256,
        &mut audio,
        None,
    );
    assert_eq!(
        rendered.heap_calls, 0,
        "the graph or the host used the heap"
    );
    let expected = reference(256, &[256, 100, 3], &schedule);
    // To the bit: a single connection into the output is copied, not
    // summed into silence, so even the sign of a zero survives.
    assert_same_bits(&audio, &expected, "the plugin inside a graph");

    plugin.set_param(level(), 0.0).unwrap();
    let mut quiet = AudioBuffer::<f32>::new(2, 256);
    engine.process(&mut quiet);
    assert!(graph.update(node, |hosted: &mut Vst3Processor| hosted.reset()));
    engine.process(&mut quiet);

    assert_eq!(plugin.deactivate(), Err(Vst3Error::ProcessorAlive));
    graph.remove(node).unwrap();
    graph.commit().unwrap();
    engine.process(&mut quiet);
    assert_eq!(quiet.peak(), 0.0, "a removed node is still heard");
    graph.collect_garbage();
    assert!(!plugin.has_processor());
    plugin.deactivate().unwrap();
    assert_eq!(plugin.failed_blocks(), 0);
}

// --- Things that are not plugins -------------------------------------------

#[test]
fn what_is_not_a_plugin_is_an_error_and_nothing_more() {
    let folder = common::scratch_folder("vst3-not-plugins");

    let missing = folder.join("missing.vst3");
    assert_eq!(
        common::load_vst3(&missing).unwrap_err(),
        Vst3Error::NotFound(missing.clone())
    );
    let text = folder.join("notes.vst3");
    std::fs::write(&text, "this is not a shared library\n").unwrap();
    let error = common::load_vst3(&text).unwrap_err();
    assert!(
        matches!(&error, Vst3Error::Load { path, reason } if path == &text && !reason.is_empty()),
        "{error:?}"
    );
    let empty = folder.join("empty.vst3");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(
        common::load_vst3(&empty),
        Err(Vst3Error::Load { .. })
    ));
    // The first 16 bytes of a library, fewer than the header of any
    // executable format: every system refuses it before mapping anything.
    // (A library cut after its headers would crash Linux's loader, which
    // is why `load` does not promise to survive damaged libraries.)
    let real = std::fs::read(common::built_library("mangue_pluck")).unwrap();
    let cut = folder.join("cut.vst3");
    std::fs::write(&cut, &real[..real.len().min(16)]).unwrap();
    assert!(matches!(
        common::load_vst3(&cut),
        Err(Vst3Error::Load { .. })
    ));

    // A real shared library that is not a VST 3 module.
    let system = if cfg!(windows) {
        Some(
            std::path::PathBuf::from(std::env::var("SystemRoot").unwrap())
                .join("System32/kernel32.dll"),
        )
    } else {
        [
            "/usr/lib/libSystem.B.dylib",
            "/lib/x86_64-linux-gnu/libc.so.6",
            "/lib/aarch64-linux-gnu/libc.so.6",
            "/usr/lib/libc.so.6",
            "/lib64/libc.so.6",
        ]
        .into_iter()
        .map(std::path::PathBuf::from)
        .find(|path| path.is_file())
    };
    if let Some(system) = system {
        assert_eq!(
            common::load_vst3(&system).unwrap_err(),
            Vst3Error::NoEntryPoint(system.clone()),
            "{}",
            system.display()
        );
    }

    // A folder that is not a bundle.
    let bare = folder.join("Bare.vst3");
    std::fs::create_dir_all(bare.join("Contents")).unwrap();
    assert_eq!(
        common::load_vst3(&bare).unwrap_err(),
        Vst3Error::BundleWithoutBinary(bare.clone())
    );

    // A class the module does not have.
    let module = module();
    let other = ClassId::from_words([1, 2, 3, 4]);
    assert_eq!(
        module.create(other).unwrap_err(),
        Vst3Error::UnknownClass(other)
    );

    let mut plugin = module.create(class()).unwrap();
    let mut processor = plugin.activate(RATE, 64).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 64));
    assert_eq!(processor.status(), ProcessStatus::Processed);
    let _ = std::fs::remove_dir_all(&folder);
}

/// The folder a VST 3 bundle keeps this system's library in, and the
/// library's name in it, as VST 3 lays bundles out.
fn bundle_place(bundle: &std::path::Path, name: &str) -> std::path::PathBuf {
    let contents = bundle.join("Contents");
    let arch = std::env::consts::ARCH;
    match std::env::consts::OS {
        "windows" => {
            let machine = if arch == "aarch64" { "arm64" } else { arch };
            contents
                .join(format!("{machine}-win"))
                .join(format!("{name}.vst3"))
        }
        "macos" => contents.join("MacOS").join(name),
        os => contents
            .join(format!("{arch}-{os}"))
            .join(format!("{name}.so")),
    }
}

#[test]
fn a_bundle_folder_is_loaded_through_the_library_for_this_system() {
    let folder = common::scratch_folder("vst3-bundles");
    let built = common::built_library("mangue_pluck");
    let place = |target: &std::path::Path| {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if std::fs::hard_link(&built, target).is_err() {
            std::fs::copy(&built, target).unwrap();
        }
    };

    // Named after the bundle, as VST 3 has it.
    let bundle = folder.join("Mangue Pluck.vst3");
    place(&bundle_place(&bundle, "Mangue Pluck"));
    std::fs::create_dir_all(bundle.join("Contents/Resources")).unwrap();
    // Another name, alone in its folder.
    let renamed = folder.join("Renamed.vst3");
    place(&bundle_place(&renamed, "Original"));

    for bundle in [&bundle, &renamed] {
        let module = common::load_vst3(bundle)
            .unwrap_or_else(|error| panic!("{}: {error}", bundle.display()));
        assert_eq!(module.path(), bundle.as_path());
        let mut plugin = module.create(class()).unwrap();
        let mut processor = plugin.activate(RATE, 64).unwrap();
        let mut events = EventBuffer::with_capacity(1);
        events.push(note_on(0, NoteRef::new(0, 69))).unwrap();
        let mut block = AudioBuffer::<f32>::new(2, 64);
        processor.process_with(
            &mut block,
            &ProcessContext::new(&events, &Transport::STOPPED),
        );
        assert!(block.peak() > 0.0, "{} is silent", bundle.display());
    }

    // Two candidates and nothing to choose by.
    let unclear = folder.join("Unclear.vst3");
    place(&bundle_place(&unclear, "one"));
    place(&bundle_place(&unclear, "two"));
    assert_eq!(
        common::load_vst3(&unclear).unwrap_err(),
        Vst3Error::BundleWithoutBinary(unclear.clone())
    );
    let _ = std::fs::remove_dir_all(&folder);
}
