//! Where an instrument's files come from.
//!
//! The sampler never opens a file by itself. It asks a [`SampleSource`] for
//! the bytes of each sample and each `#include`d file, by the path the SFZ
//! text gives. [`FileSource`] reads them from a directory;
//! [`MemorySource`] hands out bytes it was given, which is how a program
//! with no file system (a web page) supplies them.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;

/// Supplies the files an SFZ instrument refers to.
///
/// Paths arrive as the SFZ text writes them, with any `default_path` in
/// front, `\` turned into `/`, and `.` and `..` segments resolved (see
/// [`normalize_path`]).
///
/// Sources are used while parsing and loading, off the audio thread.
pub trait SampleSource {
    /// Returns the whole contents of the file at `path`.
    fn read(&mut self, path: &str) -> io::Result<Vec<u8>>;
}

impl<T: SampleSource + ?Sized> SampleSource for &mut T {
    fn read(&mut self, path: &str) -> io::Result<Vec<u8>> {
        (**self).read(path)
    }
}

/// Reads files relative to a directory, usually the one the `.sfz` file is
/// in. Absolute paths are read as they are, and a path starting with `../`
/// reads from the directory above. Nothing confines the reads to the
/// directory: a `FileSource` reads whatever the SFZ text names, as the
/// program running it could.
///
/// On targets with no file system, such as `wasm32-unknown-unknown`, every
/// read fails; use a [`MemorySource`] there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSource {
    root: PathBuf,
}

impl FileSource {
    /// A source that reads paths relative to `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The directory paths are relative to.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl SampleSource for FileSource {
    fn read(&mut self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(self.root.join(path))
    }
}

/// Files held in memory, by name.
///
/// Names are normalised like the paths asked for (see [`normalize_path`]).
/// A path that matches no name exactly matches one that differs only in
/// ASCII case, since sample libraries made on Windows or macOS often
/// disagree with their own file names.
///
/// The names form no directory tree with anything above it: a path that
/// climbs (`../Samples/a.wav`, or `a.wav` under `default_path=../Samples/`)
/// finds only a file inserted under that same name, `../` included. Insert
/// the files of a library laid out as `Programs/*.sfz` and `Samples/*.wav`
/// under the names its text uses.
///
/// ```
/// use manguebyte_sampler::{MemorySource, SampleSource};
///
/// let mut source = MemorySource::new();
/// source.insert("Drums\\Kick.wav", vec![1, 2, 3]);
/// assert_eq!(source.read("drums/kick.wav").unwrap(), [1, 2, 3]);
/// assert!(source.read("snare.wav").is_err());
///
/// source.insert("../Samples/a.wav", vec![4]);
/// assert_eq!(source.read("..\\Samples\\a.wav").unwrap(), [4]);
/// assert!(source.read("Samples/a.wav").is_err());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MemorySource {
    files: HashMap<String, Vec<u8>>,
}

impl MemorySource {
    /// An empty source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces the file called `name`.
    pub fn insert(&mut self, name: &str, bytes: impl Into<Vec<u8>>) {
        self.files.insert(normalize_path(name), bytes.into());
    }

    /// Adds a file, for building a source in one expression.
    pub fn with(mut self, name: &str, bytes: impl Into<Vec<u8>>) -> Self {
        self.insert(name, bytes);
        self
    }

    /// How many files it holds.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether it holds no files.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

impl SampleSource for MemorySource {
    fn read(&mut self, path: &str) -> io::Result<Vec<u8>> {
        let path = normalize_path(path);
        if let Some(bytes) = self.files.get(&path) {
            return Ok(bytes.clone());
        }
        // Several names may match without regard to case; the smallest
        // keeps the choice independent of the map's order.
        self.files
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(&path))
            .min_by(|a, b| a.0.cmp(b.0))
            .map(|(_, bytes)| bytes.clone())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no file named {path:?}"))
            })
    }
}

/// Puts a path in the form sources receive: `\` becomes `/`, repeated `/`
/// collapse, `.` segments disappear and `..` removes the segment before it.
/// A `..` with nothing before it to remove is kept at the start of a
/// relative path, so that `../Samples/a.wav` still climbs out of the
/// directory of the `.sfz` file (a common layout for sample libraries), and
/// dropped after a leading `/` or drive letter, above which there is
/// nothing.
///
/// ```
/// use manguebyte_sampler::normalize_path;
///
/// assert_eq!(normalize_path(".\\Piano\\..\\Strings//A3.wav"), "Strings/A3.wav");
/// assert_eq!(normalize_path("/abs/./x.wav"), "/abs/x.wav");
/// assert_eq!(normalize_path("..\\Samples\\a.wav"), "../Samples/a.wav");
/// assert_eq!(normalize_path("/../x.wav"), "/x.wav");
/// ```
pub fn normalize_path(path: &str) -> String {
    let unified = path.trim().replace('\\', "/");
    let absolute = unified.starts_with('/');
    let is_drive = |part: &str| {
        let bytes = part.as_bytes();
        bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in unified.split('/') {
        match part {
            "" | "." => {}
            ".." => match parts.last() {
                // Nothing above the root, or above a drive.
                None if absolute => {}
                Some(&first) if parts.len() == 1 && !absolute && is_drive(first) => {}
                // A relative path that already climbs keeps climbing.
                None | Some(&"..") => parts.push(".."),
                Some(_) => {
                    parts.pop();
                }
            },
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation() {
        assert_eq!(normalize_path("a/b/../../c"), "c");
        assert_eq!(normalize_path("../../c"), "../../c");
        assert_eq!(normalize_path("a/../../c"), "../c");
        assert_eq!(normalize_path("./../a/../b"), "../b");
        assert_eq!(normalize_path("/a/../../c"), "/c");
        assert_eq!(normalize_path("C:\\x\\y.wav"), "C:/x/y.wav");
        assert_eq!(normalize_path("C:\\..\\y.wav"), "C:/y.wav");
        assert_eq!(normalize_path(".."), "..");
        assert_eq!(normalize_path(""), "");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn file_source_reports_missing_files() {
        let mut source = FileSource::new("this directory does not exist");
        assert!(source.read("x.wav").is_err());
        assert_eq!(
            source.root(),
            std::path::Path::new("this directory does not exist")
        );
    }

    #[test]
    fn memory_source_prefers_the_exact_name() {
        let mut source = MemorySource::new()
            .with("A.wav", vec![1])
            .with("a.wav", vec![2])
            .with("b.WAV", vec![3]);
        assert_eq!(source.len(), 3);
        assert!(!source.is_empty());
        assert_eq!(source.read("a.wav").unwrap(), [2]);
        assert_eq!(source.read("A.wav").unwrap(), [1]);
        assert_eq!(source.read("B.wav").unwrap(), [3]);
        let error = source.read("c.wav").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }
}
