//! Scans a folder of plugins the way a host scans the system's: the
//! framework's example plugins as CLAP, VST 3 and LV2, and fake plugins
//! built here that crash, hang and refuse while they are being listed.
//! The scanner process is the crate's own `manguebyte-scan`, so whatever
//! the fakes do happens to it, and the test, which is the host, goes on.
//!
//! Not under Miri, which can start no process.
#![cfg(all(feature = "clap", feature = "vst3", feature = "lv2", not(miri)))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mangue_pluck::Pluck;
use manguebyte_host::any::{Format, Kind};
use manguebyte_host::scan::{
    Candidate, Fingerprint, ScanCache, ScanError, ScanReport, Scanner, find,
};
use manguebyte_plugin::Plugin;

const ALL: [Format; 3] = [Format::Clap, Format::Vst3, Format::Lv2];

/// How long the scanner is given for one file. A hanging plugin costs this
/// much, once.
const TIMEOUT: Duration = Duration::from_secs(4);

fn scanner() -> Scanner {
    Scanner::new(env!("CARGO_BIN_EXE_manguebyte-scan")).with_timeout(TIMEOUT)
}

/// What a fake plugin's `init` does.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Ends the process at once, as a crash would.
    Crash,
    /// Never returns.
    Hang,
    /// Says no, as a plugin that cannot start does.
    Refuse,
}

/// The source of a CLAP library whose entry point does `behaviour` when it
/// is initialised. It is the smallest thing a host loads as CLAP: a
/// `clap_entry` with a version CLAP accepts.
fn fake_source(behaviour: Behaviour) -> String {
    let init = match behaviour {
        Behaviour::Crash => "std::process::abort()",
        Behaviour::Hang => "loop { std::thread::sleep(std::time::Duration::from_secs(1)); }",
        Behaviour::Refuse => "false",
    };
    format!(
        r#"
#![allow(non_upper_case_globals, unreachable_code)]
use std::ffi::{{c_char, c_void}};
#[repr(C)]
pub struct Version {{ major: u32, minor: u32, revision: u32 }}
#[repr(C)]
pub struct Entry {{
    version: Version,
    init: unsafe extern "C" fn(*const c_char) -> bool,
    deinit: unsafe extern "C" fn(),
    get_factory: unsafe extern "C" fn(*const c_char) -> *const c_void,
}}
unsafe extern "C" fn init(_path: *const c_char) -> bool {{ {init} }}
unsafe extern "C" fn deinit() {{}}
unsafe extern "C" fn get_factory(_id: *const c_char) -> *const c_void {{ std::ptr::null() }}
#[no_mangle]
pub static clap_entry: Entry = Entry {{
    version: Version {{ major: 1, minor: 2, revision: 0 }},
    init,
    deinit,
    get_factory,
}};
"#
    )
}

/// The compiler that built this test, as near as can be found: the one
/// cargo names, the one beside cargo, or the one on the path.
fn rustc() -> PathBuf {
    if let Some(rustc) = std::env::var_os("RUSTC") {
        return rustc.into();
    }
    if let Some(cargo) = std::env::var_os("CARGO") {
        let beside =
            Path::new(&cargo).with_file_name(format!("rustc{}", std::env::consts::EXE_SUFFIX));
        if beside.is_file() {
            return beside;
        }
    }
    "rustc".into()
}

/// The three fake plugins, built once for all the tests in this file.
fn fakes() -> &'static [(Behaviour, PathBuf); 3] {
    static BUILT: OnceLock<[(Behaviour, PathBuf); 3]> = OnceLock::new();
    BUILT.get_or_init(|| {
        let folder = common::lasting_scratch_folder("scan-fakes");
        [Behaviour::Crash, Behaviour::Hang, Behaviour::Refuse].map(|behaviour| {
            let name = match behaviour {
                Behaviour::Crash => "crash",
                Behaviour::Hang => "hang",
                Behaviour::Refuse => "refuse",
            };
            let source = folder.join(format!("{name}.rs"));
            std::fs::write(&source, fake_source(behaviour)).unwrap();
            let library = folder.join(format!("{name}.clap"));
            let status = Command::new(rustc())
                .args([
                    "--edition",
                    "2021",
                    "--crate-type",
                    "cdylib",
                    "-C",
                    "debuginfo=0",
                ])
                .arg("-o")
                .arg(&library)
                .arg(&source)
                .status()
                .expect("rustc runs");
            assert!(status.success(), "the fake plugin {name} builds");
            (behaviour, library)
        })
    })
}

