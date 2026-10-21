//! Hosts Mangue Duck, the framework's example effect, as an Audio Unit:
//! its main input fed through a render callback and its sidechain through
//! another, every parameter automated through
//! `AudioUnitScheduleParameters`, and the unit's own bypass.
//!
//! As in `au_pluck.rs`, the unit is made in this process through the
//! wrapper's factory, so this runs on every system, and the plugin is also
//! driven with no host: the two have to agree to the bit.
//!
//! Not under Miri: the effect is too slow there.
#![cfg(all(feature = "au", not(miri)))]

mod common;

use mangue_duck::Duck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_dsp::{ProcessSpec, Processor};
use manguebyte_host::au::{AuError, AuEvent, AuFailure, AuPlugin, ComponentCodes};
use manguebyte_params::ParamSet;
use manguebyte_plugin::{Harness, Plugin};

use common::{as_au_delivers, assert_same_bits, au_library, plain_at as plain, render};

const RATE: f64 = 48_000.0;

fn create() -> AuPlugin {
    let [kind, subtype, manufacturer] = manguebyte_au::codes(&Duck::INFO);
    au_library::<Duck>()
        .create(ComponentCodes::new(kind, subtype, manufacturer))
        .expect("the unit is made")
}

fn finish(plugin: &mut AuPlugin, processor: impl Processor<f32>) {
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0, "an event was dropped");
}

fn programme(channels: usize, frames: usize) -> AudioBuffer {
    let mut buffer = AudioBuffer::new(channels, frames);
    for (index, channel) in buffer.iter_channels_mut().enumerate() {
        for (frame, sample) in channel.iter_mut().enumerate() {
            let phase = frame as f32 * (0.05 + 0.01 * index as f32);
            *sample = 0.5 * phase.sin();
        }
    }
    buffer
}

fn key(channels: usize, frames: usize) -> AudioBuffer {
    let mut buffer = AudioBuffer::new(channels, frames);
    for (index, channel) in buffer.iter_channels_mut().enumerate() {
        for (frame, sample) in channel.iter_mut().enumerate() {
            let on = (frame / 3_000) % 2 == 0;
            *sample = if on { 0.8 / (1 + index) as f32 } else { 0.0 };
        }
    }
    buffer
}

const THRESHOLD: ParamId = ParamId::from_name("threshold");
const DEPTH: ParamId = ParamId::from_name("depth");
const ATTACK: ParamId = ParamId::from_name("attack");
const RELEASE: ParamId = ParamId::from_name("release");
const BYPASS: ParamId = ParamId::from_name("bypass");

/// Every parameter moved, in its own unit, some on the same frame.
fn duck_automation() -> Vec<(u64, EventKind)> {
    let params = Duck::new().params();
    let value = |param, position| EventKind::ParamValue {
        param,
        value: plain(&*params, param, position),
    };
    vec![
        (2_500, value(DEPTH, 0.75)),
        (7_777, value(RELEASE, 0.2)),
        (9_001, value(THRESHOLD, 0.8)),
        (9_001, value(DEPTH, 0.3)),
        (12_000, value(ATTACK, 0.6)),
        (15_000, value(BYPASS, 1.0)),
        (18_123, value(BYPASS, 0.0)),
        (23_999, value(DEPTH, 1.0)),
    ]
}

#[test]
fn the_effect_sounds_as_it_does_with_no_host_however_the_blocks_fall() {
    let frames = 24_000;
    let input = programme(2, frames);
    let sidechain = key(2, frames);
    let schedule = duck_automation();
    let cases: [(usize, &[usize]); 5] = [
        (256, &[256]),
        (64, &[64]),
        (1_024, &[1, 999, 13, 1_024]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
        (100_000, &[5_000, 3, 12_000]),
    ];
    let mut first: Option<AudioBuffer> = None;
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let mut harness = Harness::<Duck>::new(RATE, max_block).with_block_pattern(pattern);
        for &(frame, kind) in &schedule {
            harness.schedule(frame, as_au_delivers(kind));
        }
        let expected = harness.process_with_sidechain(&input, &sidechain);
        assert!(expected != input, "{what}: nothing was ducked");

        let mut plugin = create();
        let mut processor = plugin.activate(RATE, max_block).unwrap();
        let mut audio = input.clone();
        let rendered = render(
            &mut processor,
            RATE,
            &schedule,
            pattern,
            max_block,
            &mut audio,
            Some(&sidechain),
        );
        finish(&mut plugin, processor);
        assert_eq!(plugin.undeliverable_events(), 0);
        assert_same_bits(&audio, &expected, &what);
        assert_eq!(rendered.heap_calls, 0, "{what}: the heap was used");
        assert_eq!(
            plugin.param_value(DEPTH),
            Some(plain(&*Duck::new().params(), DEPTH, 1.0))
        );
        match &first {
            Some(first) => assert_same_bits(&audio, first, &format!("{what}, against the first")),
            None => first = Some(audio),
        }
    }
}

