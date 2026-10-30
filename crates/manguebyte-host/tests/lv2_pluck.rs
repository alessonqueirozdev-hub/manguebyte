//! Hosts Mangue Pluck, the framework's example instrument, as an LV2
//! plugin, and holds the result against the same plugin driven directly.
//! LV2 carries notes as MIDI 1.0, so the plugin with no host is given what
//! MIDI can carry: velocities in 7 bits and no note ids.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "lv2", not(miri)))]

mod common;

use mangue_pluck::Pluck;
use manguebyte_core::log::LogLevel;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, Processor, Transport};
use manguebyte_host::lv2::{Lv2Event, Lv2Plugin};
use manguebyte_midi::MidiMessage;
use manguebyte_plugin::{Harness, Plugin};

use common::{as_lv2_delivers, assert_same_bits, load_lv2 as load, plain_at as plain, render};

const RATE: f64 = 48_000.0;
const PLUCK: &str = "urn:manguebyte:org.manguebyte.pluck";

fn pluck_bundle(name: &str) -> common::Lv2Bundle {
    common::lv2_bundle(
        "mangue_pluck",
        mangue_pluck::__manguebyte_lv2_bundle_files,
        name,
    )
}

fn finish(plugin: &mut Lv2Plugin, processor: impl Processor<f32>) {
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0, "an event was dropped");
}

// --- The instrument ---------------------------------------------------------

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const RESONANCE: ParamId = ParamId::from_name("resonance");
const DECAY: ParamId = ParamId::from_name("decay");
const REVERB: ParamId = ParamId::from_name("reverb");
const LEVEL: ParamId = ParamId::from_name("level");

/// Notes, pressure, the pitch wheel and automation, as MIDI carries them:
/// no ids, so no two notes on one key at once.
fn song() -> Vec<(u64, EventKind)> {
    let params = Pluck::new().params();
    let value = |param, position| EventKind::ParamValue {
        param,
        value: plain(&*params, param, position),
    };
    let low = NoteRef::new(0, 45);
    let fifth = NoteRef::new(0, 52);
    let other = NoteRef::new(1, 57);
    let on = |note, velocity| EventKind::NoteOn { note, velocity };
    let off = |note, velocity| EventKind::NoteOff { note, velocity };
    vec![
        (100, on(low, 0.8)),
        (2_500, value(RESONANCE, 0.6)),
        (4_000, on(fifth, 0.7)),
        (4_000, value(CUTOFF, 0.4)),
        (4_001, on(other, 0.6)),
        (
            4_500,
            EventKind::NoteExpression {
                note: fifth,
                expression: Expression::Pressure,
                value: 0.75,
            },
        ),
        (
            5_500,
            EventKind::Midi(MidiMessage::PitchBend {
                channel: 0,
                value: 2_000,
            }),
        ),
        (6_001, off(other, 0.0)),
        (7_777, value(DECAY, 0.8)),
        (9_100, off(low, 0.3)),
        (10_000, value(REVERB, 0.6)),
        (12_000, value(LEVEL, 0.9)),
        (13_000, off(fifth, 0.0)),
        (14_000, on(low, 1.0)),
        (16_000, off(low, 0.0)),
    ]
}

#[test]
fn the_instrument_plays_as_it_does_with_no_host() {
    let bundle = pluck_bundle("instrument");
    let library = load(&bundle);
    let [info] = library.plugins() else {
        panic!("one plugin");
    };
    assert!(info.is_a("InstrumentPlugin"));
    let frames = 24_000;
    let schedule = song();
    let cases: [(usize, &[usize]); 3] = [
        (512, &[512]),
        (256, &[1, 255, 100, 3]),
        (2_048, &[2_048, 7, 1_000]),
    ];
    let mut first: Option<AudioBuffer> = None;
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let mut harness = Harness::<Pluck>::new(RATE, max_block).with_block_pattern(pattern);
        for &(frame, kind) in &schedule {
            harness.schedule(frame, as_lv2_delivers(kind));
        }
        let expected = harness.render(frames);
        assert!(
            expected.channel(0).iter().any(|&s| s != 0.0),
            "{what}: silence"
        );

        let mut plugin = library.create_with(PLUCK, RATE, max_block).unwrap();
        assert!(plugin.takes_notes());
        assert_eq!(plugin.audio_channels(), (0, 0, 2));
        let mut processor = plugin.activate(RATE, max_block).unwrap();
        let mut audio = AudioBuffer::<f32>::new(2, frames);
        let rendered = render(
            &mut processor,
            RATE,
            &schedule,
            pattern,
            max_block,
            &mut audio,
            None,
        );
        finish(&mut plugin, processor);
        assert_eq!(plugin.undeliverable_events(), 0);
        assert_same_bits(&audio, &expected, &what);
        assert_eq!(rendered.heap_calls, 0, "{what}: the host used the heap");
        match &first {
            Some(first) => assert_same_bits(&audio, first, &format!("{what}, against the first")),
            None => first = Some(audio),
        }
    }
}

