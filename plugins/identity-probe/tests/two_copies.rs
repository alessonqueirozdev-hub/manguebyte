//! Two byte-identical copies of one plugin library, each with a different
//! identity appended to its file, hosted side by side in one process
//! through `manguebyte-host`, as a host loads two plugins an exporter made
//! from one pre-built engine (ADR 0012).
//!
//! The test copies the library cargo built for this package to two folders
//! under the same file name, appends an identity to each, and a third copy
//! with nothing appended. Then, in one process:
//!
//! - as CLAP libraries, the two copies give two descriptors with their own
//!   ids, names and vendors, and the bare copy gives the compiled one;
//! - as VST 3 modules, the same files give two class ids, each the
//!   framework's `class_id` of its copy's id;
//! - each copy processes audio with its own gain;
//! - each saves state under its own id, takes back its own, and refuses the
//!   other's;
//! - on Windows, both copies' editors are open at once, through CLAP and
//!   then through VST 3, each inside a real top-level window of the test's,
//!   with windows of classes registered by two different modules.
//!
//! The test must not name anything of this package: that would link it
//! into the executable, with its exported entry points. So the format of
//! the appended identity is restated here, as `src/lib.rs` documents it.
//!
//! Not under Miri, which cannot load a shared library, and not on macOS,
//! where a signed binary must not have bytes after its end: there an
//! exporter keeps the data in the bundle's `Resources` folder instead,
//! beside the file `library_path` names.
#![cfg(all(not(miri), not(target_os = "macos")))]

use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::path::{Path, PathBuf};

use manguebyte::dsp::{ProcessContext, Processor};
use manguebyte::{AudioBuffer, ParamId};
use manguebyte_host::clap::ClapLibrary;
use manguebyte_host::vst3::{ClassId, Vst3Module};

/// The compiled identity, which a copy with nothing appended keeps.
const COMPILED_ID: &str = "org.manguebyte.test.identity-probe";
const COMPILED_NAME: &str = "Identity Probe";

/// The two copies' identities: id, name, vendor.
const COPIES: [(&str, &str, &str); 2] = [
    ("parabolic.aaaaaaaaaaaaaaaaaaaaaaaaaa", "Copy A", "Vendor A"),
    ("parabolic.bbbbbbbbbbbbbbbbbbbbbbbbbb", "Copy B", "Vendor B"),
];

/// The library cargo built for this package, beside the test executable.
fn built_library() -> PathBuf {
    let executable = std::env::current_exe().expect("the test executable has a path");
    let folder = executable.parent().expect("it is in a folder");
    let file = folder.join(format!("{DLL_PREFIX}mangue_identity_probe{DLL_SUFFIX}"));
    assert!(file.is_file(), "cargo has not built {}", file.display());
    file
}

/// `identity` appended to `bytes`, as the probe reads it.
fn with_identity(bytes: &[u8], (id, name, vendor): (&str, &str, &str)) -> Vec<u8> {
    let text = format!("{id}\n{name}\n{vendor}");
    let mut out = bytes.to_vec();
    out.extend_from_slice(text.as_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(b"MBIDPROB");
    out
}

/// Where this process writes its copies. The process id keeps two runs of
/// the test that share a target directory apart.
fn copies_folder() -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("identity-probe-{}", std::process::id()))
}

/// Writes the copies: two with an identity and one without, each in a
/// folder of its own under the same file name.
fn make_copies() -> ([PathBuf; 2], PathBuf) {
    let engine = std::fs::read(built_library()).unwrap();
    // Folders that runs which ended long ago left behind: a copy a run
    // still had loaded on Windows could not be removed then.
    let hour = std::time::Duration::from_secs(3600);
    let tmp = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    for old in std::fs::read_dir(&tmp).into_iter().flatten().flatten() {
        let stale = old
            .file_name()
            .to_str()
            .and_then(|name| name.strip_prefix("identity-probe-"))
            .is_some_and(|pid| pid.bytes().all(|b| b.is_ascii_digit()))
            && old
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > hour);
        if stale {
            let _ = std::fs::remove_dir_all(old.path());
        }
    }
    let root = copies_folder();
    let _ = std::fs::remove_dir_all(&root);
    let write = |folder: &str, bytes: &[u8]| -> PathBuf {
        let folder = root.join(folder);
        std::fs::create_dir_all(&folder).unwrap();
        // The same name in every folder.
        let file = folder.join(format!("Instrument-{}.clap", std::process::id()));
        std::fs::write(&file, bytes).unwrap();
        file
    };
    let a = write("a", &with_identity(&engine, COPIES[0]));
    let b = write("b", &with_identity(&engine, COPIES[1]));
    let plain = write("plain", &engine);
    // The engine part of each is the built library, byte for byte.
    for copy in [&a, &b, &plain] {
        assert_eq!(std::fs::read(copy).unwrap()[..engine.len()], engine[..]);
    }
    assert_eq!(a.file_name(), b.file_name());
    ([a, b], plain)
}

