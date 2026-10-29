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
//! Two scenarios run on each pair of binaries:
//!
//! - `..._open_and_close_editors_and_unload_in_both_orders` opens both
//!   editors at once, closes and unloads them in both orders, and opens and
//!   closes one editor twenty times while the other stays open;
//! - `..._take_random_gui_calls_and_leave_nothing_behind` makes random
//!   `gui` calls on a plugin of each binary with hostile arguments (wrong
//!   or missing window APIs, floating windows, nonsense scales and sizes,
//!   handles that are no window, calls out of order), while the host
//!   sometimes grants a resize request by calling `set_size` from inside
//!   it and sometimes destroys its own window under an open editor. After
//!   every call it checks the answer, the resize requests and the windows
//!   and their sizes against a model of CLAP's rules (an open editor of
//!   fixed size takes only its own size from `set_size`, and asks the host
//!   for its size at a new scale, which it gets if the host grants it); at
//!   the end it unloads the binaries in the order the seed picks and checks
//!   that nothing is left.
//!
//! The `two_debug_binaries_...` tests load the libraries cargo builds next
//! to this test's executable: this package's own, and Mangue Pluck's, a
//! dev-dependency for that reason. (The test must not name anything of
//! either crate: that would link both into the executable, and their
//! exported entry points would clash.) These are not the binaries a person
//! installs: they are debug builds, and cargo builds them with this
//! package's dev-dependency features unified in, `manguebyte-window`'s
//! `testing` and `manguebyte-gui`'s `headless`. Both only add API, and an
//! editor given no headless hook, as here, opens in real Win32 windows all
//! the same; but the code differs from what `scripts/bundle-clap.sh`
//! ships. The configuration that ships is checked by the
//! `two_bundled_binaries_...` tests, which load the release bundles; they
//! are ignored by default and expect the bundles to exist. Build them and
//! run the tests with
//!
//! ```text
//! ./scripts/bundle-clap.sh mangue-duck "Mangue Duck"
//! ./scripts/bundle-clap.sh mangue-pluck "Mangue Pluck"
//! cargo test -p mangue-duck --test two_binaries -- --ignored
//! ```
//!
//! They all count the process's GDI and USER objects, so they never run at
//! the same time: a lock keeps them apart. A test that fails leaves its
//! libraries loaded, so the tests after it in the same run fail on loading
//! them again; the first failure is the one to read. Its windows are
//! destroyed all the same, with the parents they are inside.
//!
//! The pixel checks read the parent window back from the screen. In a
//! session that cannot read windows back they fail, unless
//! `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is set (CI never sets it); then they
//! say so on stderr and are skipped.
#![cfg(windows)]
// Every `unsafe` block here is a call across the CLAP interface or to the
// system's loader, with pointers this test owns and keeps alive for the
// call; each block says what it relies on.

use std::cell::{Cell, RefCell};
use std::env::consts::{DLL_PREFIX, DLL_SUFFIX};
use std::ffi::{CStr, CString, c_char, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::Mutex;
use std::time::Duration;

use manguebyte::clap::clap_sys::entry::clap_plugin_entry;
use manguebyte::clap::clap_sys::ext::gui::{
    CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_gui_resize_hints, clap_host_gui, clap_plugin_gui,
    clap_window, clap_window_handle,
};
use manguebyte::clap::clap_sys::factory::plugin_factory::{
    CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory,
};
use manguebyte::clap::clap_sys::host::clap_host;
use manguebyte::clap::clap_sys::plugin::clap_plugin;
use manguebyte::clap::clap_sys::version::CLAP_VERSION;
use manguebyte::plugin::editor::HostSize;
use manguebyte_gfx::IRect;
use manguebyte_window::win32::testing::{self, TopLevel, WindowClass};
use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};

/// Keeps the tests of this file from running at the same time: they count
/// the whole process's GDI and USER objects.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

/// The background of the editors' theme, as the screen shows it.
const BACKGROUND: [u8; 4] = [0x10, 0x11, 0x16, 0xff];

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
struct HostState {
    /// Calls to the host's `gui` extension: resize requests and closes.
    gui_calls: RefCell<Vec<String>>,
    /// Whether the host grants a resize request, by calling the plugin's
    /// `set_size` from inside it, as CLAP allows. Otherwise it refuses.
    grant_resize: Cell<bool>,
    /// The plugin and its `gui` extension, once created, for that call.
    plugin: Cell<*const clap_plugin>,
    gui: Cell<*const clap_plugin_gui>,
}

