//! The X11 backend (Linux): a plugin editor's window inside a host's X
//! window, driven by the host's loop (ADR 0028, part 1).
//!
//! It speaks the X11 protocol through x11rb's own connection, so neither
//! libX11 nor libxcb is needed: the core protocol, and the XKB extension
//! for the keyboard. Each window has to itself a connection to the server
//! `DISPLAY` names, which closes with it.
//!
//! # The host's loop
//!
//! The window has no thread and no loop of its own. It registers the
//! connection's descriptor with the host
//! ([`EditorContext::watch_fd`]) and reads what the server sent each time
//! the host says there is something; where the host watches no descriptor,
//! a timer of the host's reads about every 16 ms instead. A window's own
//! timers are timers of the host's, so a host without them refuses
//! [`set_timer`](crate::Window::set_timer). Where the host offers neither,
//! no window opens ([`WindowError::NoHostLoop`]).
//!
//! Requests that wait for the server's reply, and writes the server is slow
//! to take, can leave events read from the socket but not yet handled, with
//! nothing more to make the descriptor readable. Whenever that may have
//! happened outside a turn of the host's loop, the window sends itself a
//! message, which does.
//!
//! # What it does
//!
//! - The window is made with the screen's 24-bit TrueColor visual whatever
//!   its parent's, so alpha is ignored and a pixel shows as its
//!   premultiplied colour over black. Presenting sends the damaged
//!   rectangles of the back buffer with `PutImage`, cut into as many
//!   requests as the server's request limit needs; an expose is answered
//!   from the back buffer. A side is at least one pixel, as X11 requires.
//! - The scale is the `Xft.dpi` resource over 96, read from the root
//!   window of the first screen and followed when it changes.
//! - The server holds the pointer from the first button down to the last
//!   button up (X11's implicit grab). A drag is cancelled when the window
//!   goes out of sight, as when it or a parent is unmapped: the window
//!   learns of that from the pointer or the keyboard leaving it, or from a
//!   move with no button held. Buttons 4 to 7 are wheel notches. Clicks are
//!   counted with a 500 ms interval and a 4 by 4 pixel box, as the core
//!   protocol has no setting for them.
//! - Keys are read through the server's keyboard map, which is fetched
//!   again when it changes: the layout in use (of the first two), Shift,
//!   Caps Lock, AltGr, Num Lock, and the repeats of a held key (XKB's
//!   detectable auto-repeat). Dead keys, input methods and the older
//!   keysyms of non-Latin alphabets give no text yet.
//! - A key or wheel notch the handler does not use (neither the key nor the
//!   character it types) is sent on towards the parent window with
//!   `SendEvent`; one that comes back is dropped.
//! - There is no clipboard, file dialog or drag and drop yet.
//!
//! # What it does to the process, and what it does not
//!
//! No statics, threads, hooks or process-wide settings, and no name given to
//! the server (it interns no atom). It reads `DISPLAY`, `XAUTHORITY` and
//! `HOME` to connect. On the server it selects `PropertyNotify` on the root
//! window and turns on XKB's detectable auto-repeat, both for its own
//! connection only. Writes never raise `SIGPIPE`. All of it is safe code.
//!
//! The calls that wait for the server (opening, resizing, and reading the
//! keyboard map or the resources again) wait without a time limit, as the
//! libraries of X do.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::os::fd::AsRawFd;
use std::rc::{Rc, Weak};

use manguebyte_gfx::IRect;
use manguebyte_plugin::editor::{EditorContext, LoopHandle};
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xkb::{
    ConnectionExt as _, EventType, MapPart, PerClientFlag, SelectEventsAux,
};
use x11rb::protocol::xproto::{
    AtomEnum, BackPixmap, ButtonPressEvent, ClientMessageEvent, ColormapAlloc, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gravity, ImageFormat, ImageOrder,
    InputFocus, KeyPressEvent, MapState, NotifyDetail, NotifyMode, Screen, VisualClass,
    WindowClass,
};
use x11rb::{CURRENT_TIME, NONE};

use crate::damage::Damage;
use crate::dispatch::{Origin, WindowHandler, drop_quietly};
use crate::event::{Button, Handled, PhysicalPoint, PhysicalSize, RawInput, TimerId, WHEEL_DELTA};
use crate::pixels::BackBuffer;
use crate::window::{Inner, Platform, WindowError, WindowOptions};

mod connect;
mod image;
mod keyboard;
mod resources;

use connect::Connection;
use keyboard::Keyboard;

/// How often the connection is read when the host watches no descriptor, in
/// milliseconds.
const POLL_PERIOD_MS: u32 = 16;

