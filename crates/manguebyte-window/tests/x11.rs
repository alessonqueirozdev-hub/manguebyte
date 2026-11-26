//! The X11 backend against a real X server (ADR 0028, decision 5).
//!
//! The test plays the host over a connection of its own: it makes the parent
//! window, offers a loop that polls the descriptor the window registers and
//! fires its timers, reads pixels back with `GetImage` and injects input
//! with the XTEST extension.
//!
//! The pointer, the keyboard and the resources belong to the whole server,
//! and one test breaks the window's connection with `SIGPIPE` left to end
//! the process, so every test takes one lock, and they are ignored unless
//! asked for. Run them on a server nobody is using, with a screen large
//! enough for the largest picture:
//!
//! ```sh
//! xvfb-run -a -s "-screen 0 2560x2560x24" \
//!     cargo test -p manguebyte-window --test x11 -- --ignored
//! ```

#![cfg(target_os = "linux")]

use std::cell::{Cell, RefCell};
use std::os::fd::BorrowedFd;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use manguebyte_gfx::{Color, IRect, Pixmap, Point};
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_plugin::editor::{EditorContext, HostLink, HostLoop, LoopToken, ParentWindow};
use manguebyte_window::{
    Button, Event, FileDialog, Handled, Key, KeyEvent, Modifiers, PhysicalPoint, PhysicalSize,
    PointerEvent, TimerId, Window, WindowCx, WindowError, WindowOptions,
};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use x11rb::connection::Connection as _;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask,
    ImageFormat, InputFocus, KeyPressEvent, MapState, PropMode, Rectangle, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// What a test waits for at most.
const PATIENCE: Duration = Duration::from_secs(10);

/// How long a test listens for something that must not happen.
const QUIET: Duration = Duration::from_millis(150);

/// Where the host's window is on the screen.
const PARENT_AT: (i16, i16) = (20, 30);

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION: u8 = 6;

fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    // A server starts afresh when its last client leaves, and a client that
    // connects meanwhile is dropped: one connection stays for the whole run.
    static KEPT: OnceLock<RustConnection> = OnceLock::new();
    let guard = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    KEPT.get_or_init(|| RustConnection::connect(None).expect("an X server").0);
    guard
}

// ---------------------------------------------------------------------------
// The host's loop.

enum Source {
    Fd(i32),
    Timer { period: Duration, due: Instant },
}

struct Entry {
    token: LoopToken,
    source: Source,
    /// `None` while the callback runs.
    callback: Option<Box<dyn FnMut()>>,
}

/// A host's loop: it polls the descriptors watched and fires timers when
/// they are due, as a host on Linux does.
struct TestLoop {
    offers_fds: bool,
    offers_timers: bool,
    next: Cell<u64>,
    entries: RefCell<Vec<Entry>>,
}

impl TestLoop {
    fn new(offers_fds: bool, offers_timers: bool) -> Rc<TestLoop> {
        Rc::new(TestLoop {
            offers_fds,
            offers_timers,
            next: Cell::new(1),
            entries: RefCell::new(Vec::new()),
        })
    }

    fn register(&self, source: Source, callback: Box<dyn FnMut()>) -> LoopToken {
        let token = LoopToken::new(self.next.get());
        self.next.set(token.get() + 1);
        self.entries.borrow_mut().push(Entry {
            token,
            source,
            callback: Some(callback),
        });
        token
    }

    fn fds(&self) -> Vec<(LoopToken, i32)> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry.source {
                Source::Fd(fd) => Some((entry.token, fd)),
                Source::Timer { .. } => None,
            })
            .collect()
    }

    /// The periods of the timers registered, in milliseconds.
    fn timers(&self) -> Vec<u128> {
        self.entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry.source {
                Source::Timer { period, .. } => Some(period.as_millis()),
                Source::Fd(_) => None,
            })
            .collect()
    }

    fn registrations(&self) -> usize {
        self.entries.borrow().len()
    }

    fn run(&self, token: LoopToken) {
        let callback = self
            .entries
            .borrow_mut()
            .iter_mut()
            .find(|entry| entry.token == token)
            .and_then(|entry| entry.callback.take());
        let Some(mut callback) = callback else { return };
        callback();
        let mut entries = self.entries.borrow_mut();
        match entries.iter_mut().find(|entry| entry.token == token) {
            Some(entry) => entry.callback = Some(callback),
            None => {
                drop(entries);
                drop(callback);
            }
        }
    }

    /// One turn: waits at most `wait` for a descriptor or a timer, then
    /// calls back what is ready. A descriptor is called back only when the
    /// system says it can be read.
    fn turn(&self, wait: Duration) {
        let now = Instant::now();
        let wait = self
            .entries
            .borrow()
            .iter()
            .filter_map(|entry| match entry.source {
                Source::Timer { due, .. } => Some(due.saturating_duration_since(now)),
                Source::Fd(_) => None,
            })
            .fold(wait, Duration::min);
        let watched = self.fds();
        let ready: Vec<LoopToken> = {
            let borrowed: Vec<BorrowedFd<'_>> = watched
                .iter()
                // SAFETY: a watched descriptor is open: the window cancels
                // its watch before it closes the connection, and nothing
                // runs between taking this list and the end of the poll.
                .map(|&(_, fd)| unsafe { BorrowedFd::borrow_raw(fd) })
                .collect();
            let mut fds: Vec<PollFd<'_>> = borrowed
                .iter()
                .map(|fd| PollFd::new(fd, PollFlags::IN))
                .collect();
            let timeout = Timespec {
                tv_sec: wait.as_secs() as _,
                tv_nsec: wait.subsec_nanos() as _,
            };
            poll(&mut fds, Some(&timeout)).expect("poll");
            fds.iter()
                .zip(&watched)
                .filter(|(fd, _)| !fd.revents().is_empty())
                .map(|(_, &(token, _))| token)
                .collect()
        };
        for token in ready {
            self.run(token);
        }
        let now = Instant::now();
        let due: Vec<LoopToken> = self
            .entries
            .borrow_mut()
            .iter_mut()
            .filter_map(|entry| match &mut entry.source {
                Source::Timer { period, due } if *due <= now => {
                    *due = now + *period;
                    Some(entry.token)
                }
                _ => None,
            })
            .collect();
        for token in due {
            self.run(token);
        }
    }
}

impl HostLoop for TestLoop {
    fn watch_fd(&self, fd: i32, ready: Box<dyn FnMut()>) -> Option<LoopToken> {
        self.offers_fds
            .then(|| self.register(Source::Fd(fd), ready))
    }

    fn every(&self, period_ms: u32, tick: Box<dyn FnMut()>) -> Option<LoopToken> {
        let period = Duration::from_millis(u64::from(period_ms));
        let due = Instant::now() + period;
        self.offers_timers
            .then(|| self.register(Source::Timer { period, due }, tick))
    }

    fn cancel(&self, token: LoopToken) {
        let removed = {
            let mut entries = self.entries.borrow_mut();
            let index = entries.iter().position(|entry| entry.token == token);
            index.map(|index| entries.remove(index))
        };
        // Its callback goes with nothing borrowed.
        drop(removed);
    }
}

// ---------------------------------------------------------------------------
// The host's side of the server.

