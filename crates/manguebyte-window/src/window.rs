//! The window value, what it shares with its backend, and the logic both
//! backends run.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use manguebyte_gfx::{IRect, MAX_SIDE, Pixmap, Point, Size};
use manguebyte_plugin::editor::{ParentWindow, WindowApi, checked_scale};

use crate::convert::{self, Utf16Joiner};
use crate::damage::Damage;
use crate::dispatch::{Dispatcher, Origin, WindowHandler, drop_quietly};
use crate::event::{Event, Handled, PhysicalPoint, PhysicalSize, PointerEvent, RawInput, TimerId};
use crate::headless::{Headless, HeadlessPlatform};
use crate::pixels::BackBuffer;
use crate::pointer::{ClickSettings, PointerState, Up};

/// The requests a window and its handler's view of it share.
macro_rules! window_methods {
    () => {
        /// Shows the pixels of `damage` from `pixels`, a premultiplied RGBA
        /// pixmap whose top-left pixel is the window's. Only those
        /// rectangles, cut to the pixmap and the window, are converted and
        /// copied; more than [`MAX_RECTS`](crate::MAX_RECTS) are merged
        /// first. GDI ignores alpha, so a pixel shows as its premultiplied
        /// colour over black.
        ///
        /// # Errors
        ///
        /// [`WindowError::Closed`] after the window was destroyed by
        /// someone else.
        pub fn present(&self, pixels: &Pixmap, damage: &[IRect]) -> Result<(), WindowError> {
            self.inner().present(pixels, damage)
        }

        /// Shows the whole of `pixels`.
        ///
        /// # Errors
        ///
        /// [`WindowError::Closed`] after the window was destroyed.
        pub fn present_all(&self, pixels: &Pixmap) -> Result<(), WindowError> {
            self.inner().present(pixels, &[pixels.bounds()])
        }

        /// Marks `rect` (device pixels) as needing drawing: the handler
        /// gets it in an [`Event::Paint`] when the system next asks for the
        /// window's contents. Nothing happens outside the window or after
        /// it was destroyed.
        pub fn invalidate(&self, rect: IRect) {
            if self.inner().is_open() {
                self.inner().damage(rect);
            }
        }

        /// Marks the whole window as needing drawing.
        pub fn invalidate_all(&self) {
            self.invalidate(self.size().bounds());
        }

        /// Asks for the window to become `size`. The handler is told with
        /// an [`Event::Resized`], after this returns when asked from inside
        /// the handler.
        ///
        /// # Errors
        ///
        /// [`WindowError::InvalidSize`] for a side above
        /// [`MAX_SIDE`](manguebyte_gfx::MAX_SIDE), [`WindowError::Closed`],
        /// or [`WindowError::Platform`].
        pub fn resize(&self, size: PhysicalSize) -> Result<(), WindowError> {
            self.inner().resize(size)
        }

        /// The size in device pixels.
        pub fn size(&self) -> PhysicalSize {
            self.inner().size()
        }

        /// The size in logical pixels: the device size divided by the
        /// scale.
        pub fn logical_size(&self) -> Size {
            convert::logical_size(self.size(), self.scale())
        }

        /// The scale: the host's if it gave one, otherwise the system's.
        /// Device pixels are logical pixels times this.
        pub fn scale(&self) -> f64 {
            self.inner().scale()
        }

        /// Sets or clears the host's scale. The handler is told with an
        /// [`Event::ScaleChanged`] if the scale in use changes.
        ///
        /// # Errors
        ///
        /// [`WindowError::InvalidScale`] or [`WindowError::Closed`].
        pub fn set_host_scale(&self, scale: Option<f64>) -> Result<(), WindowError> {
            self.inner().set_host_scale(scale)
        }

        /// A device position in logical pixels.
        pub fn to_logical(&self, at: PhysicalPoint) -> Point {
            convert::to_logical(at, self.scale())
        }

        /// Starts or restarts the timer `id`, firing about every
        /// `period_ms` milliseconds as [`Event::Timer`] until it is killed
        /// or the window goes. Win32 fires no faster than every 10 ms.
        ///
        /// # Errors
        ///
        /// [`WindowError::InvalidPeriod`] for 0, [`WindowError::Closed`],
        /// or [`WindowError::Platform`].
        pub fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
            self.inner().set_timer(id, period_ms)
        }

        /// Stops the timer `id`. Returns whether it was running. A tick the
        /// system had already queued is not delivered.
        pub fn kill_timer(&self, id: TimerId) -> bool {
            self.inner().kill_timer(id)
        }

        /// Shows or hides the window.
        ///
        /// # Errors
        ///
        /// [`WindowError::Closed`].
        pub fn set_visible(&self, visible: bool) -> Result<(), WindowError> {
            self.inner().set_visible(visible)
        }

        /// Whether the window was last asked to be visible.
        pub fn is_visible(&self) -> bool {
            self.inner().visible.get()
        }

        /// Whether the window still exists: false once someone else
        /// destroyed it.
        pub fn is_open(&self) -> bool {
            self.inner().is_open()
        }

        /// Whether the window has the keyboard.
        pub fn has_focus(&self) -> bool {
            self.inner().focused.get()
        }

        /// Whether the window holds the pointer for a drag.
        pub fn has_capture(&self) -> bool {
            self.inner().pointer.borrow().capturing()
        }

        /// The system's handle (the `HWND` on Win32), or `None` for a
        /// headless window or one that was destroyed.
        pub fn raw_handle(&self) -> Option<usize> {
            self.inner().platform.handle()
        }

        /// How many pixels presenting has converted since the window was
        /// made: a measure of the work presents did.
        pub fn presented_pixels(&self) -> u64 {
            self.inner().back().borrow().converted()
        }

        /// How many events arrived while the handler was running and had
        /// to wait for it (merged repeats and dropped events included).
        pub fn deferred_events(&self) -> u64 {
            self.inner().dispatcher().deferred()
        }

        /// How many events were dropped because more than
        /// [`MAX_QUEUED`](crate::MAX_QUEUED) waited for a handler that ran
        /// for a long time.
        pub fn dropped_events(&self) -> u64 {
            self.inner().dispatcher().dropped()
        }

        /// Asks for every key while the window has the keyboard, Tab,
        /// Enter and Escape included, as a text field being edited needs.
        /// Off by default: a host that runs a dialog then keeps those keys
        /// for its own controls (on Win32, the answer to
        /// `WM_GETDLGCODE`). Arrow keys and typed characters are always
        /// asked for.
        pub fn set_wants_all_keys(&self, wants: bool) {
            self.inner().wants_all_keys.set(wants);
        }

        /// Whether the window asks for every key; see
        /// [`set_wants_all_keys`](Self::set_wants_all_keys).
        pub fn wants_all_keys(&self) -> bool {
            self.inner().wants_all_keys.get()
        }
    };
}