/// The most events handled in one turn of the host's loop, so that a server
/// that never stops talking cannot keep the loop to itself.
const EVENTS_PER_TURN: usize = 512;

/// The most keys and wheel notches that wait for the handler's answer, and
/// the most passed on that are remembered.
const REMEMBERED: usize = 64;

/// The bits of an event's state for the three buttons a window reports.
const BUTTONS_HELD: u16 = 0x100 | 0x200 | 0x400;

/// `SendEvent` marks what it sends with the top bit of the event's type.
const SENT: u8 = 0x80;

/// A key or wheel notch the handler has not finished with, kept so that it
/// can go on to the parent.
struct Passable {
    id: usize,
    event: KeyPressEvent,
    /// How many answers are still to come: the key's and its character's.
    waiting: u8,
    used: bool,
}

/// The X11 side of a window.
pub(crate) struct X11Platform {
    connection: Connection,
    /// The window's id, or 0 once it is gone.
    window: Cell<u32>,
    parent: u32,
    /// The root of the first screen, where the resources are.
    resources_root: u32,
    graphics: u32,
    /// Whether the server wants the most significant byte first.
    reversed: bool,
    /// The most bytes one request may have.
    max_request: usize,
    /// Rows on their way to the server.
    rows: RefCell<Vec<u8>>,
    keyboard: RefCell<Keyboard>,
    /// The keys down, a bit to a keycode.
    held: Cell<[u64; 4]>,
    context: EditorContext,
    /// The shared state, for the host's callbacks.
    me: RefCell<Weak<Inner>>,
    /// The host's watch on the descriptor, or the timer that reads instead.
    reading: RefCell<Option<LoopHandle>>,
    timers: RefCell<Vec<(TimerId, LoopHandle)>>,
    /// Events read and not yet handled.
    backlog: RefCell<VecDeque<XEvent>>,
    /// How many turns of the host's loop are being handled, one inside
    /// another.
    turns: Cell<u32>,
    /// Whether a message to ourselves is on its way.
    waking: Cell<bool>,
    mapped: Cell<bool>,
    /// Damage was announced and the handler is still to be asked.
    paint_wanted: Cell<bool>,
    /// What the server said it lost, until the last of a run of exposes.
    exposed: Cell<Damage>,
    /// Where the window is in its parent.
    offset: Cell<(i16, i16)>,
    next_passable: Cell<usize>,
    passable: RefCell<VecDeque<Passable>>,
    /// The type, key or button, and time of what was passed on.
    passed: RefCell<VecDeque<(u8, u8, u32)>>,
}

/// Counts a turn for as long as it lives, however it ends.
struct Turn<'a>(&'a Cell<u32>);

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get().saturating_sub(1));
    }
}

/// The failure of `call`, which the server refused or never answered.
fn refused(call: &'static str) -> WindowError {
    WindowError::Platform { call, code: 0 }
}

/// A side as the server takes it: at least one pixel.
fn side(pixels: u32) -> u32 {
    pixels.clamp(1, u32::from(u16::MAX))
}

/// The screen's 24-bit TrueColor visual whose red is the high byte, the root's
/// own if it is one.
fn visual(screen: &Screen) -> Option<u32> {
    let fits = |depth: u8| {
        screen
            .allowed_depths
            .iter()
            .filter(move |allowed| allowed.depth == depth)
            .flat_map(|allowed| allowed.visuals.iter())
            .filter(|visual| {
                visual.class == VisualClass::TRUE_COLOR
                    && visual.red_mask == 0x00FF_0000
                    && visual.green_mask == 0x0000_FF00
                    && visual.blue_mask == 0x0000_00FF
            })
    };
    fits(24)
        .find(|visual| visual.visual_id == screen.root_visual)
        .or_else(|| fits(24).next())
        .map(|visual| visual.visual_id)
}

/// The server's keyboard map now.
fn keyboard_map(connection: &Connection) -> Result<Keyboard, WindowError> {
    let setup = connection.setup();
    let first = setup.min_keycode;
    let count = setup.max_keycode.saturating_sub(first).saturating_add(1);
    let map = connection
        .get_keyboard_mapping(first, count)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .ok_or_else(|| refused("GetKeyboardMapping"))?;
    let modifiers = connection
        .get_modifier_mapping()
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .ok_or_else(|| refused("GetModifierMapping"))?;
    Ok(Keyboard::new(
        first,
        map.keysyms_per_keycode,
        map.keysyms,
        &modifiers.keycodes,
    ))
}

/// The scale the resources on `root` ask for now.
fn resource_scale(connection: &Connection, root: u32) -> f64 {
    connection
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            // In units of four bytes: a megabyte of resources at most.
            1 << 18,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .map_or(1.0, |property| resources::scale(&property.value))
}

