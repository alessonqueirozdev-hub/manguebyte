use std::fmt;

use manguebyte_params::{LoadReport, ParamSet, ParamState, ParseStateError};

use crate::{Plugin, plugin_info};

const HEADER: &str = "manguebyte-plugin";
/// The version written when the parameters carry no extra state, so such
/// state reads exactly as it did before extra state existed.
const VERSION_VALUES: u32 = 1;
/// The version written when extra state follows the values.
const VERSION_SECTIONS: u32 = 2;
/// Starts a piece of extra state. No parameter line can start this way:
/// a parameter's id has no spaces.
const SECTION: &str = "section ";
/// Starts each line of a piece of extra state.
const SECTION_LINE: &str = "| ";

/// Why saved state could not be restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateError {
    /// The text is not plugin state at all.
    NotPluginState,
    /// The state was written by a newer, incompatible version of the format.
    UnsupportedVersion(u32),
    /// The state belongs to a different plugin. Holds that plugin's id.
    WrongPlugin(String),
    /// The parameter values inside the state could not be read.
    Params(ParseStateError),
}

impl fmt::Display for StateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotPluginState => f.write_str("not plugin state: header line missing"),
            Self::UnsupportedVersion(version) => {
                write!(f, "plugin state format version {version} is not supported")
            }
            Self::WrongPlugin(id) => write!(f, "this state belongs to the plugin \"{id}\""),
            Self::Params(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for StateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Params(error) => Some(error),
            _ => None,
        }
    }
}

/// Whether `name` can name a piece of extra state: printable ASCII with no
/// spaces and no `=`, as a parameter id.
fn is_valid_section_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b'=')
}

/// Captures a plugin's state as text.
///
/// The text names the plugin, by the id [`plugin_info`] gives, and holds
/// every parameter's real value by identifier. It is safe to call from any
/// thread while audio is running: the values are read from atomics, and
/// nothing is locked but, briefly, `plugin_info`'s cache.
///
/// ```text
/// manguebyte-plugin 1 org.example.gain
/// manguebyte-params 1
/// gain=0.5
/// ```
///
/// When the parameters carry extra state
/// ([`ExtraState`](manguebyte_params::ExtraState) fields, such as a
/// `manguebyte-state` document), the header says version 2, and each piece
/// follows the values as a `section` line with its name and then its text,
/// every line of it behind `| `. Saving extra state may lock whatever holds
/// it, which is never the audio thread's business.
///
/// ```text
/// manguebyte-plugin 2 org.example.sampler
/// manguebyte-params 1
/// gain=0.5
/// section document
/// | manguebyte-state 1
/// | ...
/// ```
pub fn save_state<P: Plugin>(params: &P::Params) -> String {
    let mut sections = String::new();
    params.save_extra(&mut |name, text| {
        debug_assert!(
            is_valid_section_name(name),
            "invalid extra state name {name:?}"
        );
        if !is_valid_section_name(name) {
            return;
        }
        sections.push_str(SECTION);
        sections.push_str(name);
        sections.push('\n');
        for line in text.split('\n') {
            sections.push_str(SECTION_LINE);
            sections.push_str(line);
            sections.push('\n');
        }
    });
    let version = if sections.is_empty() {
        VERSION_VALUES
    } else {
        VERSION_SECTIONS
    };
    format!(
        "{HEADER} {version} {}\n{}{sections}",
        plugin_info::<P>().id,
        params.save()
    )
}