impl Default for HostState {
    fn default() -> Self {
        HostState {
            gui_calls: RefCell::default(),
            grant_resize: Cell::new(false),
            plugin: Cell::new(ptr::null()),
            gui: Cell::new(ptr::null()),
        }
    }
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
    let (plugin, gui) = (state.plugin.get(), state.gui.get());
    if !state.grant_resize.get() || plugin.is_null() || gui.is_null() {
        return false;
    }
    // The host makes its window the size asked for and tells the plugin
    // before it returns.
    // SAFETY: the plugin and its extension are live while it calls the
    // host; the wrapper takes `set_size` from inside this request as its
    // answer.
    unsafe { (*gui).set_size.expect("set_size")(plugin, width, height) }
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
            (*state).plugin.set(plugin);
            (*state).gui.set(gui);
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

    /// The calls to the host's `gui` extension since the last time, which
    /// are then forgotten.
    fn take_gui_calls(&self) -> Vec<String> {
        // SAFETY: the state lives until `destroy`.
        unsafe { (*self.state).gui_calls.take() }
    }

    fn host_state(&self) -> &HostState {
        // SAFETY: the state lives until `destroy`, which takes `self`.
        unsafe { &*self.state }
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
    let second = TopLevel::new(560, 260).expect("a desktop");
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
    assert_eq!(testing::client_size_of(p_window), Some((780, 240)));
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

// --- Random orders of `gui` calls ------------------------------------------

/// A small generator of numbers, the same on every run.
struct Numbers(u64);

impl Numbers {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// Window API names a host might pass, most of them wrong.
const APIS: [Option<&CStr>; 7] = [
    Some(c"win32"),
    Some(c"win32"),
    Some(c"cocoa"),
    Some(c"x11"),
    Some(c"WIN32"),
    Some(c""),
    None,
];

/// Scales a host might ask for, some of them nonsense.
const SCALES: [f64; 10] = [
    1.0,
    1.25,
    1.5,
    2.0,
    3.0,
    0.0,
    -1.0,
    f64::NAN,
    f64::INFINITY,
    1e300,
];

/// What the test expects of one plugin's `gui` extension: CLAP's rules as
/// the wrapper documents them, kept beside the real plugin and compared
/// with it after every call.
struct Model<'a> {
    instance: Instance,
    library: &'a Library,
    /// The editor's size at scale 1.
    base: HostSize,
    /// The size of the editor's window while it is open, as it must be
    /// after every call.
    size: Option<(u32, u32)>,
    /// Its parent, which the host may destroy and make again.
    parent: TopLevel,
    parent_size: (u32, u32),
    created: bool,
    /// The scale set since `create`, if any.
    scale: Option<f64>,
    /// The editor's window, from a `set_parent` that succeeded until
    /// `destroy`, and whether the host destroyed the parent under it.
    window: Option<usize>,
    parent_lost: bool,
    /// How many times the host must have been told the editor closed.
    closes: usize,
    /// What happened, to show that the calls reached the interesting
    /// cases: windows opened, closes the host was told of, resize
    /// requests, and scales and sizes set while the editor was open.
    seen: [usize; 5],
}

impl Model<'_> {
    fn name(&self) -> &'static str {
        self.instance.name
    }

    fn plugin(&self) -> *const clap_plugin {
        self.instance.plugin
    }

    fn gui(&self) -> &clap_plugin_gui {
        self.instance.table()
    }

    /// The size the editor has before it opens: its size at the scale set.
    fn size_before_open(&self) -> HostSize {
        self.base.scale_by(self.scale.unwrap_or(1.0))
    }

    /// The size `get_size` gives, or `None` when it gives nothing.
    fn get_size(&self) -> Option<(u32, u32)> {
        let (mut width, mut height) = (0_u32, 0_u32);
        // SAFETY: a live plugin, and two places this function owns.
        let ok = unsafe {
            self.gui().get_size.expect("get_size")(self.plugin(), &mut width, &mut height)
        };
        ok.then_some((width, height))
    }

