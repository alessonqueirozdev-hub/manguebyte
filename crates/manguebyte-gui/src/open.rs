//! An open editor: the window, the root, and the glue between them.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::panic::{self, AssertUnwindSafe};
use std::rc::Rc;
use std::time::{Duration, Instant};

use manguebyte_plugin::editor::{
    EditorContext, EditorError, HostSize, Key, KeyEvent, OpenEditor, ParentWindow, Resize,
    WindowApi, checked_scale,
};
use manguebyte_ui::{self as ui, MAX_BUFFER_SIDE, Root};
use manguebyte_window::{
    Event, Handled, IRect, PhysicalSize, TimerId, Window, WindowCx, WindowError, WindowHandler,
    WindowOptions,
};

use crate::editor::Editor;

/// How often frames run while an editor is visible, in milliseconds:
/// about 60 a second. Win32 timers fire no faster than every 10 ms and
/// round to the system's tick.
pub const FRAME_PERIOD_MS: u32 = 16;

/// The window timer that runs frames.
pub(crate) const FRAME_TIMER: TimerId = TimerId(1);

/// Which window backend an editor opens with. An editor chooses it when it
/// opens (see `Editor::backend`), so that turning on the `headless`
/// feature only adds API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Backend {
    /// The system's window inside the host's.
    Native,
    /// No system window at all; the test drives it.
    #[cfg_attr(not(any(test, feature = "headless")), allow(dead_code))]
    Headless,
}

impl Backend {
    /// Whether an editor can be put inside a parent of kind `api`.
    pub(crate) fn supports(self, api: WindowApi) -> bool {
        match self {
            Backend::Native => cfg!(windows) && api == WindowApi::Win32,
            Backend::Headless => true,
        }
    }
}

/// Where an editor's time comes from.
pub(crate) enum Clock {
    /// The system's clock, from when the editor opened.
    Real(Instant),
    /// A clock a test moves.
    #[cfg_attr(not(any(test, feature = "headless")), allow(dead_code))]
    Manual(Cell<Duration>),
}

impl Clock {
    pub(crate) fn now(&self) -> Duration {
        match self {
            Clock::Real(start) => start.elapsed(),
            Clock::Manual(now) => now.get(),
        }
    }
}

/// What the open editor and its window handler share.
pub(crate) struct Shared {
    /// The interface, until the editor closes or its window goes.
    pub(crate) root: RefCell<Option<Root>>,
    pub(crate) clock: Clock,
    context: EditorContext,
    /// The editor's size at scale 1.
    base: HostSize,
    rules: Resize,
    /// The scale the host gave, if it gave one.
    host_scale: Cell<Option<f64>>,
    /// The scale the window had at its last event, for working out sizes
    /// when the system's scale changes.
    scale_seen: Cell<f64>,
    /// Whether the host has been told the editor cannot go on.
    reported_closed: Cell<bool>,
    /// While set, a change of size or scale does not run a frame at once:
    /// the caller runs one when it has made all its changes.
    hold: Cell<bool>,
    /// Whether a frame was put off, because the editor was hidden or held.
    stale: Cell<bool>,
    /// Frames run, and frames that presented something.
    pub(crate) frames: Cell<u64>,
    pub(crate) presented: Cell<u64>,
    /// Over all frames: widgets that recorded a new scene, and layouts.
    pub(crate) painted: Cell<u64>,
    pub(crate) layouts: Cell<u64>,
    /// Whether the interface took typed text after the last input.
    typing: Cell<bool>,
    /// The keys and characters given to the interface since the last
    /// frame, with where they came from and what the interface said, so
    /// that the same keystroke arriving both ways is taken once.
    recent: RefCell<Vec<(Path, Typed, Handled)>>,
    /// Whether the window is an application's: the system sizes it for a
    /// new monitor itself, and it hears close requests.
    pub(crate) app: bool,
    /// What a close request does: closes the window unless this says no.
    pub(crate) close_request: RefCell<Option<CloseRequest>>,
    /// What the drag over the window carries, for the interface.
    dropping: RefCell<Option<ui::DropData>>,
}

