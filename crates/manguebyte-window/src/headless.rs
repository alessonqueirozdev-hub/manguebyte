//! A window with no system window behind it, for tests on every system.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use manguebyte_gfx::{IRect, Pixmap};
use manguebyte_plugin::editor::checked_scale;

use crate::damage::Damage;
use crate::dialog::{DialogKind, DialogRequest, FileDialog};
use crate::dispatch::Origin;
use crate::drag::{DragData, RawDrag};
use crate::event::{Event, Handled, Modifiers, PhysicalPoint, PhysicalSize, RawInput, TimerId};
use crate::window::Inner;

/// How a headless window answers a dialog: with the paths chosen, or
/// `None` for a cancel.
type DialogAnswer = Box<dyn FnOnce(&DialogRequest) -> Option<Vec<PathBuf>>>;

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
    /// The window's own clipboard.
    clipboard: RefCell<Option<String>>,
    /// Where the system would show the caret, while the window has the
    /// keyboard.
    caret: Cell<Option<IRect>>,
    /// How the next dialogs are answered, in order.
    dialog_answers: RefCell<VecDeque<DialogAnswer>>,
    /// The dialogs asked for, in order.
    dialogs: RefCell<Vec<DialogRequest>>,
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
            clipboard: RefCell::new(None),
            caret: Cell::new(None),
            dialog_answers: RefCell::new(VecDeque::new()),
            dialogs: RefCell::new(Vec::new()),
            #[cfg(test)]
            passed_on: RefCell::new(Vec::new()),
        }
    }

    /// Records a dialog and answers it as the test scripted, or cancels it.
    pub(crate) fn dialog(&self, kind: DialogKind, dialog: &FileDialog) -> Option<Vec<PathBuf>> {
        let request = DialogRequest {
            kind,
            dialog: dialog.clone(),
        };
        self.dialogs.borrow_mut().push(request.clone());
        let answer = self.dialog_answers.borrow_mut().pop_front();
        // Called with nothing borrowed: an answer may drive the window, as
        // the person can while a dialog is open.
        answer.and_then(|answer| answer(&request))
    }

    /// The clipboard's text, cut as the system's is read.
    pub(crate) fn read_clipboard(&self) -> Option<String> {
        let text = self.clipboard.borrow();
        let text = text.as_deref()?;
        let units: Vec<u16> = text
            .encode_utf16()
            .take(crate::clipboard::MAX_CLIPBOARD_UNITS)
            .collect();
        Some(String::from_utf16_lossy(&units))
    }

    pub(crate) fn write_clipboard(&self, text: &str) {
        *self.clipboard.borrow_mut() = Some(text.to_owned());
    }

    pub(crate) fn show_caret(&self, caret: Option<IRect>) {
        self.caret.set(caret);
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
        self.caret.set(None);
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

    /// Whether the window asks for every key, Tab, Enter and Escape
    /// included, as a host's dialog would hear it (see
    /// [`Window::set_wants_all_keys`](crate::Window::set_wants_all_keys)).
    pub fn wants_all_keys(&self) -> bool {
        self.with(|inner| inner.wants_all_keys.get())
            .unwrap_or(false)
    }

    /// What the clipboard holds, as another program would read it.
    pub fn clipboard_text(&self) -> Option<String> {
        self.platform(|p| p.clipboard.borrow().clone()).flatten()
    }

    /// Puts `text` on the clipboard, or empties it, as another program
    /// would.
    pub fn set_clipboard_text(&self, text: Option<&str>) {
        self.platform(|p| *p.clipboard.borrow_mut() = text.map(str::to_owned));
    }

    /// Where the system is shown the text caret, in device pixels: the
    /// window's [`text_caret`](crate::Window::text_caret) while it has the
    /// keyboard, and nothing otherwise, as the system's caret goes with the
    /// keyboard.
    pub fn system_caret(&self) -> Option<IRect> {
        self.platform(|p| p.caret.get()).flatten()
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

    /// Asks an application window to close, as its close button does: the
    /// handler gets [`Event::CloseRequested`](crate::Event::CloseRequested)
    /// and the window stays open unless it closes itself. Returns what the
    /// handler said, or `None` for a child window, a closed one, or a
    /// request that waited for a running handler.
    pub fn request_close(&self) -> Option<Handled> {
        self.with(Inner::close_requested).flatten()
    }

    /// Resizes an application window as the person dragging its frame
    /// does: within its limits, and not at all if it is not resizable.
    /// Returns the size it has afterwards, or `None` once it is gone.
    pub fn drag_frame_to(&self, size: PhysicalSize) -> Option<PhysicalSize> {
        self.with(|inner| {
            if let Some(frame) = inner.frame.as_ref().filter(|frame| frame.resizable) {
                inner.on_size(frame.clamp(size, inner.scale()));
            }
            inner.size()
        })
    }

    /// Moves an application window to a monitor of `scale`, as Win32's
    /// `WM_DPICHANGED` does: the scale changes, then the window takes the
    /// size the system suggests, the same logical size at the new scale,
    /// within its limits. A scale outside `0.25..=8.0` is ignored. Returns
    /// whether it was taken.
    pub fn move_to_monitor(&self, scale: f64) -> bool {
        if checked_scale(scale).is_none() {
            return false;
        }
        self.with(|inner| {
            if !inner.is_open() {
                return false;
            }
            let old = inner.scale();
            let size = inner.size();
            let suggested = PhysicalSize::new(
                ((f64::from(size.width) / old * scale).round() as u32).max(1),
                ((f64::from(size.height) / old * scale).round() as u32).max(1),
            );
            let size = inner.on_dpi_changed(scale, suggested);
            inner.on_size(size);
            true
        })
        .unwrap_or(false)
    }

    /// A drag carrying `data` comes over the window at `at`, as one from
    /// another program does. Returns whether the handler takes it: never
    /// for a window that takes no drops, a drag that carries nothing, or
    /// a handler that is running.
    pub fn drag_enter(&self, at: PhysicalPoint, data: DragData) -> Handled {
        self.drag(RawDrag::Enter {
            at,
            data,
            modifiers: Modifiers::NONE,
        })
    }

    /// The drag moves to `at`. Returns whether the handler takes it there.
    pub fn drag_over(&self, at: PhysicalPoint) -> Handled {
        self.drag(RawDrag::Over {
            at,
            modifiers: Modifiers::NONE,
        })
    }

    /// The drag leaves the window, or is cancelled.
    pub fn drag_leave(&self) {
        self.drag(RawDrag::Leave);
    }

    /// The drag is dropped at `at`. Returns whether the handler took it.
    pub fn drop_at(&self, at: PhysicalPoint) -> Handled {
        self.drag(RawDrag::Drop {
            at,
            modifiers: Modifiers::NONE,
        })
    }

    /// Drags `data` onto the window at `at` and drops it there, in one go.
    /// Returns whether the handler took it.
    pub fn drop_files(&self, at: PhysicalPoint, data: DragData) -> Handled {
        if self.drag_enter(at, data) == Handled::No {
            self.drag_leave();
            return Handled::No;
        }
        self.drop_at(at)
    }

    fn drag(&self, drag: RawDrag) -> Handled {
        self.with(|inner| inner.drag(drag)).unwrap_or(Handled::No)
    }

    /// Answers the next dialog the window shows with `paths`, as the person
    /// choosing them would, or cancels it with `None`. Answers are used in
    /// the order they are given; a dialog with none left is cancelled. An
    /// open dialog for one file, a folder dialog and a save dialog take the
    /// first path only.
    pub fn answer_next_dialog(&self, paths: Option<Vec<PathBuf>>) {
        self.on_next_dialog(move |_| paths);
    }

    /// Answers the next dialog the window shows with what `answer` returns
    /// for it. `answer` runs while the dialog is "open", with the caller
    /// of the dialog waiting: it may give the window input, which waits for
    /// a handler that is the caller, as it would during a system dialog.
    pub fn on_next_dialog(
        &self,
        answer: impl FnOnce(&DialogRequest) -> Option<Vec<PathBuf>> + 'static,
    ) {
        self.platform(|p| p.dialog_answers.borrow_mut().push_back(Box::new(answer)));
    }

    /// The dialogs the window was asked to show since the last call, in
    /// order.
    pub fn take_dialogs(&self) -> Vec<DialogRequest> {
        self.platform(|p| p.dialogs.take()).unwrap_or_default()
    }
}

impl fmt::Debug for Headless {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Headless")
            .field("exists", &self.exists())
            .finish_non_exhaustive()
    }
}
