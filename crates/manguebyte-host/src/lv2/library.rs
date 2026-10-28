//! Loading an LV2 bundle: its description and the libraries it brings.

use std::ffi::{OsString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::description::{Lv2PluginInfo, read_bundle};
use super::features::{MAX_URI_BYTES, bounded};
use super::sys::{LV2_Descriptor, LV2_Descriptor_Function};
use super::{Lv2Error, Lv2Plugin};

/// The name of the symbol every LV2 library exports.
const ENTRY_SYMBOL: &[u8] = b"lv2_descriptor\0";

/// Most descriptors read from one library while looking for a plugin.
const MAX_DESCRIPTORS: u32 = 4_096;

/// A function that does what a library's `lv2_descriptor` does: returns
/// the `LV2_Descriptor` of its plugin number `index`, or null past the
/// last, for [`Lv2Library::from_entry`].
pub type Lv2Entry = unsafe extern "C" fn(index: u32) -> *const c_void;

/// Where a library's descriptors come from.
#[derive(Clone, Copy)]
enum Entry {
    /// The `lv2_descriptor` of a loaded library.
    Loaded(LV2_Descriptor_Function),
    /// A function of this process.
    Given(Lv2Entry),
}

/// One library and its entry point.
struct Binary {
    path: PathBuf,
    entry: Entry,
    /// Kept loaded for as long as anything holds the bundle; none for an
    /// entry point of this process.
    _library: Option<libloading::Library>,
}

pub(crate) struct Inner {
    path: PathBuf,
    plugins: Vec<Lv2PluginInfo>,
    binaries: Vec<Binary>,
}

// SAFETY: the entry points belong to libraries that stay loaded for as
// long as this value exists, or are functions of this process that the
// caller of `from_entry` keeps callable. LV2's discovery functions, `lv2_descriptor`
// among them, may be called from any thread.
unsafe impl Send for Inner {}
// SAFETY: as above.
unsafe impl Sync for Inner {}

/// A loaded LV2 bundle: its plugins' descriptions and their libraries.
///
/// The handle can be cloned and sent between threads. Every plugin created
/// from it keeps the libraries loaded.
///
/// ```
/// use manguebyte_host::lv2::{Lv2Error, Lv2Library};
///
/// // A path with nothing behind it is an error, not a crash.
/// // SAFETY: there is no bundle at this path, so no code is run.
/// let missing = unsafe { Lv2Library::load("no/such/plugin.lv2") };
/// assert!(matches!(missing, Err(Lv2Error::NotFound(_))));
/// ```
#[derive(Clone)]
pub struct Lv2Library {
    pub(crate) inner: Arc<Inner>,
}

impl Lv2Library {
    /// Reads the bundle folder `path` and loads the library of each plugin
    /// it describes.
    ///
    /// The description is read as [`read_bundle`](super::read_bundle)
    /// reads it, which runs nothing. Then each library is loaded and its
    /// `lv2_descriptor` found.
    ///
    /// # Errors
    ///
    /// The errors of [`read_bundle`](super::read_bundle), then
    /// [`Lv2Error::Load`] if the system cannot load a library and
    /// [`Lv2Error::NoEntryPoint`] if one exports no `lv2_descriptor`. In
    /// every case nothing stays loaded.
    ///
    /// # Safety
    ///
    /// Loading a library runs code from it, with the full rights of this
    /// process. The caller vouches for the bundle: the code each library
    /// runs as it is loaded and unloaded must be sound to run here, and a
    /// symbol named `lv2_descriptor` must be an `LV2_Descriptor_Function`
    /// whose descriptors, and the plugins they create, follow the LV2
    /// specification. This host checks what they return, but it has to
    /// call them. Every safe function of the library and of its plugins
    /// relies on these terms.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, Lv2Error> {
        let path = path.as_ref();
        let plugins = read_bundle(path)?;
        let mut binaries: Vec<Binary> = Vec::new();
        for plugin in &plugins {
            if binaries.iter().any(|binary| binary.path == plugin.binary) {
                continue;
            }
            // SAFETY: passed on from the caller, who vouches for every
            // library of the bundle.
            binaries.push(unsafe { open(&plugin.binary) }?);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_owned(),
                plugins,
                binaries,
            }),
        })
    }

    /// Reads the bundle folder `path` and takes every plugin it describes
    /// from `entry`, a function of this process, instead of loading the
    /// library the bundle names; that library must still exist, since the
    /// bundle is read as [`read_bundle`](super::read_bundle) reads it, but
    /// nothing in it is run.
    ///
    /// This is for a plugin linked into the program, such as one a test
    /// hosts through the framework's own LV2 wrapper, as
    /// [`ClapLibrary::from_entry`](crate::clap::ClapLibrary::from_entry)
    /// and [`AuLibrary::from_factory`](crate::au::AuLibrary::from_factory)
    /// are for the other formats.
    ///
    /// # Errors
    ///
    /// The errors of [`read_bundle`](super::read_bundle).
    ///
    /// # Safety
    ///
    /// `entry` must do what an `lv2_descriptor` does, for the plugins of
    /// this bundle: return null or a descriptor whose functions, and the
    /// plugins they create, follow the LV2 specification, and stay
    /// callable for as long as anything holds the library or a plugin made
    /// from it. This host checks what they return, but it has to call
    /// them. Every safe function of the library and of its plugins relies
    /// on these terms.
    pub unsafe fn from_entry(path: impl AsRef<Path>, entry: Lv2Entry) -> Result<Self, Lv2Error> {
        let path = path.as_ref();
        let plugins = read_bundle(path)?;
        let mut binaries: Vec<Binary> = Vec::new();
        for plugin in &plugins {
            if binaries.iter().all(|binary| binary.path != plugin.binary) {
                binaries.push(Binary {
                    path: plugin.binary.clone(),
                    entry: Entry::Given(entry),
                    _library: None,
                });
            }
        }
        Ok(Self {
            inner: Arc::new(Inner {
                path: path.to_owned(),
                plugins,
                binaries,
            }),
        })
    }

    /// The bundle folder, as it was given to [`load`](Self::load) or
    /// [`from_entry`](Self::from_entry).
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The plugins the bundle describes.
    pub fn plugins(&self) -> &[Lv2PluginInfo] {
        &self.inner.plugins
    }

    /// The description of the plugin with this URI, if the bundle has one.
    pub fn plugin(&self, uri: &str) -> Option<&Lv2PluginInfo> {
        self.inner.plugins.iter().find(|plugin| plugin.uri == uri)
    }

    /// Creates the plugin with this URI, ready to be inspected, to have its
    /// state restored and to be activated.
    ///
    /// LV2 fixes an instance's sample rate and longest block when it is
    /// created, so the instance is created for 48 kHz and blocks of up to
    /// 4,096 frames, and created again, with its state carried over, if
    /// [`Lv2Plugin::activate`] asks for others. Use
    /// [`create_with`](Self::create_with) to avoid that.
    ///
    /// # Errors
    ///
    /// [`Lv2Error::UnknownPlugin`] if the bundle describes no such plugin
    /// or its library does not have it, [`Lv2Error::MissingFeature`] if
    /// the plugin requires a feature this host does not give,
    /// [`Lv2Error::Unsupported`] for ports this host cannot connect, and
    /// [`Lv2Error::InstantiateFailed`] if the plugin refuses.
    pub fn create(&self, uri: &str) -> Result<Lv2Plugin, Lv2Error> {
        self.create_with(uri, 48_000.0, 4_096)
    }

    /// Creates the plugin with this URI for `sample_rate`, in blocks of up
    /// to `max_block_size` frames.
    ///
    /// # Errors
    ///
    /// As for [`create`](Self::create), and [`Lv2Error::InvalidArgument`]
    /// for a sample rate that is not a positive number or a block size of
    /// zero or above [`MAX_BLOCK_SIZE`](super::MAX_BLOCK_SIZE).
    pub fn create_with(
        &self,
        uri: &str,
        sample_rate: f64,
        max_block_size: usize,
    ) -> Result<Lv2Plugin, Lv2Error> {
        Lv2Plugin::create(self.clone(), uri, sample_rate, max_block_size)
    }

    /// The descriptor of the plugin `uri`, from its library.
    pub(crate) fn descriptor(&self, uri: &str) -> Option<*const LV2_Descriptor> {
        let plugin = self.plugin(uri)?;
        let binary = self
            .inner
            .binaries
            .iter()
            .find(|binary| binary.path == plugin.binary)?;
        for index in 0..MAX_DESCRIPTORS {
            // SAFETY: the library's own entry point, or the function that
            // stands for it, which per the terms of `load` and `from_entry`
            // returns a descriptor or null for any index.
            let descriptor = unsafe {
                match binary.entry {
                    Entry::Loaded(entry) => entry(index),
                    Entry::Given(entry) => entry(index).cast::<LV2_Descriptor>(),
                }
            };
            if descriptor.is_null() {
                return None;
            }
            if !descriptor.is_aligned() {
                continue;
            }
            // SAFETY: not null, aligned, and a descriptor, which LV2 keeps
            // valid for as long as the library is loaded.
            let found: *const c_char = unsafe { (*descriptor).URI };
            // SAFETY: a descriptor's URI is a C string.
            if unsafe { bounded(found, MAX_URI_BYTES) } == Some(uri.as_bytes()) {
                return Some(descriptor);
            }
        }
        None
    }
}

