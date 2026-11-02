//! Hosts Mangue Pluck, the framework's example instrument, through the
//! shared library cargo builds for it, and holds the result against the
//! same plugin driven directly.
//!
//! The plugin is in this executable twice. Once as an ordinary Rust
//! library, which the test drives with `manguebyte_plugin::Harness`: no
//! plugin format, no host. And once as the `cdylib` a real host loads,
//! reached only through its exported `clap_entry` and the C interface
//! behind it. If the host does its job, the two cannot be told apart: the
//! tests ask for the same bits.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "clap", not(miri)))]

mod common;

use std::sync::Arc;

use mangue_pluck::Pluck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_graph::Graph;
use manguebyte_host::clap::clap_sys::version::CLAP_VERSION;
use manguebyte_host::clap::{
    ClapError, ClapLibrary, ClapProcessor, HostEvent, ParamsRescan, ProcessFailure, ProcessStatus,
    Tail,
};
use manguebyte_midi::MidiMessage;
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{Harness, Plugin};

use common::{assert_same_bits, clap_from_plain, for_harness, plain_from_clap, render};

const RATE: f64 = 48_000.0;
const ID: &str = "org.manguebyte.pluck";

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const RESONANCE: ParamId = ParamId::from_name("resonance");
const DECAY: ParamId = ParamId::from_name("decay");
const RELEASE: ParamId = ParamId::from_name("release");
const REVERB: ParamId = ParamId::from_name("reverb");
const LEVEL: ParamId = ParamId::from_name("level");

fn library() -> ClapLibrary {
    common::load(common::built_library("mangue_pluck")).expect("Mangue Pluck loads")
}

