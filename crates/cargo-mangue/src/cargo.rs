//! Asking cargo about a workspace and building a plugin with it.
//!
//! The tool never guesses where cargo put a library or what it is called.
//! `cargo metadata` says which packages exist, which of them build a plugin
//! library, and what each sets under `[package.metadata.manguebyte]`.
//! `cargo build --message-format=json` reports every file it produces, and
//! the library is taken from that report. Both are documented in *The Cargo
//! Book* ("External tools").
//!
//! The functions that read cargo's output take text and return values, so
//! they are tested without running cargo. What they are given comes from
//! another program: they return an error for anything they do not
//! understand and never panic.

use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{self, BufReader, Read};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::error::Error;
use crate::lines::Lines;
use crate::platform::{Os, host_triple};

/// Most bytes of `cargo metadata` output that are read. Real output is
/// kilobytes for a workspace of dozens of packages.
const MAX_METADATA_BYTES: u64 = 64 << 20;

/// Most bytes kept of one line of `cargo build` output.
const MAX_BUILD_LINE_BYTES: usize = 1 << 20;

/// What a plugin's `Cargo.toml` sets under `[package.metadata.manguebyte]`.
///
/// ```toml
/// [package.metadata.manguebyte]
/// name = "Mangue Pluck"
/// id = "org.manguebyte.pluck"
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    /// The plugin's name as shown to people, which names its bundles.
    pub name: Option<String>,
    /// The bundle identifier written on macOS, in reverse-domain form:
    /// usually the plugin's own id. Bundles for the other systems hold no
    /// identifier.
    pub id: Option<String>,
}

/// One package of the workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Its name.
    pub name: String,
    /// Its version.
    pub version: String,
    /// The identifier cargo gives it, which also appears in build messages.
    pub id: String,
    /// Its `Cargo.toml`.
    pub manifest_path: PathBuf,
    /// Whether it has a library target of crate type `cdylib`, which is
    /// what a plugin is.
    pub builds_plugin: bool,
    /// Its `[package.metadata.manguebyte]` table.
    pub settings: Settings,
}

impl Package {
    /// The plugin's name: the one in its settings, else the package name.
    pub fn plugin_name(&self) -> &str {
        self.settings.name.as_deref().unwrap_or(&self.name)
    }

    /// The bundle identifier: the one in its settings, else
    /// `org.manguebyte.` followed by the package name, which is what the
    /// shell scripts write. An underscore in the package name becomes a
    /// hyphen, since macOS does not accept one.
    pub fn identifier(&self) -> String {
        match &self.settings.id {
            Some(id) => id.clone(),
            None => format!("org.manguebyte.{}", self.name.replace('_', "-")),
        }
    }
}

/// What `cargo metadata` says about a workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// Where cargo puts what it builds.
    pub target_directory: PathBuf,
    /// The workspace's folder, which holds its `Cargo.lock`, if cargo said.
    pub workspace_root: Option<PathBuf>,
    /// The packages that are members of the workspace.
    pub packages: Vec<Package>,
}

