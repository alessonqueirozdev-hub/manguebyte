//! Hosts a plugin with an editor that does nothing but record what the host
//! asks of it and call the host back, at the worst moments it can find.
//!
//! The example plugins' editors open real windows, and the ones in
//! `clap_editor.rs` are opened on Windows only. This plugin, written here
//! against the raw C interface and reached through
//! `ClapLibrary::from_entry`, opens no window at all, so it runs on every
//! system. It registers timers and descriptors as an editor does, asks for
//! new sizes from inside the host's calls, unregisters timers from inside
//! `on_timer`, calls from the wrong thread and closes itself, and each test
//! checks the host's calls into it, in order, against what CLAP asks for.
//!
//! Every `unsafe` block in this file is one side or the other of the C
//! interface, with pointers that are valid for the call, so the blocks are
//! not commented one by one.
#![cfg(feature = "clap")]
#![allow(clippy::undocumented_unsafe_blocks)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::rc::Rc;
use std::time::{Duration, Instant};

use manguebyte_host::clap::clap_sys::entry::clap_plugin_entry;
use manguebyte_host::clap::clap_sys::ext::gui::*;
use manguebyte_host::clap::clap_sys::ext::posix_fd_support::*;
use manguebyte_host::clap::clap_sys::ext::timer_support::*;
use manguebyte_host::clap::clap_sys::factory::plugin_factory::*;
use manguebyte_host::clap::clap_sys::host::clap_host;
use manguebyte_host::clap::clap_sys::id::{CLAP_INVALID_ID, clap_id};
use manguebyte_host::clap::clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use manguebyte_host::clap::clap_sys::process::{CLAP_PROCESS_CONTINUE, clap_process};
use manguebyte_host::clap::clap_sys::version::CLAP_VERSION;
use manguebyte_host::clap::{ClapError, ClapLibrary, ClapPlugin, HostEvent, LogSeverity};
use manguebyte_host::editor::{
    EditorRequest, EditorSize, FdInterest, FdWatch, ParentWindow, WindowApi,
};

/// The editor's size at the scale it is first given, or at 1.
const SIZE: EditorSize = EditorSize::new(400, 300);

/// No requests, and no descriptors.
const NONE: [EditorRequest; 0] = [];
const NO_FDS: [FdWatch; 0] = [];

/// A window handle the fake editor is given. It opens nothing inside it.
fn parent() -> ParentWindow {
    ParentWindow::win32(0x1234).unwrap()
}

/// The host as the plugin calls it.
#[derive(Clone, Copy)]
struct Host(*const clap_host);

impl Host {
    fn extension<T>(self, id: &CStr) -> &'static T {
        unsafe {
            (*self.0).get_extension.unwrap()(self.0, id.as_ptr())
                .cast::<T>()
                .as_ref()
                .expect("the host offers the extension")
        }
    }

    fn gui(self) -> &'static clap_host_gui {
        self.extension(CLAP_EXT_GUI)
    }

    fn timers(self) -> &'static clap_host_timer_support {
        self.extension(CLAP_EXT_TIMER_SUPPORT)
    }

    fn fds(self) -> &'static clap_host_posix_fd_support {
        self.extension(CLAP_EXT_POSIX_FD_SUPPORT)
    }

    fn register_timer(self, period_ms: u32) -> Option<clap_id> {
        let mut id = 7;
        let ok = unsafe { self.timers().register_timer.unwrap()(self.0, period_ms, &mut id) };
        assert_eq!(ok, id != CLAP_INVALID_ID, "the id says whether it worked");
        ok.then_some(id)
    }

    fn unregister_timer(self, id: clap_id) -> bool {
        unsafe { self.timers().unregister_timer.unwrap()(self.0, id) }
    }

    fn register_fd(self, fd: i32, flags: clap_posix_fd_flags) -> bool {
        unsafe { self.fds().register_fd.unwrap()(self.0, fd, flags) }
    }

    fn modify_fd(self, fd: i32, flags: clap_posix_fd_flags) -> bool {
        unsafe { self.fds().modify_fd.unwrap()(self.0, fd, flags) }
    }

    fn unregister_fd(self, fd: i32) -> bool {
        unsafe { self.fds().unregister_fd.unwrap()(self.0, fd) }
    }

    fn request_resize(self, width: u32, height: u32) -> bool {
        unsafe { self.gui().request_resize.unwrap()(self.0, width, height) }
    }

    fn request_show(self) -> bool {
        unsafe { self.gui().request_show.unwrap()(self.0) }
    }

    fn request_hide(self) -> bool {
        unsafe { self.gui().request_hide.unwrap()(self.0) }
    }

    fn closed(self, was_destroyed: bool) {
        unsafe { self.gui().closed.unwrap()(self.0, was_destroyed) }
    }
}