/// The XKB name for the keyboard the core protocol speaks of.
const CORE_KEYBOARD: u16 = 0x100;

/// Sets the connection up with XKB, where the server has it: a held key's
/// repeats then come as presses alone, with no release between them, and
/// the connection is told when the keyboard map changes. Both are choices of
/// this connection only. A server without XKB sends releases between the
/// repeats, and the core protocol's own notice of a new map.
fn use_xkb(connection: &Connection) {
    let supported = connection
        .xkb_use_extension(1, 0)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|reply| reply.supported);
    if !supported {
        return;
    }
    let flag = PerClientFlag::DETECTABLE_AUTO_REPEAT;
    let none = 0u32.into();
    if let Ok(cookie) = connection.xkb_per_client_flags(CORE_KEYBOARD, flag, flag, none, none, none)
    {
        // Waited for, so that no key arrives before it holds.
        let _ = cookie.reply();
    }
    // A client of XKB hears of a new map through XKB.
    let parts = MapPart::KEY_TYPES | MapPart::KEY_SYMS | MapPart::MODIFIER_MAP;
    let _ = connection.xkb_select_events(
        CORE_KEYBOARD,
        0u16.into(),
        EventType::NEW_KEYBOARD_NOTIFY | EventType::MAP_NOTIFY,
        parts,
        parts,
        &SelectEventsAux::new(),
    );
}

/// Makes a window inside the X window `parent`, driven by the host's loop in
/// `context`.
pub(crate) fn open(
    parent: NonZeroU32,
    options: &WindowOptions,
    context: &EditorContext,
    handler: Box<dyn WindowHandler>,
) -> Result<Rc<Inner>, WindowError> {
    if !context.has_host_loop() {
        return Err(WindowError::NoHostLoop);
    }
    let parent = parent.get();
    let connection = connect::connect()?;
    // Dropping the connection destroys whatever was made on it, so a failure
    // below leaves nothing on the server.
    let geometry = connection
        .get_geometry(parent)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .ok_or(WindowError::InvalidParent("not a window"))?;
    let attributes = connection
        .get_window_attributes(parent)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .ok_or(WindowError::InvalidParent("not a window"))?;
    if attributes.class == WindowClass::INPUT_ONLY {
        return Err(WindowError::InvalidParent("an input-only window"));
    }
    let setup = connection.setup();
    let screen = setup
        .roots
        .iter()
        .find(|screen| screen.root == geometry.root)
        .ok_or(WindowError::InvalidParent("on no screen of this server"))?;
    let visual = visual(screen)
        .filter(|_| {
            setup
                .pixmap_formats
                .iter()
                .any(|format| format.depth == 24 && format.bits_per_pixel == 32)
        })
        .ok_or_else(|| refused("finding a 24-bit TrueColor visual"))?;
    let resources_root = setup
        .roots
        .first()
        .map_or(geometry.root, |screen| screen.root);
    let reversed = setup.image_byte_order == ImageOrder::MSB_FIRST;
    let root = screen.root;
    let (root_visual, default_colormap) = (screen.root_visual, screen.default_colormap);

    let id = || connection.generate_id().map_err(|_| refused("generate_id"));
    let colormap = if visual == root_visual {
        default_colormap
    } else {
        let colormap = id()?;
        connection
            .create_colormap(ColormapAlloc::NONE, colormap, root, visual)
            .map_err(|_| refused("CreateColormap"))?;
        colormap
    };
    let window = id()?;
    let events = EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::LEAVE_WINDOW
        | EventMask::POINTER_MOTION
        | EventMask::EXPOSURE
        | EventMask::STRUCTURE_NOTIFY
        | EventMask::FOCUS_CHANGE;
    // A border and a colormap of our own, so that a parent of another depth
    // is no mismatch; no background, so nothing is cleared before the back
    // buffer is copied.
    let aux = CreateWindowAux::new()
        .background_pixmap(BackPixmap::NONE)
        .border_pixel(0)
        .bit_gravity(Gravity::NORTH_WEST)
        .colormap(colormap)
        .event_mask(events);
    connection
        .create_window(
            24,
            window,
            parent,
            0,
            0,
            // Both are within a u16.
            side(options.size.width) as u16,
            side(options.size.height) as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual,
            &aux,
        )
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .ok_or_else(|| refused("CreateWindow"))?;
    let graphics = id()?;
    connection
        .create_gc(graphics, window, &CreateGCAux::new().graphics_exposures(0))
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .ok_or_else(|| refused("CreateGC"))?;
    // The resources are watched before they are read, so no change is lost.
    connection
        .change_window_attributes(
            resources_root,
            &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE),
        )
        .ok()
        .and_then(|cookie| cookie.check().ok())
        .ok_or_else(|| refused("ChangeWindowAttributes"))?;
    let scale = resource_scale(&connection, resources_root);
    use_xkb(&connection);
    let keyboard = keyboard_map(&connection)?;
    let max_request = connection.maximum_request_bytes();

    let platform = X11Platform {
        connection,
        window: Cell::new(window),
        parent,
        resources_root,
        graphics,
        reversed,
        max_request,
        rows: RefCell::new(Vec::new()),
        keyboard: RefCell::new(keyboard),
        held: Cell::new([0; 4]),
        context: context.clone(),
        me: RefCell::new(Weak::new()),
        reading: RefCell::new(None),
        timers: RefCell::new(Vec::new()),
        backlog: RefCell::new(VecDeque::new()),
        turns: Cell::new(0),
        waking: Cell::new(false),
        mapped: Cell::new(false),
        paint_wanted: Cell::new(false),
        exposed: Cell::new(Damage::new()),
        offset: Cell::new((0, 0)),
        next_passable: Cell::new(1),
        passable: RefCell::new(VecDeque::new()),
        passed: RefCell::new(VecDeque::new()),
    };
    let inner = Rc::new(Inner::new(Platform::X11(platform), options, handler));
    let Platform::X11(platform) = &inner.platform else {
        unreachable!("made just above");
    };
    *platform.me.borrow_mut() = Rc::downgrade(&inner);
    // The starting size and scale: no event for them.
    inner.quiet.set(true);
    inner.init_system_scale(scale);
    inner.on_size(PhysicalSize::new(
        side(options.size.width),
        side(options.size.height),
    ));
    inner.quiet.set(false);

    if let Err(error) = platform.start_reading() {
        // Nobody had the window yet: its handler hears nothing of its end.
        if let Some(handler) = inner.dispatcher().retire() {
            drop_quietly(handler);
        }
        platform.destroy();
        return Err(error);
    }
    if options.visible {
        platform.set_visible(true);
    }
    platform.settle();
    Ok(inner)
}

