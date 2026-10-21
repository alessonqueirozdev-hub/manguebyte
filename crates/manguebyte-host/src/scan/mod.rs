//! Finding the plugins installed on a system, and reading what each one
//! is, in a process of its own.
//!
//! Loading a CLAP or VST 3 library runs its code, and a plugin that
//! crashes or hangs while it is being listed would take a host down with
//! it. So the [`Scanner`] never loads one itself: for each file it starts
//! a scanner process, which loads the library, reads its descriptions
//! without creating a plugin or running any audio, writes them out and
//! exits. The host waits a set time, kills a scanner that is still
//! running, and reports what it found, plugin by plugin, and what failed,
//! with how. An LV2 bundle says what it is in its Turtle files and runs no
//! code to be listed, so it is read in the host itself. An Audio Unit
//! bundle (`.component`, on macOS) says what it holds in its `Info.plist`,
//! but a unit's code runs only when it is instantiated, so the scanner
//! process finds each of its units with `AudioComponentFindNext` and
//! instantiates it, which is what a unit that crashes or hangs does it
//! on.
//!
//! What a scan found is kept in a [`ScanCache`], by path and by a
//! [`Fingerprint`] of when the file was last written and how large it is,
//! so the next scan only starts a process for what is new or has changed.
//!
//! # The scanner process
//!
//! Any program can be the scanner if it hands its arguments to
//! [`serve_if_asked`] first thing: a host application can be its own
//! scanner, with [`Scanner::this_program`]. This crate also builds one,
//! `manguebyte-scan`, which does nothing else, for [`Scanner::new`]. It is
//! started as `<program> --manguebyte-scan <clap|vst3|au> <path>`.
//!
//! ```no_run
//! use manguebyte_host::any::Format;
//! use manguebyte_host::scan::{ScanCache, Scanner, find, standard_paths};
//!
//! fn main() -> std::io::Result<()> {
//!     // SAFETY: this program loads plugins only when started as the
//!     // scanner, and exits right after.
//!     unsafe { manguebyte_host::scan::serve_if_asked() };
//!
//!     let formats = [Format::Clap, Format::Vst3, Format::Lv2, Format::Au];
//!     let mut places = Vec::new();
//!     for format in formats {
//!         places.extend(standard_paths(format));
//!     }
//!     let mut cache = ScanCache::load("plugins.cache");
//!     let report = Scanner::this_program()?.scan(&find(&places, &formats), &mut cache);
//!     for found in &report.plugins {
//!         println!("{} ({}): {}", found.entry.name, found.entry.format, found.path.display());
//!     }
//!     for failed in &report.failures {
//!         eprintln!("{}: {}", failed.path.display(), failed.error);
//!     }
//!     cache.save("plugins.cache")
//! }
//! ```

mod cache;
mod paths;
mod record;

use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub use cache::ScanCache;
pub use paths::{Candidate, Fingerprint, find, standard_paths};

use crate::any::{Format, PluginEntry};
use cache::Outcome;
use paths::scanner_arguments;
use record::{fields, format_from_word, format_word, line, plugin_fields, plugin_from_fields};

/// The first argument of a program started as the scanner.
pub const SCAN_ARGUMENT: &str = "--manguebyte-scan";

/// How long a scanner process may take over one file unless told
/// otherwise.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// What the scanner process puts in front of each of its lines, so that
/// what a plugin prints while it loads is not taken for an answer.
const TAG: &str = "\u{1}manguebyte-scan";

/// Most of a scanner's output read; the rest is read and dropped.
const MAX_OUTPUT_BYTES: u64 = 4 << 20;

/// How much of a scanner's error output is kept for a report.
const KEPT_ERROR_BYTES: usize = 2_048;

/// Why a file could not be listed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanError {
    /// The scanner process ended without finishing: the plugin crashed it,
    /// or brought it down while it was being unloaded. Holds the exit
    /// code, if the process had one, and the end of what it wrote to its
    /// error output.
    Crashed {
        /// The exit code, or `None` if the system ended it, by a signal.
        code: Option<i32>,
        /// The last of its error output.
        output: String,
    },
    /// The scanner process was still running when its time was up, and
    /// was killed.
    TimedOut,
    /// The file was read, and is not a plugin this host can use: the
    /// reason, as the format's host gave it.
    Refused(String),
    /// The scanner could not be started, or what it wrote could not be
    /// read. Not the file's fault, and not kept in the cache.
    Unreadable(String),
}

impl fmt::Display for ScanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Crashed {
                code: Some(code), ..
            } => {
                write!(f, "the scanner crashed with exit code {code}")
            }
            Self::Crashed { code: None, .. } => f.write_str("the scanner was ended by the system"),
            Self::TimedOut => f.write_str("the scanner took too long and was stopped"),
            Self::Refused(why) => write!(f, "not a usable plugin: {why}"),
            Self::Unreadable(why) => write!(f, "the scanner could not be used: {why}"),
        }
    }
}

