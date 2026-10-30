//! The Turtle files of an LV2 bundle.
//!
//! An LV2 host learns what a plugin is, and which ports it has, from
//! Turtle (RDF) files in the plugin's bundle folder, before it loads the
//! library. [`bundle_files`] writes both: `manifest.ttl`, which names the
//! plugin and its library, and the plugin's own description.

use std::collections::HashSet;
use std::fmt::Write;

use manguebyte_params::{Param, ParamSet};
use manguebyte_plugin::{Plugin, PluginKind};

use crate::ports::{PortLayout, PortRole};
use crate::uri::{percent_encode, plugin_uri};

/// The text files of a plugin's `.lv2` bundle folder, besides its library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleFiles {
    /// The text of `manifest.ttl`.
    pub manifest: String,
    /// The file name of the plugin's description, which the manifest
    /// points at: the plugin's id with every character other than ASCII
    /// letters, digits, `.`, `-` and `_` replaced by `_`, then `.ttl`.
    pub plugin_file: String,
    /// The text of the plugin's description.
    pub plugin: String,
}

impl BundleFiles {
    /// Each file's name in the bundle folder, with its text.
    pub fn files(&self) -> [(&str, &str); 2] {
        [
            ("manifest.ttl", &self.manifest),
            (&self.plugin_file, &self.plugin),
        ]
    }
}

/// The Turtle files of `P`'s bundle. `binary` is the file name of the
/// plugin's library inside the bundle, such as `"my_plugin.so"`.
///
/// This creates the plugin once, with [`Plugin::new`], to read its
/// parameters.
///
/// A parameter with a whole number of steps over a whole-number range is an
/// `lv2:integer` port (`lv2:toggled` for a switch), and lists its values as
/// `lv2:scalePoint`s with `lv2:enumeration` only when their text is a name
/// rather than the number itself, as a choice's is. Any other stepped
/// parameter gives `pprops:rangeSteps`, the number of values it can take.
///
/// The plugin's version becomes `lv2:minorVersion` and `lv2:microVersion`
/// by a mapping that keeps lv2core's meanings (minor 0 is experimental, odd
/// numbers are development versions) and the order of versions: minor is
/// 2 × (1000 × major + minor) + 2, and micro is 2 × patch + 2, or
/// 2 × patch + 1 for a pre-release such as `1.0.0-beta`. So `1.2.3` is
/// minor 2006, micro 8.
///
/// ```
/// # use std::sync::Arc;
/// # use manguebyte_core::AudioBuffer;
/// # use manguebyte_dsp::{ProcessContext, ProcessSpec};
/// # use manguebyte_params::{FloatParam, FloatRange, param_set};
/// # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
/// # struct GainParams { gain: FloatParam }
/// # param_set!(GainParams { gain });
/// # struct Gain { params: Arc<GainParams> }
/// # impl Plugin for Gain {
/// #     const INFO: PluginInfo = PluginInfo {
/// #         id: "org.example.gain", name: "Gain", vendor: "Example", version: "1.0.0",
/// #         description: "Turns things up and down", kind: PluginKind::Effect,
/// #     };
/// #     const LAYOUT: AudioLayout = AudioLayout::MONO_EFFECT;
/// #     type Params = GainParams;
/// #     fn new() -> Self {
/// #         let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
/// #         Self { params: Arc::new(GainParams { gain: gain.with_unit("dB") }) }
/// #     }
/// #     fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
/// #     fn prepare(&mut self, _: &ProcessSpec) {}
/// #     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
/// # }
/// let files = manguebyte_lv2::bundle_files::<Gain>("gain.so");
/// assert_eq!(files.plugin_file, "org.example.gain.ttl");
/// assert!(files.manifest.contains("lv2:binary <gain.so> ;"));
/// // The parameter's control port, after the fixed ports.
/// assert!(files.plugin.contains(concat!(
///     "        lv2:index 5 ;\n",
///     "        lv2:symbol \"gain\" ;\n",
///     "        lv2:name \"Gain\" ;\n",
///     "        lv2:default 1.0 ;\n",
///     "        lv2:minimum 0.0 ;\n",
///     "        lv2:maximum 2.0 ;\n",
///     "        units:unit units:db ;\n",
/// )));
/// ```
pub fn bundle_files<P: Plugin>(binary: &str) -> BundleFiles {
    let plugin_file = plugin_file_name::<P>();
    BundleFiles {
        manifest: manifest_ttl::<P>(binary, &plugin_file),
        plugin: plugin_ttl::<P>(),
        plugin_file,
    }
}