struct Host {
    conn: RustConnection,
    root: u32,
    screen: (u16, u16),
    parent: u32,
}

impl Host {
    /// A host with a window of `size` on the screen, with every key and
    /// button up and the pointer in a corner outside it.
    fn new(size: PhysicalSize) -> Host {
        let (conn, number) = RustConnection::connect(None).expect("an X server");
        let screen = &conn.setup().roots[number];
        let root = screen.root;
        let screen = (screen.width_in_pixels, screen.height_in_pixels);
        let parent = conn.generate_id().expect("an id");
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            parent,
            root,
            PARENT_AT.0,
            PARENT_AT.1,
            size.width as u16,
            size.height as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            // What the window passes on arrives here.
            &CreateWindowAux::new().override_redirect(1).event_mask(
                EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE,
            ),
        )
        .expect("request")
        .check()
        .expect("a parent window");
        conn.map_window(parent).expect("request");
        let host = Host {
            conn,
            root,
            screen,
            parent,
        };
        host.release_everything();
        host.pointer_to_root(1, 1);
        host
    }

    /// Waits until the server has done everything asked so far.
    fn sync(&self) {
        self.conn
            .get_input_focus()
            .expect("request")
            .reply()
            .expect("the server answers");
    }

    fn parent(&self) -> ParentWindow {
        ParentWindow::x11(u64::from(self.parent)).expect("not zero")
    }

    fn children(&self) -> Vec<u32> {
        self.conn
            .query_tree(self.parent)
            .expect("request")
            .reply()
            .expect("the parent exists")
            .children
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) {
        self.conn
            .xtest_fake_input(kind, detail, 0, self.root, x, y, 0)
            .expect("request")
            .check()
            .expect("XTEST");
        self.sync();
    }

    fn pointer_to_root(&self, x: i16, y: i16) {
        self.fake(MOTION, 0, x, y);
    }

    /// The pointer to `(x, y)` of the parent, where the child's corner is
    /// unless the test moved it.
    fn pointer(&self, x: i16, y: i16) {
        self.pointer_to_root(PARENT_AT.0 + x, PARENT_AT.1 + y);
    }

    fn button(&self, button: u8, down: bool) {
        let kind = if down { BUTTON_PRESS } else { BUTTON_RELEASE };
        self.fake(kind, button, 0, 0);
    }

    fn click(&self, button: u8) {
        self.button(button, true);
        self.button(button, false);
    }

    fn key(&self, code: u8, down: bool) {
        let kind = if down { KEY_PRESS } else { KEY_RELEASE };
        self.fake(kind, code, 0, 0);
    }

    fn tap(&self, code: u8) {
        self.key(code, true);
        self.key(code, false);
    }

    fn release_everything(&self) {
        let keys = self
            .conn
            .query_keymap()
            .expect("request")
            .reply()
            .expect("keys")
            .keys;
        for code in 8..=255u8 {
            if keys[usize::from(code / 8)] & (1 << (code % 8)) != 0 {
                self.key(code, false);
            }
        }
        for button in 1..=3 {
            self.button(button, false);
        }
    }

    /// The keycode whose unshifted keysym is `keysym`.
    fn keycode(&self, keysym: u32) -> u8 {
        let setup = self.conn.setup();
        let (first, last) = (setup.min_keycode, setup.max_keycode);
        let map = self
            .conn
            .get_keyboard_mapping(first, last - first + 1)
            .expect("request")
            .reply()
            .expect("a keyboard map");
        let per = usize::from(map.keysyms_per_keycode);
        let index = map
            .keysyms
            .chunks(per)
            .position(|row| row[0] == keysym)
            .unwrap_or_else(|| panic!("no key types {keysym:#x}"));
        first + index as u8
    }

    /// The red, green and blue of every pixel of `window`'s `size`.
    fn pixels(&self, window: u32, size: PhysicalSize) -> Vec<[u8; 3]> {
        let image = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                window,
                0,
                0,
                size.width as u16,
                size.height as u16,
                !0,
            )
            .expect("request")
            .reply()
            .expect("the window is on the screen");
        assert_eq!(image.depth, 24);
        assert_eq!(
            self.conn.setup().image_byte_order,
            x11rb::protocol::xproto::ImageOrder::LSB_FIRST
        );
        image
            .data
            .chunks_exact(4)
            .map(|p| [p[2], p[1], p[0]])
            .collect()
    }

    fn geometry(&self, window: u32) -> (i16, i16, u16, u16, u8) {
        let g = self
            .conn
            .get_geometry(window)
            .expect("request")
            .reply()
            .expect("a window");
        (g.x, g.y, g.width, g.height, g.depth)
    }

    fn map_state(&self, window: u32) -> MapState {
        self.conn
            .get_window_attributes(window)
            .expect("request")
            .reply()
            .expect("a window")
            .map_state
    }

    /// What the server has sent the host since last asked.
    fn received(&self) -> Vec<XEvent> {
        self.sync();
        std::iter::from_fn(|| self.conn.poll_for_event().expect("connected")).collect()
    }

    /// The keys and buttons passed on to the parent since last asked.
    fn passed(&self) -> Vec<KeyPressEvent> {
        self.received()
            .into_iter()
            .filter_map(|event| match event {
                XEvent::KeyPress(e) | XEvent::KeyRelease(e) => Some(e),
                XEvent::ButtonPress(e) | XEvent::ButtonRelease(e) => Some(KeyPressEvent {
                    response_type: e.response_type,
                    detail: e.detail,
                    sequence: e.sequence,
                    time: e.time,
                    root: e.root,
                    event: e.event,
                    child: e.child,
                    root_x: e.root_x,
                    root_y: e.root_y,
                    event_x: e.event_x,
                    event_y: e.event_y,
                    state: e.state,
                    same_screen: e.same_screen,
                }),
                _ => None,
            })
            .collect()
    }

    fn set_resources(&self, resources: Option<&str>) {
        let root = self.conn.setup().roots[0].root;
        match resources {
            Some(text) => {
                self.conn
                    .change_property8(
                        PropMode::REPLACE,
                        root,
                        AtomEnum::RESOURCE_MANAGER,
                        AtomEnum::STRING,
                        text.as_bytes(),
                    )
                    .expect("request");
            }
            None => {
                self.conn
                    .delete_property(root, AtomEnum::RESOURCE_MANAGER.into())
                    .expect("request");
            }
        }
        self.sync();
    }

    fn focus_parent(&self) {
        self.conn
            .set_input_focus(InputFocus::PARENT, self.parent, x11rb::CURRENT_TIME)
            .expect("request")
            .check()
            .expect("the parent takes the keyboard");
    }
}

// ---------------------------------------------------------------------------
// The editor under test.