/// Something the plugin does when the host calls one of its functions.
type Hook = Box<dyn FnMut(Host, u32)>;

/// What the test tells the plugin to do, and what the plugin saw.
#[derive(Default)]
struct Script {
    /// The plugin offers no `gui` at all.
    no_gui: bool,
    /// The window APIs the editor can go in, by CLAP name.
    apis: Vec<&'static CStr>,
    /// The `gui` call the plugin refuses, by name.
    refuse: RefCell<Option<&'static str>>,
    /// Whether the editor can be resized.
    resizable: Cell<bool>,
    /// The editor's size; `set_size` and `set_scale` change it.
    size: Cell<(u32, u32)>,
    /// Every call the host made into the plugin's editor extensions, and
    /// its own `destroy`, in order.
    calls: RefCell<Vec<String>>,
    /// The host the plugin was created with.
    host: Cell<Option<Host>>,
    /// The timers the editor registered as it was given its window.
    timers: RefCell<Vec<clap_id>>,
    /// What the plugin does from inside `set_parent`, `set_scale`,
    /// `set_size`, `on_timer` (with the timer's id) and `on_fd` (with the
    /// descriptor). By default `set_parent` registers a timer of 1 ms and
    /// watches descriptor 7 for reading, as an editor on X11 would.
    on_set_parent: RefCell<Option<Hook>>,
    on_set_scale: RefCell<Option<Hook>>,
    on_set_size: RefCell<Option<Hook>>,
    on_timer: RefCell<Option<Hook>>,
    on_fd: RefCell<Option<Hook>>,
}

impl Script {
    fn new() -> Rc<Script> {
        let script = Script {
            apis: vec![
                CLAP_WINDOW_API_WIN32,
                CLAP_WINDOW_API_COCOA,
                CLAP_WINDOW_API_X11,
            ],
            size: Cell::new((SIZE.width, SIZE.height)),
            ..Script::default()
        };
        *script.on_set_parent.borrow_mut() = Some(Box::new(|host: Host, _| {
            let timers = THIS.with(|this| this.borrow().clone().unwrap());
            timers
                .timers
                .borrow_mut()
                .push(host.register_timer(1).unwrap());
            assert!(host.register_fd(7, CLAP_POSIX_FD_READ));
        }));
        Rc::new(script)
    }

    fn log(&self, call: impl Into<String>) {
        self.calls.borrow_mut().push(call.into());
    }

    /// The calls since the last time, which are then forgotten.
    fn take_calls(&self) -> Vec<String> {
        self.calls.take()
    }

    /// Whether the plugin refuses `call`, which it then logs.
    fn refuses(&self, call: &str) -> bool {
        *self.refuse.borrow() == Some(call)
    }

    /// Runs one of the hooks, taken out while it runs so that a hook that
    /// leads, through the host, to another call of the same kind would
    /// find none instead of a borrowed one.
    fn run(&self, hook: &RefCell<Option<Hook>>, argument: u32) {
        let taken = hook.borrow_mut().take();
        if let Some(mut taken) = taken {
            taken(self.host.get().expect("created"), argument);
            let mut slot = hook.borrow_mut();
            if slot.is_none() {
                *slot = Some(taken);
            }
        }
    }
}

thread_local! {
    /// The script the next library loaded on this thread follows, and the
    /// one of the plugin made last. Tests run one to a thread.
    static THIS: RefCell<Option<Rc<Script>>> = const { RefCell::new(None) };
}

fn current_script() -> Rc<Script> {
    THIS.with(|this| this.borrow().clone().expect("a script"))
}

// --- The entry point, the factory and the plugin ---------------------------

struct Features([*const c_char; 1]);
unsafe impl Sync for Features {}
static FEATURES: Features = Features([ptr::null()]);

static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"test.editor".as_ptr(),
    name: c"Editor".as_ptr(),
    vendor: c"Test".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1".as_ptr(),
    description: c"".as_ptr(),
    features: FEATURES.0.as_ptr(),
};

unsafe extern "C" fn entry_init(_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        (&raw const FACTORY).cast()
    } else {
        ptr::null()
    }
}

static ENTRY: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn factory_count(_: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_descriptor(
    _: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &raw const DESCRIPTOR
    } else {
        ptr::null()
    }
}

#[repr(C)]
struct Instance {
    table: clap_plugin,
    script: Rc<Script>,
}

unsafe fn this<'a>(plugin: *const clap_plugin) -> &'a Script {
    unsafe { &(*(*plugin).plugin_data.cast::<Instance>()).script }
}