/// Reads the output of `cargo metadata --format-version 1 --no-deps`.
///
/// ```
/// use cargo_mangue::cargo::parse_metadata;
///
/// let metadata = parse_metadata(r#"{
///     "target_directory": "/work/target",
///     "packages": [{
///         "name": "my-gain", "version": "0.1.0", "id": "my-gain 0.1.0",
///         "manifest_path": "/work/Cargo.toml",
///         "targets": [{ "name": "my_gain", "crate_types": ["cdylib", "rlib"] }],
///         "metadata": { "manguebyte": { "name": "My Gain" } }
///     }]
/// }"#)?;
/// let package = &metadata.packages[0];
/// assert!(package.builds_plugin);
/// assert_eq!(package.plugin_name(), "My Gain");
/// assert_eq!(package.identifier(), "org.manguebyte.my-gain");
/// # Ok::<(), cargo_mangue::Error>(())
/// ```
///
/// # Errors
///
/// Fails if the text is not the JSON cargo documents, or if a setting under
/// `[package.metadata.manguebyte]` is not a string.
pub fn parse_metadata(json: &str) -> Result<Metadata, Error> {
    let unreadable = |what: &str| {
        Error::failed(format!(
            "The output of cargo metadata could not be read: {what}. Check that cargo works \
             here by running cargo metadata --no-deps."
        ))
    };
    let root: Value =
        serde_json::from_str(json).map_err(|error| unreadable(&format!("{error}")))?;
    let target_directory = root
        .get("target_directory")
        .and_then(Value::as_str)
        .ok_or_else(|| unreadable("it has no target_directory"))?;
    let listed = root
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| unreadable("it has no packages"))?;

    let mut packages = Vec::with_capacity(listed.len());
    for package in listed {
        let text = |key: &str| {
            package
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| unreadable(&format!("a package has no {key}")))
        };
        let name = text("name")?;
        let builds_plugin = package
            .get("targets")
            .and_then(Value::as_array)
            .is_some_and(|targets| targets.iter().any(is_plugin_library));
        let settings = read_settings(&name, package.get("metadata"))?;
        packages.push(Package {
            version: text("version")?,
            id: text("id")?,
            manifest_path: PathBuf::from(text("manifest_path")?),
            builds_plugin,
            settings,
            name,
        });
    }
    Ok(Metadata {
        target_directory: PathBuf::from(target_directory),
        workspace_root: root
            .get("workspace_root")
            .and_then(Value::as_str)
            .map(PathBuf::from),
        packages,
    })
}

/// Whether a target, as cargo describes it, is a library a host can load.
fn is_plugin_library(target: &Value) -> bool {
    target
        .get("crate_types")
        .and_then(Value::as_array)
        .is_some_and(|types| types.iter().any(|kind| kind.as_str() == Some("cdylib")))
}

/// Reads `[package.metadata.manguebyte]` out of a package's metadata.
fn read_settings(package: &str, metadata: Option<&Value>) -> Result<Settings, Error> {
    let Some(table) = metadata.and_then(|metadata| metadata.get("manguebyte")) else {
        return Ok(Settings::default());
    };
    let setting = |key: &str| match table.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(Error::failed(format!(
            "In the Cargo.toml of {package}, {key} under [package.metadata.manguebyte] must be \
             text in quotes, such as {key} = \"...\"."
        ))),
    };
    if !table.is_object() {
        return Err(Error::failed(format!(
            "In the Cargo.toml of {package}, [package.metadata.manguebyte] must be a table \
             with settings such as name = \"...\"."
        )));
    }
    Ok(Settings {
        name: setting("name")?,
        id: setting("id")?,
    })
}

