//! Turning a [`BundlePlan`] into files.
//!
//! [`plan`](crate::layout::plan) decides what a bundle contains; [`write()`]
//! puts it on disk. On macOS two more steps follow from the plan: several
//! architectures are merged into one universal binary with `lipo`, and the
//! bundle is given an ad-hoc signature with `codesign`. Both programs come
//! with Apple's developer tools and exist only on macOS, so they are
//! reached through [`Tools`] and their absence is reported, never ignored
//! silently.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::Error;
use crate::layout::{BundlePlan, Contents};
use crate::platform::Arch;
use crate::tools::{ToolCommand, Tools};

/// A bundle that was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    /// The bundle: a file or a folder.
    pub path: PathBuf,
    /// Things the person should know that did not stop the work, each a
    /// full sentence. Today: that the bundle could not be signed.
    pub notes: Vec<String>,
}

/// The `lipo` command that merges libraries of several architectures into
/// one universal binary.
pub fn lipo_command(inputs: &[PathBuf], output: &Path) -> ToolCommand {
    let mut args = vec![
        OsString::from("-create"),
        OsString::from("-output"),
        output.as_os_str().to_owned(),
    ];
    args.extend(inputs.iter().map(|input| input.as_os_str().to_owned()));
    ToolCommand {
        program: "lipo",
        args,
    }
}

/// The `codesign` command that gives a bundle an ad-hoc signature,
/// replacing any it has.
///
/// An ad-hoc signature (the identity `-`) names no developer. It is what
/// lets a library load on Apple silicon, where unsigned code does not run;
/// it does not satisfy Gatekeeper on someone else's Mac, which needs a
/// Developer ID signature and notarisation.
pub fn codesign_command(bundle: &Path) -> ToolCommand {
    ToolCommand {
        program: "codesign",
        args: vec![
            OsString::from("--force"),
            OsString::from("--sign"),
            OsString::from("-"),
            bundle.as_os_str().to_owned(),
        ],
    }
}

/// Writes a bundle into `out_dir`, replacing one of the same name.
///
/// `libraries` says where the plugin's library is for each architecture
/// the plan mentions. The library is copied as it is, so a library that is
/// a copy of a pre-built engine with data appended to it, as a plugin
/// exporter makes (ADR 0012), is bundled with its data.
///
/// This function and [`plan`](crate::layout::plan) are public API for
/// programs that bundle plugins without cargo, such as an exporter that
/// ships one pre-built engine: their signatures and what they write change
/// only with a breaking-change entry in the changelog.
///
/// ```
/// use std::ffi::OsString;
/// use std::path::{Path, PathBuf};
/// use cargo_mangue::bundle::write;
/// use cargo_mangue::layout::{BundleSpec, plan};
/// use cargo_mangue::platform::{Arch, Format, Os};
/// use cargo_mangue::tools::Tools;
///
/// /// No programs: nothing for Windows needs any.
/// struct NoTools;
/// impl Tools for NoTools {
///     fn find(&self, _: &str) -> Option<PathBuf> { None }
///     fn run(&mut self, _: &Path, _: &[OsString]) -> Result<(), String> { Ok(()) }
/// }
///
/// let folder = std::env::temp_dir().join(format!("bundle-write-doc-{}", std::process::id()));
/// std::fs::create_dir_all(&folder)?;
/// // An engine with an instrument's data appended, as an exporter makes it.
/// let engine = folder.join("engine.dll");
/// std::fs::write(&engine, b"MZ...the engine...the instrument's data")?;
///
/// let plan = plan(&BundleSpec {
///     format: Format::Vst3,
///     os: Os::Windows,
///     archs: vec![Arch::X86_64],
///     name: "My Piano".into(),
///     identifier: "parabolic.k7m2q9xh4cv3bnw8".into(),
///     version: "1.0.0".into(),
/// })?;
/// let out = folder.join("out");
/// let written = write(&plan, &out, &[(Arch::X86_64, engine.clone())], &mut NoTools)?;
/// assert_eq!(written.path, out.join("My Piano.vst3"));
/// assert!(written.notes.is_empty());
/// let library = out.join("My Piano.vst3/Contents/x86_64-win/My Piano.vst3");
/// assert_eq!(std::fs::read(library)?, std::fs::read(&engine)?);
/// # std::fs::remove_dir_all(&folder)?;
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # Errors
///
/// Fails if a file cannot be written, if an existing bundle cannot be
/// replaced (on Windows, because a host has it loaded), if the plan needs a
/// universal binary and `lipo` is not installed, and if `lipo` or
/// `codesign` fails. A missing `codesign` is not an error: the bundle is
/// written unsigned and a note says so.
pub fn write(
    plan: &BundlePlan,
    out_dir: &Path,
    libraries: &[(Arch, PathBuf)],
    tools: &mut dyn Tools,
) -> Result<Written, Error> {
    let library = |arch: Arch| {
        libraries
            .iter()
            .find(|(built, _)| *built == arch)
            .map(|(_, path)| path.as_path())
            .ok_or_else(|| {
                Error::failed(format!(
                    "The bundle needs the library for {}, and none was built.",
                    arch.name()
                ))
            })
    };

    let root = out_dir.join(&plan.root);
    remove(&root)?;
    fs::create_dir_all(out_dir).map_err(|error| cannot("create the folder", out_dir, &error))?;

    for file in &plan.files {
        let destination = file
            .path
            .split('/')
            .fold(out_dir.to_path_buf(), |path, part| path.join(part));
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| cannot("create the folder", parent, &error))?;
        }
        match &file.contents {
            Contents::Bytes(bytes) => {
                fs::write(&destination, bytes)
                    .map_err(|error| cannot("write", &destination, &error))?;
            }
            Contents::Library(arch) => {
                let source = library(*arch)?;
                fs::copy(source, &destination).map_err(|error| {
                    Error::failed(format!(
                        "Could not copy {} to {}: {error}.",
                        source.display(),
                        destination.display()
                    ))
                })?;
            }
            Contents::Universal(archs) => {
                let inputs = archs
                    .iter()
                    .map(|&arch| library(arch).map(Path::to_path_buf))
                    .collect::<Result<Vec<_>, _>>()?;
                let command = lipo_command(&inputs, &destination);
                let Some(lipo) = tools.find(command.program) else {
                    return Err(Error::failed(
                        "lipo was not found, so the libraries of the targets cannot be merged \
                         into one universal binary. It comes with Apple's command line tools: \
                         run xcode-select --install on a Mac, or bundle one --target at a time.",
                    ));
                };
                tools.run(&lipo, &command.args).map_err(|said| {
                    Error::failed(format!(
                        "lipo could not make the universal binary: {said}. Check that every \
                         --target is for a different architecture."
                    ))
                })?;
            }
        }
    }

    let mut notes = Vec::new();
    if plan.sign {
        let signed = plan
            .signed
            .split('/')
            .fold(out_dir.to_path_buf(), |path, part| path.join(part));
        let command = codesign_command(&signed);
        match tools.find(command.program) {
            Some(codesign) => tools.run(&codesign, &command.args).map_err(|said| {
                Error::failed(format!(
                    "codesign could not sign {}: {said}.",
                    signed.display()
                ))
            })?,
            None => notes.push(format!(
                "codesign was not found, so {} is not signed, and a Mac with Apple silicon \
                 will refuse to load it. Sign it on a Mac with: codesign --force --sign - \
                 \"{}\"",
                plan.root,
                signed.display()
            )),
        }
    }
    Ok(Written { path: root, notes })
}

