//! One interface over every plugin format.
//!
//! An application that hosts plugins mostly does not care which format a
//! plugin comes in. It lists what a file holds, creates a plugin, shows its
//! parameters, restores a saved state, and runs it. This module does those
//! things through the same types for a CLAP, a VST 3, an LV2 plugin and an
//! Audio Unit:
//!
//! | Type | What it is | Stands for |
//! |---|---|---|
//! | [`Library`] | A loaded file and the plugins in it | [`ClapLibrary`], [`Vst3Module`], [`Lv2Library`] or [`AuLibrary`] |
//! | [`Plugin`] | One plugin, on the main thread | [`ClapPlugin`], [`Vst3Plugin`], [`Lv2Plugin`] or [`AuPlugin`] |
//! | [`AnyProcessor`], or `Box<dyn Processor<f32> + Send>` | The same plugin, on the audio thread | [`ClapProcessor`], [`Vst3Processor`], [`Lv2Processor`] or [`AuProcessor`] |
//!
//! The thread rules are those of the formats' own modules: a [`Plugin`] is
//! not `Send`, and its audio half is the only thing that processes.
//!
//! # Values
//!
//! Each format moves parameter values in a scale of its own, and this
//! module does not pretend otherwise: a [`Param`] says what its scale is
//! with [`min`](Param::min) and [`max`](Param::max), and every value given
//! to or read from a [`Plugin`], or carried by an
//! [`EventKind::ParamValue`](manguebyte_dsp::event::EventKind::ParamValue)
//! for its audio half, is in that scale. For CLAP it is the range the
//! plugin declares; for VST 3 it is the normalized `0..=1` that VST 3
//! automates; for LV2 it is the control port's own unit and range; for an
//! Audio Unit it is the parameter's own unit and range, as
//! `AudioUnitSetParameter` takes it.
//! [`Plugin::param_to_text`] turns any of them into what a person should
//! see.
//!
//! # Example
//!
//! This runs as a test. It hosts the framework's example instrument twice,
//! once through each format, from the entry points it exports, and plays
//! the same note through both.
//!
//! ```
//! use manguebyte_core::AudioBuffer;
//! use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
//! use manguebyte_dsp::{ProcessContext, Transport};
//! use manguebyte_host::any::{Format, Library};
//! use manguebyte_host::clap::ClapLibrary;
//! use manguebyte_host::vst3::Vst3Module;
//!
//! // SAFETY: both entry points are Mangue Pluck's, made by the framework's
//! // own wrappers, which keep to the CLAP and VST 3 interfaces.
//! let libraries = unsafe {
//!     [
//!         Library::from(ClapLibrary::from_entry(&raw const mangue_pluck::clap_entry, "pluck")?),
//!         Library::from(Vst3Module::from_factory(mangue_pluck::GetPluginFactory().cast(), "pluck")?),
//!     ]
//! };
//!
//! let mut played = Vec::new();
//! for library in &libraries {
//!     let entry = &library.plugins()[0];
//!     assert_eq!(entry.name, "Mangue Pluck");
//!     let mut plugin = library.create(&entry.id)?;
//!     let level = plugin.params().into_iter().find(|param| param.name == "Level").unwrap();
//!     assert!(plugin.param_to_text(level.id, level.default).unwrap().starts_with("-4"));
//!
//!     let mut processor = plugin.activate(48_000.0, 512)?;
//!     let mut events = EventBuffer::with_capacity(4);
//!     let note = NoteRef::with_id(0, 57, 1);
//!     events.push(Event { offset: 0, kind: EventKind::NoteOn { note, velocity: 0.8 } }).ok();
//!     let mut block = AudioBuffer::<f32>::new(2, 512);
//!     let transport = Transport::playing_at(120.0);
//!     processor.process_with(&mut block, &ProcessContext::new(&events, &transport));
//!     played.push((library.format(), block.channel(0).to_vec()));
//!
//!     let state = plugin.save_state()?;
//!     drop(processor);
//!     plugin.deactivate()?;
//!     plugin.load_state(&state)?;
//! }
//! // The same plugin, the same note, the same samples, whichever format.
//! assert_eq!(played[0].0, Format::Clap);
//! assert_eq!(played[1].0, Format::Vst3);
//! assert_eq!(played[0].1, played[1].1);
//! assert!(played[0].1.iter().any(|&sample| sample != 0.0));
//! # Ok::<(), manguebyte_host::any::Error>(())
//! ```
//!
//! # Limits
//!
//! Only what the formats share is here. For the rest of a format, such as
//! CLAP's note ports or VST 3's controller mappings, reach the format's own
//! type through [`Library::as_clap`], [`Plugin::as_vst3_mut`],
//! [`AnyProcessor::as_clap`] and their neighbours. The events a plugin
//! sends while processing are read through [`AnyProcessor::output_events`]
//! in one type for every format; what only one format can say, such as
//! CLAP's gestures, from that format's processor.

use std::fmt;
use std::path::{Path, PathBuf};

use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::Event;
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor};

#[cfg(feature = "au")]
use crate::au::{AuError, AuLibrary, AuPlugin, AuProcessor};
#[cfg(feature = "clap")]
use crate::clap::{ClapError, ClapLibrary, ClapPlugin, ClapProcessor};
#[cfg(feature = "lv2")]
use crate::lv2::{Lv2Error, Lv2Library, Lv2Plugin, Lv2Processor};
#[cfg(feature = "vst3")]
use crate::vst3::{ClassId, Vst3Error, Vst3Module, Vst3Plugin, Vst3Processor};

/// A plugin format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Format {
    /// [CLAP](https://cleveraudio.org).
    Clap,
    /// VST 3.
    Vst3,
    /// [LV2](https://lv2plug.in).
    Lv2,
    /// Audio Unit, version 2.
    Au,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Clap => "CLAP",
            Self::Vst3 => "VST 3",
            Self::Lv2 => "LV2",
            Self::Au => "AU",
        })
    }
}

/// What kind of plugin a library says one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Kind {
    /// Makes sound from notes.
    Instrument,
    /// Changes audio.
    Effect,
    /// Changes notes.
    NoteEffect,
    /// Looks at audio and passes it on.
    Analyzer,
    /// The library does not say, or says something else.
    Other,
}