    /// One call, chosen and given arguments by `numbers`, checked against
    /// what CLAP and the wrapper promise.
    fn step(&mut self, numbers: &mut Numbers, step: usize) {
        let gui = *self.gui();
        let plugin = self.plugin();
        let at = format!("{} at step {step}", self.name());
        let open = self.window.is_some();
        if self.parent_lost && numbers.below(4) == 0 {
            // The host, told the editor closed, destroys it.
            self.destroy(&at);
            self.check(&at);
            return;
        }
        match numbers.below(20) {
            0 => {
                let api = numbers.pick(&APIS);
                let floating = numbers.below(4) == 0;
                let wanted = api == Some(c"win32") && !floating;
                // SAFETY: a live plugin and a NUL-terminated name or null.
                let got = unsafe {
                    gui.is_api_supported.expect("is_api_supported")(
                        plugin,
                        api.map_or(ptr::null(), CStr::as_ptr),
                        floating,
                    )
                };
                assert_eq!(got, wanted, "is_api_supported({api:?}, {floating}), {at}");
            }
            1 => {
                let mut api: *const c_char = ptr::null();
                let mut floating = true;
                let places = numbers.below(4) != 0;
                // SAFETY: a live plugin, and places this function owns, or
                // null.
                let got = unsafe {
                    if places {
                        gui.get_preferred_api.expect("get_preferred_api")(
                            plugin,
                            &mut api,
                            &mut floating,
                        )
                    } else {
                        gui.get_preferred_api.expect("get_preferred_api")(
                            plugin,
                            ptr::null_mut(),
                            ptr::null_mut(),
                        )
                    }
                };
                assert_eq!(got, places, "get_preferred_api, {at}");
                if places {
                    // SAFETY: the plugin wrote a static NUL-terminated name.
                    assert_eq!(unsafe { CStr::from_ptr(api) }, c"win32", "{at}");
                    assert!(!floating, "{at}");
                }
            }
            2..=4 => {
                let api = if numbers.below(3) == 0 {
                    numbers.pick(&APIS)
                } else {
                    Some(c"win32")
                };
                let floating = numbers.below(6) == 0;
                let wanted = api == Some(c"win32") && !floating && !self.created;
                // SAFETY: a live plugin and a NUL-terminated name or null.
                let got = unsafe {
                    gui.create.expect("create")(
                        plugin,
                        api.map_or(ptr::null(), CStr::as_ptr),
                        floating,
                    )
                };
                assert_eq!(got, wanted, "create({api:?}, {floating}), {at}");
                if got {
                    self.created = true;
                    self.scale = None;
                }
            }
            5 => self.destroy(&at),
            7..=8 => {
                let scale = numbers.pick(&SCALES);
                let valid = manguebyte::plugin::editor::checked_scale(scale).is_some();
                let grant = numbers.below(2) == 0;
                self.instance.host_state().grant_resize.set(grant);
                let asked_before = self.instance.gui_calls().len();
                // SAFETY: a live plugin.
                let got = unsafe { gui.set_scale.expect("set_scale")(plugin, scale) };
                if !self.created || !valid {
                    assert!(!got, "set_scale({scale}), {at}");
                } else if !open {
                    assert!(got, "set_scale({scale}), {at}");
                } else if !self.parent_lost {
                    // An open editor takes the scale and asks the host for
                    // its size at that scale, unless it already has it; it
                    // gets it only if the host grants it.
                    assert!(got, "set_scale({scale}) while open, {at}");
                    self.seen[3] += 1;
                    let wanted = self.base.scale_by(scale);
                    let wanted = (wanted.width, wanted.height);
                    let current = self.size.expect("an open editor has a size");
                    let asked = self.instance.gui_calls()[asked_before..].to_vec();
                    if wanted == current {
                        assert_eq!(asked, Vec::<String>::new(), "set_scale({scale}), {at}");
                    } else {
                        assert_eq!(
                            asked,
                            [format!("request_resize({}, {})", wanted.0, wanted.1)],
                            "set_scale({scale}), {at}"
                        );
                        if grant {
                            self.size = Some(wanted);
                        }
                    }
                }
                if got {
                    self.scale = Some(scale);
                }
            }
            9 => {
                let got = self.get_size();
                assert_eq!(got.is_some(), self.created, "get_size, {at}");
                if self.created && !open {
                    let size = self.size_before_open();
                    assert_eq!(got, Some((size.width, size.height)), "{at}");
                }
            }
            10 => {
                let mut hints = clap_gui_resize_hints {
                    can_resize_horizontally: true,
                    can_resize_vertically: true,
                    preserve_aspect_ratio: true,
                    aspect_ratio_width: 7,
                    aspect_ratio_height: 7,
                };
                let (mut width, mut height) = (numbers.next() as u32, numbers.next() as u32);
                // SAFETY: a live plugin and places this function owns.
                unsafe {
                    assert!(!gui.can_resize.expect("can_resize")(plugin), "{at}");
                    assert!(
                        !gui.adjust_size.expect("adjust_size")(plugin, &mut width, &mut height),
                        "{at}"
                    );
                    assert!(
                        !gui.get_resize_hints.expect("get_resize_hints")(plugin, ptr::null_mut()),
                        "{at}"
                    );
                    let got = gui.get_resize_hints.expect("get_resize_hints")(plugin, &mut hints);
                    assert_eq!(got, self.created, "get_resize_hints, {at}");
                }
                if self.created {
                    // A fixed editor.
                    assert!(!hints.can_resize_horizontally && !hints.can_resize_vertically);
                    assert!(!hints.preserve_aspect_ratio);
                }
            }
            11 => {
                let fitting = self.size_before_open();
                let (width, height) = match numbers.below(3) {
                    0 => (fitting.width, fitting.height),
                    1 => (numbers.below(5_000) as u32, numbers.below(5_000) as u32),
                    _ => (u32::MAX, 0),
                };
                let (width, height) = match (self.size, numbers.below(4)) {
                    // The size an open editor has now.
                    (Some(current), 0) if open => current,
                    _ => (width, height),
                };
                // SAFETY: a live plugin.
                let got = unsafe { gui.set_size.expect("set_size")(plugin, width, height) };
                if !self.created {
                    assert!(!got, "set_size, {at}");
                } else if !open {
                    let wanted = (width, height) == (fitting.width, fitting.height);
                    assert_eq!(got, wanted, "set_size({width}, {height}), {at}");
                } else if !self.parent_lost {
                    // A fixed editor takes only the size it has; the window
                    // keeps it either way (checked below).
                    let wanted = Some((width, height)) == self.size;
                    assert_eq!(got, wanted, "set_size({width}, {height}) while open, {at}");
                    self.seen[4] += 1;
                }
            }
            6 | 12..=14 => self.set_parent(numbers, &at),
            15 => {
                // SAFETY: a live plugin.
                let got = unsafe {
                    if numbers.below(2) == 0 {
                        gui.show.expect("show")(plugin)
                    } else {
                        gui.hide.expect("hide")(plugin)
                    }
                };
                assert_eq!(got, self.created && open, "show or hide, {at}");
            }
            16 => {
                let title =
                    [c"A title".as_ptr(), c"".as_ptr(), ptr::null()][numbers.below(3) as usize];
                let window = clap_window {
                    api: CLAP_WINDOW_API_WIN32.as_ptr(),
                    specific: clap_window_handle {
                        win32: ptr::without_provenance_mut(0x10),
                    },
                };
                // SAFETY: a live plugin; a NUL-terminated title or null; a
                // window description that outlives the call.
                unsafe {
                    gui.suggest_title.expect("suggest_title")(plugin, title);
                    assert!(!gui.set_transient.expect("set_transient")(plugin, &window));
                    assert!(!gui.set_transient.expect("set_transient")(
                        plugin,
                        ptr::null()
                    ));
                }
            }
            17 => {
                // The host's loop calls the plugin's main-thread callback
                // whenever it likes.
                // SAFETY: a live plugin, on the main thread.
                unsafe { (*plugin).on_main_thread.expect("on_main_thread")(plugin) };
            }
            18 => {
                // The host destroys its own window, the editor's parent,
                // and makes another.
                if open && !self.parent_lost {
                    self.closes += 1;
                }
                let child = self.window;
                self.parent.destroy();
                if let Some(child) = child {
                    assert!(!testing::is_window(child), "{at}");
                    self.parent_lost = true;
                }
                self.parent =
                    TopLevel::new(self.parent_size.0, self.parent_size.1).expect("a desktop");
            }
            _ => testing::pump(Duration::from_millis(numbers.below(12))),
        }
        self.check(&at);
    }

