//! Hosts Mangue Duck, the framework's example effect, as an LV2 plugin:
//! its bundle is written from the Turtle files its own code describes,
//! next to the shared library cargo built, and loaded the way an LV2 host
//! loads it. Also hostile bundles, which are refused with a reason.
//!
//! As with the CLAP and VST 3 hosts, the plugin is also linked in as an
//! ordinary library and driven with no host, and the two have to agree to
//! the bit.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "lv2", not(miri)))]

mod common;

use mangue_duck::Duck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::Processor;
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_host::lv2::{
    Direction, Lv2Error, Lv2Event, Lv2Failure, Lv2Library, Lv2Plugin, PortKind, read_bundle,
};
use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{Harness, Plugin};

use common::{as_lv2_delivers, assert_same_bits, load_lv2 as load, plain_at as plain, render};

const RATE: f64 = 48_000.0;
const DUCK: &str = "urn:manguebyte:org.manguebyte.duck";

fn duck_bundle(name: &str) -> common::Lv2Bundle {
    common::lv2_bundle(
        "mangue_duck",
        mangue_duck::__manguebyte_lv2_bundle_files,
        name,
    )
}

// --- What the bundle says ---------------------------------------------------

#[test]
fn the_bundle_describes_the_plugin_without_running_it() {
    let folder = duck_bundle("describe");
    // Reading the bundle runs nothing: it works with the library replaced
    // by bytes no system would load.
    let binary = std::fs::read_dir(&folder)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_none_or(|ext| ext != "ttl"))
        .unwrap();
    std::fs::write(&binary, b"not a library").unwrap();
    let [info] = read_bundle(&folder).unwrap().try_into().unwrap();

    assert_eq!(info.uri, DUCK);
    assert_eq!(info.name, Duck::INFO.name);
    assert_eq!(info.vendor, Duck::INFO.vendor);
    // 0.1.0 as the wrapper writes it: minor 2 × 1 + 2, micro 2.
    assert_eq!(info.version, "4.2");
    assert!(info.classes.is_empty(), "an effect: {:?}", info.classes);
    assert_eq!(
        info.required_features,
        ["http://lv2plug.in/ns/ext/urid#map"]
    );
    assert!(
        info.extension_data
            .contains(&"http://lv2plug.in/ns/ext/state#interface".to_owned())
    );
    assert_eq!(std::fs::canonicalize(&binary).unwrap(), info.binary);

    let kinds: Vec<(PortKind, Direction, bool)> = info
        .ports
        .iter()
        .map(|port| (port.kind, port.direction, port.properties.side_chain))
        .collect();
    use Direction::{Input, Output};
    assert_eq!(
        kinds[..9],
        [
            (PortKind::Audio, Input, false),
            (PortKind::Audio, Input, false),
            (PortKind::Audio, Input, true),
            (PortKind::Audio, Input, true),
            (PortKind::Audio, Output, false),
            (PortKind::Audio, Output, false),
            (PortKind::Atom, Input, false),
            (PortKind::Atom, Output, false),
            (PortKind::Control, Output, false),
        ]
    );
    assert!(info.ports[6].supports_midi && info.ports[6].supports_time);
    assert!(info.ports[8].is_designated("latency"));

    // Every parameter is a control input, in its own unit and range.
    let declared = Duck::new().params();
    let mut expected: Vec<&dyn Param> = Vec::new();
    declared.visit(&mut |param| expected.push(param));
    let controls = &info.ports[9..];
    assert_eq!(controls.len(), expected.len());
    for (port, param) in controls.iter().zip(expected) {
        assert_eq!(port.symbol, param.id());
        assert_eq!(port.name, param.name());
        assert_eq!(port.default, Some(param.default_plain() as f32));
    }
    let threshold = &controls[0];
    assert_eq!(
        (threshold.minimum, threshold.maximum),
        (Some(-60.0), Some(0.0))
    );
    assert_eq!(threshold.unit, "dB");
    assert!(controls[4].properties.toggled, "bypass is a switch");
}