fn plugin_file_name<P: Plugin>() -> String {
    let mut name: String = manguebyte_plugin::plugin_info::<P>()
        .id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    name.push_str(".ttl");
    name
}

fn manifest_ttl<P: Plugin>(binary: &str, plugin_file: &str) -> String {
    format!(
        "@prefix lv2: <http://lv2plug.in/ns/lv2core#> .\n\
         @prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n\
         \n\
         <{uri}>\n    \
             a lv2:Plugin ;\n    \
             lv2:binary <{binary}> ;\n    \
             rdfs:seeAlso <{plugin_file}> .\n",
        uri = plugin_uri::<P>(),
        binary = percent_encode(binary),
        plugin_file = percent_encode(plugin_file),
    )
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

/// A Turtle string literal.
fn string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A Turtle decimal. A value an `f32` holds exactly, as every value of a
/// float parameter is, is written as the `f32` it is.
fn decimal(value: f64) -> String {
    let single = value as f32;
    let mut text = if f64::from(single) == value {
        format!("{single}")
    } else {
        format!("{value}")
    };
    if !text.contains('.') {
        text.push_str(".0");
    }
    text
}

/// A Turtle integer.
fn integer(value: f64) -> String {
    format!("{}", value as i64)
}

/// Largest major version kept apart by [`lv2_version`]; larger ones count as
/// this one.
const MAX_MAJOR: u64 = 1_000_000;
/// Largest minor version kept apart by [`lv2_version`].
const MAX_MINOR: u64 = 999;
/// Largest patch version kept apart by [`lv2_version`].
const MAX_PATCH: u64 = 1_000_000_000;

/// The `lv2:minorVersion` and `lv2:microVersion` of a semantic version such
/// as `1.2.3` or `1.3.0-beta.2`.
///
/// lv2core gives these numbers meanings of their own: a minor version of
/// zero marks an experimental plugin that hosts hide by default, and an odd
/// minor or micro version marks a development version. Hosts compare the
/// pair to choose the newest of two bundles of the same plugin. So the
/// numbers are not copied across; instead:
///
/// - minor = 2 × (1000 × major + minor) + 2, always even and never zero;
/// - micro = 2 × patch + 2 for a release, and 2 × patch + 1, odd, for a
///   pre-release (anything after a `-`), which sorts before its release
///   and after the release before it.
///
/// So `0.1.0` is minor 4, micro 2; `1.2.3` is minor 2006, micro 8; and
/// `1.3.0-beta.2` is minor 2008, micro 1. Build metadata after a `+` is
/// ignored, a missing or unreadable number counts as zero, and numbers
/// above 1 000 000 (major), 999 (minor) and 1 000 000 000 (patch) count as
/// those, which keeps both results within a signed 32-bit integer.
fn lv2_version(version: &str) -> (u64, u64) {
    let version = version.split('+').next().unwrap_or("");
    let (release, pre_release) = match version.split_once('-') {
        Some((release, _)) => (release, true),
        None => (version, false),
    };
    let mut numbers = release.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        // Too many digits for a `u64` is as large as it gets.
        if digits.is_empty() {
            0
        } else {
            digits.parse().unwrap_or(u64::MAX)
        }
    });
    let major = numbers.next().unwrap_or(0).min(MAX_MAJOR);
    let minor = numbers.next().unwrap_or(0).min(MAX_MINOR);
    let patch = numbers.next().unwrap_or(0).min(MAX_PATCH);
    let lv2_minor = 2 * (1000 * major + minor) + 2;
    let lv2_micro = 2 * patch + if pre_release { 1 } else { 2 };
    (lv2_minor, lv2_micro)
}

/// Whether a step's text is a name rather than the step's own number, as
/// a choice's option names are. Text that reads as the number itself, such
/// as `"-12.00"` for -12, is not a name.
fn is_name(value: f64, label: &str) -> bool {
    label.trim().parse::<f64>() != Ok(value)
}

