//! What an LV2 bundle's Turtle files say about its plugins.
//!
//! An LV2 host learns everything it needs to list a plugin, and to connect
//! its ports, from the bundle's `manifest.ttl` and the files it points at,
//! without loading the plugin's library. This module reads them with the
//! crate's own Turtle reader and turns the graph into plain Rust values.

use std::path::{Path, PathBuf};

use super::Lv2Error;
use super::turtle::{Graph, Literal, RDF, RDF_TYPE, Term};

/// Longest Turtle file read from a bundle.
const MAX_FILE_BYTES: u64 = super::turtle::MAX_DOCUMENT_BYTES as u64;
/// Most files one bundle's manifest may point the reader at.
const MAX_SEE_ALSO: usize = 64;
/// Most plugins listed from one bundle.
pub const MAX_PLUGINS_PER_BUNDLE: usize = 256;
/// Most ports one plugin may have.
pub const MAX_PORTS: usize = 4_096;
/// Most scale points kept for one port.
const MAX_SCALE_POINTS: usize = 1_024;

const LV2: &str = "http://lv2plug.in/ns/lv2core#";
const RDFS: &str = "http://www.w3.org/2000/01/rdf-schema#";
const DOAP: &str = "http://usefulinc.com/ns/doap#";
const FOAF: &str = "http://xmlns.com/foaf/0.1/";
const ATOM: &str = "http://lv2plug.in/ns/ext/atom#";
const PPROPS: &str = "http://lv2plug.in/ns/ext/port-props#";
const UNITS: &str = "http://lv2plug.in/ns/extensions/units#";
const RSZ: &str = "http://lv2plug.in/ns/ext/resize-port#";
const PG: &str = "http://lv2plug.in/ns/ext/port-groups#";
const MIDI_EVENT: &str = "http://lv2plug.in/ns/ext/midi#MidiEvent";
const TIME_POSITION: &str = "http://lv2plug.in/ns/ext/time#Position";

/// What one plugin of a bundle is, as its Turtle files describe it.
#[derive(Debug, Clone, PartialEq)]
pub struct Lv2PluginInfo {
    /// The plugin's URI, which identifies it everywhere: what
    /// [`Lv2Library::create`](super::Lv2Library::create) takes and what a
    /// session stores.
    pub uri: String,
    /// The name shown to people: `doap:name`, in no language or English
    /// if the bundle gives several.
    pub name: String,
    /// Who makes it: the `foaf:name` of its `doap:maintainer`, or of its
    /// `doap:developer`. Empty if the bundle does not say.
    pub vendor: String,
    /// `lv2:minorVersion` and `lv2:microVersion`, as `"minor.micro"`, or
    /// empty if the bundle gives neither.
    pub version: String,
    /// The plugin's classes besides `lv2:Plugin`, as IRIs, such as
    /// `http://lv2plug.in/ns/lv2core#InstrumentPlugin`.
    pub classes: Vec<String>,
    /// The bundle folder.
    pub bundle: PathBuf,
    /// The plugin's library, inside the bundle.
    pub binary: PathBuf,
    /// Features the plugin cannot do without, as URIs.
    pub required_features: Vec<String>,
    /// Features the plugin uses when they are offered.
    pub optional_features: Vec<String>,
    /// Extensions the plugin's `extension_data` answers for.
    pub extension_data: Vec<String>,
    /// The ports, in index order: port `i` is `ports[i]`.
    pub ports: Vec<PortInfo>,
    /// The port groups the ports name, each once, in the order of the
    /// first port in each.
    pub port_groups: Vec<PortGroup>,
}

impl Lv2PluginInfo {
    /// Whether the plugin is of the class `lv2:<name>`, such as
    /// `"InstrumentPlugin"`.
    pub fn is_a(&self, name: &str) -> bool {
        self.classes
            .iter()
            .any(|class| class.strip_prefix(LV2) == Some(name))
    }
}

/// What a port carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PortKind {
    /// A block of audio samples, one `f32` per frame.
    Audio,
    /// One `f32` per block: a control.
    Control,
    /// A control signal at audio rate, one `f32` per frame.
    Cv,
    /// A buffer of atoms, such as a sequence of MIDI events.
    Atom,
    /// A kind this host does not know, such as the long-gone event ports.
    Other,
}