/// What an application decides on a close request: true to close.
pub(crate) type CloseRequest = Box<dyn FnMut(&WindowCx<'_>) -> bool>;

/// Which way a key reached the editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// The editor's own window received it from the system.
    Window,
    /// The host delivered it through the plugin interface (VST 3's
    /// `onKeyDown` and `onKeyUp`).
    Host,
}

/// A keystroke, as compared between the two paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Typed {
    Key(KeyEvent),
    Text(char),
}

/// The most keystrokes remembered between two frames.
const RECENT: usize = 32;

/// The interface's clipboard: the system's, through the editor's window.
struct WindowClipboard(manguebyte_window::Clipboard);

impl ui::Clipboard for WindowClipboard {
    fn read_text(&self) -> Option<String> {
        self.0.read_text()
    }

    fn write_text(&self, text: &str) -> bool {
        self.0.write_text(text).is_ok()
    }
}

/// The character a key the host delivered types, if it types one: VST 3's
/// `onKeyDown` carries it, and no character message follows. Control (on
/// its own: Control with Alt is AltGr) and Command make shortcuts, not text.
fn typed_by(event: KeyEvent) -> Option<char> {
    let modifiers = event.modifiers;
    let shortcut = (modifiers.control && !modifiers.alt) || modifiers.meta;
    match event.key {
        Key::Character(c) if event.pressed && !shortcut && !c.is_control() => Some(c),
        _ => None,
    }
}

/// `size` with no side larger than the interface can draw
/// ([`MAX_BUFFER_SIDE`] device pixels).
pub(crate) fn cap(size: HostSize) -> HostSize {
    HostSize::new(
        size.width.min(MAX_BUFFER_SIDE),
        size.height.min(MAX_BUFFER_SIDE),
    )
}

/// `rules` with every limit capped (see [`cap`]), so that no size they give
/// is larger than the interface can draw. A shape that no size within the
/// capped limits can have is treated as free, as [`Resize::fit`] does.
fn capped(rules: Resize) -> Resize {
    match rules {
        Resize::Fixed => Resize::Fixed,
        Resize::Free { min, max } => Resize::Free {
            min: cap(min),
            max: cap(max),
        },
        Resize::Ratio {
            min,
            max,
            width,
            height,
        } => Resize::Ratio {
            min: cap(min),
            max: cap(max),
            width,
            height,
        },
    }
}

/// The size in the host's unit that the editor wants at `scale`, given
/// that it is `current` at `old` now: its size at scale 1 scaled for a
/// fixed editor, and otherwise the same logical size, within the rules;
/// never larger than the interface can draw.
fn size_at(base: HostSize, rules: Resize, current: HostSize, old: f64, scale: f64) -> HostSize {
    if !rules.can_resize() {
        return cap(base.scale_by(scale));
    }
    let side = |length: u32| -> u32 {
        let device = (f64::from(length) / old * scale).round();
        // `as` saturates, which is what is wanted at the top.
        (device as u32).max(1)
    };
    let wanted = HostSize::new(side(current.width), side(current.height));
    capped(rules.scaled(scale)).fit(wanted, wanted)
}

fn host_size(size: PhysicalSize) -> HostSize {
    HostSize::new(size.width, size.height)
}

fn physical(size: HostSize) -> PhysicalSize {
    PhysicalSize::new(size.width, size.height)
}

pub(crate) fn editor_error(error: WindowError) -> EditorError {
    match error {
        WindowError::Unsupported(api) => EditorError::Unsupported(api),
        other => EditorError::Platform(other.to_string()),
    }
}

