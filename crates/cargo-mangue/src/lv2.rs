//! The Turtle files of an LV2 bundle.
//!
//! An LV2 host reads a bundle's `manifest.ttl`, and the plugin's own `.ttl`
//! it points to, before it loads the library. They list the plugin's ports
//! and parameters, which only the plugin's code knows:
//! `manguebyte_lv2::bundle_files` writes them from the plugin's type, and
//! `export_lv2!` puts a function that calls it, `__manguebyte_lv2_bundle_files`,
//! at the root of the plugin's crate.
//!
//! This tool does not know the plugin's type, so it writes a small program
//! that does: a package that depends on the plugin's crate (which needs the
//! `rlib` type for that) and writes the files that function gives into a
//! folder. The program is built and run with cargo, for this machine, with
//! the plugin build's profile and features, in
//! [`helper_target_dir`](crate::cargo::helper_target_dir), apart from the
//! workspace's own builds. It is a
//! workspace of its own, in `<target>/manguebyte/lv2/<package>/`, with a
//! copy of the plugin workspace's `Cargo.lock`, so that it builds the same
//! versions of every dependency.
//!
//! [`helper_manifest`], [`helper_features`] and [`read_turtle_files`] decide
//! everything; [`turtle_files`] does the work.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::cargo::{BuildRequest, Cargo, Metadata, Package};
use crate::error::Error;

/// The name of the program's package.
pub const HELPER_PACKAGE: &str = "mangue-lv2-turtle";

/// The name the program gives the plugin's crate.
const PLUGIN: &str = "plugin";

/// Largest Turtle file read back, in bytes. A plugin with a thousand
/// parameters writes well under a megabyte.
const MAX_TURTLE_BYTES: u64 = 16 << 20;

/// The program's source: it writes each file `export_lv2!` gives into the
/// folder named by its second argument, for a library named by its first.
pub const HELPER_SOURCE: &str = "\
//! Writes the Turtle files of a plugin's LV2 bundle. Made by cargo mangue.

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(binary), Some(folder)) = (args.next(), args.next()) else {
        eprintln!(\"usage: mangue-lv2-turtle <library file name> <folder>\");
        std::process::exit(2);
    };
    let binary = binary.to_string_lossy();
    let folder = std::path::PathBuf::from(folder);
    for (name, text) in plugin::__manguebyte_lv2_bundle_files(&binary) {
        if let Err(error) = std::fs::write(folder.join(&name), text) {
            eprintln!(\"{name} could not be written: {error}\");
            std::process::exit(1);
        }
    }
}
";

/// The `Cargo.toml` of the program, for the plugin's package.
///
/// ```
/// use std::path::Path;
/// use cargo_mangue::cargo::parse_metadata;
/// use cargo_mangue::lv2::helper_manifest;
///
/// let metadata = parse_metadata(r#"{
///     "target_directory": "/work/target",
///     "packages": [{
///         "name": "my-gain", "version": "0.1.0", "id": "my-gain 0.1.0",
///         "manifest_path": "/work/my-gain/Cargo.toml",
///         "targets": [{ "name": "my_gain", "crate_types": ["cdylib", "rlib"] }]
///     }]
/// }"#)?;
/// let manifest = helper_manifest(&metadata.packages[0], false);
/// assert!(manifest.contains(r#"plugin = { package = "my-gain", path = "/work/my-gain" }"#));
/// assert!(manifest.contains("[workspace]"));
/// # Ok::<(), cargo_mangue::Error>(())
/// ```
pub fn helper_manifest(plugin: &Package, no_default_features: bool) -> String {
    helper_manifest_for(
        plugin,
        no_default_features,
        HELPER_PACKAGE,
        "the Turtle files of the LV2 bundle of",
    )
}

/// The `Cargo.toml` of a program named `helper_package` that depends on
/// the plugin's package and writes `what` it, for this module's program
/// and for [`au`](crate::au)'s.
pub(crate) fn helper_manifest_for(
    plugin: &Package,
    no_default_features: bool,
    helper_package: &str,
    what: &str,
) -> String {
    let folder = plugin
        .manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let defaults = if no_default_features {
        ", default-features = false"
    } else {
        ""
    };
    format!(
        "# Writes {what} {package}. Made by cargo mangue,\n\
         # and made again on every run.\n\
         [package]\n\
         name = \"{helper_package}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         # A workspace of its own, wherever it is.\n\
         [workspace]\n\
         \n\
         [dependencies]\n\
         {PLUGIN} = {{ package = {name}, path = {path}{defaults} }}\n",
        package = plugin.name,
        name = toml_string(&plugin.name),
        path = toml_string(&folder.to_string_lossy()),
    )
}

/// The `--features` the program is run with, to build the plugin with the
/// features the plugin's own build was asked for.
///
/// # Errors
///
/// Fails, with a usage error, for `--all-features`, which cargo applies to
/// the program and not to the plugin, and for a feature of one of the
/// plugin's dependencies (`dependency/feature`), which the program cannot
/// name.
///
/// ```
/// use cargo_mangue::cargo::BuildRequest;
/// use cargo_mangue::lv2::helper_features;
///
/// let request = BuildRequest {
///     features: vec!["gui".into(), "fast, small".into()],
///     ..BuildRequest::default()
/// };
/// assert_eq!(helper_features(&request)?, ["plugin/gui", "plugin/fast", "plugin/small"]);
/// # Ok::<(), cargo_mangue::Error>(())
/// ```
pub fn helper_features(request: &BuildRequest) -> Result<Vec<String>, Error> {
    plugin_features(request, "An LV2 bundle")
}

/// What [`helper_features`] does, for a bundle described as `what` in the
/// messages.
pub(crate) fn plugin_features(request: &BuildRequest, what: &str) -> Result<Vec<String>, Error> {
    if request.all_features {
        return Err(Error::usage(format!(
            "{what} cannot be made with --all-features yet: name the features with \
             --features instead."
        )));
    }
    let mut features = Vec::new();
    for list in &request.features {
        for feature in list
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|feature| !feature.is_empty())
        {
            if feature.contains('/') {
                return Err(Error::usage(format!(
                    "{what} cannot be made with the feature {feature} of a dependency: give \
                     the plugin a feature of its own that turns it on."
                )));
            }
            features.push(format!("{PLUGIN}/{feature}"));
        }
    }
    Ok(features)
}