/// Which way a port goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Into the plugin.
    Input,
    /// Out of the plugin.
    Output,
}

/// The properties a port's Turtle gives it, as far as a host acts on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct PortProperties {
    /// `lv2:integer`: whole numbers only.
    pub integer: bool,
    /// `lv2:toggled`: off at or below zero, on above.
    pub toggled: bool,
    /// `lv2:enumeration`: only the values of the scale points.
    pub enumeration: bool,
    /// `pprops:logarithmic`: best shown on a logarithmic scale.
    pub logarithmic: bool,
    /// `pprops:notOnGUI`: not to be shown to people.
    pub not_on_gui: bool,
    /// `pprops:notAutomatic`: not to be automated.
    pub not_automatic: bool,
    /// `lv2:isSideChain`: an input that is not part of the main signal.
    pub side_chain: bool,
    /// `lv2:connectionOptional`: may be left unconnected.
    pub connection_optional: bool,
    /// `lv2:reportsLatency`, the older way of marking the latency output.
    pub reports_latency: bool,
}

/// One port of a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct PortInfo {
    /// Its index, which `connect_port` takes.
    pub index: u32,
    /// Its symbol: a short identifier, unique within the plugin, that
    /// stays the same across versions.
    pub symbol: String,
    /// The name shown to people; the symbol if the bundle gives none.
    pub name: String,
    /// What it carries.
    pub kind: PortKind,
    /// Which way it goes.
    pub direction: Direction,
    /// `lv2:default`, if given and a finite number.
    pub default: Option<f32>,
    /// `lv2:minimum`, if given and a finite number.
    pub minimum: Option<f32>,
    /// `lv2:maximum`, if given and a finite number.
    pub maximum: Option<f32>,
    /// What the host acts on of its properties.
    pub properties: PortProperties,
    /// `lv2:designation`, as an IRI, such as
    /// `http://lv2plug.in/ns/lv2core#latency`.
    pub designation: Option<String>,
    /// Whether an atom port says it carries `midi:MidiEvent`.
    pub supports_midi: bool,
    /// Whether an atom port says it carries `time:Position`.
    pub supports_time: bool,
    /// `rsz:minimumSize`: the fewest bytes an atom port's buffer may have.
    pub minimum_size: Option<usize>,
    /// The unit's symbol, such as `"dB"` or `"Hz"`, or empty.
    pub unit: String,
    /// Named values, as `(value, label)`, in the order given.
    pub scale_points: Vec<(f32, String)>,
    /// `pprops:rangeSteps`: how many values the port takes, if given.
    pub range_steps: Option<u32>,
    /// `pg:group`: the IRI of the port group it belongs to, such as one
    /// output bus of several, if it says. See
    /// [`Lv2PluginInfo::port_groups`].
    pub group: Option<String>,
}

/// A group of ports that belong together, such as the channels of one
/// output bus (the port-groups extension, `pg:Group`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortGroup {
    /// Its IRI, which [`PortInfo::group`] names.
    pub iri: String,
    /// Its `lv2:symbol`, or empty if it gives none.
    pub symbol: String,
    /// The name shown to people: its `rdfs:label`, or else its `lv2:name`,
    /// or else its symbol.
    pub name: String,
}

impl PortInfo {
    /// Whether the port has this designation, `lv2:<name>`.
    pub fn is_designated(&self, name: &str) -> bool {
        self.designation
            .as_deref()
            .and_then(|d| d.strip_prefix(LV2))
            == Some(name)
    }
}

// --- Paths and file IRIs --------------------------------------------------------

/// The `file:` IRI of an absolute path, every byte other than unreserved
/// characters and `/` percent-encoded.
pub(crate) fn file_iri(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    // `\\?\C:\...`, as Windows gives a canonical path, is the path `C:\...`.
    let text = text.strip_prefix("//?/").unwrap_or(&text);
    let mut iri = String::from("file://");
    if !text.starts_with('/') {
        iri.push('/');
    }
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            iri.push(char::from(byte));
        } else {
            iri.push_str(&format!("%{byte:02X}"));
        }
    }
    iri
}

