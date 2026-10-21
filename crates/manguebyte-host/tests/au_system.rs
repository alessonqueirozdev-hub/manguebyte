//! Hosts the framework's example Audio Units as a Mac's hosts find them:
//! installed in `~/Library/Audio/Plug-Ins/Components`, found in the
//! system's registry with `AudioComponentFindNext`, made with
//! `AudioComponentInstanceNew` and driven through AudioToolbox. Hosted
//! that way they have to sound the same, to the bit, as the same plugins
//! driven with no host, and the scanner has to list their bundles, and
//! report a unit that crashes or hangs while it is made.
//!
//! macOS only, and ignored unless asked for: the units have to be
//! installed first, which the "Audio Unit validation (macOS)" job of
//! continuous integration does (`cargo mangue install --format au
//! --release`) before it runs this file with `--release --ignored`, so
//! that the plugins linked in here are built as the installed ones are.
#![cfg(all(feature = "au", target_os = "macos", not(miri)))]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use mangue_pluck::Pluck;
use manguebyte_core::{AudioBuffer, ParamId};
use manguebyte_dsp::event::{EventKind, NoteRef};
use manguebyte_host::any::{Format, Library};
use manguebyte_host::au::{AuLibrary, AuPlugin, ComponentCodes, components};
use manguebyte_host::scan::{Candidate, ScanCache, ScanError, Scanner, find, standard_paths};
use manguebyte_plugin::{Harness, Plugin};

use common::{as_au_delivers, assert_same_bits, plain_at as plain, render};

const RATE: f64 = 48_000.0;

fn codes_of(info: &manguebyte_plugin::PluginInfo) -> ComponentCodes {
    let [kind, subtype, manufacturer] = manguebyte_au::codes(info);
    ComponentCodes::new(kind, subtype, manufacturer)
}

fn installed(name: &str) -> PathBuf {
    let home = std::env::var_os("HOME").expect("a home folder");
    Path::new(&home)
        .join("Library/Audio/Plug-Ins/Components")
        .join(format!("{name}.component"))
}

fn system() -> AuLibrary {
    // SAFETY: the registered components this file creates are the
    // framework's own example units, installed by continuous integration,
    // whose code is the framework's AU wrapper.
    unsafe { AuLibrary::system() }.expect("AudioToolbox lists the components")
}

fn finish(plugin: &mut AuPlugin, processor: impl manguebyte_dsp::Processor<f32>) {
    drop(processor);
    plugin.deactivate().expect("it deactivates");
    assert_eq!(plugin.failed_blocks(), 0);
    assert_eq!(plugin.lost_events(), 0);
}

#[test]
#[ignore = "needs the example units installed, as the macOS AU job does"]
fn the_registry_lists_the_installed_units() {
    let listed = components();
    assert!(
        listed.iter().any(|unit| unit.vendor == "Apple"),
        "Apple's own units are registered"
    );
    for info in [&Pluck::INFO, &mangue_duck_info()] {
        let unit = listed
            .iter()
            .find(|unit| unit.codes == codes_of(info))
            .unwrap_or_else(|| panic!("{} is registered", info.name));
        assert_eq!(unit.name, info.name);
        assert_eq!(unit.vendor, info.vendor);
        assert_eq!(
            unit.version,
            manguebyte_au::version_number(info.version),
            "{}",
            info.name
        );
    }
    let library = system();
    assert!(
        library
            .components()
            .iter()
            .all(|unit| unit.codes.is_processor()),
        "no output units or converters"
    );
}

/// Mangue Duck's description, without linking its library into this test,
/// which already links Mangue Pluck's: the two export the same symbols.
fn mangue_duck_info() -> manguebyte_plugin::PluginInfo {
    manguebyte_plugin::PluginInfo {
        id: "org.manguebyte.duck",
        name: "Mangue Duck",
        vendor: "Manguebyte",
        version: Pluck::INFO.version,
        description: "",
        kind: manguebyte_plugin::PluginKind::Effect,
    }
}

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const RESONANCE: ParamId = ParamId::from_name("resonance");
const LEVEL: ParamId = ParamId::from_name("level");