/// What the test and the window's handler share.
#[derive(Default)]
struct Seen {
    events: RefCell<Vec<Event>>,
    /// What the handler presents when asked to paint.
    picture: RefCell<Option<Pixmap>>,
    /// Which events the handler says it used.
    uses: Cell<Uses>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
enum Uses {
    #[default]
    Everything,
    Nothing,
    /// The characters typed, but not the keys.
    Text,
}

struct Editor {
    // Dropped first, as a wrapper drops an editor before its link.
    window: Window,
    link: Option<HostLink>,
    host_loop: Rc<TestLoop>,
    seen: Rc<Seen>,
}

fn context(host_loop: &Rc<TestLoop>) -> (EditorContext, HostLink) {
    let host = RecordingEditorHost::new().with_loop(host_loop.clone());
    EditorContext::attach(Rc::new(host))
}

fn open_with(
    host: &Host,
    options: WindowOptions,
    host_loop: Rc<TestLoop>,
) -> Result<Editor, WindowError> {
    let (context, link) = context(&host_loop);
    let seen = Rc::new(Seen::default());
    let handler = {
        let seen = Rc::clone(&seen);
        move |window: &WindowCx<'_>, event: Event| {
            seen.events.borrow_mut().push(event);
            if let (Event::Paint(damage), Some(picture)) = (event, seen.picture.borrow().as_ref()) {
                window.present(picture, damage.rects()).expect("open");
            }
            match (seen.uses.get(), event) {
                (Uses::Everything, _) | (Uses::Text, Event::Text(_)) => Handled::Yes,
                _ => Handled::No,
            }
        }
    };
    let window = Window::open_with_context(host.parent(), options, &context, handler)?;
    Ok(Editor {
        window,
        link: Some(link),
        host_loop,
        seen,
    })
}

impl Editor {
    /// A visible window of `size` on a host that offers both a watch and
    /// timers, after its first paint.
    fn open(host: &Host, size: PhysicalSize) -> Editor {
        let editor =
            open_with(host, WindowOptions::new(size), TestLoop::new(true, true)).expect("a window");
        editor.painted();
        editor
    }

    fn painted(&self) {
        self.until("a paint", |events| {
            events.iter().any(|e| matches!(e, Event::Paint(_)))
        });
        self.take();
    }

    fn id(&self) -> u32 {
        u32::try_from(self.window.raw_handle().expect("open")).expect("an X id")
    }

    /// Turns the host's loop until `done` says the events so far are enough.
    fn until(&self, what: &str, done: impl Fn(&[Event]) -> bool) {
        let end = Instant::now() + PATIENCE;
        while !done(&self.seen.events.borrow()) {
            assert!(
                Instant::now() < end,
                "no {what}; saw {:#?}",
                self.seen.events.borrow()
            );
            self.host_loop.turn(Duration::from_millis(20));
        }
    }

    /// Turns the host's loop for `time`.
    fn run(&self, time: Duration) {
        let end = Instant::now() + time;
        while Instant::now() < end {
            self.host_loop.turn(Duration::from_millis(10));
        }
    }

    fn take(&self) -> Vec<Event> {
        self.seen.events.take()
    }

    /// Waits for `count` events that `wanted` picks and returns them,
    /// forgetting every event.
    fn expect(&self, what: &str, count: usize, wanted: impl Fn(&Event) -> bool) -> Vec<Event> {
        self.until(what, |events| {
            events.iter().filter(|e| wanted(e)).count() >= count
        });
        self.take().into_iter().filter(|e| wanted(e)).collect()
    }

    /// Turns the loop until the server shows `expected` in the window.
    fn shows(&self, host: &Host, expected: &[[u8; 3]]) {
        let end = Instant::now() + PATIENCE;
        loop {
            let shown = host.pixels(self.id(), self.window.size());
            if shown == expected {
                return;
            }
            if Instant::now() >= end {
                let wrong: Vec<usize> = (0..expected.len().min(shown.len()))
                    .filter(|&i| shown[i] != expected[i])
                    .collect();
                panic!(
                    "{} of {} pixels differ, the first at index {:?}",
                    wrong.len(),
                    expected.len(),
                    wrong.first()
                );
            }
            self.host_loop.turn(Duration::from_millis(10));
        }
    }
}

/// An opaque picture whose every pixel says where it is.
fn pattern(width: u32, height: u32, tint: u8) -> Pixmap {
    let mut data = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[
                (x % 251) as u8,
                (y % 241) as u8,
                tint ^ ((x / 251 + y / 241 * 16) as u8),
                255,
            ]);
        }
    }
    Pixmap::from_premultiplied(width, height, data).expect("opaque")
}

/// What `GetImage` should give for `picture`.
fn rgb(picture: &Pixmap) -> Vec<[u8; 3]> {
    picture
        .data()
        .chunks_exact(4)
        .map(|p| [p[0], p[1], p[2]])
        .collect()
}

fn is_key(event: &Event) -> bool {
    matches!(event, Event::Key { .. } | Event::Text(_))
}

fn is_pointer(event: &Event) -> bool {
    matches!(
        event,
        Event::PointerDown { .. }
            | Event::PointerMove(_)
            | Event::PointerUp { .. }
            | Event::PointerLeave
            | Event::PointerCancel
    )
}

fn key(key: Key, modifiers: Modifiers, pressed: bool) -> Event {
    Event::Key {
        key: KeyEvent {
            key,
            modifiers,
            pressed,
        },
        repeat: false,
    }
}

fn at(x: i32, y: i32, scale: f32) -> PointerEvent {
    PointerEvent {
        position: Point::new(x as f32 / scale, y as f32 / scale),
        physical: PhysicalPoint::new(x, y),
        modifiers: Modifiers::NONE,
    }
}

const SMALL: PhysicalSize = PhysicalSize::new(200, 150);
const PARENT: PhysicalSize = PhysicalSize::new(400, 300);