impl X11Platform {
    /// The window's id, while it exists.
    fn id(&self) -> Option<u32> {
        let window = self.window.get();
        (window != 0).then_some(window)
    }

    pub(crate) fn handle(&self) -> Option<usize> {
        self.id().and_then(|window| usize::try_from(window).ok())
    }

    /// Has the host's loop read the connection: by watching its descriptor,
    /// or else on a timer.
    fn start_reading(&self) -> Result<(), WindowError> {
        let me = self.me.borrow().clone();
        let ready = move || {
            if let Some(inner) = me.upgrade() {
                if let Platform::X11(platform) = &inner.platform {
                    platform.turn(&inner, || {});
                }
            }
        };
        let fd = self.connection.stream().as_raw_fd();
        let reading = self
            .context
            .watch_fd(fd, ready.clone())
            .or_else(|| self.context.every(POLL_PERIOD_MS, ready));
        let reading = reading.ok_or(WindowError::NoHostLoop)?;
        *self.reading.borrow_mut() = Some(reading);
        Ok(())
    }

    /// Cancels everything registered on the host's loop. The handles are
    /// dropped with nothing borrowed, as cancelling drops callbacks.
    fn stop_reading(&self) {
        let reading = self.reading.borrow_mut().take();
        let timers = std::mem::take(&mut *self.timers.borrow_mut());
        drop(reading);
        drop(timers);
    }

    /// One turn of the host's loop: `first`, then what the server sent, then
    /// the paint that is due.
    fn turn(&self, inner: &Inner, first: impl FnOnce()) {
        {
            self.turns.set(self.turns.get().saturating_add(1));
            let _turn = Turn(&self.turns);
            first();
            let mut handled = 0;
            while handled < EVENTS_PER_TURN && inner.is_open() {
                let Some(event) = self.next_event(inner) else {
                    break;
                };
                self.event(inner, event);
                handled += 1;
            }
            if self.paint_wanted.get() && self.mapped.get() {
                self.paint_wanted.set(false);
                inner.paint_request();
            }
        }
        if self.turns.get() == 0 {
            self.settle();
        }
    }

    /// Reads one event from the connection without waiting. Our own message
    /// to ourselves is noted and left out.
    fn read(&self) -> Result<Option<XEvent>, ()> {
        loop {
            match self.connection.poll_for_event() {
                Ok(Some(XEvent::ClientMessage(message))) if self.is_wake(&message) => {
                    self.waking.set(false);
                }
                Ok(event) => return Ok(event),
                Err(_) => return Err(()),
            }
        }
    }

