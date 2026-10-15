//! Two separately built plugin binaries in one process, with their editors
//! open on real Win32 windows at the same time (ADR 0007, decision 7).
//!
//! Mangue Duck and Mangue Pluck are each a shared library with its own
//! copy of the framework, its window code included. A host may load both,
//! open both editors, close them in any order and unload the libraries in
//! any order, and nothing of one may depend on the other: no window class
//! of the same name, nothing left registered after the last window is
//! gone, no leaked GDI or USER object, and nothing that runs after its
//! library is unloaded.
//!
//! The test is a small CLAP host. It loads each library with
//! `LoadLibraryW`, reaches `clap_entry`, creates the plugin through the
//! factory and drives its `gui` extension as a host does, with a
//! top-level window of its own as the parent. It runs the message loop,
//! so the editors' frame timers fire and they paint.
//!
//! `two_debug_binaries_open_and_close_editors_and_unload_in_both_orders`
//! loads the libraries cargo builds next to this test's executable: this
//! package's own, and Mangue Pluck's, a dev-dependency for that reason.
//! `two_bundled_binaries_...` loads the release bundles instead; it is
//! ignored by default and expects them to exist. Build them with
//!
//! ```text
//! ./scripts/bundle-clap.sh mangue-duck "Mangue Duck"
//! ./scripts/bundle-clap.sh mangue-pluck "Mangue Pluck"
//! cargo test -p mangue-duck --test two_binaries -- --ignored
//! ```
//!
//! Both count the process's GDI and USER objects, so they never run at
//! the same time: a lock keeps them apart.
//!
//! The pixel checks read the parent window back from the screen. In a
//! session that cannot read windows back they fail, unless
//! `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is set (CI never sets it); then they
//! say so on stderr and are skipped.
#![cfg(windows)]
// Every `unsafe` block here is a call across the CLAP interface or to the
// system's loader, with pointers this test owns and keeps alive for the
// call; each block says what it relies on.

use std::cell::RefCell;
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use manguebyte::clap::clap_sys::entry::clap_plugin_entry;
use manguebyte::clap::clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_host_gui, clap_plugin_gui, clap_window,
    clap_window_handle,
};
use manguebyte::clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte::clap::clap_sys::host::clap_host;
use manguebyte::clap::clap_sys::plugin::clap_plugin;
use manguebyte::clap::clap_sys::version::CLAP_VERSION;
use manguebyte_gfx::IRect;
use manguebyte_window::win32::testing::{self, TopLevel, WindowClass};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

/// Keeps the tests of this file from running at the same time: they count
/// the whole process's GDI and USER objects.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The background of the editors' theme, as the screen shows it.
const BACKGROUND: [u8; 4] = [0x16, 0x18, 0x1c, 0xff];

// --- Loading a plugin binary ---------------------------------------------

/// A plugin binary, loaded and initialised.
struct Library {
    path: PathBuf,
    module: HMODULE,
    entry: *const clap_plugin_entry,
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain([0]).collect()
}

/// Whether the system has a module loaded from `path`.
fn is_loaded(path: &Path) -> bool {
    let name = wide(path);
    // SAFETY: a NUL-terminated path; GetModuleHandleW takes no reference.
    !unsafe { GetModuleHandleW(name.as_ptr()) }.is_null()
}

impl Library {
    fn load(path: &Path) -> Library {
        assert!(path.is_file(), "no plugin binary at {}", path.display());
        assert!(!is_loaded(path), "{} is already loaded", path.display());
        let name = wide(path);
        // SAFETY: a NUL-terminated path to a plugin binary built from this
        // repository.
        let module = unsafe { LoadLibraryW(name.as_ptr()) };
        assert!(!module.is_null(), "{} did not load", path.display());
        // SAFETY: `module` is loaded; the name is NUL-terminated.
        let symbol = unsafe { GetProcAddress(module, c"clap_entry".as_ptr().cast()) }
            .expect("the binary exports clap_entry");
        let entry = (symbol as *const ()).cast::<clap_plugin_entry>();
        let text = CString::new(path.to_string_lossy().as_bytes()).expect("no NUL in a path");
        // SAFETY: `clap_entry` is a `clap_plugin_entry` that lives as long
        // as the module; `text` is NUL-terminated.
        assert!(unsafe { (*entry).init.expect("init")(text.as_ptr()) });
        Library {
            path: path.to_owned(),
            module,
            entry,
        }
    }

    fn factory(&self) -> *const clap_plugin_factory {
        // SAFETY: the entry is initialised.
        let factory = unsafe {
            (*self.entry).get_factory.expect("get_factory")(CLAP_PLUGIN_FACTORY_ID.as_ptr())
        };
        assert!(!factory.is_null());
        factory.cast()
    }