    fn set_parent(&mut self, numbers: &mut Numbers, at: &str) {
        let gui = *self.gui();
        let plugin = self.plugin();
        let parent = self.parent.handle().expect("open");
        // Handles that are no window: null, and values the system never
        // gives a window.
        let (handle, real) = match numbers.below(6) {
            0 => (0, false),
            1 => (0x10, false),
            2 => (usize::MAX & !0xF, false),
            _ => (parent, true),
        };
        let api = if numbers.below(5) == 0 {
            numbers.pick(&APIS)
        } else {
            Some(c"win32")
        };
        let window = clap_window {
            api: api.map_or(ptr::null(), CStr::as_ptr),
            specific: clap_window_handle {
                win32: ptr::with_exposed_provenance_mut(handle),
            },
        };
        let described = numbers.below(8) != 0;
        let children = testing::children_of(parent);
        // SAFETY: a live plugin and a window description that outlives the
        // call, or null.
        let got = unsafe {
            gui.set_parent.expect("set_parent")(
                plugin,
                if described { &window } else { ptr::null() },
            )
        };
        let wanted =
            self.created && self.window.is_none() && described && real && api == Some(c"win32");
        assert_eq!(got, wanted, "set_parent({api:?}, {handle:#x}), {at}");
        let now = testing::children_of(parent);
        if !got {
            assert_eq!(now, children, "a refused set_parent made no window, {at}");
            return;
        }
        // Exactly one new window inside the parent, of a class the plugin
        // binary registered.
        let new: Vec<usize> = now
            .iter()
            .copied()
            .filter(|child| !children.contains(child))
            .collect();
        assert_eq!(new.len(), 1, "{at}");
        let class = testing::class_of(new[0]).expect("a window");
        assert_eq!(class.module, self.library.module.addr(), "{at}");
        self.window = Some(new[0]);
        self.parent_lost = false;
        self.seen[0] += 1;
        let size = self.size_before_open();
        self.size = Some((size.width, size.height));
        assert_eq!(
            testing::client_size_of(new[0]),
            Some((size.width, size.height)),
            "the editor opens at the size it gave, {at}"
        );
    }