/// Removes a bundle, whether it is a file or a folder. Returns whether
/// there was one.
///
/// # Errors
///
/// Fails if it exists and cannot be removed.
pub fn remove(bundle: &Path) -> Result<bool, Error> {
    let metadata = match fs::symlink_metadata(bundle) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(cannot("look at", bundle, &error)),
    };
    let removed = if metadata.is_dir() {
        fs::remove_dir_all(bundle)
    } else {
        fs::remove_file(bundle)
    };
    match removed {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::failed(format!(
            "Could not remove {}: {error}. {}",
            bundle.display(),
            advice(&error)
        ))),
    }
}

/// Copies a bundle, file or folder, to `destination`, which must not exist.
///
/// # Errors
///
/// Fails if anything cannot be read or written.
pub fn copy(bundle: &Path, destination: &Path) -> Result<(), Error> {
    let failed = |error: io::Error| {
        Error::failed(format!(
            "Could not copy {} to {}: {error}. {}",
            bundle.display(),
            destination.display(),
            advice(&error)
        ))
    };
    copy_tree(bundle, destination, 0).map_err(failed)
}

/// Deepest folder nesting [`copy`] follows. A bundle is three levels deep.
const MAX_DEPTH: usize = 32;

fn copy_tree(source: &Path, destination: &Path, depth: usize) -> io::Result<()> {
    if depth > MAX_DEPTH {
        return Err(io::Error::other("the folders are nested too deeply"));
    }
    if fs::metadata(source)?.is_dir() {
        fs::create_dir_all(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                depth + 1,
            )?;
        }
    } else {
        fs::copy(source, destination)?;
    }
    Ok(())
}

/// What to try, for the errors that have an obvious cause.
fn advice(error: &io::Error) -> &'static str {
    if error.kind() == io::ErrorKind::PermissionDenied {
        "Close any host that has the plugin loaded. For a folder shared by all users, run the \
         command as an administrator (with sudo on Linux and macOS)."
    } else {
        "Check that the folder exists and can be written to."
    }
}

fn cannot(action: &str, path: &Path, error: &io::Error) -> Error {
    Error::failed(format!(
        "Could not {action} {}: {error}. {}",
        path.display(),
        advice(error)
    ))
}
