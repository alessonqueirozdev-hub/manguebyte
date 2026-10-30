//! Where a host's units come from: the system's registry, one bundle, or a
//! factory function in this process.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::AuError;
use super::component::{AuComponentInfo, ComponentCodes};
use super::plugin::AuPlugin;
#[cfg_attr(not(target_os = "macos"), allow(unused_imports))]
use super::sys::*;
use super::unit::Unit;

/// A component's factory function: what a `.component` bundle exports
/// under the name its `Info.plist` gives (`AudioComponentFactoryFunction`
/// in `AudioComponent.h`), and what [`export_au!`] defines as
/// `ManguebyteAudioUnitFactory`. It is given the component's description
/// and returns its `AudioComponentPlugInInterface`, or null.
///
/// [`export_au!`]: https://docs.rs/manguebyte-au/latest/manguebyte_au/macro.export_au.html
pub type AuFactory =
    unsafe extern "C" fn(description: *const std::ffi::c_void) -> *mut std::ffi::c_void;

#[derive(Clone, Copy)]
enum Source {
    Factory(AuFactory),
    #[cfg(target_os = "macos")]
    System,
}

struct Inner {
    path: PathBuf,
    components: Vec<AuComponentInfo>,
    source: Source,
}

/// Audio Unit components, and the way to make their instances.
///
/// The handle can be cloned and sent between threads.
///
/// ```
/// use manguebyte_host::au::{AuError, AuLibrary};
///
/// // A path with nothing behind it is an error, not a crash.
/// // SAFETY: there is no bundle at this path, so no code is run.
/// let missing = unsafe { AuLibrary::load("no/such/Unit.component") };
/// assert!(matches!(missing, Err(AuError::NotFound(_))));
/// ```
#[derive(Clone)]
pub struct AuLibrary {
    inner: Arc<Inner>,
}