/// How to make a window.
///
/// ```
/// use manguebyte_window::{PhysicalSize, WindowOptions};
///
/// let options = WindowOptions::new(PhysicalSize::new(640, 360)).with_scale(1.5);
/// assert_eq!(options.scale, Some(1.5));
/// assert!(options.visible);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct WindowOptions {
    /// The size in device pixels. Each side may be at most
    /// [`MAX_SIDE`](manguebyte_gfx::MAX_SIDE).
    pub size: PhysicalSize,
    /// The scale the host gave, if it gave one (CLAP `set_scale`, VST 3
    /// `setContentScaleFactor`). Without one the window uses the system's
    /// scale for its own position: on Windows, its DPI divided by 96.
    pub scale: Option<f64>,
    /// Whether the window is shown at once.
    pub visible: bool,
    /// Whether a press takes the keyboard for the window.
    pub focus_on_click: bool,
}

impl WindowOptions {
    /// A visible window of `size` that takes the keyboard when pressed and
    /// uses the system's scale.
    pub fn new(size: PhysicalSize) -> Self {
        WindowOptions {
            size,
            scale: None,
            visible: true,
            focus_on_click: true,
        }
    }

    /// The same with the host's scale.
    pub fn with_scale(mut self, scale: f64) -> Self {
        self.scale = Some(scale);
        self
    }

    /// The same, hidden until [`set_visible`](Window::set_visible).
    pub fn hidden(mut self) -> Self {
        self.visible = false;
        self
    }

    /// The same, but a press leaves the keyboard where it is.
    pub fn without_focus_on_click(mut self) -> Self {
        self.focus_on_click = false;
        self
    }
}

/// Why a window could not be made or could not do what was asked.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum WindowError {
    /// There is no backend for this kind of parent on this system.
    Unsupported(WindowApi),
    /// The parent is not a window this process may embed into: it does not
    /// exist, or it belongs to another process.
    InvalidParent(&'static str),
    /// A side is larger than [`MAX_SIDE`](manguebyte_gfx::MAX_SIDE).
    InvalidSize(PhysicalSize),
    /// A scale outside `0.25..=8.0`, or not a number.
    InvalidScale(f64),
    /// A timer period of zero.
    InvalidPeriod,
    /// The window was destroyed by someone else.
    Closed,
    /// The window is in the middle of the same operation: a request made
    /// from inside one that cannot be nested.
    Busy,
    /// The system refused.
    Platform {
        /// The system call that failed.
        call: &'static str,
        /// The system's error code.
        code: u32,
    },
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowError::Unsupported(api) => {
                write!(f, "no window backend for {api:?} parents on this system")
            }
            WindowError::InvalidParent(why) => write!(f, "invalid parent window: {why}"),
            WindowError::InvalidSize(size) => write!(
                f,
                "a {}x{} window is larger than {MAX_SIDE} pixels on a side",
                size.width, size.height
            ),
            WindowError::InvalidScale(scale) => write!(f, "{scale} is not a usable scale"),
            WindowError::InvalidPeriod => f.write_str("a timer period must be at least 1 ms"),
            WindowError::Closed => f.write_str("the window was destroyed"),
            WindowError::Busy => f.write_str("the window is busy with the same request"),
            WindowError::Platform { call, code } => write!(f, "{call} failed with error {code}"),
        }
    }
}