impl Shared {
    /// What an editor of `base` size within `rules` shares with its window,
    /// before either is made: nothing drawn until it is let go (`hold`).
    pub(crate) fn new(
        context: EditorContext,
        base: HostSize,
        rules: Resize,
        scale: Option<f64>,
        clock: Clock,
        app: bool,
    ) -> Rc<Shared> {
        Rc::new(Shared {
            root: RefCell::new(None),
            clock,
            context,
            base,
            rules,
            host_scale: Cell::new(scale),
            scale_seen: Cell::new(scale.unwrap_or(1.0)),
            reported_closed: Cell::new(false),
            hold: Cell::new(true),
            stale: Cell::new(false),
            frames: Cell::new(0),
            presented: Cell::new(0),
            painted: Cell::new(0),
            layouts: Cell::new(0),
            typing: Cell::new(false),
            recent: RefCell::new(Vec::with_capacity(RECENT)),
            app,
            close_request: RefCell::new(None),
            dropping: RefCell::new(None),
        })
    }

    /// Lets the frames go, after the window and the root are made.
    pub(crate) fn release(&self) {
        self.hold.set(false);
    }

    /// The person asked to close the application window: it closes unless
    /// the application's answer says no.
    fn close_requested(&self, window: &WindowCx<'_>) {
        let decide = self.close_request.borrow_mut().take();
        let close = match decide {
            Some(mut decide) => {
                let close = decide(window);
                let mut slot = self.close_request.borrow_mut();
                if slot.is_none() {
                    *slot = Some(decide);
                }
                close
            }
            None => true,
        };
        if close {
            window.close();
        }
    }

    /// Gives a drag from another program to the interface.
    fn file_drop(&self, window: &WindowCx<'_>, event: Event) -> Handled {
        let at = |pointer: manguebyte_window::PointerEvent| pointer.position;
        let drop = match event {
            Event::DragEnter(pointer) => {
                *self.dropping.borrow_mut() = window.drag_data().map(|data| {
                    let dropped = ui::DropData::files(data.files.iter().cloned());
                    match &data.text {
                        Some(text) => dropped.with_text(text.clone()),
                        None => dropped,
                    }
                });
                ui::FileDrop::Enter { at: at(pointer) }
            }
            Event::DragOver(pointer) => ui::FileDrop::Over { at: at(pointer) },
            Event::Drop(pointer) => ui::FileDrop::Drop { at: at(pointer) },
            _ => ui::FileDrop::Leave,
        };
        let data = self.dropping.borrow().clone().unwrap_or_default();
        if matches!(drop, ui::FileDrop::Leave | ui::FileDrop::Drop { .. }) {
            *self.dropping.borrow_mut() = None;
        }
        let handled = self.guarded(window, || {
            let Ok(mut slot) = self.root.try_borrow_mut() else {
                return Handled::No;
            };
            match slot.as_mut().map(|root| root.file_drop(drop, &data)) {
                Some(ui::Handled::Yes) => Handled::Yes,
                _ => Handled::No,
            }
        });
        handled.unwrap_or(Handled::No)
    }

    /// Brings the root to the window's size and scale.
    fn sync(root: &mut Root, window: &WindowCx<'_>) {
        let scale = window.scale() as f32;
        let size = window.logical_size();
        if root.size() != size || root.scale() != scale {
            root.resize(size, scale);
        }
    }

    /// Whether the interface is gone: the editor gave up on it.
    fn is_closed(&self) -> bool {
        self.root.try_borrow().is_ok_and(|root| root.is_none())
    }