unsafe extern "C" fn factory_create(
    _: *const clap_plugin_factory,
    host: *const clap_host,
    _id: *const c_char,
) -> *const clap_plugin {
    let script = current_script();
    script.host.set(Some(Host(host)));
    let instance = Box::into_raw(Box::new(Instance {
        table: clap_plugin {
            desc: &raw const DESCRIPTOR,
            plugin_data: ptr::null_mut(),
            init: Some(agree),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(nothing),
            start_processing: Some(agree),
            stop_processing: Some(nothing),
            reset: Some(nothing),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(nothing),
        },
        script,
    }));
    unsafe {
        (*instance).table.plugin_data = instance.cast();
        &raw const (*instance).table
    }
}

unsafe extern "C" fn agree(_: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn nothing(_: *const clap_plugin) {}

unsafe extern "C" fn activate(_: *const clap_plugin, _: f64, _: u32, _: u32) -> bool {
    true
}

unsafe extern "C" fn process(_: *const clap_plugin, _: *const clap_process) -> i32 {
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn destroy(plugin: *const clap_plugin) {
    let instance = unsafe { Box::from_raw((*plugin).plugin_data.cast::<Instance>()) };
    instance.script.log("plugin.destroy");
}

unsafe extern "C" fn get_extension(plugin: *const clap_plugin, id: *const c_char) -> *const c_void {
    let id = unsafe { CStr::from_ptr(id) };
    let script = unsafe { this(plugin) };
    if id == CLAP_EXT_GUI && !script.no_gui {
        (&raw const GUI).cast()
    } else if id == CLAP_EXT_TIMER_SUPPORT {
        (&raw const TIMER).cast()
    } else if id == CLAP_EXT_POSIX_FD_SUPPORT {
        (&raw const FD).cast()
    } else {
        ptr::null()
    }
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_is_api_supported),
    get_preferred_api: None,
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: Some(gui_get_size),
    can_resize: Some(gui_can_resize),
    get_resize_hints: None,
    adjust_size: Some(gui_adjust_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_set_parent),
    set_transient: None,
    suggest_title: None,
    show: Some(gui_show),
    hide: Some(gui_hide),
};

fn api_text(api: *const c_char) -> String {
    unsafe { CStr::from_ptr(api) }
        .to_string_lossy()
        .into_owned()
}

unsafe extern "C" fn gui_is_api_supported(
    plugin: *const clap_plugin,
    api: *const c_char,
    floating: bool,
) -> bool {
    let script = unsafe { this(plugin) };
    let name = unsafe { CStr::from_ptr(api) };
    script.log(format!("is_api_supported({}, {floating})", api_text(api)));
    !floating && script.apis.contains(&name)
}

unsafe extern "C" fn gui_create(
    plugin: *const clap_plugin,
    api: *const c_char,
    floating: bool,
) -> bool {
    let script = unsafe { this(plugin) };
    script.log(format!("create({}, {floating})", api_text(api)));
    // A new editor starts at its size at scale 1.
    script.size.set((SIZE.width, SIZE.height));
    !script.refuses("create")
}

unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
    unsafe { this(plugin) }.log("destroy");
}

unsafe extern "C" fn gui_set_scale(plugin: *const clap_plugin, scale: f64) -> bool {
    let script = unsafe { this(plugin) };
    script.log(format!("set_scale({scale})"));
    let (width, height) = (SIZE.width as f64 * scale, SIZE.height as f64 * scale);
    script.size.set((width as u32, height as u32));
    script.run(&script.on_set_scale, 0);
    !script.refuses("set_scale")
}

unsafe extern "C" fn gui_get_size(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    let script = unsafe { this(plugin) };
    script.log("get_size");
    if script.refuses("get_size") {
        return false;
    }
    let (w, h) = script.size.get();
    unsafe {
        *width = w;
        *height = h;
    }
    true
}

unsafe extern "C" fn gui_can_resize(plugin: *const clap_plugin) -> bool {
    let script = unsafe { this(plugin) };
    script.log("can_resize");
    script.resizable.get()
}

/// A resizable editor takes sizes in steps of ten pixels.
unsafe extern "C" fn gui_adjust_size(
    plugin: *const clap_plugin,
    width: *mut u32,
    height: *mut u32,
) -> bool {
    let script = unsafe { this(plugin) };
    let (w, h) = unsafe { (*width, *height) };
    script.log(format!("adjust_size({w}, {h})"));
    if !script.resizable.get() {
        return false;
    }
    unsafe {
        *width = w / 10 * 10;
        *height = h / 10 * 10;
    }
    true
}