impl std::error::Error for ScanError {}

/// One plugin a scan found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedPlugin {
    /// The file or bundle it is in.
    pub path: PathBuf,
    /// What its library or bundle says about it. Its `id` is what
    /// [`Library::create`](crate::any::Library::create) takes.
    pub entry: PluginEntry,
}

/// A file or bundle a scan could not list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanFailure {
    /// The file or bundle.
    pub path: PathBuf,
    /// The format its name says it is in.
    pub format: Format,
    /// Why.
    pub error: ScanError,
}

/// What a scan found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanReport {
    /// Every plugin found, in the order of the candidates.
    pub plugins: Vec<ScannedPlugin>,
    /// Every candidate that could not be listed, this time or, as the
    /// cache remembers, before.
    pub failures: Vec<ScanFailure>,
    /// How many candidates were read this time.
    pub scanned: usize,
    /// How many were answered from the cache.
    pub cached: usize,
}

/// Lists plugins out of process. See the [module documentation](self).
#[derive(Debug, Clone)]
pub struct Scanner {
    program: PathBuf,
    arguments: Vec<OsString>,
    timeout: Duration,
    jobs: usize,
}

impl Scanner {
    /// A scanner that starts `program` for each file: `manguebyte-scan`, or
    /// any program that calls [`serve_if_asked`] first thing.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let jobs = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .clamp(1, 4);
        Self {
            program: program.into(),
            arguments: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            jobs,
        }
    }

    /// A scanner that starts the program that is running, which must call
    /// [`serve_if_asked`] first thing.
    ///
    /// # Errors
    ///
    /// If the system cannot say where the running program is.
    pub fn this_program() -> std::io::Result<Self> {
        Ok(Self::new(std::env::current_exe()?))
    }

    /// Arguments given to the program ahead of the scanner's own.
    pub fn with_arguments(
        mut self,
        arguments: impl IntoIterator<Item = impl Into<OsString>>,
    ) -> Self {
        self.arguments = arguments.into_iter().map(Into::into).collect();
        self
    }

    /// How long one file may take: [`DEFAULT_TIMEOUT`] unless set.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// How many scanner processes run at once: as many as the processor
    /// has cores, up to 4, unless set. At least 1.
    pub fn with_jobs(mut self, jobs: usize) -> Self {
        self.jobs = jobs.max(1);
        self
    }

    /// Lists every candidate, from the cache where it knows the file as it
    /// is, and keeps what it finds in the cache.
    pub fn scan(&self, candidates: &[Candidate], cache: &mut ScanCache) -> ScanReport {
        let mut outcomes: Vec<Option<(Fingerprint, Outcome, bool)>> = vec![None; candidates.len()];
        let mut pending: Vec<usize> = Vec::new();
        for (index, candidate) in candidates.iter().enumerate() {
            let Some(print) = Fingerprint::of(&candidate.path) else {
                outcomes[index] = Some((
                    Fingerprint::default(),
                    Err(ScanError::Unreadable(
                        "there is nothing at this path".into(),
                    )),
                    false,
                ));
                continue;
            };
            match cache.lookup(candidate, &print) {
                Some(known) => outcomes[index] = Some((print, known.clone(), true)),
                None => {
                    outcomes[index] = Some((print, Ok(Vec::new()), false));
                    pending.push(index);
                }
            }
        }

        // The files no cache knows, a few at a time.
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Outcome)>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..self.jobs.min(pending.len()) {
                scope.spawn(|| {
                    loop {
                        let at = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&index) = pending.get(at) else {
                            break;
                        };
                        let outcome = self.scan_one(&candidates[index]);
                        results
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push((index, outcome));
                    }
                });
            }
        });
        for (index, outcome) in results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            if let Some((print, slot, _)) = &mut outcomes[index] {
                *slot = outcome;
                cache.store(&candidates[index], *print, slot.clone());
            }
        }

        let mut report = ScanReport::default();
        for (candidate, outcome) in candidates.iter().zip(outcomes) {
            let Some((_, outcome, from_cache)) = outcome else {
                continue;
            };
            if from_cache {
                report.cached += 1;
            } else {
                report.scanned += 1;
            }
            match outcome {
                Ok(entries) => {
                    report
                        .plugins
                        .extend(entries.into_iter().map(|entry| ScannedPlugin {
                            path: candidate.path.clone(),
                            entry,
                        }))
                }
                Err(error) => report.failures.push(ScanFailure {
                    path: candidate.path.clone(),
                    format: candidate.format,
                    error,
                }),
            }
        }
        report
    }

    /// Finds the candidates in `paths` with [`find`] and lists them with
    /// [`scan`](Self::scan).
    pub fn scan_paths(
        &self,
        paths: &[PathBuf],
        formats: &[Format],
        cache: &mut ScanCache,
    ) -> ScanReport {
        self.scan(&find(paths, formats), cache)
    }

    /// Lists one file or bundle now, without a cache: an LV2 bundle in this
    /// process, from its Turtle files, and anything else in a scanner
    /// process.
    ///
    /// # Errors
    ///
    /// Why the file could not be listed.
    pub fn scan_one(&self, candidate: &Candidate) -> Result<Vec<PluginEntry>, ScanError> {
        if candidate.format == Format::Lv2 {
            return read_lv2(&candidate.path);
        }
        self.run(candidate)
    }

    /// Starts a scanner process for one file and reads its answer.
    fn run(&self, candidate: &Candidate) -> Result<Vec<PluginEntry>, ScanError> {
        let mut child = Command::new(&self.program)
            .args(&self.arguments)
            .args(scanner_arguments(
                format_word(candidate.format),
                &candidate.path,
            ))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                ScanError::Unreadable(format!(
                    "{} could not be started: {error}",
                    self.program.display()
                ))
            })?;
        let stdout = child
            .stdout
            .take()
            .map(|pipe| std::thread::spawn(move || drain(pipe)));
        let stderr = child
            .stderr
            .take()
            .map(|pipe| std::thread::spawn(move || drain(pipe)));

        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if started.elapsed() >= self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ScanError::Unreadable(error.to_string()));
                }
            }
        };
        // The pipes close when the process is gone, which ends both reads.
        let output = stdout
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let errors = stderr
            .and_then(|reader| reader.join().ok())
            .unwrap_or_default();
        let Some(status) = status else {
            return Err(ScanError::TimedOut);
        };
        let answer = String::from_utf8_lossy(&output);
        let crashed = || ScanError::Crashed {
            code: status.code(),
            output: last_of(&errors),
        };
        if !status.success() {
            return Err(crashed());
        }
        parse_answer(&answer, candidate.format).ok_or_else(crashed)?
    }
}