/// The LV2 unit for a parameter's unit text, if LV2 defines one.
fn known_unit(unit: &str) -> Option<&'static str> {
    Some(match unit {
        "dB" | "db" | "dBFS" => "units:db",
        "Hz" | "hz" => "units:hz",
        "kHz" | "khz" => "units:khz",
        "MHz" => "units:mhz",
        "ms" => "units:ms",
        "s" | "sec" => "units:s",
        "min" => "units:min",
        "%" => "units:pc",
        "st" | "semitones" => "units:semitone12TET",
        "ct" | "cent" | "cents" => "units:cent",
        "bpm" | "BPM" => "units:bpm",
        "°" | "deg" => "units:degree",
        "oct" | "octaves" => "units:oct",
        "bar" | "bars" => "units:bar",
        "beat" | "beats" => "units:beat",
        "frames" | "samples" => "units:frame",
        "m" => "units:m",
        "cm" => "units:cm",
        "mm" => "units:mm",
        "km" => "units:km",
        _ => return None,
    })
}

/// Hands out port symbols: letters, digits and `_`, not starting with a
/// digit, and each used once.
struct Symbols(HashSet<String>);

impl Symbols {
    fn take(&mut self, wanted: &str) -> String {
        let mut base: String = wanted
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        if base.is_empty() || base.starts_with(|c: char| c.is_ascii_digit()) {
            base.insert(0, '_');
        }
        let mut symbol = base.clone();
        let mut count = 1;
        while self.0.contains(&symbol) {
            count += 1;
            symbol = format!("{base}_{count}");
        }
        self.0.insert(symbol.clone());
        symbol
    }
}

/// The symbol and name suffixes for one channel of a group of `count`.
fn channel_suffix(count: usize, channel: usize) -> (String, String) {
    match count {
        1 => (String::new(), String::new()),
        2 => {
            let side = if channel == 0 { "l" } else { "r" };
            (
                format!("_{side}"),
                format!(" {}", side.to_ascii_uppercase()),
            )
        }
        _ => (format!("_{}", channel + 1), format!(" {}", channel + 1)),
    }
}

/// Writes one port: its index, symbol and name, then `lines`, each a
/// property ending with ` ;`.
fn port(out: &mut String, types: &str, index: usize, symbol: &str, name: &str, lines: &[String]) {
    let _ = writeln!(out, "        a {types} ;");
    let _ = writeln!(out, "        lv2:index {index} ;");
    let _ = writeln!(out, "        lv2:symbol {} ;", string(symbol));
    let _ = writeln!(out, "        lv2:name {} ;", string(name));
    for line in lines {
        let _ = writeln!(out, "        {line}");
    }
}

/// A parameter's lowest or highest value: what it stores when given minus
/// or plus infinity, which is its range's end, or else what the end of its
/// `0..=1` positions stands for.
fn bound(param: &dyn Param, toward: f64, normalized: f64) -> f64 {
    match param.stored_plain(toward) {
        Some(value) if value.is_finite() => value,
        _ => param.normalized_to_plain(normalized),
    }
}