/// Where the program's package is written for `package`.
pub fn helper_dir(target_directory: &Path, package: &str) -> PathBuf {
    target_directory
        .join("manguebyte")
        .join("lv2")
        .join(package)
}

/// Reads the Turtle files the program wrote into `folder`: `manifest.ttl`
/// first, then the others in the order of their names.
///
/// # Errors
///
/// Fails if the folder cannot be read, if it holds no `manifest.ttl` or
/// nothing else, or if a file is larger than 16 MiB.
pub fn read_turtle_files(folder: &Path) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let cannot = |what: &Path, error: &dyn std::fmt::Display| {
        Error::failed(format!(
            "The LV2 bundle's Turtle files could not be read from {}: {error}.",
            what.display()
        ))
    };
    let mut files = Vec::new();
    let entries = fs::read_dir(folder).map_err(|error| cannot(folder, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| cannot(folder, &error))?;
        let path = entry.path();
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.ends_with(".ttl") || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let mut text = Vec::new();
        let file = fs::File::open(&path).map_err(|error| cannot(&path, &error))?;
        file.take(MAX_TURTLE_BYTES + 1)
            .read_to_end(&mut text)
            .map_err(|error| cannot(&path, &error))?;
        if text.len() as u64 > MAX_TURTLE_BYTES {
            return Err(cannot(&path, &"it is larger than 16 MiB"));
        }
        files.push((name, text));
    }
    files.sort_by(|a, b| (a.0 != "manifest.ttl", &a.0).cmp(&(b.0 != "manifest.ttl", &b.0)));
    match files.first() {
        Some((first, _)) if first == "manifest.ttl" && files.len() >= 2 => Ok(files),
        _ => Err(Error::failed(format!(
            "The program that writes the LV2 bundle's Turtle files left {} without a \
             manifest.ttl and the plugin's own file.",
            folder.display()
        ))),
    }
}