    /// Runs `work`, which uses the interface. If it panics, the interface
    /// is dropped and the host is told at once (see [`give_up`]), so that
    /// no gesture stays open and the host does not wait for its next call
    /// to the editor to find out; the frame timer stops. Returns `None`
    /// then.
    ///
    /// [`give_up`]: Self::give_up
    fn guarded<T>(&self, window: &WindowCx<'_>, work: impl FnOnce() -> T) -> Option<T> {
        match panic::catch_unwind(AssertUnwindSafe(work)) {
            Ok(value) => Some(value),
            Err(payload) => {
                // A payload whose drop panics too is not let out.
                let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(payload)));
                window.kill_timer(FRAME_TIMER);
                self.give_up();
                None
            }
        }
    }

    /// Runs one frame and presents what it drew again. Returns whether it
    /// presented anything.
    pub(crate) fn frame(&self, window: &WindowCx<'_>) -> bool {
        self.stale.set(false);
        // A keystroke that has not come the other way by now is not going
        // to.
        self.recent.borrow_mut().clear();
        let presented = self
            .guarded(window, || self.frame_unguarded(window))
            .unwrap_or(false);
        // The caret is where the frame drew it.
        let caret = self.read_root(caret_in_device).flatten();
        window.set_text_caret(caret);
        presented
    }

    fn frame_unguarded(&self, window: &WindowCx<'_>) -> bool {
        let Ok(mut slot) = self.root.try_borrow_mut() else {
            return false;
        };
        let Some(root) = slot.as_mut() else {
            return false;
        };
        Shared::sync(root, window);
        let frame = root.frame(self.clock.now());
        self.frames.set(self.frames.get().wrapping_add(1));
        self.painted
            .set(self.painted.get().wrapping_add(frame.painted as u64));
        if frame.laid_out {
            self.layouts.set(self.layouts.get().wrapping_add(1));
        }
        if frame.damage.is_empty() {
            return false;
        }
        // Fails only once the window is gone, when there is nothing to
        // show anything on.
        let presented = window.present(frame.pixels, frame.damage).is_ok();
        if presented {
            self.presented.set(self.presented.get().wrapping_add(1));
        }
        presented
    }

    /// `read` of the interface, if there is one and it is not busy.
    fn read_root<T>(&self, read: impl FnOnce(&Root) -> T) -> Option<T> {
        let slot = self.root.try_borrow().ok()?;
        slot.as_ref().map(read)
    }

    /// Tells the window what the interface now wants of the keyboard: every
    /// key while it takes typed text or a menu is open, so that a host's
    /// dialog keeps none of Tab, Enter and Escape; the keyboard itself when
    /// either starts and the window does not have it, so that the keys that
    /// follow come to the window rather than through the host; and where
    /// the caret is.
    fn follow_typing(&self, window: &WindowCx<'_>) {
        let Some((wants, caret)) =
            self.read_root(|root| (root.wants_text() || root.has_menu(), caret_in_device(root)))
        else {
            return;
        };
        window.set_wants_all_keys(wants);
        let started = wants && !self.typing.get();
        self.typing.set(wants);
        if started {
            window.take_focus();
        }
        window.set_text_caret(caret);
    }

    /// Gives a keystroke that came by `path` to the interface, unless the
    /// same one came the other way since the last frame: then it was taken
    /// already, and the answer it got then is given again.
    fn deliver_typed(&self, window: &WindowCx<'_>, path: Path, typed: Typed) -> Handled {
        let earlier = {
            let mut recent = self.recent.borrow_mut();
            let found = recent
                .iter()
                .position(|&(from, seen, _)| from != path && seen == typed);
            found.map(|at| recent.remove(at).2)
        };
        if let Some(handled) = earlier {
            return handled;
        }
        let event = match typed {
            Typed::Key(key) => ui::Event::Key(key),
            Typed::Text(c) => ui::Event::Text(c),
        };
        let handled = self.deliver(window, event);
        let mut recent = self.recent.borrow_mut();
        if recent.len() >= RECENT {
            recent.remove(0);
        }
        recent.push((path, typed, handled));
        handled
    }

    /// The window's size or scale changed: a frame now, unless the editor
    /// is hidden, which does no work, or a caller holds frames back. Then
    /// the frame is put off until the editor is shown or let go.
    fn refresh(&self, window: &WindowCx<'_>) {
        if self.hold.get() || !window.is_visible() {
            self.stale.set(true);
        } else {
            self.frame(window);
        }
    }

    /// Shows `rects` of the root's back buffer, as the system asked.
    fn repaint(&self, window: &WindowCx<'_>, rects: &[IRect]) {
        let Ok(slot) = self.root.try_borrow() else {
            return;
        };
        if let Some(root) = slot.as_ref() {
            let _ = window.present(root.pixels(), rects);
        }
    }

    /// Gives `event` to the root. Not used if there is no root, or if it is
    /// busy (it never is: the window does not re-enter its handler, and
    /// wrappers never call an editor that is calling out).
    pub(crate) fn deliver(&self, window: &WindowCx<'_>, event: ui::Event) -> Handled {
        let handled = self.guarded(window, || {
            let Ok(mut slot) = self.root.try_borrow_mut() else {
                return Handled::No;
            };
            match slot.as_mut().map(|root| root.event(event)) {
                Some(ui::Handled::Yes) => Handled::Yes,
                _ => Handled::No,
            }
        });
        // A move neither starts nor ends typing or a menu, and comes often:
        // asking the interface costs a description of the focused widget.
        if !matches!(event, ui::Event::PointerMove { .. }) {
            self.follow_typing(window);
        }
        handled.unwrap_or(Handled::No)
    }

    /// The window is gone or the interface failed: the interface is
    /// dropped, which ends its gestures, and the host is told once.
    fn give_up(&self) {
        let root = self
            .root
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take());
        // A widget that panics as it is dropped does not stop the host
        // from being told; the wrapper's link ends what it leaves open.
        let _ = panic::catch_unwind(AssertUnwindSafe(move || drop(root)));
        if !self.reported_closed.replace(true) {
            self.context.closed();
        }
    }

    /// The system's scale changed and the host gave none: the editor asks
    /// for its size at the new scale.
    fn follow_system_scale(&self, window: &WindowCx<'_>, old: f64, scale: f64) {
        // An application's window takes the size the system suggests for
        // its new monitor by itself.
        if self.app || self.host_scale.get().is_some() || self.is_closed() {
            return;
        }
        let current = host_size(window.size());
        let wanted = size_at(self.base, self.rules, current, old, scale);
        if wanted == current {
            return;
        }
        if let Some(granted) = self.context.request_resize(wanted) {
            let _ = window.resize(physical(cap(granted)));
        }
    }
}