/// Chooses the package to work on.
///
/// With `wanted`, it is the package of that name. Without, it is the plugin
/// package whose folder `current_dir` is in, and failing that the only
/// plugin package of the workspace.
///
/// # Errors
///
/// Fails if `wanted` names no package or one that builds no plugin library,
/// and if there is no plugin package or no way to choose between several.
/// The message lists the choices.
pub fn select_package<'a>(
    metadata: &'a Metadata,
    wanted: Option<&str>,
    current_dir: Option<&Path>,
) -> Result<&'a Package, Error> {
    let plugins: Vec<&Package> = metadata
        .packages
        .iter()
        .filter(|package| package.builds_plugin)
        .collect();
    let choices = || {
        let names: Vec<&str> = plugins
            .iter()
            .map(|package| package.name.as_str())
            .collect();
        names.join(", ")
    };

    if let Some(wanted) = wanted {
        let Some(package) = metadata.packages.iter().find(|p| p.name == wanted) else {
            return Err(Error::usage(if plugins.is_empty() {
                format!(
                    "There is no package named \"{wanted}\" in this workspace, and no package \
                     in it builds a plugin. Create one with cargo mangue new <name>."
                )
            } else {
                format!(
                    "There is no package named \"{wanted}\" in this workspace. The packages \
                     that build a plugin are: {}.",
                    choices()
                )
            }));
        };
        if !package.builds_plugin {
            return Err(Error::failed(format!(
                "The package {wanted} does not build a plugin library. A plugin needs \
                 crate-type = [\"cdylib\"] under [lib] in {}.",
                package.manifest_path.display()
            )));
        }
        return Ok(package);
    }

    // The plugin whose folder we are in, the innermost if they nest. This
    // is the package cargo itself would pick from here.
    let here = current_dir.and_then(|dir| {
        plugins
            .iter()
            .filter_map(|package| {
                let folder = package.manifest_path.parent()?;
                // A manifest path with no folder in it says nothing
                // about where the package is, and every path starts
                // with the empty one.
                let depth = folder.components().count();
                (depth > 0 && dir.starts_with(folder)).then_some((depth, *package))
            })
            .max_by_key(|&(depth, _)| depth)
            .map(|(_, package)| package)
    });
    if let Some(package) = here {
        return Ok(package);
    }
    match plugins.as_slice() {
        [only] => Ok(only),
        [] => Err(Error::failed(
            "No package in this workspace builds a plugin library (crate-type \"cdylib\"). \
             Create one with cargo mangue new <name>.",
        )),
        _ => Err(Error::usage(format!(
            "Several packages here build a plugin: {}. Choose one with -p <package>.",
            choices()
        ))),
    }
}

/// How to build a plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildRequest {
    /// The profile to build with.
    pub profile: Profile,
    /// The `Cargo.toml` to use, if not the one cargo finds by itself.
    pub manifest_path: Option<PathBuf>,
    /// Features to switch on, each as given to `--features`.
    pub features: Vec<String>,
    /// Switch every feature on.
    pub all_features: bool,
    /// Leave the default features off.
    pub no_default_features: bool,
}

/// A cargo profile.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Profile {
    /// The `dev` profile, cargo's default.
    #[default]
    Dev,
    /// The `release` profile.
    Release,
    /// A profile by name.
    Named(String),
}

/// Where the programs that write a bundle's metadata are built, for a
/// workspace whose target directory is `target_directory`.
///
/// Not `target_directory` itself: a program's package is a workspace of
/// its own, in which the plugin's crate and the framework's crates are path
/// dependencies rather than members, and cargo builds them with other flags
/// but can give them the same file names. Built in the same directory, they
/// replaced the workspace's own builds, and a later `cargo test --release`
/// in the workspace linked a plugin against one build of `manguebyte-plugin`
/// and the test against another. The programs share this directory with
/// each other, so that what they have in common is built once.
pub fn helper_target_dir(target_directory: &Path) -> PathBuf {
    target_directory.join("manguebyte").join("helpers")
}

/// The arguments of the `cargo build` that builds a plugin's library.
///
/// `target` is a target triple, or `None` to build for cargo's default.
pub fn build_args(package: &str, request: &BuildRequest, target: Option<&str>) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut add = |arg: &str| args.push(OsString::from(arg));
    add("build");
    add("--message-format=json-render-diagnostics");
    add("--package");
    add(package);
    add("--lib");
    match &request.profile {
        Profile::Dev => {}
        Profile::Release => add("--release"),
        Profile::Named(name) => {
            add("--profile");
            add(name);
        }
    }
    if let Some(target) = target {
        add("--target");
        add(target);
    }
    for features in &request.features {
        add("--features");
        add(features);
    }
    if request.all_features {
        add("--all-features");
    }
    if request.no_default_features {
        add("--no-default-features");
    }
    if let Some(manifest_path) = &request.manifest_path {
        args.push(OsString::from("--manifest-path"));
        args.push(manifest_path.clone().into_os_string());
    }
    args
}

/// Picks the plugin's library out of the messages of a build, one line at
/// a time.
///
/// Feed it every line cargo prints on its standard output under
/// `--message-format=json`, then ask for the result.
#[derive(Debug)]
pub struct LibraryScan<'a> {
    package: &'a Package,
    os: Os,
    library: Option<PathBuf>,
    /// A file the package's library target produced that is not a library
    /// for `os`, kept for the error message.
    other: Option<String>,
}