/// The path a `file:` IRI names, or `None` for any other IRI, or one with
/// a host, a query, or an escape that is not valid UTF-8.
pub(crate) fn iri_path(iri: &str) -> Option<PathBuf> {
    let rest = iri.strip_prefix("file://")?;
    if !rest.starts_with('/') || rest.contains(['?', '#']) {
        return None;
    }
    let mut bytes = Vec::with_capacity(rest.len());
    let mut iter = rest.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let high = (iter.next()? as char).to_digit(16)?;
            let low = (iter.next()? as char).to_digit(16)?;
            bytes.push((high * 16 + low) as u8);
        } else {
            bytes.push(byte);
        }
    }
    let text = String::from_utf8(bytes).ok()?;
    if text.contains('\0') {
        return None;
    }
    // `/C:/...` is the Windows path `C:/...`.
    let windows =
        text.len() >= 3 && text.as_bytes()[1].is_ascii_alphabetic() && text.as_bytes()[2] == b':';
    if cfg!(windows) && windows {
        Some(PathBuf::from(&text[1..]))
    } else {
        Some(PathBuf::from(text))
    }
}

/// `path` if it names something inside `folder`, which is canonical.
fn inside(folder: &Path, path: &Path) -> Option<PathBuf> {
    let canonical = std::fs::canonicalize(path).ok()?;
    canonical.starts_with(folder).then_some(canonical)
}