/// Why something in this module did not work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// What the CLAP host said.
    #[cfg(feature = "clap")]
    Clap(ClapError),
    /// What the VST 3 host said.
    #[cfg(feature = "vst3")]
    Vst3(Vst3Error),
    /// What the LV2 host said.
    #[cfg(feature = "lv2")]
    Lv2(Lv2Error),
    /// What the Audio Unit host said.
    #[cfg(feature = "au")]
    Au(AuError),
    /// The file is a plugin of neither format this crate was built for.
    UnknownFormat(PathBuf),
    /// The format is one this crate was built without: its feature is off.
    NotBuiltIn(Format),
    /// The library has no plugin with this identifier.
    UnknownPlugin(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(feature = "clap")]
            Self::Clap(error) => write!(f, "CLAP: {error}"),
            #[cfg(feature = "vst3")]
            Self::Vst3(error) => write!(f, "VST 3: {error}"),
            #[cfg(feature = "lv2")]
            Self::Lv2(error) => write!(f, "LV2: {error}"),
            #[cfg(feature = "au")]
            Self::Au(error) => write!(f, "AU: {error}"),
            Self::UnknownFormat(path) => {
                write!(f, "{} is not a plugin of a known format", path.display())
            }
            Self::NotBuiltIn(format) => write!(f, "this build has no {format} hosting"),
            Self::UnknownPlugin(id) => write!(f, "the library has no plugin {id:?}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            #[cfg(feature = "clap")]
            Self::Clap(error) => Some(error),
            #[cfg(feature = "vst3")]
            Self::Vst3(error) => Some(error),
            #[cfg(feature = "lv2")]
            Self::Lv2(error) => Some(error),
            #[cfg(feature = "au")]
            Self::Au(error) => Some(error),
            _ => None,
        }
    }
}

#[cfg(feature = "clap")]
impl From<ClapError> for Error {
    fn from(error: ClapError) -> Self {
        Self::Clap(error)
    }
}

#[cfg(feature = "vst3")]
impl From<Vst3Error> for Error {
    fn from(error: Vst3Error) -> Self {
        Self::Vst3(error)
    }
}

#[cfg(feature = "lv2")]
impl From<Lv2Error> for Error {
    fn from(error: Lv2Error) -> Self {
        Self::Lv2(error)
    }
}

#[cfg(feature = "au")]
impl From<AuError> for Error {
    fn from(error: AuError) -> Self {
        Self::Au(error)
    }
}

/// What a library says about one of its plugins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEntry {
    /// The plugin's format.
    pub format: Format,
    /// What [`Library::create`] takes and what a session stores: a CLAP
    /// plugin's identifier, a VST 3 class identifier as 32 hexadecimal
    /// digits, written in upper case, an LV2 plugin's URI, or an Audio
    /// Unit's three codes as [`ComponentCodes`](crate::au::ComponentCodes)
    /// writes them, such as `aufx:dcmp:appl`.
    pub id: String,
    /// The name shown to people.
    pub name: String,
    /// Who makes it.
    pub vendor: String,
    /// Its version, as text.
    pub version: String,
    /// What kind of plugin it is.
    pub kind: Kind,
    /// The words the library files it under: CLAP's features, VST 3's
    /// sub-categories, the names of LV2's classes, such as
    /// `InstrumentPlugin`, or an Audio Unit's tags, or its type's code.
    pub categories: Vec<String>,
}

/// A loaded plugin file or bundle of any format, and the plugins in it.
///
/// The handle can be cloned and sent between threads.
#[derive(Debug, Clone)]
pub struct Library {
    inner: LibraryInner,
    plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone)]
enum LibraryInner {
    #[cfg(feature = "clap")]
    Clap(ClapLibrary),
    #[cfg(feature = "vst3")]
    Vst3(Vst3Module),
    #[cfg(feature = "lv2")]
    Lv2(Lv2Library),
    #[cfg(feature = "au")]
    Au(AuLibrary),
}

/// The format a path's name says it is in, if it says.
fn format_by_name(path: &Path) -> Option<Format> {
    let extension = path.extension()?.to_str()?;
    if extension.eq_ignore_ascii_case("clap") {
        Some(Format::Clap)
    } else if extension.eq_ignore_ascii_case("vst3") {
        Some(Format::Vst3)
    } else if extension.eq_ignore_ascii_case("lv2") {
        Some(Format::Lv2)
    } else if extension.eq_ignore_ascii_case("component") {
        Some(Format::Au)
    } else {
        None
    }
}