impl<'a> LibraryScan<'a> {
    /// Looks for the library `package` builds for `os`.
    pub fn new(package: &'a Package, os: Os) -> Self {
        Self {
            package,
            os,
            library: None,
            other: None,
        }
    }

    /// Takes one line of cargo's output.
    ///
    /// Lines that are not JSON objects are ignored: cargo prints only JSON
    /// there, but a wrapper around it might not.
    ///
    /// # Errors
    ///
    /// Fails if a line that starts like a JSON object is not one, which
    /// means the output was cut or damaged.
    pub fn feed(&mut self, line: &str) -> Result<(), Error> {
        let line = line.trim();
        if !line.starts_with('{') {
            return Ok(());
        }
        let message: Value = serde_json::from_str(line).map_err(|error| {
            Error::failed(format!(
                "A message from cargo build could not be read ({error}). Run the build again; \
                 if it keeps happening, build with cargo build to see what cargo prints."
            ))
        })?;
        if message.get("reason").and_then(Value::as_str) != Some("compiler-artifact") {
            return Ok(());
        }
        let text = |key: &str| message.get(key).and_then(Value::as_str);
        let ours = text("package_id") == Some(self.package.id.as_str())
            || text("manifest_path").map(Path::new) == Some(self.package.manifest_path.as_path());
        if !ours || !message.get("target").is_some_and(is_plugin_library) {
            return Ok(());
        }
        let Some(files) = message.get("filenames").and_then(Value::as_array) else {
            return Ok(());
        };
        let wanted = self.os.library_extension();
        for file in files.iter().filter_map(Value::as_str) {
            let extension = Path::new(file).extension().and_then(|e| e.to_str());
            if extension == Some(wanted) {
                self.library = Some(PathBuf::from(file));
            } else if matches!(extension, Some("dll" | "so" | "dylib")) {
                self.other = Some(file.to_owned());
            }
        }
        Ok(())
    }

    /// The library, once every line has been fed.
    ///
    /// # Errors
    ///
    /// Fails if cargo reported no library for the package, or one for a
    /// different system.
    pub fn finish(self) -> Result<PathBuf, Error> {
        if let Some(library) = self.library {
            return Ok(library);
        }
        Err(Error::failed(match self.other {
            Some(other) => format!(
                "cargo built {other}, which is not a library for {}. If cargo is set up to \
                 build for another system, say which with --target.",
                self.os.name()
            ),
            None => format!(
                "cargo built {} without reporting a plugin library for it. Check that {} has \
                 crate-type = [\"cdylib\"] under [lib].",
                self.package.name,
                self.package.manifest_path.display()
            ),
        }))
    }
}

/// Picks the plugin's library out of the whole output of a build.
///
/// ```
/// use cargo_mangue::cargo::{Package, Settings, find_library};
/// use cargo_mangue::platform::Os;
///
/// let package = Package {
///     name: "my-gain".into(),
///     version: "0.1.0".into(),
///     id: "path+file:///work#my-gain@0.1.0".into(),
///     manifest_path: "/work/Cargo.toml".into(),
///     builds_plugin: true,
///     settings: Settings::default(),
/// };
/// let output = r#"{"reason":"compiler-artifact","package_id":"path+file:///work#my-gain@0.1.0","target":{"crate_types":["cdylib","rlib"]},"filenames":["/work/target/debug/libmy_gain.so","/work/target/debug/libmy_gain.rlib"]}
/// {"reason":"build-finished","success":true}"#;
/// let library = find_library(output, &package, Os::Linux)?;
/// assert_eq!(library.to_str(), Some("/work/target/debug/libmy_gain.so"));
/// # Ok::<(), cargo_mangue::Error>(())
/// ```
///
/// # Errors
///
/// The errors of [`LibraryScan::feed`] and [`LibraryScan::finish`].
pub fn find_library(output: &str, package: &Package, os: Os) -> Result<PathBuf, Error> {
    let mut scan = LibraryScan::new(package, os);
    for line in output.lines() {
        scan.feed(line)?;
    }
    scan.finish()
}