    /// The id of the binary's one plugin.
    fn plugin_id(&self) -> CString {
        let factory = self.factory();
        // SAFETY: a live factory, and index 0 of the one plugin it has.
        unsafe {
            assert_eq!((*factory).get_plugin_count.expect("count")(factory), 1);
            let descriptor = (*factory).get_plugin_descriptor.expect("descriptor")(factory, 0);
            CStr::from_ptr((*descriptor).id).to_owned()
        }
    }

    /// Deinitialises and unloads the binary, and checks that it is gone.
    fn unload(self) {
        // SAFETY: the entry was initialised once and every plugin made
        // from it has been destroyed.
        unsafe { (*self.entry).deinit.expect("deinit")() };
        // SAFETY: the module this value loaded, released once.
        assert_ne!(unsafe { FreeLibrary(self.module) }, 0);
        assert!(
            !is_loaded(&self.path),
            "{} is still loaded",
            self.path.display()
        );
    }
}

// --- A plugin and the host it is given ------------------------------------

/// What the host side of one plugin records.
#[derive(Default)]
struct HostState {
    /// Calls to the host's `gui` extension: resize requests and closes.
    gui_calls: RefCell<Vec<String>>,
}

/// # Safety
///
/// `host` must be one of the hosts this file makes.
unsafe fn state<'a>(host: *const clap_host) -> &'a HostState {
    // SAFETY: `host_data` points to the `HostState` boxed beside it.
    unsafe { &*(*host).host_data.cast::<HostState>() }
}

unsafe extern "C" fn host_get_extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    // SAFETY: the plugin passes a NUL-terminated id.
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_GUI {
        ptr::from_ref(&HOST_GUI).cast()
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn host_nothing(_: *const clap_host) {}

unsafe extern "C" fn host_resize_hints_changed(_: *const clap_host) {}

unsafe extern "C" fn host_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    // SAFETY: one of this file's hosts.
    let state = unsafe { state(host) };
    state
        .gui_calls
        .borrow_mut()
        .push(format!("request_resize({width}, {height})"));
    false
}

unsafe extern "C" fn host_request_show_or_hide(_: *const clap_host) -> bool {
    false
}

unsafe extern "C" fn host_closed(host: *const clap_host, was_destroyed: bool) {
    // SAFETY: one of this file's hosts.
    let state = unsafe { state(host) };
    state
        .gui_calls
        .borrow_mut()
        .push(format!("closed({was_destroyed})"));
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_resize_hints_changed),
    request_resize: Some(host_request_resize),
    request_show: Some(host_request_show_or_hide),
    request_hide: Some(host_request_show_or_hide),
    closed: Some(host_closed),
};

/// A plugin created from a [`Library`], with its `gui` extension.
struct Instance {
    name: &'static str,
    host: *mut clap_host,
    state: *mut HostState,
    plugin: *const clap_plugin,
    gui: *const clap_plugin_gui,
}

impl Instance {
    fn create(library: &Library, name: &'static str) -> Instance {
        let state = Box::into_raw(Box::new(HostState::default()));
        let host = Box::into_raw(Box::new(clap_host {
            clap_version: CLAP_VERSION,
            host_data: state.cast(),
            name: c"two binaries".as_ptr(),
            vendor: c"Manguebyte".as_ptr(),
            url: c"".as_ptr(),
            version: c"1".as_ptr(),
            get_extension: Some(host_get_extension),
            request_restart: Some(host_nothing),
            request_process: Some(host_nothing),
            request_callback: Some(host_nothing),
        }));
        let id = library.plugin_id();
        let factory = library.factory();
        // SAFETY: a live factory, a host that outlives the plugin (it is
        // freed in `destroy`, after the plugin) and a NUL-terminated id.
        unsafe {
            let plugin =
                (*factory).create_plugin.expect("create_plugin")(factory, host, id.as_ptr());
            assert!(!plugin.is_null(), "{name} was created");
            assert!((*plugin).init.expect("init")(plugin), "{name} initialised");
            let gui =
                (*plugin).get_extension.expect("get_extension")(plugin, CLAP_EXT_GUI.as_ptr())
                    .cast::<clap_plugin_gui>();
            assert!(!gui.is_null(), "{name} offers a gui");
            Instance {
                name,
                host,
                state,
                plugin,
                gui,
            }
        }
    }

    fn table(&self) -> &clap_plugin_gui {
        // SAFETY: the extension lives as long as the plugin.
        unsafe { &*self.gui }
    }

