//! A plugin whose identity is read, when the host first asks for it, from
//! the file the host says the plugin was loaded from.
//!
//! The test plays the host, in process: it calls the entry point's `init`
//! with the path of a file it wrote, as a host passes the path of the
//! library it loaded, and the plugin's `Plugin::info` reads its id, name,
//! vendor and version from that file through `library_path`. The
//! descriptor, and the id `create_plugin` takes, are then those, and not
//! the plugin's compiled `INFO`.
//!
//! Every `unsafe` block in this file is a call across the CLAP interface
//! with pointers the test owns and keeps alive for the duration of the
//! call, so the blocks are not commented one by one.
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::Arc;

use manguebyte_clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte_clap::clap_sys::host::clap_host;
use manguebyte_clap::clap_sys::plugin_features::CLAP_PLUGIN_FEATURE_INSTRUMENT;
use manguebyte_clap::clap_sys::version::CLAP_VERSION;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::param_set;
use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, library_path};

struct NoParams {}
param_set!(NoParams {});

/// Reads its identity from its library's file: four lines, the id, the
/// name, the vendor and the version. Says it is an effect, but was
/// compiled as an instrument.
struct Engine(Arc<NoParams>);

impl Plugin for Engine {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.engine",
        name: "Engine",
        vendor: "Manguebyte tests",
        version: "0.0.0",
        description: "Plays whatever it is given",
        kind: PluginKind::Instrument,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
    type Params = NoParams;

    fn info() -> PluginInfo {
        let Some(text) = library_path().and_then(|path| std::fs::read_to_string(path).ok()) else {
            return Self::INFO;
        };
        let lines: Vec<&'static str> = text.leak().lines().collect();
        let [id, name, vendor, version] = lines[..] else {
            return Self::INFO;
        };
        PluginInfo {
            id,
            name,
            vendor,
            version,
            description: "Read from its file",
            kind: PluginKind::Effect,
        }
    }
    fn new() -> Self {
        Self(Arc::new(NoParams {}))
    }
    fn params(&self) -> Arc<NoParams> {
        Arc::clone(&self.0)
    }
    fn prepare(&mut self, _: &ProcessSpec) {}
    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

unsafe extern "C" fn no_extension(_: *const clap_host, _: *const c_char) -> *const c_void {
    ptr::null()
}

fn text(pointer: *const c_char) -> String {
    unsafe { CStr::from_ptr(pointer) }
        .to_str()
        .unwrap()
        .to_owned()
}

/// The folder of this process's library files; the process id keeps two
/// runs of the tests that share a target directory apart.
fn library_folder() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("clap-identity-{}", std::process::id()))
}

/// A file standing for the plugin's library, holding its identity.
fn library_file(name: &str, identity: &str) -> PathBuf {
    let folder = library_folder();
    std::fs::create_dir_all(&folder).unwrap();
    let file = folder.join(name);
    std::fs::write(&file, identity).unwrap();
    file
}

#[test]
#[cfg_attr(miri, ignore = "reads and writes files, which Miri cannot")]
fn the_descriptor_is_the_identity_read_from_the_path_init_was_given() {
    let file = library_file(
        "Copy One.clap",
        "parabolic.copy-one\nCopy One\nSomeone\n3.1.4\n",
    );
    let entry = manguebyte_clap::plugin_entry::<Engine>();
    let host = clap_host {
        clap_version: CLAP_VERSION,
        host_data: ptr::null_mut(),
        name: c"host".as_ptr(),
        vendor: c"".as_ptr(),
        url: c"".as_ptr(),
        version: c"1".as_ptr(),
        get_extension: Some(no_extension),
        request_restart: None,
        request_process: None,
        request_callback: None,
    };
    unsafe {
        let path = CString::new(file.to_str().unwrap()).unwrap();
        assert!(entry.init.unwrap()(path.as_ptr()));
        // The path the host gave is the library's, from then on.
        assert_eq!(library_path(), Some(file.as_path()));
        // A second `init` with another path changes nothing.
        let other = library_file("Other.clap", "parabolic.other\nOther\nX\n1\n");
        let other = CString::new(other.to_str().unwrap()).unwrap();
        assert!(entry.init.unwrap()(other.as_ptr()));
        assert_eq!(library_path(), Some(file.as_path()));

        let factory = entry.get_factory.unwrap()(CLAP_PLUGIN_FACTORY_ID.as_ptr())
            .cast::<clap_plugin_factory>();
        assert_eq!((*factory).get_plugin_count.unwrap()(factory), 1);
        let descriptor = (*factory).get_plugin_descriptor.unwrap()(factory, 0);
        assert!(!descriptor.is_null());
        let descriptor = &*descriptor;
        assert_eq!(text(descriptor.id), "parabolic.copy-one");
        assert_eq!(text(descriptor.name), "Copy One");
        assert_eq!(text(descriptor.vendor), "Someone");
        assert_eq!(text(descriptor.version), "3.1.4");
        assert_eq!(text(descriptor.description), "Read from its file");
        // The kind is the compiled one, whatever `info` says.
        assert_eq!(
            CStr::from_ptr(*descriptor.features),
            CLAP_PLUGIN_FEATURE_INSTRUMENT
        );
        // The same descriptor every time: the identity was read once.
        assert!(ptr::eq(
            descriptor,
            (*factory).get_plugin_descriptor.unwrap()(factory, 0)
        ));

        // The compiled id is no longer this plugin's.
        let create = (*factory).create_plugin.unwrap();
        let compiled = create(factory, &host, c"org.manguebyte.test.engine".as_ptr());
        assert!(compiled.is_null());
        let plugin = create(factory, &host, c"parabolic.copy-one".as_ptr());
        assert!(!plugin.is_null());
        assert!(ptr::eq((*plugin).desc, descriptor));
        assert!((*plugin).init.unwrap()(plugin));
        (*plugin).destroy.unwrap()(plugin);
        entry.deinit.unwrap()();
        entry.deinit.unwrap()();
    }
    let _ = std::fs::remove_dir_all(library_folder());
}
