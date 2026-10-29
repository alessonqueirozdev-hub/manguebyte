//! What a plugin bundle looks like on disk.
//!
//! A host finds a plugin by its place in a folder with a fixed layout, and
//! the layout differs with the format and the operating system. [`plan`]
//! is the whole of that knowledge: a function from a description of the
//! plugin to the list of files to write. It reads nothing and writes
//! nothing, so the layout for every system is checked by the tests on
//! whichever system they run on. [`write`](crate::bundle::write) turns a
//! plan into files.
//!
//! The layouts are the ones `scripts/bundle-clap.sh` and
//! `scripts/bundle-vst3.sh` produce, which the CLAP project's validator and
//! Steinberg's accept in CI:
//!
//! | | CLAP | VST 3 |
//! |---|---|---|
//! | Windows | `Name.clap` (the library itself) | `Name.vst3/Contents/x86_64-win/Name.vst3` |
//! | Linux | `Name.clap` (the library itself) | `Name.vst3/Contents/x86_64-linux/Name.so` |
//! | macOS | `Name.clap/Contents/MacOS/Name`, `Info.plist` | `Name.vst3/Contents/MacOS/Name`, `Info.plist`, `PkgInfo` |
//!
//! An LV2 bundle is a plain folder on every system, `Name.lv2`, holding the
//! library (`Name.dll`, `Name.so` or `Name.dylib`) and two Turtle files,
//! `manifest.ttl` and one named after the plugin's id, that describe it.
//! Only the plugin's own code can write those, so the plan lists the
//! library alone, and [`lv2`](crate::lv2) supplies the Turtle files once
//! the plugin is built.
//!
//! An Audio Unit is a macOS bundle, `Name.component/Contents/MacOS/Name`
//! with `PkgInfo` and an `Info.plist` whose `AudioComponents` entry holds
//! codes only the plugin's code knows; [`au`](crate::au) supplies that
//! `Info.plist` once the plugin is built. It is refused for other systems.
//!
//! # Sources
//!
//! - *VST 3 Developer Portal*, "Plug-in Format Structure" (Steinberg): the
//!   bundle folders and the architecture folder names.
//! - The CLAP header `clap/entry.h` (free-audio): a CLAP is a dynamic
//!   library named `.clap`, and a bundle on macOS.
//! - *Information Property List Key Reference* (Apple): the `Info.plist`
//!   keys.
//! - *LV2 core* (lv2plug.in): a bundle is a folder whose `manifest.ttl`
//!   names the plugin's library and its description.

use crate::error::Error;
use crate::platform::{Arch, Format, Os};

/// The contents of `PkgInfo` in a macOS bundle: the type `BNDL` and the
/// creator `????`, eight bytes with no line ending.
pub const PKG_INFO: &[u8] = b"BNDL????";

/// Longest plugin name accepted, in bytes. File systems allow 255 for a
/// file name, and the name gets an extension added.
const MAX_NAME_BYTES: usize = 200;

/// A description of one plugin bundle: everything its layout depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSpec {
    /// The plugin format.
    pub format: Format,
    /// The system the bundle is for.
    pub os: Os,
    /// The architectures the library was built for. One, or several where
    /// the format can hold them: a VST 3 bundle on any system, and either
    /// format on macOS, where they become one universal binary.
    pub archs: Vec<Arch>,
    /// The plugin's name, as shown to people. It names the bundle and the
    /// library inside it.
    pub name: String,
    /// The bundle identifier written to `Info.plist` on macOS, in
    /// reverse-domain form. An underscore in it is written as a hyphen,
    /// since a plugin's id may hold one and macOS does not accept one. A
    /// bundle for Windows or Linux holds no identifier, so there it is not
    /// looked at.
    pub identifier: String,
    /// The version written to `Info.plist` on macOS.
    pub version: String,
}

/// Where the bytes of a planned file come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Contents {
    /// These bytes, known in advance.
    Bytes(Vec<u8>),
    /// The plugin's library as built for one architecture.
    Library(Arch),
    /// One universal binary holding the libraries of several architectures.
    /// Only planned for macOS, and made by `lipo`.
    Universal(Vec<Arch>),
}

/// One file of a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    /// Its path relative to the output folder, with `/` between the parts
    /// whatever the system. It always starts with [`BundlePlan::root`].
    pub path: String,
    /// What goes in it.
    pub contents: Contents,
}