/// Where the caret of what is being typed into is, in the window's device
/// pixels.
fn caret_in_device(root: &Root) -> Option<IRect> {
    let caret = root.caret()?;
    Some(caret.to_device(root.scale())).filter(|rect| !rect.is_empty())
}

/// What the window calls with every event.
pub(crate) struct Glue {
    shared: Rc<Shared>,
}

impl Glue {
    pub(crate) fn new(shared: &Rc<Shared>) -> Glue {
        Glue {
            shared: Rc::clone(shared),
        }
    }
}

/// A window event as the interface receives it, if it has a meaning there.
fn to_ui(event: Event) -> Option<ui::Event> {
    let button = |button: manguebyte_window::Button| match button {
        manguebyte_window::Button::Secondary => ui::Button::Secondary,
        manguebyte_window::Button::Middle => ui::Button::Middle,
        _ => ui::Button::Primary,
    };
    Some(match event {
        Event::PointerDown {
            pointer,
            button: pressed,
            clicks,
        } => ui::Event::PointerDown {
            at: pointer.position,
            button: button(pressed),
            clicks,
            modifiers: pointer.modifiers,
        },
        Event::PointerMove(pointer) => ui::Event::PointerMove {
            at: pointer.position,
            modifiers: pointer.modifiers,
        },
        Event::PointerUp {
            pointer,
            button: released,
        } => ui::Event::PointerUp {
            at: pointer.position,
            button: button(released),
            modifiers: pointer.modifiers,
        },
        Event::PointerCancel => ui::Event::PointerCancel,
        Event::PointerLeave => ui::Event::PointerLeave,
        Event::Wheel { pointer, delta } => ui::Event::Wheel {
            at: pointer.position,
            delta,
            modifiers: pointer.modifiers,
        },
        Event::Key { key, .. } => ui::Event::Key(key),
        Event::Text(c) => ui::Event::Text(c),
        Event::Focus(gained) => ui::Event::Focus(gained),
        // Everything else means nothing to the interface.
        _ => return None,
    })
}