/// Builds and runs the program for `package`, and returns the Turtle files
/// of its bundle, for a library named `binary` beside them.
///
/// # Errors
///
/// Fails with the errors of [`helper_features`], if the program's files
/// cannot be written, with the errors of
/// [`Cargo::run_lv2_helper`](crate::cargo::Cargo::run_lv2_helper), and with
/// those of [`read_turtle_files`].
pub fn turtle_files(
    cargo: &Cargo,
    metadata: &Metadata,
    package: &Package,
    request: &BuildRequest,
    binary: &str,
) -> Result<Vec<(String, Vec<u8>)>, Error> {
    let features = helper_features(request)?;
    let dir = helper_dir(&metadata.target_directory, &package.name);
    let out = dir.join("out");
    let cannot = |what: &Path, error: &std::io::Error| {
        Error::failed(format!(
            "The program that writes the LV2 bundle's Turtle files could not be set up in {}: \
             {error}.",
            what.display()
        ))
    };
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(dir.join("src")).map_err(|error| cannot(&dir, &error))?;
    fs::create_dir_all(&out).map_err(|error| cannot(&out, &error))?;
    let manifest = dir.join("Cargo.toml");
    fs::write(
        &manifest,
        helper_manifest(package, request.no_default_features),
    )
    .map_err(|error| cannot(&manifest, &error))?;
    let main = dir.join("src").join("main.rs");
    fs::write(&main, HELPER_SOURCE).map_err(|error| cannot(&main, &error))?;
    // The versions the plugin's workspace has settled on, if it has a lock.
    if let Some(root) = &metadata.workspace_root {
        let lock = root.join("Cargo.lock");
        if lock.is_file() {
            fs::copy(&lock, dir.join("Cargo.lock")).map_err(|error| cannot(&lock, &error))?;
        }
    }
    cargo.run_lv2_helper(
        &manifest,
        &metadata.target_directory,
        request,
        &features,
        &[OsString::from(binary), out.clone().into_os_string()],
    )?;
    read_turtle_files(&out)
}

/// `text` as a TOML basic string, quotes included.
fn toml_string(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => quoted.push_str(&format!("\\u{:04X}", c as u32)),
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_are_quoted_for_toml() {
        assert_eq!(toml_string("plain"), "\"plain\"");
        assert_eq!(
            toml_string(r"C:\Users\Ana\my-gain"),
            r#""C:\\Users\\Ana\\my-gain""#
        );
        assert_eq!(toml_string("say \"hi\"\n"), r#""say \"hi\"\u000A""#);
    }

    #[test]
    fn features_name_the_plugins_and_refuse_what_cannot_be_passed_on() {
        let request = |features: &[&str], all: bool| BuildRequest {
            features: features.iter().map(|&f| f.to_owned()).collect(),
            all_features: all,
            ..BuildRequest::default()
        };
        assert_eq!(
            helper_features(&request(&[], false)).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            helper_features(&request(&["a b", ",c,"], false)).unwrap(),
            ["plugin/a", "plugin/b", "plugin/c"]
        );
        assert_eq!(
            helper_features(&request(&[], true))
                .unwrap_err()
                .exit_code(),
            2
        );
        assert_eq!(
            helper_features(&request(&["serde/std"], false))
                .unwrap_err()
                .exit_code(),
            2
        );
    }

    #[test]
    fn the_manifest_names_the_plugin_by_path_and_its_default_features() {
        let metadata = crate::cargo::parse_metadata(
            r#"{
                "target_directory": "/work/target",
                "workspace_root": "/work",
                "packages": [{
                    "name": "my-gain", "version": "0.1.0", "id": "my-gain 0.1.0",
                    "manifest_path": "/work/plugins/my gain/Cargo.toml",
                    "targets": [{ "name": "my_gain", "crate_types": ["cdylib", "rlib"] }]
                }]
            }"#,
        )
        .unwrap();
        assert_eq!(metadata.workspace_root.as_deref(), Some(Path::new("/work")));
        let package = &metadata.packages[0];
        let manifest = helper_manifest(package, true);
        assert!(manifest.contains(
            r#"plugin = { package = "my-gain", path = "/work/plugins/my gain", default-features = false }"#
        ));
        assert!(manifest.contains("name = \"mangue-lv2-turtle\""));
        assert_eq!(
            helper_dir(&metadata.target_directory, "my-gain"),
            Path::new("/work/target/manguebyte/lv2/my-gain")
        );
    }

    #[test]
    fn the_manifest_comes_first_and_a_missing_one_is_an_error() {
        let folder =
            std::env::temp_dir().join(format!("cargo-mangue-lv2-read-{}", std::process::id()));
        let _ = fs::remove_dir_all(&folder);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("org.example.gain.ttl"), "plugin").unwrap();
        fs::write(folder.join("notes.txt"), "not Turtle").unwrap();
        assert!(read_turtle_files(&folder).is_err());
        fs::write(folder.join("manifest.ttl"), "manifest").unwrap();
        let files = read_turtle_files(&folder).unwrap();
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["manifest.ttl", "org.example.gain.ttl"]);
        assert_eq!(files[0].1, b"manifest");
        fs::remove_dir_all(&folder).unwrap();
    }
}