/// What the place cargo put a library in says about what it was built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltFor<'a> {
    /// It is straight in a profile's folder, `<target directory>/<profile>`:
    /// no target was named anywhere, and cargo built for the host of the
    /// Rust compiler.
    Host,
    /// It is in `<target directory>/<triple>/<profile>`: cargo built for
    /// this target, named with `--target`, with the `CARGO_BUILD_TARGET`
    /// variable or with `build.target` in a configuration file.
    Target(&'a str),
    /// It is somewhere else, which says nothing.
    Unknown,
}

/// Reads what a library was built for off the place cargo reports it in.
///
/// The tool decides how to lay out a bundle before it builds, and the only
/// targets it can know of then are the ones it passes to cargo itself and
/// the `CARGO_BUILD_TARGET` variable. Cargo may be set up to build for
/// another one, with `build.target` in a configuration file, and it is
/// cargo that knows. It says so by where it puts what it builds: a build
/// for a named target goes to a folder of that target's name inside the
/// target directory, and a build for the host goes to the target directory
/// itself (*The Cargo Book*, "Build cache"). In either, the library is in
/// the folder of the profile it was built with, or in `deps` inside that.
///
/// ```
/// use std::path::Path;
/// use cargo_mangue::cargo::{BuiltFor, built_for};
///
/// let target = Path::new("/work/target");
/// let host = target.join("release").join("libmy_gain.so");
/// assert_eq!(built_for(&host, target), BuiltFor::Host);
///
/// let cross = target
///     .join("aarch64-unknown-linux-gnu")
///     .join("release")
///     .join("libmy_gain.so");
/// assert_eq!(
///     built_for(&cross, target),
///     BuiltFor::Target("aarch64-unknown-linux-gnu")
/// );
/// assert_eq!(built_for(Path::new("/elsewhere/x.so"), target), BuiltFor::Unknown);
/// ```
pub fn built_for<'a>(library: &'a Path, target_directory: &Path) -> BuiltFor<'a> {
    let Ok(inside) = library.strip_prefix(target_directory) else {
        return BuiltFor::Unknown;
    };
    // The file is in the profile's folder, or in the `deps` folder inside
    // it, which is where cargo leaves the libraries of dependencies.
    let (Some(_file), Some(mut folder)) = (inside.file_name(), inside.parent()) else {
        return BuiltFor::Unknown;
    };
    if folder.file_name() == Some(OsStr::new("deps")) {
        match folder.parent() {
            Some(profile) => folder = profile,
            None => return BuiltFor::Unknown,
        }
    }
    // What is left is the profile's folder, with the target's folder before
    // it if there is one. Anything else is not cargo's layout.
    let mut parts = folder.components();
    match [parts.next(), parts.next(), parts.next()] {
        [Some(Component::Normal(_profile)), None, _] => BuiltFor::Host,
        [
            Some(Component::Normal(triple)),
            Some(Component::Normal(_profile)),
            None,
        ] => match triple.to_str() {
            Some(triple) => BuiltFor::Target(triple),
            None => BuiltFor::Unknown,
        },
        _ => BuiltFor::Unknown,
    }
}

/// The cargo program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cargo {
    program: OsString,
}

impl Cargo {
    /// The cargo that started this tool, which it names in the `CARGO`
    /// variable, or else `cargo` from `PATH`.
    pub fn from_env() -> Self {
        Self {
            program: env::var_os("CARGO")
                .filter(|program| !program.is_empty())
                .unwrap_or_else(|| "cargo".into()),
        }
    }

    fn could_not_start(&self, error: &std::io::Error) -> Error {
        Error::failed(format!(
            "cargo could not be started ({error}). Install Rust from https://rustup.rs, or \
             set the CARGO variable to the cargo program."
        ))
    }

