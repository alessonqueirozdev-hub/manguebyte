//! A plugin's identity as the wrappers report it, and the path of the
//! library it is in.
//!
//! [`Plugin::INFO`] is a constant, fixed when the plugin is compiled.
//! [`Plugin::info`] may work the identity out when the library is loaded
//! instead, for a library that is one pre-built engine copied many times,
//! each copy with its identity in data next to it or appended to its file.
//! [`plugin_info`] calls it once per plugin type and keeps the result for
//! the life of the library; every wrapper, the saved state and the
//! [`Harness`](crate::Harness) read the identity from there. See ADR 0012.
//!
//! [`library_path`] says which file the plugin's code was loaded from, so
//! that `info` can find that data. The wrappers record what the format's
//! entry point tells them (CLAP's `init` passes the path); otherwise it is
//! asked of the system, by address.

use std::any::TypeId;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{Plugin, PluginInfo};

/// One cell per plugin type, each made once and kept for the rest of the
/// program, so that what it holds can be handed out as `&'static`.
type Cells = Vec<(TypeId, &'static OnceLock<PluginInfo>)>;

static CELLS: Mutex<Cells> = Mutex::new(Vec::new());

/// The path of the library, once known.
static LIBRARY_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Whether `id` can be a plugin's id: not empty, and with no white space
/// and no control characters, which saved state, CLAP's C strings and
/// hosts' session files could not carry.
///
/// ```
/// use manguebyte_plugin::is_valid_id;
///
/// assert!(is_valid_id("org.example.gain"));
/// assert!(is_valid_id("parabolic.k7m2q9xh4cv3bnw8"));
/// assert!(!is_valid_id(""));
/// assert!(!is_valid_id("org.example gain"));
/// assert!(!is_valid_id("org.example.gain\0"));
/// ```
pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The identity of `P` for the life of the library: what [`Plugin::info`]
/// returns, asked once.
///
/// The first call for a plugin type calls `P::info()`, and every later call
/// returns the same value, from any thread. Two rules make the result safe
/// for every wrapper to depend on:
///
/// - the [`kind`](PluginInfo::kind) is always [`Plugin::INFO`]'s, since a
///   plugin's buses, and VST 3's parameters for MIDI controllers, are fixed
///   when it is compiled;
/// - if `info()` gives an id that [`is_valid_id`] refuses, the whole of
///   `INFO` is used instead.
///
/// Every call takes a lock briefly, to find the type's cell; the first
/// then runs `info()` outside it, and takes as long as that does. Calls
/// made meanwhile wait for its answer. The wrappers call this when a
/// host first asks about the plugin, before any instance exists, and never
/// on the audio thread. If `info()` panics, nothing is kept and the next
/// call asks again.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessContext, ProcessSpec};
/// use manguebyte_params::param_set;
/// use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind, plugin_info};
///
/// struct NoParams {}
/// param_set!(NoParams {});
/// struct Engine(Arc<NoParams>);
///
/// impl Plugin for Engine {
///     const INFO: PluginInfo = PluginInfo {
///         id: "org.example.engine",
///         name: "Engine",
///         vendor: "Example",
///         version: "1.0.0",
///         description: "",
///         kind: PluginKind::Instrument,
///     };
///     const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
///     type Params = NoParams;
///
///     // Worked out when the library is loaded. A real engine would read
///     // it from data next to its `library_path()`.
///     fn info() -> PluginInfo {
///         let name = format!("Engine #{}", 2);
///         PluginInfo { name: name.leak(), ..Self::INFO }
///     }
///     fn new() -> Self { Self(Arc::new(NoParams {})) }
///     fn params(&self) -> Arc<NoParams> { Arc::clone(&self.0) }
///     fn prepare(&mut self, _: &ProcessSpec) {}
///     fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
/// }
///
/// let info = plugin_info::<Engine>();
/// assert_eq!(info.name, "Engine #2");
/// assert_eq!(info.id, "org.example.engine");
/// // The same value every time, from one call to `info`.
/// assert!(std::ptr::eq(info, plugin_info::<Engine>()));
/// ```
pub fn plugin_info<P: Plugin>() -> &'static PluginInfo {
    let key = TypeId::of::<P>();
    let cell = {
        let mut cells = CELLS.lock().unwrap_or_else(|poison| poison.into_inner());
        match cells.iter().find(|(id, _)| *id == key) {
            Some(&(_, cell)) => cell,
            None => {
                let cell: &'static OnceLock<PluginInfo> = Box::leak(Box::new(OnceLock::new()));
                cells.push((key, cell));
                cell
            }
        }
    };
    // Outside the lock: `info` may take its time, and may ask for the
    // identity of another plugin type.
    cell.get_or_init(|| {
        let info = P::info();
        if is_valid_id(info.id) {
            PluginInfo {
                kind: P::INFO.kind,
                ..info
            }
        } else {
            P::INFO
        }
    })
}