    /// The next event to handle, if there is one. A connection that failed
    /// closes the window.
    fn next_event(&self, inner: &Inner) -> Option<XEvent> {
        let waiting = self.backlog.borrow_mut().pop_front();
        if waiting.is_some() {
            return waiting;
        }
        match self.read() {
            Ok(event) => event,
            Err(()) => {
                self.gone(inner);
                None
            }
        }
    }

    /// After requests made outside a turn, or at the end of one: sends them,
    /// and makes sure that anything already read gets a turn.
    fn settle(&self) {
        if self.turns.get() != 0 || self.id().is_none() {
            return;
        }
        // A failure shows at the next read, in a turn.
        let _ = self.connection.flush();
        let woken_before = self.waking.get();
        // Everything read so far goes to the backlog; a connection that
        // failed is readable, and the next turn finds out.
        while let Ok(Some(event)) = self.read() {
            self.backlog.borrow_mut().push_back(event);
        }
        // A message that was read here will not make the descriptor
        // readable any more.
        let swallowed = woken_before && !self.waking.get();
        let due = self.paint_wanted.get() && self.mapped.get();
        if swallowed || due || !self.backlog.borrow().is_empty() {
            self.wake();
        }
    }

    /// Sends ourselves a message, unless one is on its way, so that the
    /// descriptor becomes readable and the host gives us a turn.
    fn wake(&self) {
        let Some(window) = self.id() else { return };
        if self.waking.replace(true) {
            return;
        }
        // A predefined atom as the type: the backend gives the server no
        // name of its own.
        let message = ClientMessageEvent::new(32, window, AtomEnum::INTEGER, [window, 0, 0, 0, 0]);
        // With no event mask the event goes to the window's maker: us.
        let _ = self
            .connection
            .send_event(false, window, EventMask::NO_EVENT, message);
        let _ = self.connection.flush();
    }

    fn is_wake(&self, message: &ClientMessageEvent) -> bool {
        let window = self.window.get();
        message.format == 32
            && message.window == window
            && message.type_ == u32::from(AtomEnum::INTEGER)
            && message.data.as_data32()[0] == window
    }

    /// The window is gone from the server, or the server from us.
    fn gone(&self, inner: &Inner) {
        self.window.set(0);
        self.stop_reading();
        self.backlog.borrow_mut().clear();
        inner.on_destroyed();
    }

    fn event(&self, inner: &Inner, event: XEvent) {
        let window = self.window.get();
        match event {
            XEvent::Expose(expose) if expose.window == window => {
                let mut exposed = self.exposed.get();
                exposed.add(IRect::new(
                    i32::from(expose.x),
                    i32::from(expose.y),
                    u32::from(expose.width),
                    u32::from(expose.height),
                ));
                if expose.count > 0 {
                    self.exposed.set(exposed);
                    return;
                }
                self.exposed.set(Damage::new());
                // As on Win32: the handler draws what waits, then what was
                // lost is copied from the back buffer.
                self.paint_wanted.set(false);
                inner.paint_request();
                if let Ok(back) = inner.back().try_borrow() {
                    for rect in exposed.rects() {
                        self.blit(&back, *rect);
                    }
                }
            }
            XEvent::ConfigureNotify(configure) if configure.window == window => {
                // The size now, not the notice's, which a later request of
                // ours may have overtaken.
                if let Some(size) = self.query_geometry() {
                    inner.on_size(size);
                }
            }
            XEvent::DestroyNotify(destroy) if destroy.window == window => self.gone(inner),
            XEvent::UnmapNotify(unmap) if unmap.window == window => {
                inner.input(RawInput::CaptureLost);
            }
            XEvent::MotionNotify(motion) if motion.event == window => {
                // A drag whose every button is up lost its release to
                // someone else.
                if u16::from(motion.state) & BUTTONS_HELD == 0 && inner.holds_pointer() {
                    inner.input(RawInput::CaptureLost);
                }
                inner.input(RawInput::PointerMove {
                    at: point(motion.event_x, motion.event_y),
                    modifiers: self.modifiers(u16::from(motion.state)),
                });
            }
            XEvent::LeaveNotify(leave) if leave.event == window => {
                if leave.detail == NotifyDetail::INFERIOR {
                    return;
                }
                // A key's release may go elsewhere once the pointer has.
                if !inner.has_keyboard() {
                    self.held.set([0; 4]);
                }
                if leave.mode == NotifyMode::UNGRAB && inner.holds_pointer() {
                    inner.input(RawInput::CaptureLost);
                } else {
                    self.check_grab(inner);
                }
                inner.input(RawInput::PointerLeft);
            }
            XEvent::ButtonPress(press) if press.event == window => self.press(inner, press),
            XEvent::ButtonRelease(release) if release.event == window => {
                if self.came_back(&release) {
                    return;
                }
                if let Some(button) = button(release.detail) {
                    inner.input(RawInput::PointerUp {
                        at: point(release.event_x, release.event_y),
                        button,
                        modifiers: self.modifiers(u16::from(release.state)),
                    });
                }
            }
            XEvent::KeyPress(press) if press.event == window => {
                if self.came_back(&press) {
                    return;
                }
                // A key a host forwards by hand may have no release: it
                // is not held.
                let repeat = press.response_type & SENT == 0 && self.hold(press.detail, true);
                self.key(inner, press, true, repeat);
            }
            XEvent::KeyRelease(release) if release.event == window => {
                if self.came_back(&release) {
                    return;
                }
                self.hold(release.detail, false);
                self.key(inner, release, false, false);
            }
            XEvent::FocusIn(focus) if focus.event == window => {
                if keyboard_moved(focus.mode, focus.detail) {
                    inner.input(RawInput::Focus(true));
                }
            }
            XEvent::FocusOut(focus) if focus.event == window => {
                if keyboard_moved(focus.mode, focus.detail) {
                    // The releases may go elsewhere.
                    self.held.set([0; 4]);
                    self.check_grab(inner);
                    inner.input(RawInput::Focus(false));
                }
            }
            XEvent::MappingNotify(_)
            | XEvent::XkbMapNotify(_)
            | XEvent::XkbNewKeyboardNotify(_) => {
                if let Ok(keyboard) = keyboard_map(&self.connection) {
                    *self.keyboard.borrow_mut() = keyboard;
                }
            }
            XEvent::PropertyNotify(property)
                if property.window == self.resources_root
                    && property.atom == u32::from(AtomEnum::RESOURCE_MANAGER) =>
            {
                inner.on_system_scale(resource_scale(&self.connection, self.resources_root));
            }
            // Errors too: a request on a window the host destroyed fails,
            // and the notice of that follows.
            _ => {}
        }
    }