/// The files that make up one bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundlePlan {
    /// The name of the bundle inside the output folder, such as
    /// `Mangue Pluck.vst3`. This is what a host is pointed at and what gets
    /// installed.
    pub root: String,
    /// Whether the bundle is a single file. Otherwise it is a folder.
    pub single_file: bool,
    /// The files, in the order they are written.
    pub files: Vec<PlannedFile>,
    /// Whether the system expects the bundle to carry a code signature.
    /// True on macOS, where an unsigned library does not load on Apple
    /// silicon.
    pub sign: bool,
    /// What is signed when [`sign`](Self::sign) is true, relative to the
    /// output folder: the bundle itself, or for LV2, whose bundle is a
    /// plain folder and not one macOS signs, the library inside it.
    pub signed: String,
}

impl BundlePlan {
    /// The file name of the plugin's library inside the bundle, such as
    /// `Mangue Pluck.so`: the last part of the first file that holds it.
    pub fn library_file_name(&self) -> Option<&str> {
        self.files
            .iter()
            .find(|file| matches!(file.contents, Contents::Library(_) | Contents::Universal(_)))
            .and_then(|file| file.path.rsplit('/').next())
    }
}

/// Works out the files of a bundle.
///
/// This function and [`write`](crate::bundle::write) are public API for
/// programs that bundle plugins without cargo, such as an exporter that
/// ships one pre-built engine (ADR 0012): their signatures and the layouts
/// they produce change only with a breaking-change entry in the changelog.
///
/// ```
/// use cargo_mangue::layout::{BundleSpec, Contents, plan};
/// use cargo_mangue::platform::{Arch, Format, Os};
///
/// let plan = plan(&BundleSpec {
///     format: Format::Vst3,
///     os: Os::Windows,
///     archs: vec![Arch::X86_64],
///     name: "Mangue Pluck".into(),
///     identifier: "org.manguebyte.pluck".into(),
///     version: "0.1.0".into(),
/// })?;
/// assert_eq!(plan.root, "Mangue Pluck.vst3");
/// assert_eq!(plan.files.len(), 1);
/// assert_eq!(
///     plan.files[0].path,
///     "Mangue Pluck.vst3/Contents/x86_64-win/Mangue Pluck.vst3"
/// );
/// assert_eq!(plan.files[0].contents, Contents::Library(Arch::X86_64));
/// # Ok::<(), cargo_mangue::Error>(())
/// ```
///
/// # Errors
///
/// Fails if the name cannot be a file name on all three systems, if no
/// architecture or the same one twice is given, and if several are given
/// for a CLAP on Windows or Linux, which is a single library. For macOS,
/// the one system whose bundles hold the identifier, it also fails if the
/// identifier cannot be written as a bundle identifier.
pub fn plan(spec: &BundleSpec) -> Result<BundlePlan, Error> {
    check_plugin_name(&spec.name)?;
    if spec.archs.is_empty() {
        return Err(Error::failed(
            "A bundle needs the library for at least one architecture.",
        ));
    }
    for (index, arch) in spec.archs.iter().enumerate() {
        if spec.archs[..index].contains(arch) {
            return Err(Error::usage(format!(
                "Two of the targets are for {} on {}. Give each architecture once.",
                arch.name(),
                spec.os.name()
            )));
        }
    }

    let name = &spec.name;
    let root = format!("{name}.{}", spec.format.extension());
    let mut files = Vec::new();
    let mut single_file = false;
    let mut signed = root.clone();

    match (spec.os, spec.format) {
        (os @ (Os::Windows | Os::Linux), Format::Au) => {
            return Err(Error::usage(format!(
                "An Audio Unit is a macOS component, and the bundle is for {}. Bundle it on a \
                 Mac, for a macOS target, with --format au.",
                os.name()
            )));
        }
        (os, Format::Lv2) => {
            // A plain folder on every system, with the library beside the
            // Turtle files that commands add once the plugin is built.
            let binary = match spec.archs.as_slice() {
                [only] => Contents::Library(*only),
                several if os == Os::Macos => Contents::Universal(several.to_vec()),
                several => {
                    return Err(Error::usage(format!(
                        "A .lv2 for {} holds one library, for one architecture, and {} were \
                         asked for. Bundle each target on its own, with --out to keep the \
                         results apart.",
                        os.name(),
                        several.len()
                    )));
                }
            };
            let extension = match os {
                Os::Windows => "dll",
                Os::Linux => "so",
                Os::Macos => "dylib",
            };
            let path = format!("{root}/{name}.{extension}");
            signed = path.clone();
            files.push(PlannedFile {
                path,
                contents: binary,
            });
        }
        (Os::Macos, format) => {
            // On macOS both formats are bundles with the same skeleton, and
            // one binary holds every architecture. Only here is there a
            // file that holds the identifier, so only here is it checked.
            let identifier = macos_identifier(&spec.identifier)?;
            let binary = match spec.archs.as_slice() {
                [only] => Contents::Library(*only),
                several => Contents::Universal(several.to_vec()),
            };
            files.push(PlannedFile {
                path: format!("{root}/Contents/MacOS/{name}"),
                contents: binary,
            });
            if matches!(format, Format::Vst3 | Format::Au) {
                files.push(PlannedFile {
                    path: format!("{root}/Contents/PkgInfo"),
                    contents: Contents::Bytes(PKG_INFO.to_vec()),
                });
            }
            // An Audio Unit's Info.plist names codes only the plugin knows,
            // and comes from its code once it is built.
            if format != Format::Au {
                files.push(PlannedFile {
                    path: format!("{root}/Contents/Info.plist"),
                    contents: Contents::Bytes(
                        info_plist(format, name, &identifier, &spec.version).into_bytes(),
                    ),
                });
            }
        }
        (os, Format::Clap) => {
            // The library itself, renamed.
            let [arch] = spec.archs.as_slice() else {
                return Err(Error::usage(format!(
                    "A .clap for {} is one library and holds one architecture, and {} were \
                     asked for. Bundle each target on its own, with --out to keep the results \
                     apart, or bundle only the VST 3 with --format vst3.",
                    os.name(),
                    spec.archs.len()
                )));
            };
            single_file = true;
            files.push(PlannedFile {
                path: root.clone(),
                contents: Contents::Library(*arch),
            });
        }
        (os, Format::Vst3) => {
            // One folder per architecture, each with its own library.
            let extension = if os == Os::Windows { "vst3" } else { "so" };
            for &arch in &spec.archs {
                let folder = vst3_arch_folder(os, arch);
                files.push(PlannedFile {
                    path: format!("{root}/Contents/{folder}/{name}.{extension}"),
                    contents: Contents::Library(arch),
                });
            }
        }
    }

    Ok(BundlePlan {
        root,
        single_file,
        files,
        sign: spec.os == Os::Macos,
        signed,
    })
}