fn read_file(path: &Path) -> Result<String, Lv2Error> {
    use std::io::Read;
    let failed = |reason: String| Lv2Error::Read {
        path: path.to_owned(),
        reason,
    };
    let file = std::fs::File::open(path).map_err(|error| failed(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| failed(error.to_string()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(failed("the file is too long".to_owned()));
    }
    String::from_utf8(bytes).map_err(|_| failed("the file is not UTF-8".to_owned()))
}

fn parse_into(graph: &mut Graph, path: &Path) -> Result<(), Lv2Error> {
    let text = read_file(path)?;
    graph
        .parse(&text, &file_iri(path))
        .map_err(|error| Lv2Error::Turtle {
            path: path.to_owned(),
            reason: error.to_string(),
        })
}

// --- Reading a bundle -----------------------------------------------------------

/// Reads the Turtle files of the bundle folder `path` and describes every
/// plugin in it. Runs no code from the bundle.
///
/// The manifest is read first, and then the files it points at with
/// `rdfs:seeAlso` from each plugin, as far as they are inside the bundle
/// and there are at most 64 of them. A plugin's library must be inside the
/// bundle too: one that points elsewhere is refused, so that a bundle can
/// never make a host load a library it did not bring.
///
/// # Errors
///
/// [`Lv2Error::NotFound`] if there is nothing at `path`,
/// [`Lv2Error::NotABundle`] if it is not a folder with a `manifest.ttl`,
/// [`Lv2Error::Read`] and [`Lv2Error::Turtle`] if a file cannot be read,
/// and [`Lv2Error::Malformed`] if a plugin's description is not one this
/// host can use: no library, a library outside the bundle, ports that are
/// not numbered `0..n` once each, a port that is neither an input nor an
/// output.
pub fn read_bundle(path: impl AsRef<Path>) -> Result<Vec<Lv2PluginInfo>, Lv2Error> {
    let path = path.as_ref();
    let metadata = std::fs::metadata(path).map_err(|_| Lv2Error::NotFound(path.to_owned()))?;
    if !metadata.is_dir() {
        return Err(Lv2Error::NotABundle(path.to_owned()));
    }
    let folder = std::fs::canonicalize(path).map_err(|_| Lv2Error::NotFound(path.to_owned()))?;
    let manifest = folder.join("manifest.ttl");
    if !manifest.is_file() {
        return Err(Lv2Error::NotABundle(path.to_owned()));
    }
    let mut graph = Graph::new();
    parse_into(&mut graph, &manifest)?;

    let plugin_class = Graph::iri(&format!("{LV2}Plugin"));
    let subjects = graph.subjects(RDF_TYPE, &plugin_class);
    let plugins: Vec<Term> = subjects
        .into_iter()
        .filter(|subject| matches!(subject, Term::Iri(_)))
        .take(MAX_PLUGINS_PER_BUNDLE)
        .collect();

    // The files each plugin points at, each read once.
    let mut read: Vec<PathBuf> = vec![manifest.clone()];
    let see_also = format!("{RDFS}seeAlso");
    let mut pending: Vec<PathBuf> = Vec::new();
    for plugin in &plugins {
        for target in graph.objects(plugin, &see_also) {
            let Some(file) = target.iri().and_then(iri_path) else {
                continue;
            };
            if let Some(file) = inside(&folder, &file) {
                if !read.contains(&file) && !pending.contains(&file) {
                    pending.push(file);
                }
            }
        }
    }
    for file in pending.into_iter().take(MAX_SEE_ALSO) {
        parse_into(&mut graph, &file)?;
        read.push(file);
    }

    plugins
        .iter()
        .map(|plugin| describe(&graph, plugin, &folder))
        .collect()
}

fn malformed(uri: &str, what: &'static str) -> Lv2Error {
    Lv2Error::Malformed {
        uri: uri.to_owned(),
        what,
    }
}

/// The text of a literal object, preferring no language, then English.
fn text(graph: &Graph, subject: &Term, predicate: &str) -> Option<String> {
    let literals: Vec<&Literal> = graph
        .objects(subject, predicate)
        .filter_map(Term::literal)
        .collect();
    let pick = literals
        .iter()
        .find(|literal| literal.language.is_none())
        .or_else(|| {
            literals.iter().find(|literal| {
                literal
                    .language
                    .as_deref()
                    .is_some_and(|language| language == "en" || language.starts_with("en-"))
            })
        })
        .or_else(|| literals.first())?;
    Some(pick.value.to_string())
}

fn number(graph: &Graph, subject: &Term, predicate: &str) -> Option<f64> {
    graph
        .objects(subject, predicate)
        .filter_map(Term::literal)
        .find_map(Literal::number)
}

fn iris(graph: &Graph, subject: &Term, predicate: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for iri in graph.objects(subject, predicate).filter_map(Term::iri) {
        if !found.iter().any(|known| known == iri) {
            found.push(iri.to_owned());
        }
    }
    found
}

fn describe(graph: &Graph, plugin: &Term, folder: &Path) -> Result<Lv2PluginInfo, Lv2Error> {
    let uri = plugin.iri().unwrap_or_default().to_owned();
    let binary = graph
        .object(plugin, &format!("{LV2}binary"))
        .and_then(Term::iri)
        .ok_or_else(|| malformed(&uri, "it names no library"))?;
    let binary_path =
        iri_path(binary).ok_or_else(|| malformed(&uri, "its library is not a file"))?;
    // The library has to exist, inside the bundle; a link inside that leads
    // out of it does not count.
    let binary = inside(folder, &binary_path)
        .ok_or_else(|| malformed(&uri, "its library is missing or outside the bundle"))?;

    let name = text(graph, plugin, &format!("{DOAP}name")).unwrap_or_else(|| uri.clone());
    let person = |role: &str| {
        let node = graph.object(plugin, &format!("{DOAP}{role}"))?;
        match node {
            Term::Literal(literal) => Some(literal.value.to_string()),
            node => text(graph, node, &format!("{FOAF}name")),
        }
    };
    let vendor = person("maintainer")
        .or_else(|| person("developer"))
        .unwrap_or_default();
    let minor = number(graph, plugin, &format!("{LV2}minorVersion"));
    let micro = number(graph, plugin, &format!("{LV2}microVersion"));
    let version = match (minor, micro) {
        (None, None) => String::new(),
        (minor, micro) => format!(
            "{}.{}",
            minor.unwrap_or(0.0) as i64,
            micro.unwrap_or(0.0) as i64
        ),
    };
    let classes = iris(graph, plugin, RDF_TYPE)
        .into_iter()
        .filter(|class| class.strip_prefix(LV2) != Some("Plugin"))
        .collect();

    let declared: Vec<&Term> = graph.objects(plugin, &format!("{LV2}port")).collect();
    if declared.len() > MAX_PORTS {
        return Err(malformed(&uri, "it has more ports than this host serves"));
    }
    let mut ports = declared
        .into_iter()
        .map(|node| describe_port(graph, node, &uri))
        .collect::<Result<Vec<PortInfo>, Lv2Error>>()?;
    ports.sort_by_key(|port| port.index);
    if ports
        .iter()
        .enumerate()
        .any(|(position, port)| port.index as usize != position)
    {
        return Err(malformed(
            &uri,
            "its ports are not numbered 0 to n - 1, once each",
        ));
    }
    let mut symbols: Vec<&str> = ports.iter().map(|port| port.symbol.as_str()).collect();
    symbols.sort_unstable();
    if symbols.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(malformed(&uri, "two ports have the same symbol"));
    }

    let mut port_groups: Vec<PortGroup> = Vec::new();
    for iri in ports.iter().filter_map(|port| port.group.as_deref()) {
        if port_groups.iter().any(|group| group.iri == iri) {
            continue;
        }
        let node = Graph::iri(iri);
        let symbol = text(graph, &node, &format!("{LV2}symbol")).unwrap_or_default();
        let name = text(graph, &node, &format!("{RDFS}label"))
            .or_else(|| text(graph, &node, &format!("{LV2}name")))
            .unwrap_or_else(|| symbol.clone());
        port_groups.push(PortGroup {
            iri: iri.to_owned(),
            symbol,
            name,
        });
    }

    Ok(Lv2PluginInfo {
        name,
        vendor,
        version,
        classes,
        bundle: folder.to_owned(),
        binary,
        required_features: iris(graph, plugin, &format!("{LV2}requiredFeature")),
        optional_features: iris(graph, plugin, &format!("{LV2}optionalFeature")),
        extension_data: iris(graph, plugin, &format!("{LV2}extensionData")),
        ports,
        port_groups,
        uri,
    })
}

/// The symbol of a unit LV2 defines, by its IRI.
fn unit_symbol(iri: &str) -> Option<&'static str> {
    Some(match iri.strip_prefix(UNITS)? {
        "db" => "dB",
        "hz" => "Hz",
        "khz" => "kHz",
        "mhz" => "MHz",
        "ms" => "ms",
        "s" => "s",
        "min" => "min",
        "pc" => "%",
        "semitone12TET" => "st",
        "cent" => "ct",
        "bpm" => "BPM",
        "degree" => "°",
        "oct" => "oct",
        "bar" => "bars",
        "beat" => "beats",
        "frame" => "frames",
        "m" => "m",
        "cm" => "cm",
        "mm" => "mm",
        "km" => "km",
        "inch" => "in",
        "mile" => "mi",
        _ => return None,
    })
}

