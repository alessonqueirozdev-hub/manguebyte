//! The Turtle files of a bundle, checked against the exact text expected.
//!
//! The two plugins here have the parameters, layouts and facts of the
//! example plugins `mangue-duck` and `mangue-pluck`, rebuilt so that this
//! test does not depend on them.

use std::sync::Arc;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_lv2::bundle_files;
use manguebyte_params::{BoolParam, ChoiceParam, FloatParam, FloatRange, IntParam, param_set};
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};

struct DuckParams {
    threshold: FloatParam,
    depth: FloatParam,
    attack: FloatParam,
    release: FloatParam,
    bypass: BoolParam,
}
param_set!(DuckParams {
    threshold,
    depth,
    attack,
    release,
    bypass
});

struct Duck(Arc<DuckParams>);

impl Plugin for Duck {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.duck",
        name: "Mangue Duck",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "A sidechain ducker, as an example of the Manguebyte framework",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 2,
        outputs: 2,
        sidechain: 2,
    };
    type Params = DuckParams;

    fn new() -> Self {
        Self(Arc::new(DuckParams {
            threshold: FloatParam::new(
                "threshold",
                "Threshold",
                FloatRange::linear(-60.0, 0.0),
                -30.0,
            )
            .with_unit("dB")
            .with_precision(1),
            depth: FloatParam::new("depth", "Depth", FloatRange::linear(0.0, 40.0), 12.0)
                .with_unit("dB")
                .with_precision(1),
            attack: FloatParam::new(
                "attack",
                "Attack",
                FloatRange::skewed_around(0.1, 200.0, 10.0),
                5.0,
            )
            .with_unit("ms")
            .with_precision(1),
            release: FloatParam::new(
                "release",
                "Release",
                FloatRange::skewed_around(5.0, 2_000.0, 150.0),
                150.0,
            )
            .with_unit("ms")
            .with_precision(0),
            bypass: BoolParam::new("bypass", "Bypass", false).as_bypass(),
        }))
    }
    fn params(&self) -> Arc<DuckParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

struct PluckParams {
    cutoff: FloatParam,
    resonance: FloatParam,
    decay: FloatParam,
    release: FloatParam,
    reverb: FloatParam,
    level: FloatParam,
}
param_set!(PluckParams {
    cutoff,
    resonance,
    decay,
    release,
    reverb,
    level
});

struct Pluck(Arc<PluckParams>);

impl Plugin for Pluck {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.pluck",
        name: "Mangue Pluck",
        vendor: "Manguebyte",
        version: "0.1.0",
        description: "A polyphonic plucked synth, as an example of the Manguebyte framework",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = PluckParams;