    fn destroy(&mut self, at: &str) {
        let parent = self.parent.handle().expect("open");
        // SAFETY: a live plugin; `destroy` may come at any time.
        unsafe { self.gui().destroy.expect("destroy")(self.plugin()) };
        if let Some(child) = self.window.take() {
            assert!(
                !testing::is_window(child),
                "destroy closes the window, {at}"
            );
        }
        assert_eq!(testing::children_of(parent), Vec::<usize>::new(), "{at}");
        self.created = false;
        self.scale = None;
        self.size = None;
        self.parent_lost = false;
    }

    /// What holds after every call.
    fn check(&mut self, at: &str) {
        let parent = self.parent.handle().expect("open");
        let children = testing::children_of(parent);
        match self.window {
            Some(child) if !self.parent_lost => {
                assert_eq!(children, [child], "{at}");
                // The size the model expects, from the calls so far.
                assert_eq!(testing::client_size_of(child), self.size, "{at}");
                // The size the host is told is the size of the window.
                assert_eq!(self.get_size(), testing::client_size_of(child), "{at}");
            }
            _ => assert_eq!(children, Vec::<usize>::new(), "{at}"),
        }
        // The host hears of resize requests and, once per parent it
        // destroyed under an open editor, of the editor being closed.
        for call in self.instance.take_gui_calls() {
            if call == "closed(true)" {
                assert!(self.closes > 0, "an unexpected {call}, {at}");
                self.closes -= 1;
                self.seen[1] += 1;
            } else {
                assert!(call.starts_with("request_resize("), "{call}, {at}");
                assert!(self.window.is_some(), "{call} with no editor, {at}");
                self.seen[2] += 1;
            }
        }
        assert_eq!(self.closes, 0, "the host was told of every close, {at}");
    }
}

