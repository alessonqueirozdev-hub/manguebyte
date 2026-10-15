//! A window with no system window behind it, for tests on every system.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::{Rc, Weak};

use manguebyte_gfx::{IRect, Pixmap};
use manguebyte_plugin::editor::checked_scale;

use crate::damage::Damage;
use crate::dispatch::Origin;
use crate::event::{Event, Handled, PhysicalSize, RawInput, TimerId};
use crate::window::Inner;

/// The headless backend's side of a window: a record of what the window
/// asked the system for.
pub(crate) struct HeadlessPlatform {
    captured: Cell<bool>,
    visible: Cell<bool>,
    destroyed: Cell<bool>,
    focus_requests: Cell<u32>,
    leave_requests: Cell<u32>,
    blits: RefCell<Vec<IRect>>,
    invalidations: RefCell<Vec<IRect>>,
    /// The events the handler declined that came with a system message.
    #[cfg(test)]
    passed_on: RefCell<Vec<(Origin, Event)>>,
}

impl HeadlessPlatform {
    pub(crate) fn new(visible: bool) -> Self {
        HeadlessPlatform {
            captured: Cell::new(false),
            visible: Cell::new(visible),
            destroyed: Cell::new(false),
            focus_requests: Cell::new(0),
            leave_requests: Cell::new(0),
            blits: RefCell::new(Vec::new()),
            invalidations: RefCell::new(Vec::new()),
            #[cfg(test)]
            passed_on: RefCell::new(Vec::new()),
        }
    }

    pub(crate) fn blit(&self, rect: IRect) {
        self.blits.borrow_mut().push(rect);
    }

    pub(crate) fn invalidate(&self, rect: IRect) {
        self.invalidations.borrow_mut().push(rect);
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        self.visible.set(visible);
    }

    pub(crate) fn capture(&self, on: bool) {
        self.captured.set(on);
    }

    pub(crate) fn focus_requested(&self) {
        self.focus_requests
            .set(self.focus_requests.get().saturating_add(1));
    }

    pub(crate) fn track_leave(&self) {
        self.leave_requests
            .set(self.leave_requests.get().saturating_add(1));
    }

    /// A headless window has no host to pass anything on to; the crate's
    /// tests read what would have gone.
    pub(crate) fn answered(&self, origin: Origin, event: &Event, handled: Handled) {
        #[cfg(test)]
        if handled == Handled::No {
            self.passed_on.borrow_mut().push((origin, *event));
        }
        #[cfg(not(test))]
        let _ = (origin, event, handled);
    }

    /// What would have gone on to the host since the last call.
    #[cfg(test)]
    pub(crate) fn take_passed_on(&self) -> Vec<(Origin, Event)> {
        self.passed_on.take()
    }

    pub(crate) fn destroy(&self) {
        self.destroyed.set(true);
        self.captured.set(false);
    }
}

/// The test's side of a headless window, from [`Window::headless`].
///
/// It plays the system: it gives the window input, asks for its contents,
/// resizes it as a host would, changes the system's scale, fires its
/// timers and destroys its parent, and it reads back what the window shows.
/// Everything it does reaches the window's handler exactly as the same
/// thing would from a system window.
///
/// The controller holds the window weakly, so a handler may keep a clone
/// of it without making a cycle: once the [`Window`] is dropped, every
/// method does nothing and the readers return `None` or nothing.
///
/// ```
/// use std::cell::RefCell;
/// use std::rc::Rc;
/// use manguebyte_gfx::Point;
/// use manguebyte_window::{
///     Button, Event, Handled, Modifiers, PhysicalPoint, PhysicalSize, RawInput, Window,
///     WindowOptions,
/// };
///
/// let seen = Rc::new(RefCell::new(Vec::new()));
/// let log = Rc::clone(&seen);
/// let options = WindowOptions::new(PhysicalSize::new(300, 150)).with_scale(1.5);
/// let (window, headless) = Window::headless(options, move |_: &manguebyte_window::WindowCx<'_>, event| {
///     log.borrow_mut().push(event);
///     Handled::Yes
/// })
/// .expect("valid options");
///
/// headless.input(RawInput::PointerDown {
///     at: PhysicalPoint::new(15, 30),
///     button: Button::Primary,
///     modifiers: Modifiers::NONE,
///     time_ms: 0,
/// });
/// // The press took the keyboard, then arrived in logical pixels.
/// let seen = seen.borrow();
/// assert_eq!(seen[0], Event::Focus(true));
/// let Event::PointerDown { pointer, clicks, .. } = seen[1] else { panic!("{:?}", seen[1]) };
/// assert_eq!(pointer.position, Point::new(10.0, 20.0));
/// assert_eq!(clicks, 1);
/// assert!(headless.has_capture());
/// # drop(window);
/// ```
///
/// [`Window::headless`]: crate::Window::headless
/// [`Window`]: crate::Window
#[derive(Clone)]
pub struct Headless {
    inner: Weak<Inner>,
}

