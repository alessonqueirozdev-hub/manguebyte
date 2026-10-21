//! Where plugins are installed, finding them there, and telling when a
//! file has changed.

use std::collections::HashSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::any::Format;

/// Deepest a folder is searched below a place plugins are installed.
const MAX_DEPTH: usize = 16;
/// Most entries looked at in one search, over all its folders.
const MAX_ENTRIES: usize = 200_000;
/// Most files counted in one bundle's fingerprint.
const MAX_BUNDLE_FILES: usize = 10_000;

/// A file or bundle that is, by its name, a plugin of a format.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// The file, or the bundle folder.
    pub path: PathBuf,
    /// The format its name says it is in.
    pub format: Format,
}

fn variable(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    if cfg!(windows) {
        variable("USERPROFILE")
    } else {
        variable("HOME")
    }
}

/// The folders listed in the environment variable `name`, separated as the
/// system separates paths.
fn listed(name: &str) -> Vec<PathBuf> {
    std::env::var_os(name)
        .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .collect()
}

/// The folders a format's plugins are installed in on this system, as the
/// format's own documentation gives them, in the order a host searches
/// them. Folders that do not exist are listed all the same.
///
/// | Format | Windows | macOS | Linux and others |
/// |---|---|---|---|
/// | CLAP | `%COMMONPROGRAMFILES%\CLAP`, `%LOCALAPPDATA%\Programs\Common\CLAP` | `~/Library/Audio/Plug-Ins/CLAP`, `/Library/Audio/Plug-Ins/CLAP` | `~/.clap`, `/usr/lib/clap` |
/// | VST 3 | `%COMMONPROGRAMFILES%\VST3`, `%LOCALAPPDATA%\Programs\Common\VST3` | `~/Library/Audio/Plug-Ins/VST3`, `/Library/Audio/Plug-Ins/VST3`, `/Network/Library/Audio/Plug-Ins/VST3` | `~/.vst3`, `/usr/lib/vst3`, `/usr/local/lib/vst3` |
/// | LV2 | `%APPDATA%\LV2`, `%COMMONPROGRAMFILES%\LV2` | `~/Library/Audio/Plug-Ins/LV2`, `~/.lv2`, `/usr/local/lib/lv2`, `/usr/lib/lv2`, `/Library/Audio/Plug-Ins/LV2` | `~/.lv2`, `/usr/local/lib/lv2`, `/usr/lib/lv2` |
/// | AU | none | `~/Library/Audio/Plug-Ins/Components`, `/Library/Audio/Plug-Ins/Components` | none |
///
/// CLAP's `CLAP_PATH` adds folders before these, and LV2's `LV2_PATH`
/// replaces them, as each format's specification says.
pub fn standard_paths(format: Format) -> Vec<PathBuf> {
    let home = home();
    let in_home = |rest: &str| home.as_ref().map(|home| home.join(rest));
    let mut paths: Vec<Option<PathBuf>> = Vec::new();
    match format {
        Format::Clap => {
            paths.extend(listed("CLAP_PATH").into_iter().map(Some));
            if cfg!(windows) {
                paths.push(variable("COMMONPROGRAMFILES").map(|path| path.join("CLAP")));
                paths.push(variable("LOCALAPPDATA").map(|path| path.join("Programs/Common/CLAP")));
            } else if cfg!(target_os = "macos") {
                paths.push(in_home("Library/Audio/Plug-Ins/CLAP"));
                paths.push(Some("/Library/Audio/Plug-Ins/CLAP".into()));
            } else {
                paths.push(in_home(".clap"));
                paths.push(Some("/usr/lib/clap".into()));
            }
        }
        Format::Vst3 => {
            if cfg!(windows) {
                paths.push(variable("COMMONPROGRAMFILES").map(|path| path.join("VST3")));
                paths.push(variable("LOCALAPPDATA").map(|path| path.join("Programs/Common/VST3")));
            } else if cfg!(target_os = "macos") {
                paths.push(in_home("Library/Audio/Plug-Ins/VST3"));
                paths.push(Some("/Library/Audio/Plug-Ins/VST3".into()));
                paths.push(Some("/Network/Library/Audio/Plug-Ins/VST3".into()));
            } else {
                paths.push(in_home(".vst3"));
                paths.push(Some("/usr/lib/vst3".into()));
                paths.push(Some("/usr/local/lib/vst3".into()));
            }
        }
        Format::Au => {
            // Apple's "Audio Unit Programming Guide", "Installing": the
            // user's folder and the system's.
            if cfg!(target_os = "macos") {
                paths.push(in_home("Library/Audio/Plug-Ins/Components"));
                paths.push(Some("/Library/Audio/Plug-Ins/Components".into()));
            }
        }
        Format::Lv2 => {
            let given = listed("LV2_PATH");
            if !given.is_empty() {
                return given;
            }
            if cfg!(windows) {
                paths.push(variable("APPDATA").map(|path| path.join("LV2")));
                paths.push(variable("COMMONPROGRAMFILES").map(|path| path.join("LV2")));
            } else if cfg!(target_os = "macos") {
                paths.push(in_home("Library/Audio/Plug-Ins/LV2"));
                paths.push(in_home(".lv2"));
                paths.push(Some("/usr/local/lib/lv2".into()));
                paths.push(Some("/usr/lib/lv2".into()));
                paths.push(Some("/Library/Audio/Plug-Ins/LV2".into()));
            } else {
                paths.push(in_home(".lv2"));
                paths.push(Some("/usr/local/lib/lv2".into()));
                paths.push(Some("/usr/lib/lv2".into()));
            }
        }
    }
    let mut unique: Vec<PathBuf> = Vec::new();
    for path in paths.into_iter().flatten() {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// The format a path's name says it is in, among `formats`.
fn format_of(path: &Path, is_folder: bool, formats: &[Format]) -> Option<Format> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let format = match extension.as_str() {
        "clap" => Format::Clap,
        "vst3" => Format::Vst3,
        "lv2" if is_folder => Format::Lv2,
        "component" if is_folder => Format::Au,
        _ => return None,
    };
    formats.contains(&format).then_some(format)
}

/// Finds the plugins of `formats` in `paths` and the folders below them,
/// by their names: a file or folder ending in `.clap` or `.vst3`, and a
/// folder ending in `.lv2` or `.component`. Runs nothing, and reads no
/// file.
///
/// A bundle is not searched inside. Each path is given at most once,
/// sorted, even when a folder is reached twice through links; a folder is
/// searched at most 16 levels deep, and at most 200,000 entries are looked
/// at in all. A path that is itself a plugin is a candidate.
pub fn find(paths: &[PathBuf], formats: &[Format]) -> Vec<Candidate> {
    let mut found: Vec<Candidate> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut visited = 0usize;
    let mut stack: Vec<(PathBuf, usize)> = Vec::new();
    for path in paths {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        if let Some(format) = format_of(path, metadata.is_dir(), formats) {
            if seen.insert(canonical(path)) {
                found.push(Candidate {
                    path: path.clone(),
                    format,
                });
            }
        } else if metadata.is_dir() {
            stack.push((path.clone(), 0));
        }
    }
    let mut searched: HashSet<PathBuf> = HashSet::new();
    while let Some((folder, depth)) = stack.pop() {
        if !searched.insert(canonical(&folder)) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&folder) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_ENTRIES {
                stack.clear();
                break;
            }
            let path = entry.path();
            // Links are followed to what they point at.
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if let Some(format) = format_of(&path, metadata.is_dir(), formats) {
                if seen.insert(canonical(&path)) {
                    found.push(Candidate { path, format });
                }
            } else if metadata.is_dir() && depth < MAX_DEPTH {
                stack.push((path, depth + 1));
            }
        }
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned())
}