/// The name of the folder that holds the library for one architecture
/// inside a VST 3 bundle, such as `x86_64-win`.
///
/// On Windows the names are fixed by the VST 3 documentation. On Linux the
/// documentation defines the name as what `uname -m` prints followed by
/// `-linux`, and writes 32-bit x86 as `i386-linux`. On macOS there is one
/// folder, `MacOS`, for every architecture.
pub fn vst3_arch_folder(os: Os, arch: Arch) -> &'static str {
    match (os, arch) {
        (Os::Windows, Arch::X86_64) => "x86_64-win",
        (Os::Windows, Arch::Aarch64) => "arm64-win",
        (Os::Windows, Arch::X86) => "x86-win",
        (Os::Linux, Arch::X86_64) => "x86_64-linux",
        (Os::Linux, Arch::Aarch64) => "aarch64-linux",
        (Os::Linux, Arch::X86) => "i386-linux",
        (Os::Macos, _) => "MacOS",
    }
}

/// The identifier of a plugin as the `Info.plist` of a macOS bundle takes
/// it.
///
/// What a plugin sets as its identifier is usually its id, the one hosts
/// store in sessions and that must never change. CLAP allows an underscore
/// in an id; macOS accepts only letters, digits, hyphens and dots in
/// `CFBundleIdentifier`. So an underscore is written as a hyphen, exactly
/// as in the identifier a package gets when it sets none, and the id itself
/// can stay as it is.
pub(crate) fn macos_identifier(identifier: &str) -> Result<String, Error> {
    let written = identifier.replace('_', "-");
    if check_identifier(&written).is_ok() {
        return Ok(written);
    }
    Err(Error::usage(format!(
        "The identifier \"{}\" cannot be the bundle identifier of a macOS bundle: one is made of \
         letters, digits and hyphens in parts separated by dots, such as com.example.my-plugin \
         (an underscore is written as a hyphen). Set id in [package.metadata.manguebyte] in the \
         plugin's Cargo.toml.",
        identifier.escape_debug()
    )))
}