impl std::error::Error for WindowError {}

/// Where a window is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Life {
    Open,
    /// Destroyed by someone else; the value still exists.
    Closed,
    /// The value is being dropped.
    Dropped,
}

/// The backend under a window.
pub(crate) enum Platform {
    Headless(HeadlessPlatform),
    #[cfg(windows)]
    Win32(crate::win32::Win32Platform),
}

impl Platform {
    fn blit(&self, back: &BackBuffer, rect: IRect) {
        match self {
            Platform::Headless(p) => p.blit(rect),
            #[cfg(windows)]
            Platform::Win32(p) => p.blit(back, rect),
        }
        #[cfg(not(windows))]
        let _ = back;
    }

    fn invalidate(&self, rect: IRect) {
        match self {
            Platform::Headless(p) => p.invalidate(rect),
            #[cfg(windows)]
            Platform::Win32(p) => p.invalidate(rect),
        }
    }

    /// Asks for the size; the backend reports the new size through
    /// [`Inner::on_size`], possibly from inside this call, and returns the
    /// size the window has afterwards.
    fn set_size(&self, inner: &Inner, size: PhysicalSize) -> Result<PhysicalSize, WindowError> {
        match self {
            Platform::Headless(_) => {
                inner.on_size(size);
                Ok(size)
            }
            #[cfg(windows)]
            Platform::Win32(p) => p.set_size(size),
        }
    }

    fn set_visible(&self, visible: bool) {
        match self {
            Platform::Headless(p) => p.set_visible(visible),
            #[cfg(windows)]
            Platform::Win32(p) => p.set_visible(visible),
        }
    }

    fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
        match self {
            Platform::Headless(_) => {
                let _ = (id, period_ms);
                Ok(())
            }
            #[cfg(windows)]
            Platform::Win32(p) => p.set_timer(id, period_ms),
        }
    }

    fn kill_timer(&self, id: TimerId) {
        match self {
            Platform::Headless(_) => {
                let _ = id;
            }
            #[cfg(windows)]
            Platform::Win32(p) => p.kill_timer(id),
        }
    }

    fn capture(&self, on: bool) {
        match self {
            Platform::Headless(p) => p.capture(on),
            #[cfg(windows)]
            Platform::Win32(p) => p.capture(on),
        }
    }

    /// Takes the keyboard; the backend reports it through
    /// [`Inner::input`], possibly from inside this call.
    fn take_focus(&self, inner: &Inner) {
        match self {
            Platform::Headless(p) => {
                p.focus_requested();
                inner.input(RawInput::Focus(true));
            }
            #[cfg(windows)]
            Platform::Win32(p) => p.take_focus(),
        }
    }

    fn track_leave(&self) {
        match self {
            Platform::Headless(p) => p.track_leave(),
            #[cfg(windows)]
            Platform::Win32(p) => p.track_leave(),
        }
    }

    fn click_settings(&self) -> ClickSettings {
        match self {
            Platform::Headless(_) => ClickSettings::DEFAULT,
            #[cfg(windows)]
            Platform::Win32(p) => p.click_settings(),
        }
    }

    /// The handler answered an event that came from `origin`.
    fn answered(&self, origin: Origin, event: &Event, handled: Handled) {
        match self {
            Platform::Headless(p) => p.answered(origin, event, handled),
            #[cfg(windows)]
            Platform::Win32(p) => p.answered(origin, event, handled),
        }
    }

    fn destroy(&self) {
        match self {
            Platform::Headless(p) => p.destroy(),
            #[cfg(windows)]
            Platform::Win32(p) => p.destroy(),
        }
    }

    fn handle(&self) -> Option<usize> {
        match self {
            Platform::Headless(_) => None,
            #[cfg(windows)]
            Platform::Win32(p) => p.handle(),
        }
    }
}