impl Library {
    /// Loads a plugin file or bundle, in whichever format it is.
    ///
    /// The name decides: a path ending in `.clap` is loaded as CLAP, one
    /// ending in `.vst3` as VST 3, file or bundle folder alike, and one
    /// ending in `.lv2` as an LV2 bundle, and one ending in `.component` as
    /// an Audio Unit bundle, whose units are made through AudioToolbox (on
    /// macOS only). A folder with another name that
    /// holds a `manifest.ttl` is an LV2 bundle too. A library with another
    /// name is tried as CLAP, and then, if it has no CLAP entry point, as
    /// VST 3. A file that exports both is taken as CLAP; use
    /// [`load_as`](Self::load_as) to choose.
    ///
    /// # Errors
    ///
    /// The error of the format's own loader, [`Error::UnknownFormat`] if a
    /// library is a plugin of neither format, and [`Error::NotBuiltIn`] if
    /// the name asks for a format this crate was built without.
    ///
    /// # Safety
    ///
    /// The terms of [`ClapLibrary::load`], [`Vst3Module::load`],
    /// [`Lv2Library::load`] and [`AuLibrary::load`]: the file's code is
    /// run, and the caller vouches for it.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        if let Some(format) = format_by_name(path) {
            // SAFETY: passed on from the caller.
            return unsafe { Self::load_as(path, format) };
        }
        if path.join("manifest.ttl").is_file() {
            // SAFETY: passed on from the caller.
            return unsafe { Self::load_as(path, Format::Lv2) };
        }
        #[cfg(feature = "clap")]
        {
            // SAFETY: passed on from the caller.
            match unsafe { ClapLibrary::load(path) } {
                Ok(library) => return Ok(library.into()),
                Err(ClapError::NoEntryPoint(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        #[cfg(feature = "vst3")]
        {
            // SAFETY: passed on from the caller.
            match unsafe { Vst3Module::load(path) } {
                Ok(module) => return Ok(module.into()),
                Err(Vst3Error::NoEntryPoint(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(Error::UnknownFormat(path.to_owned()))
    }

    /// Loads a plugin file as the format given, whatever its name.
    ///
    /// # Errors
    ///
    /// The error of the format's own loader, or [`Error::NotBuiltIn`].
    ///
    /// # Safety
    ///
    /// As for [`load`](Self::load).
    pub unsafe fn load_as(path: impl AsRef<Path>, format: Format) -> Result<Self, Error> {
        let path = path.as_ref();
        match format {
            #[cfg(feature = "clap")]
            // SAFETY: passed on from the caller.
            Format::Clap => Ok(unsafe { ClapLibrary::load(path) }?.into()),
            #[cfg(feature = "vst3")]
            // SAFETY: passed on from the caller.
            Format::Vst3 => Ok(unsafe { Vst3Module::load(path) }?.into()),
            #[cfg(feature = "lv2")]
            // SAFETY: passed on from the caller.
            Format::Lv2 => Ok(unsafe { Lv2Library::load(path) }?.into()),
            #[cfg(feature = "au")]
            // SAFETY: passed on from the caller.
            Format::Au => Ok(unsafe { AuLibrary::load(path) }?.into()),
            #[allow(unreachable_patterns)]
            other => Err(Error::NotBuiltIn(other)),
        }
    }

    /// The library's format.
    pub fn format(&self) -> Format {
        match self.inner {
            #[cfg(feature = "clap")]
            LibraryInner::Clap(_) => Format::Clap,
            #[cfg(feature = "vst3")]
            LibraryInner::Vst3(_) => Format::Vst3,
            #[cfg(feature = "lv2")]
            LibraryInner::Lv2(_) => Format::Lv2,
            #[cfg(feature = "au")]
            LibraryInner::Au(_) => Format::Au,
        }
    }

    /// The path the library was loaded from, as it was given.
    pub fn path(&self) -> &Path {
        match &self.inner {
            #[cfg(feature = "clap")]
            LibraryInner::Clap(library) => library.path(),
            #[cfg(feature = "vst3")]
            LibraryInner::Vst3(module) => module.path(),
            #[cfg(feature = "lv2")]
            LibraryInner::Lv2(library) => library.path(),
            #[cfg(feature = "au")]
            LibraryInner::Au(library) => library.path(),
        }
    }

    /// The plugins in the library: for VST 3, the classes that are audio
    /// plugins, without their controllers.
    pub fn plugins(&self) -> &[PluginEntry] {
        &self.plugins
    }

    /// Creates the plugin with this identifier, as
    /// [`PluginEntry::id`] gives it.
    ///
    /// A CLAP identifier and an LV2 URI are matched exactly. A VST 3
    /// identifier is read as the class identifier it is, so its
    /// hexadecimal digits may be in either case, as `vst3::ClassId` reads
    /// them. An Audio Unit's codes are read as the codes they are.
    ///
    /// The calling thread becomes the plugin's main thread.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownPlugin`] if the library has no such plugin, and
    /// otherwise the error of the format's own host.
    pub fn create(&self, id: &str) -> Result<Plugin, Error> {
        let unknown = || Error::UnknownPlugin(id.to_owned());
        let find = |wanted: &str| {
            self.plugins
                .iter()
                .find(|entry| entry.id == wanted)
                .cloned()
                .ok_or_else(unknown)
        };
        match &self.inner {
            #[cfg(feature = "clap")]
            LibraryInner::Clap(library) => {
                let entry = find(id)?;
                let inner = PluginInner::Clap(Box::new(library.create(id)?));
                Ok(Plugin { inner, entry })
            }
            #[cfg(feature = "vst3")]
            LibraryInner::Vst3(module) => {
                let class: ClassId = id.parse().map_err(|_| unknown())?;
                // The entries hold the identifier as `ClassId` writes it.
                let entry = find(&class.to_string())?;
                let inner = PluginInner::Vst3(Box::new(module.create(class)?));
                Ok(Plugin { inner, entry })
            }
            #[cfg(feature = "lv2")]
            LibraryInner::Lv2(library) => {
                let entry = find(id)?;
                let inner = PluginInner::Lv2(Box::new(library.create(id)?));
                Ok(Plugin { inner, entry })
            }
            #[cfg(feature = "au")]
            LibraryInner::Au(library) => {
                let codes: crate::au::ComponentCodes = id.parse().map_err(|_| unknown())?;
                let entry = find(&codes.to_string())?;
                let inner = PluginInner::Au(Box::new(library.create(codes)?));
                Ok(Plugin { inner, entry })
            }
        }
    }

    /// The CLAP library, if this is one.
    #[cfg(feature = "clap")]
    pub fn as_clap(&self) -> Option<&ClapLibrary> {
        match &self.inner {
            LibraryInner::Clap(library) => Some(library),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The VST 3 module, if this is one.
    #[cfg(feature = "vst3")]
    pub fn as_vst3(&self) -> Option<&Vst3Module> {
        match &self.inner {
            LibraryInner::Vst3(module) => Some(module),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The LV2 bundle, if this is one.
    #[cfg(feature = "lv2")]
    pub fn as_lv2(&self) -> Option<&Lv2Library> {
        match &self.inner {
            LibraryInner::Lv2(library) => Some(library),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The Audio Unit components, if this is them.
    #[cfg(feature = "au")]
    pub fn as_au(&self) -> Option<&AuLibrary> {
        match &self.inner {
            LibraryInner::Au(library) => Some(library),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

#[cfg(feature = "au")]
impl From<AuLibrary> for Library {
    fn from(library: AuLibrary) -> Self {
        let plugins = library
            .components()
            .iter()
            .map(crate::au::AuComponentInfo::entry)
            .collect();
        Self {
            inner: LibraryInner::Au(library),
            plugins,
        }
    }
}

#[cfg(feature = "clap")]
impl From<ClapLibrary> for Library {
    fn from(library: ClapLibrary) -> Self {
        let plugins = library
            .descriptors()
            .iter()
            .map(|plugin| {
                let kind = if plugin.has_feature("instrument") {
                    Kind::Instrument
                } else if plugin.has_feature("note-effect") {
                    Kind::NoteEffect
                } else if plugin.has_feature("analyzer") {
                    Kind::Analyzer
                } else if plugin.has_feature("audio-effect") {
                    Kind::Effect
                } else {
                    Kind::Other
                };
                PluginEntry {
                    format: Format::Clap,
                    id: plugin.id.clone(),
                    name: plugin.name.clone(),
                    vendor: plugin.vendor.clone(),
                    version: plugin.version.clone(),
                    kind,
                    categories: plugin.features.clone(),
                }
            })
            .collect();
        Self {
            inner: LibraryInner::Clap(library),
            plugins,
        }
    }
}

#[cfg(feature = "vst3")]
impl From<Vst3Module> for Library {
    fn from(module: Vst3Module) -> Self {
        let plugins = module
            .classes()
            .iter()
            .filter(|class| class.is_audio_module())
            .map(|class| {
                let has = |word: &str| class.sub_categories.iter().any(|given| given == word);
                let kind = if has("Instrument") {
                    Kind::Instrument
                } else if has("Analyzer") {
                    Kind::Analyzer
                } else if has("Fx") {
                    Kind::Effect
                } else {
                    Kind::Other
                };
                PluginEntry {
                    format: Format::Vst3,
                    id: class.id.to_string(),
                    name: class.name.clone(),
                    vendor: if class.vendor.is_empty() {
                        module.factory_info().vendor.clone()
                    } else {
                        class.vendor.clone()
                    },
                    version: class.version.clone(),
                    kind,
                    categories: class.sub_categories.clone(),
                }
            })
            .collect();
        Self {
            inner: LibraryInner::Vst3(module),
            plugins,
        }
    }
}

#[cfg(feature = "lv2")]
impl From<Lv2Library> for Library {
    fn from(library: Lv2Library) -> Self {
        let plugins = library.plugins().iter().map(lv2_entry).collect();
        Self {
            inner: LibraryInner::Lv2(library),
            plugins,
        }
    }
}

/// What an LV2 bundle says about one of its plugins, as an entry.
#[cfg(feature = "lv2")]
pub(crate) fn lv2_entry(info: &crate::lv2::Lv2PluginInfo) -> PluginEntry {
    use crate::lv2::{Direction, PortKind};
    let audio = |direction| {
        info.ports
            .iter()
            .any(|port| port.kind == PortKind::Audio && port.direction == direction)
    };
    let kind = if info.is_a("InstrumentPlugin") {
        Kind::Instrument
    } else if info.is_a("AnalyserPlugin") {
        Kind::Analyzer
    } else if info.is_a("MIDIPlugin") {
        Kind::NoteEffect
    } else if audio(Direction::Input) && audio(Direction::Output) {
        Kind::Effect
    } else {
        Kind::Other
    };
    PluginEntry {
        format: Format::Lv2,
        id: info.uri.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        kind,
        categories: info
            .classes
            .iter()
            .map(|class| class.rsplit(['#', '/']).next().unwrap_or(class).to_owned())
            .collect(),
    }
}

/// What a plugin says about one of its parameters, the same way for every
/// format.
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    /// The number that stands for the parameter.
    pub id: ParamId,
    /// The name shown to people.
    pub name: String,
    /// The unit of the value as the plugin shows it, if the format says.
    pub unit: String,
    /// The lowest value in the plugin's automation scale: the declared
    /// minimum for CLAP and LV2, 0 for VST 3.
    pub min: f64,
    /// The highest value: the declared maximum for CLAP and LV2, 1 for
    /// VST 3.
    pub max: f64,
    /// The value the parameter starts at.
    pub default: f64,
    /// How many steps the value moves in, or `None` for a continuous one.
    pub steps: Option<u32>,
    /// May be automated.
    pub automatable: bool,
    /// Not to be shown to people.
    pub hidden: bool,
    /// The host may read it but not change it.
    pub read_only: bool,
    /// This is the plugin's bypass switch.
    pub bypass: bool,
}

/// What a plugin says about one of its audio ports or buses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bus {
    /// The name shown to people.
    pub name: String,
    /// How many channels it has.
    pub channels: u32,
    /// Whether it is the main one of its direction.
    pub is_main: bool,
}

/// Why a block came out silent, the same way for every format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Failure {
    /// The plugin reported an error while processing.
    Error,
    /// The plugin refused to start processing.
    StartRefused,
    /// The audio half was prepared for another sample rate than the one
    /// the plugin was activated with.
    SampleRateMismatch,
}

/// Something a plugin asked for or reported, as returned by
/// [`Plugin::pump`]: what every format can say.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Notice {
    /// The plugin asks to be deactivated and activated again.
    RestartRequested,
    /// The plugin changed a parameter by itself, to a value in its
    /// automation scale.
    ParamValue {
        /// The parameter.
        param: ParamId,
        /// Its new value.
        value: f64,
    },
    /// A control in the plugin's interface was taken hold of.
    GestureBegin {
        /// The parameter.
        param: ParamId,
    },
    /// The control was let go.
    GestureEnd {
        /// The parameter.
        param: ParamId,
    },
    /// The list of parameters, or their values, changed: read
    /// [`Plugin::params`] again.
    ParamsChanged,
    /// The plugin's state differs from what was last saved or loaded.
    StateDirty,
    /// The plugin's latency is now this many frames.
    LatencyChanged(u32),
    /// At least one block came out silent since the last call.
    ProcessFailed(Failure),
    /// A message from the plugin, or from the host about the plugin.
    Message(String),
}

/// A plugin of any format, as the main thread deals with it.
///
/// Like the formats' own types, it cannot leave the thread that created
/// it, and its audio half, from [`activate`](Self::activate), is the only
/// thing that processes.
pub struct Plugin {
    inner: PluginInner,
    entry: PluginEntry,
}

enum PluginInner {
    #[cfg(feature = "clap")]
    Clap(Box<ClapPlugin>),
    #[cfg(feature = "vst3")]
    Vst3(Box<Vst3Plugin>),
    #[cfg(feature = "lv2")]
    Lv2(Box<Lv2Plugin>),
    #[cfg(feature = "au")]
    Au(Box<AuPlugin>),
}

/// Runs the same expression on whichever plugin `$inner` holds.
macro_rules! either {
    ($inner:expr, $plugin:ident => $work:expr) => {
        match $inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap($plugin) => $work,
            #[cfg(feature = "vst3")]
            PluginInner::Vst3($plugin) => $work,
            #[cfg(feature = "lv2")]
            PluginInner::Lv2($plugin) => $work,
            #[cfg(feature = "au")]
            PluginInner::Au($plugin) => $work,
        }
    };
}

impl Plugin {
    /// What the library said about the plugin.
    pub fn entry(&self) -> &PluginEntry {
        &self.entry
    }

    /// The plugin's format.
    pub fn format(&self) -> Format {
        self.entry.format
    }

    /// The plugin's parameters, in the plugin's order, as they are now.
    pub fn params(&self) -> Vec<Param> {
        match &self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => plugin
                .params()
                .iter()
                .map(|param| Param {
                    id: param.id,
                    name: param.name.clone(),
                    unit: String::new(),
                    min: param.min,
                    max: param.max,
                    default: param.default,
                    steps: param.flags.stepped.then(|| {
                        (param.max - param.min)
                            .round()
                            .clamp(0.0, f64::from(u32::MAX)) as u32
                    }),
                    automatable: param.flags.automatable,
                    hidden: param.flags.hidden,
                    read_only: param.flags.read_only,
                    bypass: param.flags.bypass,
                })
                .collect(),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => plugin
                .params()
                .iter()
                .map(|param| Param {
                    id: param.id,
                    name: param.title.clone(),
                    unit: param.units.clone(),
                    min: 0.0,
                    max: 1.0,
                    default: param.default,
                    steps: (param.step_count > 0).then_some(param.step_count),
                    automatable: param.flags.automatable,
                    hidden: param.flags.hidden,
                    read_only: param.flags.read_only,
                    bypass: param.flags.bypass,
                })
                .collect(),
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => plugin
                .params()
                .iter()
                .map(|param| Param {
                    id: param.id,
                    name: param.name.clone(),
                    unit: param.unit.clone(),
                    min: param.min,
                    max: param.max,
                    default: param.default,
                    steps: param.steps,
                    automatable: !param.properties.not_automatic,
                    hidden: param.properties.not_on_gui,
                    read_only: false,
                    bypass: false,
                })
                .collect(),
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => plugin
                .params()
                .iter()
                .map(|param| Param {
                    id: param.id,
                    name: param.name.clone(),
                    unit: param.unit.clone(),
                    min: param.min,
                    max: param.max,
                    default: param.default,
                    steps: param.steps,
                    automatable: param.is_automatable(),
                    hidden: false,
                    read_only: !param.is_writable(),
                    bypass: false,
                })
                .collect(),
        }
    }

    /// The current value of a parameter, in its automation scale.
    pub fn param_value(&self, id: ParamId) -> Option<f64> {
        either!(&self.inner, plugin => plugin.param_value(id))
    }

    /// Sets a parameter, in its automation scale. While an audio half
    /// exists, the plugin hears it on the first frame of the next block.
    ///
    /// # Errors
    ///
    /// The error of the format's own host: an unknown parameter, a value
    /// that is not a number, a full queue.
    pub fn set_param(&mut self, id: ParamId, value: f64) -> Result<(), Error> {
        either!(&mut self.inner, plugin => Ok(plugin.set_param(id, value)?))
    }

    /// The text a person should see for a value of a parameter, as the
    /// plugin words it.
    pub fn param_to_text(&self, id: ParamId, value: f64) -> Option<String> {
        either!(&self.inner, plugin => plugin.param_to_text(id, value))
    }

    /// The value a piece of text stands for, as the plugin reads it.
    pub fn text_to_param(&self, id: ParamId, text: &str) -> Option<f64> {
        either!(&self.inner, plugin => plugin.text_to_param(id, text))
    }

    /// The plugin's audio inputs, main one first.
    pub fn audio_inputs(&self) -> Vec<Bus> {
        match &self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => clap_buses(plugin.audio_inputs()),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => vst3_buses(plugin.audio_inputs()),
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => {
                let (main, sidechain, _) = plugin.audio_channels();
                lv2_buses(&[("Main", main, true), ("Sidechain", sidechain, false)])
            }
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => au_buses(plugin.audio_inputs()),
        }
    }

    /// The plugin's audio outputs, main one first.
    pub fn audio_outputs(&self) -> Vec<Bus> {
        match &self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => clap_buses(plugin.audio_outputs()),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => vst3_buses(plugin.audio_outputs()),
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => lv2_buses(&[("Main", plugin.audio_channels().2, true)]),
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => au_buses(plugin.audio_outputs()),
        }
    }

    /// Whether the plugin takes notes.
    pub fn takes_notes(&self) -> bool {
        match &self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => !plugin.note_inputs().is_empty(),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => !plugin.event_inputs().is_empty(),
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => plugin.takes_notes(),
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => plugin.takes_notes(),
        }
    }

    /// How many frames late the plugin's output is, as it reported when it
    /// was last activated.
    pub fn latency(&self) -> u32 {
        either!(&self.inner, plugin => plugin.latency())
    }

    /// Everything the plugin needs to come back as it is now, as bytes for
    /// [`load_state`](Self::load_state). The bytes are the format's: the
    /// plugin's own for CLAP, component and controller state together for
    /// VST 3, control values and the plugin's own values for LV2, the
    /// property list of `ClassInfo`, as XML, for an Audio Unit.
    ///
    /// # Errors
    ///
    /// The error of the format's own host.
    pub fn save_state(&mut self) -> Result<Vec<u8>, Error> {
        either!(&mut self.inner, plugin => Ok(plugin.save_state()?))
    }

    /// Gives the plugin back a state saved by [`save_state`](Self::save_state).
    ///
    /// # Errors
    ///
    /// The error of the format's own host.
    pub fn load_state(&mut self, state: &[u8]) -> Result<(), Error> {
        either!(&mut self.inner, plugin => Ok(plugin.load_state(state)?))
    }

    /// Whether the plugin is active.
    pub fn is_active(&self) -> bool {
        either!(&self.inner, plugin => plugin.is_active())
    }

    /// Whether the audio half made by the last activation still exists.
    pub fn has_processor(&self) -> bool {
        either!(&self.inner, plugin => plugin.has_processor())
    }

    /// Activates the plugin and returns its audio half, boxed, whichever
    /// the format.
    ///
    /// # Errors
    ///
    /// The error of the format's own host.
    pub fn activate(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<Box<dyn Processor<f32> + Send>, Error> {
        either!(&mut self.inner, plugin => {
            let processor = plugin.activate(sample_rate, max_block_size)?;
            Ok(Box::new(processor) as Box<dyn Processor<f32> + Send>)
        })
    }

    /// Activates the plugin and returns its audio half as an
    /// [`AnyProcessor`], which processes as the box from
    /// [`activate`](Self::activate) does and also reads what the plugin
    /// sends, and reaches the format's own processor.
    ///
    /// # Errors
    ///
    /// The error of the format's own host.
    pub fn activate_processor(
        &mut self,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<AnyProcessor, Error> {
        let inner = match &mut self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => {
                ProcessorInner::Clap(Box::new(plugin.activate(sample_rate, max_block_size)?))
            }
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => {
                ProcessorInner::Vst3(Box::new(plugin.activate(sample_rate, max_block_size)?))
            }
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => {
                ProcessorInner::Lv2(Box::new(plugin.activate(sample_rate, max_block_size)?))
            }
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => {
                ProcessorInner::Au(Box::new(plugin.activate(sample_rate, max_block_size)?))
            }
        };
        Ok(AnyProcessor { inner })
    }

    /// Deactivates the plugin, once its audio half has been dropped.
    ///
    /// # Errors
    ///
    /// The error of the format's own host, notably that the audio half
    /// still exists.
    pub fn deactivate(&mut self) -> Result<(), Error> {
        either!(&mut self.inner, plugin => Ok(plugin.deactivate()?))
    }

    /// How many blocks have come out silent because the plugin failed.
    pub fn failed_blocks(&self) -> u64 {
        either!(&self.inner, plugin => plugin.failed_blocks())
    }

    /// How many events have been dropped for want of room.
    pub fn lost_events(&self) -> u64 {
        either!(&self.inner, plugin => plugin.lost_events())
    }

    /// How many events have not been sent because the plugin has no way to
    /// receive them.
    pub fn undeliverable_events(&self) -> u64 {
        either!(&self.inner, plugin => plugin.undeliverable_events())
    }

    /// Answers what the plugin asked for, and returns what both formats can
    /// say about it. What only one format can say is turned into a
    /// [`Notice::Message`], or left out when it has been dealt with; the
    /// format's own `pump`, through [`as_clap_mut`](Self::as_clap_mut) or
    /// [`as_vst3_mut`](Self::as_vst3_mut), gives everything.
    pub fn pump(&mut self) -> Vec<Notice> {
        match &mut self.inner {
            #[cfg(feature = "clap")]
            PluginInner::Clap(plugin) => plugin.pump().into_iter().filter_map(from_clap).collect(),
            #[cfg(feature = "vst3")]
            PluginInner::Vst3(plugin) => plugin.pump().into_iter().filter_map(from_vst3).collect(),
            #[cfg(feature = "lv2")]
            PluginInner::Lv2(plugin) => plugin.pump().into_iter().filter_map(from_lv2).collect(),
            #[cfg(feature = "au")]
            PluginInner::Au(plugin) => plugin.pump().into_iter().filter_map(from_au).collect(),
        }
    }

    /// The CLAP plugin, if this is one.
    #[cfg(feature = "clap")]
    pub fn as_clap(&self) -> Option<&ClapPlugin> {
        match &self.inner {
            PluginInner::Clap(plugin) => Some(&**plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The CLAP plugin, if this is one, to change.
    #[cfg(feature = "clap")]
    pub fn as_clap_mut(&mut self) -> Option<&mut ClapPlugin> {
        match &mut self.inner {
            PluginInner::Clap(plugin) => Some(&mut **plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The VST 3 plugin, if this is one.
    #[cfg(feature = "vst3")]
    pub fn as_vst3(&self) -> Option<&Vst3Plugin> {
        match &self.inner {
            PluginInner::Vst3(plugin) => Some(&**plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The VST 3 plugin, if this is one, to change.
    #[cfg(feature = "vst3")]
    pub fn as_vst3_mut(&mut self) -> Option<&mut Vst3Plugin> {
        match &mut self.inner {
            PluginInner::Vst3(plugin) => Some(&mut **plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The LV2 plugin, if this is one.
    #[cfg(feature = "lv2")]
    pub fn as_lv2(&self) -> Option<&Lv2Plugin> {
        match &self.inner {
            PluginInner::Lv2(plugin) => Some(&**plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The LV2 plugin, if this is one, to change.
    #[cfg(feature = "lv2")]
    pub fn as_lv2_mut(&mut self) -> Option<&mut Lv2Plugin> {
        match &mut self.inner {
            PluginInner::Lv2(plugin) => Some(&mut **plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The Audio Unit, if this is one.
    #[cfg(feature = "au")]
    pub fn as_au(&self) -> Option<&AuPlugin> {
        match &self.inner {
            PluginInner::Au(plugin) => Some(&**plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The Audio Unit, if this is one, to change.
    #[cfg(feature = "au")]
    pub fn as_au_mut(&mut self) -> Option<&mut AuPlugin> {
        match &mut self.inner {
            PluginInner::Au(plugin) => Some(&mut **plugin),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl fmt::Debug for Plugin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Plugin")
            .field("format", &self.entry.format)
            .field("id", &self.entry.id)
            .field("name", &self.entry.name)
            .finish_non_exhaustive()
    }
}

/// A plugin of any format, as the audio thread deals with it: a
/// [`Processor`], made by [`Plugin::activate_processor`].
///
/// It processes exactly as the format's own processor does, and so as the
/// box [`Plugin::activate`] returns; the format's processor says how a
/// block is laid out and what is real-time safe. What it adds is
/// [`output_events`](Self::output_events), the events the plugin sent, in
/// one type whatever the format, and a way to the format's processor for
/// the rest.
///
/// ```
/// fn is_a_processor<T: manguebyte_dsp::Processor<f32> + Send>() {}
/// is_a_processor::<manguebyte_host::any::AnyProcessor>();
/// ```
pub struct AnyProcessor {
    inner: ProcessorInner,
}

enum ProcessorInner {
    #[cfg(feature = "clap")]
    Clap(Box<ClapProcessor>),
    #[cfg(feature = "vst3")]
    Vst3(Box<Vst3Processor>),
    #[cfg(feature = "lv2")]
    Lv2(Box<Lv2Processor>),
    #[cfg(feature = "au")]
    Au(Box<AuProcessor>),
}

/// Runs the same expression on whichever processor `$inner` holds.
macro_rules! each_processor {
    ($inner:expr, $processor:ident => $work:expr) => {
        match $inner {
            #[cfg(feature = "clap")]
            ProcessorInner::Clap($processor) => $work,
            #[cfg(feature = "vst3")]
            ProcessorInner::Vst3($processor) => $work,
            #[cfg(feature = "lv2")]
            ProcessorInner::Lv2($processor) => $work,
            #[cfg(feature = "au")]
            ProcessorInner::Au($processor) => $work,
        }
    };
}

impl AnyProcessor {
    /// The plugin's format.
    pub fn format(&self) -> Format {
        match &self.inner {
            #[cfg(feature = "clap")]
            ProcessorInner::Clap(_) => Format::Clap,
            #[cfg(feature = "vst3")]
            ProcessorInner::Vst3(_) => Format::Vst3,
            #[cfg(feature = "lv2")]
            ProcessorInner::Lv2(_) => Format::Lv2,
            #[cfg(feature = "au")]
            ProcessorInner::Au(_) => Format::Au,
        }
    }

    /// What the plugin sent during the last call to
    /// [`process_with`](Processor::process_with), on the frames of the
    /// block it was sent on, whatever the format. Real-time safe: nothing
    /// is converted or allocated here.
    ///
    /// Each format says what it can:
    ///
    /// - CLAP: notes with their ids, note expression, MIDI, and the values
    ///   the plugin gave its parameters, in the CLAP scale; the kinds of
    ///   [`OutputKind::Event`](crate::clap::OutputKind::Event), in the
    ///   order of [`ClapProcessor::output_events`], which also has what
    ///   the framework's events cannot say, such as gestures and the end
    ///   of a note.
    /// - VST 3: what [`Vst3Processor::output_events`] has, parameter values
    ///   normalized.
    /// - LV2 and Audio Units: the MIDI 1.0 the plugin sent, as
    ///   [`Lv2Processor::output_events`] and
    ///   [`AuProcessor::output_events`] read it: notes with 7-bit
    ///   velocities and no ids, pressure on one key as a note expression,
    ///   and other messages as
    ///   [`EventKind::Midi`](manguebyte_dsp::event::EventKind::Midi).
    ///
    /// A parameter value is in the format's scale, as everywhere in this
    /// module (see [Values](self#values)). So a note effect sends the same
    /// notes on the same frames in every format, up to what each format
    /// carries: the velocity's precision and the note's id.
    pub fn output_events(&self) -> &[Event] {
        match &self.inner {
            #[cfg(feature = "clap")]
            ProcessorInner::Clap(processor) => processor.framework_events(),
            #[cfg(feature = "vst3")]
            ProcessorInner::Vst3(processor) => processor.output_events(),
            #[cfg(feature = "lv2")]
            ProcessorInner::Lv2(processor) => processor.output_events(),
            #[cfg(feature = "au")]
            ProcessorInner::Au(processor) => processor.output_events(),
        }
    }

    /// The sample rate the plugin runs at.
    pub fn sample_rate(&self) -> f64 {
        each_processor!(&self.inner, processor => processor.sample_rate())
    }

    /// The longest piece of a block the plugin is handed in one call.
    /// Longer blocks are processed in pieces.
    pub fn max_block_size(&self) -> usize {
        each_processor!(&self.inner, processor => processor.max_block_size())
    }

    /// The CLAP processor, if this is one.
    #[cfg(feature = "clap")]
    pub fn as_clap(&self) -> Option<&ClapProcessor> {
        match &self.inner {
            ProcessorInner::Clap(processor) => Some(&**processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The CLAP processor, if this is one, to change, such as to
    /// [`flush`](ClapProcessor::flush) it.
    #[cfg(feature = "clap")]
    pub fn as_clap_mut(&mut self) -> Option<&mut ClapProcessor> {
        match &mut self.inner {
            ProcessorInner::Clap(processor) => Some(&mut **processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The VST 3 processor, if this is one.
    #[cfg(feature = "vst3")]
    pub fn as_vst3(&self) -> Option<&Vst3Processor> {
        match &self.inner {
            ProcessorInner::Vst3(processor) => Some(&**processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The VST 3 processor, if this is one, to change.
    #[cfg(feature = "vst3")]
    pub fn as_vst3_mut(&mut self) -> Option<&mut Vst3Processor> {
        match &mut self.inner {
            ProcessorInner::Vst3(processor) => Some(&mut **processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The LV2 processor, if this is one.
    #[cfg(feature = "lv2")]
    pub fn as_lv2(&self) -> Option<&Lv2Processor> {
        match &self.inner {
            ProcessorInner::Lv2(processor) => Some(&**processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The LV2 processor, if this is one, to change.
    #[cfg(feature = "lv2")]
    pub fn as_lv2_mut(&mut self) -> Option<&mut Lv2Processor> {
        match &mut self.inner {
            ProcessorInner::Lv2(processor) => Some(&mut **processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The Audio Unit's processor, if this is one.
    #[cfg(feature = "au")]
    pub fn as_au(&self) -> Option<&AuProcessor> {
        match &self.inner {
            ProcessorInner::Au(processor) => Some(&**processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The Audio Unit's processor, if this is one, to change.
    #[cfg(feature = "au")]
    pub fn as_au_mut(&mut self) -> Option<&mut AuProcessor> {
        match &mut self.inner {
            ProcessorInner::Au(processor) => Some(&mut **processor),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }
}

impl Processor<f32> for AnyProcessor {
    fn prepare(&mut self, spec: &ProcessSpec) {
        each_processor!(&mut self.inner, processor => processor.prepare(spec));
    }

    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        each_processor!(&mut self.inner, processor => processor.process(buffer));
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        each_processor!(&mut self.inner, processor => processor.process_with(buffer, context));
    }

    fn reset(&mut self) {
        each_processor!(&mut self.inner, processor => processor.reset());
    }

    fn latency_samples(&self) -> usize {
        each_processor!(&self.inner, processor => processor.latency_samples())
    }
}

impl fmt::Debug for AnyProcessor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AnyProcessor")
            .field("format", &self.format())
            .field("sample_rate", &self.sample_rate())
            .field("max_block_size", &self.max_block_size())
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "clap")]
fn clap_buses(ports: &[crate::clap::AudioPortInfo]) -> Vec<Bus> {
    ports
        .iter()
        .map(|port| Bus {
            name: port.name.clone(),
            channels: port.channels,
            is_main: port.is_main,
        })
        .collect()
}

#[cfg(feature = "vst3")]
fn vst3_buses(buses: &[crate::vst3::BusInfo]) -> Vec<Bus> {
    buses
        .iter()
        .map(|bus| Bus {
            name: bus.name.clone(),
            channels: bus.channels,
            is_main: bus.is_main,
        })
        .collect()
}

#[cfg(feature = "lv2")]
fn lv2_buses(buses: &[(&str, usize, bool)]) -> Vec<Bus> {
    buses
        .iter()
        .filter(|(_, channels, _)| *channels > 0)
        .map(|&(name, channels, is_main)| Bus {
            name: name.to_owned(),
            channels: channels as u32,
            is_main,
        })
        .collect()
}

#[cfg(feature = "au")]
fn au_buses(buses: &[crate::au::AuBus]) -> Vec<Bus> {
    buses
        .iter()
        .map(|bus| Bus {
            name: bus.name.clone(),
            channels: bus.channels,
            is_main: bus.element == 0,
        })
        .collect()
}

#[cfg(feature = "au")]
fn from_au(event: crate::au::AuEvent) -> Option<Notice> {
    use crate::au::{AuEvent, AuFailure};
    Some(match event {
        AuEvent::LatencyChanged(frames) => Notice::LatencyChanged(frames),
        AuEvent::ProcessFailed(AuFailure::SampleRateMismatch) => {
            Notice::ProcessFailed(Failure::SampleRateMismatch)
        }
        AuEvent::ProcessFailed(_) => Notice::ProcessFailed(Failure::Error),
        AuEvent::Remark(message) => Notice::Message(message),
    })
}

#[cfg(feature = "lv2")]
fn from_lv2(event: crate::lv2::Lv2Event) -> Option<Notice> {
    use crate::lv2::{Lv2Event, Lv2Failure};
    Some(match event {
        Lv2Event::LatencyChanged(frames) => Notice::LatencyChanged(frames),
        Lv2Event::ProcessFailed(Lv2Failure::SampleRateMismatch) => {
            Notice::ProcessFailed(Failure::SampleRateMismatch)
        }
        Lv2Event::Log { level, message } => Notice::Message(format!("{level:?}: {message}")),
        Lv2Event::Remark(message) => Notice::Message(message),
    })
}

#[cfg(feature = "clap")]
fn from_clap(event: crate::clap::HostEvent) -> Option<Notice> {
    use crate::clap::{HostEvent, ProcessFailure};
    Some(match event {
        HostEvent::RestartRequested => Notice::RestartRequested,
        HostEvent::ParamValue { param, value } => Notice::ParamValue { param, value },
        HostEvent::GestureBegin { param } => Notice::GestureBegin { param },
        HostEvent::GestureEnd { param } => Notice::GestureEnd { param },
        HostEvent::ParamsRescanned(_) => Notice::ParamsChanged,
        HostEvent::StateDirty => Notice::StateDirty,
        HostEvent::LatencyChanged(frames) => Notice::LatencyChanged(frames),
        HostEvent::ProcessFailed(failure) => Notice::ProcessFailed(match failure {
            ProcessFailure::Error => Failure::Error,
            ProcessFailure::StartRefused => Failure::StartRefused,
            ProcessFailure::SampleRateMismatch => Failure::SampleRateMismatch,
        }),
        HostEvent::Log { severity, message } => Notice::Message(format!("{severity:?}: {message}")),
        // Requests the CLAP host has answered, or that only a CLAP host
        // acts on.
        _ => return None,
    })
}

#[cfg(feature = "vst3")]
fn from_vst3(event: crate::vst3::HostEvent) -> Option<Notice> {
    use crate::vst3::{HostEvent, ProcessFailure};
    Some(match event {
        HostEvent::RestartRequested { .. } => Notice::RestartRequested,
        HostEvent::ParamValue { param, value } => Notice::ParamValue { param, value },
        HostEvent::GestureBegin { param } => Notice::GestureBegin { param },
        HostEvent::GestureEnd { param } => Notice::GestureEnd { param },
        HostEvent::ParamsRescanned { .. } => Notice::ParamsChanged,
        HostEvent::StateDirty => Notice::StateDirty,
        HostEvent::LatencyChanged(frames) => Notice::LatencyChanged(frames),
        HostEvent::ProcessFailed(failure) => Notice::ProcessFailed(match failure {
            ProcessFailure::Error => Failure::Error,
            ProcessFailure::StartRefused => Failure::StartRefused,
            ProcessFailure::SampleRateMismatch => Failure::SampleRateMismatch,
        }),
        HostEvent::Remark(message) => Notice::Message(message),
        HostEvent::EditorRequested => {
            Notice::Message("the plugin asked for its editor, which this host does not show".into())
        }
        // Only an editor opened through the VST 3 type itself asks for
        // anything, and its requests go to whoever opened it.
        HostEvent::Editor(_) => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_says_the_format() {
        assert_eq!(
            format_by_name(Path::new("a/Plugin.clap")),
            Some(Format::Clap)
        );
        assert_eq!(format_by_name(Path::new("Plugin.VST3")), Some(Format::Vst3));
        assert_eq!(format_by_name(Path::new("Plugin.lv2")), Some(Format::Lv2));
        assert_eq!(
            format_by_name(Path::new("Plugin.component")),
            Some(Format::Au)
        );
        assert_eq!(format_by_name(Path::new("libplugin.so")), None);
        assert_eq!(format_by_name(Path::new("plugin")), None);
    }

    #[test]
    fn a_path_with_nothing_at_it_is_an_error_in_every_format() {
        for path in [
            "no/such/plugin.clap",
            "no/such/plugin.vst3",
            "no/such/plugin.lv2",
            "no/such/plugin.component",
            "no/such/plugin.dll",
        ] {
            // SAFETY: there is nothing at these paths, so no code is run.
            let outcome = unsafe { Library::load(path) };
            assert!(outcome.is_err(), "{path}");
        }
    }
}