/// The text of `Info.plist` for a macOS bundle.
///
/// The keys are the ones the shell scripts write: the executable, the
/// identifier, the name, the package type `BNDL` and the version, plus the
/// signature `????` for VST 3 to go with its `PkgInfo`. The values are
/// escaped for XML.
pub fn info_plist(format: Format, name: &str, identifier: &str, version: &str) -> String {
    let name = xml_escape(name);
    let identifier = xml_escape(identifier);
    let version = xml_escape(version);
    let signature = match format {
        // An LV2 bundle has no Info.plist, and an Audio Unit's comes from
        // the plugin: `plan` never writes one for either.
        Format::Clap | Format::Lv2 | Format::Au => "",
        Format::Vst3 => "    <key>CFBundleSignature</key><string>????</string>\n",
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20   <key>CFBundleExecutable</key><string>{name}</string>\n\
         \x20   <key>CFBundleIdentifier</key><string>{identifier}</string>\n\
         \x20   <key>CFBundleName</key><string>{name}</string>\n\
         \x20   <key>CFBundlePackageType</key><string>BNDL</string>\n\
         {signature}\
         \x20   <key>CFBundleVersion</key><string>{version}</string>\n\
         </dict>\n\
         </plist>\n"
    )
}

/// Replaces the three characters that cannot stand for themselves in XML
/// text.
fn xml_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Turns a package version into one macOS accepts as `CFBundleVersion`: up
/// to three numbers separated by dots.
///
/// A pre-release or build suffix is dropped, and a version with no number
/// at its start becomes `1.0`.
///
/// ```
/// use cargo_mangue::layout::bundle_version;
///
/// assert_eq!(bundle_version("0.1.0"), "0.1.0");
/// assert_eq!(bundle_version("2.3.4-beta.1+build5"), "2.3.4");
/// assert_eq!(bundle_version("nightly"), "1.0");
/// ```
pub fn bundle_version(package_version: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in package_version.split('.').take(3) {
        let digits = part.len() - part.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            break;
        }
        parts.push(&part[..digits]);
        if digits < part.len() {
            // Something follows the number: a suffix starts here.
            break;
        }
    }
    if parts.is_empty() {
        "1.0".to_owned()
    } else {
        parts.join(".")
    }
}