fn fake(behaviour: Behaviour) -> &'static Path {
    let index = match behaviour {
        Behaviour::Crash => 0,
        Behaviour::Hang => 1,
        Behaviour::Refuse => 2,
    };
    &fakes()[index].1
}

/// A folder laid out as plugins are installed: the example plugins, the
/// fakes, a file that is not a plugin, and things that are not plugins by
/// their names, some of it a few folders down.
fn plugin_folder(name: &str) -> common::Scratch {
    let root = common::scratch_folder(&format!("scan-{name}"));
    let deeper = root.join("vendor").join("deeper");
    std::fs::create_dir_all(&deeper).unwrap();
    std::fs::copy(
        common::built_library("mangue_duck"),
        root.join("Mangue Duck.clap"),
    )
    .unwrap();
    std::fs::copy(
        common::built_library("mangue_pluck"),
        deeper.join("Mangue Pluck.vst3"),
    )
    .unwrap();
    let bundle = common::lv2_bundle(
        "mangue_pluck",
        mangue_pluck::__manguebyte_lv2_bundle_files,
        &format!("scan-{name}-lv2"),
    );
    copy_folder(&bundle, &root.join("vendor").join("Mangue Pluck.lv2"));
    std::fs::copy(fake(Behaviour::Crash), root.join("crash.clap")).unwrap();
    std::fs::copy(fake(Behaviour::Hang), deeper.join("hang.clap")).unwrap();
    std::fs::copy(fake(Behaviour::Refuse), root.join("refuse.clap")).unwrap();
    std::fs::write(root.join("text.clap"), "this is not a library").unwrap();
    std::fs::write(root.join("readme.txt"), "not a plugin by its name").unwrap();
    std::fs::create_dir_all(root.join("empty.lv2")).unwrap();
    root
}

fn copy_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
    }
}

fn names(report: &ScanReport) -> Vec<(String, Format, String)> {
    let mut found: Vec<(String, Format, String)> = report
        .plugins
        .iter()
        .map(|plugin| {
            let file = plugin
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (file, plugin.entry.format, plugin.entry.name.clone())
        })
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

fn failure<'a>(report: &'a ScanReport, file: &str) -> &'a ScanError {
    &report
        .failures
        .iter()
        .find(|failure| failure.path.file_name().is_some_and(|name| name == file))
        .unwrap_or_else(|| panic!("no failure for {file}: {:?}", report.failures))
        .error
}