/// What a window and its backend share.
pub(crate) struct Inner {
    pub(crate) platform: Platform,
    dispatcher: Dispatcher,
    life: Cell<Life>,
    size: Cell<PhysicalSize>,
    system_scale: Cell<f64>,
    host_scale: Cell<Option<f64>>,
    pending: Cell<Damage>,
    back: RefCell<BackBuffer>,
    timers: RefCell<Vec<(TimerId, u32)>>,
    pointer: RefCell<PointerState>,
    text: RefCell<Utf16Joiner>,
    focused: Cell<bool>,
    visible: Cell<bool>,
    focus_on_click: bool,
    pub(crate) wants_all_keys: Cell<bool>,
}

/// Checks the options a window is made with.
pub(crate) fn check_options(options: &WindowOptions) -> Result<(), WindowError> {
    if options.size.width > MAX_SIDE || options.size.height > MAX_SIDE {
        return Err(WindowError::InvalidSize(options.size));
    }
    if let Some(scale) = options.scale {
        checked_scale(scale).ok_or(WindowError::InvalidScale(scale))?;
    }
    Ok(())
}

impl Inner {
    /// A window's shared state, with the whole window damaged.
    pub(crate) fn new(
        platform: Platform,
        options: &WindowOptions,
        handler: Box<dyn WindowHandler>,
    ) -> Self {
        let mut back = BackBuffer::empty();
        back.resize(options.size);
        Inner {
            platform,
            dispatcher: Dispatcher::new(handler),
            life: Cell::new(Life::Open),
            size: Cell::new(options.size),
            system_scale: Cell::new(1.0),
            host_scale: Cell::new(options.scale),
            pending: Cell::new(Damage::of(options.size.bounds())),
            back: RefCell::new(back),
            timers: RefCell::new(Vec::new()),
            pointer: RefCell::new(PointerState::default()),
            text: RefCell::new(Utf16Joiner::default()),
            focused: Cell::new(false),
            visible: Cell::new(options.visible),
            focus_on_click: options.focus_on_click,
            wants_all_keys: Cell::new(false),
        }
    }