    /// Cancels a drag if the window is out of sight. The server then takes
    /// back the pointer it held, and when a parent was unmapped says so only
    /// by taking the pointer or the keyboard away in the ordinary way.
    fn check_grab(&self, inner: &Inner) {
        if !inner.holds_pointer() {
            return;
        }
        let viewable = self.id().is_some_and(|window| {
            self.connection
                .get_window_attributes(window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE)
        });
        if !viewable {
            inner.input(RawInput::CaptureLost);
        }
    }

    /// Where the window is in its parent and how large, as the server says
    /// now.
    fn query_geometry(&self) -> Option<PhysicalSize> {
        let geometry = self
            .connection
            .get_geometry(self.id()?)
            .ok()?
            .reply()
            .ok()?;
        self.offset.set((geometry.x, geometry.y));
        Some(PhysicalSize::new(
            u32::from(geometry.width),
            u32::from(geometry.height),
        ))
    }

    fn modifiers(&self, state: u16) -> crate::Modifiers {
        self.keyboard.borrow().modifiers(state)
    }

    fn press(&self, inner: &Inner, press: ButtonPressEvent) {
        if self.came_back(&press) {
            return;
        }
        let at = point(press.event_x, press.event_y);
        let modifiers = self.modifiers(u16::from(press.state));
        if let Some(button) = button(press.detail) {
            inner.input(RawInput::PointerDown {
                at,
                button,
                modifiers,
                time_ms: press.time,
            });
            return;
        }
        let (delta_x, delta_y) = match press.detail {
            4 => (0, WHEEL_DELTA),
            5 => (0, -WHEEL_DELTA),
            6 => (-WHEEL_DELTA, 0),
            7 => (WHEEL_DELTA, 0),
            _ => return,
        };
        // A button event has the fields of a key event, in the same places.
        let origin = self.passable(
            KeyPressEvent {
                response_type: press.response_type,
                detail: press.detail,
                sequence: press.sequence,
                time: press.time,
                root: press.root,
                event: press.event,
                child: press.child,
                root_x: press.root_x,
                root_y: press.root_y,
                event_x: press.event_x,
                event_y: press.event_y,
                state: press.state,
                same_screen: press.same_screen,
            },
            1,
        );
        inner.input_from(
            RawInput::Wheel {
                at,
                delta_x,
                delta_y,
                modifiers,
            },
            Some(origin),
        );
    }

    /// Notes the key `code` as down or up; returns whether it was down.
    fn hold(&self, code: u8, down: bool) -> bool {
        let mut held = self.held.get();
        let (word, bit) = (usize::from(code / 64), 1u64 << (code % 64));
        let was = held[word] & bit != 0;
        if down {
            held[word] |= bit;
        } else {
            held[word] &= !bit;
        }
        self.held.set(held);
        was
    }