fn load_clap(path: &Path) -> ClapLibrary {
    // SAFETY: a copy of this package's own library, whose entry point is
    // the framework's CLAP wrapper, with data after its end that the
    // system's loader ignores.
    unsafe { ClapLibrary::load(path) }.expect("the copy loads as CLAP")
}

fn load_vst3(path: &Path) -> Vst3Module {
    // SAFETY: as for `load_clap`; its `GetPluginFactory` is the
    // framework's VST 3 wrapper.
    unsafe { Vst3Module::load(path) }.expect("the copy loads as VST 3")
}

/// The class id the framework derives from `id`.
fn class_of(id: &str) -> ClassId {
    ClassId::from_bytes(manguebyte::vst3::class_id(id).map(|unit| unit as u8))
}

fn gain() -> ParamId {
    ParamId::from_name("gain")
}

/// The gain's id as the VST 3 wrapper gives it.
fn vst3_gain() -> ParamId {
    ParamId(gain().0 & 0x7FFF_FFFF)
}

/// One block of ones through `processor`, and the first sample out.
fn first_sample(processor: &mut impl Processor<f32>) -> f32 {
    let mut block = AudioBuffer::<f32>::new(2, 64);
    for channel in 0..2 {
        block.channel_mut(channel).fill(1.0);
    }
    processor.process_with(&mut block, &ProcessContext::empty());
    block.channel(0)[0]
}

#[test]
fn two_copies_of_one_library_are_two_plugins_in_one_process() {
    let ([a, b], plain) = make_copies();

    // --- CLAP: three descriptors, from three files with the same code.
    let clap = [load_clap(&a), load_clap(&b)];
    let bare = load_clap(&plain);
    for (library, (id, name, vendor)) in clap.iter().zip(COPIES) {
        let [descriptor] = library.descriptors() else {
            panic!("one plugin per copy: {:?}", library.descriptors());
        };
        assert_eq!(
            (
                descriptor.id.as_str(),
                descriptor.name.as_str(),
                descriptor.vendor.as_str()
            ),
            (id, name, vendor)
        );
        assert!(library.create(COMPILED_ID).is_err());
    }
    let [descriptor] = bare.descriptors() else {
        panic!("one plugin: {:?}", bare.descriptors());
    };
    assert_eq!(
        (descriptor.id.as_str(), descriptor.name.as_str()),
        (COMPILED_ID, COMPILED_NAME),
        "a copy with nothing appended is the compiled plugin"
    );

    // --- VST 3: the same files, two class ids.
    let vst3 = [load_vst3(&a), load_vst3(&b)];
    for (module, (id, name, vendor)) in vst3.iter().zip(COPIES) {
        let [class] = module.classes() else {
            panic!("one class per copy: {:?}", module.classes());
        };
        assert_eq!(class.id, class_of(id));
        assert_eq!((class.name.as_str(), class.vendor.as_str()), (name, vendor));
        assert_eq!(module.factory_info().vendor, vendor);
        assert!(module.create(class_of(COMPILED_ID)).is_err());
    }
    assert_ne!(class_of(COPIES[0].0), class_of(COPIES[1].0));
    let bare_module = load_vst3(&plain);
    let [bare_class] = bare_module.classes() else {
        panic!("one class: {:?}", bare_module.classes());
    };
    assert_eq!(bare_class.id, class_of(COMPILED_ID));

    // --- Each processes audio with its own gain, at once.
    let mut clap_plugins = [
        clap[0].create(COPIES[0].0).unwrap(),
        clap[1].create(COPIES[1].0).unwrap(),
    ];
    let mut vst3_plugins = [
        vst3[0].create(class_of(COPIES[0].0)).unwrap(),
        vst3[1].create(class_of(COPIES[1].0)).unwrap(),
    ];
    let gains = [0.5, 0.25];
    for (plugin, value) in clap_plugins.iter_mut().zip(gains) {
        plugin.set_param(gain(), value).unwrap();
    }
    for (plugin, value) in vst3_plugins.iter_mut().zip(gains) {
        plugin.set_param(vst3_gain(), value).unwrap();
    }
    {
        let mut processors: Vec<_> = clap_plugins
            .iter_mut()
            .map(|plugin| plugin.activate(48_000.0, 256).unwrap())
            .collect();
        let mut vst3_processors: Vec<_> = vst3_plugins
            .iter_mut()
            .map(|plugin| plugin.activate(48_000.0, 256).unwrap())
            .collect();
        for _ in 0..2 {
            for (processor, value) in processors.iter_mut().zip(gains) {
                assert_eq!(first_sample(processor), value as f32);
            }
            for (processor, value) in vst3_processors.iter_mut().zip(gains) {
                assert_eq!(first_sample(processor), value as f32);
            }
        }
    }
    for plugin in &mut clap_plugins {
        plugin.deactivate().unwrap();
    }
    for plugin in &mut vst3_plugins {
        plugin.deactivate().unwrap();
    }

    // --- Each saves under its own id, and refuses the other's state.
    let clap_saved: Vec<Vec<u8>> = clap_plugins
        .iter_mut()
        .map(|plugin| plugin.save_state().unwrap())
        .collect();
    for (saved, (id, _, _)) in clap_saved.iter().zip(COPIES) {
        let text = String::from_utf8_lossy(saved);
        assert!(
            text.starts_with(&format!("manguebyte-plugin 1 {id}\n")),
            "{text}"
        );
    }
    for (index, library) in clap.iter().enumerate() {
        let mut fresh = library.create(COPIES[index].0).unwrap();
        assert!(fresh.load_state(&clap_saved[1 - index]).is_err());
        assert_eq!(fresh.param_value(gain()), Some(1.0), "refused, unchanged");
        fresh.load_state(&clap_saved[index]).unwrap();
        assert_eq!(fresh.param_value(gain()), Some(gains[index]));
    }
    let vst3_saved: Vec<Vec<u8>> = vst3_plugins
        .iter_mut()
        .map(|plugin| plugin.save_state().unwrap())
        .collect();
    for (index, module) in vst3.iter().enumerate() {
        let mut fresh = module.create(class_of(COPIES[index].0)).unwrap();
        assert!(fresh.load_state(&vst3_saved[1 - index]).is_err());
        assert_eq!(fresh.param_value(vst3_gain()), Some(1.0));
        fresh.load_state(&vst3_saved[index]).unwrap();
        assert_eq!(fresh.param_value(vst3_gain()), Some(gains[index]));
    }

    #[cfg(windows)]
    windows::both_editors_open_at_once(&mut clap_plugins, &mut vst3_plugins);

    drop(clap_plugins);
    drop(vst3_plugins);
    drop((clap, bare, vst3, bare_module));
    // Best effort: on Windows a library the system keeps loaded stays
    // until a later run sweeps it.
    let _ = std::fs::remove_dir_all(copies_folder());
}