impl WindowHandler for Glue {
    fn event(&mut self, window: &WindowCx<'_>, event: Event) -> Handled {
        let shared = &self.shared;
        match event {
            Event::Paint(damage) => {
                // Brought up to date first, so that what is shown is what
                // the interface is now; then what the system asked for.
                shared.frame(window);
                shared.repaint(window, damage.rects());
                Handled::Yes
            }
            Event::Timer(FRAME_TIMER) => {
                shared.frame(window);
                Handled::Yes
            }
            Event::Resized(_) => {
                shared.refresh(window);
                Handled::Yes
            }
            Event::ScaleChanged(scale) => {
                let old = shared.scale_seen.replace(scale);
                shared.follow_system_scale(window, old, scale);
                shared.refresh(window);
                Handled::Yes
            }
            Event::Closed => {
                shared.give_up();
                Handled::Yes
            }
            Event::CloseRequested => {
                shared.close_requested(window);
                Handled::Yes
            }
            Event::DragEnter(_) | Event::DragOver(_) | Event::DragLeave | Event::Drop(_) => {
                shared.file_drop(window, event)
            }
            Event::Key { key, .. } => shared.deliver_typed(window, Path::Window, Typed::Key(key)),
            Event::Text(c) => shared.deliver_typed(window, Path::Window, Typed::Text(c)),
            other => match to_ui(other) {
                Some(event) => shared.deliver(window, event),
                None => Handled::No,
            },
        }
    }
}

/// An editor inside the host's window.
pub(crate) struct Opened {
    pub(crate) shared: Rc<Shared>,
    pub(crate) window: Window,
}

impl Opened {
    /// The rules at the scale in use, capped at what the interface can
    /// draw.
    fn rules(&self) -> Resize {
        capped(self.shared.rules.scaled(self.window.scale()))
    }

    /// Tells the host, once, if the window's handler failed outside the
    /// glue's own guard (the glue catches a failing interface itself).
    fn check(&self) {
        if self.window.is_poisoned() {
            self.shared.give_up();
        }
    }

    /// Runs the frame that was put off while frames were held or the
    /// editor hidden, if there is one and the editor is visible.
    fn catch_up(&self) {
        if self.shared.stale.get() && self.window.is_visible() {
            self.shared.frame(&self.window.cx());
        }
    }
}

impl OpenEditor for Opened {
    fn size(&self) -> HostSize {
        host_size(self.window.size())
    }

    fn fit(&self, wanted: HostSize) -> HostSize {
        self.rules().fit(self.size(), wanted)
    }

    fn set_size(&mut self, size: HostSize) -> bool {
        self.check();
        let current = self.size();
        if size == current {
            return true;
        }
        if self.rules().fit(current, size) != size {
            return false;
        }
        self.window.resize(physical(size)).is_ok() && self.size() == size
    }

    fn set_scale(&mut self, scale: f64) -> bool {
        self.check();
        let Some(scale) = checked_scale(scale) else {
            return false;
        };
        let shared = &self.shared;
        let current = self.size();
        let wanted = size_at(
            shared.base,
            shared.rules,
            current,
            self.window.scale(),
            scale,
        );
        // The new scale and the size the host grants for it are drawn once,
        // together, at the end: no frame at the old size and the new scale.
        shared.hold.set(true);
        let before = shared.host_scale.replace(Some(scale));
        let taken = self.window.set_host_scale(Some(scale)).is_ok();
        if !taken {
            shared.host_scale.set(before);
        } else if wanted != current && !shared.is_closed() {
            // An editor that gave up asks the host for nothing.
            if let Some(granted) = shared.context.request_resize(wanted) {
                let _ = self.window.resize(physical(cap(granted)));
            }
        }
        shared.hold.set(false);
        self.catch_up();
        taken
    }