/// The lines describing a parameter's control port after its name.
fn param_lines(param: &dyn Param) -> Vec<String> {
    let (mut min, mut max) = (
        bound(param, f64::NEG_INFINITY, 0.0),
        bound(param, f64::INFINITY, 1.0),
    );
    if min > max {
        std::mem::swap(&mut min, &mut max);
    }
    let default = param.default_plain().clamp(min, max);
    let steps = param.step_count().filter(|&steps| steps > 0);
    let whole = steps
        .filter(|&steps| min.fract() == 0.0 && max.fract() == 0.0 && max - min == f64::from(steps));

    let mut lines = Vec::new();
    let mut properties = Vec::new();
    let mut scale_points = Vec::new();
    match whole {
        Some(steps) => {
            lines.push(format!("lv2:default {} ;", integer(default)));
            lines.push(format!("lv2:minimum {} ;", integer(min)));
            lines.push(format!("lv2:maximum {} ;", integer(max)));
            if steps == 1 && min == 0.0 {
                properties.push("lv2:toggled");
            } else {
                properties.push("lv2:integer");
                // Named steps, as a choice has, are listed by name.
                if steps <= 128 {
                    let points: Vec<(f64, String)> = (0..=steps)
                        .map(|step| {
                            let value = min + f64::from(step);
                            (value, param.plain_to_text(value))
                        })
                        .collect();
                    if points.iter().any(|(value, label)| is_name(*value, label)) {
                        properties.push("lv2:enumeration");
                        scale_points = points;
                    }
                }
            }
        }
        None => {
            lines.push(format!("lv2:default {} ;", decimal(default)));
            lines.push(format!("lv2:minimum {} ;", decimal(min)));
            lines.push(format!("lv2:maximum {} ;", decimal(max)));
            let logarithmic = min > 0.0 && max > min && {
                let at = |fraction: f64| min * (max / min).powf(fraction);
                (param.plain_to_normalized(at(0.5)) - 0.5).abs() < 1e-4
                    && (param.plain_to_normalized(at(0.25)) - 0.25).abs() < 1e-4
            };
            if logarithmic {
                properties.push("pprops:logarithmic");
            }
            // A stepped parameter that is not a range of whole numbers:
            // the host is told how many values it has.
            if let Some(steps) = steps {
                lines.push(format!("pprops:rangeSteps {} ;", u64::from(steps) + 1));
            }
        }
    }
    let flags = param.info().flags();
    if !flags.automatable {
        properties.push("pprops:notAutomatic");
    }
    if flags.hidden {
        properties.push("pprops:notOnGUI");
    }
    if !properties.is_empty() {
        lines.push(format!("lv2:portProperty {} ;", properties.join(" , ")));
    }
    if !scale_points.is_empty() {
        let points: Vec<String> = scale_points
            .iter()
            .map(|(value, label)| {
                format!(
                    "[\n            rdfs:label {} ;\n            rdf:value {} ;\n        ]",
                    string(label),
                    integer(*value)
                )
            })
            .collect();
        lines.push(format!("lv2:scalePoint {} ;", points.join(" , ")));
    }
    match param.info().unit() {
        "" => {}
        unit => match known_unit(unit) {
            Some(known) => lines.push(format!("units:unit {known} ;")),
            None => lines.push(format!(
                "units:unit [\n            a units:Unit ;\n            rdfs:label {label} ;\n            \
                 units:symbol {label} ;\n            units:render {render} ;\n        ] ;",
                label = string(unit),
                render = string(&format!("%f {}", unit.replace('%', "%%"))),
            )),
        },
    }
    lines
}