/// Random orders of `gui` calls with hostile arguments, on a plugin from
/// each binary at once, with real windows and the message loop running;
/// then the plugins are destroyed and the binaries unloaded in the order
/// `seed` picks, and nothing is left behind.
fn random_gui_calls(duck_path: &Path, pluck_path: &Path, seeds: std::ops::RangeInclusive<u64>) {
    let _alone = ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // A parent made and destroyed once first, so that the test parent's
    // class is registered before counting.
    drop(TopLevel::new(10, 10).expect("a desktop"));
    testing::pump(Duration::from_millis(20));
    let before = testing::gui_resources();
    let mut classes = Vec::new();
    let mut total = [0_usize; 5];
    for seed in seeds {
        let mut numbers = Numbers(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let duck = Library::load(duck_path);
        let pluck = Library::load(pluck_path);
        // The editors' sizes at scale 1, written out: naming the plugin
        // crates' constants would link both into this executable, and their
        // exported entry points would clash.
        let mut models = [
            Model {
                instance: Instance::create(&duck, "Mangue Duck"),
                library: &duck,
                base: HostSize::new(440, 210),
                size: None,
                parent: TopLevel::new(500, 260).expect("a desktop"),
                parent_size: (500, 260),
                created: false,
                scale: None,
                window: None,
                parent_lost: false,
                closes: 0,
                seen: [0; 5],
            },
            Model {
                instance: Instance::create(&pluck, "Mangue Pluck"),
                library: &pluck,
                base: HostSize::new(520, 160),
                size: None,
                parent: TopLevel::new(560, 260).expect("a desktop"),
                parent_size: (560, 260),
                created: false,
                scale: None,
                window: None,
                parent_lost: false,
                closes: 0,
                seen: [0; 5],
            },
        ];
        let mut opened = 0;
        for step in 0..600 {
            let which = numbers.below(2) as usize;
            let model = &mut models[which];
            model.step(&mut numbers, step);
            if let Some(child) = model.window {
                if let Some(class) = testing::class_of(child) {
                    if !classes.contains(&class) {
                        classes.push(class);
                    }
                }
            }
            opened += usize::from(models.iter().any(|model| model.window.is_some()));
        }
        let seen: Vec<[usize; 5]> = models.iter().map(|model| model.seen).collect();
        eprintln!(
            "seed {seed}: an editor was open on {opened} of 600 steps; [windows opened, closes told, resize requests, scales set while open, sizes set while open] per plugin: {seen:?}"
        );
        assert!(
            opened > 100,
            "seed {seed}: editors were open on {opened} steps"
        );
        assert!(
            seen.iter().all(|counts| counts[0] >= 2) && seen[0][0] + seen[1][0] >= 8,
            "seed {seed}: too few windows opened"
        );
        for (index, count) in total.iter_mut().enumerate() {
            *count += seen[0][index] + seen[1][index];
        }
        // The host closes everything, in an order of the seed's choosing.
        if seed % 2 == 0 {
            models.reverse();
        }
        for model in models {
            let mut model = model;
            model.destroy("the end");
            model.check("the end");
            model.instance.destroy();
        }
        if seed % 3 == 0 {
            duck.unload();
            pluck.unload();
        } else {
            pluck.unload();
            duck.unload();
        }
        testing::pump(Duration::from_millis(20));
        assert_eq!(testing::gui_resources(), before, "seed {seed}");
    }
    assert!(classes.len() >= 2, "{classes:?}");
    // Every kind of case came up.
    assert!(total.iter().all(|&count| count > 0), "{total:?}");
    for class in &classes {
        assert!(!testing::class_exists(class), "{class:?} leaked");
    }
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

/// The plugin binaries cargo builds beside this test's executable.
fn debug_binaries() -> (PathBuf, PathBuf) {
    duck_and_pluck(
        &deps_folder(),
        &format!("{DLL_PREFIX}mangue_duck{DLL_SUFFIX}"),
        &format!("{DLL_PREFIX}mangue_pluck{DLL_SUFFIX}"),
    )
}

/// The bundles `scripts/bundle-clap.sh` makes.
fn bundled_binaries() -> (PathBuf, PathBuf) {
    let bundled = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/bundled");
    duck_and_pluck(&bundled, "Mangue Duck.clap", "Mangue Pluck.clap")
}

#[test]
fn two_debug_binaries_open_and_close_editors_and_unload_in_both_orders() {
    let (duck, pluck) = debug_binaries();
    two_binaries(&duck, &pluck);
}

#[test]
fn two_debug_binaries_take_random_gui_calls_and_leave_nothing_behind() {
    let (duck, pluck) = debug_binaries();
    random_gui_calls(&duck, &pluck, 1..=4);
}

#[test]
#[ignore = "needs the release bundles: see the file's documentation"]
fn two_bundled_binaries_open_and_close_editors_and_unload_in_both_orders() {
    let (duck, pluck) = bundled_binaries();
    two_binaries(&duck, &pluck);
}

#[test]
#[ignore = "needs the release bundles: see the file's documentation"]
fn two_bundled_binaries_take_random_gui_calls_and_leave_nothing_behind() {
    let (duck, pluck) = bundled_binaries();
    random_gui_calls(&duck, &pluck, 1..=12);
}