fn song() -> Vec<(u64, EventKind)> {
    let params = Pluck::new().params();
    let value = |param, position| EventKind::ParamValue {
        param,
        value: plain(&*params, param, position),
    };
    let low = NoteRef::new(0, 45);
    let fifth = NoteRef::new(0, 52);
    vec![
        (
            100,
            EventKind::NoteOn {
                note: low,
                velocity: 0.8,
            },
        ),
        (2_500, value(RESONANCE, 0.6)),
        (
            4_000,
            EventKind::NoteOn {
                note: fifth,
                velocity: 0.7,
            },
        ),
        (4_000, value(CUTOFF, 0.4)),
        (
            9_100,
            EventKind::NoteOff {
                note: low,
                velocity: 0.3,
            },
        ),
        (12_000, value(LEVEL, 0.9)),
        (
            13_000,
            EventKind::NoteOff {
                note: fifth,
                velocity: 0.0,
            },
        ),
    ]
}

#[test]
#[ignore = "needs the example units installed, as the macOS AU job does"]
fn the_installed_instrument_sounds_as_it_does_with_no_host() {
    let library = system();
    let frames = 24_000;
    let schedule = song();
    let cases: [(usize, &[usize]); 3] = [
        (512, &[512]),
        (256, &[1, 255, 100, 3]),
        (2_048, &[2_048, 7, 1_000]),
    ];
    for (max_block, pattern) in cases {
        let what = format!("blocks of {pattern:?}, at most {max_block}");
        let mut harness = Harness::<Pluck>::new(RATE, max_block).with_block_pattern(pattern);
        for &(frame, kind) in &schedule {
            harness.schedule(frame, as_au_delivers(kind));
        }
        let expected = harness.render(frames);
        let mut plugin = library.create(codes_of(&Pluck::INFO)).unwrap();
        assert!(plugin.takes_notes());
        let mut processor = plugin.activate(RATE, max_block).unwrap();
        let mut audio = AudioBuffer::<f32>::new(2, frames);
        let rendered = render(
            &mut processor,
            RATE,
            &schedule,
            pattern,
            max_block,
            &mut audio,
            None,
        );
        finish(&mut plugin, processor);
        assert_eq!(plugin.undeliverable_events(), 0);
        assert_same_bits(&audio, &expected, &what);
        assert_eq!(rendered.heap_calls, 0, "{what}: the host used the heap");
    }
}

/// The effect, from its own library: the plugin with no host is the one
/// cargo built for this test, which is not linked in (see
/// [`mangue_duck_info`]), so it is driven through its CLAP entry, which the
/// CLAP tests hold against the plugin itself.
#[test]
#[ignore = "needs the example units installed, as the macOS AU job does"]
fn the_installed_effect_sounds_as_its_clap_build_does() {
    use manguebyte_host::clap::ClapLibrary;
    let frames = 24_000;
    let mut input = AudioBuffer::<f32>::new(2, frames);
    let mut key = AudioBuffer::<f32>::new(2, frames);
    for frame in 0..frames {
        let phase = frame as f32 * 0.05;
        input.channel_mut(0)[frame] = 0.5 * phase.sin();
        input.channel_mut(1)[frame] = 0.5 * (phase * 1.2).sin();
        let on = (frame / 3_000) % 2 == 0;
        key.channel_mut(0)[frame] = if on { 0.8 } else { 0.0 };
        key.channel_mut(1)[frame] = if on { 0.4 } else { 0.0 };
    }
    // SAFETY: the example plugin built from this workspace, whose
    // entry point is the framework's own CLAP wrapper.
    let clap = unsafe { ClapLibrary::load(common::built_library("mangue_duck")) }.unwrap();
    let mut reference = clap.create("org.manguebyte.duck").unwrap();
    let mut processor = reference.activate(RATE, 256).unwrap();
    let mut expected = input.clone();
    render(
        &mut processor,
        RATE,
        &[],
        &[256],
        256,
        &mut expected,
        Some(&key),
    );
    drop(processor);
    reference.deactivate().unwrap();
    assert!(expected != input, "nothing was ducked");

    let library = system();
    let mut plugin = library.create(codes_of(&mangue_duck_info())).unwrap();
    assert!(!plugin.takes_notes() && plugin.can_bypass());
    assert_eq!(plugin.audio_inputs().len(), 2);
    let mut processor = plugin.activate(RATE, 256).unwrap();
    let mut audio = input.clone();
    let rendered = render(
        &mut processor,
        RATE,
        &[],
        &[1, 255, 77],
        256,
        &mut audio,
        Some(&key),
    );
    assert_eq!(rendered.heap_calls, 0);
    assert_same_bits(&audio, &expected, "the effect through AudioToolbox");
    plugin.set_bypass(true).unwrap();
    let mut audio = input.clone();
    render(
        &mut processor,
        RATE,
        &[],
        &[256],
        256,
        &mut audio,
        Some(&key),
    );
    assert_same_bits(&audio, &input, "bypassed");
    finish(&mut plugin, processor);
}