/// Restores state captured by [`save_state`].
///
/// State from another version of the same plugin loads as far as it can; the
/// report says which parameters were missing or unknown. Pieces of extra
/// state that no field recognises are skipped. State from a different
/// plugin is refused, and nothing is changed. Safe to call from any thread
/// while audio is running.
///
/// "The same plugin" means the same id as [`plugin_info`] gives, which is
/// [`Plugin::info`]'s, not [`Plugin::INFO`]'s: two copies of one library
/// that work out different ids refuse each other's state, with
/// [`StateError::WrongPlugin`], and a copy whose identity falls back to
/// `INFO` takes state saved under `INFO`'s id and only that.
///
/// The values are restored first, then each piece of extra state in the
/// order it was saved. Its text comes back exactly, except that a carriage
/// return just before a line feed is lost.
pub fn load_state<P: Plugin>(params: &P::Params, text: &str) -> Result<LoadReport, StateError> {
    let (header, rest) = text.split_once('\n').unwrap_or((text, ""));
    let mut fields = header.split_whitespace();
    if fields.next() != Some(HEADER) {
        return Err(StateError::NotPluginState);
    }
    let version: u32 = fields
        .next()
        .and_then(|field| field.parse().ok())
        .ok_or(StateError::NotPluginState)?;
    if version != VERSION_VALUES && version != VERSION_SECTIONS {
        return Err(StateError::UnsupportedVersion(version));
    }
    let id = fields.next().ok_or(StateError::NotPluginState)?;
    if id != plugin_info::<P>().id {
        return Err(StateError::WrongPlugin(id.to_owned()));
    }
    let (values, sections) = if version == VERSION_SECTIONS {
        split_sections(rest)
    } else {
        (rest, Vec::new())
    };
    let state: ParamState = values.parse().map_err(StateError::Params)?;
    let report = params.load(&state);
    for (name, text) in &sections {
        // A piece nothing recognises is skipped, as an unknown value is.
        let _ = params.load_extra(name, text);
    }
    Ok(report)
}