    fn new() -> Self {
        let time = || FloatRange::skewed_around(0.01, 5.0, 0.5);
        Self(Arc::new(PluckParams {
            cutoff: FloatParam::new(
                "cutoff",
                "Cutoff",
                FloatRange::logarithmic(100.0, 12_000.0),
                1_800.0,
            )
            .with_unit("Hz")
            .with_precision(0),
            resonance: FloatParam::new("resonance", "Resonance", FloatRange::linear(0.5, 8.0), 2.0),
            decay: FloatParam::new("decay", "Decay", time(), 0.3).with_unit("s"),
            release: FloatParam::new("release", "Release", time(), 0.4).with_unit("s"),
            reverb: FloatParam::new("reverb", "Reverb", FloatRange::linear(0.0, 1.0), 0.25),
            level: FloatParam::new("level", "Level", FloatRange::linear(-40.0, 6.0), -12.0)
                .with_unit("dB")
                .with_precision(1),
        }))
    }
    fn params(&self) -> Arc<PluckParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

const PREFIXES: &str = "\
@prefix atom: <http://lv2plug.in/ns/ext/atom#> .
@prefix bufsz: <http://lv2plug.in/ns/ext/buf-size#> .
@prefix doap: <http://usefulinc.com/ns/doap#> .
@prefix foaf: <http://xmlns.com/foaf/0.1/> .
@prefix log: <http://lv2plug.in/ns/ext/log#> .
@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix midi: <http://lv2plug.in/ns/ext/midi#> .
@prefix opts: <http://lv2plug.in/ns/ext/options#> .
@prefix pprops: <http://lv2plug.in/ns/ext/port-props#> .
@prefix rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix state: <http://lv2plug.in/ns/ext/state#> .
@prefix time: <http://lv2plug.in/ns/ext/time#> .
@prefix units: <http://lv2plug.in/ns/extensions/units#> .
@prefix urid: <http://lv2plug.in/ns/ext/urid#> .

";

/// Compares line by line first, so a failure names the first wrong line.
fn assert_text(actual: &str, expected: &str) {
    for (number, (got, wanted)) in actual.lines().zip(expected.lines()).enumerate() {
        assert_eq!(got, wanted, "line {}", number + 1);
    }
    assert_eq!(actual, expected);
}

#[test]
fn the_duck_bundle_is_described_exactly() {
    let files = bundle_files::<Duck>("libmangue_duck.so");
    assert_eq!(files.plugin_file, "org.manguebyte.duck.ttl");
    assert_text(
        &files.manifest,
        "\
@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

<urn:manguebyte:org.manguebyte.duck>
    a lv2:Plugin ;
    lv2:binary <libmangue_duck.so> ;
    rdfs:seeAlso <org.manguebyte.duck.ttl> .
",
    );
    let expected = String::from(PREFIXES)
        + "\
<urn:manguebyte:org.manguebyte.duck>
    a lv2:Plugin ;
    doap:name \"Mangue Duck\" ;
    doap:maintainer [
        foaf:name \"Manguebyte\" ;
    ] ;
    rdfs:comment \"A sidechain ducker, as an example of the Manguebyte framework\" ;
    lv2:minorVersion 4 ;
    lv2:microVersion 2 ;
    lv2:requiredFeature urid:map ;
    lv2:optionalFeature lv2:hardRTCapable , opts:options , bufsz:boundedBlockLength , log:log ;
    opts:supportedOption bufsz:maxBlockLength ;
    lv2:extensionData state:interface ;
    lv2:port [
        a lv2:AudioPort , lv2:InputPort ;
        lv2:index 0 ;
        lv2:symbol \"in_l\" ;
        lv2:name \"Input L\" ;
    ] , [
        a lv2:AudioPort , lv2:InputPort ;
        lv2:index 1 ;
        lv2:symbol \"in_r\" ;
        lv2:name \"Input R\" ;
    ] , [
        a lv2:AudioPort , lv2:InputPort ;
        lv2:index 2 ;
        lv2:symbol \"sidechain_l\" ;
        lv2:name \"Sidechain L\" ;
        lv2:portProperty lv2:isSideChain ;
    ] , [
        a lv2:AudioPort , lv2:InputPort ;
        lv2:index 3 ;
        lv2:symbol \"sidechain_r\" ;
        lv2:name \"Sidechain R\" ;
        lv2:portProperty lv2:isSideChain ;
    ] , [
        a lv2:AudioPort , lv2:OutputPort ;
        lv2:index 4 ;
        lv2:symbol \"out_l\" ;
        lv2:name \"Output L\" ;
    ] , [
        a lv2:AudioPort , lv2:OutputPort ;
        lv2:index 5 ;
        lv2:symbol \"out_r\" ;
        lv2:name \"Output R\" ;
    ] , [
        a atom:AtomPort , lv2:InputPort ;
        lv2:index 6 ;
        lv2:symbol \"control\" ;
        lv2:name \"Control\" ;
        atom:bufferType atom:Sequence ;
        atom:supports midi:MidiEvent , time:Position ;
        lv2:designation lv2:control ;
    ] , [
        a atom:AtomPort , lv2:OutputPort ;
        lv2:index 7 ;
        lv2:symbol \"notify\" ;
        lv2:name \"Notify\" ;
        atom:bufferType atom:Sequence ;
        lv2:designation lv2:control ;
    ] , [
        a lv2:ControlPort , lv2:OutputPort ;
        lv2:index 8 ;
        lv2:symbol \"latency\" ;
        lv2:name \"Latency\" ;
        lv2:designation lv2:latency ;
        lv2:minimum 0 ;
        lv2:maximum 16777216 ;
        lv2:portProperty lv2:integer , pprops:notOnGUI ;
        units:unit units:frame ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 9 ;
        lv2:symbol \"threshold\" ;
        lv2:name \"Threshold\" ;
        lv2:default -30.0 ;
        lv2:minimum -60.0 ;
        lv2:maximum 0.0 ;
        units:unit units:db ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 10 ;
        lv2:symbol \"depth\" ;
        lv2:name \"Depth\" ;
        lv2:default 12.0 ;
        lv2:minimum 0.0 ;
        lv2:maximum 40.0 ;
        units:unit units:db ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 11 ;
        lv2:symbol \"attack\" ;
        lv2:name \"Attack\" ;
        lv2:default 5.0 ;
        lv2:minimum 0.1 ;
        lv2:maximum 200.0 ;
        units:unit units:ms ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 12 ;
        lv2:symbol \"release\" ;
        lv2:name \"Release\" ;
        lv2:default 150.0 ;
        lv2:minimum 5.0 ;
        lv2:maximum 2000.0 ;
        units:unit units:ms ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 13 ;
        lv2:symbol \"bypass\" ;
        lv2:name \"Bypass\" ;
        lv2:default 0 ;
        lv2:minimum 0 ;
        lv2:maximum 1 ;
        lv2:portProperty lv2:toggled ;
    ] .
";
    assert_text(&files.plugin, &expected);
}

#[test]
fn the_pluck_bundle_is_described_exactly() {
    let files = bundle_files::<Pluck>("mangue_pluck.dll");
    assert_eq!(files.plugin_file, "org.manguebyte.pluck.ttl");
    assert_eq!(
        files.files(),
        [
            ("manifest.ttl", files.manifest.as_str()),
            ("org.manguebyte.pluck.ttl", files.plugin.as_str())
        ]
    );
    assert_text(
        &files.manifest,
        "\
@prefix lv2: <http://lv2plug.in/ns/lv2core#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .

<urn:manguebyte:org.manguebyte.pluck>
    a lv2:Plugin ;
    lv2:binary <mangue_pluck.dll> ;
    rdfs:seeAlso <org.manguebyte.pluck.ttl> .
",
    );
    let expected = String::from(PREFIXES)
        + "\
<urn:manguebyte:org.manguebyte.pluck>
    a lv2:Plugin , lv2:InstrumentPlugin ;
    doap:name \"Mangue Pluck\" ;
    doap:maintainer [
        foaf:name \"Manguebyte\" ;
    ] ;
    rdfs:comment \"A polyphonic plucked synth, as an example of the Manguebyte framework\" ;
    lv2:minorVersion 4 ;
    lv2:microVersion 2 ;
    lv2:requiredFeature urid:map ;
    lv2:optionalFeature lv2:hardRTCapable , opts:options , bufsz:boundedBlockLength , log:log ;
    opts:supportedOption bufsz:maxBlockLength ;
    lv2:extensionData state:interface ;
    lv2:port [
        a lv2:AudioPort , lv2:OutputPort ;
        lv2:index 0 ;
        lv2:symbol \"out_l\" ;
        lv2:name \"Output L\" ;
    ] , [
        a lv2:AudioPort , lv2:OutputPort ;
        lv2:index 1 ;
        lv2:symbol \"out_r\" ;
        lv2:name \"Output R\" ;
    ] , [
        a atom:AtomPort , lv2:InputPort ;
        lv2:index 2 ;
        lv2:symbol \"control\" ;
        lv2:name \"Control\" ;
        atom:bufferType atom:Sequence ;
        atom:supports midi:MidiEvent , time:Position ;
        lv2:designation lv2:control ;
    ] , [
        a atom:AtomPort , lv2:OutputPort ;
        lv2:index 3 ;
        lv2:symbol \"notify\" ;
        lv2:name \"Notify\" ;
        atom:bufferType atom:Sequence ;
        lv2:designation lv2:control ;
    ] , [
        a lv2:ControlPort , lv2:OutputPort ;
        lv2:index 4 ;
        lv2:symbol \"latency\" ;
        lv2:name \"Latency\" ;
        lv2:designation lv2:latency ;
        lv2:minimum 0 ;
        lv2:maximum 16777216 ;
        lv2:portProperty lv2:integer , pprops:notOnGUI ;
        units:unit units:frame ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 5 ;
        lv2:symbol \"cutoff\" ;
        lv2:name \"Cutoff\" ;
        lv2:default 1800.0 ;
        lv2:minimum 100.0 ;
        lv2:maximum 12000.0 ;
        lv2:portProperty pprops:logarithmic ;
        units:unit units:hz ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 6 ;
        lv2:symbol \"resonance\" ;
        lv2:name \"Resonance\" ;
        lv2:default 2.0 ;
        lv2:minimum 0.5 ;
        lv2:maximum 8.0 ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 7 ;
        lv2:symbol \"decay\" ;
        lv2:name \"Decay\" ;
        lv2:default 0.3 ;
        lv2:minimum 0.01 ;
        lv2:maximum 5.0 ;
        units:unit units:s ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 8 ;
        lv2:symbol \"release\" ;
        lv2:name \"Release\" ;
        lv2:default 0.4 ;
        lv2:minimum 0.01 ;
        lv2:maximum 5.0 ;
        units:unit units:s ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 9 ;
        lv2:symbol \"reverb\" ;
        lv2:name \"Reverb\" ;
        lv2:default 0.25 ;
        lv2:minimum 0.0 ;
        lv2:maximum 1.0 ;
    ] , [
        a lv2:ControlPort , lv2:InputPort ;
        lv2:index 10 ;
        lv2:symbol \"level\" ;
        lv2:name \"Level\" ;
        lv2:default -12.0 ;
        lv2:minimum -40.0 ;
        lv2:maximum 6.0 ;
        units:unit units:db ;
    ] .
";
    assert_text(&files.plugin, &expected);
}

struct OddParams {
    shape: ChoiceParam,
    voices: IntParam,
    secret: BoolParam,
    drive: FloatParam,
    latency: FloatParam,
    fine: FloatParam,
}
param_set!(OddParams {
    shape,
    voices,
    secret,
    drive,
    latency,
    fine
});

struct Odd(Arc<OddParams>);

impl Plugin for Odd {
    const INFO: PluginInfo = PluginInfo {
        id: "com.example/odd \"one\"",
        name: "Odd \"one\"",
        vendor: "",
        version: "2.4.6",
        description: "",
        kind: PluginKind::NoteEffect,
    };
    const LAYOUT: AudioLayout = AudioLayout {
        inputs: 1,
        outputs: 3,
        sidechain: 1,
    };
    type Params = OddParams;