#[test]
#[ignore = "needs the example units installed, as the macOS AU job does"]
fn a_bundle_loads_through_the_one_interface_and_its_state_travels() {
    let path = installed("Mangue Pluck");
    // SAFETY: the installed example unit, as above.
    let library = unsafe { Library::load(&path) }.expect("the bundle loads");
    assert_eq!(library.format(), Format::Au);
    let [entry] = library.plugins() else {
        panic!("one unit");
    };
    assert_eq!(entry.id, codes_of(&Pluck::INFO).to_string());
    assert_eq!(entry.name, "Mangue Pluck");
    assert_eq!(entry.categories, ["Synthesizer"]);
    let mut plugin = library.create(&entry.id).unwrap();
    let level = plugin
        .params()
        .into_iter()
        .find(|param| param.name == "Level")
        .unwrap();
    assert_eq!(level.unit, "dB");
    plugin.set_param(level.id, -12.0).unwrap();
    let state = plugin.save_state().unwrap();
    let text = String::from_utf8(state.clone()).unwrap();
    assert!(text.contains("<key>data</key>"), "{text}");
    let mut again = library.create(&entry.id).unwrap();
    again.load_state(&state).unwrap();
    assert_eq!(again.param_value(level.id), Some(-12.0));
}

#[test]
#[ignore = "needs the example units installed, as the macOS AU job does"]
fn the_scanner_lists_the_installed_bundles() {
    let candidates = find(&standard_paths(Format::Au), &[Format::Au]);
    for name in ["Mangue Pluck", "Mangue Duck"] {
        assert!(
            candidates.iter().any(|found| found.path == installed(name)),
            "{name} is found by its name"
        );
    }
    let scanner = Scanner::new(env!("CARGO_BIN_EXE_manguebyte-scan"));
    let mut cache = ScanCache::default();
    let mine: Vec<Candidate> = candidates
        .into_iter()
        .filter(|found| {
            found
                .path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("Mangue "))
        })
        .collect();
    let report = scanner.scan(&mine, &mut cache);
    assert!(report.failures.is_empty(), "{:?}", report.failures);
    let mut names: Vec<&str> = report
        .plugins
        .iter()
        .map(|plugin| plugin.entry.name.as_str())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["Mangue Duck", "Mangue Pluck"]);
    // The second scan is the cache's.
    let again = scanner.scan(&mine, &mut cache);
    assert_eq!((again.cached, again.scanned), (mine.len(), 0));
}

// --- Units that crash and hang --------------------------------------------------

/// A component bundle whose factory does what `body` says, installed where
/// the registry looks, and taken away again when this value goes.
struct Fake {
    bundle: PathBuf,
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.bundle);
    }
}

fn rustc() -> PathBuf {
    if let Some(rustc) = std::env::var_os("RUSTC") {
        return rustc.into();
    }
    if let Some(cargo) = std::env::var_os("CARGO") {
        let beside = Path::new(&cargo).with_file_name("rustc");
        if beside.is_file() {
            return beside;
        }
    }
    "rustc".into()
}