    fn cx(&self) -> WindowCx<'_> {
        WindowCx { inner: self }
    }

    pub(crate) fn is_open(&self) -> bool {
        self.life.get() == Life::Open
    }

    fn check_open(&self) -> Result<(), WindowError> {
        if self.is_open() {
            Ok(())
        } else {
            Err(WindowError::Closed)
        }
    }

    fn dispatch(&self, event: Event) -> Option<Handled> {
        self.dispatch_from(event, None)
    }

    /// Delivers an event that came from the system message `origin`, so
    /// that the platform hears the handler's answer to it.
    fn dispatch_from(&self, event: Event, origin: Option<Origin>) -> Option<Handled> {
        if !self.is_open() {
            return None;
        }
        self.dispatcher.dispatch(&self.cx(), event, origin)
    }

    pub(crate) fn back(&self) -> &RefCell<BackBuffer> {
        &self.back
    }

    pub(crate) fn size(&self) -> PhysicalSize {
        self.size.get()
    }

    pub(crate) fn scale(&self) -> f64 {
        self.host_scale.get().unwrap_or(self.system_scale.get())
    }

    pub(crate) fn pending(&self) -> Damage {
        self.pending.get()
    }

    pub(crate) fn timers(&self) -> Vec<(TimerId, u32)> {
        self.timers.borrow().clone()
    }

    pub(crate) fn dispatcher(&self) -> &Dispatcher {
        &self.dispatcher
    }

    fn damage(&self, rect: IRect) {
        let Some(rect) = rect.intersect(&self.size.get().bounds()) else {
            return;
        };
        let mut pending = self.pending.get();
        pending.add(rect);
        self.pending.set(pending);
        self.platform.invalidate(rect);
    }

    fn present(&self, pixels: &Pixmap, rects: &[IRect]) -> Result<(), WindowError> {
        self.check_open()?;
        let bounds = self.size.get().bounds();
        let mut damage = Damage::new();
        for rect in rects {
            damage.add_clipped(*rect, bounds);
        }
        let mut back = self.back.try_borrow_mut().map_err(|_| WindowError::Busy)?;
        for rect in damage.rects() {
            if let Some(done) = back.convert(pixels, *rect) {
                self.platform.blit(&back, done);
            }
        }
        Ok(())
    }

    fn resize(&self, size: PhysicalSize) -> Result<(), WindowError> {
        self.check_open()?;
        if size.width > MAX_SIDE || size.height > MAX_SIDE {
            return Err(WindowError::InvalidSize(size));
        }
        let actual = self.platform.set_size(self, size)?;
        self.on_size(actual);
        Ok(())
    }

    /// The window is now `size`, by its own request or the host's.
    pub(crate) fn on_size(&self, size: PhysicalSize) {
        if !self.is_open() {
            return;
        }
        let old = self.size.replace(size);
        if old == size {
            return;
        }
        if let Ok(mut back) = self.back.try_borrow_mut() {
            back.resize(size);
        }
        // What became visible: a strip on the right and one at the bottom.
        if size.width > old.width {
            self.damage(IRect::new(
                old.width as i32,
                0,
                size.width - old.width,
                size.height,
            ));
        }
        if size.height > old.height {
            self.damage(IRect::new(
                0,
                old.height as i32,
                size.width.min(old.width),
                size.height - old.height,
            ));
        }
        self.pending.set(self.pending.get().clipped(size.bounds()));
        self.dispatch(Event::Resized(size));
    }

    /// The system's scale when the window is made; no event.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn init_system_scale(&self, scale: f64) {
        if checked_scale(scale).is_some() {
            self.system_scale.set(scale);
        }
    }

    /// The system's scale for the window is now `scale`. Returns whether
    /// it was taken: not by a closed window, nor for an unusable scale.
    pub(crate) fn on_system_scale(&self, scale: f64) -> bool {
        if !self.is_open() || checked_scale(scale).is_none() {
            return false;
        }
        let before = self.scale();
        self.system_scale.set(scale);
        if self.scale() != before {
            self.scale_changed();
        }
        true
    }

    fn set_host_scale(&self, scale: Option<f64>) -> Result<(), WindowError> {
        self.check_open()?;
        if let Some(scale) = scale {
            checked_scale(scale).ok_or(WindowError::InvalidScale(scale))?;
        }
        let before = self.scale();
        self.host_scale.set(scale);
        if self.scale() != before {
            self.scale_changed();
        }
        Ok(())
    }

    fn scale_changed(&self) {
        self.damage(self.size.get().bounds());
        self.dispatch(Event::ScaleChanged(self.scale()));
    }

    /// The system asks for the window's contents: the handler gets the
    /// damage that waits, if any.
    /// Returns whether there was damage to hand over.
    pub(crate) fn paint_request(&self) -> bool {
        if !self.is_open() {
            return false;
        }
        let mut pending = self.pending.get();
        let damage = pending.take().clipped(self.size.get().bounds());
        self.pending.set(pending);
        if damage.is_empty() {
            return false;
        }
        self.dispatch(Event::Paint(damage));
        true
    }

    fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
        self.check_open()?;
        if period_ms == 0 {
            return Err(WindowError::InvalidPeriod);
        }
        self.platform.set_timer(id, period_ms)?;
        let mut timers = self.timers.borrow_mut();
        match timers.iter_mut().find(|(kept, _)| *kept == id) {
            Some(timer) => timer.1 = period_ms,
            None => timers.push((id, period_ms)),
        }
        Ok(())
    }

    fn kill_timer(&self, id: TimerId) -> bool {
        let removed = {
            let mut timers = self.timers.borrow_mut();
            let index = timers.iter().position(|(kept, _)| *kept == id);
            index.map(|index| timers.remove(index)).is_some()
        };
        if removed {
            self.platform.kill_timer(id);
        }
        removed
    }

    fn set_visible(&self, visible: bool) -> Result<(), WindowError> {
        self.check_open()?;
        self.platform.set_visible(visible);
        self.visible.set(visible);
        Ok(())
    }

    fn pointer_event(&self, at: PhysicalPoint, modifiers: crate::Modifiers) -> PointerEvent {
        PointerEvent {
            position: convert::to_logical(at, self.scale()),
            physical: at,
            modifiers,
        }
    }

    /// Turns raw input into events for the handler. Returns what the
    /// handler said about the last event it was given, or `None` if the
    /// input made no event or the event had to wait.
    pub(crate) fn input(&self, input: RawInput) -> Option<Handled> {
        self.input_from(input, None)
    }

    /// The same for input that came from the system message `origin`:
    /// the key, text and wheel events it makes carry it, so that one the
    /// handler does not use can go on to the host.
    pub(crate) fn input_from(&self, input: RawInput, origin: Option<Origin>) -> Option<Handled> {
        if !self.is_open() {
            return None;
        }
        match input {
            RawInput::PointerDown {
                at,
                button,
                modifiers,
                time_ms,
            } => {
                let settings = self.platform.click_settings();
                let down = self
                    .pointer
                    .borrow_mut()
                    .down(button, at, time_ms, settings);
                if self.focus_on_click && !self.focused.get() {
                    self.platform.take_focus(self);
                }
                if down.capture {
                    self.platform.capture(true);
                }
                if down.track_leave {
                    self.platform.track_leave();
                }
                self.dispatch(Event::PointerDown {
                    pointer: self.pointer_event(at, modifiers),
                    button,
                    clicks: down.clicks,
                })
            }
            RawInput::PointerMove { at, modifiers } => {
                let track_leave = self.pointer.borrow_mut().moved(at, modifiers)?;
                if track_leave {
                    self.platform.track_leave();
                }
                self.dispatch(Event::PointerMove(self.pointer_event(at, modifiers)))
            }
            RawInput::PointerUp {
                at,
                button,
                modifiers,
            } => {
                let up = self.pointer.borrow_mut().up(button, at, self.size.get());
                let Up::Report { release, left } = up else {
                    return None;
                };
                if release {
                    self.platform.capture(false);
                }
                let handled = self.dispatch(Event::PointerUp {
                    pointer: self.pointer_event(at, modifiers),
                    button,
                });
                if left {
                    self.dispatch(Event::PointerLeave);
                }
                handled
            }
            RawInput::PointerLeft => {
                let left = self.pointer.borrow_mut().left();
                if left {
                    self.dispatch(Event::PointerLeave)
                } else {
                    None
                }
            }
            RawInput::CaptureLost => {
                let cancelled = self.pointer.borrow_mut().capture_lost();
                // If the system still says the window holds the pointer
                // (the notice was not the system's own), let it go, so the
                // window and the system agree.
                self.platform.capture(false);
                if cancelled {
                    self.dispatch(Event::PointerCancel)
                } else {
                    None
                }
            }
            RawInput::Wheel {
                at,
                delta_x,
                delta_y,
                modifiers,
            } => {
                if delta_x == 0 && delta_y == 0 {
                    return None;
                }
                self.dispatch_from(
                    Event::Wheel {
                        pointer: self.pointer_event(at, modifiers),
                        delta: Point::new(
                            convert::wheel_lines(delta_x),
                            convert::wheel_lines(delta_y),
                        ),
                    },
                    origin,
                )
            }
            RawInput::Key { key, repeat } => self.dispatch_from(Event::Key { key, repeat }, origin),
            RawInput::Utf16(unit) => {
                let chars = self.text.borrow_mut().push(unit);
                let mut handled = None;
                for c in chars.into_iter().flatten() {
                    handled = self.dispatch_from(Event::Text(c), origin);
                }
                handled
            }
            RawInput::Focus(focused) => {
                if self.focused.replace(focused) == focused {
                    return None;
                }
                if !focused {
                    *self.text.borrow_mut() = Utf16Joiner::default();
                }
                self.dispatch(Event::Focus(focused))
            }
            RawInput::Timer(id) => {
                let known = self.timers.borrow().iter().any(|(kept, _)| *kept == id);
                if known {
                    self.dispatch(Event::Timer(id))
                } else {
                    None
                }
            }
        }
    }

    /// The window was destroyed under its value, or as its value drops.
    pub(crate) fn on_destroyed(&self) {
        if self.life.get() != Life::Open {
            return;
        }
        self.life.set(Life::Closed);
        self.timers.borrow_mut().clear();
        self.pointer.borrow_mut().reset();
        self.focused.set(false);
        self.dispatcher.dispatch(&self.cx(), Event::Closed, None);
    }
}