    fn new() -> Self {
        Self(Arc::new(OddParams {
            shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw \"bright\""], 1)
                .non_automatable(),
            voices: IntParam::new("voices", "Voices", 1, 16, 8),
            secret: BoolParam::new("secret", "Secret", true).hidden(),
            drive: FloatParam::new("drive", "Drive", FloatRange::linear(0.0, 2.0), 1.0)
                .with_unit("x%"),
            latency: FloatParam::new("latency", "Latency", FloatRange::linear(0.0, 1.0), 0.0),
            fine: FloatParam::new("fine", "Fine", FloatRange::linear(-1.0, 1.0), 0.0)
                .with_step(0.5),
        }))
    }
    fn params(&self) -> Arc<OddParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

/// Choices, whole numbers, hidden and fixed parameters, units LV2 does not
/// know, a parameter whose id clashes with a fixed port, steps that are not
/// whole numbers, mono and many-channel groups, and an id that has to be
/// encoded.
#[test]
fn every_kind_of_parameter_and_port_is_described() {
    let files = bundle_files::<Odd>("odd one.so");
    assert_eq!(files.plugin_file, "com.example_odd__one_.ttl");
    assert!(
        files.manifest.contains(concat!(
            "<urn:manguebyte:com.example%2Fodd%20%22one%22>\n",
            "    a lv2:Plugin ;\n",
            "    lv2:binary <odd%20one.so> ;\n",
            "    rdfs:seeAlso <com.example_odd__one_.ttl> .\n",
        )),
        "{}",
        files.manifest
    );
    let text = &files.plugin;
    let expected = concat!(
        "<urn:manguebyte:com.example%2Fodd%20%22one%22>\n",
        "    a lv2:Plugin , lv2:MIDIPlugin ;\n",
        "    doap:name \"Odd \\\"one\\\"\" ;\n",
        "    lv2:minorVersion 4010 ;\n",
        "    lv2:microVersion 14 ;\n",
        "    lv2:requiredFeature urid:map ;\n",
        "    lv2:optionalFeature lv2:hardRTCapable , opts:options , bufsz:boundedBlockLength , log:log ;\n",
        "    opts:supportedOption bufsz:maxBlockLength ;\n",
        "    lv2:extensionData state:interface ;\n",
        "    lv2:port [\n",
        "        a lv2:AudioPort , lv2:InputPort ;\n",
        "        lv2:index 0 ;\n",
        "        lv2:symbol \"in\" ;\n",
        "        lv2:name \"Input\" ;\n",
        "    ] , [\n",
        "        a lv2:AudioPort , lv2:InputPort ;\n",
        "        lv2:index 1 ;\n",
        "        lv2:symbol \"sidechain\" ;\n",
        "        lv2:name \"Sidechain\" ;\n",
        "        lv2:portProperty lv2:isSideChain ;\n",
        "    ] , [\n",
        "        a lv2:AudioPort , lv2:OutputPort ;\n",
        "        lv2:index 2 ;\n",
        "        lv2:symbol \"out_1\" ;\n",
        "        lv2:name \"Output 1\" ;\n",
        "    ] , [\n",
    );
    assert!(text.contains(expected), "{text}");
    assert!(text.contains(
        "        lv2:index 4 ;\n        lv2:symbol \"out_3\" ;\n        lv2:name \"Output 3\" ;\n"
    ));

    let params = concat!(
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 8 ;\n",
        "        lv2:symbol \"shape\" ;\n",
        "        lv2:name \"Shape\" ;\n",
        "        lv2:default 1 ;\n",
        "        lv2:minimum 0 ;\n",
        "        lv2:maximum 1 ;\n",
        "        lv2:portProperty lv2:toggled , pprops:notAutomatic ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 9 ;\n",
        "        lv2:symbol \"voices\" ;\n",
        "        lv2:name \"Voices\" ;\n",
        "        lv2:default 8 ;\n",
        "        lv2:minimum 1 ;\n",
        "        lv2:maximum 16 ;\n",
        "        lv2:portProperty lv2:integer ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 10 ;\n",
        "        lv2:symbol \"secret\" ;\n",
        "        lv2:name \"Secret\" ;\n",
        "        lv2:default 1 ;\n",
        "        lv2:minimum 0 ;\n",
        "        lv2:maximum 1 ;\n",
        "        lv2:portProperty lv2:toggled , pprops:notOnGUI ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 11 ;\n",
        "        lv2:symbol \"drive\" ;\n",
        "        lv2:name \"Drive\" ;\n",
        "        lv2:default 1.0 ;\n",
        "        lv2:minimum 0.0 ;\n",
        "        lv2:maximum 2.0 ;\n",
        "        units:unit [\n",
        "            a units:Unit ;\n",
        "            rdfs:label \"x%\" ;\n",
        "            units:symbol \"x%\" ;\n",
        "            units:render \"%f x%%\" ;\n",
        "        ] ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 12 ;\n",
        "        lv2:symbol \"latency_2\" ;\n",
        "        lv2:name \"Latency\" ;\n",
        "        lv2:default 0.0 ;\n",
        "        lv2:minimum 0.0 ;\n",
        "        lv2:maximum 1.0 ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 13 ;\n",
        "        lv2:symbol \"fine\" ;\n",
        "        lv2:name \"Fine\" ;\n",
        "        lv2:default 0.0 ;\n",
        "        lv2:minimum -1.0 ;\n",
        "        lv2:maximum 1.0 ;\n",
        "        pprops:rangeSteps 5 ;\n",
        "    ] .\n",
    );
    assert!(text.ends_with(params), "{text}");
}