impl Headless {
    pub(crate) fn new(inner: Weak<Inner>) -> Self {
        Headless { inner }
    }

    fn with<T>(&self, f: impl FnOnce(&Inner) -> T) -> Option<T> {
        let inner: Rc<Inner> = self.inner.upgrade()?;
        Some(f(&inner))
    }

    fn platform<T>(&self, f: impl FnOnce(&HeadlessPlatform) -> T) -> Option<T> {
        self.with(|inner| match &inner.platform {
            crate::window::Platform::Headless(platform) => Some(f(platform)),
            #[cfg(windows)]
            _ => None,
        })
        .flatten()
    }

    /// Gives the window input, as the system would. Returns what the
    /// handler said about the last event the input made, or `None` if it
    /// made none, the event waited for a handler that is running, or the
    /// window is gone.
    pub fn input(&self, input: RawInput) -> Option<Handled> {
        self.with(|inner| inner.input(input)).flatten()
    }

    /// Asks for the window's contents, as an expose does: the handler gets
    /// an [`Event::Paint`](crate::Event::Paint) with the damage that
    /// waits, if any. Returns whether there was damage to hand over; a
    /// window that was destroyed has none.
    pub fn expose(&self) -> bool {
        self.with(Inner::paint_request).unwrap_or(false)
    }

    /// The damage that waits for the next expose.
    pub fn pending_damage(&self) -> Damage {
        self.with(|inner| inner.pending()).unwrap_or_default()
    }

    /// Resizes the window as a host does to its child.
    pub fn resize(&self, size: PhysicalSize) {
        self.with(|inner| inner.on_size(size));
    }

    /// Changes the system's scale for the window, as moving it to another
    /// monitor does. A scale outside `0.25..=8.0` is ignored, as the Win32
    /// backend ignores a DPI that gives one. Returns whether it was taken:
    /// a window that is closed or gone takes none.
    pub fn set_system_scale(&self, scale: f64) -> bool {
        if checked_scale(scale).is_none() {
            return false;
        }
        self.with(|inner| inner.on_system_scale(scale))
            .unwrap_or(false)
    }

    /// Destroys the window as a host does by destroying its parent: the
    /// handler gets [`Event::Closed`](crate::Event::Closed).
    pub fn destroy_parent(&self) {
        self.platform(HeadlessPlatform::destroy);
        self.with(Inner::on_destroyed);
    }

    /// Fires each running timer once, in the order of their ids, and
    /// returns how many fired.
    pub fn fire_timers(&self) -> usize {
        let mut ids: Vec<TimerId> = self.timers().into_iter().map(|(id, _)| id).collect();
        ids.sort();
        let mut fired = 0;
        for id in ids {
            if self.input(RawInput::Timer(id)).is_some() {
                fired += 1;
            }
        }
        fired
    }

    /// The running timers and their periods in milliseconds, in the order
    /// they were first set.
    pub fn timers(&self) -> Vec<(TimerId, u32)> {
        self.with(|inner| inner.timers()).unwrap_or_default()
    }

    /// What the window shows: the back buffer as premultiplied RGBA, or
    /// `None` once the window is gone. Pixels never presented are
    /// transparent black.
    pub fn frame(&self) -> Option<Pixmap> {
        self.with(|inner| inner.back().borrow().to_pixmap())
    }

    /// The rectangles copied to the "screen" since the last call, in order.
    pub fn take_blits(&self) -> Vec<IRect> {
        self.platform(|p| p.blits.take()).unwrap_or_default()
    }

    /// The rectangles the window asked the system to repaint since the
    /// last call, in order.
    pub fn take_invalidations(&self) -> Vec<IRect> {
        self.platform(|p| p.invalidations.take())
            .unwrap_or_default()
    }

    /// Whether the window holds the pointer, as the system sees it.
    pub fn has_capture(&self) -> bool {
        self.platform(|p| p.captured.get()).unwrap_or(false)
    }

    /// How often the window asked for the keyboard.
    pub fn focus_requests(&self) -> u32 {
        self.platform(|p| p.focus_requests.get()).unwrap_or(0)
    }

    /// How often the window asked to hear when the pointer leaves.
    pub fn leave_requests(&self) -> u32 {
        self.platform(|p| p.leave_requests.get()).unwrap_or(0)
    }

    /// Whether the window is shown, as the system sees it.
    pub fn is_visible(&self) -> bool {
        self.platform(|p| p.visible.get() && !p.destroyed.get())
            .unwrap_or(false)
    }

    /// Whether the window's value still exists.
    pub fn exists(&self) -> bool {
        self.inner.strong_count() > 0
    }

    /// Whether the handler is running now: true when called from inside
    /// it.
    pub fn handler_running(&self) -> bool {
        self.with(|inner| inner.dispatcher().busy())
            .unwrap_or(false)
    }
}

impl fmt::Debug for Headless {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Headless")
            .field("exists", &self.exists())
            .finish_non_exhaustive()
    }
}
