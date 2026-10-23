//! Hosts Mangue Pluck, the framework's example instrument, as an Audio
//! Unit, and holds the result against the same plugin driven directly.
//!
//! The unit is made in this process through the factory of the
//! framework's AU wrapper, from the `Info.plist` the wrapper writes, and
//! driven through the methods it looks up by selector, as AudioToolbox
//! drives a version 2 unit: so on every system. (On macOS, the AU job of
//! continuous integration also hosts the installed component through
//! AudioToolbox, in `au_system.rs`.) An Audio Unit takes notes as MIDI
//! 1.0, so the plugin with no host is given what MIDI can carry.
//!
//! Not under Miri: the instrument is too slow there.
#![cfg(all(feature = "au", not(miri)))]

mod common;

use std::sync::{Mutex, MutexGuard, PoisonError};

use mangue_pluck::Pluck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, Processor, Transport};
use manguebyte_host::au::{AuError, AuEvent, AuPlugin, ComponentCodes};
use manguebyte_midi::MidiMessage;
use manguebyte_params::ParamSet;
use manguebyte_plugin::{Harness, Plugin};

use common::{as_au_delivers, assert_same_bits, au_library, plain_at as plain, render};

const RATE: f64 = 48_000.0;

/// The tests of this file run one at a time, so that the count of live
/// objects of the wrapper's CoreFoundation stand-in belongs to one test.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    ONE_AT_A_TIME.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many CoreFoundation objects are alive, where they are the
/// wrapper's stand-in.
fn live_objects() -> usize {
    #[cfg(not(target_os = "macos"))]
    return manguebyte_au::fake_cf::LIVE.load(std::sync::atomic::Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    0
}

fn pluck_codes() -> ComponentCodes {
    let [kind, subtype, manufacturer] = manguebyte_au::codes(&Pluck::INFO);
    ComponentCodes::new(kind, subtype, manufacturer)
}

fn create() -> AuPlugin {
    au_library::<Pluck>()
        .create(pluck_codes())
        .expect("the unit is made")
}

fn finish(plugin: &mut AuPlugin, processor: impl Processor<f32>) {
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0, "an event was dropped");
}

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const RESONANCE: ParamId = ParamId::from_name("resonance");
const DECAY: ParamId = ParamId::from_name("decay");
const REVERB: ParamId = ParamId::from_name("reverb");
const LEVEL: ParamId = ParamId::from_name("level");

/// Notes, pressure, the pitch wheel and automation, as MIDI carries them,
/// with a note and a change on one frame.
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
fn the_instrument_plays_as_it_does_with_no_host_however_the_blocks_fall() {
    let _serial = serial();
    let library = au_library::<Pluck>();
    let [info] = library.components() else {
        panic!("one unit");
    };
    assert_eq!(info.codes, pluck_codes());
    assert_eq!(
        (info.name.as_str(), info.vendor.as_str()),
        ("Mangue Pluck", "Manguebyte")
    );
    assert_eq!(info.tags, ["Synthesizer"]);
    let frames = 24_000;
    let schedule = song();
    let cases: [(usize, &[usize]); 4] = [
        (512, &[512]),
        (256, &[1, 255, 100, 3]),
        (2_048, &[2_048, 7, 1_000]),
        // Blocks longer than the wrapper's longest slice, which it keeps
        // when it refuses one of 100,000 frames: each block is rendered in
        // slices, with its events scheduled across them.
        (100_000, &[3_000, 5_000, 1, 7_000]),
    ];
    let mut first: Option<AudioBuffer> = None;
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let mut harness = Harness::<Pluck>::new(RATE, max_block).with_block_pattern(pattern);
        for &(frame, kind) in &schedule {
            harness.schedule(frame, as_au_delivers(kind));
        }
        let expected = harness.render(frames);
        assert!(
            expected.channel(0).iter().any(|&s| s != 0.0),
            "{what}: silence"
        );

        let mut plugin = create();
        assert!(plugin.takes_notes());
        assert!(plugin.audio_inputs().is_empty());
        assert_eq!(plugin.audio_outputs()[0].channels, 2);
        let mut processor = plugin.activate(RATE, max_block).unwrap();
        assert_eq!(processor.max_block_size() == max_block, max_block <= 65_536);
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
        assert_eq!(rendered.heap_calls, 0, "{what}: the heap was used");
        // The last change is what the unit holds.
        assert_eq!(
            plugin.param_value(LEVEL),
            Some(plain(&*Pluck::new().params(), LEVEL, 0.9))
        );
        match &first {
            Some(first) => assert_same_bits(&audio, first, &format!("{what}, against the first")),
            None => first = Some(audio),
        }
    }
}