/// Checks that a plugin name can be the name of a file and of a folder on
/// Windows, Linux and macOS alike.
///
/// The name becomes part of a path, so this is also what keeps a name such
/// as `../x` from reaching outside the output folder.
///
/// # Errors
///
/// Fails for an empty name, one that starts or ends with a space or ends
/// with a dot, one containing `/ \ : * ? " < > |` or a control character,
/// one of the device names Windows reserves, and one longer than 200 bytes.
pub fn check_plugin_name(name: &str) -> Result<(), Error> {
    let problem = if name.is_empty() {
        Some("it is empty".to_owned())
    } else if name.len() > MAX_NAME_BYTES {
        Some(format!("it is longer than {MAX_NAME_BYTES} bytes"))
    } else if name != name.trim() {
        Some("it starts or ends with a space".to_owned())
    } else if name.ends_with('.') {
        Some("it ends with a dot".to_owned())
    } else if let Some(bad) = name
        .chars()
        .find(|&c| c.is_control() || r#"/\:*?"<>|"#.contains(c))
    {
        Some(if bad.is_control() {
            "it contains a control character".to_owned()
        } else {
            format!("it contains the character {bad}")
        })
    } else if is_reserved_on_windows(name) {
        Some("Windows reserves it for a device".to_owned())
    } else {
        None
    };
    match problem {
        None => Ok(()),
        Some(problem) => Err(Error::usage(format!(
            "The plugin name \"{}\" cannot be used as a file name: {problem}. Choose another \
             with --name, or set name in [package.metadata.manguebyte] in the plugin's \
             Cargo.toml.",
            name.escape_debug()
        ))),
    }
}

/// Whether Windows treats a file with this name, whatever its extension, as
/// a device (Microsoft, "Naming Files, Paths, and Namespaces").
fn is_reserved_on_windows(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or_default().trim_end();
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let numbered = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"));
    numbered.is_some_and(|digit| digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit())
}

/// Checks that an identifier is in reverse-domain form using only what
/// macOS accepts in `CFBundleIdentifier`: letters, digits, hyphens and
/// dots.
///
/// # Errors
///
/// Fails for an empty identifier, one with another character, and one with
/// an empty part, such as `com..gain` or `.gain`.
pub fn check_identifier(identifier: &str) -> Result<(), Error> {
    let well_formed = !identifier.is_empty()
        && identifier.split('.').all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if well_formed {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "The identifier \"{}\" is not usable: an identifier is made of letters, digits and \
             hyphens in parts separated by dots, such as com.example.my-plugin. Set id in \
             [package.metadata.manguebyte] in the plugin's Cargo.toml.",
            identifier.escape_debug()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_that_cannot_be_file_names_are_refused() {
        for name in [
            "",
            " Pluck",
            "Pluck ",
            "Pluck.",
            ".",
            "..",
            "../Pluck",
            "a/b",
            "a\\b",
            "C:Pluck",
            "what?",
            "star*",
            "\"quoted\"",
            "<tag>",
            "a|b",
            "line\nbreak",
            "tab\there",
            "nul\0",
            "CON",
            "con",
            "NUL.old",
            "COM0",
            "COM1",
            "lpt9",
            "aux ",
        ] {
            let error = check_plugin_name(name).unwrap_err();
            assert_eq!(error.exit_code(), 2, "{name:?}");
            assert!(error.message().contains("--name"), "{error}");
        }
        assert!(check_plugin_name(&"x".repeat(201)).is_err());
    }

    #[test]
    fn ordinary_names_are_accepted() {
        for name in [
            "Mangue Pluck",
            "mangue-pluck",
            "Órgão & Côro",
            "Gain (v2.1)",
            "COM10",
            "COMX",
            "Console",
            "Nulla",
            ".hidden",
            "x",
        ] {
            check_plugin_name(name).unwrap_or_else(|error| panic!("{name}: {error}"));
        }
        check_plugin_name(&"x".repeat(200)).unwrap();
    }

    #[test]
    fn identifiers_are_reverse_domain_names() {
        for good in ["org.manguebyte.pluck", "com.u-he.diva", "a", "a1.b2"] {
            check_identifier(good).unwrap_or_else(|error| panic!("{good}: {error}"));
        }
        for bad in [
            "",
            ".",
            "com..gain",
            ".gain",
            "gain.",
            "com.example.my_gain",
            "com.example.my gain",
            "com.exämple.gain",
            "com/example",
        ] {
            assert_eq!(check_identifier(bad).unwrap_err().exit_code(), 2, "{bad:?}");
        }
    }

    #[test]
    fn versions_are_cut_down_to_what_a_bundle_takes() {
        assert_eq!(bundle_version("0.1.0"), "0.1.0");
        assert_eq!(bundle_version("1.2"), "1.2");
        assert_eq!(bundle_version("7"), "7");
        assert_eq!(bundle_version("1.2.3.4"), "1.2.3");
        assert_eq!(bundle_version("1.2.3-rc.1"), "1.2.3");
        assert_eq!(bundle_version("1.2-rc.1"), "1.2");
        assert_eq!(bundle_version("1.0.0+20261013"), "1.0.0");
        assert_eq!(bundle_version("1..2"), "1");
        assert_eq!(bundle_version(""), "1.0");
        assert_eq!(bundle_version("v1.2.3"), "1.0");
        assert_eq!(bundle_version("ünïcode"), "1.0");
    }

    #[test]
    fn text_is_escaped_for_the_property_list() {
        let plist = info_plist(Format::Clap, "Rock & <Roll>", "com.example.r-r", "1.0");
        assert!(
            plist.contains("<key>CFBundleExecutable</key><string>Rock &amp; &lt;Roll&gt;</string>")
        );
        assert!(plist.contains("<key>CFBundleName</key><string>Rock &amp; &lt;Roll&gt;</string>"));
        assert!(!plist.contains("<Roll>"));
    }

    #[test]
    fn an_audio_unit_is_a_macos_bundle_and_nothing_else() {
        let spec = |os| BundleSpec {
            format: Format::Au,
            os,
            archs: vec![Arch::Aarch64],
            name: "Mangue Pluck".into(),
            identifier: "org.manguebyte.pluck".into(),
            version: "0.1.0".into(),
        };
        let plan = plan(&spec(Os::Macos)).unwrap();
        assert_eq!(plan.root, "Mangue Pluck.component");
        let paths: Vec<&str> = plan.files.iter().map(|file| file.path.as_str()).collect();
        // The Info.plist comes from the plugin once it is built.
        assert_eq!(
            paths,
            [
                "Mangue Pluck.component/Contents/MacOS/Mangue Pluck",
                "Mangue Pluck.component/Contents/PkgInfo",
            ]
        );
        assert!(plan.sign);
        assert_eq!(plan.signed, plan.root);
        for os in [Os::Windows, Os::Linux] {
            let error = super::plan(&spec(os)).unwrap_err();
            assert_eq!(error.exit_code(), 2);
            assert!(error.message().contains("macOS"), "{error}");
        }
    }
}