unsafe extern "C" fn gui_set_size(plugin: *const clap_plugin, width: u32, height: u32) -> bool {
    let script = unsafe { this(plugin) };
    script.log(format!("set_size({width}, {height})"));
    script.run(&script.on_set_size, 0);
    if script.refuses("set_size") || !script.resizable.get() {
        return false;
    }
    script.size.set((width, height));
    true
}

/// An X11 window id, which is 32 bits wide on some systems and 64 on
/// others, as 64.
fn widen(window: impl Into<u64>) -> u64 {
    window.into()
}

unsafe extern "C" fn gui_set_parent(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    let script = unsafe { this(plugin) };
    let window = unsafe { &*window };
    let api = api_text(window.api);
    // The field the window's API names, which is the one the host wrote.
    let handle = if api == "x11" {
        widen(unsafe { window.specific.x11 }) as usize
    } else {
        unsafe { window.specific.ptr }.addr()
    };
    script.log(format!("set_parent({api}, {handle:#x})"));
    script.run(&script.on_set_parent, 0);
    !script.refuses("set_parent")
}

unsafe extern "C" fn gui_show(plugin: *const clap_plugin) -> bool {
    let script = unsafe { this(plugin) };
    script.log("show");
    !script.refuses("show")
}

unsafe extern "C" fn gui_hide(plugin: *const clap_plugin) -> bool {
    unsafe { this(plugin) }.log("hide");
    true
}

static TIMER: clap_plugin_timer_support = clap_plugin_timer_support {
    on_timer: Some(on_timer),
};

unsafe extern "C" fn on_timer(plugin: *const clap_plugin, id: clap_id) {
    let script = unsafe { this(plugin) };
    script.log(format!("on_timer({id})"));
    script.run(&script.on_timer, id);
}

static FD: clap_plugin_posix_fd_support = clap_plugin_posix_fd_support { on_fd: Some(on_fd) };

unsafe extern "C" fn on_fd(plugin: *const clap_plugin, fd: i32, flags: clap_posix_fd_flags) {
    let script = unsafe { this(plugin) };
    script.log(format!("on_fd({fd}, {flags})"));
    script.run(&script.on_fd, fd as u32);
}

// --- Helpers ----------------------------------------------------------------

/// Creates the plugin, following `script`.
fn host(script: &Rc<Script>) -> ClapPlugin {
    THIS.with(|this| *this.borrow_mut() = Some(Rc::clone(script)));
    let library = unsafe { ClapLibrary::from_entry(&raw const ENTRY, "/plugins/editor.clap") }
        .expect("the fake library loads");
    let plugin = library.create("test.editor").expect("the fake plugin");
    script.take_calls();
    plugin
}

/// What the host calls, in order, to open an editor at `scale` in the
/// fake parent: CLAP's order.
fn opening(scale: Option<f64>) -> Vec<String> {
    let mut calls = vec!["is_api_supported(win32, false)", "create(win32, false)"]
        .into_iter()
        .map(String::from)
        .collect::<Vec<_>>();
    if let Some(scale) = scale {
        calls.push(format!("set_scale({scale})"));
    }
    calls.extend(["get_size", "set_parent(win32, 0x1234)", "show"].map(String::from));
    calls
}

fn editor_events(events: &[HostEvent]) -> Vec<EditorRequest> {
    events
        .iter()
        .filter_map(|event| match event {
            HostEvent::Editor(request) => Some(*request),
            _ => None,
        })
        .collect()
}

/// Waits until the plugin's next timer is due.
fn wait_for_timers(plugin: &ClapPlugin) {
    let due = plugin.next_editor_timer().expect("a timer is registered");
    let now = Instant::now();
    if due > now {
        std::thread::sleep(due - now);
    }
}

// --- What is checked ----------------------------------------------------------