    fn set_visible(&mut self, visible: bool) {
        self.check();
        if self.window.set_visible(visible).is_err() {
            return;
        }
        if !visible {
            // A hidden editor does no work.
            self.window.kill_timer(FRAME_TIMER);
        } else if !self.shared.is_closed() {
            let _ = self.window.set_timer(FRAME_TIMER, FRAME_PERIOD_MS);
            // What changed while it was hidden is shown at once.
            self.catch_up();
        }
    }

    /// A key the host delivered through the plugin interface, as VST 3
    /// hosts may (`onKeyDown`, `onKeyUp`). It goes to the interface as a
    /// key; and while the interface takes typed text, the character it
    /// types follows as typed text, since no character message follows a
    /// key that came this way. A key that also reaches the editor's own
    /// window before the next frame, as some hosts send both, is taken
    /// once.
    fn key(&mut self, event: KeyEvent) -> bool {
        self.check();
        let window = self.window.cx();
        let shared = &self.shared;
        let mut used = shared.deliver_typed(&window, Path::Host, Typed::Key(event)) == Handled::Yes;
        let wants = shared.read_root(Root::wants_text).unwrap_or(false);
        if let Some(c) = typed_by(event).filter(|_| wants) {
            used |= shared.deliver_typed(&window, Path::Host, Typed::Text(c)) == Handled::Yes;
        }
        used
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        self.window.kill_timer(FRAME_TIMER);
        // The interface first, while the window and the host are still
        // there: it ends any gesture a control holds.
        let root = self
            .shared
            .root
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take());
        drop(root);
        // The window goes with the fields, and its handler with it.
    }
}