    /// Runs `cargo metadata` and reads the result.
    ///
    /// # Errors
    ///
    /// Fails if cargo cannot be started, if it fails (its own message is
    /// passed on), and with the errors of [`parse_metadata`].
    pub fn metadata(&self, manifest_path: Option<&Path>) -> Result<Metadata, Error> {
        let mut command = Command::new(&self.program);
        command.args(["metadata", "--format-version", "1", "--no-deps"]);
        if let Some(manifest_path) = manifest_path {
            command.arg("--manifest-path").arg(manifest_path);
        }
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| self.could_not_start(&error))?;
        let mut json = String::new();
        let mut read = Ok(0);
        let mut too_long = false;
        if let Some(mut stdout) = child.stdout.take() {
            read = (&mut stdout)
                .take(MAX_METADATA_BYTES)
                .read_to_string(&mut json);
            // Anything beyond the limit is read and dropped, so that cargo
            // is not left blocked on a full pipe.
            too_long = io::copy(&mut stdout, &mut io::sink()).is_ok_and(|extra| extra > 0);
        }
        let status = child.wait().map_err(|error| self.could_not_start(&error))?;
        if !status.success() {
            return Err(Error::failed(
                "cargo could not describe the workspace; its message is above. Run the command \
                 from a folder with a Cargo.toml, or name one with --manifest-path.",
            ));
        }
        if too_long && read.is_ok() {
            return Err(Error::failed(format!(
                "The output of cargo metadata is longer than {MAX_METADATA_BYTES} bytes and \
                 was not read."
            )));
        }
        read.map_err(|error| {
            Error::failed(format!(
                "The output of cargo metadata could not be read: {error}."
            ))
        })?;
        parse_metadata(&json)
    }

    /// Builds the library of `package` and returns where cargo put it.
    ///
    /// Cargo's own progress and diagnostics go straight to the terminal.
    ///
    /// # Errors
    ///
    /// Fails with [`Failure::Build`](crate::Failure::Build) if the build
    /// fails, and with the errors of [`LibraryScan`] if no library for `os`
    /// comes out of it.
    pub fn build(
        &self,
        package: &Package,
        request: &BuildRequest,
        target: Option<&str>,
        os: Os,
    ) -> Result<PathBuf, Error> {
        let mut child = Command::new(&self.program)
            .args(build_args(&package.name, request, target))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|error| self.could_not_start(&error))?;

        let mut scan = LibraryScan::new(package, os);
        let mut problem = None;
        if let Some(stdout) = child.stdout.take() {
            let mut lines = Lines::new(BufReader::new(stdout), MAX_BUILD_LINE_BYTES);
            // Keep reading to the end even after a problem, so that cargo
            // is never left blocked on a full pipe.
            loop {
                match lines.next_line() {
                    Ok(Some(line)) => {
                        if problem.is_some() {
                            continue;
                        }
                        if line.truncated {
                            problem = Some(Error::failed(format!(
                                "A message from cargo build is longer than \
                                 {MAX_BUILD_LINE_BYTES} bytes and was not read."
                            )));
                        } else if let Err(error) = scan.feed(&String::from_utf8_lossy(line.bytes)) {
                            problem = Some(error);
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        problem.get_or_insert_with(|| {
                            Error::failed(format!(
                                "The output of cargo build could not be read: {error}."
                            ))
                        });
                        break;
                    }
                }
            }
        }
        let status = child.wait().map_err(|error| self.could_not_start(&error))?;
        if !status.success() {
            return Err(Error::build(format!(
                "cargo build failed for {}; its messages are above.",
                package.name
            )));
        }
        match problem {
            Some(problem) => Err(problem),
            None => scan.finish(),
        }
    }

    /// Builds and runs the program that writes an LV2 bundle's Turtle files
    /// (see [`lv2`](crate::lv2)): `cargo run` on the package in
    /// `helper_manifest`, with the profile of `request`, in
    /// [`helper_target_dir`]`(target_directory)`. `args` go to the program.
    ///
    /// Cargo's own progress and diagnostics go straight to the terminal.
    ///
    /// # Errors
    ///
    /// Fails with [`Failure::Build`](crate::Failure::Build) if the program
    /// does not build or does not finish its work.
    pub fn run_lv2_helper(
        &self,
        helper_manifest: &Path,
        target_directory: &Path,
        request: &BuildRequest,
        features: &[String],
        args: &[OsString],
    ) -> Result<(), Error> {
        self.run_helper(
            helper_manifest,
            target_directory,
            request,
            features,
            args,
            "The program that writes the LV2 bundle's Turtle files did not build or did not \
             finish; its messages are above. The plugin must invoke export_lv2! (from \
             manguebyte::lv2 or manguebyte_lv2) at the root of its crate, and the crate must \
             have the rlib type as well as cdylib.",
        )
    }

    /// Builds and runs a program that depends on the plugin's crate, as
    /// [`run_lv2_helper`](Self::run_lv2_helper) does, failing with
    /// `failure` if it does not build or does not finish.
    ///
    /// # Errors
    ///
    /// Fails with [`Failure::Build`](crate::Failure::Build) and `failure`.
    pub fn run_helper(
        &self,
        helper_manifest: &Path,
        target_directory: &Path,
        request: &BuildRequest,
        features: &[String],
        args: &[OsString],
        failure: &str,
    ) -> Result<(), Error> {
        let mut command = Command::new(&self.program);
        command
            .arg("run")
            .arg("--quiet")
            .arg("--manifest-path")
            .arg(helper_manifest)
            .arg("--target-dir")
            .arg(helper_target_dir(target_directory));
        match &request.profile {
            Profile::Dev => {}
            Profile::Release => {
                command.arg("--release");
            }
            Profile::Named(name) => {
                command.arg("--profile").arg(name);
            }
        }
        for feature in features {
            command.arg("--features").arg(feature);
        }
        let status = command
            .arg("--")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .map_err(|error| self.could_not_start(&error))?;
        if status.success() {
            Ok(())
        } else {
            Err(Error::build(failure))
        }
    }

    /// The target triple cargo is expected to build for when none is named:
    /// the `CARGO_BUILD_TARGET` variable if set, else the host of the Rust
    /// compiler.
    ///
    /// This is what can be known before a build, and it is used to refuse
    /// early what cannot work. A `build.target` in a cargo configuration
    /// file is not seen here. What cargo really built for is read after the
    /// build from where it put the library, with [`built_for`], and that is
    /// what decides the bundle.
    ///
    /// # Errors
    ///
    /// Fails if the compiler cannot be started or does not say what its
    /// host is.
    pub fn default_target(&self) -> Result<String, Error> {
        match self.target_variable() {
            Some(target) => Ok(target),
            None => self.host_target(),
        }
    }

    /// The target the `CARGO_BUILD_TARGET` variable names, if it names one.
    pub fn target_variable(&self) -> Option<String> {
        env::var("CARGO_BUILD_TARGET")
            .ok()
            .filter(|target| !target.is_empty())
    }

    /// The host of the Rust compiler, which is what cargo builds for when
    /// no target is named anywhere.
    ///
    /// # Errors
    ///
    /// Fails if the compiler cannot be started or does not say what its
    /// host is.
    pub fn host_target(&self) -> Result<String, Error> {
        let rustc = env::var_os("RUSTC")
            .filter(|program| !program.is_empty())
            .unwrap_or_else(|| "rustc".into());
        let unknown = |why: String| {
            Error::failed(format!(
                "The Rust compiler did not say which system it builds for ({why}). Name the \
                 target yourself with --target, such as --target x86_64-unknown-linux-gnu."
            ))
        };
        let output = Command::new(&rustc)
            .arg("-vV")
            .stdin(Stdio::null())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| unknown(format!("it could not be started: {error}")))?;
        let printed = String::from_utf8_lossy(&output.stdout);
        host_triple(&printed)
            .map(str::to_owned)
            .ok_or_else(|| unknown("rustc -vV printed no host line".to_owned()))
    }
}