/// A few seconds of everything the event model has: notes with and without
/// ids, two notes on one key, expression, modulation for one note and for
/// all, automation and MIDI. Parameter values and modulation amounts are in
/// the plugin's CLAP scale, as a host has them.
fn song() -> Vec<(u64, EventKind)> {
    let low = NoteRef::with_id(0, 45, 1);
    let fifth = NoteRef::with_id(0, 52, 2);
    let plain = NoteRef::new(1, 57);
    // The same key as `low` while `low` still sounds: only the id tells
    // them apart.
    let again = NoteRef::with_id(0, 45, 3);
    let on = |note, velocity| EventKind::NoteOn { note, velocity };
    let off = |note, velocity| EventKind::NoteOff { note, velocity };
    let shape = |note, expression, value| EventKind::NoteExpression {
        note,
        expression,
        value,
    };
    let value = |param, value| EventKind::ParamValue { param, value };
    let modulate = |amount, note| EventKind::ParamModulation {
        param: CUTOFF,
        amount,
        note,
    };
    vec![
        (100, on(low, 0.8)),
        (2_500, value(RESONANCE, 0.6)),
        (3_000, shape(low, Expression::Tuning, 0.5)),
        (3_500, modulate(0.2, Some(low))),
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
        (6_000, shape(plain, Expression::Volume, 0.5)),
        (6_001, off(plain, 0.0)),
        (7_000, modulate(-0.1, None)),
        (7_777, value(DECAY, 0.8)),
        (9_000, value(RELEASE, 0.1)),
        (9_100, off(low, 0.3)),
        (10_000, value(REVERB, 0.6)),
        (11_000, EventKind::NoteChoke { note: again }),
        (12_000, value(LEVEL, 0.9)),
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

/// What the plugin reports a parameter to be once it has been set to
/// `value`. The plugin keeps each value as a 32-bit float in the
/// parameter's own unit, so what comes back is the nearest value it can
/// hold: close to what was set, and not always equal to it.
fn settled(param: ParamId, value: f64) -> f64 {
    let declared = Pluck::new().params();
    let target = declared.find_param(param).unwrap();
    target.set_plain(plain_from_clap(target, value));
    clap_from_plain(target, target.plain())
}

/// The song as the plugin plays it with nothing in between.
fn reference(max_block: usize, pattern: &[usize], schedule: &[(u64, EventKind)]) -> AudioBuffer {
    let mut harness = Harness::<Pluck>::new(RATE, max_block).with_block_pattern(pattern);
    // Before anything is played, the parameters hold their defaults.
    let fresh = Pluck::new().params();
    for &(frame, kind) in schedule {
        harness.schedule(frame, for_harness(&*fresh, kind));
    }
    harness.render(SONG_FRAMES)
}

/// The song as the plugin plays it when hosted, and what hosting cost.
fn hosted(
    library: &ClapLibrary,
    max_block: usize,
    pattern: &[usize],
    schedule: &[(u64, EventKind)],
) -> (AudioBuffer, common::Rendered) {
    let mut plugin = library.create(ID).expect("the plugin is created");
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
    assert_eq!(processor.status(), ProcessStatus::Continue);
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

// --- What the plugin is ---------------------------------------------------

#[test]
fn the_library_lists_what_the_plugin_declares() {
    let path = common::built_library("mangue_pluck");
    let library = common::load(&path).unwrap();
    assert_eq!(library.path(), path);
    assert_eq!(
        library.clap_version(),
        (
            CLAP_VERSION.major,
            CLAP_VERSION.minor,
            CLAP_VERSION.revision
        )
    );

    let [descriptor] = library.descriptors() else {
        panic!("expected one plugin, found {:?}", library.descriptors());
    };
    let info = Pluck::INFO;
    assert_eq!(descriptor.id, info.id);
    assert_eq!(descriptor.name, info.name);
    assert_eq!(descriptor.vendor, info.vendor);
    assert_eq!(descriptor.version, info.version);
    assert_eq!(descriptor.description, info.description);
    assert_eq!(descriptor.features, ["instrument", "synthesizer"]);
    assert!(descriptor.has_feature("instrument") && !descriptor.has_feature("audio-effect"));
    assert_eq!(library.descriptor(ID), Some(descriptor));
    assert_eq!(library.descriptor("org.manguebyte.other"), None);

    // The same file again is the same library, not a second copy of it.
    let again = common::load(&path).unwrap();
    assert_eq!(again, library);
    // And a handle can be used from another thread.
    let listed = std::thread::spawn(move || again.descriptors().len())
        .join()
        .unwrap();
    assert_eq!(listed, 1);

    let plugin = library.create(ID).unwrap();
    assert_eq!(plugin.descriptor(), descriptor);
    assert!(!plugin.is_active() && !plugin.has_processor());
}

#[test]
fn ports_follow_the_layout_the_plugin_declares() {
    let plugin = library().create(ID).unwrap();

    // An instrument: no audio in, one stereo port out.
    assert!(plugin.audio_inputs().is_empty());
    let [output] = plugin.audio_outputs() else {
        panic!("expected one output, found {:?}", plugin.audio_outputs());
    };
    assert_eq!(output.channels as usize, Pluck::LAYOUT.outputs);
    assert!(output.is_main);
    assert_eq!(output.name, "Main");
    assert_eq!(output.port_type.as_deref(), Some("stereo"));
    assert_eq!(output.in_place_pair, None);

    let [notes] = plugin.note_inputs() else {
        panic!("expected one note input, found {:?}", plugin.note_inputs());
    };
    assert!(notes.clap && notes.midi);
    assert_eq!(notes.name, "Notes");
    assert!(plugin.note_outputs().is_empty());

    assert_eq!(plugin.latency(), 0);
    assert_eq!(
        plugin.tail(),
        Some(Tail::Infinite),
        "before activation the plugin reports an infinite tail"
    );
}

#[test]
fn parameters_are_the_ones_the_plugin_declares() {
    let plugin = library().create(ID).unwrap();
    let declared = Pluck::new().params();

    let mut expected: Vec<&dyn Param> = Vec::new();
    declared.visit(&mut |param| expected.push(param));
    assert_eq!(plugin.params().len(), expected.len());
    assert_eq!(expected.len(), 6);

    for (listed, declared) in plugin.params().iter().zip(expected) {
        assert_eq!(listed.id, declared.param_id());
        assert_eq!(listed.name, declared.name());
        assert_eq!(listed.module, "");
        // Every parameter of this plugin is continuous, which the wrapper
        // presents as a position between 0 and 1.
        assert_eq!((listed.min, listed.max), (0.0, 1.0));
        assert_eq!(listed.default, declared.default_normalized());
        let flags = declared.info().flags();
        assert_eq!(listed.flags.automatable, flags.automatable);
        assert_eq!(listed.flags.hidden, flags.hidden);
        assert_eq!(listed.flags.bypass, flags.bypass);
        assert!(listed.flags.modulatable && !listed.flags.stepped);

        assert_eq!(plugin.param(listed.id), Some(listed));
        assert_eq!(
            plugin.param_value(listed.id),
            Some(declared.default_normalized())
        );
    }

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(plugin.param(nothing), None);
    assert_eq!(plugin.param_value(nothing), None);
}

#[test]
fn text_goes_both_ways_in_the_plugins_own_words() {
    let plugin = library().create(ID).unwrap();
    let declared = Pluck::new().params();

    let mut checked = 0;
    declared.visit(&mut |param| {
        let id = param.param_id();
        for position in [0.0, 0.25, 0.5, 1.0] {
            // What the plugin's own parameter says, unit included.
            let plain = param.normalized_to_plain(position);
            let mut expected = param.plain_to_text(plain);
            if !param.info().unit().is_empty() {
                expected.push(' ');
                expected.push_str(param.info().unit());
            }
            let text = plugin.param_to_text(id, position).unwrap();
            assert_eq!(text, expected);

            // And back: the text names a value, as near as its digits go.
            let back = plugin.text_to_param(id, &text).unwrap();
            let direct = param.plain_to_normalized(param.text_to_plain(&text).unwrap());
            assert_eq!(back, direct);
            checked += 1;
        }
        assert_eq!(plugin.text_to_param(id, "not a number"), None);
        assert_eq!(plugin.text_to_param(id, "1\0 2"), None);
    });
    assert_eq!(checked, 24);

    // The spot checks a person would make.
    assert_eq!(plugin.param_to_text(CUTOFF, 0.0).unwrap(), "100 Hz");
    assert_eq!(plugin.param_to_text(CUTOFF, 1.0).unwrap(), "12000 Hz");
    assert_eq!(plugin.param_to_text(LEVEL, 1.0).unwrap(), "6.0 dB");
    assert_eq!(plugin.text_to_param(LEVEL, "6 dB"), Some(1.0));
    assert_eq!(plugin.text_to_param(LEVEL, "-40"), Some(0.0));

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(plugin.param_to_text(nothing, 0.5), None);
    assert_eq!(plugin.text_to_param(nothing, "1"), None);
}

// --- Sound ----------------------------------------------------------------

#[test]
fn the_hosted_plugin_sounds_exactly_as_the_plugin_does_alone() {
    let library = library();
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

        let (actual, rendered) = hosted(&library, max_block, pattern, &schedule);
        assert_same_bits(&actual, &expected, &what);
        assert_eq!(
            rendered.heap_calls, 0,
            "{what}: the host used the heap while processing"
        );
        assert!(rendered.blocks >= SONG_FRAMES / max_block);
    }
}

#[test]
fn every_kind_of_event_is_heard() {
    // Bit-identical output would also follow from the host delivering
    // nothing, if the reference were deaf too. So: each kind of event,
    // taken out of the song, changes what the hosted plugin plays.
    let library = library();
    let whole = song();
    let (complete, _) = hosted(&library, 256, &[256], &whole);

    type IsKind = fn(&EventKind) -> bool;
    let kinds: [(&str, IsKind); 6] = [
        ("note on", |kind| matches!(kind, EventKind::NoteOn { .. })),
        ("note off", |kind| matches!(kind, EventKind::NoteOff { .. })),
        ("note choke", |kind| {
            matches!(kind, EventKind::NoteChoke { .. })
        }),
        ("note expression", |kind| {
            matches!(kind, EventKind::NoteExpression { .. })
        }),
        ("parameter modulation", |kind| {
            matches!(kind, EventKind::ParamModulation { .. })
        }),
        ("MIDI", |kind| matches!(kind, EventKind::Midi(_))),
    ];
    for (name, is_kind) in kinds {
        let without: Vec<_> = whole
            .iter()
            .copied()
            .filter(|(_, kind)| !is_kind(kind))
            .collect();
        assert!(without.len() < whole.len(), "the song has no {name}");
        let (partial, _) = hosted(&library, 256, &[256], &without);
        assert!(partial != complete, "{name} events made no difference");
    }
    for param in [RESONANCE, DECAY, RELEASE, REVERB, LEVEL] {
        let without: Vec<_> = whole
            .iter()
            .copied()
            .filter(
                |(_, kind)| !matches!(kind, EventKind::ParamValue { param: p, .. } if *p == param),
            )
            .collect();
        assert_eq!(without.len(), whole.len() - 1);
        let (partial, _) = hosted(&library, 256, &[256], &without);
        assert!(
            partial != complete,
            "automation of {param:?} made no difference"
        );
    }
}

#[test]
fn the_sound_does_not_depend_on_how_the_host_cuts_time() {
    let library = library();
    // This plugin reads its reverb mix and its output level once per
    // block, as its documentation says, so automating those two is heard
    // at block boundaries by design. Everything else in the song follows
    // events and belongs to its frame, wherever the blocks fall.
    let schedule: Vec<_> = song()
        .into_iter()
        .filter(|(_, kind)| {
            !matches!(kind, EventKind::ParamValue { param, .. } if [REVERB, LEVEL].contains(param))
        })
        .collect();
    assert_eq!(schedule.len(), song().len() - 2);
    let (reference, _) = hosted(&library, 256, &[256], &schedule);
    assert!(reference.peak() > 0.05);

    for (max_block, pattern) in [
        (64usize, &[64usize][..]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
        (997, &[997]),
        (4_096, &[4_096]),
    ] {
        let (actual, rendered) = hosted(&library, max_block, pattern, &schedule);
        assert_same_bits(
            &actual,
            &reference,
            &format!("blocks of {pattern:?}, at most {max_block}"),
        );
        assert_eq!(rendered.heap_calls, 0);
    }

    // A caller may also hand over more than the plugin was activated for.
    // The host then cuts the block up itself, and every event still lands
    // on its frame.
    let mut plugin = library.create(ID).unwrap();
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

    let mut plugin = library().create(ID).unwrap();
    let mut processor = plugin.activate(RATE, 128).unwrap();
    let transport = Transport::playing_at(120.0);

    // More events in one block than the host has room for: the rest are
    // dropped and counted, not allocated for.
    let mut flood = EventBuffer::with_capacity(6_000);
    for index in 0..6_000u32 {
        let kind = EventKind::ParamValue {
            param: RESONANCE,
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
    for (offset, kind) in [
        (
            3,
            EventKind::NoteOn {
                note,
                velocity: 0.9,
            },
        ),
        // Beyond the end of a 128-frame block: kept for the next one.
        (
            500,
            EventKind::NoteOff {
                note,
                velocity: 0.0,
            },
        ),
    ] {
        notes.push(Event { offset, kind }).unwrap();
    }

    let mut block = AudioBuffer::<f32>::new(2, 128);
    let mut long = AudioBuffer::<f32>::new(2, 1_000);
    let mut empty = AudioBuffer::<f32>::new(2, 0);
    // Changes queued on the main thread are picked up inside the block.
    for index in 0..50 {
        plugin.set_param(CUTOFF, f64::from(index) / 50.0).unwrap();
    }
    let heap_calls = common::heap_calls_during(|| {
        processor.process(&mut block);
        processor.process_with(&mut block, &ProcessContext::new(&notes, &transport));
        processor.process_with(&mut block, &ProcessContext::new(&flood, &transport));
        processor.process_with(&mut long, &ProcessContext::new(&notes, &transport));
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
    assert_eq!(processor.status(), ProcessStatus::Continue);
    assert_eq!(
        plugin.param_value(CUTOFF),
        Some(settled(CUTOFF, 49.0 / 50.0))
    );
    // 6,000 were sent in one block that has room for 4,096, and the
    // note-off kept from the block before had taken the first place.
    assert_eq!(plugin.lost_events(), 6_000 - 4_095);
}

// --- Parameters and state -------------------------------------------------

#[test]
fn a_parameter_set_on_the_main_thread_reaches_the_plugin() {
    let library = library();
    let mut plugin = library.create(ID).unwrap();

    // Not active: the plugin has the value before the call returns.
    plugin.set_param(RESONANCE, 0.75).unwrap();
    assert_eq!(plugin.param_value(RESONANCE), Some(0.75));
    // Held to the range the plugin declared.
    plugin.set_param(REVERB, 7.0).unwrap();
    assert_eq!(plugin.param_value(REVERB), Some(1.0));
    plugin.set_param(REVERB, 0.0).unwrap();

    let nothing = ParamId::from_name("no such parameter");
    assert_eq!(
        plugin.set_param(nothing, 0.5),
        Err(ClapError::UnknownParam(nothing))
    );
    assert!(matches!(
        plugin.set_param(CUTOFF, f64::NAN),
        Err(ClapError::InvalidArgument(_))
    ));

    // Active: the value crosses to the audio half and arrives with the
    // next block, on its first frame.
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

    let before = plugin.param_value(CUTOFF);
    plugin.set_param(CUTOFF, 0.95).unwrap();
    assert_eq!(
        plugin.param_value(CUTOFF),
        before,
        "the plugin had the value before a block was processed"
    );
    let mut second = AudioBuffer::<f32>::new(2, 4_096);
    render(&mut processor, RATE, &[], &[256], 256, &mut second, None);
    assert_eq!(plugin.param_value(CUTOFF), Some(settled(CUTOFF, 0.95)));
    assert!((settled(CUTOFF, 0.95) - 0.95).abs() < 1e-6);

    // Exactly as if automation had put it there.
    let declared = Pluck::new().params();
    let plain = |id, value| plain_from_clap(declared.find_param(id).unwrap(), value);
    let mut harness = Harness::<Pluck>::new(RATE, 256);
    harness
        .params()
        .resonance
        .set(plain(RESONANCE, 0.75) as f32);
    harness.params().reverb.set(plain(REVERB, 0.0) as f32);
    harness.schedule(0, play[0].1);
    harness.schedule(
        4_096,
        EventKind::ParamValue {
            param: CUTOFF,
            value: plain(CUTOFF, 0.95),
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
        match plugin.set_param(DECAY, 0.5) {
            Ok(()) => accepted += 1,
            Err(error) => break error,
        }
        assert!(accepted < 100_000, "the queue never filled");
    };
    assert_eq!(full, ClapError::QueueFull);
    assert_eq!(accepted, 1_024);
    processor.process(&mut AudioBuffer::<f32>::new(2, 16));
    plugin.set_param(DECAY, 0.25).unwrap();

    // A value still in the queue when the audio half goes is not lost: the
    // plugin is given it on the way to being deactivated.
    drop(processor);
    assert_eq!(plugin.param_value(DECAY), Some(settled(DECAY, 0.5)));
    plugin.deactivate().unwrap();
    assert_eq!(plugin.param_value(DECAY), Some(settled(DECAY, 0.25)));
    assert_ne!(settled(DECAY, 0.25), settled(DECAY, 0.5));
}

#[test]
fn state_survives_a_round_trip_through_the_hosts_streams() {
    let library = library();
    let mut plugin = library.create(ID).unwrap();
    plugin.set_param(CUTOFF, 0.8).unwrap();
    plugin.set_param(LEVEL, 0.3).unwrap();
    let saved = plugin.save_state().unwrap();

    // It is the plugin's own state: the plugin, with no host, reads it.
    let text = std::str::from_utf8(&saved).unwrap();
    assert!(text.starts_with("manguebyte-plugin 1 org.manguebyte.pluck\n"));
    let alone = Harness::<Pluck>::new(RATE, 64);
    assert!(alone.load_state(text).unwrap().is_exact());
    let declared = Pluck::new().params();
    let cutoff = declared.find_param(CUTOFF).unwrap();
    assert_eq!(
        f64::from(alone.params().cutoff.value()),
        f64::from(plain_from_clap(cutoff, 0.8) as f32)
    );
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
        let mut other = library.create(ID).unwrap();
        assert_ne!(other.param_value(CUTOFF), plugin.param_value(CUTOFF));
        other.set_stream_chunk(chunk);
        other.load_state(&saved).unwrap();
        for param in plugin.params() {
            assert_eq!(
                other.param_value(param.id),
                plugin.param_value(param.id),
                "{} after loading {chunk} bytes per call",
                param.name
            );
        }
        // The plugin asked for its values to be read again.
        let rescan = ParamsRescan {
            values: true,
            ..ParamsRescan::default()
        };
        assert_eq!(other.pump(), [HostEvent::ParamsRescanned(rescan)]);
        assert!(other.pump().is_empty(), "the request was answered twice");
    }

    // Damaged, cut short or meant for someone else: refused, and nothing
    // is changed.
    let mut other = library.create(ID).unwrap();
    let before = other.param_value(CUTOFF);
    let foreign = text.replace("manguebyte.pluck", "manguebyte.other");
    for bad in [
        &b""[..],
        b"not a state",
        &[0xFF, 0xFE, 0x00],
        foreign.as_bytes(),
    ] {
        assert_eq!(
            other.load_state(bad),
            Err(ClapError::StateLoadFailed),
            "accepted {bad:?}"
        );
    }
    assert_eq!(other.param_value(CUTOFF), before);
    assert!(other.pump().is_empty(), "a refused state caused a rescan");
    // Cut anywhere, the state is either refused or read as far as it goes.
    // Which of the two is the plugin's business; the host's is to get the
    // answer across.
    for length in 0..saved.len() {
        let _ = other.load_state(&saved[..length]);
    }
    other.load_state(&saved).unwrap();
    assert_eq!(other.param_value(CUTOFF), plugin.param_value(CUTOFF));

    // State restored while the plugin runs is heard from the next block,
    // exactly as when the plugin is restored with no host in between.
    let mut running = library.create(ID).unwrap();
    let mut processor = running.activate(RATE, 256).unwrap();
    let note = NoteRef::new(0, 50);
    let play = [(
        0,
        EventKind::NoteOn {
            note,
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
    harness.load_state(text).unwrap();
    let expected_second = harness.render(2_048);
    assert_same_bits(&first, &expected_first, "before the state was loaded");
    assert_same_bits(&second, &expected_second, "after the state was loaded");
    assert!(processor.output_events().is_empty());
}

// --- Life cycle -----------------------------------------------------------

#[test]
fn a_plugin_can_be_activated_again_and_again() {
    let library = library();
    let mut plugin = library.create(ID).unwrap();
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
        // A different rate and block size each time round, then the same
        // ones again.
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

        // While it is active it cannot be activated or deactivated.
        assert_eq!(
            plugin.activate(rate, max_block).unwrap_err(),
            ClapError::AlreadyActive
        );
        assert_eq!(plugin.deactivate(), Err(ClapError::ProcessorAlive));
        assert_eq!(plugin.flush(), Err(ClapError::ProcessorAlive));

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

        // A plugin that has just been activated is a plugin that has just
        // been made.
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
                Err(ClapError::InvalidArgument(_))
            ),
            "activated at {rate} Hz with blocks of {max_block}"
        );
        assert!(!plugin.is_active());
    }
}

#[test]
fn the_audio_half_runs_on_another_thread() {
    fn can_be_sent<T: Send>() {}
    can_be_sent::<ClapProcessor>();
    can_be_sent::<ClapLibrary>();

    let library = library();
    let mut plugin = library.create(ID).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let schedule = song();
    let expected = reference(256, &[256], &schedule);

    // The audio half plays the song on a thread of its own, while this
    // thread, the plugin's main thread, goes on talking to the plugin.
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
        let _ = plugin.param_value(CUTOFF);
        let _ = plugin.param_to_text(LEVEL, 0.5);
        std::thread::yield_now();
    }
    let (audio, heap_calls) = audio_thread.join().unwrap();
    pumped.extend(plugin.pump());

    assert_same_bits(&audio, &expected, "played on another thread");
    assert_eq!(heap_calls, 0);
    assert_eq!(pumped, [], "the plugin had nothing to report");
    assert!(!plugin.has_processor());
    plugin.deactivate().unwrap();

    // The plugin is whole afterwards: it can be used again, and when it
    // goes, it goes on this thread.
    let mut again = plugin.activate(RATE, 64).unwrap();
    again.process(&mut AudioBuffer::<f32>::new(2, 64));
    assert_eq!(again.status(), ProcessStatus::Continue);
    drop(again);
    drop(plugin);
}

#[test]
fn a_processor_asked_for_another_sample_rate_is_silent_and_says_so() {
    let mut plugin = library().create(ID).unwrap();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut events = EventBuffer::with_capacity(4);
    let note = NoteRef::new(0, 60);
    events
        .push(Event {
            offset: 0,
            kind: EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
        })
        .unwrap();
    let transport = Transport::STOPPED;
    let context = ProcessContext::new(&events, &transport);
    let spec = |sample_rate| ProcessSpec {
        sample_rate,
        max_block_size: 256,
        num_channels: 2,
    };

    // The plugin was activated at 48 kHz, and only its main thread could
    // change that. Asked to run at 44.1 kHz, the audio half plays nothing
    // sooner than play at the wrong pitch.
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

    // Told the right rate, it plays. A larger block size than it was
    // activated with is no obstacle.
    processor.prepare(&ProcessSpec {
        max_block_size: 4_096,
        ..spec(RATE)
    });
    let mut block = AudioBuffer::<f32>::new(2, 1_000);
    processor.process_with(&mut block, &context);
    assert!(block.peak() > 0.01);
    assert_eq!(processor.status(), ProcessStatus::Continue);
    assert_eq!(plugin.failed_blocks(), 1);
}

// --- In a graph -----------------------------------------------------------

#[test]
fn a_hosted_plugin_is_a_node_in_a_graph() {
    let library = library();
    let mut plugin = library.create(ID).unwrap();
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
    assert_eq!(graph.latency_samples(), 0);

    // The graph hands every node the events of the block, so the song
    // plays through it as it does through the plugin alone.
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
    assert_eq!(engine.num_nodes(), 1);
    assert_eq!(
        rendered.heap_calls, 0,
        "the graph or the host used the heap"
    );
    let expected = reference(256, &[256, 100, 3], &schedule);
    // To the bit: a single connection into the output is copied, not
    // summed into silence, so even the sign of a zero survives.
    assert_same_bits(&audio, &expected, "the plugin inside a graph");
    assert!(audio.peak() > 0.05);

    // The main half still reaches the plugin while the graph runs it.
    plugin.set_param(LEVEL, 0.0).unwrap();
    let mut quiet = AudioBuffer::<f32>::new(2, 256);
    engine.process(&mut quiet);
    assert_eq!(plugin.param_value(LEVEL), Some(0.0));
    // The graph reaches into the node on the audio thread as well.
    assert!(graph.update(node, |hosted: &mut ClapProcessor| hosted.reset()));
    engine.process(&mut quiet);

    // Removing the node hands it back to this thread to be dropped. Only
    // then is the audio half gone, and only then can the plugin be
    // deactivated.
    assert_eq!(plugin.deactivate(), Err(ClapError::ProcessorAlive));
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
    let folder = common::scratch_folder("clap-not-plugins");

    // Nothing there.
    let missing = folder.join("missing.clap");
    assert_eq!(
        common::load(&missing).unwrap_err(),
        ClapError::NotFound(missing.clone())
    );

    // A file that is no library at all.
    let text = folder.join("notes.clap");
    std::fs::write(&text, "this is not a shared library\n").unwrap();
    let error = common::load(&text).unwrap_err();
    assert!(
        matches!(&error, ClapError::Load { path, reason } if path == &text && !reason.is_empty()),
        "{error:?}"
    );
    // An empty file, and a file that only starts like a library: its first
    // 16 bytes, fewer than the header of any executable format, so every
    // system refuses it before mapping anything. (A library cut after its
    // headers is a different matter: Linux's loader maps it without
    // checking its length and the process dies inside the loader, which
    // is why `load` does not promise to survive damaged libraries.)
    let empty = folder.join("empty.clap");
    std::fs::write(&empty, b"").unwrap();
    assert!(matches!(common::load(&empty), Err(ClapError::Load { .. })));
    let real = std::fs::read(common::built_library("mangue_pluck")).unwrap();
    let cut = folder.join("cut.clap");
    std::fs::write(&cut, &real[..real.len().min(16)]).unwrap();
    assert!(matches!(common::load(&cut), Err(ClapError::Load { .. })));

    // A real shared library that is not a CLAP plugin: one that every
    // system has.
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
            common::load(&system).unwrap_err(),
            ClapError::NoEntryPoint(system.clone()),
            "{}",
            system.display()
        );
    }

    // A folder that is not a bundle.
    let bare = folder.join("Bare.clap");
    std::fs::create_dir_all(&bare).unwrap();
    assert_eq!(
        common::load(&bare).unwrap_err(),
        ClapError::BundleWithoutExecutable(bare.clone())
    );

    // A plugin the library does not have.
    let library = library();
    for id in [
        "org.manguebyte.other",
        "",
        "org.manguebyte.pluck ",
        "org.manguebyte\0.pluck",
    ] {
        assert_eq!(
            library.create(id).unwrap_err(),
            ClapError::UnknownPlugin(id.to_owned())
        );
    }
    // And after all that, the real one still loads and plays.
    let mut plugin = library.create(ID).unwrap();
    let mut processor = plugin.activate(RATE, 64).unwrap();
    processor.process(&mut AudioBuffer::<f32>::new(2, 64));
    assert_eq!(processor.status(), ProcessStatus::Continue);
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn a_bundle_folder_is_loaded_through_its_executable() {
    let folder = common::scratch_folder("clap-bundles");
    let built = common::built_library("mangue_pluck");
    // A hard link where the file system allows one, to save the copy.
    let place = |target: &std::path::Path| {
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        if std::fs::hard_link(&built, target).is_err() {
            std::fs::copy(&built, target).unwrap();
        }
    };
    let plist = |executable: &str| {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\">\n<dict>\n\
             \t<key>CFBundleExecutable</key>\n\t<string>{executable}</string>\n</dict>\n</plist>\n"
        )
    };

    // The executable the property list names.
    let named = folder.join("Named.clap");
    place(&named.join("Contents/MacOS/pluck-binary"));
    std::fs::write(named.join("Contents/Info.plist"), plist("pluck-binary")).unwrap();
    // No property list: the executable named after the bundle.
    let by_name = folder.join("Mangue Pluck.clap");
    place(&by_name.join("Contents/MacOS/Mangue Pluck"));
    // A property list that names nothing that exists, and one file.
    let single = folder.join("Single.clap");
    place(&single.join("Contents/MacOS/whatever"));
    std::fs::write(
        single.join("Contents/Info.plist"),
        plist("../../../outside"),
    )
    .unwrap();

    for bundle in [&named, &by_name, &single] {
        let library =
            common::load(bundle).unwrap_or_else(|error| panic!("{}: {error}", bundle.display()));
        assert_eq!(library.path(), bundle.as_path());
        assert_eq!(library.descriptors().len(), 1);
        let mut plugin = library.create(ID).unwrap();
        let mut processor = plugin.activate(RATE, 64).unwrap();
        let mut events = EventBuffer::with_capacity(1);
        let note = NoteRef::new(0, 69);
        events
            .push(Event {
                offset: 0,
                kind: EventKind::NoteOn {
                    note,
                    velocity: 1.0,
                },
            })
            .unwrap();
        let mut block = AudioBuffer::<f32>::new(2, 64);
        processor.process_with(
            &mut block,
            &ProcessContext::new(&events, &Transport::STOPPED),
        );
        assert!(block.peak() > 0.0, "{} is silent", bundle.display());
    }

    // Two candidates and nothing to choose by.
    let unclear = folder.join("Unclear.clap");
    place(&unclear.join("Contents/MacOS/one"));
    place(&unclear.join("Contents/MacOS/two"));
    assert_eq!(
        common::load(&unclear).unwrap_err(),
        ClapError::BundleWithoutExecutable(unclear.clone())
    );
    let _ = std::fs::remove_dir_all(&folder);
}

#[test]
fn values_in_the_plugins_scale_and_in_its_own_unit_agree() {
    // The helpers the other tests lean on, checked against the plugin.
    let plugin = library().create(ID).unwrap();
    let declared = Pluck::new().params();
    let cutoff = declared.find_param(CUTOFF).unwrap();
    assert_eq!(plain_from_clap(cutoff, 0.0), 100.0);
    assert_eq!(plain_from_clap(cutoff, 1.0), 12_000.0);
    assert_eq!(
        clap_from_plain(cutoff, 1_800.0),
        plugin.param_value(CUTOFF).unwrap()
    );
    let shared: Arc<dyn ParamSet> = declared.clone();
    assert_eq!(
        for_harness(
            &*shared,
            EventKind::ParamValue {
                param: CUTOFF,
                value: 1.0
            }
        ),
        EventKind::ParamValue {
            param: CUTOFF,
            value: 12_000.0
        }
    );
}