impl AuLibrary {
    /// Every component the system has registered that can run as a
    /// processor (see [`ComponentCodes::is_processor`]), as [`components`]
    /// lists them.
    ///
    /// # Errors
    ///
    /// [`AuError::NotOnThisSystem`] except on macOS.
    ///
    /// # Safety
    ///
    /// Listing runs no code of any component, but [`create`](Self::create)
    /// makes an instance, which runs the component's code with the full
    /// rights of this process. The caller vouches for every registered
    /// component it creates: its code must keep to Apple's Audio Unit
    /// interface. Every safe function of the library and of its plugins
    /// relies on these terms.
    pub unsafe fn system() -> Result<Self, AuError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                inner: Arc::new(Inner {
                    path: PathBuf::new(),
                    components: components()
                        .into_iter()
                        .filter(|info| info.codes.is_processor())
                        .collect(),
                    source: Source::System,
                }),
            })
        }
        #[cfg(not(target_os = "macos"))]
        Err(AuError::NotOnThisSystem)
    }

    /// The units a `.component` bundle lists in its `Info.plist`, each as
    /// the system registered it, to be made through AudioToolbox.
    ///
    /// # Errors
    ///
    /// [`AuError::NotFound`] if nothing is at `path`,
    /// [`AuError::NotABundle`] without `Contents/Info.plist`,
    /// [`AuError::BadInfoPlist`] if it lists no units, then
    /// [`AuError::NotOnThisSystem`] except on macOS and
    /// [`AuError::UnknownComponent`] for a unit the system has not
    /// registered.
    ///
    /// # Safety
    ///
    /// As for [`system`](Self::system): the bundle's code runs when a unit
    /// is created, and the caller vouches for it.
    pub unsafe fn load(path: impl AsRef<Path>) -> Result<Self, AuError> {
        let path = path.as_ref();
        if !path.exists() {
            return Err(AuError::NotFound(path.to_owned()));
        }
        let plist = path.join("Contents").join("Info.plist");
        let bytes = std::fs::read(&plist).map_err(|_| AuError::NotABundle(path.to_owned()))?;
        let listed = read_bundle_plist(&bytes)?;
        #[cfg(target_os = "macos")]
        {
            let mut found = Vec::with_capacity(listed.len());
            for unit in listed {
                let Some(registered) = find(unit.codes) else {
                    return Err(AuError::UnknownComponent(unit.codes.to_string()));
                };
                // The registry's name and version are what hosts show; the
                // bundle's description and tags are kept.
                let mut info = info_of(registered).unwrap_or_else(|| unit.clone());
                info.description = unit.description;
                info.tags = unit.tags;
                found.push(info);
            }
            Ok(Self {
                inner: Arc::new(Inner {
                    path: path.to_owned(),
                    components: found,
                    source: Source::System,
                }),
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = listed;
            Err(AuError::NotOnThisSystem)
        }
    }

    /// Components whose instances `factory` makes in this process, as
    /// AudioToolbox makes a version 2 unit's: the factory returns the
    /// unit's `AudioComponentPlugInInterface`, which is opened, and whose
    /// methods are looked up by selector. `path` is what
    /// [`path`](Self::path) says, such as the bundle the factory came
    /// from. Works on every system.
    ///
    /// # Safety
    ///
    /// `factory` must be an `AudioComponentFactoryFunction` that, given the
    /// description of any of `components`, returns null or an interface
    /// whose methods keep to Apple's Audio Unit interface, and that stays
    /// callable for as long as anything holds the library or a plugin made
    /// from it. Making an instance runs its code with the full rights of
    /// this process; every safe function of the library and of its
    /// plugins relies on these terms.
    pub unsafe fn from_factory(
        factory: AuFactory,
        components: Vec<AuComponentInfo>,
        path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                path: path.into(),
                components,
                source: Source::Factory(factory),
            }),
        }
    }

    /// The bundle the library was loaded from, or what
    /// [`from_factory`](Self::from_factory) was given; empty for the
    /// system's.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// The components.
    pub fn components(&self) -> &[AuComponentInfo] {
        &self.inner.components
    }

    /// The component with these codes, if the library has it.
    pub fn component(&self, codes: ComponentCodes) -> Option<&AuComponentInfo> {
        self.inner
            .components
            .iter()
            .find(|info| info.codes == codes)
    }

    /// Makes an instance of the component with these codes, ready to have
    /// its channels set, its state restored and to be activated.
    ///
    /// The calling thread becomes the plugin's own.
    ///
    /// # Errors
    ///
    /// [`AuError::UnknownComponent`] if the library has no such component
    /// or the system no longer has it registered, and
    /// [`AuError::InstantiateFailed`] if the unit could not be made.
    pub fn create(&self, codes: ComponentCodes) -> Result<AuPlugin, AuError> {
        let info = self
            .component(codes)
            .cloned()
            .ok_or_else(|| AuError::UnknownComponent(codes.to_string()))?;
        let unit = match self.inner.source {
            // SAFETY: the factory, per the terms of `from_factory`, given
            // the component's own description.
            Source::Factory(factory) => unsafe {
                Unit::from_factory(factory, &codes.description())
            },
            #[cfg(target_os = "macos")]
            Source::System => {
                let component =
                    find(codes).ok_or_else(|| AuError::UnknownComponent(codes.to_string()))?;
                // SAFETY: a registered component, which the caller vouched
                // for when the library was made.
                unsafe { Unit::from_component(component, codes.takes_midi()) }
            }
        }?;
        AuPlugin::new(self.clone(), info, unit)
    }

    /// Makes the component whose codes `id` writes, as
    /// [`PluginEntry::id`](crate::any::PluginEntry::id) gives them.
    ///
    /// # Errors
    ///
    /// As for [`create`](Self::create), and [`AuError::UnknownComponent`]
    /// for text that is not three codes.
    pub fn create_by_id(&self, id: &str) -> Result<AuPlugin, AuError> {
        let codes: ComponentCodes = id
            .parse()
            .map_err(|_| AuError::UnknownComponent(id.to_owned()))?;
        self.create(codes)
    }
}

impl std::fmt::Debug for AuLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuLibrary")
            .field("path", &self.inner.path)
            .field("components", &self.inner.components.len())
            .finish_non_exhaustive()
    }
}

/// The units a bundle's `Info.plist` lists, in XML or, on macOS, binary
/// form.
fn read_bundle_plist(bytes: &[u8]) -> Result<Vec<AuComponentInfo>, AuError> {
    if bytes.len() > super::MAX_INFO_PLIST_BYTES {
        return Err(AuError::BadInfoPlist("the Info.plist is too large".into()));
    }
    #[cfg(target_os = "macos")]
    if bytes.starts_with(b"bplist") {
        let xml = binary_to_xml(bytes)
            .ok_or_else(|| AuError::BadInfoPlist("CoreFoundation cannot read it".into()))?;
        return super::read_info_plist(&xml);
    }
    super::read_info_plist(bytes)
}