#[cfg(windows)]
mod windows {
    use std::time::Duration;

    use manguebyte_host::clap::ClapPlugin;
    use manguebyte_host::editor::EditorSize;
    use manguebyte_host::vst3::Vst3Plugin;
    use manguebyte_window::win32::testing::{self, TopLevel, WindowClass};

    /// The probe's editor size at scale 1.
    const SIZE: EditorSize = EditorSize::new(240, 160);

    /// The one window inside `parent`, and its class.
    fn editor_window(parent: &TopLevel) -> (usize, WindowClass) {
        let children = testing::children_of(parent.handle().expect("open"));
        assert_eq!(children.len(), 1, "one window inside the parent");
        let class = testing::class_of(children[0]).expect("a window");
        assert!(testing::class_exists(&class));
        (children[0], class)
    }

    /// Checks two editors' windows: the right size, and of classes two
    /// different modules registered, neither of them this executable.
    fn check_apart(parents: &[TopLevel; 2]) -> [WindowClass; 2] {
        let [(first, first_class), (second, second_class)] =
            [&parents[0], &parents[1]].map(editor_window);
        for window in [first, second] {
            assert_eq!(
                testing::client_size_of(window),
                Some((SIZE.width, SIZE.height))
            );
        }
        assert_ne!(first_class.module, second_class.module, "two modules");
        assert_ne!(first_class.name, second_class.name, "two classes");
        for class in [&first_class, &second_class] {
            assert_ne!(class.name, testing::window_class_name(), "not ours");
        }
        [first_class, second_class]
    }

    pub(super) fn both_editors_open_at_once(
        clap: &mut [ClapPlugin; 2],
        vst3: &mut [Vst3Plugin; 2],
    ) {
        let parents =
            || [0, 1].map(|_| TopLevel::new(SIZE.width + 40, SIZE.height + 40).expect("a desktop"));

        // Through CLAP.
        let windows = parents();
        for (plugin, parent) in clap.iter_mut().zip(&windows) {
            assert!(plugin.has_editor());
            assert_eq!(plugin.open_editor(parent.parent(), Some(1.0)), Ok(SIZE));
        }
        testing::pump_until(Duration::from_millis(250), || {
            for plugin in clap.iter_mut() {
                plugin.pump();
            }
            false
        });
        let clap_classes = check_apart(&windows);
        for plugin in clap.iter_mut() {
            plugin.close_editor();
        }
        for parent in &windows {
            assert!(testing::children_of(parent.handle().unwrap()).is_empty());
        }

        // Through VST 3, from the same files.
        let windows = parents();
        for (plugin, parent) in vst3.iter_mut().zip(&windows) {
            assert!(plugin.has_editor());
            assert_eq!(plugin.open_editor(parent.parent(), Some(1.0)), Ok(SIZE));
        }
        testing::pump_until(Duration::from_millis(250), || {
            for plugin in vst3.iter_mut() {
                plugin.pump();
            }
            false
        });
        let vst3_classes = check_apart(&windows);
        // One module per file, whichever format opened it.
        for (clap_class, vst3_class) in clap_classes.iter().zip(&vst3_classes) {
            assert_eq!(clap_class.module, vst3_class.module);
        }
        for plugin in vst3.iter_mut() {
            plugin.close_editor();
        }
        testing::pump(Duration::from_millis(20));
    }
}