/// Reads all of a pipe, keeping at most [`MAX_OUTPUT_BYTES`].
fn drain(mut pipe: impl Read) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut pipe).take(MAX_OUTPUT_BYTES).read_to_end(&mut kept);
    let _ = std::io::copy(&mut pipe, &mut std::io::sink());
    kept
}

/// The end of a process's error output, as text.
fn last_of(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(KEPT_ERROR_BYTES);
    String::from_utf8_lossy(&bytes[start..]).trim().to_owned()
}

/// Reads what a scanner wrote: `None` if it never finished, and otherwise
/// the plugins or the reason there are none.
fn parse_answer(answer: &str, format: Format) -> Option<Result<Vec<PluginEntry>, ScanError>> {
    let mut begun = false;
    let mut plugins = Vec::new();
    let mut refusal = None;
    for text in answer.lines() {
        let Some(record) = text
            .strip_prefix(TAG)
            .and_then(|rest| rest.strip_prefix('\t'))
        else {
            continue;
        };
        let record = fields(record)?;
        match record.first().map(String::as_str) {
            Some("begin") => begun = record.get(1).map(String::as_str) == Some("1"),
            Some("plugin") if begun => plugins.push(plugin_from_fields(format, &record[1..])?),
            Some("error") if begun => refusal = record.get(1).cloned(),
            Some("end") if begun => {
                return Some(match refusal {
                    Some(why) => Err(ScanError::Refused(why)),
                    None => Ok(plugins),
                });
            }
            _ => {}
        }
    }
    None
}

#[cfg(feature = "lv2")]
fn read_lv2(path: &Path) -> Result<Vec<PluginEntry>, ScanError> {
    crate::lv2::read_bundle(path)
        .map(|plugins| plugins.iter().map(crate::any::lv2_entry).collect())
        .map_err(|error| ScanError::Refused(error.to_string()))
}

#[cfg(not(feature = "lv2"))]
fn read_lv2(_path: &Path) -> Result<Vec<PluginEntry>, ScanError> {
    Err(ScanError::Refused(
        crate::any::Error::NotBuiltIn(Format::Lv2).to_string(),
    ))
}

// --- The scanner process -------------------------------------------------------

/// Acts as the scanner and ends the process, if the program was started as
/// one; otherwise returns at once. Call it first thing in `main` of a
/// program that is its own scanner (see [`Scanner::this_program`]).
///
/// # Safety
///
/// Started as the scanner, the process loads the file it is given, which
/// runs the file's code: see [`Library::load`](crate::any::Library::load).
/// The scanner is a process of its own so that whatever that code does
/// ends with it, and it exits as soon as it has written its answer. The
/// caller vouches that the program does nothing else that the file's code
/// could harm before then.
pub unsafe fn serve_if_asked() {
    let arguments: Vec<OsString> = std::env::args_os().skip(1).collect();
    if arguments
        .first()
        .is_some_and(|first| first == SCAN_ARGUMENT)
    {
        // SAFETY: passed on from the caller.
        let served = unsafe { serve(&arguments[1..]) };
        std::process::exit(if served { 0 } else { 2 });
    }
}