/// The file the plugin's code was loaded from: the plugin's shared
/// library, or the program's executable when a plugin runs inside one,
/// as in the [`Harness`](crate::Harness) and in tests. `None` where the
/// system cannot say.
///
/// It is the library's file itself, never a bundle's folder: on macOS,
/// `Name.vst3/Contents/MacOS/Name`, whose bundle's resources are in
/// `../Resources`. It can be read from anywhere, [`Plugin::info`] included,
/// and is the same for every plugin type in the library.
///
/// The first of these that happens decides it, for the life of the
/// library:
///
/// - a wrapper records the path the host gave it, with
///   [`set_library_path`]: the CLAP wrapper does in `clap_entry.init`,
///   which a host calls before anything else, when the path it is given is
///   a file;
/// - otherwise it is asked of the system, by an address inside the library,
///   on the first call: `GetModuleHandleExW` and `GetModuleFileNameW` on
///   Windows, `dladdr` elsewhere (see
///   [`manguebyte_core::module::file`]).
pub fn library_path() -> Option<&'static Path> {
    LIBRARY_PATH
        .get_or_init(manguebyte_core::module::file)
        .as_deref()
}

/// Records `path` as the library's own path, for [`library_path`], if
/// nothing was recorded or asked of the system yet. Returns whether it was
/// recorded. For wrappers, whose format's entry point passes the path.
pub fn set_library_path(path: &Path) -> bool {
    LIBRARY_PATH.set(Some(path.to_owned())).is_ok()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use manguebyte_core::AudioBuffer;
    use manguebyte_dsp::{ProcessContext, ProcessSpec};
    use manguebyte_params::param_set;

    use super::*;
    use crate::{AudioLayout, PluginKind};

    struct NoParams {}
    param_set!(NoParams {});

    static ASKED: AtomicUsize = AtomicUsize::new(0);

    /// A plugin whose identity is made when it is first asked for, and
    /// says it is an effect although it was compiled as an instrument.
    struct Made(Arc<NoParams>);

    impl Plugin for Made {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.made",
            name: "Compiled",
            vendor: "Compiled",
            version: "0.0.0",
            description: "",
            kind: PluginKind::Instrument,
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        type Params = NoParams;

        fn info() -> PluginInfo {
            ASKED.fetch_add(1, Ordering::SeqCst);
            PluginInfo {
                id: format!("org.manguebyte.test.made-{}", 7).leak(),
                name: "Made",
                vendor: "At run time",
                version: "2.0.0",
                description: "Found when loaded",
                kind: PluginKind::Effect,
            }
        }
        fn new() -> Self {
            Self(Arc::new(NoParams {}))
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    }

    /// One whose run-time id could not be saved or hosted.
    struct Spaced(Arc<NoParams>);

    impl Plugin for Spaced {
        const INFO: PluginInfo = PluginInfo {
            id: "org.manguebyte.test.spaced",
            name: "Spaced",
            ..Made::INFO
        };
        const LAYOUT: AudioLayout = AudioLayout::STEREO_INSTRUMENT;
        type Params = NoParams;

        fn info() -> PluginInfo {
            PluginInfo {
                id: "org.manguebyte.test spaced",
                name: "Other",
                ..Self::INFO
            }
        }
        fn new() -> Self {
            Self(Arc::new(NoParams {}))
        }
        fn params(&self) -> Arc<NoParams> {
            Arc::clone(&self.0)
        }
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
    }

    #[test]
    fn the_identity_is_asked_once_and_keeps_the_compiled_kind() {
        let threads: Vec<_> = (0..8)
            .map(|_| std::thread::spawn(|| plugin_info::<Made>() as *const PluginInfo as usize))
            .collect();
        let addresses: Vec<usize> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(addresses.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(ASKED.load(Ordering::SeqCst), 1);

        let info = plugin_info::<Made>();
        assert_eq!(info.id, "org.manguebyte.test.made-7");
        assert_eq!(
            (info.name, info.vendor, info.version, info.description),
            ("Made", "At run time", "2.0.0", "Found when loaded")
        );
        assert_eq!(info.kind, PluginKind::Instrument);
        assert_eq!(ASKED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_id_that_cannot_be_used_gives_the_compiled_identity() {
        assert_eq!(*plugin_info::<Spaced>(), Spaced::INFO);
    }

    #[test]
    fn the_library_path_is_this_test_executable() {
        // Nothing in this executable records a path, so the system is asked.
        let path = library_path();
        if cfg!(all(windows, not(miri))) {
            let expected = std::env::current_exe().unwrap().canonicalize().unwrap();
            assert_eq!(path.unwrap().canonicalize().unwrap(), expected);
        }
        // Decided once: a later record changes nothing.
        assert!(!set_library_path(Path::new("elsewhere.dll")));
        assert_eq!(library_path(), path);
    }
}
