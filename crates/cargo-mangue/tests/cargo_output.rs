//! Reading what cargo prints: the description of a workspace and the
//! report of a build.
//!
//! The fixtures are cut down from the real output of cargo 1.99 on this
//! repository, on Windows. The Linux and macOS build reports are the same
//! lines with the file names those systems produce. What matters most is
//! at the end of the file: output that is cut short or damaged gives an
//! error, never a panic.

use std::path::{Path, PathBuf};

use cargo_mangue::cargo::{
    BuildRequest, BuiltFor, LibraryScan, Metadata, Package, Profile, Settings, build_args,
    built_for, find_library, helper_target_dir, parse_metadata, select_package,
};
use cargo_mangue::platform::Os;

/// `cargo metadata --format-version 1 --no-deps`, reduced to four packages:
/// a library, this tool (a library and a program), and two plugins, the
/// second with settings of its own.
const METADATA: &str = r#"{"packages":[
{"name":"manguebyte-core","version":"0.1.0","id":"path+file:///C:/manguebyte/crates/manguebyte-core#0.1.0","license":"MIT OR Apache-2.0","manifest_path":"C:\\manguebyte\\crates\\manguebyte-core\\Cargo.toml","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"manguebyte_core","src_path":"C:\\manguebyte\\crates\\manguebyte-core\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true}],"features":{},"metadata":null},
{"name":"cargo-mangue","version":"0.1.0","id":"path+file:///C:/manguebyte/crates/cargo-mangue#0.1.0","manifest_path":"C:\\manguebyte\\crates\\cargo-mangue\\Cargo.toml","targets":[{"kind":["lib"],"crate_types":["lib"],"name":"cargo_mangue","src_path":"C:\\manguebyte\\crates\\cargo-mangue\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},{"kind":["bin"],"crate_types":["bin"],"name":"cargo-mangue","src_path":"C:\\manguebyte\\crates\\cargo-mangue\\src\\main.rs","edition":"2024","doc":false,"doctest":false,"test":false}],"metadata":null},
{"name":"mangue-duck","version":"0.1.0","id":"path+file:///C:/manguebyte/plugins/duck#mangue-duck@0.1.0","manifest_path":"C:\\manguebyte\\plugins\\duck\\Cargo.toml","targets":[{"kind":["cdylib","rlib"],"crate_types":["cdylib","rlib"],"name":"mangue_duck","src_path":"C:\\manguebyte\\plugins\\duck\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true}],"metadata":null},
{"name":"mangue-pluck","version":"0.1.0","id":"path+file:///C:/manguebyte/plugins/pluck#mangue-pluck@0.1.0","manifest_path":"C:\\manguebyte\\plugins\\pluck\\Cargo.toml","targets":[{"kind":["cdylib","rlib"],"crate_types":["cdylib","rlib"],"name":"mangue_pluck","src_path":"C:\\manguebyte\\plugins\\pluck\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},{"kind":["example"],"crate_types":["bin"],"name":"play","src_path":"C:\\manguebyte\\plugins\\pluck\\examples\\play.rs","edition":"2024","required-features":["standalone"],"doc":false,"doctest":false,"test":false}],"metadata":{"manguebyte":{"name":"Mangue Pluck","id":"org.manguebyte.pluck"}}}
],"workspace_members":["path+file:///C:/manguebyte/crates/manguebyte-core#0.1.0"],"resolve":null,"target_directory":"C:\\manguebyte\\target","build_directory":"C:\\manguebyte\\target","version":1,"workspace_root":"C:\\manguebyte","metadata":null}"#;

/// `cargo build --message-format=json-render-diagnostics -p mangue-duck
/// --lib`: two of the libraries it depends on, the plugin, and the end.
const BUILD_WINDOWS: &str = r#"{"reason":"compiler-artifact","package_id":"path+file:///C:/manguebyte/crates/manguebyte-core#0.1.0","manifest_path":"C:\\manguebyte\\crates\\manguebyte-core\\Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"manguebyte_core","src_path":"C:\\manguebyte\\crates\\manguebyte-core\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["C:\\manguebyte\\target\\debug\\deps\\libmanguebyte_core-0a1b2c3d4e5f6a7b.rlib","C:\\manguebyte\\target\\debug\\deps\\libmanguebyte_core-0a1b2c3d4e5f6a7b.rmeta"],"executable":null,"fresh":true}
{"reason":"compiler-artifact","package_id":"registry+https://github.com/rust-lang/crates.io-index#clap-sys@0.5.0","manifest_path":"C:\\Users\\ana\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f\\clap-sys-0.5.0\\Cargo.toml","target":{"kind":["lib"],"crate_types":["lib"],"name":"clap_sys","src_path":"C:\\Users\\ana\\.cargo\\registry\\src\\index.crates.io-1949cf8c6b5b557f\\clap-sys-0.5.0\\src\\lib.rs","edition":"2021","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["C:\\manguebyte\\target\\debug\\deps\\libclap_sys-1122334455667788.rlib","C:\\manguebyte\\target\\debug\\deps\\libclap_sys-1122334455667788.rmeta"],"executable":null,"fresh":true}
{"reason":"compiler-artifact","package_id":"path+file:///C:/manguebyte/plugins/duck#mangue-duck@0.1.0","manifest_path":"C:\\manguebyte\\plugins\\duck\\Cargo.toml","target":{"kind":["cdylib","rlib"],"crate_types":["cdylib","rlib"],"name":"mangue_duck","src_path":"C:\\manguebyte\\plugins\\duck\\src\\lib.rs","edition":"2024","doc":true,"doctest":true,"test":true},"profile":{"opt_level":"0","debuginfo":2,"debug_assertions":true,"overflow_checks":true,"test":false},"features":[],"filenames":["C:\\manguebyte\\target\\debug\\mangue_duck.dll","C:\\manguebyte\\target\\debug\\mangue_duck.dll.lib","C:\\manguebyte\\target\\debug\\mangue_duck.dll.exp","C:\\manguebyte\\target\\debug\\mangue_duck.pdb","C:\\manguebyte\\target\\debug\\libmangue_duck.rlib"],"executable":null,"fresh":false}
{"reason":"build-finished","success":true}
"#;

const WINDOWS_LIBRARY: &str = r"C:\manguebyte\target\debug\mangue_duck.dll";

/// The plugin's line of the same build on another system.
fn build_elsewhere(files: &[&str]) -> String {
    let files: Vec<String> = files.iter().map(|file| format!("\"{file}\"")).collect();
    format!(
        "{{\"reason\":\"compiler-artifact\",\"package_id\":\"path+file:///C:/manguebyte/plugins/duck#mangue-duck@0.1.0\",\
         \"manifest_path\":\"/elsewhere/Cargo.toml\",\
         \"target\":{{\"kind\":[\"cdylib\",\"rlib\"],\"crate_types\":[\"cdylib\",\"rlib\"],\"name\":\"mangue_duck\"}},\
         \"filenames\":[{}],\"executable\":null,\"fresh\":false}}\n\
         {{\"reason\":\"build-finished\",\"success\":true}}\n",
        files.join(",")
    )
}

fn metadata() -> Metadata {
    parse_metadata(METADATA).unwrap()
}

fn duck() -> Package {
    metadata()
        .packages
        .into_iter()
        .find(|package| package.name == "mangue-duck")
        .unwrap()
}

#[test]
fn metadata_says_which_packages_build_a_plugin() {
    let metadata = metadata();
    assert_eq!(
        metadata.target_directory,
        Path::new(r"C:\manguebyte\target")
    );
    let summary: Vec<(&str, bool)> = metadata
        .packages
        .iter()
        .map(|package| (package.name.as_str(), package.builds_plugin))
        .collect();
    assert_eq!(
        summary,
        [
            ("manguebyte-core", false),
            ("cargo-mangue", false),
            ("mangue-duck", true),
            ("mangue-pluck", true),
        ]
    );
    let duck = &metadata.packages[2];
    assert_eq!(duck.version, "0.1.0");
    assert_eq!(
        duck.id,
        "path+file:///C:/manguebyte/plugins/duck#mangue-duck@0.1.0"
    );
    assert_eq!(
        duck.manifest_path,
        Path::new(r"C:\manguebyte\plugins\duck\Cargo.toml")
    );
}

#[test]
fn the_plugin_name_and_identifier_default_from_the_manifest() {
    let metadata = metadata();
    // No settings: the package name, and the identifier the scripts write.
    let duck = &metadata.packages[2];
    assert_eq!(duck.settings, Settings::default());
    assert_eq!(duck.plugin_name(), "mangue-duck");
    assert_eq!(duck.identifier(), "org.manguebyte.mangue-duck");
    // [package.metadata.manguebyte] with name and id.
    let pluck = &metadata.packages[3];
    assert_eq!(pluck.plugin_name(), "Mangue Pluck");
    assert_eq!(pluck.identifier(), "org.manguebyte.pluck");

    // An underscore cannot be in a bundle identifier.
    let mut snake = duck.clone();
    snake.name = "my_gain".to_owned();
    assert_eq!(snake.identifier(), "org.manguebyte.my-gain");
}

/// The same workspace as cargo describes it on Linux and macOS, in
/// `/work`. Folders are taken apart in these tests, and a path with
/// backslashes is one long name anywhere but on Windows, while one with
/// slashes is a path everywhere.
fn metadata_with_slashes() -> Metadata {
    let json = METADATA
        .replace(r"C:\\manguebyte", "/work")
        .replace(r"\\", "/");
    parse_metadata(&json).unwrap()
}

#[test]
fn a_package_is_chosen_by_name_by_folder_or_by_being_the_only_plugin() {
    let metadata = metadata_with_slashes();
    assert_eq!(
        metadata.packages[3].manifest_path,
        Path::new("/work/plugins/pluck/Cargo.toml")
    );
    let chosen = |wanted: Option<&str>, dir: Option<&str>| {
        select_package(&metadata, wanted, dir.map(Path::new)).map(|package| package.name.as_str())
    };

    assert_eq!(chosen(Some("mangue-pluck"), None), Ok("mangue-pluck"));
    // A name wins over the folder.
    assert_eq!(
        chosen(Some("mangue-duck"), Some("/work/plugins/pluck")),
        Ok("mangue-duck")
    );
    // In a plugin's folder, or anywhere below it, it is that plugin.
    assert_eq!(
        chosen(None, Some("/work/plugins/pluck")),
        Ok("mangue-pluck")
    );
    assert_eq!(
        chosen(None, Some("/work/plugins/duck/src")),
        Ok("mangue-duck")
    );
    // A folder whose name merely starts the same is not below it.
    assert!(chosen(None, Some("/work/plugins/duckling")).is_err());

    // Elsewhere, two plugins are a question, and the answer lists both.
    for dir in [
        Some("/work"),
        Some("/work/crates/cargo-mangue"),
        Some(""),
        None,
    ] {
        let error = chosen(None, dir).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(
            error.message().contains("mangue-duck, mangue-pluck"),
            "{error}"
        );
        assert!(error.message().contains("-p <package>"), "{error}");
    }

    let unknown = chosen(Some("mangue-plcuk"), None).unwrap_err();
    assert_eq!(unknown.exit_code(), 2);
    assert!(
        unknown.message().contains("mangue-duck, mangue-pluck"),
        "{unknown}"
    );

    let not_a_plugin = chosen(Some("manguebyte-core"), None).unwrap_err();
    assert_eq!(not_a_plugin.exit_code(), 1);
    assert!(not_a_plugin.message().contains("cdylib"), "{not_a_plugin}");
}

#[test]
fn the_only_plugin_of_a_workspace_needs_no_naming() {
    let mut metadata = metadata_with_slashes();
    metadata
        .packages
        .retain(|package| package.name != "mangue-pluck");
    for dir in [Some("/work"), Some("/somewhere/else"), None] {
        let chosen = select_package(&metadata, None, dir.map(Path::new)).unwrap();
        assert_eq!(chosen.name, "mangue-duck");
    }

    metadata.packages.retain(|package| !package.builds_plugin);
    let none = select_package(&metadata, None, None).unwrap_err();
    assert_eq!(none.exit_code(), 1);
    assert!(none.message().contains("cargo mangue new"), "{none}");
    let none = select_package(&metadata, Some("x"), None).unwrap_err();
    assert!(none.message().contains("cargo mangue new"), "{none}");
}

#[test]
fn the_library_is_taken_from_what_cargo_reports() {
    let found = find_library(BUILD_WINDOWS, &duck(), Os::Windows).unwrap();
    assert_eq!(found, Path::new(WINDOWS_LIBRARY));

    let linux = build_elsewhere(&[
        "/work/target/release/libmangue_duck.so",
        "/work/target/release/libmangue_duck.rlib",
    ]);
    assert_eq!(
        find_library(&linux, &duck(), Os::Linux).unwrap(),
        Path::new("/work/target/release/libmangue_duck.so")
    );

    let macos = build_elsewhere(&[
        "/work/target/aarch64-apple-darwin/release/libmangue_duck.dylib",
        "/work/target/aarch64-apple-darwin/release/libmangue_duck.rlib",
    ]);
    assert_eq!(
        find_library(&macos, &duck(), Os::Macos).unwrap(),
        Path::new("/work/target/aarch64-apple-darwin/release/libmangue_duck.dylib")
    );
}

#[test]
fn where_cargo_put_a_library_says_what_it_was_built_for() {
    let target = Path::new("work").join("target");
    let file = "libmangue_duck.so";
    // Straight in a profile's folder, whatever the profile is called: no
    // target was named, and the build is for the compiler's host.
    for profile in ["debug", "release", "fast", "x86_64"] {
        assert_eq!(
            built_for(&target.join(profile).join(file), &target),
            BuiltFor::Host,
            "{profile}"
        );
    }
    // In a target's folder: for that target, whatever it is.
    for triple in [
        "aarch64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
        "wasm32-unknown-unknown",
        "my-own-target",
    ] {
        let library = target.join(triple).join("release").join(file);
        assert_eq!(
            built_for(&library, &target),
            BuiltFor::Target(triple),
            "{triple}"
        );
    }
    // A library left in a profile's `deps` folder, as those of dependencies
    // are, counts as being in the profile's folder.
    let in_deps = target.join("debug").join("deps").join(file);
    assert_eq!(built_for(&in_deps, &target), BuiltFor::Host);
    let cross = target.join("x86_64-unknown-linux-gnu").join("debug");
    assert_eq!(
        built_for(&cross.join("deps").join(file), &target),
        BuiltFor::Target("x86_64-unknown-linux-gnu")
    );
    // Anywhere else says nothing: outside the target directory, in it with
    // no profile's folder, deeper than cargo's layout goes, or reached
    // through a step back.
    let elsewhere = Path::new("work").join("other").join("debug").join(file);
    let deeper = target.join("a").join("b").join("debug").join(file);
    let stepping = target.join("..").join("debug").join(file);
    for library in [
        elsewhere,
        target.join(file),
        target.join("deps").join(file),
        target.clone(),
        deeper.clone(),
        deeper.parent().unwrap().join("deps").join(file),
        stepping,
        PathBuf::new(),
    ] {
        assert_eq!(
            built_for(&library, &target),
            BuiltFor::Unknown,
            "{}",
            library.display()
        );
    }
    // An absolute library is not inside a relative folder of the same name.
    let absolute = std::path::absolute(target.join("debug").join(file)).unwrap();
    assert_eq!(built_for(&absolute, &target), BuiltFor::Unknown);
    assert_eq!(
        built_for(&absolute, &std::path::absolute(&target).unwrap()),
        BuiltFor::Host
    );
}

/// The paths of the fixtures above, which are real output of cargo on
/// Windows, and the same build for a named target.
#[cfg(windows)]
#[test]
fn the_paths_cargo_reports_on_windows_are_read_as_they_are() {
    let target_directory = metadata().target_directory;
    let library = find_library(BUILD_WINDOWS, &duck(), Os::Windows).unwrap();
    assert_eq!(built_for(&library, &target_directory), BuiltFor::Host);
    let cross = Path::new(r"C:\manguebyte\target\aarch64-pc-windows-msvc\debug\mangue_duck.dll");
    assert_eq!(
        built_for(cross, &target_directory),
        BuiltFor::Target("aarch64-pc-windows-msvc")
    );
    // A dependency's library, which cargo reports in `deps`.
    let dependency =
        Path::new(r"C:\manguebyte\target\debug\deps\libclap_sys-1122334455667788.rlib");
    assert!(BUILD_WINDOWS.contains(&dependency.to_str().unwrap().replace('\\', "\\\\")));
    assert_eq!(built_for(dependency, &target_directory), BuiltFor::Host);
}

#[test]
fn a_package_is_recognised_by_its_manifest_when_cargo_words_its_id_differently() {
    // Older cargo wrote ids as "name version (path+file://...)".
    let mut duck = duck();
    duck.id = "mangue-duck 0.1.0 (path+file:///C:/manguebyte/plugins/duck)".to_owned();
    let found = find_library(BUILD_WINDOWS, &duck, Os::Windows).unwrap();
    assert_eq!(found, Path::new(WINDOWS_LIBRARY));

    // Neither the id nor the manifest: somebody else's library.
    duck.manifest_path = PathBuf::from(r"C:\elsewhere\Cargo.toml");
    assert!(find_library(BUILD_WINDOWS, &duck, Os::Windows).is_err());
}

#[test]
fn a_library_for_another_system_is_an_error_that_points_at_target() {
    let error = find_library(BUILD_WINDOWS, &duck(), Os::Linux).unwrap_err();
    assert_eq!(error.exit_code(), 1);
    assert!(error.message().contains("mangue_duck.dll"), "{error}");
    assert!(
        error.message().contains("not a library for Linux"),
        "{error}"
    );
    assert!(error.message().contains("--target"), "{error}");
}

#[test]
fn a_build_that_reports_no_plugin_library_is_an_error_that_says_why() {
    let only_libraries: String = BUILD_WINDOWS
        .lines()
        .filter(|line| !line.contains("mangue_duck"))
        .map(|line| format!("{line}\n"))
        .collect();
    for output in [only_libraries.as_str(), ""] {
        let error = find_library(output, &duck(), Os::Windows).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert!(error.message().contains("cdylib"), "{error}");
    }
}

#[test]
fn lines_that_are_not_cargo_messages_are_passed_over() {
    let noisy = format!("warning: something\n\n   Compiling x\n[1/3]\n{BUILD_WINDOWS}\ndone\n");
    let found = find_library(&noisy, &duck(), Os::Windows).unwrap();
    assert_eq!(found, Path::new(WINDOWS_LIBRARY));

    // Fed line by line, with the line endings Windows adds.
    let duck = duck();
    let mut scan = LibraryScan::new(&duck, Os::Windows);
    for line in BUILD_WINDOWS.lines() {
        scan.feed(&format!("{line}\r")).unwrap();
    }
    assert_eq!(scan.finish().unwrap(), Path::new(WINDOWS_LIBRARY));
}

#[test]
fn the_build_command_asks_for_the_library_and_a_report() {
    let text = |args: Vec<std::ffi::OsString>| -> Vec<String> {
        args.into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    };
    let plain = BuildRequest::default();
    assert_eq!(
        text(build_args("mangue-duck", &plain, None)),
        [
            "build",
            "--message-format=json-render-diagnostics",
            "--package",
            "mangue-duck",
            "--lib",
        ]
    );

    let release = BuildRequest {
        profile: Profile::Release,
        ..BuildRequest::default()
    };
    let args = text(build_args("x", &release, Some("aarch64-apple-darwin")));
    assert_eq!(args[5..], ["--release", "--target", "aarch64-apple-darwin"]);

    let everything = BuildRequest {
        profile: Profile::Named("fast".to_owned()),
        manifest_path: Some(PathBuf::from("plugins/duck/Cargo.toml")),
        features: vec!["a,b".to_owned(), "c".to_owned()],
        all_features: true,
        no_default_features: true,
    };
    assert_eq!(
        text(build_args("x", &everything, None))[5..],
        [
            "--profile",
            "fast",
            "--features",
            "a,b",
            "--features",
            "c",
            "--all-features",
            "--no-default-features",
            "--manifest-path",
            "plugins/duck/Cargo.toml",
        ]
    );
}

#[test]
fn metadata_cut_short_anywhere_is_an_error() {
    assert!(parse_metadata(METADATA).is_ok());
    assert!(METADATA.is_ascii() && METADATA.len() > 2_000);
    for cut in 0..METADATA.len() {
        let error = parse_metadata(&METADATA[..cut]).unwrap_err();
        assert_eq!(error.exit_code(), 1);
        assert!(error.message().contains("cargo metadata"), "{error}");
    }
}

#[test]
fn metadata_of_the_wrong_shape_is_an_error() {
    for damaged in [
        "",
        "null",
        "[]",
        "{}",
        "42",
        "\"text\"",
        r#"{"packages":[]}"#,
        r#"{"target_directory":"t"}"#,
        r#"{"target_directory":7,"packages":[]}"#,
        r#"{"target_directory":"t","packages":{}}"#,
        r#"{"target_directory":"t","packages":[7]}"#,
        r#"{"target_directory":"t","packages":[{}]}"#,
        r#"{"target_directory":"t","packages":[{"name":"x"}]}"#,
        r#"{"target_directory":"t","packages":[{"name":7,"version":"1","id":"i","manifest_path":"m"}]}"#,
        r#"{"target_directory":"t","packages":[{"name":"x","version":1,"id":"i","manifest_path":"m"}]}"#,
        r#"{"target_directory":"t","packages":[{"name":"x","version":"1","id":null,"manifest_path":"m"}]}"#,
        r#"{"target_directory":"t","packages":[{"name":"x","version":"1","id":"i","manifest_path":[]}]}"#,
        "{\"target_directory\":\"t\",\"packages\":[]}\0",
        "\u{feff}{}",
    ] {
        assert_eq!(
            parse_metadata(damaged).unwrap_err().exit_code(),
            1,
            "{damaged}"
        );
    }

    // Odd but harmless: no targets, or targets of the wrong shape, is a
    // package that builds no plugin.
    for targets in [
        "",
        r#""targets":7,"#,
        r#""targets":[7,{"crate_types":"cdylib"},{"crate_types":[1]}],"#,
    ] {
        let json = format!(
            r#"{{"target_directory":"t","packages":[{{{targets}"name":"x","version":"1","id":"i","manifest_path":"m"}}]}}"#
        );
        let metadata = parse_metadata(&json).unwrap();
        assert!(!metadata.packages[0].builds_plugin, "{targets}");
    }
}

#[test]
fn settings_of_the_wrong_type_are_an_error_that_names_the_key() {
    let with = |settings: &str| {
        parse_metadata(&format!(
            r#"{{"target_directory":"t","packages":[{{"name":"my-gain","version":"1","id":"i","manifest_path":"m","metadata":{settings}}}]}}"#
        ))
    };
    for (settings, word) in [
        (r#"{"manguebyte":{"name":7}}"#, "name"),
        (r#"{"manguebyte":{"name":["a"]}}"#, "name"),
        (r#"{"manguebyte":{"id":true}}"#, "id"),
        (r#"{"manguebyte":"Mangue"}"#, "table"),
        (r#"{"manguebyte":[]}"#, "table"),
    ] {
        let error = with(settings).unwrap_err();
        assert!(error.message().contains(word), "{error}");
        assert!(error.message().contains("my-gain"), "{error}");
        assert!(
            error.message().contains("[package.metadata.manguebyte]"),
            "{error}"
        );
    }
    // Nothing set, in every way cargo can say so, and keys of other tools.
    for settings in [
        "null",
        "{}",
        r#"{"manguebyte":{}}"#,
        r#"{"manguebyte":{"name":null}}"#,
        r#"{"docs":{"rs":true},"manguebyte":{"vendor":7}}"#,
        "7",
    ] {
        let metadata = with(settings).unwrap();
        assert_eq!(
            metadata.packages[0].settings,
            Settings::default(),
            "{settings}"
        );
    }
}

#[test]
fn a_build_report_cut_short_anywhere_is_never_a_panic_and_never_a_wrong_answer() {
    let duck = duck();
    let line_start = BUILD_WINDOWS.find(r#"{"reason":"compiler-artifact","package_id":"path+file:///C:/manguebyte/plugins/duck"#).unwrap();
    let line_end = line_start + BUILD_WINDOWS[line_start..].find('\n').unwrap();
    let mut found = 0;
    for cut in 0..=BUILD_WINDOWS.len() {
        match find_library(&BUILD_WINDOWS[..cut], &duck, Os::Windows) {
            Ok(library) => {
                // Only once the plugin's whole line is in.
                assert!(cut >= line_end, "a library from {cut} bytes");
                assert_eq!(library, Path::new(WINDOWS_LIBRARY));
                found += 1;
            }
            Err(error) => assert_eq!(error.exit_code(), 1),
        }
    }
    assert!(found > 0);
    // Every cut inside the plugin's own line fails, as damaged or as
    // missing.
    for cut in line_start..line_end {
        assert!(find_library(&BUILD_WINDOWS[..cut], &duck, Os::Windows).is_err());
    }
}

#[test]
fn a_damaged_build_report_is_an_error_or_is_passed_over_and_never_a_panic() {
    let duck = duck();
    // Wrong types where the plugin's line has values.
    let line = BUILD_WINDOWS
        .lines()
        .find(|line| line.contains("mangue_duck.dll"))
        .unwrap();
    for (from, to) in [
        (r#""reason":"compiler-artifact""#, r#""reason":7"#),
        (
            r#""reason":"compiler-artifact""#,
            r#""reason":["compiler-artifact"]"#,
        ),
        (
            r#""crate_types":["cdylib","rlib"]"#,
            r#""crate_types":"cdylib""#,
        ),
        (
            r#""crate_types":["cdylib","rlib"]"#,
            r#""crate_types":[7,null]"#,
        ),
        (r#""target":{"#, r#""target":null,"was":{"#),
        (r#""filenames":["#, r#""filenames":7,"were":["#),
        (r#""filenames":["#, r#""filenames":[7,null,{},["#),
    ] {
        assert!(line.contains(from), "{from}");
        let damaged = line.replace(from, to);
        let _ = find_library(&damaged, &duck, Os::Windows);
    }
    // A library with no usable name among the files is not found.
    let nameless = line.replace(r#""C:\\manguebyte\\target\\debug\\mangue_duck.dll","#, "7,");
    assert!(find_library(&nameless, &duck, Os::Windows).is_err());

    // Every byte of the line replaced in turn.
    for at in (0..line.len()).filter(|&at| line.is_char_boundary(at)) {
        for replacement in ["", "\"", "{", "}", "[", "\\", "\0", "é"] {
            let mut damaged = line.to_owned();
            damaged.replace_range(at..=at, replacement);
            let _ = find_library(&damaged, &duck, Os::Windows);
        }
    }

    // Things that are not cargo's at all.
    let deep = "[".repeat(200_000);
    let deep_object = "{\"a\":".repeat(100_000);
    for junk in [
        "{",
        "{}",
        "{\"reason\":",
        "{\"reason\":\"compiler-artifact\"}",
        "{\"reason\":\"compiler-artifact\",\"package_id\":null,\"target\":7,\"filenames\":null}",
        "\u{feff}{\"reason\":\"compiler-artifact\"}",
        deep.as_str(),
        deep_object.as_str(),
        "{\"reason\":\"\u{0}\"}",
    ] {
        let result = find_library(junk, &duck, Os::Windows);
        assert!(result.is_err(), "{:.40}", junk);
    }
}

/// The programs that write a bundle's metadata are workspaces of their own,
/// so they are built apart from the workspace's own builds, which they would
/// otherwise replace, and still under its target directory.
#[test]
fn the_metadata_programs_are_built_apart_from_the_workspace() {
    let target_directory = metadata().target_directory;
    let helpers = helper_target_dir(&target_directory);
    assert!(helpers.starts_with(&target_directory));
    assert_ne!(helpers, target_directory);
    for profile in ["debug", "release"] {
        assert!(!helpers.starts_with(target_directory.join(profile)));
    }
}