struct Choices {
    mode: ChoiceParam,
}
param_set!(Choices { mode });

struct Chooser(Arc<Choices>);

impl Plugin for Chooser {
    const INFO: PluginInfo = PluginInfo {
        id: "org.example.chooser",
        name: "Chooser",
        vendor: "",
        version: "1",
        description: "",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
    type Params = Choices;

    fn new() -> Self {
        Self(Arc::new(Choices {
            mode: ChoiceParam::new("mode", "Mode", &["Low", "Mid", "High"], 2),
        }))
    }
    fn params(&self) -> Arc<Choices> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

#[test]
fn a_choice_of_more_than_two_lists_its_options_by_name() {
    let text = bundle_files::<Chooser>("chooser.so").plugin;
    let expected = concat!(
        "        lv2:symbol \"mode\" ;\n",
        "        lv2:name \"Mode\" ;\n",
        "        lv2:default 2 ;\n",
        "        lv2:minimum 0 ;\n",
        "        lv2:maximum 2 ;\n",
        "        lv2:portProperty lv2:integer , lv2:enumeration ;\n",
        "        lv2:scalePoint [\n",
        "            rdfs:label \"Low\" ;\n",
        "            rdf:value 0 ;\n",
        "        ] , [\n",
        "            rdfs:label \"Mid\" ;\n",
        "            rdf:value 1 ;\n",
        "        ] , [\n",
        "            rdfs:label \"High\" ;\n",
        "            rdf:value 2 ;\n",
        "        ] ;\n",
        "    ] .\n",
    );
    assert!(text.ends_with(expected), "{text}");
    // Version "1" is 1.0.0.
    assert!(text.contains("    lv2:minorVersion 2002 ;\n    lv2:microVersion 2 ;\n"));
    // No vendor and no description: neither line is written.
    assert!(!text.contains("doap:maintainer") && !text.contains("rdfs:comment"));
}

struct StepParams {
    semitones: FloatParam,
    fine: FloatParam,
    even: FloatParam,
    register: FloatParam,
}
param_set!(StepParams {
    semitones,
    fine,
    even,
    register
});

struct Steps(Arc<StepParams>);

impl Plugin for Steps {
    const INFO: PluginInfo = PluginInfo {
        id: "org.example.steps",
        name: "Steps",
        vendor: "",
        version: "1.0.0",
        description: "",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
    type Params = StepParams;

    fn new() -> Self {
        let names = |value: f32| match value as i32 {
            0 => "Low".to_owned(),
            1 => "Mid".to_owned(),
            _ => "High".to_owned(),
        };
        Self(Arc::new(StepParams {
            semitones: FloatParam::new(
                "semitones",
                "Semitones",
                FloatRange::linear(-12.0, 12.0),
                0.0,
            )
            .with_step(1.0)
            .with_unit("st"),
            fine: FloatParam::new("fine", "Fine", FloatRange::linear(-1.0, 1.0), 0.0)
                .with_step(0.5),
            even: FloatParam::new("even", "Even", FloatRange::linear(0.0, 10.0), 4.0)
                .with_step(2.0),
            register: FloatParam::new("register", "Register", FloatRange::linear(0.0, 2.0), 1.0)
                .with_step(1.0)
                .with_formatter(names),
        }))
    }
    fn params(&self) -> Arc<StepParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

/// A float with a step of 1 is a whole-number knob even though its text
/// has decimals ("-12.00"): it is not a list of options. A float with any
/// other step tells the host how many values it has. A float whose steps
/// have names, from its formatter, is listed by name.
#[test]
fn stepped_floats_are_whole_numbers_counted_steps_or_named_options() {
    let text = bundle_files::<Steps>("steps.so").plugin;
    let expected = concat!(
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 5 ;\n",
        "        lv2:symbol \"semitones\" ;\n",
        "        lv2:name \"Semitones\" ;\n",
        "        lv2:default 0 ;\n",
        "        lv2:minimum -12 ;\n",
        "        lv2:maximum 12 ;\n",
        "        lv2:portProperty lv2:integer ;\n",
        "        units:unit units:semitone12TET ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 6 ;\n",
        "        lv2:symbol \"fine\" ;\n",
        "        lv2:name \"Fine\" ;\n",
        "        lv2:default 0.0 ;\n",
        "        lv2:minimum -1.0 ;\n",
        "        lv2:maximum 1.0 ;\n",
        "        pprops:rangeSteps 5 ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 7 ;\n",
        "        lv2:symbol \"even\" ;\n",
        "        lv2:name \"Even\" ;\n",
        "        lv2:default 4.0 ;\n",
        "        lv2:minimum 0.0 ;\n",
        "        lv2:maximum 10.0 ;\n",
        "        pprops:rangeSteps 6 ;\n",
        "    ] , [\n",
        "        a lv2:ControlPort , lv2:InputPort ;\n",
        "        lv2:index 8 ;\n",
        "        lv2:symbol \"register\" ;\n",
        "        lv2:name \"Register\" ;\n",
        "        lv2:default 1 ;\n",
        "        lv2:minimum 0 ;\n",
        "        lv2:maximum 2 ;\n",
        "        lv2:portProperty lv2:integer , lv2:enumeration ;\n",
        "        lv2:scalePoint [\n",
        "            rdfs:label \"Low\" ;\n",
        "            rdf:value 0 ;\n",
        "        ] , [\n",
        "            rdfs:label \"Mid\" ;\n",
        "            rdf:value 1 ;\n",
        "        ] , [\n",
        "            rdfs:label \"High\" ;\n",
        "            rdf:value 2 ;\n",
        "        ] ;\n",
        "    ] .\n",
    );
    assert!(text.ends_with(expected), "{text}");
}

/// lv2core reserves minor version 0 for experimental plugins, which hosts
/// hide by default, and odd minor or micro versions for development
/// versions. A released 1.0.0 is neither.
#[test]
fn a_released_version_is_not_described_as_experimental_or_in_development() {
    let text = bundle_files::<Steps>("steps.so").plugin;
    assert!(
        text.contains("    lv2:minorVersion 2002 ;\n    lv2:microVersion 2 ;\n"),
        "{text}"
    );
}