    /// Opens the editor inside `parent` at `scale`, as a host does, and
    /// returns the editor's window: the one window now inside `parent`.
    fn open(&self, parent: &TopLevel, scale: f64) -> usize {
        let gui = self.table();
        let api = CLAP_WINDOW_API_WIN32.as_ptr();
        let handle = parent.handle().expect("open");
        let (mut width, mut height) = (0_u32, 0_u32);
        let window = clap_window {
            api,
            specific: clap_window_handle {
                win32: ptr::with_exposed_provenance_mut(handle),
            },
        };
        // SAFETY: a live plugin; every pointer is to a local that outlives
        // the call.
        unsafe {
            assert!(gui.is_api_supported.expect("is_api_supported")(
                self.plugin,
                api,
                false
            ));
            assert!(gui.create.expect("create")(self.plugin, api, false));
            assert!(gui.set_scale.expect("set_scale")(self.plugin, scale));
            assert!(gui.get_size.expect("get_size")(
                self.plugin,
                &mut width,
                &mut height
            ));
            assert!(gui.set_parent.expect("set_parent")(self.plugin, &window));
            assert!(gui.show.expect("show")(self.plugin));
        }
        let children = testing::children_of(handle);
        assert_eq!(
            children.len(),
            1,
            "{}: one window inside the parent",
            self.name
        );
        let child = children[0];
        assert_eq!(testing::parent_of_any(child), Some(handle));
        assert_eq!(
            testing::client_size_of(child),
            Some((width, height)),
            "{}: the size the host was told",
            self.name
        );
        child
    }

    /// Hides and destroys the editor, as a host closing it does.
    fn close(&self) {
        let gui = self.table();
        // SAFETY: a live plugin with its editor created.
        unsafe {
            assert!(gui.hide.expect("hide")(self.plugin));
            gui.destroy.expect("destroy")(self.plugin);
        }
    }

    fn gui_calls(&self) -> Vec<String> {
        // SAFETY: the state lives until `destroy`.
        unsafe { (*self.state).gui_calls.borrow().clone() }
    }

    fn destroy(self) {
        assert_eq!(self.gui_calls(), Vec::<String>::new(), "{}", self.name);
        // SAFETY: a live plugin, destroyed once, then the host and state it
        // pointed to, which nothing uses any more.
        unsafe {
            (*self.plugin).destroy.expect("destroy")(self.plugin);
            drop(Box::from_raw(self.host));
            drop(Box::from_raw(self.state));
        }
    }
}

// --- What is checked -------------------------------------------------------

/// Runs the message loop until the editor `child` inside `parent` shows
/// its theme's background at `(4, 4)`, or, where the session cannot read
/// windows back, for a fixed while.
fn shows_its_background(parent: &TopLevel, child: usize, readable: bool, name: &str) {
    let corner = testing::position_in_parent(child).expect("a window");
    if !readable {
        testing::pump(Duration::from_millis(200));
        return;
    }
    let spot = IRect::new(corner.0 + 4, corner.1 + 4, 1, 1);
    let mut last = None;
    let shown = testing::pump_until(Duration::from_secs(5), || {
        let pixel = parent
            .read_back(spot)
            .ok()
            .and_then(|pixels| pixels.pixel(0, 0).map(|p| p.to_array()));
        last = pixel;
        pixel == Some(BACKGROUND)
    });
    assert!(shown, "{name}: the screen shows {last:?} at {spot:?}");
}

/// The class of the editor window `child`, checked against the other
/// binaries in the process.
fn class(child: usize, name: &str) -> WindowClass {
    let class = testing::class_of(child).expect("a window");
    assert!(testing::class_exists(&class), "{name}");
    assert_ne!(
        class.name,
        testing::window_class_name(),
        "{name}: not the test executable's class"
    );
    class
}

fn duck_and_pluck(folder: &Path, duck: &str, pluck: &str) -> (PathBuf, PathBuf) {
    (folder.join(duck), folder.join(pluck))
}