impl std::fmt::Debug for Lv2Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lv2Library")
            .field("path", &self.inner.path)
            .field("plugins", &self.inner.plugins.len())
            .finish()
    }
}

/// The name to give the operating system so that it loads `binary`, that
/// file and no other: absolute, so that no search path is consulted, and
/// on Windows with a dot at the end if it has no extension, so that `.dll`
/// is not added. (The CLAP host does the same, for the same reasons.)
fn exact_name(binary: &Path) -> OsString {
    let absolute = std::path::absolute(binary).unwrap_or_else(|_| binary.to_owned());
    let needs_dot = cfg!(windows) && absolute.extension().is_none();
    let mut name = absolute.into_os_string();
    if needs_dot {
        name.push(".");
    }
    name
}

/// Loads one library and finds its entry point.
///
/// # Safety
///
/// As for [`Lv2Library::load`].
unsafe fn open(binary: &Path) -> Result<Binary, Lv2Error> {
    // SAFETY: loading runs the library's initialisation routines, which
    // the caller vouches for.
    let library = unsafe { libloading::Library::new(exact_name(binary)) }.map_err(|error| {
        Lv2Error::Load {
            path: binary.to_owned(),
            reason: error.to_string(),
        }
    })?;
    // SAFETY: a symbol of this name is an `LV2_Descriptor_Function`, per the
    // caller; the function pointer is used only while `library` is loaded,
    // which the returned value keeps it.
    let entry = match unsafe { library.get::<LV2_Descriptor_Function>(ENTRY_SYMBOL) } {
        Ok(symbol) => *symbol,
        Err(_) => return Err(Lv2Error::NoEntryPoint(binary.to_owned())),
    };
    Ok(Binary {
        path: binary.to_owned(),
        entry: Entry::Loaded(entry),
        _library: Some(library),
    })
}