#[test]
fn an_editor_opens_in_clap_order_and_closes_with_nothing_left() {
    let script = Script::new();
    let mut plugin = host(&script);
    assert!(plugin.has_editor());
    assert!(!plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), None);
    assert_eq!(plugin.next_editor_timer(), None);
    script.take_calls();

    let size = plugin.open_editor(parent(), Some(1.5)).unwrap();
    assert_eq!(size, EditorSize::new(600, 450));
    assert_eq!(script.take_calls(), opening(Some(1.5)));
    assert!(plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), Some(size));
    // What the editor registered as it was given its window.
    assert!(plugin.next_editor_timer().is_some());
    let read = FdInterest {
        read: true,
        ..FdInterest::default()
    };
    assert_eq!(
        plugin.editor_fds(),
        [FdWatch {
            fd: 7,
            interest: read
        }]
    );
    assert_eq!(
        plugin.open_editor(parent(), None),
        Err(ClapError::EditorAlreadyOpen)
    );
    assert_eq!(script.take_calls(), Vec::<String>::new());

    // The editor left its timer and descriptor registered; closing it
    // takes them away.
    plugin.close_editor();
    assert_eq!(script.take_calls(), ["hide", "destroy"]);
    assert!(!plugin.is_editor_open());
    assert_eq!(plugin.editor_size(), None);
    assert_eq!(plugin.next_editor_timer(), None);
    assert_eq!(plugin.editor_fds(), NO_FDS);
    plugin.close_editor();
    assert_eq!(script.take_calls(), Vec::<String>::new(), "closed twice");
    std::thread::sleep(Duration::from_millis(3));
    assert_eq!(editor_events(&plugin.pump()), NONE);
    assert_eq!(script.take_calls(), Vec::<String>::new(), "no timer fired");

    // Opened again, without a scale: none is set.
    assert_eq!(plugin.open_editor(parent(), None), Ok(SIZE));
    assert_eq!(script.take_calls(), opening(None));
    drop(plugin);
    assert_eq!(script.take_calls(), ["hide", "destroy", "plugin.destroy"]);
}

#[test]
fn timers_fire_from_pump_and_may_change_from_inside_their_own_firing() {
    let script = Script::new();
    let mut plugin = host(&script);
    // A second timer, registered from inside `set_parent` too.
    *script.on_set_parent.borrow_mut() = Some(Box::new(|host: Host, _| {
        let script = current_script();
        let mut timers = script.timers.borrow_mut();
        timers.push(host.register_timer(1).unwrap());
        timers.push(host.register_timer(0).unwrap());
    }));
    plugin.open_editor(parent(), None).unwrap();
    let (first, second) = {
        let timers = script.timers.borrow();
        (timers[0], timers[1])
    };
    assert_ne!(first, second);
    script.take_calls();

    // The first timer to fire unregisters the second, which is due in the
    // same round, and registers a third.
    let third = Rc::new(Cell::new(None));
    let made = Rc::clone(&third);
    *script.on_timer.borrow_mut() = Some(Box::new(move |host: Host, id| {
        if made.get().is_none() {
            assert!(host.unregister_timer(if id == first { second } else { first }));
            made.set(host.register_timer(5));
        }
    }));
    wait_for_timers(&plugin);
    std::thread::sleep(Duration::from_millis(2));
    plugin.pump();
    let calls = script.take_calls();
    assert_eq!(
        calls.len(),
        1,
        "one fired and the other was stopped: {calls:?}"
    );
    let third = third
        .get()
        .expect("a timer registered from inside on_timer");
    let survivor = if calls[0] == format!("on_timer({first})") {
        first
    } else {
        assert_eq!(calls[0], format!("on_timer({second})"));
        second
    };

    // From now on the survivor and the new timer fire, each at most once
    // per pump, however late it is.
    std::thread::sleep(Duration::from_millis(12));
    plugin.pump();
    let mut calls = script.take_calls();
    calls.sort();
    let mut wanted = vec![
        format!("on_timer({survivor})"),
        format!("on_timer({third})"),
    ];
    wanted.sort();
    assert_eq!(calls, wanted);

    // A timer that unregisters itself fires no more.
    *script.on_timer.borrow_mut() = Some(Box::new(|host: Host, id| {
        assert!(host.unregister_timer(id));
        assert!(!host.unregister_timer(id), "gone already");
    }));
    std::thread::sleep(Duration::from_millis(12));
    plugin.pump();
    assert_eq!(script.take_calls().len(), 2);
    assert_eq!(plugin.next_editor_timer(), None);
    std::thread::sleep(Duration::from_millis(12));
    plugin.pump();
    assert_eq!(script.take_calls(), Vec::<String>::new());
}

#[test]
fn a_timer_registered_without_an_editor_outlives_the_editor() {
    let script = Script::new();
    let mut plugin = host(&script);
    let host_side = script.host.get().unwrap();
    let own = host_side.register_timer(1).unwrap();
    plugin.open_editor(parent(), None).unwrap();
    plugin.close_editor();
    script.take_calls();
    wait_for_timers(&plugin);
    plugin.pump();
    assert_eq!(script.take_calls(), [format!("on_timer({own})")]);
    assert!(host_side.unregister_timer(own));
    assert_eq!(plugin.next_editor_timer(), None);
}