#[test]
fn a_block_longer_than_the_unit_takes_is_rendered_in_slices() {
    let _serial = serial();
    // The wrapper takes slices of up to 65,536 frames, and refuses more.
    let mut plugin = create();
    let mut processor = plugin.activate(RATE, 100_000).unwrap();
    assert!(processor.max_block_size() < 100_000);
    let remarks = plugin.pump();
    assert!(
        remarks
            .iter()
            .any(|event| matches!(event, AuEvent::Remark(text) if text.contains("refused"))),
        "{remarks:?}"
    );
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

    // As loud, to the bit, as the plugin with no host in one block.
    let mut harness = Harness::<Pluck>::new(RATE, 100_000);
    harness.schedule(70_000, as_au_delivers(events.as_slice()[0].kind));
    let expected = harness.render(100_000);
    assert_same_bits(&audio, &expected, "one long block");
}

#[test]
fn a_reset_clears_what_was_sounding() {
    let _serial = serial();
    let mut plugin = create();
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
fn parameters_text_and_state() {
    let _serial = serial();
    let live = live_objects();
    {
        let mut plugin = create();
        let declared = Pluck::new().params();
        assert_eq!(plugin.params().len(), declared.len());
        let level = plugin.param(LEVEL).unwrap().clone();
        let own = declared.find_param(LEVEL).unwrap();
        assert_eq!(level.name, "Level");
        assert_eq!(level.unit, "dB");
        assert_eq!(level.steps, None);
        assert!(level.is_automatable() && level.has_strings());
        assert_eq!(level.default, f64::from(own.default_plain() as f32));
        // The unit words its values itself.
        assert_eq!(
            plugin.param_to_text(LEVEL, -12.0).as_deref(),
            Some(own.plain_to_text(-12.0).as_str())
        );
        assert!(
            plugin
                .param_to_text(LEVEL, level.default)
                .unwrap()
                .starts_with("-4")
        );
        assert_eq!(
            plugin.text_to_param(LEVEL, &own.plain_to_text(-9.0)),
            own.text_to_plain(&own.plain_to_text(-9.0))
                .map(|value| f64::from(value as f32))
        );
        assert_eq!(plugin.param_to_text(ParamId(1), 0.0), None);

        plugin.set_param(LEVEL, -12.0).unwrap();
        assert_eq!(plugin.param_value(LEVEL), Some(-12.0));
        plugin.set_param(LEVEL, 1_000.0).unwrap();
        assert_eq!(
            plugin.param_value(LEVEL),
            Some(level.max),
            "held to the range"
        );
        plugin.set_param(LEVEL, -12.0).unwrap();
        assert!(matches!(
            plugin.set_param(ParamId(1), 0.0),
            Err(AuError::UnknownParam(_))
        ));
        assert!(plugin.set_param(LEVEL, f64::NAN).is_err());

        // The state is the XML of ClassInfo, and comes back.
        let state = plugin.save_state().unwrap();
        let text = String::from_utf8(state.clone()).unwrap();
        assert!(text.starts_with("<?xml"), "{text}");
        assert!(text.contains("<key>data</key>"), "{text}");
        let mut again = create();
        assert_ne!(again.param_value(LEVEL), Some(-12.0));
        again.load_state(&state).unwrap();
        assert_eq!(again.param_value(LEVEL), Some(-12.0));
        // Bytes that are no unit's state are refused, and change nothing.
        // Which error depends on the system: the CoreFoundation stand-in
        // refuses them as a property list, while the real one reads the
        // bare word as an old-style (OpenStep) string and the unit then
        // refuses that as its class data.
        assert!(again.load_state(b"nonsense").is_err());
        assert_eq!(again.param_value(LEVEL), Some(-12.0));
        // Another unit's state is refused, and changes nothing.
        let other = another_units(&text);
        assert_ne!(other, text);
        assert!(matches!(
            again.load_state(other.as_bytes()),
            Err(AuError::Status { .. })
        ));
        assert_eq!(again.param_value(LEVEL), Some(-12.0));

        // A state given while the processor runs is heard from the next
        // block, as Apple's units take one.
        let mut processor = again.activate(RATE, 256).unwrap();
        plugin.set_param(LEVEL, -20.0).unwrap();
        again.load_state(&plugin.save_state().unwrap()).unwrap();
        let mut block = AudioBuffer::<f32>::new(2, 256);
        processor.process(&mut block);
        assert_eq!(again.param_value(LEVEL), Some(-20.0));
        finish(&mut again, processor);
        assert!(again.pump().is_empty());
    }
    assert_eq!(live_objects(), live, "a CoreFoundation object leaked");
}

/// A state as `text` is, with another subtype: another unit's.
fn another_units(text: &str) -> String {
    let key = text
        .find("<key>subtype</key>")
        .expect("the state has a subtype");
    let open = key + text[key..].find("<integer>").unwrap() + "<integer>".len();
    let close = open + text[open..].find("</integer>").unwrap();
    let subtype: i64 = text[open..close].trim().parse().unwrap();
    format!("{}{}{}", &text[..open], subtype + 1, &text[close..])
}

#[test]
fn what_cannot_be_asked_is_refused() {
    let _serial = serial();
    let library = au_library::<Pluck>();
    let other = ComponentCodes::new(*b"aumu", *b"none", *b"Mngb");
    assert!(matches!(
        library.create(other),
        Err(AuError::UnknownComponent(_))
    ));
    assert!(matches!(
        library.create_by_id("not codes"),
        Err(AuError::UnknownComponent(_))
    ));
    let mut plugin = create();
    assert!(
        !plugin.can_bypass(),
        "an instrument has nothing to pass through"
    );
    assert!(matches!(
        plugin.set_bypass(true),
        Err(AuError::Unsupported(_))
    ));
    assert!(matches!(
        plugin.set_channels(2, 2),
        Err(AuError::Channels { .. })
    ));
    for (rate, block) in [(0.0, 512), (f64::NAN, 512), (RATE, 0), (RATE, usize::MAX)] {
        assert!(matches!(
            plugin.activate(rate, block),
            Err(AuError::InvalidArgument(_))
        ));
    }
    let processor = plugin.activate(RATE, 512).unwrap();
    assert!(plugin.is_active() && plugin.has_processor());
    assert!(matches!(
        plugin.activate(RATE, 512),
        Err(AuError::AlreadyActive)
    ));
    assert!(matches!(plugin.deactivate(), Err(AuError::ProcessorAlive)));
    assert!(matches!(
        plugin.set_channels(0, 2),
        Err(AuError::ProcessorAlive)
    ));
    assert_eq!(plugin.latency(), 0);
    assert_eq!(plugin.tail_seconds(), 0.0);
    finish(&mut plugin, processor);
    assert!(!plugin.is_active());
}

#[test]
fn the_one_interface_lists_restores_and_runs_it() {
    use manguebyte_host::any::{Format, Kind, Library};
    let _serial = serial();
    let library = Library::from(au_library::<Pluck>());
    assert_eq!(library.format(), Format::Au);
    assert_eq!(library.path().to_str(), Some("Mangue Pluck.component"));
    let [entry] = library.plugins() else {
        panic!("one plugin");
    };
    assert_eq!(entry.id, pluck_codes().to_string());
    assert_eq!(entry.name, Pluck::INFO.name);
    assert_eq!(entry.vendor, Pluck::INFO.vendor);
    assert_eq!(entry.version, "0.1.0");
    assert_eq!(entry.kind, Kind::Instrument);
    assert_eq!(entry.categories, ["Synthesizer"]);
    assert!(library.as_au().is_some());

    let mut plugin = library.create(&entry.id).unwrap();
    assert_eq!(plugin.format(), Format::Au);
    assert!(plugin.takes_notes());
    assert!(plugin.audio_inputs().is_empty());
    assert_eq!(plugin.audio_outputs()[0].channels, 2);
    assert!(plugin.audio_outputs()[0].is_main);
    let level = plugin
        .params()
        .into_iter()
        .find(|param| param.name == "Level")
        .unwrap();
    assert_eq!(level.unit, "dB");
    assert!(level.automatable && !level.read_only);
    plugin.set_param(level.id, -12.0).unwrap();
    let state = plugin.save_state().unwrap();

    let mut again = library.create(&entry.id).unwrap();
    again.load_state(&state).unwrap();
    assert_eq!(again.param_value(level.id), Some(-12.0));
    let mut processor = again.activate(RATE, 512).unwrap();
    let mut events = EventBuffer::with_capacity(4);
    events
        .push(Event {
            offset: 0,
            kind: EventKind::NoteOn {
                note: NoteRef::new(0, 57),
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
    assert!(matches!(
        library.create("aumu:none:none"),
        Err(manguebyte_host::any::Error::UnknownPlugin(_))
    ));
}