/// A platform window inside a host's window, or a headless one for tests.
///
/// It is made with [`Window::open`] as a child of the [`ParentWindow`] a
/// host gives, or with [`Window::headless`] for tests on any system. It
/// belongs to the thread that made it (the host's main thread in a plugin)
/// and cannot be sent to another. Its [`WindowHandler`] receives every
/// event; the window presents premultiplied RGBA pixmaps from a back
/// buffer of its own, so covered parts that become visible again are
/// redrawn without the handler.
///
/// Dropping the window destroys it and its timers, even if the host
/// destroyed its parent first. A window destroyed by someone else tells its
/// handler [`Event::Closed`] and refuses every request from then on.
///
/// ```
/// use manguebyte_gfx::{Color, IRect, Pixmap};
/// use manguebyte_window::{Event, Handled, PhysicalSize, Window, WindowOptions};
///
/// let size = PhysicalSize::new(4, 3);
/// let (window, headless) = Window::headless(WindowOptions::new(size), |window: &manguebyte_window::WindowCx<'_>, event| {
///     if let Event::Paint(damage) = event {
///         let mut frame = Pixmap::new(4, 3).expect("small");
///         frame.fill(Color::rgb(255, 0, 0));
///         window.present(&frame, damage.rects()).expect("open");
///     }
///     Handled::Yes
/// })
/// .expect("valid options");
///
/// // The system asks for the window's contents; the handler draws them.
/// headless.expose();
/// let shown = headless.frame().expect("the window exists");
/// assert_eq!(shown.pixel(3, 2).expect("inside").to_array(), [255, 0, 0, 255]);
/// drop(window);
/// assert!(headless.frame().is_none());
/// ```
///
/// A window stays on the thread that made it:
///
/// ```compile_fail
/// use manguebyte_window::{Handled, PhysicalSize, Window, WindowCx, WindowOptions};
///
/// fn elsewhere<T: Send>(_: T) {}
/// let (window, _headless) = Window::headless(
///     WindowOptions::new(PhysicalSize::new(1, 1)),
///     |_: &WindowCx<'_>, _| Handled::Yes,
/// )
/// .expect("valid options");
/// elsewhere(window);
/// ```
pub struct Window {
    inner: Rc<Inner>,
}