#[test]
fn requests_from_inside_the_hosts_calls_come_out_of_pump() {
    let script = Script::new();
    let mut plugin = host(&script);
    let host_side = script.host.get().unwrap();
    // Nothing is taken while there is no editor.
    assert!(!host_side.request_resize(10, 10));
    assert!(!host_side.request_show());
    assert!(!host_side.request_hide());
    host_side.closed(true);
    script.resizable.set(true);
    plugin.open_editor(parent(), Some(1.0)).unwrap();
    assert_eq!(editor_events(&plugin.pump()), NONE);

    // The editor asks for the size it has at its new scale from inside
    // `set_scale`, and again from inside the `set_size` that answers it;
    // the host takes both and calls nothing from inside either.
    *script.on_set_scale.borrow_mut() = Some(Box::new(|host: Host, _| {
        assert!(host.request_resize(800, 600));
    }));
    *script.on_set_size.borrow_mut() = Some(Box::new(|host: Host, _| {
        assert!(host.request_resize(805, 603));
    }));
    script.take_calls();
    plugin.set_editor_scale(2.0).unwrap();
    assert_eq!(script.take_calls(), ["set_scale(2)", "get_size"]);
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(800, 600)));
    let events = plugin.pump();
    assert_eq!(
        editor_events(&events),
        [EditorRequest::Resize(EditorSize::new(800, 600))]
    );
    assert_eq!(script.take_calls(), Vec::<String>::new());

    // The application answers the request, and the editor asks again from
    // inside: a resizable editor fits the size to its steps of ten first.
    assert_eq!(
        plugin.set_editor_size(EditorSize::new(805, 603)),
        Ok(EditorSize::new(800, 600))
    );
    assert_eq!(
        script.take_calls(),
        ["can_resize", "adjust_size(805, 603)", "set_size(800, 600)"]
    );
    assert_eq!(plugin.editor_size(), Some(EditorSize::new(800, 600)));
    assert_eq!(
        editor_events(&plugin.pump()),
        [EditorRequest::Resize(EditorSize::new(805, 603))]
    );
    assert_eq!(editor_events(&plugin.pump()), NONE, "answered once");

    // Of two requests to be shown or hidden, the last counts; a size of
    // nothing is refused.
    assert!(host_side.request_hide());
    assert!(host_side.request_show());
    assert!(!host_side.request_resize(0, 100));
    assert_eq!(editor_events(&plugin.pump()), [EditorRequest::Show]);
    assert!(host_side.request_show());
    assert!(host_side.request_hide());
    assert_eq!(editor_events(&plugin.pump()), [EditorRequest::Hide]);

    // A request made just before the editor closes is not reported.
    assert!(host_side.request_resize(1, 1));
    plugin.close_editor();
    assert_eq!(editor_events(&plugin.pump()), NONE);
    assert!(!plugin.can_resize_editor());
    assert_eq!(plugin.set_editor_size(SIZE), Err(ClapError::EditorNotOpen));
    assert_eq!(plugin.set_editor_scale(1.0), Err(ClapError::EditorNotOpen));
}

#[test]
fn a_fixed_editor_keeps_its_size_and_is_not_told_another() {
    let script = Script::new();
    let mut plugin = host(&script);
    plugin.open_editor(parent(), None).unwrap();
    script.take_calls();
    assert!(!plugin.can_resize_editor());
    assert_eq!(plugin.set_editor_size(EditorSize::new(999, 999)), Ok(SIZE));
    assert_eq!(
        script.take_calls(),
        ["can_resize", "can_resize", "get_size"]
    );
    assert_eq!(plugin.editor_size(), Some(SIZE));
    assert_eq!(
        plugin.set_editor_size(EditorSize::new(0, 10)),
        Err(ClapError::InvalidArgument(
            "an editor is at least one pixel wide and high"
        ))
    );
}

#[test]
fn scales_that_are_not_scales_are_refused_before_the_plugin_hears_of_them() {
    let script = Script::new();
    let mut plugin = host(&script);
    for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
        assert!(matches!(
            plugin.open_editor(parent(), Some(scale)),
            Err(ClapError::InvalidArgument(_))
        ));
    }
    assert_eq!(script.take_calls(), Vec::<String>::new());
    plugin.open_editor(parent(), None).unwrap();
    script.take_calls();
    assert!(matches!(
        plugin.set_editor_scale(f64::NAN),
        Err(ClapError::InvalidArgument(_))
    ));
    *script.refuse.borrow_mut() = Some("set_scale");
    assert_eq!(
        plugin.set_editor_scale(1.25),
        Err(ClapError::EditorRefused("set_scale"))
    );
    assert_eq!(script.take_calls(), ["set_scale(1.25)"]);
    assert!(plugin.is_editor_open(), "a refused scale closes nothing");
}