#[test]
fn the_elements_the_channels_and_the_parameters() {
    let mut plugin = create();
    assert!(!plugin.takes_notes());
    let inputs = plugin.audio_inputs();
    assert_eq!(inputs.len(), 2);
    assert_eq!(
        (
            inputs[0].element,
            inputs[0].name.as_str(),
            inputs[0].channels
        ),
        (0, "Input", 2)
    );
    assert_eq!(
        (
            inputs[1].element,
            inputs[1].name.as_str(),
            inputs[1].channels
        ),
        (1, "Sidechain", 2)
    );
    assert_eq!(plugin.audio_outputs()[0].name, "Output");
    assert_eq!(plugin.supported_channels(), [(2, 2)]);
    assert!(matches!(
        plugin.set_channels(1, 1),
        Err(AuError::Channels {
            inputs: 1,
            outputs: 1
        })
    ));
    plugin.set_channels(2, 2).unwrap();

    let declared = Duck::new().params();
    assert_eq!(plugin.params().len(), declared.len());
    let bypass = plugin.param(BYPASS).unwrap();
    assert_eq!((bypass.min, bypass.max, bypass.steps), (0.0, 1.0, Some(1)));
    let threshold = plugin.param(THRESHOLD).unwrap();
    assert_eq!(
        (
            threshold.min,
            threshold.max,
            threshold.default,
            threshold.unit.as_str()
        ),
        (-60.0, 0.0, -30.0, "dB")
    );
    let depth = declared.find_param(DEPTH).unwrap();
    assert_eq!(
        plugin.param_to_text(DEPTH, 20.0).as_deref(),
        Some(depth.plain_to_text(20.0).as_str())
    );
    assert_eq!(
        plugin.text_to_param(DEPTH, "999"),
        Some(40.0),
        "held to the range"
    );

    // The one interface says the same.
    let library = manguebyte_host::any::Library::from(au_library::<Duck>());
    let entry = &library.plugins()[0];
    assert_eq!(entry.kind, manguebyte_host::any::Kind::Effect);
    assert_eq!(entry.categories, ["Effects"]);
    let any = library.create(&entry.id).unwrap();
    let buses = any.audio_inputs();
    assert_eq!(buses.len(), 2);
    assert!(buses[0].is_main && !buses[1].is_main);
    let switch = any.params().into_iter().find(|p| p.id == BYPASS).unwrap();
    assert_eq!(switch.steps, Some(1));
}

#[test]
fn the_units_own_bypass_passes_the_input_through() {
    let mut plugin = create();
    assert!(plugin.can_bypass());
    assert!(!plugin.is_bypassed());
    let input = programme(2, 4_800);
    let sidechain = key(2, 4_800);
    let mut processor = plugin.activate(RATE, 480).unwrap();
    plugin.set_bypass(true).unwrap();
    assert!(plugin.is_bypassed());
    let mut audio = input.clone();
    let rendered = render(
        &mut processor,
        RATE,
        &[],
        &[480],
        480,
        &mut audio,
        Some(&sidechain),
    );
    assert_eq!(rendered.heap_calls, 0);
    assert_same_bits(&audio, &input, "bypassed");

    // Back in, the unit starts again from silence, as it does when it is
    // first switched on.
    plugin.set_bypass(false).unwrap();
    let mut audio = input.clone();
    render(
        &mut processor,
        RATE,
        &[],
        &[480],
        480,
        &mut audio,
        Some(&sidechain),
    );
    let expected = Harness::<Duck>::new(RATE, 480).process_with_sidechain(&input, &sidechain);
    assert_same_bits(&audio, &expected, "back in");
    finish(&mut plugin, processor);
}