/// Splits what follows the header into the parameter values and the pieces
/// of extra state, each with its text put back together.
fn split_sections(rest: &str) -> (&str, Vec<(&str, String)>) {
    let mut values_end = rest.len();
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.starts_with(SECTION) {
            values_end = offset;
            break;
        }
        offset += line.len();
    }
    let mut sections: Vec<(&str, String)> = Vec::new();
    let mut started = false;
    for line in rest[values_end..].lines() {
        if let Some(name) = line.strip_prefix(SECTION) {
            sections.push((name.trim(), String::new()));
            started = false;
        } else if let Some((_, text)) = sections.last_mut() {
            let content = match line.strip_prefix(SECTION_LINE) {
                Some(content) => content,
                None if line == SECTION_LINE.trim_end() => "",
                // Anything else is not part of a piece: skip it.
                None => continue,
            };
            if started {
                text.push('\n');
            }
            text.push_str(content);
            started = true;
        }
    }
    (&rest[..values_end], sections)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use manguebyte_core::AudioBuffer;
    use manguebyte_dsp::{ProcessContext, ProcessSpec};
    use manguebyte_params::{ExtraState, FloatParam, FloatRange, param_set};

    use super::{StateError, load_state, save_state, split_sections};
    use crate::{AudioLayout, Plugin, PluginInfo, PluginKind};

    /// Pieces of text kept by name, as documents would be.
    #[derive(Default)]
    struct Pieces(Mutex<Vec<(String, String)>>);

    impl ExtraState for Pieces {
        fn save_extra(&self, section: &mut dyn FnMut(&str, &str)) {
            for (name, text) in self.0.lock().unwrap().iter() {
                section(name, text);
            }
        }
        fn load_extra(&self, name: &str, text: &str) -> bool {
            let mut pieces = self.0.lock().unwrap();
            match pieces.iter_mut().find(|(known, _)| known == name) {
                Some(piece) => {
                    piece.1 = text.to_owned();
                    true
                }
                None => false,
            }
        }
    }

    struct NotesParams {
        gain: FloatParam,
        pieces: Pieces,
    }
    param_set!(NotesParams { gain, extra pieces });

    struct Notes(Arc<NotesParams>);

    impl Plugin for Notes {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.notes",
            name: "Notes",
            vendor: "Manguebyte tests",
            version: "0.1.0",
            description: "Keeps text with its state",
            kind: PluginKind::Effect,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
        type Params = NotesParams;

        fn new() -> Self {
            Self(Arc::new(NotesParams {
                gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
                pieces: Pieces::default(),
            }))
        }
        fn params(&self) -> Arc<NotesParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    }

    fn pieces(params: &NotesParams) -> Vec<(String, String)> {
        params.pieces.0.lock().unwrap().clone()
    }

    #[test]
    fn extra_state_round_trips_after_the_values() {
        let plugin = Notes::new();
        let params = plugin.params();
        // No extra state: version 1, exactly as before extra state existed.
        assert_eq!(
            save_state::<Notes>(&params),
            "manguebyte-plugin 1 org.manguebyte.test.notes\nmanguebyte-params 1\ngain=0.5\n"
        );

        let texts = [
            ("document", "manguebyte-state 1\nnode 0 root {\n}\n"),
            ("empty", ""),
            ("odd", "| section x\n\n  indented\nsection y=1"),
        ];
        *params.pieces.0.lock().unwrap() = texts
            .iter()
            .map(|(name, text)| ((*name).to_owned(), (*text).to_owned()))
            .collect();
        params.gain.set(0.25);
        let saved = save_state::<Notes>(&params);
        assert!(saved.starts_with(
            "manguebyte-plugin 2 org.manguebyte.test.notes\nmanguebyte-params 1\ngain=0.25\nsection document\n| manguebyte-state 1\n"
        ));

        let fresh = Notes::new().params();
        fresh.pieces.0.lock().unwrap().extend(
            ["document", "empty", "odd"].map(|name| (name.to_owned(), String::from("before"))),
        );
        let report = load_state::<Notes>(&fresh, &saved).unwrap();
        assert!(report.is_exact());
        assert_eq!(fresh.gain.value(), 0.25);
        assert_eq!(pieces(&fresh), pieces(&params));

        // A piece nobody recognises is skipped; the rest still loads.
        let unknown = saved.replace("section empty", "section gone");
        let other = Notes::new().params();
        other
            .pieces
            .0
            .lock()
            .unwrap()
            .push((String::from("empty"), String::from("kept")));
        load_state::<Notes>(&other, &unknown).unwrap();
        assert_eq!(
            pieces(&other),
            [(String::from("empty"), String::from("kept"))]
        );

        // Refused state changes nothing, extra state included.
        let foreign = saved.replace("test.notes", "test.other");
        assert!(matches!(
            load_state::<Notes>(&other, &foreign),
            Err(StateError::WrongPlugin(_))
        ));
        assert_eq!(
            pieces(&other),
            [(String::from("empty"), String::from("kept"))]
        );
        assert_eq!(
            load_state::<Notes>(&other, "manguebyte-plugin 3 org.manguebyte.test.notes\n"),
            Err(StateError::UnsupportedVersion(3))
        );
    }

    /// The same plugin as `Notes`, but for the id it works out when loaded.
    struct Renamed(Arc<NotesParams>);

    impl Plugin for Renamed {
        const INFO: PluginInfo = Notes::INFO;
        const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
        type Params = NotesParams;

        fn info() -> PluginInfo {
            PluginInfo {
                id: "org.manguebyte.test.notes-copy",
                ..Self::INFO
            }
        }
        fn new() -> Self {
            Self(Notes::new().0)
        }
        fn params(&self) -> Arc<NotesParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    }

    /// State is saved under the id the plugin works out at run time, and
    /// only state saved under that id loads: not state saved under its
    /// compiled id, which is another plugin's.
    #[test]
    fn state_belongs_to_the_run_time_id() {
        let params = Renamed::new().params();
        params.gain.set(0.75);
        let saved = save_state::<Renamed>(&params);
        assert!(saved.starts_with("manguebyte-plugin 1 org.manguebyte.test.notes-copy\n"));

        let fresh = Renamed::new().params();
        load_state::<Renamed>(&fresh, &saved).unwrap();
        assert_eq!(fresh.gain.value(), 0.75);

        let compiled = save_state::<Notes>(&Notes::new().params());
        assert_eq!(
            load_state::<Renamed>(&fresh, &compiled),
            Err(StateError::WrongPlugin("org.manguebyte.test.notes".into()))
        );
        assert_eq!(
            load_state::<Notes>(&fresh, &saved),
            Err(StateError::WrongPlugin(
                "org.manguebyte.test.notes-copy".into()
            ))
        );
        assert_eq!(fresh.gain.value(), 0.75, "refused state changed nothing");
    }

    #[test]
    fn pieces_of_extra_state_split_from_the_values_exactly() {
        let rest = "manguebyte-params 1\na=1\nsection doc\n| one\n| \n|  two\n|\nsection empty\n| \nsection end\n| x";
        let (values, sections) = split_sections(rest);
        assert_eq!(values, "manguebyte-params 1\na=1\n");
        assert_eq!(
            sections,
            [
                ("doc", String::from("one\n\n two\n")),
                ("empty", String::new()),
                ("end", String::from("x")),
            ]
        );
    }
}