#[test]
fn plugins_are_found_by_their_names_alone() {
    let root = plugin_folder("find");
    let found = find(&[root.to_path_buf()], &ALL);
    let mut files: Vec<(String, Format)> = found
        .iter()
        .map(|candidate| {
            let file = candidate
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            (file, candidate.format)
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        files,
        [
            ("Mangue Duck.clap".to_owned(), Format::Clap),
            ("Mangue Pluck.lv2".to_owned(), Format::Lv2),
            ("Mangue Pluck.vst3".to_owned(), Format::Vst3),
            ("crash.clap".to_owned(), Format::Clap),
            ("empty.lv2".to_owned(), Format::Lv2),
            ("hang.clap".to_owned(), Format::Clap),
            ("refuse.clap".to_owned(), Format::Clap),
            ("text.clap".to_owned(), Format::Clap),
        ]
    );
    // One format only, a path that is itself a plugin, a path twice, and a
    // path with nothing at it.
    let only = find(&[root.to_path_buf()], &[Format::Vst3]);
    assert_eq!(only.len(), 1);
    let direct = root.join("crash.clap");
    let twice = find(
        &[direct.clone(), direct.clone(), root.join("nowhere")],
        &ALL,
    );
    assert_eq!(
        twice,
        [Candidate {
            path: direct,
            format: Format::Clap
        }]
    );
}

#[test]
fn a_scan_lists_what_works_and_survives_what_does_not() {
    let root = plugin_folder("survive");
    let candidates = find(&[root.to_path_buf()], &ALL);
    let mut cache = ScanCache::new();
    let started = Instant::now();
    let report = scanner().scan(&candidates, &mut cache);
    let took = started.elapsed();

    // Every plugin the example files hold, in each format.
    assert_eq!(
        names(&report),
        [
            (
                "Mangue Duck.clap".to_owned(),
                Format::Clap,
                "Mangue Duck".to_owned()
            ),
            (
                "Mangue Pluck.lv2".to_owned(),
                Format::Lv2,
                Pluck::INFO.name.to_owned()
            ),
            (
                "Mangue Pluck.vst3".to_owned(),
                Format::Vst3,
                Pluck::INFO.name.to_owned()
            ),
        ]
    );
    for plugin in &report.plugins {
        let expected = if plugin.entry.name == Pluck::INFO.name {
            Kind::Instrument
        } else {
            Kind::Effect
        };
        assert_eq!(plugin.entry.kind, expected, "{:?}", plugin.entry);
    }
    let lv2 = report
        .plugins
        .iter()
        .find(|plugin| plugin.entry.format == Format::Lv2)
        .unwrap();
    assert_eq!(lv2.entry.id, "urn:manguebyte:org.manguebyte.pluck");

    // And every way of failing, each reported as what it was.
    assert_eq!(report.failures.len(), 5, "{:?}", report.failures);
    assert!(
        matches!(failure(&report, "crash.clap"), ScanError::Crashed { .. }),
        "{:?}",
        failure(&report, "crash.clap")
    );
    assert_eq!(failure(&report, "hang.clap"), &ScanError::TimedOut);
    match failure(&report, "refuse.clap") {
        ScanError::Refused(why) => assert!(why.contains("initialise"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        failure(&report, "text.clap"),
        ScanError::Refused(_)
    ));
    assert!(matches!(
        failure(&report, "empty.lv2"),
        ScanError::Refused(_)
    ));
    assert_eq!((report.scanned, report.cached), (8, 0));
    // The hanging plugin was given its time and no more, while the others
    // were scanned beside it.
    assert!(took >= TIMEOUT, "{took:?}");
    assert!(took < TIMEOUT * 4, "{took:?}");

    // The same files again: answered from the cache, nothing is started,
    // and the plugin that hung is not waited for again.
    let started = Instant::now();
    let again = scanner().scan(&candidates, &mut cache);
    assert!(started.elapsed() < TIMEOUT, "{:?}", started.elapsed());
    assert_eq!((again.scanned, again.cached), (0, 8));
    assert_eq!(names(&again), names(&report));
    assert_eq!(again.failures, report.failures);

    // A file that changes is scanned again: the plugin that refused is
    // replaced by one that works.
    let refused = root.join("refuse.clap");
    std::fs::copy(common::built_library("mangue_duck"), &refused).unwrap();
    let changed = scanner().scan(&candidates, &mut cache);
    assert_eq!((changed.scanned, changed.cached), (1, 7));
    assert!(
        changed
            .plugins
            .iter()
            .any(|plugin| plugin.path == refused && plugin.entry.name == "Mangue Duck")
    );
    assert_eq!(changed.failures.len(), 4);

    // The cache survives a file, and a damaged file is an empty cache.
    let file = root.join("scan.cache");
    cache.save(&file).unwrap();
    assert_eq!(ScanCache::load(&file), cache);
    std::fs::write(&file, b"\xff\xfe garbage").unwrap();
    assert!(ScanCache::load(&file).is_empty());

    // Forgetting a file has it scanned again.
    assert!(cache.forget(&root.join("crash.clap")));
    let retried = scanner().scan(&candidates, &mut cache);
    assert_eq!((retried.scanned, retried.cached), (1, 7));
}

#[test]
fn a_scanner_that_cannot_start_is_not_the_plugins_fault() {
    let root = plugin_folder("no-scanner");
    let candidates = find(&[root.join("Mangue Duck.clap")], &ALL);
    let mut cache = ScanCache::new();
    let report = Scanner::new(root.join("no-such-scanner")).scan(&candidates, &mut cache);
    assert!(matches!(report.failures[0].error, ScanError::Unreadable(_)));
    // Nothing is kept, so the next scan tries again.
    assert!(cache.is_empty());
}

#[test]
fn a_fingerprint_changes_with_the_file() {
    let root = common::scratch_folder("scan-fingerprint");
    let file = root.join("a.clap");
    std::fs::write(&file, b"one").unwrap();
    let first = Fingerprint::of(&file).unwrap();
    assert_eq!((first.size, first.files), (3, 1));
    std::fs::write(&file, b"three").unwrap();
    assert_ne!(Fingerprint::of(&file).unwrap(), first);
    let bundle = root.join("b.lv2");
    std::fs::create_dir_all(bundle.join("sub")).unwrap();
    std::fs::write(bundle.join("manifest.ttl"), b"12").unwrap();
    std::fs::write(bundle.join("sub").join("x"), b"345").unwrap();
    let print = Fingerprint::of(&bundle).unwrap();
    assert_eq!((print.size, print.files), (5, 2));
    assert_eq!(Fingerprint::of(&root.join("nothing")), None);
}