/// What tells a file apart from the same file changed: when it was last
/// written, how large it is, and for a bundle, how many files it holds.
/// For a bundle, the newest time and the total size of its files count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Fingerprint {
    /// When it was last written, in nanoseconds since 1970, or 0 if the
    /// system does not say.
    pub modified: u128,
    /// Its size in bytes.
    pub size: u64,
    /// How many files it is: 1 for a file.
    pub files: u64,
}

impl Fingerprint {
    /// The fingerprint of a file or bundle, or `None` if there is nothing
    /// at `path`.
    pub fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        if !metadata.is_dir() {
            return Some(Self::of_file(&metadata));
        }
        let mut print = Self::default();
        let mut stack = vec![path.to_owned()];
        while let Some(folder) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                if print.files >= MAX_BUNDLE_FILES as u64 {
                    return Some(print);
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if metadata.is_dir() {
                    stack.push(entry.path());
                } else {
                    let file = Self::of_file(&metadata);
                    print.modified = print.modified.max(file.modified);
                    print.size = print.size.saturating_add(file.size);
                    print.files += 1;
                }
            }
        }
        Some(print)
    }

    fn of_file(metadata: &std::fs::Metadata) -> Self {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |since| since.as_nanos());
        Self {
            modified,
            size: metadata.len(),
            files: 1,
        }
    }
}

/// The arguments that ask a program to act as the scanner, ahead of the
/// format and the path.
pub(crate) fn scanner_arguments(format: &str, path: &Path) -> Vec<OsString> {
    vec![
        OsString::from(super::SCAN_ARGUMENT),
        OsString::from(format),
        path.as_os_str().to_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_has_places_to_look() {
        for format in [Format::Clap, Format::Vst3, Format::Lv2] {
            let paths = standard_paths(format);
            assert!(!paths.is_empty(), "{format}");
            for path in &paths {
                assert!(path.is_absolute(), "{path:?}");
            }
        }
        if cfg!(target_os = "linux") {
            assert!(standard_paths(Format::Vst3).contains(&PathBuf::from("/usr/lib/vst3")));
        }
        if cfg!(windows) && std::env::var_os("COMMONPROGRAMFILES").is_some() {
            let common = PathBuf::from(std::env::var_os("COMMONPROGRAMFILES").unwrap());
            assert_eq!(standard_paths(Format::Vst3)[0], common.join("VST3"));
        }
    }

    #[test]
    fn names_say_the_format_and_only_folders_are_lv2() {
        let all = [Format::Clap, Format::Vst3, Format::Lv2];
        assert_eq!(
            format_of(Path::new("a/B.clap"), false, &all),
            Some(Format::Clap)
        );
        assert_eq!(
            format_of(Path::new("B.VST3"), true, &all),
            Some(Format::Vst3)
        );
        assert_eq!(format_of(Path::new("B.lv2"), true, &all), Some(Format::Lv2));
        assert_eq!(format_of(Path::new("B.lv2"), false, &all), None);
        let au = [Format::Au];
        assert_eq!(
            format_of(Path::new("B.component"), true, &au),
            Some(Format::Au)
        );
        assert_eq!(format_of(Path::new("B.component"), false, &au), None);
        assert_eq!(format_of(Path::new("B.component"), true, &all), None);
        assert_eq!(
            standard_paths(Format::Au).is_empty(),
            cfg!(not(target_os = "macos"))
        );
        assert_eq!(format_of(Path::new("B.dll"), false, &all), None);
        assert_eq!(format_of(Path::new("B.clap"), false, &[Format::Vst3]), None);
    }
}