/// The scenario, for two binaries at `duck` and `pluck`.
fn two_binaries(duck_path: &Path, pluck_path: &Path) {
    let _alone = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let first = TopLevel::new(500, 260).expect("a desktop");
    let second = TopLevel::new(300, 260).expect("a desktop");
    let readable = testing::require_read_back(&first, IRect::new(470, 240, 20, 16));
    testing::pump(Duration::from_millis(50));
    let before = testing::gui_resources();
    let mut classes: Vec<WindowClass> = Vec::new();

    // --- Round 1: both open at once; closed and unloaded duck first.
    let duck = Library::load(duck_path);
    let pluck = Library::load(pluck_path);
    let d = Instance::create(&duck, "Mangue Duck");
    let p = Instance::create(&pluck, "Mangue Pluck");
    let d_window = d.open(&first, 1.0);
    let p_window = p.open(&second, 1.5);
    assert_eq!(testing::client_size_of(d_window), Some((440, 210)));
    assert_eq!(testing::client_size_of(p_window), Some((270, 225)));
    shows_its_background(&first, d_window, readable, "Mangue Duck");
    shows_its_background(&second, p_window, readable, "Mangue Pluck");
    let d_class = class(d_window, "Mangue Duck");
    let p_class = class(p_window, "Mangue Pluck");
    // Each binary registered a class of its own, under its own module.
    assert_ne!(d_class.name, p_class.name);
    assert_ne!(d_class.module, p_class.module);
    classes.extend([d_class.clone(), p_class.clone()]);

    d.close();
    assert!(!testing::is_window(d_window), "the duck's window is gone");
    assert!(!testing::class_exists(&d_class), "the duck's class is gone");
    // The other editor is untouched and still paints.
    assert!(testing::is_window(p_window));
    assert!(testing::class_exists(&p_class));
    testing::pump(Duration::from_millis(50));
    d.destroy();
    duck.unload();
    // With the duck's binary gone, the pluck's editor goes on running its
    // frames.
    testing::pump(Duration::from_millis(100));
    assert!(testing::is_window(p_window));
    p.close();
    assert!(!testing::class_exists(&p_class));
    p.destroy();
    pluck.unload();
    testing::pump(Duration::from_millis(50));
    assert_eq!(testing::gui_resources(), before, "after round 1");

    // --- Round 2: loaded the other way round; the pluck's binary is
    // unloaded while the duck's editor is open, and the duck's editor is
    // opened and closed many times while the pluck's stays open.
    let pluck = Library::load(pluck_path);
    let duck = Library::load(duck_path);
    let p = Instance::create(&pluck, "Mangue Pluck");
    let d = Instance::create(&duck, "Mangue Duck");
    let p_window = p.open(&second, 1.0);
    for cycle in 0..20 {
        let scale = [1.0, 1.25, 1.5, 2.0][cycle % 4];
        let d_window = d.open(&first, scale);
        if cycle % 5 == 0 {
            shows_its_background(&first, d_window, readable, "Mangue Duck");
        } else {
            testing::pump(Duration::from_millis(20));
        }
        let d_class = class(d_window, "Mangue Duck");
        d.close();
        assert!(!testing::class_exists(&d_class), "cycle {cycle}");
        classes.push(d_class);
    }
    let d_window = d.open(&first, 1.0);
    let d_class = class(d_window, "Mangue Duck");
    let p_class = class(p_window, "Mangue Pluck");
    p.close();
    p.destroy();
    pluck.unload();
    assert!(!testing::class_exists(&p_class));
    shows_its_background(&first, d_window, readable, "Mangue Duck");
    testing::pump(Duration::from_millis(100));
    assert!(testing::is_window(d_window));
    d.close();
    assert!(!testing::class_exists(&d_class));
    d.destroy();
    duck.unload();
    classes.extend([d_class, p_class]);
    testing::pump(Duration::from_millis(50));
    assert_eq!(testing::gui_resources(), before, "after round 2");

    // --- Round 3: both open; the pluck closed first, the duck unloaded
    // last.
    let duck = Library::load(duck_path);
    let pluck = Library::load(pluck_path);
    let d = Instance::create(&duck, "Mangue Duck");
    let p = Instance::create(&pluck, "Mangue Pluck");
    let d_window = d.open(&first, 2.0);
    let p_window = p.open(&second, 1.0);
    assert_eq!(testing::client_size_of(d_window), Some((880, 420)));
    shows_its_background(&second, p_window, readable, "Mangue Pluck");
    let d_class = class(d_window, "Mangue Duck");
    let p_class = class(p_window, "Mangue Pluck");
    p.close();
    d.close();
    p.destroy();
    d.destroy();
    pluck.unload();
    duck.unload();
    classes.extend([d_class, p_class]);
    testing::pump(Duration::from_millis(50));
    assert_eq!(testing::gui_resources(), before, "after round 3");

    // No class of either binary is left registered, under any module it
    // was ever loaded at.
    for class in &classes {
        assert!(!testing::class_exists(class), "{class:?} leaked");
    }
    drop(first);
    drop(second);
}

/// The folder cargo builds this test's executable and its dependencies'
/// shared libraries in.
fn deps_folder() -> PathBuf {
    std::env::current_exe()
        .expect("the test executable has a path")
        .parent()
        .expect("the test executable is in a folder")
        .to_owned()
}

#[test]
fn two_debug_binaries_open_and_close_editors_and_unload_in_both_orders() {
    let (duck, pluck) = duck_and_pluck(
        &deps_folder(),
        &format!("{DLL_PREFIX}mangue_duck{DLL_SUFFIX}"),
        &format!("{DLL_PREFIX}mangue_pluck{DLL_SUFFIX}"),
    );
    two_binaries(&duck, &pluck);
}

#[test]
#[ignore = "needs the release bundles: see the file's documentation"]
fn two_bundled_binaries_open_and_close_editors_and_unload_in_both_orders() {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bundled");
    let (duck, pluck) = duck_and_pluck(&bundled, "Mangue Duck.clap", "Mangue Pluck.clap");
    two_binaries(&duck, &pluck);
}