#[test]
fn the_state_another_sample_rate_and_a_wrong_one() {
    let mut plugin = create();
    plugin.set_param(DEPTH, 40.0).unwrap();
    plugin.set_param(BYPASS, 1.0).unwrap();
    let saved = plugin.save_state().unwrap();

    let mut other = create();
    assert_eq!(other.param_value(BYPASS), Some(0.0));
    other.load_state(&saved).unwrap();
    assert_eq!(other.param_value(BYPASS), Some(1.0));
    assert_eq!(other.param_value(DEPTH), Some(40.0));

    // At 44.1 kHz, restored into bypass: the input comes through whole.
    let input = programme(2, 4_410);
    let sidechain = key(2, 4_410);
    let mut processor = other.activate(44_100.0, 256).unwrap();
    let mut audio = input.clone();
    render(
        &mut processor,
        44_100.0,
        &[],
        &[256],
        256,
        &mut audio,
        Some(&sidechain),
    );
    assert_same_bits(&audio, &input, "restored into bypass");

    // Switched in from this thread: the unit hears it from the next block.
    other.set_param(BYPASS, 0.0).unwrap();
    let mut audio = input.clone();
    render(
        &mut processor,
        44_100.0,
        &[],
        &[256],
        256,
        &mut audio,
        Some(&sidechain),
    );
    assert_eq!(other.param_value(BYPASS), Some(0.0));
    let mut harness = Harness::<Duck>::new(44_100.0, 256);
    harness.params().depth.set(40.0);
    harness.params().bypass.set(true);
    harness.plugin_mut().prepare(&ProcessSpec {
        sample_rate: 44_100.0,
        max_block_size: 256,
        num_channels: 2,
    });
    harness.process_with_sidechain(&input, &sidechain);
    harness.automate(4_410, "bypass", 0.0);
    let expected = harness.process_with_sidechain(&input, &sidechain);
    assert_same_bits(&audio, &expected, "switched in from the other thread");

    // A processor prepared for another rate is silent, and says so.
    processor.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 256,
        num_channels: 2,
    });
    let mut audio = input.clone();
    processor.process(&mut audio);
    assert!(audio.channel(0).iter().all(|&sample| sample == 0.0));
    assert!(
        other
            .pump()
            .contains(&AuEvent::ProcessFailed(AuFailure::SampleRateMismatch))
    );
    drop(processor);
    assert_eq!(other.failed_blocks(), 1);
    other.deactivate().unwrap();
}

#[test]
fn notes_have_nowhere_to_go_in_an_effect_and_are_counted() {
    let mut plugin = create();
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let note = NoteRef::new(0, 60);
    let schedule = [
        (
            10,
            EventKind::NoteOn {
                note,
                velocity: 1.0,
            },
        ),
        (20, EventKind::NoteChoke { note }),
        (
            30,
            EventKind::ParamValue {
                param: ParamId(12_345),
                value: 0.5,
            },
        ),
        (
            40,
            EventKind::ParamModulation {
                param: DEPTH,
                amount: 1.0,
                note: None,
            },
        ),
    ];
    let input = programme(2, 1_024);
    let mut audio = input.clone();
    let rendered = render(
        &mut processor,
        RATE,
        &schedule,
        &[256],
        256,
        &mut audio,
        None,
    );
    assert_eq!(rendered.heap_calls, 0);
    // An effect (`aufx`) has no `MusicDeviceMIDIEvent`: the note goes
    // nowhere, with the choke, the unknown parameter and the modulation.
    assert_eq!(plugin.undeliverable_events(), 4);
    assert_same_bits(&audio, &input, "an effect with a quiet sidechain");
    // A change beyond the block's last frame reaches the unit on the first
    // frame of the next block.
    let mut events = manguebyte_dsp::event::EventBuffer::with_capacity(1);
    events
        .push(manguebyte_dsp::event::Event {
            offset: 300,
            kind: EventKind::ParamValue {
                param: BYPASS,
                value: 1.0,
            },
        })
        .unwrap();
    let mut block = AudioBuffer::<f32>::new(2, 256);
    processor.process_with(
        &mut block,
        &manguebyte_dsp::ProcessContext::new(&events, &manguebyte_dsp::Transport::STOPPED),
    );
    assert_eq!(plugin.param_value(BYPASS), Some(0.0), "not yet");
    processor.process(&mut block);
    assert_eq!(plugin.param_value(BYPASS), Some(1.0));
    finish(&mut plugin, processor);
}