#[test]
fn hostile_bundles_are_refused_with_a_reason() {
    let base = common::scratch_folder("lv2-hostile");
    let make = |name: &str, manifest: &str| {
        let folder = base.join(format!("{name}.lv2"));
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(folder.join("manifest.ttl"), manifest).unwrap();
        std::fs::write(folder.join("plugin.so"), b"").unwrap();
        folder
    };
    let prefix = "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n";
    let plugin =
        |rest: &str| format!("{prefix}<urn:x> a lv2:Plugin ; lv2:binary <plugin.so> {rest} .");

    assert!(matches!(
        read_bundle(base.join("missing.lv2")),
        Err(Lv2Error::NotFound(_))
    ));
    let not_bundle = base.join("plain");
    std::fs::create_dir_all(&not_bundle).unwrap();
    assert!(matches!(
        read_bundle(&not_bundle),
        Err(Lv2Error::NotABundle(_))
    ));

    let broken = make(
        "broken",
        &format!("{prefix}\n\n<urn:x> a lv2:Plugin ; lv2:binary"),
    );
    match read_bundle(&broken) {
        Err(Lv2Error::Turtle { reason, .. }) => assert!(reason.starts_with("line 4"), "{reason}"),
        other => panic!("{other:?}"),
    }

    let outside = make(
        "outside",
        &format!("{prefix}<urn:x> a lv2:Plugin ; lv2:binary <../elsewhere.so> ."),
    );
    std::fs::write(base.join("elsewhere.so"), b"").unwrap();
    assert!(matches!(
        read_bundle(&outside),
        Err(Lv2Error::Malformed { what, .. }) if what.contains("outside")
    ));

    let port = |index: u32, kinds: &str| {
        format!("[ a {kinds} ; lv2:index {index} ; lv2:symbol \"p{index}\" ]")
    };
    let gap = make(
        "gap",
        &plugin(&format!(
            "; lv2:port {} , {}",
            port(0, "lv2:AudioPort , lv2:InputPort"),
            port(2, "lv2:AudioPort , lv2:OutputPort")
        )),
    );
    assert!(matches!(read_bundle(&gap), Err(Lv2Error::Malformed { .. })));
    let both = make(
        "both",
        &plugin(&format!(
            "; lv2:port {}",
            port(0, "lv2:AudioPort , lv2:InputPort , lv2:OutputPort")
        )),
    );
    assert!(matches!(
        read_bundle(&both),
        Err(Lv2Error::Malformed { .. })
    ));

    // A plugin that requires what this host does not give is refused by
    // name, before its library is even asked for it.
    let needy = make(
        "needy",
        &plugin("; lv2:requiredFeature <http://lv2plug.in/ns/ext/worker#schedule>"),
    );
    assert_eq!(read_bundle(&needy).unwrap().len(), 1);
    // SAFETY: the library is an empty file, which no system loads, so no
    // code is run.
    let loaded = unsafe { Lv2Library::load(&needy) };
    assert!(matches!(loaded, Err(Lv2Error::Load { .. })), "{loaded:?}");

    // A seeAlso that leads outside the bundle is not read.
    std::fs::write(base.join("secret.ttl"), "this is not turtle").unwrap();
    let curious = make(
        "curious",
        &plugin("; <http://www.w3.org/2000/01/rdf-schema#seeAlso> <../secret.ttl>"),
    );
    assert_eq!(read_bundle(&curious).unwrap().len(), 1);
}

#[test]
fn a_required_feature_the_host_lacks_is_named() {
    let folder = duck_bundle("needy-duck");
    let manifest = folder.join("manifest.ttl");
    let text = std::fs::read_to_string(&manifest).unwrap();
    let needy = text.replace(
        "a lv2:Plugin ;",
        "a lv2:Plugin ;\n    lv2:requiredFeature <http://lv2plug.in/ns/ext/worker#schedule> ;",
    );
    assert_ne!(text, needy);
    std::fs::write(&manifest, needy).unwrap();
    let library = load(&folder);
    assert_eq!(
        library.create(DUCK).unwrap_err(),
        Lv2Error::MissingFeature("http://lv2plug.in/ns/ext/worker#schedule".into())
    );
    assert!(matches!(
        library.create("urn:nothing"),
        Err(Lv2Error::UnknownPlugin(_))
    ));
}

// --- The effect -------------------------------------------------------------

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

fn duck_reference(
    max_block: usize,
    pattern: &[usize],
    schedule: &[(u64, EventKind)],
    input: &AudioBuffer,
    sidechain: &AudioBuffer,
) -> AudioBuffer {
    let mut harness = Harness::<Duck>::new(RATE, max_block).with_block_pattern(pattern);
    for &(frame, kind) in schedule {
        harness.schedule(frame, as_lv2_delivers(kind));
    }
    harness.process_with_sidechain(input, sidechain)
}

fn finish(plugin: &mut Lv2Plugin, processor: impl Processor<f32>) {
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0, "an event was dropped");
}