fn describe_port(graph: &Graph, node: &Term, uri: &str) -> Result<PortInfo, Lv2Error> {
    let index = number(graph, node, &format!("{LV2}index"))
        .filter(|index| index.fract() == 0.0 && *index >= 0.0 && *index < MAX_PORTS as f64)
        .ok_or_else(|| malformed(uri, "a port has no index, or one out of range"))?
        as u32;
    let types = iris(graph, node, RDF_TYPE);
    let has = |name: &str| {
        types
            .iter()
            .any(|kind| kind.strip_prefix(LV2) == Some(name))
    };
    let kind = if has("AudioPort") {
        PortKind::Audio
    } else if has("ControlPort") {
        PortKind::Control
    } else if has("CVPort") {
        PortKind::Cv
    } else if types
        .iter()
        .any(|kind| kind.strip_prefix(ATOM) == Some("AtomPort"))
    {
        PortKind::Atom
    } else {
        PortKind::Other
    };
    let direction = match (has("InputPort"), has("OutputPort")) {
        (true, false) => Direction::Input,
        (false, true) => Direction::Output,
        _ => {
            return Err(malformed(
                uri,
                "a port is not exactly one of input and output",
            ));
        }
    };
    let symbol =
        text(graph, node, &format!("{LV2}symbol")).unwrap_or_else(|| format!("port{index}"));
    let name = text(graph, node, &format!("{LV2}name")).unwrap_or_else(|| symbol.clone());
    let finite = |predicate: &str| {
        number(graph, node, &format!("{LV2}{predicate}"))
            .map(|value| value as f32)
            .filter(|value| value.is_finite())
    };

    let properties_list = iris(graph, node, &format!("{LV2}portProperty"));
    let property = |prefix: &str, name: &str| {
        properties_list
            .iter()
            .any(|given| given.strip_prefix(prefix) == Some(name))
    };
    let properties = PortProperties {
        integer: property(LV2, "integer"),
        toggled: property(LV2, "toggled"),
        enumeration: property(LV2, "enumeration"),
        logarithmic: property(PPROPS, "logarithmic"),
        not_on_gui: property(PPROPS, "notOnGUI"),
        not_automatic: property(PPROPS, "notAutomatic"),
        side_chain: property(LV2, "isSideChain"),
        connection_optional: property(LV2, "connectionOptional"),
        reports_latency: property(LV2, "reportsLatency"),
    };
    let supports = iris(graph, node, &format!("{ATOM}supports"));
    let unit = match graph.object(node, &format!("{UNITS}unit")) {
        Some(Term::Iri(iri)) => unit_symbol(iri).unwrap_or_default().to_owned(),
        Some(custom @ Term::Blank(_)) => {
            text(graph, custom, &format!("{UNITS}symbol")).unwrap_or_default()
        }
        _ => String::new(),
    };
    let mut scale_points = Vec::new();
    for point in graph
        .objects(node, &format!("{LV2}scalePoint"))
        .take(MAX_SCALE_POINTS)
    {
        let value = number(graph, point, &format!("{RDF}value")).map(|value| value as f32);
        let label = text(graph, point, &format!("{RDFS}label"));
        if let (Some(value), Some(label)) = (value.filter(|value| value.is_finite()), label) {
            scale_points.push((value, label));
        }
    }
    let range_steps = number(graph, node, &format!("{PPROPS}rangeSteps"))
        .filter(|steps| *steps >= 2.0 && *steps <= f64::from(u32::MAX))
        .map(|steps| steps as u32);
    let minimum_size = number(graph, node, &format!("{RSZ}minimumSize"))
        .filter(|size| *size >= 0.0 && *size <= (1u64 << 30) as f64)
        .map(|size| size as usize);

    Ok(PortInfo {
        index,
        symbol,
        name,
        kind,
        direction,
        default: finite("default"),
        minimum: finite("minimum"),
        maximum: finite("maximum"),
        properties,
        designation: graph
            .object(node, &format!("{LV2}designation"))
            .and_then(Term::iri)
            .map(str::to_owned),
        supports_midi: supports.iter().any(|kind| kind == MIDI_EVENT),
        supports_time: supports.iter().any(|kind| kind == TIME_POSITION),
        minimum_size,
        unit,
        scale_points,
        range_steps,
        group: graph
            .object(node, &format!("{PG}group"))
            .and_then(Term::iri)
            .map(str::to_owned),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_file_iris_go_both_ways() {
        let path = if cfg!(windows) {
            PathBuf::from(r"C:\Plug ins\Gain é.lv2\gain.dll")
        } else {
            PathBuf::from("/plug ins/Gain é.lv2/gain.so")
        };
        let iri = file_iri(&path);
        assert!(iri.starts_with("file:///"), "{iri}");
        assert!(iri.contains("%20") && iri.contains("%C3%A9"), "{iri}");
        assert_eq!(iri_path(&iri), Some(path));
        if cfg!(windows) {
            assert_eq!(file_iri(Path::new(r"\\?\C:\a\b.ttl")), "file:///C:/a/b.ttl");
        }
        for not_a_path in [
            "http://example.org/x",
            "file://host/x",
            "file:///x?y",
            "file:///x%zz",
            "file:///x%FF",
            "file:///x%00y",
            "file:///x%2",
        ] {
            assert_eq!(iri_path(not_a_path), None, "{not_a_path}");
        }
    }
}