#[test]
fn an_editor_that_fails_to_open_leaves_nothing_open() {
    let script = Script::new();
    let mut plugin = host(&script);
    for refused in ["create", "get_size", "set_parent", "show"] {
        *script.refuse.borrow_mut() = Some(refused);
        script.take_calls();
        assert_eq!(
            plugin.open_editor(parent(), None),
            Err(ClapError::EditorRefused(refused)),
        );
        let calls = script.take_calls();
        let made = opening(None);
        let upto = made
            .iter()
            .position(|call| call.starts_with(refused))
            .unwrap();
        let mut wanted = made[..=upto].to_vec();
        if refused != "create" {
            // Destroyed again, never having been shown.
            wanted.push("destroy".into());
        }
        assert_eq!(calls, wanted, "{refused}");
        assert!(!plugin.is_editor_open(), "{refused}");
        // What the editor registered before it failed is gone with it.
        assert_eq!(plugin.next_editor_timer(), None, "{refused}");
        assert_eq!(plugin.editor_fds(), NO_FDS, "{refused}");
    }
    *script.refuse.borrow_mut() = None;
    assert_eq!(plugin.open_editor(parent(), None), Ok(SIZE));
}

#[test]
fn an_editor_for_another_kind_of_window_is_refused() {
    let script = Rc::new(Script {
        apis: vec![CLAP_WINDOW_API_X11],
        size: Cell::new((SIZE.width, SIZE.height)),
        ..Script::default()
    });
    let mut plugin = host(&script);
    let x11_here = !cfg!(any(target_os = "windows", target_os = "macos"));
    assert_eq!(plugin.has_editor(), x11_here);
    assert_eq!(
        plugin.open_editor(parent(), None),
        Err(ClapError::EditorApiUnsupported(WindowApi::Win32))
    );
    script.take_calls();
    // An X11 window, which the editor takes on any system: the host does
    // not judge what kind of window the application has.
    let x11 = ParentWindow::x11(0x0040_0001).unwrap();
    assert_eq!(
        plugin.open_editor(x11, Some(2.0)),
        Ok(EditorSize::new(800, 600))
    );
    let calls = script.take_calls();
    assert_eq!(
        calls[..3],
        [
            "is_api_supported(x11, false)",
            "create(x11, false)",
            "set_scale(2)"
        ]
    );
    assert_eq!(calls[4], "set_parent(x11, 0x400001)");

    let script = Rc::new(Script {
        no_gui: true,
        ..Script::default()
    });
    let mut plugin = host(&script);
    assert!(!plugin.has_editor());
    assert_eq!(plugin.open_editor(parent(), None), Err(ClapError::NoEditor));
}

#[test]
fn a_cocoa_view_is_not_given_a_scale() {
    let script = Script::new();
    let mut plugin = host(&script);
    let view = ParentWindow::cocoa(0x7000).unwrap();
    assert_eq!(plugin.open_editor(view, Some(2.0)), Ok(SIZE));
    let calls = script.take_calls();
    assert!(
        !calls.iter().any(|call| call.starts_with("set_scale")),
        "{calls:?}"
    );
    assert_eq!(plugin.set_editor_scale(2.0), Ok(()));
    assert_eq!(script.take_calls(), Vec::<String>::new());
}

#[test]
fn calls_from_another_thread_are_refused_and_counted_unless_clap_allows_them() {
    let script = Script::new();
    let mut plugin = host(&script);
    plugin.open_editor(parent(), None).unwrap();
    plugin.pump();
    let timer = script.timers.borrow()[0];
    let address = script.host.get().unwrap().0.addr();
    let (registered, unregistered, watched, modified, dropped, resized) =
        std::thread::spawn(move || {
            let host = Host(ptr::with_exposed_provenance(address));
            let mut id = 3;
            let registered = unsafe { host.timers().register_timer.unwrap()(host.0, 10, &mut id) };
            assert_eq!(id, CLAP_INVALID_ID);
            (
                registered,
                host.unregister_timer(timer),
                host.register_fd(9, CLAP_POSIX_FD_READ),
                host.modify_fd(7, CLAP_POSIX_FD_WRITE),
                host.unregister_fd(7),
                // CLAP marks the `gui` requests thread-safe.
                host.request_resize(640, 480),
            )
        })
        .join()
        .unwrap();
    assert!(!registered && !unregistered && !watched && !modified && !dropped);
    assert!(resized);
    // Nothing changed.
    assert_eq!(plugin.editor_fds().len(), 1);
    assert!(plugin.next_editor_timer().is_some());
    let events = plugin.pump();
    assert_eq!(
        editor_events(&events),
        [EditorRequest::Resize(EditorSize::new(640, 480))]
    );
    let warned = events.iter().any(|event| {
        matches!(event, HostEvent::Log { severity: LogSeverity::PluginMisbehaving, message }
            if message.contains("another thread 5 time(s)"))
    });
    assert!(warned, "{events:?}");
}