#[test]
fn the_effect_sounds_as_it_does_with_no_host_however_the_blocks_fall() {
    let bundle = duck_bundle("effect");
    let library = load(&bundle);
    let frames = 24_000;
    let input = programme(2, frames);
    let sidechain = key(2, frames);
    let schedule = duck_automation();

    let cases: [(usize, &[usize]); 4] = [
        (256, &[256]),
        (64, &[64]),
        (1_024, &[1, 999, 13, 1_024]),
        (512, &[1, 511, 7, 300, 64, 2, 129]),
    ];
    let mut first: Option<AudioBuffer> = None;
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let expected = duck_reference(max_block, pattern, &schedule, &input, &sidechain);
        assert!(expected != input, "{what}: nothing was ducked");

        let mut plugin = library.create_with(DUCK, RATE, max_block).unwrap();
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
        assert_eq!(rendered.heap_calls, 0, "{what}: the host used the heap");
        // The last change landed on the last frame of the timeline.
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
fn the_switch_the_state_and_another_sample_rate() {
    let bundle = duck_bundle("state");
    let library = load(&bundle);
    let mut plugin = library.create(DUCK).unwrap();
    assert_eq!(plugin.params().len(), 5);
    let bypass = plugin.param(BYPASS).unwrap();
    assert_eq!(
        (bypass.min, bypass.max, bypass.default, bypass.steps),
        (0.0, 1.0, 0.0, Some(1))
    );
    assert_eq!(plugin.param_to_text(BYPASS, 1.0).as_deref(), Some("On"));
    assert_eq!(plugin.text_to_param(BYPASS, "off"), Some(0.0));
    assert_eq!(plugin.param_to_text(DEPTH, 20.0).as_deref(), Some("20 dB"));
    assert_eq!(plugin.text_to_param(DEPTH, "17.5 dB"), Some(17.5));
    assert_eq!(
        plugin.text_to_param(DEPTH, "999"),
        Some(40.0),
        "held to the range"
    );
    let threshold = plugin.param(THRESHOLD).unwrap();
    assert_eq!(
        (threshold.min, threshold.max, threshold.default),
        (-60.0, 0.0, -30.0)
    );

    // Set from the thread that owns the plugin. An LV2 plugin reads its
    // control ports only when it runs, so until then the values are the
    // host's alone: a state saved now carries them for the ports, and the
    // plugin's own part does not have them yet. Restored, they reach the
    // plugin as changes on the first frame of its first block.
    plugin.set_param(DEPTH, 40.0).unwrap();
    plugin.set_param(BYPASS, 1.0).unwrap();
    assert!(matches!(
        plugin.set_param(ParamId(7), 1.0),
        Err(Lv2Error::UnknownParam(_))
    ));
    assert!(plugin.set_param(DEPTH, f64::NAN).is_err());
    let unheard = plugin.save_state().unwrap();
    let input = programme(2, 4_410);
    let sidechain = key(2, 4_410);
    {
        let mut early = library.create(DUCK).unwrap();
        early.load_state(&unheard).unwrap();
        let mut processor = early.activate(RATE, 256).unwrap();
        let mut audio = input.clone();
        render(
            &mut processor,
            RATE,
            &[],
            &[256],
            256,
            &mut audio,
            Some(&sidechain),
        );
        let mut harness = Harness::<Duck>::new(RATE, 256);
        harness.automate(0, "depth", 40.0);
        harness.automate(0, "bypass", 1.0);
        let expected = harness.process_with_sidechain(&input, &sidechain);
        assert_same_bits(&audio, &expected, "values the plugin had not heard yet");
    }

    // Once it has run, its own state has them too, and an instance restored
    // from it starts bypassed: here one created for another sample rate.
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut audio = input.clone();
    render(
        &mut processor,
        RATE,
        &[],
        &[256],
        256,
        &mut audio,
        Some(&sidechain),
    );
    let saved = plugin.save_state().unwrap();
    finish(&mut plugin, processor);

    let mut other = library.create(DUCK).unwrap();
    assert_eq!(other.param_value(BYPASS), Some(0.0));
    other.load_state(&saved).unwrap();
    assert_eq!(other.param_value(BYPASS), Some(1.0));
    assert_eq!(other.param_value(DEPTH), Some(40.0));
    assert!(matches!(
        other.load_state(b"nonsense"),
        Err(Lv2Error::BadState)
    ));

    // Activated at 44.1 kHz: the instance is created again and keeps the
    // state; bypassed, the input comes through whole.
    let mut processor = other.activate(44_100.0, 256).unwrap();
    assert_eq!(other.sample_rate(), 44_100.0);
    assert!(matches!(
        other.load_state(&saved),
        Err(Lv2Error::ProcessorAlive)
    ));
    assert!(matches!(
        other.activate(RATE, 256),
        Err(Lv2Error::AlreadyActive)
    ));
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

    // Switched in while it runs: the plugin reads it from the next block.
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
    harness.plugin_mut().prepare(&manguebyte_dsp::ProcessSpec {
        sample_rate: 44_100.0,
        max_block_size: 256,
        num_channels: 2,
    });
    harness.process_with_sidechain(&input, &sidechain);
    harness.automate(4_410, "bypass", 0.0);
    let expected = harness.process_with_sidechain(&input, &sidechain);
    assert_same_bits(&audio, &expected, "switched in from the other thread");

    // A processor prepared for another rate is silent, and says so.
    processor.prepare(&manguebyte_dsp::ProcessSpec {
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
            .contains(&Lv2Event::ProcessFailed(Lv2Failure::SampleRateMismatch))
    );
    drop(processor);
    assert_eq!(other.failed_blocks(), 1);
    other.deactivate().unwrap();
}

#[test]
fn notes_have_nowhere_to_go_in_an_effect_and_are_counted() {
    let bundle = duck_bundle("notes");
    let library = load(&bundle);
    let mut plugin = library.create_with(DUCK, RATE, 256).unwrap();
    // The effect's event input takes MIDI, as every plugin of the wrapper's
    // does; a note on an effect reaches it and does nothing.
    assert!(plugin.takes_notes());
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
    finish(&mut plugin, processor);
    assert_eq!(plugin.undeliverable_events(), 3);
    assert_same_bits(&audio, &input, "an effect with a quiet sidechain");
}