/// A property list of any form, as XML, through CoreFoundation.
#[cfg(target_os = "macos")]
fn binary_to_xml(bytes: &[u8]) -> Option<Vec<u8>> {
    use super::cf::Owned;
    // SAFETY: the bytes are readable for their length; every object made
    // is released by its `Owned`.
    unsafe {
        let data = Owned::new(CFDataCreate(
            std::ptr::null(),
            bytes.as_ptr(),
            bytes.len() as CFIndex,
        ))?;
        let list = Owned::new(CFPropertyListCreateWithData(
            std::ptr::null(),
            data.as_ptr(),
            kCFPropertyListImmutable,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ))?;
        let xml = Owned::new(CFPropertyListCreateData(
            std::ptr::null(),
            list.as_ptr(),
            kCFPropertyListXMLFormat_v1_0,
            0,
            std::ptr::null_mut(),
        ))?;
        let length = usize::try_from(CFDataGetLength(xml.as_ptr())).ok()?;
        let start = CFDataGetBytePtr(xml.as_ptr());
        (!start.is_null()).then(|| std::slice::from_raw_parts(start, length).to_vec())
    }
}

/// Every component the system has registered, as AudioToolbox lists them
/// with `AudioComponentFindNext`, with the name, maker and version each
/// gives. No code of any component runs. Empty except on macOS.
pub fn components() -> Vec<AuComponentInfo> {
    #[cfg(target_os = "macos")]
    {
        let any = AudioComponentDescription::default();
        let mut found = Vec::new();
        let mut component: AudioComponent = std::ptr::null_mut();
        // The registry is finite; this keeps a broken one from looping.
        for _ in 0..100_000 {
            // SAFETY: the last component found, or null to start, and a
            // description that matches every component.
            component = unsafe { AudioComponentFindNext(component, &any) };
            if component.is_null() {
                break;
            }
            if let Some(info) = info_of(component) {
                found.push(info);
            }
        }
        found
    }
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// The registered component with exactly these codes.
#[cfg(target_os = "macos")]
fn find(codes: ComponentCodes) -> Option<AudioComponent> {
    let description = codes.description();
    // SAFETY: a description to match, from the start of the registry.
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &description) };
    (!component.is_null()).then_some(component)
}

/// What the registry says about a component.
#[cfg(target_os = "macos")]
fn info_of(component: AudioComponent) -> Option<AuComponentInfo> {
    let mut description = AudioComponentDescription::default();
    let mut name: CFStringRef = std::ptr::null();
    let mut version = 0u32;
    // SAFETY: a registered component and places for its answers; the
    // name is handed over and released by `take_string`.
    unsafe {
        if AudioComponentGetDescription(component, &mut description) != noErr {
            return None;
        }
        let name = if AudioComponentCopyName(component, &mut name) == noErr {
            super::cf::take_string(name).unwrap_or_default()
        } else {
            String::new()
        };
        if AudioComponentGetVersion(component, &mut version) != noErr {
            version = 0;
        }
        let codes = ComponentCodes {
            kind: description.componentType,
            subtype: description.componentSubType,
            manufacturer: description.componentManufacturer,
        };
        Some(AuComponentInfo::named(codes, &name, version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_without_an_info_plist_is_not_a_bundle() {
        // The process id keeps two runs of the tests apart.
        let folder = std::env::temp_dir().join(format!(
            "manguebyte-au-not-a-bundle-{}.component",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        // SAFETY: the folder holds no code.
        let loaded = unsafe { AuLibrary::load(&folder) };
        assert!(matches!(loaded, Err(AuError::NotABundle(_))), "{loaded:?}");
        let _ = std::fs::remove_dir_all(&folder);
        if cfg!(not(target_os = "macos")) {
            // SAFETY: nothing is listed or run off macOS.
            let system = unsafe { AuLibrary::system() };
            assert!(matches!(system, Err(AuError::NotOnThisSystem)));
            assert!(components().is_empty());
        }
    }
}