#[test]
fn descriptors_are_watched_and_their_readiness_passed_on() {
    let script = Script::new();
    let mut plugin = host(&script);
    let host_side = script.host.get().unwrap();
    plugin.open_editor(parent(), None).unwrap();
    script.take_calls();
    let read = FdInterest {
        read: true,
        ..FdInterest::default()
    };
    let write = FdInterest {
        write: true,
        ..FdInterest::default()
    };
    let everything = FdInterest {
        read: true,
        write: true,
        error: true,
    };

    // What the descriptor is not watched for is not passed on, and a
    // descriptor not watched at all is not either.
    plugin.editor_fd_ready(7, write);
    plugin.editor_fd_ready(8, everything);
    assert_eq!(script.take_calls(), Vec::<String>::new());
    plugin.editor_fd_ready(7, everything);
    assert_eq!(
        script.take_calls(),
        [format!("on_fd(7, {CLAP_POSIX_FD_READ})")]
    );

    // The plugin changes what it waits for from inside `on_fd`, and adds a
    // descriptor; nonsense is refused.
    *script.on_fd.borrow_mut() = Some(Box::new(|host: Host, _| {
        assert!(host.modify_fd(7, CLAP_POSIX_FD_WRITE | CLAP_POSIX_FD_ERROR));
        assert!(host.register_fd(9, CLAP_POSIX_FD_READ | 1 << 7));
        assert!(!host.register_fd(9, CLAP_POSIX_FD_READ), "already watched");
        assert!(!host.register_fd(-1, CLAP_POSIX_FD_READ));
        assert!(!host.register_fd(10, 0));
        assert!(!host.modify_fd(11, CLAP_POSIX_FD_READ));
    }));
    plugin.editor_fd_ready(7, read);
    *script.on_fd.borrow_mut() = None;
    assert_eq!(
        plugin.editor_fds(),
        [
            FdWatch {
                fd: 7,
                interest: FdInterest {
                    read: false,
                    write: true,
                    error: true
                }
            },
            FdWatch {
                fd: 9,
                interest: read
            }
        ]
    );
    script.take_calls();
    plugin.editor_fd_ready(7, everything);
    assert_eq!(
        script.take_calls(),
        [format!(
            "on_fd(7, {})",
            CLAP_POSIX_FD_WRITE | CLAP_POSIX_FD_ERROR
        )]
    );
    assert!(host_side.unregister_fd(9));
    assert!(!host_side.unregister_fd(9));
    plugin.close_editor();
    assert_eq!(plugin.editor_fds(), NO_FDS);
}

#[test]
fn an_editor_that_closes_by_itself_is_destroyed_by_pump() {
    for was_destroyed in [true, false] {
        let script = Script::new();
        let mut plugin = host(&script);
        plugin.open_editor(parent(), None).unwrap();
        // It closes from inside one of its own timers, and asks for a size
        // in the same breath: only the close is reported.
        *script.on_timer.borrow_mut() = Some(Box::new(move |host: Host, _| {
            assert!(host.request_resize(10, 10));
            host.closed(was_destroyed);
        }));
        script.take_calls();
        wait_for_timers(&plugin);
        let events = plugin.pump();
        assert_eq!(editor_events(&events), [EditorRequest::Closed]);
        let calls = script.take_calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].starts_with("on_timer("));
        assert_eq!(calls[1], "destroy");
        assert!(!plugin.is_editor_open());
        assert_eq!(plugin.next_editor_timer(), None);
        assert_eq!(plugin.editor_fds(), NO_FDS);
        plugin.close_editor();
        assert_eq!(script.take_calls(), Vec::<String>::new());
        assert_eq!(plugin.open_editor(parent(), None), Ok(SIZE));
    }
}

#[test]
fn dropping_the_plugin_closes_its_editor_before_the_plugin_goes() {
    // With no audio half, the plugin goes at once, after its editor.
    let script = Script::new();
    let mut plugin = host(&script);
    plugin.open_editor(parent(), None).unwrap();
    script.take_calls();
    drop(plugin);
    assert_eq!(script.take_calls(), ["hide", "destroy", "plugin.destroy"]);

    // With one, the editor still goes with the main half, on this thread;
    // the plugin waits for the audio half.
    let script = Script::new();
    let mut plugin = host(&script);
    let processor = plugin.activate(48_000.0, 64).unwrap();
    plugin.open_editor(parent(), None).unwrap();
    script.take_calls();
    drop(plugin);
    assert_eq!(script.take_calls(), ["hide", "destroy"]);
    drop(processor);
    assert_eq!(script.take_calls(), ["plugin.destroy"]);
}