// ---------------------------------------------------------------------------
// Opening and closing.

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_window_opens_inside_the_parent_and_goes_with_its_value() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let id = editor.id();
    assert_eq!(host.children(), [id]);
    assert_eq!(host.geometry(id), (0, 0, 200, 150, 24));
    assert_eq!(host.map_state(id), MapState::VIEWABLE);
    assert_eq!(editor.host_loop.fds().len(), 1, "one watch");
    assert_eq!(editor.host_loop.timers(), [] as [u128; 0], "and no timer");
    assert_eq!(editor.window.scale(), 1.0);
    editor.window.set_timer(TimerId(1), 30).expect("open");
    assert_eq!(editor.host_loop.timers(), [30]);
    let host_loop = Rc::clone(&editor.host_loop);
    drop(editor);
    assert_eq!(host_loop.registrations(), 0);
    // Another connection hears of it when the server has read our requests.
    let end = Instant::now() + PATIENCE;
    while !host.children().is_empty() {
        assert!(Instant::now() < end, "the window stayed");
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn no_window_opens_without_the_hosts_loop() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let handler = |_: &WindowCx<'_>, _: Event| Handled::No;
    let options = WindowOptions::new(SMALL);
    // `open` has no loop to give it.
    assert_eq!(
        Window::open(host.parent(), options, handler).err(),
        Some(WindowError::NoHostLoop)
    );
    // A host that offers no loop.
    let (context, _link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    assert_eq!(
        Window::open_with_context(host.parent(), options, &context, handler).err(),
        Some(WindowError::NoHostLoop)
    );
    // A loop that refuses both a watch and a timer.
    let host_loop = TestLoop::new(false, false);
    assert_eq!(
        open_with(&host, options, Rc::clone(&host_loop)).err(),
        Some(WindowError::NoHostLoop)
    );
    assert_eq!(host_loop.registrations(), 0);
    // A context whose editor is closed.
    let host_loop = TestLoop::new(true, true);
    let (context, link) = self::context(&host_loop);
    drop(link);
    assert_eq!(
        Window::open_with_context(host.parent(), options, &context, handler).err(),
        Some(WindowError::NoHostLoop)
    );
    std::thread::sleep(QUIET);
    assert_eq!(host.children(), [] as [u32; 0]);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn parents_and_options_that_cannot_be_used_are_refused() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let host_loop = TestLoop::new(true, true);
    let (context, _link) = context(&host_loop);
    let open = |parent: u32, options: WindowOptions| {
        let parent = ParentWindow::x11(u64::from(parent)).expect("not zero");
        Window::open_with_context(parent, options, &context, |_: &WindowCx<'_>, _| Handled::No)
            .err()
    };
    let options = WindowOptions::new(SMALL);
    assert_eq!(
        open(0x1FFF_FFF0, options),
        Some(WindowError::InvalidParent("not a window"))
    );
    let input_only = host.conn.generate_id().expect("an id");
    host.conn
        .create_window(
            0,
            input_only,
            host.parent,
            0,
            0,
            10,
            10,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )
        .expect("request")
        .check()
        .expect("an input-only window");
    assert_eq!(
        open(input_only, options),
        Some(WindowError::InvalidParent("an input-only window"))
    );
    let huge = PhysicalSize::new(20_000, 10);
    assert_eq!(
        open(host.parent, WindowOptions::new(huge)),
        Some(WindowError::InvalidSize(huge))
    );
    assert_eq!(
        open(host.parent, options.with_scale(0.0)),
        Some(WindowError::InvalidScale(0.0))
    );
    assert_eq!(host_loop.registrations(), 0);
    assert_eq!(host.children(), [input_only]);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_parent_of_another_depth_is_no_mismatch() {
    let _serial = serial();
    let mut host = Host::new(PARENT);
    // A parent with a 32-bit visual, as hosts with translucent windows have.
    let visual = host.conn.setup().roots[0]
        .allowed_depths
        .iter()
        .find(|depth| depth.depth == 32)
        .and_then(|depth| depth.visuals.first())
        .map(|visual| visual.visual_id)
        .expect("Xvfb offers a 32-bit visual");
    let colormap = host.conn.generate_id().expect("an id");
    host.conn
        .create_colormap(
            x11rb::protocol::xproto::ColormapAlloc::NONE,
            colormap,
            host.root,
            visual,
        )
        .expect("request");
    let deep = host.conn.generate_id().expect("an id");
    host.conn
        .create_window(
            32,
            deep,
            host.root,
            PARENT_AT.0,
            PARENT_AT.1,
            400,
            300,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &CreateWindowAux::new()
                .override_redirect(1)
                .border_pixel(0)
                .colormap(colormap),
        )
        .expect("request")
        .check()
        .expect("a 32-bit window");
    host.conn.destroy_window(host.parent).expect("request");
    host.conn.map_window(deep).expect("request");
    host.sync();
    host.parent = deep;
    let editor = Editor::open(&host, SMALL);
    let picture = pattern(200, 150, 7);
    editor.window.present_all(&picture).expect("open");
    editor.shows(&host, &rgb(&picture));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn windows_open_and_close_many_times_and_leave_nothing() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let host_loop = TestLoop::new(true, true);
    for cycle in 0..200 {
        let editor = open_with(&host, WindowOptions::new(SMALL), Rc::clone(&host_loop))
            .unwrap_or_else(|error| panic!("cycle {cycle}: {error}"));
        editor.window.set_timer(TimerId(1), 10).expect("open");
        if cycle % 10 == 0 {
            editor.painted();
        }
    }
    assert_eq!(host_loop.registrations(), 0);
    let end = Instant::now() + PATIENCE;
    while !host.children().is_empty() {
        assert!(
            Instant::now() < end,
            "windows stayed: {:?}",
            host.children()
        );
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_dropped_link_cuts_the_window_off() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let mut editor = Editor::open(&host, SMALL);
    editor.window.set_timer(TimerId(1), 10).expect("open");
    drop(editor.link.take());
    assert_eq!(editor.host_loop.registrations(), 0);
    assert!(editor.window.set_timer(TimerId(2), 10).is_err());
    host.pointer(10, 10);
    host.click(1);
    editor.run(QUIET);
    assert_eq!(editor.take(), []);
    drop(editor);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_destroyed_parent_closes_the_window() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    editor.window.set_timer(TimerId(1), 1000).expect("open");
    host.conn.destroy_window(host.parent).expect("request");
    host.sync();
    editor.until("the end", |events| events.contains(&Event::Closed));
    assert!(!editor.window.is_open());
    assert_eq!(editor.window.raw_handle(), None);
    assert_eq!(editor.host_loop.registrations(), 0);
    let picture = pattern(4, 4, 0);
    assert_eq!(
        editor.window.present_all(&picture),
        Err(WindowError::Closed)
    );
    editor.run(QUIET);
    assert_eq!(editor.take().last(), Some(&Event::Closed));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_window_closes_itself() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    editor.window.close();
    assert_eq!(editor.take(), [Event::Closed]);
    assert!(!editor.window.is_open());
    assert_eq!(editor.host_loop.registrations(), 0);
    editor.window.close();
    editor.run(QUIET);
    assert_eq!(editor.take(), []);
    assert_eq!(host.children(), [] as [u32; 0]);
}

/// Lets `SIGPIPE` end the process, as in a host that never thought of it,
/// until dropped.
struct Sigpipe(libc::sighandler_t);

impl Sigpipe {
    fn deadly() -> Sigpipe {
        // SAFETY: sets the disposition of SIGPIPE to the system's default;
        // no handler of ours runs. Every test holds the lock, so none of
        // them writes to a closed pipe meanwhile but the one asking.
        Sigpipe(unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) })
    }
}

impl Drop for Sigpipe {
    fn drop(&mut self) {
        // SAFETY: puts back what was there.
        unsafe {
            libc::signal(libc::SIGPIPE, self.0);
        }
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_lost_connection_closes_the_window_without_a_signal() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let _deadly = Sigpipe::deadly();
    // The server drops the window's connection, as `xkill` makes it.
    host.conn.kill_client(editor.id()).expect("request");
    host.sync();
    // Writing to it must not end this process.
    let picture = pattern(200, 150, 1);
    for _ in 0..3 {
        editor.window.present_all(&picture).expect("not yet known");
        editor.window.invalidate_all();
    }
    editor.until("the end", |events| events.contains(&Event::Closed));
    assert!(!editor.window.is_open());
    assert_eq!(editor.host_loop.registrations(), 0);
    assert_eq!(host.children(), [] as [u32; 0]);
}

// ---------------------------------------------------------------------------
// Pixels.

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn presented_pictures_read_back_from_the_server() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor =
        open_with(&host, WindowOptions::new(SMALL), TestLoop::new(true, true)).expect("a window");
    let first = pattern(200, 150, 0);
    *editor.seen.picture.borrow_mut() = Some(first.clone());
    let paints = editor.expect("the first paint", 1, |e| matches!(e, Event::Paint(_)));
    assert!(
        matches!(paints[0], Event::Paint(damage) if damage.rects() == [SMALL.bounds()]),
        "{paints:?}"
    );
    editor.shows(&host, &rgb(&first));
    assert_eq!(editor.window.presented_pixels(), 200 * 150);

    // Only the rectangle asked for changes.
    let second = pattern(200, 150, 0x80);
    let rect = IRect::new(30, 40, 50, 20);
    editor.window.present(&second, &[rect]).expect("open");
    let mut expected = rgb(&first);
    for y in 40..60 {
        for x in 30..80 {
            expected[y * 200 + x] = rgb(&second)[y * 200 + x];
        }
    }
    editor.shows(&host, &expected);
    assert_eq!(editor.window.presented_pixels(), 200 * 150 + 50 * 20);

    // A smaller picture at an origin, and a colour.
    let small = pattern(10, 8, 0x33);
    editor
        .window
        .present_at(&small, &[small.bounds()], PhysicalPoint::new(195, -2))
        .expect("open");
    for y in 0..6 {
        for x in 195..200 {
            expected[y * 200 + x] = rgb(&small)[(y + 2) * 10 + x - 195];
        }
    }
    editor
        .window
        .present_color(Color::rgb(10, 200, 30), &[IRect::new(0, 140, 7, 100)])
        .expect("open");
    for y in 140..150 {
        for x in 0..7 {
            expected[y * 200 + x] = [10, 200, 30];
        }
    }
    editor.shows(&host, &expected);

    // Alpha shows as the premultiplied colour over black.
    let faint = Pixmap::from_premultiplied(1, 1, vec![40, 50, 60, 100]).expect("premultiplied");
    editor.window.present_all(&faint).expect("open");
    expected[0] = [40, 50, 60];
    editor.shows(&host, &expected);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_picture_larger_than_one_request_arrives_whole() {
    let _serial = serial();
    let size = PhysicalSize::new(2300, 2100);
    let host = Host::new(size);
    assert!(
        u32::from(host.screen.0) >= 2320 && u32::from(host.screen.1) >= 2130,
        "the screen is {:?}: run Xvfb with -screen 0 2560x2560x24",
        host.screen
    );
    // The picture is larger than the largest request the server takes.
    let largest = x11rb::connection::RequestConnection::maximum_request_bytes(&host.conn);
    assert!(
        (size.width * size.height * 4) as usize > largest,
        "{largest} bytes a request"
    );
    let editor = Editor::open(&host, size);
    let picture = pattern(size.width, size.height, 0x5A);
    editor.window.present_all(&picture).expect("open");
    editor.shows(&host, &rgb(&picture));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn an_expose_is_answered_from_the_back_buffer() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let picture = pattern(200, 150, 0x11);
    editor.window.present_all(&picture).expect("open");
    editor.shows(&host, &rgb(&picture));

    // Someone else paints over the window.
    let graphics = host.conn.generate_id().expect("an id");
    host.conn
        .create_gc(
            graphics,
            editor.id(),
            &CreateGCAux::new().foreground(0x00FF_FFFF),
        )
        .expect("request");
    let lost = Rectangle {
        x: 20,
        y: 10,
        width: 100,
        height: 90,
    };
    host.conn
        .poly_fill_rectangle(editor.id(), graphics, &[lost])
        .expect("request");
    host.sync();
    assert_eq!(host.pixels(editor.id(), SMALL)[20 * 200 + 30], [255; 3]);
    editor.take();
    // The server says that part is to be drawn again.
    host.conn
        .clear_area(true, editor.id(), lost.x, lost.y, lost.width, lost.height)
        .expect("request");
    host.sync();
    editor.shows(&host, &rgb(&picture));
    editor.run(QUIET);
    assert_eq!(editor.take(), [], "without asking the handler");
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn invalidating_leads_to_a_paint() {
    let _serial = serial();
    let host = Host::new(PARENT);
    for (fds, timers) in [(true, true), (true, false), (false, true)] {
        let editor = open_with(&host, WindowOptions::new(SMALL), TestLoop::new(fds, timers))
            .expect("a window");
        editor.painted();
        editor.run(QUIET);
        editor.take();
        // Nothing comes from the server: the window sees to it.
        let rect = IRect::new(5, 6, 7, 8);
        editor.window.invalidate(rect);
        let paints = editor.expect("a paint", 1, |e| matches!(e, Event::Paint(_)));
        assert!(
            matches!(paints[0], Event::Paint(damage) if damage.rects() == [rect]),
            "{paints:?}"
        );
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_hidden_window_is_drawn_once_shown() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = open_with(
        &host,
        WindowOptions::new(SMALL).hidden(),
        TestLoop::new(true, true),
    )
    .expect("a window");
    let picture = pattern(200, 150, 0x44);
    *editor.seen.picture.borrow_mut() = Some(picture.clone());
    editor.window.invalidate_all();
    editor.run(QUIET);
    assert_eq!(editor.take(), []);
    assert_eq!(host.map_state(editor.id()), MapState::UNMAPPED);
    assert!(!editor.window.is_visible());
    editor.window.set_visible(true).expect("open");
    editor.expect("a paint", 1, |e| matches!(e, Event::Paint(_)));
    editor.shows(&host, &rgb(&picture));
    assert_eq!(host.map_state(editor.id()), MapState::VIEWABLE);
    editor.window.set_visible(false).expect("open");
    let end = Instant::now() + PATIENCE;
    while host.map_state(editor.id()) != MapState::UNMAPPED {
        assert!(Instant::now() < end);
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn the_window_and_the_host_resize_it() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let picture = pattern(320, 240, 0x22);
    *editor.seen.picture.borrow_mut() = Some(picture.clone());

    let larger = PhysicalSize::new(260, 180);
    editor.window.resize(larger).expect("open");
    assert_eq!(editor.window.size(), larger);
    assert_eq!(host.geometry(editor.id()), (0, 0, 260, 180, 24));
    editor.until("the new part", |events| {
        events.iter().any(|e| matches!(e, Event::Paint(_)))
    });
    let events = editor.take();
    assert_eq!(events[0], Event::Resized(larger), "{events:?}");
    editor.run(QUIET);
    // The whole picture, so that what is read back is known everywhere.
    editor.window.present_all(&picture).expect("open");
    let cut = |size: PhysicalSize| -> Vec<[u8; 3]> {
        rgb(&picture)
            .chunks(320)
            .take(size.height as usize)
            .flat_map(|row| row[..size.width as usize].to_vec())
            .collect()
    };
    editor.shows(&host, &cut(larger));
    editor.take();

    // The host resizes the window itself.
    host.conn
        .configure_window(
            editor.id(),
            &ConfigureWindowAux::new().width(320).height(100),
        )
        .expect("request");
    host.sync();
    let new = PhysicalSize::new(320, 100);
    editor.until("the new size", |events| {
        events.contains(&Event::Resized(new))
    });
    assert_eq!(editor.window.size(), new);
    editor.shows(&host, &cut(new));

    // X11 has no window without pixels.
    editor.window.resize(PhysicalSize::new(0, 0)).expect("open");
    assert_eq!(editor.window.size(), PhysicalSize::new(1, 1));
    editor.run(QUIET);
    assert_eq!(editor.window.size(), PhysicalSize::new(1, 1));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn quick_resizes_report_no_stale_size() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let sizes = [(210, 150), (220, 160), (230, 170)].map(|(w, h)| PhysicalSize::new(w, h));
    for size in sizes {
        editor.window.resize(size).expect("open");
    }
    editor.run(QUIET);
    let resized: Vec<Event> = editor
        .take()
        .into_iter()
        .filter(|e| matches!(e, Event::Resized(_)))
        .collect();
    assert_eq!(resized, sizes.map(Event::Resized));
}

// ---------------------------------------------------------------------------
// The pointer.

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn presses_moves_and_clicks_arrive() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    host.pointer(10, 12);
    assert_eq!(
        editor.expect("a move", 1, is_pointer),
        [Event::PointerMove(at(10, 12, 1.0))]
    );
    for _ in 0..2 {
        host.click(1);
    }
    host.click(3);
    host.click(2);
    let down = |button, clicks| Event::PointerDown {
        pointer: at(10, 12, 1.0),
        button,
        clicks,
    };
    let up = |button| Event::PointerUp {
        pointer: at(10, 12, 1.0),
        button,
    };
    assert_eq!(
        editor.expect("clicks", 8, is_pointer),
        [
            down(Button::Primary, 1),
            up(Button::Primary),
            down(Button::Primary, 2),
            up(Button::Primary),
            down(Button::Secondary, 1),
            up(Button::Secondary),
            down(Button::Middle, 1),
            up(Button::Middle),
        ]
    );
    // Later, a press counts from one again.
    std::thread::sleep(Duration::from_millis(600));
    host.click(2);
    assert_eq!(
        editor.expect("a click", 2, is_pointer),
        [down(Button::Middle, 1), up(Button::Middle)]
    );
    // Leaving with no button held.
    host.pointer(300, 200);
    assert_eq!(
        editor.expect("a leave", 1, is_pointer),
        [Event::PointerLeave]
    );
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_drag_goes_on_outside_the_window() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    host.pointer(10, 12);
    host.button(1, true);
    editor.expect("a press", 2, is_pointer);
    assert!(editor.window.has_capture());
    // Outside the window, and outside the parent.
    host.pointer(250, 100);
    host.pointer(-10, -15);
    assert_eq!(
        editor.expect("moves", 2, is_pointer),
        [
            Event::PointerMove(at(250, 100, 1.0)),
            Event::PointerMove(at(-10, -15, 1.0)),
        ]
    );
    host.button(1, false);
    assert_eq!(
        editor.expect("the end", 2, is_pointer),
        [
            Event::PointerUp {
                pointer: at(-10, -15, 1.0),
                button: Button::Primary,
            },
            Event::PointerLeave,
        ]
    );
    assert!(!editor.window.has_capture());
    editor.run(QUIET);
    assert_eq!(editor.take(), []);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_drag_is_cancelled_when_the_window_goes_out_of_sight() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    // By the host, hiding a parent; by the window, hiding itself.
    for by_host in [true, false] {
        host.pointer(10, 12);
        host.button(1, true);
        editor.until("a press", |events| {
            events
                .iter()
                .any(|e| matches!(e, Event::PointerDown { .. }))
        });
        editor.take();
        if by_host {
            host.conn.unmap_window(host.parent).expect("request");
            host.sync();
        } else {
            editor.window.set_visible(false).expect("open");
        }
        editor.until("the cancel", |events| {
            events.contains(&Event::PointerCancel)
        });
        assert!(!editor.window.has_capture());
        host.button(1, false);
        editor.run(QUIET);
        let events = editor.take();
        assert!(
            !events.iter().any(|e| matches!(e, Event::PointerUp { .. })),
            "{events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == Event::PointerCancel)
                .count(),
            1
        );
        if by_host {
            host.conn.map_window(host.parent).expect("request");
        } else {
            editor.window.set_visible(true).expect("open");
        }
        editor.run(QUIET);
        editor.take();
        host.pointer_to_root(1, 1);
        editor.run(QUIET);
        editor.take();
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn wheel_notches_arrive_as_lines() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    host.pointer(40, 50);
    editor.expect("a move", 1, is_pointer);
    for button in 4..=7 {
        host.click(button);
    }
    let wheel = |x, y| Event::Wheel {
        pointer: at(40, 50, 1.0),
        delta: Point::new(x, y),
    };
    let turned = editor.expect("four notches", 4, |e| matches!(e, Event::Wheel { .. }));
    assert_eq!(
        turned,
        [
            wheel(0.0, 1.0),
            wheel(0.0, -1.0),
            wheel(-1.0, 0.0),
            wheel(1.0, 0.0),
        ]
    );
    editor.run(QUIET);
    assert_eq!(editor.take(), [], "a notch is no button");
    assert!(host.passed().is_empty(), "all were used");
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn an_unused_wheel_notch_goes_on_to_the_parent() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    // The host places the window away from its corner.
    host.conn
        .configure_window(editor.id(), &ConfigureWindowAux::new().x(15).y(25))
        .expect("request");
    host.sync();
    editor.run(QUIET);
    editor.seen.uses.set(Uses::Nothing);
    host.pointer(55, 75);
    editor.expect("a move", 1, is_pointer);
    host.passed();
    host.click(5);
    editor.expect("a notch", 1, |e| matches!(e, Event::Wheel { .. }));
    editor.run(QUIET);
    let passed = host.passed();
    let summary: Vec<_> = passed
        .iter()
        .map(|e| {
            (
                e.response_type,
                e.detail,
                e.event,
                e.child,
                e.event_x,
                e.event_y,
            )
        })
        .collect();
    let sent = 0x80;
    assert_eq!(
        summary,
        [
            (BUTTON_PRESS | sent, 5, host.parent, editor.id(), 55, 75),
            (BUTTON_RELEASE | sent, 5, host.parent, editor.id(), 55, 75),
        ]
    );
}

// ---------------------------------------------------------------------------
// The keyboard.

const XK_A: u32 = 0x61;
const XK_1: u32 = 0x31;
const XK_SPACE: u32 = 0x20;
const XK_SHIFT: u32 = 0xFFE1;
const XK_CONTROL: u32 = 0xFFE3;

/// An editor with the keyboard.
fn focused(host: &Host) -> Editor {
    let editor = Editor::open(host, SMALL);
    editor.window.take_focus();
    editor.until("the keyboard", |events| {
        events.contains(&Event::Focus(true))
    });
    editor.take();
    editor
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn keys_arrive_with_what_they_type() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    let a = host.keycode(XK_A);
    host.tap(a);
    let none = Modifiers::NONE;
    assert_eq!(
        editor.expect("a", 3, is_key),
        [
            key(Key::Character('a'), none, true),
            Event::Text('a'),
            key(Key::Character('a'), none, false),
        ]
    );
    // With Shift, then with Control.
    let shift = host.keycode(XK_SHIFT);
    let held = Modifiers {
        shift: true,
        ..none
    };
    host.key(shift, true);
    host.tap(host.keycode(XK_1));
    host.key(shift, false);
    assert_eq!(
        editor.expect("an exclamation mark", 5, is_key),
        [
            key(Key::Other, held, true),
            key(Key::Character('1'), held, true),
            Event::Text('!'),
            key(Key::Character('1'), held, false),
            key(Key::Other, none, false),
        ]
    );
    let control = host.keycode(XK_CONTROL);
    let held = Modifiers {
        control: true,
        ..none
    };
    host.key(control, true);
    host.tap(a);
    host.key(control, false);
    assert_eq!(
        editor.expect("a shortcut", 4, is_key),
        [
            key(Key::Other, held, true),
            key(Key::Character('a'), held, true),
            key(Key::Character('a'), held, false),
            key(Key::Other, none, false),
        ]
    );
    // Keys by name, which type nothing.
    for (keysym, named) in [
        (0xFF08, Key::Backspace),
        (0xFF09, Key::Tab),
        (0xFF0D, Key::Enter),
        (0xFF1B, Key::Escape),
        (0xFF50, Key::Home),
        (0xFF51, Key::Left),
        (0xFF52, Key::Up),
        (0xFF53, Key::Right),
        (0xFF54, Key::Down),
        (0xFF55, Key::PageUp),
        (0xFF56, Key::PageDown),
        (0xFF57, Key::End),
        (0xFF63, Key::Insert),
        (0xFFFF, Key::Delete),
        (0xFFBE, Key::Function(1)),
        (0xFFC9, Key::Function(12)),
    ] {
        host.tap(host.keycode(keysym));
        assert_eq!(
            editor.expect("a named key", 2, is_key),
            [key(named, none, true), key(named, none, false)],
            "{keysym:#x}"
        );
    }
    host.tap(host.keycode(XK_SPACE));
    assert_eq!(editor.expect("a space", 3, is_key)[1], Event::Text(' '));
    editor.run(QUIET);
    assert!(host.passed().is_empty(), "all were used");
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn unused_keys_go_on_to_the_parent() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    let a = host.keycode(XK_A);
    host.pointer(30, 40);
    editor.run(QUIET);
    editor.take();
    host.passed();

    editor.seen.uses.set(Uses::Nothing);
    host.tap(a);
    editor.expect("a", 3, is_key);
    editor.run(QUIET);
    let passed = host.passed();
    let summary: Vec<_> = passed
        .iter()
        .map(|e| (e.response_type, e.detail, e.event, e.child))
        .collect();
    let sent = 0x80;
    assert_eq!(
        summary,
        [
            (KEY_PRESS | sent, a, host.parent, editor.id()),
            (KEY_RELEASE | sent, a, host.parent, editor.id()),
        ],
        "each once, though the press made a key and a character"
    );

    // A handler that takes the character has used the press; the release,
    // which types nothing, goes on.
    editor.seen.uses.set(Uses::Text);
    host.tap(a);
    editor.expect("a", 3, is_key);
    editor.run(QUIET);
    let kinds: Vec<u8> = host.passed().iter().map(|e| e.response_type).collect();
    assert_eq!(kinds, [KEY_RELEASE | sent]);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn what_the_parent_sends_back_is_dropped() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    editor.seen.uses.set(Uses::Nothing);
    host.passed();
    host.tap(host.keycode(XK_A));
    editor.expect("a", 3, is_key);
    editor.run(QUIET);
    let passed = host.passed();
    assert_eq!(passed.len(), 2);
    let send_back = |event: KeyPressEvent| {
        let mask = if event.response_type & 0x7F == KEY_PRESS {
            EventMask::KEY_PRESS
        } else {
            EventMask::KEY_RELEASE
        };
        host.conn
            .send_event(false, editor.id(), mask, event)
            .expect("request");
        host.sync();
    };
    for mut event in passed.clone() {
        event.event = editor.id();
        send_back(event);
    }
    editor.run(QUIET);
    assert_eq!(editor.take(), []);
    assert!(host.passed().is_empty());

    // A key a host forwards of its own accord is heard.
    editor.seen.uses.set(Uses::Everything);
    let mut forwarded = passed[0];
    forwarded.event = editor.id();
    forwarded.time = forwarded.time.wrapping_add(1);
    send_back(forwarded);
    assert_eq!(
        editor.expect("a forwarded key", 2, is_key)[1],
        Event::Text('a')
    );
    // Its release never came, and the key is not taken for held: the next
    // press of it is no repeat.
    let a = host.keycode(XK_A);
    host.tap(a);
    let none = Modifiers::NONE;
    assert_eq!(
        editor.expect("a press of its own", 3, is_key)[0],
        key(Key::Character('a'), none, true)
    );
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_held_key_repeats() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    // The server repeats keys: 100 ms before the first, then every 40 ms.
    let control = x11rb::protocol::xproto::ChangeKeyboardControlAux::new()
        .auto_repeat_mode(x11rb::protocol::xproto::AutoRepeatMode::ON);
    host.conn
        .change_keyboard_control(&control)
        .expect("request")
        .check()
        .expect("auto repeat");
    let a = host.keycode(XK_A);
    host.key(a, true);
    // Long enough for the server's own delay, 660 ms unless set.
    editor.until("repeats", |events| {
        events
            .iter()
            .filter(|e| matches!(e, Event::Text('a')))
            .count()
            >= 4
    });
    host.key(a, false);
    editor.until("the release", |events| {
        events
            .iter()
            .any(|e| matches!(e, Event::Key { key, .. } if !key.pressed))
    });
    editor.run(QUIET);
    let keys: Vec<(bool, bool)> = editor
        .take()
        .into_iter()
        .filter_map(|e| match e {
            Event::Key { key, repeat } => Some((key.pressed, repeat)),
            _ => None,
        })
        .collect();
    let repeats = keys.len() - 2;
    assert!(repeats >= 3, "{keys:?}");
    assert_eq!(keys[0], (true, false));
    assert_eq!(keys[1..=repeats], vec![(true, true); repeats]);
    assert_eq!(keys[repeats + 1], (false, false));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_changed_keyboard_map_is_read_again() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    let setup = host.conn.setup();
    let (first, last) = (setup.min_keycode, setup.max_keycode);
    let map = host
        .conn
        .get_keyboard_mapping(first, last - first + 1)
        .expect("request")
        .reply()
        .expect("a keyboard map");
    let per = usize::from(map.keysyms_per_keycode);
    let spare = map
        .keysyms
        .chunks(per)
        .rposition(|row| row.iter().all(|&keysym| keysym == 0))
        .expect("a key that types nothing");
    let code = first + spare as u8;
    let change = |keysyms: &[u32]| {
        host.conn
            .change_keyboard_mapping(1, code, keysyms.len() as u8, keysyms)
            .expect("request")
            .check()
            .expect("a new map");
        host.sync();
    };
    // e with an acute accent, and the Cyrillic ef as a Unicode keysym.
    for (plain, shifted) in [('\u{e9}', '\u{c9}'), ('\u{444}', '\u{424}')] {
        let keysym = |c: char| match u32::from(c) {
            latin @ ..=0xFF => latin,
            other => 0x0100_0000 + other,
        };
        change(&[keysym(plain), keysym(shifted)]);
        editor.run(QUIET);
        editor.take();
        host.tap(code);
        assert_eq!(
            editor.expect("the new key", 3, is_key)[..2],
            [
                key(Key::Character(plain), Modifiers::NONE, true),
                Event::Text(plain)
            ]
        );
    }
    change(&vec![0; per]);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_burst_of_input_arrives_whole_and_in_order() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    let a = host.keycode(XK_A);
    // Far more than one turn of the loop handles, all waiting at once.
    for _ in 0..700 {
        host.conn
            .xtest_fake_input(KEY_PRESS, a, 0, host.root, 0, 0, 0)
            .expect("request");
        host.conn
            .xtest_fake_input(KEY_RELEASE, a, 0, host.root, 0, 0, 0)
            .expect("request");
    }
    host.sync();
    let events = editor.expect("every key", 2100, is_key);
    assert_eq!(events.len(), 2100);
    for three in events.chunks(3) {
        assert!(
            matches!(
                three,
                [
                    Event::Key { key: down, .. },
                    Event::Text('a'),
                    Event::Key { key: up, .. }
                ] if down.pressed && !up.pressed
            ),
            "{three:?}"
        );
    }
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn input_read_during_a_request_is_not_left_waiting() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = focused(&host);
    let a = host.keycode(XK_A);
    for round in 0..20 {
        host.tap(a);
        // Give the key time to reach the window's socket, then ask the
        // server something, which reads it before the loop does.
        std::thread::sleep(Duration::from_millis(5));
        editor
            .window
            .resize(PhysicalSize::new(201 + round, 150))
            .expect("open");
        // Nothing else will be sent: only the window can see to it.
        editor.expect("the key", 3, is_key);
    }
}

// ---------------------------------------------------------------------------
// Focus, scale and timers.

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn the_keyboard_comes_with_a_click_and_goes() {
    let _serial = serial();
    let host = Host::new(PARENT);
    host.focus_parent();
    let editor = Editor::open(&host, SMALL);
    editor.run(QUIET);
    editor.take();
    assert!(!editor.window.has_focus());
    host.pointer(10, 10);
    host.click(1);
    editor.until("the keyboard", |events| {
        events.contains(&Event::Focus(true))
    });
    assert!(editor.window.has_focus());
    editor.take();
    host.focus_parent();
    editor.until("its loss", |events| events.contains(&Event::Focus(false)));
    assert!(!editor.window.has_focus());
    drop(editor);

    let editor = open_with(
        &host,
        WindowOptions::new(SMALL).without_focus_on_click(),
        TestLoop::new(true, true),
    )
    .expect("a window");
    editor.painted();
    host.pointer(10, 10);
    host.click(1);
    editor.expect("the click", 1, |e| matches!(e, Event::PointerUp { .. }));
    editor.run(QUIET);
    assert!(!editor.window.has_focus());
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn the_scale_is_xft_dpi_unless_the_host_gives_one() {
    let _serial = serial();
    let host = Host::new(PARENT);
    host.set_resources(Some("Xft.dpi:\t144\n"));
    let editor = Editor::open(&host, SMALL);
    assert_eq!(editor.window.scale(), 1.5);
    host.pointer(30, 45);
    assert_eq!(
        editor.expect("a move", 1, is_pointer),
        [Event::PointerMove(at(30, 45, 1.5))]
    );
    let changed = |scale: f64| {
        editor.until("the scale", |events| {
            events.contains(&Event::ScaleChanged(scale))
        });
        assert_eq!(editor.window.scale(), scale);
        editor.take();
    };
    host.set_resources(Some("Xft.antialias:\t1\nXft.dpi:\t192\n"));
    changed(2.0);
    host.set_resources(None);
    changed(1.0);
    drop(editor);

    let editor = open_with(
        &host,
        WindowOptions::new(SMALL).with_scale(1.25),
        TestLoop::new(true, true),
    )
    .expect("a window");
    editor.painted();
    host.set_resources(Some("Xft.dpi:\t192\n"));
    editor.run(QUIET);
    assert_eq!(editor.window.scale(), 1.25);
    assert_eq!(editor.take(), []);
    editor.window.set_host_scale(None).expect("open");
    assert_eq!(editor.window.scale(), 2.0);
    host.set_resources(None);
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn timers_are_the_hosts() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = Editor::open(&host, SMALL);
    let window = &editor.window;
    assert_eq!(
        window.set_timer(TimerId(1), 0),
        Err(WindowError::InvalidPeriod)
    );
    window.set_timer(TimerId(1), 15).expect("open");
    window.set_timer(TimerId(2), 40).expect("open");
    assert_eq!(editor.host_loop.timers(), [15, 40]);
    editor.until("ticks of both", |events| {
        let ticks = |id| {
            events
                .iter()
                .filter(|e| **e == Event::Timer(TimerId(id)))
                .count()
        };
        ticks(1) >= 3 && ticks(2) >= 2
    });
    assert!(window.kill_timer(TimerId(1)));
    assert!(!window.kill_timer(TimerId(1)));
    // Setting a timer again replaces it.
    window.set_timer(TimerId(2), 20).expect("open");
    assert_eq!(editor.host_loop.timers(), [20]);
    editor.take();
    editor.until("a tick", |events| {
        events.contains(&Event::Timer(TimerId(2)))
    });
    assert!(!editor.take().contains(&Event::Timer(TimerId(1))));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_host_with_only_timers_is_polled() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor =
        open_with(&host, WindowOptions::new(SMALL), TestLoop::new(false, true)).expect("a window");
    assert_eq!(editor.host_loop.fds(), []);
    assert_eq!(editor.host_loop.timers(), [16]);
    let picture = pattern(200, 150, 0x66);
    *editor.seen.picture.borrow_mut() = Some(picture.clone());
    editor.painted();
    editor.window.present_all(&picture).expect("open");
    editor.shows(&host, &rgb(&picture));
    host.pointer(10, 10);
    host.click(1);
    editor.expect("a click", 1, |e| matches!(e, Event::PointerUp { .. }));
    editor.window.set_timer(TimerId(3), 25).expect("open");
    editor.until("a tick", |events| {
        events.contains(&Event::Timer(TimerId(3)))
    });
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn a_host_with_only_a_watch_refuses_timers() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor =
        open_with(&host, WindowOptions::new(SMALL), TestLoop::new(true, false)).expect("a window");
    editor.painted();
    assert!(matches!(
        editor.window.set_timer(TimerId(1), 20),
        Err(WindowError::Platform { .. })
    ));
    assert!(!editor.window.kill_timer(TimerId(1)));
    host.pointer(10, 10);
    host.click(1);
    editor.expect("a click", 1, |e| matches!(e, Event::PointerUp { .. }));
}

#[test]
#[ignore = "needs an X server; the Window probe (X11) job runs it under xvfb-run"]
fn what_the_backend_lacks_says_so() {
    let _serial = serial();
    let host = Host::new(PARENT);
    let editor = open_with(
        &host,
        WindowOptions::new(SMALL).with_file_drops(),
        TestLoop::new(true, true),
    )
    .expect("a window");
    let window = &editor.window;
    assert!(!window.accepts_drops());
    assert_eq!(window.read_clipboard_text(), None);
    assert_eq!(
        window.write_clipboard_text("-6.0 dB"),
        Err(WindowError::Unavailable("the clipboard"))
    );
    assert_eq!(
        window.open_files(&FileDialog::new()),
        Err(WindowError::Unavailable("file dialogs"))
    );
    assert_eq!(window.set_title("no"), Err(WindowError::NotAppWindow));
}