fn install_fake(name: &str, subtype: &str, body: &str) -> Fake {
    let bundle = installed(name);
    let _ = std::fs::remove_dir_all(&bundle);
    let executable = bundle.join("Contents/MacOS").join(name);
    std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
    let source = common::scratch_folder(&format!("au-fake-{name}")).join("fake.rs");
    std::fs::write(
        &source,
        format!(
            "#[no_mangle]\npub extern \"C\" fn FakeFactory(_d: *const u8) -> *mut u8 {{ {body} }}\n"
        ),
    )
    .unwrap();
    let status = Command::new(rustc())
        .args(["--edition", "2021", "--crate-type", "cdylib", "-o"])
        .arg(&executable)
        .arg(&source)
        .status()
        .expect("rustc runs");
    assert!(status.success(), "the fake unit {name} builds");
    std::fs::write(
        bundle.join("Contents/Info.plist"),
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<plist version=\"1.0\"><dict>\
             <key>CFBundleExecutable</key><string>{name}</string>\
             <key>CFBundleIdentifier</key><string>org.manguebyte.test.{subtype}</string>\
             <key>CFBundlePackageType</key><string>BNDL</string>\
             <key>CFBundleVersion</key><string>1.0</string>\
             <key>AudioComponents</key><array><dict>\
             <key>type</key><string>aufx</string>\
             <key>subtype</key><string>{subtype}</string>\
             <key>manufacturer</key><string>Mbtx</string>\
             <key>name</key><string>Manguebyte Test: {name}</string>\
             <key>version</key><integer>65536</integer>\
             <key>factoryFunction</key><string>FakeFactory</string>\
             </dict></array></dict></plist>\n"
        ),
    )
    .unwrap();
    Fake { bundle }
}

/// This test leaves AudioToolbox's registry to notice two bundles written
/// while the system runs, which is up to the registry: it is the one part
/// of the AU host's checks that has never run anywhere but in CI.
#[test]
#[ignore = "installs fake units where the system looks; run by the macOS AU job"]
fn the_scanner_reports_a_unit_that_crashes_or_hangs() {
    let crash = install_fake("MangueCrash", "crsh", "std::process::abort()");
    let hang = install_fake(
        "MangueHang",
        "hang",
        "loop { std::thread::sleep(std::time::Duration::from_secs(1)); }",
    );
    // The registry is made to look again, as the job does before auval.
    let _ = Command::new("killall")
        .args(["-9", "AudioComponentRegistrar"])
        .status();
    let wanted = [
        ComponentCodes::new(*b"aufx", *b"crsh", *b"Mbtx"),
        ComponentCodes::new(*b"aufx", *b"hang", *b"Mbtx"),
    ];
    let started = Instant::now();
    while !wanted
        .iter()
        .all(|codes| components().iter().any(|unit| unit.codes == *codes))
    {
        assert!(
            started.elapsed() < Duration::from_secs(60),
            "the registry did not list the fake units"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
    let scanner =
        Scanner::new(env!("CARGO_BIN_EXE_manguebyte-scan")).with_timeout(Duration::from_secs(8));
    let candidates = [
        Candidate {
            path: crash.bundle.clone(),
            format: Format::Au,
        },
        Candidate {
            path: hang.bundle.clone(),
            format: Format::Au,
        },
    ];
    let report = scanner.scan(&candidates, &mut ScanCache::default());
    assert!(report.plugins.is_empty(), "{:?}", report.plugins);
    let error = |bundle: &Path| {
        report
            .failures
            .iter()
            .find(|failure| failure.path == bundle)
            .map(|failure| failure.error.clone())
            .unwrap_or_else(|| panic!("{} is reported", bundle.display()))
    };
    assert!(
        matches!(error(&crash.bundle), ScanError::Crashed { .. }),
        "{:?}",
        error(&crash.bundle)
    );
    assert_eq!(error(&hang.bundle), ScanError::TimedOut);
}