/// Acts as the scanner: lists the plugins of the file named by `arguments`,
/// its format (`clap`, `vst3`, `lv2` or `au`) and its path, on standard
/// output, for the [`Scanner`] that started this process.
///
/// Returns failure for arguments that make no sense; a file that is not a
/// plugin is an answer, and success.
///
/// # Safety
///
/// As for [`serve_if_asked`].
pub unsafe fn run_scanner(arguments: &[OsString]) -> ExitCode {
    // SAFETY: passed on from the caller.
    if unsafe { serve(arguments) } {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// What [`run_scanner`] does, saying whether it answered.
///
/// # Safety
///
/// As for [`serve_if_asked`].
unsafe fn serve(arguments: &[OsString]) -> bool {
    let [format, path] = arguments else {
        eprintln!("usage: {SCAN_ARGUMENT} <clap|vst3|lv2|au> <path>");
        return false;
    };
    let Some(format) = format.to_str().and_then(format_from_word) else {
        eprintln!("unknown format {format:?}");
        return false;
    };
    let mut lines = vec![line(&["begin", "1"])];
    // SAFETY: passed on from the caller.
    match unsafe { list(format, Path::new(path)) } {
        Ok(entries) => {
            for entry in &entries {
                let mut record = vec!["plugin"];
                record.extend(plugin_fields(entry));
                lines.push(line(&record));
            }
        }
        Err(why) => lines.push(line(&["error", &why])),
    }
    lines.push(line(&["end"]));
    let mut out = std::io::stdout().lock();
    for record in lines {
        // Each answer on a line of its own, whatever the plugin printed.
        if writeln!(out, "\n{TAG}\t{record}").is_err() {
            return false;
        }
    }
    out.flush().is_ok()
}

/// Loads a file, reads its plugins, and unloads it again before answering,
/// so that a plugin that crashes on the way out is reported as crashing.
/// Each Audio Unit is also instantiated and put away again, since that is
/// when its code first runs.
///
/// # Safety
///
/// As for [`serve_if_asked`].
unsafe fn list(format: Format, path: &Path) -> Result<Vec<PluginEntry>, String> {
    if format == Format::Lv2 {
        return read_lv2(path).map_err(|error| match error {
            ScanError::Refused(why) => why,
            other => other.to_string(),
        });
    }
    // SAFETY: passed on from the caller.
    let library =
        unsafe { crate::any::Library::load_as(path, format) }.map_err(|e| e.to_string())?;
    let entries = library.plugins().to_vec();
    if format == Format::Au {
        for entry in &entries {
            let plugin = library.create(&entry.id).map_err(|e| e.to_string())?;
            drop(plugin);
        }
    }
    drop(library);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::any::Kind;

    fn answer(lines: &[&str]) -> String {
        lines
            .iter()
            .map(|record| format!("\n{TAG}\t{record}\n"))
            .collect()
    }

    #[test]
    fn an_answer_counts_only_when_it_is_whole() {
        let plugin = "plugin\tid\tName\tVendor\t1.0\tinstrument\tsynth";
        let whole = answer(&["begin\t1", plugin, "end"]);
        let found = parse_answer(&whole, Format::Clap).unwrap().unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::Instrument);
        assert_eq!(found[0].categories, ["synth"]);

        // Cut short: the process did not finish.
        assert_eq!(
            parse_answer(&answer(&["begin\t1", plugin]), Format::Clap),
            None
        );
        // Another version of the protocol, or no beginning.
        assert_eq!(
            parse_answer(&answer(&["begin\t2", plugin, "end"]), Format::Clap),
            None
        );
        assert_eq!(parse_answer(&answer(&[plugin, "end"]), Format::Clap), None);
        // What a plugin prints among the answers is not an answer.
        let noisy = format!("hello{}plugin\tfake\nend\n{}", TAG, whole);
        assert_eq!(
            parse_answer(&noisy, Format::Clap).unwrap().unwrap().len(),
            1
        );
        // A refusal.
        let refused = answer(&["begin\t1", "error\tno entry point", "end"]);
        assert_eq!(
            parse_answer(&refused, Format::Vst3),
            Some(Err(ScanError::Refused("no entry point".into())))
        );
        // A damaged record.
        assert_eq!(
            parse_answer(&answer(&["begin\t1", "plugin\tid", "end"]), Format::Clap),
            None
        );
    }
}