    fn key(&self, inner: &Inner, event: KeyPressEvent, pressed: bool, repeat: bool) {
        let meaning = self
            .keyboard
            .borrow()
            .meaning(event.detail, u16::from(event.state), pressed);
        let origin = self.passable(event, 1 + u8::from(meaning.text.is_some()));
        let key = crate::KeyEvent {
            key: meaning.key,
            modifiers: meaning.modifiers,
            pressed,
        };
        inner.input_from(RawInput::Key { key, repeat }, Some(origin));
        if let Some(c) = meaning.text {
            for unit in c.encode_utf16(&mut [0; 2]) {
                inner.input_from(RawInput::Utf16(*unit), Some(origin));
            }
        }
    }

    /// Keeps `event` until the handler has answered `answers` times.
    fn passable(&self, event: KeyPressEvent, answers: u8) -> Origin {
        let id = self.next_passable.get();
        self.next_passable.set(id.wrapping_add(1));
        let mut passable = self.passable.borrow_mut();
        if passable.len() >= REMEMBERED {
            // Its answer never came: the handler's queue was full.
            passable.pop_front();
        }
        passable.push_back(Passable {
            id,
            event,
            waiting: answers,
            used: false,
        });
        Origin {
            message: u32::from(event.response_type & !SENT),
            wparam: id,
            lparam: 0,
        }
    }

    /// The handler answered an event that came from `origin`.
    pub(crate) fn answered(&self, origin: Origin, handled: Handled) {
        let finished = {
            let mut passable = self.passable.borrow_mut();
            let Some(index) = passable.iter().position(|kept| kept.id == origin.wparam) else {
                return;
            };
            let kept = &mut passable[index];
            kept.used |= handled == Handled::Yes;
            kept.waiting = kept.waiting.saturating_sub(1);
            if kept.waiting > 0 {
                return;
            }
            passable.remove(index)
        };
        if let Some(finished) = finished.filter(|finished| !finished.used) {
            self.pass_on(finished.event);
        }
    }

    /// Sends a key or wheel notch the handler did not use to the parent, as
    /// the server would have had we not asked for it: to the first window
    /// from the parent up whose owner wants such events.
    fn pass_on(&self, mut event: KeyPressEvent) {
        let Some(window) = self.id() else { return };
        let kind = event.response_type & !SENT;
        let (x, y) = self.offset.get();
        event.event = self.parent;
        event.child = window;
        event.event_x = event.event_x.saturating_add(x);
        event.event_y = event.event_y.saturating_add(y);
        // A notch is a press and a release; its release came to us too.
        let kinds: &[(u8, EventMask)] = match kind {
            x11rb::protocol::xproto::KEY_PRESS_EVENT => &[(kind, EventMask::KEY_PRESS)],
            x11rb::protocol::xproto::KEY_RELEASE_EVENT => &[(kind, EventMask::KEY_RELEASE)],
            _ => &[
                (
                    x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                    EventMask::BUTTON_PRESS,
                ),
                (
                    x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                    EventMask::BUTTON_RELEASE,
                ),
            ],
        };
        for &(kind, mask) in kinds {
            event.response_type = kind;
            {
                let mut passed = self.passed.borrow_mut();
                if passed.len() >= REMEMBERED {
                    passed.pop_front();
                }
                passed.push_back((kind, event.detail, event.time));
            }
            let _ = self.connection.send_event(true, self.parent, mask, event);
        }
        self.settle();
    }

    /// Whether `event` is one we passed on, sent back to us: it is dropped,
    /// so that a parent that returns what it gets cannot start a loop.
    fn came_back(&self, event: &impl SentEvent) -> bool {
        let (kind, detail, time) = event.identity();
        if kind & SENT == 0 {
            return false;
        }
        let mut passed = self.passed.borrow_mut();
        let found = passed
            .iter()
            .position(|&sent| sent == (kind & !SENT, detail, time));
        found.map(|index| passed.remove(index)).is_some()
    }

    pub(crate) fn blit(&self, back: &BackBuffer, rect: IRect) {
        let Some(window) = self.id() else { return };
        let size = back.size();
        let Some(rect) = rect.intersect(&size.bounds()) else {
            return;
        };
        let row_bytes = rect.width as usize * 4;
        let most = image::rows_per_request(self.max_request, row_bytes);
        let mut done = 0;
        while done < rect.height {
            let rows = most.min(rect.height - done);
            // `done` is within the back buffer, whose sides fit an i32.
            let band = IRect::new(rect.x, rect.y + done as i32, rect.width, rows);
            let mut packed = self.rows.borrow_mut();
            image::pack_rows(back.bgra(), size.width, band, self.reversed, &mut packed);
            // The back buffer's sides are at most MAX_SIDE, 16384, so the
            // position and size fit the protocol's 16 bits.
            let _ = self.connection.put_image(
                ImageFormat::Z_PIXMAP,
                window,
                self.graphics,
                band.width as u16,
                band.height as u16,
                band.x as i16,
                band.y as i16,
                0,
                24,
                &packed,
            );
            done += rows;
        }
        self.settle();
    }