#[test]
fn the_plugin_logs_through_the_host() {
    let bundle = pluck_bundle("log");
    let library = load(&bundle);
    // Blocks longer than the wrapper prepares for: it says so in its log.
    let mut plugin = library.create_with(PLUCK, RATE, 100_000).unwrap();
    let messages: Vec<(LogLevel, String)> = plugin
        .pump()
        .into_iter()
        .filter_map(|event| match event {
            Lv2Event::Log { level, message } => Some((level, message)),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0].0, LogLevel::Info);
    // Where the host can read the string a lone `%s` stands for, that is
    // the message; elsewhere it is the format itself.
    if cfg!(any(
        target_arch = "x86_64",
        all(target_arch = "aarch64", not(target_vendor = "apple"))
    )) {
        assert!(messages[0].1.contains("100000 frames"), "{}", messages[0].1);
    } else {
        assert_eq!(messages[0].1, "%s");
    }
    // And a 100,000-frame block still sounds as 65,536 and the rest would.
    let mut processor = plugin.activate(RATE, 100_000).unwrap();
    let mut audio = AudioBuffer::<f32>::new(2, 100_000);
    let mut events = EventBuffer::with_capacity(4);
    events
        .push(Event {
            offset: 70_000,
            kind: EventKind::NoteOn {
                note: NoteRef::new(0, 60),
                velocity: 1.0,
            },
        })
        .unwrap();
    let transport = Transport::playing_at(120.0);
    processor.process_with(&mut audio, &ProcessContext::new(&events, &transport));
    assert!(audio.channel(0)[..70_000].iter().all(|&s| s == 0.0));
    assert!(audio.channel(0)[70_000..].iter().any(|&s| s != 0.0));
    finish(&mut plugin, processor);
}

#[test]
fn a_reset_clears_what_was_sounding() {
    let bundle = pluck_bundle("reset");
    let library = load(&bundle);
    let mut plugin = library.create_with(PLUCK, RATE, 512).unwrap();
    let mut processor = plugin.activate(RATE, 512).unwrap();
    let mut events = EventBuffer::with_capacity(4);
    events
        .push(Event {
            offset: 0,
            kind: EventKind::NoteOn {
                note: NoteRef::new(0, 60),
                velocity: 1.0,
            },
        })
        .unwrap();
    let transport = Transport::playing_at(120.0);
    let mut audio = AudioBuffer::<f32>::new(2, 512);
    processor.process_with(&mut audio, &ProcessContext::new(&events, &transport));
    assert!(audio.channel(0).iter().any(|&s| s != 0.0));
    processor.reset();
    let mut after = AudioBuffer::<f32>::new(2, 512);
    processor.process(&mut after);
    assert!(
        after.channel(0).iter().all(|&s| s == 0.0),
        "the note rang on"
    );
    finish(&mut plugin, processor);
}

#[test]
fn the_one_interface_lists_restores_and_runs_it() {
    use manguebyte_host::any::{Format, Kind, Library};
    let folder = pluck_bundle("any");
    // SAFETY: the bundle `pluck_bundle` wrote, whose library is the
    // framework's own LV2 wrapper.
    let library = unsafe { Library::load(&folder) }.expect("the bundle loads");
    assert_eq!(library.format(), Format::Lv2);
    assert_eq!(library.path(), &*folder);
    let [entry] = library.plugins() else {
        panic!("one plugin");
    };
    assert_eq!(entry.id, PLUCK);
    assert_eq!(entry.name, Pluck::INFO.name);
    assert_eq!(entry.kind, Kind::Instrument);
    assert_eq!(entry.categories, ["InstrumentPlugin"]);
    assert!(library.as_lv2().is_some());

    let mut plugin = library.create(PLUCK).unwrap();
    assert!(plugin.takes_notes());
    assert!(plugin.audio_inputs().is_empty());
    assert_eq!(plugin.audio_outputs()[0].channels, 2);
    let level = plugin
        .params()
        .into_iter()
        .find(|param| param.name == "Level")
        .unwrap();
    assert_eq!(level.unit, "dB");
    assert!(
        plugin
            .param_to_text(level.id, level.default)
            .unwrap()
            .starts_with("-4")
    );
    plugin.set_param(level.id, -12.0).unwrap();
    let state = plugin.save_state().unwrap();

    let mut again = library.create(PLUCK).unwrap();
    again.load_state(&state).unwrap();
    assert_eq!(again.param_value(level.id), Some(-12.0));
    let mut processor = again.activate(RATE, 512).unwrap();
    let mut events = EventBuffer::with_capacity(4);
    let note = NoteRef::new(0, 57);
    events
        .push(Event {
            offset: 0,
            kind: EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
        })
        .unwrap();
    let mut block = AudioBuffer::<f32>::new(2, 512);
    processor.process_with(
        &mut block,
        &ProcessContext::new(&events, &Transport::playing_at(120.0)),
    );
    assert!(block.channel(0).iter().any(|&sample| sample != 0.0));
    drop(processor);
    again.deactivate().unwrap();
    assert!(again.pump().is_empty());
}