impl Window {
    /// A window inside `parent`, of the size and scale in `options`, whose
    /// events go to `handler`.
    ///
    /// # Errors
    ///
    /// [`WindowError::Unsupported`] where there is no backend for the
    /// parent's kind (in this version, anything but Win32 on Windows);
    /// [`WindowError::InvalidParent`] for a handle that is not a window of
    /// this process; [`WindowError::InvalidSize`] or
    /// [`WindowError::InvalidScale`] for bad options; or
    /// [`WindowError::Platform`] if the system refuses.
    pub fn open(
        parent: ParentWindow,
        options: WindowOptions,
        handler: impl WindowHandler + 'static,
    ) -> Result<Window, WindowError> {
        check_options(&options)?;
        match parent {
            #[cfg(windows)]
            ParentWindow::Win32 { hwnd } => {
                crate::win32::open(hwnd, &options, Box::new(handler)).map(|inner| Window { inner })
            }
            other => {
                drop(handler);
                Err(WindowError::Unsupported(other.api()))
            }
        }
    }

    /// A window with no system window behind it, and the controller a test
    /// drives it with: it injects input, exposes, resizes and destroys the
    /// parent, and reads what the window shows.
    ///
    /// The window runs the same code as a system window for everything but
    /// the system calls: damage, the back buffer, scale conversion, pointer
    /// capture, click counting, timers and the reentrancy queue.
    ///
    /// # Errors
    ///
    /// [`WindowError::InvalidSize`] or [`WindowError::InvalidScale`].
    pub fn headless(
        options: WindowOptions,
        handler: impl WindowHandler + 'static,
    ) -> Result<(Window, Headless), WindowError> {
        check_options(&options)?;
        let inner = Rc::new(Inner::new(
            Platform::Headless(HeadlessPlatform::new(options.visible)),
            &options,
            Box::new(handler),
        ));
        let headless = Headless::new(Rc::downgrade(&inner));
        Ok((Window { inner }, headless))
    }

    /// The window as its handler sees it.
    pub fn cx(&self) -> WindowCx<'_> {
        WindowCx { inner: &self.inner }
    }

    /// Whether the handler panicked. The window then delivers nothing more;
    /// its owner should drop it.
    pub fn is_poisoned(&self) -> bool {
        self.inner.dispatcher().poisoned()
    }

    fn inner(&self) -> &Inner {
        &self.inner
    }

    window_methods!();
}

impl Drop for Window {
    fn drop(&mut self) {
        let inner = &self.inner;
        inner.life.set(Life::Dropped);
        inner.timers.borrow_mut().clear();
        inner.platform.destroy();
        if let Some(handler) = inner.dispatcher().retire() {
            drop_quietly(handler);
        }
    }
}

impl fmt::Debug for Window {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.cx().fmt(f)
    }
}

/// A window as its handler sees it: everything a [`Window`] can do but
/// being dropped.
///
/// Requests that make the system send the window a message (a resize, a
/// timer, taking the keyboard) never call the handler from inside the
/// request; what they cause arrives after the handler returns.
#[derive(Clone, Copy)]
pub struct WindowCx<'a> {
    inner: &'a Inner,
}

impl WindowCx<'_> {
    fn inner(&self) -> &Inner {
        self.inner
    }

    /// The handler answered an event that came from `origin`: the
    /// platform passes it on to the host if it was not used.
    pub(crate) fn answered(&self, origin: Origin, event: &Event, handled: Handled) {
        self.inner.platform.answered(origin, event, handled);
    }

    /// The dispatcher, for the crate's own tests.
    #[cfg(test)]
    pub(crate) fn dispatcher_for_tests(&self) -> &Dispatcher {
        self.inner.dispatcher()
    }

    window_methods!();
}