    pub(crate) fn invalidate(&self, _rect: IRect) {
        self.paint_wanted.set(true);
        self.settle();
    }

    pub(crate) fn set_size(&self, size: PhysicalSize) -> Result<PhysicalSize, WindowError> {
        let window = self.id().ok_or(WindowError::Closed)?;
        let aux = ConfigureWindowAux::new()
            .width(side(size.width))
            .height(side(size.height));
        self.connection
            .configure_window(window, &aux)
            .map_err(|_| refused("ConfigureWindow"))?;
        let actual = self.query_geometry().ok_or_else(|| refused("GetGeometry"));
        self.settle();
        actual
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        let Some(window) = self.id() else { return };
        self.mapped.set(visible);
        let _ = if visible {
            self.connection.map_window(window)
        } else {
            self.connection.unmap_window(window)
        };
        self.settle();
    }

    pub(crate) fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
        self.id().ok_or(WindowError::Closed)?;
        let me = self.me.borrow().clone();
        let tick = move || {
            if let Some(inner) = me.upgrade() {
                if let Platform::X11(platform) = &inner.platform {
                    platform.turn(&inner, || {
                        inner.input(RawInput::Timer(id));
                    });
                }
            }
        };
        let handle = self
            .context
            .every(period_ms, tick)
            .ok_or_else(|| refused("registering a timer with the host"))?;
        // The timer it replaces is cancelled with nothing borrowed.
        let old = {
            let mut timers = self.timers.borrow_mut();
            match timers.iter_mut().find(|(kept, _)| *kept == id) {
                Some(timer) => Some(std::mem::replace(&mut timer.1, handle)),
                None => {
                    timers.push((id, handle));
                    None
                }
            }
        };
        drop(old);
        Ok(())
    }

    pub(crate) fn kill_timer(&self, id: TimerId) {
        let old = {
            let mut timers = self.timers.borrow_mut();
            let index = timers.iter().position(|(kept, _)| *kept == id);
            index.map(|index| timers.remove(index))
        };
        drop(old);
    }

    pub(crate) fn take_focus(&self) {
        let Some(window) = self.id() else { return };
        // Refused by the server, quietly, while the window is not viewable.
        let _ = self
            .connection
            .set_input_focus(InputFocus::PARENT, window, CURRENT_TIME);
        self.settle();
    }

    /// Destroys the window and cancels what it registered with the host. The
    /// connection closes with the shared state.
    pub(crate) fn destroy(&self) {
        self.stop_reading();
        let window = self.window.replace(0);
        if window != NONE {
            let _ = self.connection.destroy_window(window);
            let _ = self.connection.flush();
        }
    }

    /// Destroys the window now and tells the shared state.
    pub(crate) fn close(&self, inner: &Inner) {
        self.destroy();
        inner.on_destroyed();
    }
}

/// What tells one key or button event from another.
trait SentEvent {
    /// Its type with the [`SENT`] bit, key or button, and time.
    fn identity(&self) -> (u8, u8, u32);
}

impl SentEvent for KeyPressEvent {
    fn identity(&self) -> (u8, u8, u32) {
        (self.response_type, self.detail, self.time)
    }
}

impl SentEvent for ButtonPressEvent {
    fn identity(&self) -> (u8, u8, u32) {
        (self.response_type, self.detail, self.time)
    }
}

fn point(x: i16, y: i16) -> PhysicalPoint {
    PhysicalPoint::new(i32::from(x), i32::from(y))
}

/// The pointer button `detail` names; the wheel's and any beyond have none.
fn button(detail: u8) -> Option<Button> {
    match detail {
        1 => Some(Button::Primary),
        2 => Some(Button::Middle),
        3 => Some(Button::Secondary),
        _ => None,
    }
}

/// Whether a focus notice says the keyboard came or went: not one of a
/// passing grab, nor one about the window under the pointer.
fn keyboard_moved(mode: NotifyMode, detail: NotifyDetail) -> bool {
    mode != NotifyMode::GRAB
        && mode != NotifyMode::UNGRAB
        && detail != NotifyDetail::POINTER
        && detail != NotifyDetail::POINTER_ROOT
        && detail != NotifyDetail::NONE
}