/// The text of `P`'s description: what it is, what it needs from the
/// host, and every port with its index.
fn plugin_ttl<P: Plugin>() -> String {
    let info = manguebyte_plugin::plugin_info::<P>();
    let layout = P::LAYOUT;
    let plugin = P::new();
    let params = plugin.params();
    let params: &dyn ParamSet = &*params;
    let ports = PortLayout::new(layout, params.len()).with_aux(P::AUX_OUTPUTS);

    let mut out = String::from(PREFIXES);
    if !P::AUX_OUTPUTS.is_empty() {
        out.push_str("@prefix pg: <http://lv2plug.in/ns/ext/port-groups#> .\n");
    }
    // Each auxiliary bus is a port group of its own, named by a symbol.
    let mut group_symbols = Symbols(HashSet::new());
    let groups: Vec<String> = P::AUX_OUTPUTS
        .iter()
        .map(|aux| group_symbols.take(&aux.name.to_lowercase()))
        .collect();
    let _ = writeln!(out, "<{}>", plugin_uri::<P>());
    let class = match info.kind {
        PluginKind::Instrument => " , lv2:InstrumentPlugin",
        PluginKind::NoteEffect => " , lv2:MIDIPlugin",
        PluginKind::Analyzer => " , lv2:AnalyserPlugin",
        PluginKind::Effect => "",
    };
    let _ = writeln!(out, "    a lv2:Plugin{class} ;");
    let _ = writeln!(out, "    doap:name {} ;", string(info.name));
    if !info.vendor.is_empty() {
        let _ = writeln!(
            out,
            "    doap:maintainer [\n        foaf:name {} ;\n    ] ;",
            string(info.vendor)
        );
    }
    if !info.description.is_empty() {
        let _ = writeln!(out, "    rdfs:comment {} ;", string(info.description));
    }
    let (minor, micro) = lv2_version(info.version);
    let _ = writeln!(out, "    lv2:minorVersion {minor} ;");
    let _ = writeln!(out, "    lv2:microVersion {micro} ;");
    out.push_str(
        "    lv2:requiredFeature urid:map ;\n\
         \x20   lv2:optionalFeature lv2:hardRTCapable , opts:options , bufsz:boundedBlockLength , log:log ;\n\
         \x20   opts:supportedOption bufsz:maxBlockLength ;\n\
         \x20   lv2:extensionData state:interface ;\n\
         \x20   lv2:port [\n",
    );

    let mut symbols = Symbols(HashSet::new());
    let mut param_list: Vec<&dyn Param> = Vec::new();
    params.visit(&mut |param| param_list.push(param));

    for index in 0..ports.count() {
        if index > 0 {
            out.push_str("    ] , [\n");
        }
        let Some(role) = ports.role(index) else {
            continue;
        };
        match role {
            PortRole::AudioInput(channel) | PortRole::Sidechain(channel) => {
                let sidechain = matches!(role, PortRole::Sidechain(_));
                let (count, symbol, name) = if sidechain {
                    (layout.sidechain, "sidechain", "Sidechain")
                } else {
                    (layout.inputs, "in", "Input")
                };
                let (symbol_suffix, name_suffix) = channel_suffix(count, channel);
                let lines = if sidechain {
                    vec!["lv2:portProperty lv2:isSideChain ;".to_owned()]
                } else {
                    Vec::new()
                };
                port(
                    &mut out,
                    "lv2:AudioPort , lv2:InputPort",
                    index,
                    &symbols.take(&format!("{symbol}{symbol_suffix}")),
                    &format!("{name}{name_suffix}"),
                    &lines,
                );
            }
            PortRole::AudioOutput(channel) => {
                let (symbol_suffix, name_suffix) = channel_suffix(layout.outputs, channel);
                port(
                    &mut out,
                    "lv2:AudioPort , lv2:OutputPort",
                    index,
                    &symbols.take(&format!("out{symbol_suffix}")),
                    &format!("Output{name_suffix}"),
                    &[],
                );
            }
            PortRole::AuxOutput { bus, channel } => {
                // Optional: a host that leaves a bus unconnected switches
                // it off.
                let aux = P::AUX_OUTPUTS[bus];
                let (symbol_suffix, name_suffix) = channel_suffix(aux.channels, channel);
                let mut lines = vec![
                    "lv2:portProperty lv2:connectionOptional ;".to_owned(),
                    format!("pg:group <{}#{}> ;", plugin_uri::<P>(), groups[bus]),
                ];
                let designation = match (aux.channels, channel) {
                    (1, _) => Some("pg:center"),
                    (2, 0) => Some("pg:left"),
                    (2, _) => Some("pg:right"),
                    _ => None,
                };
                if let Some(designation) = designation {
                    lines.push(format!("lv2:designation {designation} ;"));
                }
                port(
                    &mut out,
                    "lv2:AudioPort , lv2:OutputPort",
                    index,
                    &symbols.take(&format!("{}{symbol_suffix}", groups[bus])),
                    &format!("{}{name_suffix}", aux.name),
                    &lines,
                );
            }
            PortRole::Control => port(
                &mut out,
                "atom:AtomPort , lv2:InputPort",
                index,
                &symbols.take("control"),
                "Control",
                &[
                    "atom:bufferType atom:Sequence ;".to_owned(),
                    "atom:supports midi:MidiEvent , time:Position ;".to_owned(),
                    "lv2:designation lv2:control ;".to_owned(),
                ],
            ),
            PortRole::Notify => {
                // A plugin that sends notes sends them here, as MIDI.
                let mut lines = vec!["atom:bufferType atom:Sequence ;".to_owned()];
                if P::NOTE_OUTPUT {
                    lines.push("atom:supports midi:MidiEvent ;".to_owned());
                }
                lines.push("lv2:designation lv2:control ;".to_owned());
                port(
                    &mut out,
                    "atom:AtomPort , lv2:OutputPort",
                    index,
                    &symbols.take("notify"),
                    "Notify",
                    &lines,
                );
            }
            PortRole::Latency => port(
                &mut out,
                "lv2:ControlPort , lv2:OutputPort",
                index,
                &symbols.take("latency"),
                "Latency",
                &[
                    "lv2:designation lv2:latency ;".to_owned(),
                    // The largest whole number of frames an `f32` port
                    // holds exactly.
                    "lv2:minimum 0 ;".to_owned(),
                    "lv2:maximum 16777216 ;".to_owned(),
                    "lv2:portProperty lv2:integer , pprops:notOnGUI ;".to_owned(),
                    "units:unit units:frame ;".to_owned(),
                ],
            ),
            PortRole::Param(at) => {
                let Some(&param) = param_list.get(at) else {
                    continue;
                };
                // The name the parameter was made with: the file is written
                // once, so hosts never see a name given at run time
                // (ADR 0020).
                port(
                    &mut out,
                    "lv2:ControlPort , lv2:InputPort",
                    index,
                    &symbols.take(param.id()),
                    param.name(),
                    &param_lines(param),
                );
            }
        }
    }
    out.push_str("    ] .\n");
    for (aux, symbol) in P::AUX_OUTPUTS.iter().zip(&groups) {
        let kind = match aux.channels {
            1 => " , pg:MonoGroup",
            2 => " , pg:StereoGroup",
            _ => "",
        };
        let _ = writeln!(out, "\n<{}#{symbol}>", plugin_uri::<P>());
        let _ = writeln!(out, "    a pg:OutputGroup{kind} ;");
        let _ = writeln!(out, "    lv2:symbol {} ;", string(symbol));
        let _ = writeln!(out, "    rdfs:label {} .", string(aux.name));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_strings_are_valid_turtle() {
        assert_eq!(decimal(-30.0), "-30.0");
        assert_eq!(decimal(f64::from(0.3f32)), "0.3");
        assert_eq!(decimal(0.1), "0.1");
        assert_eq!(decimal(1e-7), "0.0000001");
        assert_eq!(decimal(1800.0), "1800.0");
        assert_eq!(integer(-3.0), "-3");
        assert_eq!(
            string("a \"b\" \\ c\nd\u{1}é"),
            "\"a \\\"b\\\" \\\\ c\\nd\\u0001é\""
        );
    }

    #[test]
    fn versions_follow_lv2s_rules_and_keep_their_order() {
        assert_eq!(lv2_version("1.2.3"), (2006, 8));
        assert_eq!(lv2_version("0.1.0"), (4, 2));
        assert_eq!(lv2_version("1.0.0"), (2002, 2));
        assert_eq!(lv2_version("2.2.3"), (4006, 8), "the major version counts");
        assert_eq!(lv2_version("0.1.0-beta.4"), (4, 1));
        assert_eq!(lv2_version("1.0.0+build.7"), (2002, 2));
        assert_eq!(lv2_version("1.0.0-rc.1+build.7"), (2002, 1));
        assert_eq!(lv2_version("7"), (14_002, 2));
        assert_eq!(lv2_version(""), (2, 2));
        assert_eq!(lv2_version("x.y.z"), (2, 2));
        let huge = "99999999999999999999999.99999.99999999999999";
        assert_eq!(lv2_version(huge), (2_000_002_000, 2_000_000_002));
        assert!(lv2_version(huge).0 <= i32::MAX as u64 && lv2_version(huge).1 <= i32::MAX as u64);

        // Released versions are even and never minor 0; pre-releases have
        // an odd micro; and the order of versions is kept.
        let ordered = [
            "0.0.0",
            "0.0.1-alpha",
            "0.0.1",
            "0.1.0-rc.1",
            "0.1.0",
            "0.1.1",
            "0.2.0",
            "0.999.0",
            "1.0.0-beta",
            "1.0.0",
            "1.0.1",
            "1.2.3",
            "2.0.0",
            "10.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(
                lv2_version(pair[0]) < lv2_version(pair[1]),
                "{} should come before {}",
                pair[0],
                pair[1]
            );
        }
        for version in ordered {
            let (minor, micro) = lv2_version(version);
            assert!(minor != 0 && minor % 2 == 0, "{version}");
            assert_eq!(micro % 2 == 1, version.contains('-'), "{version}");
        }
    }

    #[test]
    fn a_step_is_named_only_when_its_text_is_not_its_number() {
        assert!(!is_name(-12.0, "-12.00"));
        assert!(!is_name(3.0, "3"));
        assert!(!is_name(3.0, " +3.0 "));
        assert!(is_name(3.0, "4"));
        assert!(is_name(0.0, "Sine"));
        assert!(is_name(1.0, "1 st"));
        assert!(is_name(0.0, ""));
    }

    #[test]
    fn symbols() {
        let mut symbols = Symbols(HashSet::new());
        assert_eq!(symbols.take("latency"), "latency");
        assert_eq!(symbols.take("latency"), "latency_2");
        assert_eq!(symbols.take("latency"), "latency_3");
        assert_eq!(symbols.take("1st.gain-db"), "_1st_gain_db");
        assert_eq!(symbols.take(""), "_");
    }
}