impl fmt::Debug for Opened {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

/// Builds the interface of `view` for `window`, on `context`, in `theme`,
/// copying to and pasting from the system's clipboard through the window.
pub(crate) fn attach_root(
    shared: &Shared,
    window: &Window,
    view: ui::View,
    theme: ui::Theme,
    context: EditorContext,
) {
    let mut root = Root::new(view, window.logical_size(), window.scale() as f32, context)
        .with_theme(theme)
        .with_clipboard(Rc::new(WindowClipboard(window.clipboard())));
    // People who asked the system for less motion get none: transitions
    // jump. A headless window, on a test's clock, keeps the theme's, so
    // that what it draws does not depend on the machine.
    if matches!(shared.clock, Clock::Real(_)) {
        root.set_system_motion(manguebyte_window::system_motion());
    }
    *shared.root.borrow_mut() = Some(root);
}

/// Opens `spec` inside `parent` with `backend`.
pub(crate) fn open_with(
    spec: &Editor,
    backend: Backend,
    parent: ParentWindow,
    scale: Option<f64>,
    context: EditorContext,
) -> Result<Box<dyn OpenEditor>, EditorError> {
    open_window(spec, backend, parent, scale, context).map(|opened| Box::new(opened) as _)
}

/// Opens `spec` inside `parent` with `backend`, as [`open_with`] does, and
/// returns the editor as it is.
pub(crate) fn open_window(
    spec: &Editor,
    backend: Backend,
    parent: ParentWindow,
    scale: Option<f64>,
    context: EditorContext,
) -> Result<Opened, EditorError> {
    if !backend.supports(parent.api()) {
        return Err(EditorError::Unsupported(parent.api()));
    }
    // Wrappers give only scales `checked_scale` accepts; anything else is
    // taken as no scale at all.
    let scale = scale.and_then(checked_scale);
    let size = cap(spec.size.scale_by(scale.unwrap_or(1.0)));
    let clock = match backend {
        Backend::Native => Clock::Real(Instant::now()),
        Backend::Headless => Clock::Manual(Cell::new(Duration::ZERO)),
    };
    // Until it opens, nothing is drawn: the first frame timer tick draws
    // everything.
    let shared = Shared::new(context.clone(), spec.size, spec.resize, scale, clock, false);
    // Hidden, and with no frame timer, until the host shows it: CLAP's
    // `show`, or the VST 3 wrapper once `attached` succeeded. Both reach the
    // editor as `set_visible(true)`, which starts the timer.
    let mut options = WindowOptions::new(physical(size)).hidden();
    if let Some(scale) = scale {
        options = options.with_scale(scale);
    }
    if spec.accepts_drops {
        options = options.with_file_drops();
    }
    let glue = Glue::new(&shared);
    #[cfg_attr(not(feature = "headless"), allow(unused_variables))]
    let (window, headless) = match backend {
        Backend::Native => (
            Window::open(parent, options, glue).map_err(editor_error)?,
            None,
        ),
        Backend::Headless => {
            let (window, headless) = Window::headless(options, glue).map_err(editor_error)?;
            (window, Some(headless))
        }
    };
    attach_root(&shared, &window, (spec.view)(), spec.theme, context);

    // An editor given no scale uses the system's, and asks for its size at
    // that scale.
    let system = window.scale();
    shared.scale_seen.set(system);
    if scale.is_none() && system != 1.0 {
        let current = host_size(window.size());
        let wanted = size_at(spec.size, spec.resize, current, 1.0, system);
        if wanted != current {
            if let Some(granted) = shared.context.request_resize(wanted) {
                let _ = window.resize(physical(cap(granted)));
            }
        }
    }
    shared.hold.set(false);
    let opened = Opened { shared, window };
    #[cfg(feature = "headless")]
    if let (Some(headless), Some(hook)) = (headless, &spec.on_headless) {
        hook(crate::HeadlessEditor::new(
            headless,
            Rc::downgrade(&opened.shared),
        ));
    }
    Ok(opened)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fixed_editor_scales_its_own_size() {
        let base = HostSize::new(300, 200);
        assert_eq!(
            size_at(base, Resize::Fixed, HostSize::new(7, 7), 1.0, 1.5),
            HostSize::new(450, 300)
        );
    }

    #[test]
    fn a_resizable_editor_keeps_its_logical_size_within_its_rules() {
        let base = HostSize::new(300, 200);
        let rules = Resize::Free {
            min: HostSize::new(200, 100),
            max: HostSize::new(600, 400),
        };
        // 400 by 300 at 1 is 800 by 600 at 2, and the maximum at 2 is
        // 1200 by 800.
        assert_eq!(
            size_at(base, rules, HostSize::new(400, 300), 1.0, 2.0),
            HostSize::new(800, 600)
        );
        // 900 by 150 at 1.5 is 600 by 100: at 1 that is the corner of the
        // limits.
        assert_eq!(
            size_at(base, rules, HostSize::new(900, 150), 1.5, 1.0),
            HostSize::new(600, 100)
        );
        // Beyond the maximum, the maximum.
        assert_eq!(
            size_at(base, rules, HostSize::new(1000, 1000), 1.0, 1.0),
            HostSize::new(600, 400)
        );
    }

    #[test]
    fn window_errors_become_editor_errors() {
        assert_eq!(
            editor_error(WindowError::Unsupported(WindowApi::X11)),
            EditorError::Unsupported(WindowApi::X11)
        );
        assert_eq!(
            editor_error(WindowError::Closed),
            EditorError::Platform("the window was destroyed".into())
        );
    }

    #[test]
    fn the_native_backend_takes_win32_parents_on_windows_only() {
        assert_eq!(Backend::Native.supports(WindowApi::Win32), cfg!(windows));
        assert!(!Backend::Native.supports(WindowApi::Cocoa));
        assert!(!Backend::Native.supports(WindowApi::X11));
        for api in [WindowApi::Win32, WindowApi::Cocoa, WindowApi::X11] {
            assert!(Backend::Headless.supports(api));
        }
    }
}