impl fmt::Debug for WindowCx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Window")
            .field("open", &self.is_open())
            .field("size", &self.size())
            .field("scale", &self.scale())
            .field("handle", &self.raw_handle())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::event::{Key, KeyEvent, Modifiers};

    const WM_KEYDOWN: u32 = 0x0100;
    const WM_CHAR: u32 = 0x0102;

    fn key(c: char) -> RawInput {
        RawInput::Key {
            key: KeyEvent {
                key: Key::Character(c),
                modifiers: Modifiers::NONE,
                pressed: true,
            },
            repeat: false,
        }
    }

    fn origin(message: u32, wparam: usize) -> Option<Origin> {
        Some(Origin {
            message,
            wparam,
            lparam: 1,
        })
    }

    fn passed_on(window: &Window) -> Vec<(Origin, Event)> {
        match &window.inner.platform {
            Platform::Headless(p) => p.take_passed_on(),
            #[cfg(windows)]
            Platform::Win32(_) => unreachable!("headless"),
        }
    }

    /// A handler that uses the space key and nothing else, and that, while
    /// it handles a timer, receives two keys.
    fn space_only() -> Window {
        Window::headless(
            WindowOptions::new(PhysicalSize::new(10, 10)),
            |cx: &WindowCx<'_>, event| match event {
                Event::Timer(_) => {
                    // Both wait for this call to return.
                    assert_eq!(
                        cx.inner.input_from(key(' '), origin(WM_KEYDOWN, 0x20)),
                        None
                    );
                    assert_eq!(
                        cx.inner.input_from(key('q'), origin(WM_KEYDOWN, 0x51)),
                        None
                    );
                    Handled::Yes
                }
                Event::Key { key, .. } if key.key == Key::Character(' ') => Handled::Yes,
                _ => Handled::No,
            },
        )
        .expect("valid options")
        .0
    }

    #[test]
    fn what_the_handler_declines_goes_on_and_what_it_uses_does_not() {
        let window = space_only();
        let inner = &window.inner;
        assert_eq!(
            inner.input_from(key(' '), origin(WM_KEYDOWN, 0x20)),
            Some(Handled::Yes)
        );
        assert_eq!(passed_on(&window), []);
        assert_eq!(
            inner.input_from(key('q'), origin(WM_KEYDOWN, 0x51)),
            Some(Handled::No)
        );
        assert_eq!(
            passed_on(&window),
            [(
                origin(WM_KEYDOWN, 0x51).expect("some"),
                Event::Key {
                    key: KeyEvent {
                        key: Key::Character('q'),
                        modifiers: Modifiers::NONE,
                        pressed: true
                    },
                    repeat: false
                }
            )]
        );
        // Text the handler declines goes on as the character it made.
        inner.input_from(RawInput::Utf16(0xD83D), origin(WM_CHAR, 0xD83D));
        assert_eq!(passed_on(&window), []);
        inner.input_from(RawInput::Utf16(0xDE00), origin(WM_CHAR, 0xDE00));
        assert_eq!(
            passed_on(&window),
            [(
                origin(WM_CHAR, 0xDE00).expect("some"),
                Event::Text('\u{1F600}')
            )]
        );
        // Input with no message behind it has nothing to pass on.
        inner.input(key('q'));
        assert_eq!(passed_on(&window), []);
    }

    /// A key that waited for the handler is passed on according to the
    /// handler's answer when it is delivered, like any other.
    #[test]
    fn keys_that_waited_are_passed_on_by_the_answer_they_get_later() {
        let window = space_only();
        window.set_timer(TimerId(1), 10).expect("open");
        assert_eq!(
            window.inner.input(RawInput::Timer(TimerId(1))),
            Some(Handled::Yes)
        );
        let passed = passed_on(&window);
        assert_eq!(passed.len(), 1, "{passed:?}");
        assert_eq!(passed[0].0, origin(WM_KEYDOWN, 0x51).expect("some"));
        assert_eq!(window.deferred_events(), 2);
    }

    /// A handler that panicked can use nothing: keys go on to the host.
    #[test]
    fn a_poisoned_window_passes_every_key_on() {
        let calls = Rc::new(RefCell::new(0));
        let (window, _headless) = {
            let calls = Rc::clone(&calls);
            Window::headless(
                WindowOptions::new(PhysicalSize::new(10, 10)),
                move |_: &WindowCx<'_>, _| {
                    *calls.borrow_mut() += 1;
                    panic!("a handler bug")
                },
            )
            .expect("valid options")
        };
        let inner = &window.inner;
        assert_eq!(inner.input_from(key('a'), origin(WM_KEYDOWN, 0x41)), None);
        assert!(window.is_poisoned());
        assert_eq!(passed_on(&window), []);
        assert_eq!(inner.input_from(key('b'), origin(WM_KEYDOWN, 0x42)), None);
        let passed = passed_on(&window);
        assert_eq!(passed.len(), 1);
        assert_eq!(passed[0].0, origin(WM_KEYDOWN, 0x42).expect("some"));
        assert_eq!(*calls.borrow(), 1);
    }

    #[test]
    fn the_wish_for_every_key_is_kept() {
        let window = space_only();
        assert!(!window.wants_all_keys());
        window.cx().set_wants_all_keys(true);
        assert!(window.wants_all_keys());
    }
}
