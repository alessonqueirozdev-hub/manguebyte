//! The macOS backend (ADR 0028, part 2).
//!
//! A plugin's window here is an `NSView` inside the host's `NSView`, made on
//! the process's main thread and driven by the host's run loop: the backend
//! never runs a loop of its own. There are no application windows yet: only
//! [`Window::open`](crate::Window::open) with a
//! [`ParentWindow::Cocoa`](crate::ParentWindow::Cocoa) parent makes a view.
//! (The `testing` module, compiled only with the crate's `testing` feature,
//! makes windows to host it and sends it events for tests.)
//!
//! What the view does:
//!
//! - It is placed at the top-left corner of the parent's bounds and stays
//!   there when the parent is resized. Its size in points is the window's size
//!   in device pixels divided by the screen's backing scale factor, which is
//!   also the window's scale unless the host gave one. The tests run at the
//!   machine's factor and in a window that says its factor is 2; when the
//!   factor of an open view changes, the view is written to keep its size in
//!   points and report the new scale and size, which no test does. A host that
//!   resizes the view itself is reported as a resize.
//! - Presenting converts the damaged rectangles into the blue-green-red back
//!   buffer and asks AppKit to redisplay; the view then gives its layer an
//!   sRGB image copied from the whole buffer, once per display however many
//!   rectangles were presented. As on Win32, alpha is ignored: a pixel shows
//!   its premultiplied colour over black. The same image answers
//!   `drawRect:`, which AppKit uses to draw the view into a bitmap or a
//!   printout, without the handler.
//! - Pointer, wheel, key and focus events come from the view's responder
//!   methods; the view asks for hovering through a tracking area that is
//!   active whether or not the host's window is the key window, and accepts
//!   the click that activates a window (neither is tried by a test). A wheel
//!   turn of a trackpad counts 10 points as a line; motion of less than one
//!   wheel unit is kept until it makes one. Buttons past the middle one go on
//!   up the responder chain. A Control-click is reported as the primary button
//!   with Control held. Command is
//!   [`Modifiers::meta`](crate::Modifiers::meta), Option is `alt`.
//! - Typed text goes through the system's text input
//!   (`NSTextInputClient`), so the system, not the window, makes the
//!   characters; no test runs an input method. Text being composed is not
//!   drawn by the window. The caret
//!   given with [`set_text_caret`](crate::Window::set_text_caret) is where
//!   the input method is told to place its windows.
//! - A key or wheel turn the handler does not use goes on up the responder
//!   chain, to the host, once the handler has answered everything made from
//!   it. A Command combination is offered to the handler before the host's
//!   menus take it.
//! - The window has the keyboard while the view is its window's first
//!   responder; a change of the key window alone is not reported.
//! - Timers are `NSTimer`s on the main run loop, in its common modes, so they
//!   go on while the host tracks a menu or a resize. They end with the view.
//! - A host that takes the view out of its parent, or destroys the parent,
//!   closes the window ([`Event::Closed`](crate::Event::Closed)).
//! - Not done on macOS yet: the clipboard (reads give nothing, writes fail),
//!   file dialogs and drops.
//!
//! What it does to the process, and what it does not:
//!
//! - The view's Objective-C class is declared when the first view is made,
//!   under a name that carries the address of a static of this copy of the
//!   crate, so two plugin binaries, or two copies of the framework in one
//!   plugin, never share a class. A class of that name that does not run this
//!   code is refused. The system keeps the declared class; the crate keeps no
//!   record of its own.
//! - No swizzling, no observers, no threads, no mutable statics and no change
//!   to the application's settings.
//! - A parent address is checked to be a live Objective-C object of a
//!   subclass of `NSView` before any message is sent to it. A view that was
//!   freed and whose memory a new view took cannot be told apart.
//! - Nothing unwinds into AppKit: a panic in a method of the view is caught
//!   there.

use std::cell::{Cell, RefCell};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;

use manguebyte_gfx::IRect;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSEvent, NSTrackingArea, NSView};
use objc2_core_foundation::{CFData, CFRetained, CGPoint, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo,
    CGImageByteOrderInfo, kCGColorSpaceSRGB,
};
use objc2_foundation::{
    NSNumber, NSObject, NSObjectNSDelayedPerforming, NSRunLoop, NSRunLoopCommonModes, NSTimer,
};

use crate::cocoa::{self, PassOn, Wheel};
use crate::dialog::{DialogKind, FileDialog};
use crate::dispatch::{Origin, WindowHandler};
use crate::event::{Handled, PhysicalSize, RawInput, TimerId};
use crate::pixels::BackBuffer;
use crate::pointer::ClickSettings;
use crate::window::{Inner, Platform, WindowError, WindowOptions};

#[cfg(feature = "testing")]
pub mod testing;
mod view;

/// A static that never changes; its address names this copy of the crate in
/// Objective-C class names.
static ANCHOR: u8 = 0;

/// The address of this copy of the crate's static.
fn anchor() -> usize {
    (&raw const ANCHOR).addr()
}

/// Which responder method an event kept for the host goes on to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Method {
    KeyDown,
    KeyUp,
    FlagsChanged,
    ScrollWheel,
    OtherMouseDown,
    OtherMouseDragged,
    OtherMouseUp,
}

/// The macOS side of a window.
pub(crate) struct MacPlatform {
    /// The view, until the window value is dropped. It is out of its parent
    /// and silent once `open` is false.
    view: RefCell<Option<Retained<NSView>>>,
    /// Whether the view still serves the window.
    open: Cell<bool>,
    /// Set once the view is in its parent, so that leaving it closes the
    /// window.
    attached: Cell<bool>,
    /// The screen's backing scale factor: device pixels per point.
    backing: Cell<f64>,
    timers: RefCell<Vec<(TimerId, Retained<NSTimer>)>>,
    tracking: RefCell<Option<Retained<NSTrackingArea>>>,
    /// Keys and wheel turns that may still go on to the host.
    pass_on: RefCell<PassOn<(Method, Retained<NSEvent>)>>,
    /// Set while an event is being sent up the responder chain.
    passing: Cell<bool>,
    /// The number of the key down whose text the input method is making.
    typing: Cell<Option<usize>>,
    /// How many UTF-16 units of text the input method is composing.
    composing: Cell<usize>,
    /// The time and key code of a Command combination already offered to the
    /// handler, which the system then sends as a key down as well.
    offered: Cell<Option<(u64, u16)>>,
    wheel: Cell<Wheel>,
    /// Where the text caret is, while the window has the keyboard.
    caret: Cell<Option<IRect>>,
    /// Set while the view's layer is being updated.
    updating: Cell<bool>,
}

/// Makes a view inside the view at `parent`.
pub(crate) fn open(
    parent: NonZeroUsize,
    options: &WindowOptions,
    handler: Box<dyn WindowHandler>,
) -> Result<Rc<Inner>, WindowError> {
    let mtm = MainThreadMarker::new().ok_or(WindowError::InvalidParent(
        "AppKit views are made on the process's main thread only",
    ))?;
    autoreleasepool(|_| {
        let parent = view::parent_at(parent.get())?;
        let inner = Rc::new(Inner::new(
            Platform::MacOs(MacPlatform::new()),
            options,
            handler,
        ));
        view::attach(&inner, &parent, options, mtm)?;
        Ok(inner)
    })
}

/// The sRGB image of the back buffer, with a copy of its pixels, or `None`
/// for an empty buffer. Alpha is skipped, so each pixel shows its
/// premultiplied colour, opaque.
fn image(back: &BackBuffer) -> Option<CFRetained<CGImage>> {
    let size = back.size();
    if size.is_empty() {
        return None;
    }
    let (width, height) = (size.width as usize, size.height as usize);
    let data = CFData::from_bytes(back.bgra());
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    // SAFETY: a constant string Core Graphics exports, never written.
    let name = unsafe { kCGColorSpaceSRGB };
    let space = CGColorSpace::with_name(Some(name))?;
    // Blue, green, red and an unused byte, in memory order.
    let layout =
        CGBitmapInfo(CGImageAlphaInfo::NoneSkipFirst.0 | CGImageByteOrderInfo::Order32Little.0);
    // SAFETY: the provider holds `height` rows of `width * 4` bytes, which
    // is what the other arguments describe, and keeps its data alive; a null
    // decode array asks for no remapping.
    unsafe {
        CGImage::new(
            width,
            height,
            8,
            32,
            width * 4,
            Some(&space),
            layout,
            Some(&provider),
            ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }
}

/// What a request the backend cannot serve yet reports.
fn missing(call: &'static str) -> WindowError {
    WindowError::Platform { call, code: 0 }
}

impl MacPlatform {
    fn new() -> MacPlatform {
        MacPlatform {
            view: RefCell::new(None),
            open: Cell::new(false),
            attached: Cell::new(false),
            backing: Cell::new(1.0),
            timers: RefCell::new(Vec::new()),
            tracking: RefCell::new(None),
            pass_on: RefCell::new(PassOn::default()),
            passing: Cell::new(false),
            typing: Cell::new(None),
            composing: Cell::new(0),
            offered: Cell::new(None),
            wheel: Cell::new(Wheel::default()),
            caret: Cell::new(None),
            updating: Cell::new(false),
        }
    }

    /// The view while it serves the window. The caller gets a reference of
    /// its own, so no borrow is held across a call into AppKit, which may
    /// call back.
    fn view(&self) -> Option<Retained<NSView>> {
        if !self.open.get() {
            return None;
        }
        self.view.borrow().clone()
    }

    pub(crate) fn handle(&self) -> Option<usize> {
        self.view().map(|view| Retained::as_ptr(&view).addr())
    }

    /// Asks AppKit to redisplay the view.
    pub(crate) fn invalidate(&self) {
        if let Some(view) = self.view() {
            view.setNeedsDisplay(true);
        }
    }

    /// The back buffer changed: the layer is given it at the next display.
    /// While the layer is being updated it is given the buffer right after.
    pub(crate) fn blit(&self) {
        if !self.updating.get() {
            self.invalidate();
        }
    }

    pub(crate) fn set_size(&self, size: PhysicalSize) -> Result<PhysicalSize, WindowError> {
        let view = self.view().ok_or(WindowError::Closed)?;
        let scale = self.backing.get();
        let points = CGSize::new(
            f64::from(size.width) / scale,
            f64::from(size.height) / scale,
        );
        let frame = view.frame();
        // SAFETY: the view is in the parent that holds it, on the main
        // thread, which nothing else changes during the call.
        let parent = unsafe { view.superview() };
        // The top edge stays where it is; AppKit counts from the bottom
        // unless the parent says otherwise.
        let y = if parent.is_some_and(|parent| parent.isFlipped()) {
            frame.origin.y
        } else {
            frame.origin.y + frame.size.height - points.height
        };
        view.setFrameOrigin(CGPoint::new(frame.origin.x, y));
        // The view's own `setFrameSize:` reports the new size.
        view.setFrameSize(points);
        let actual = view.frame().size;
        Ok(cocoa::device_size(actual.width, actual.height, scale))
    }

    pub(crate) fn set_visible(&self, visible: bool) {
        if let Some(view) = self.view() {
            view.setHidden(!visible);
        }
    }

    pub(crate) fn set_timer(&self, id: TimerId, period_ms: u32) -> Result<(), WindowError> {
        let view = self.view().ok_or(WindowError::Closed)?;
        self.kill_timer(id);
        let number = NSNumber::numberWithUnsignedInt(id.0);
        let target: &AnyObject = &view;
        let info: &AnyObject = &number;
        // SAFETY: the view's class declares `mangueTimer:` with the timer as
        // its argument; the timer keeps the view and the number until it is
        // invalidated, which happens when the view stops serving the window.
        let timer = unsafe {
            NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                f64::from(period_ms) / 1000.0,
                target,
                sel!(mangueTimer:),
                Some(info),
                true,
            )
        };
        // SAFETY: a constant string Foundation exports, never written.
        let modes = unsafe { NSRunLoopCommonModes };
        // SAFETY: the main thread's run loop, from the main thread, with one
        // of its own modes.
        unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer, modes) };
        self.timers.borrow_mut().push((id, timer));
        Ok(())
    }

    pub(crate) fn kill_timer(&self, id: TimerId) {
        let timer = {
            let mut timers = self.timers.borrow_mut();
            let index = timers.iter().position(|(kept, _)| *kept == id);
            index.map(|index| timers.remove(index).1)
        };
        if let Some(timer) = timer {
            timer.invalidate();
        }
    }

    pub(crate) fn take_focus(&self, inner: &Inner) {
        let Some(view) = self.view() else { return };
        let Some(window) = view.window() else { return };
        if view::is_first_responder(&view) {
            inner.input(RawInput::Focus(true));
        } else {
            // The view's `becomeFirstResponder` reports it.
            window.makeFirstResponder(Some(&view));
        }
    }

    pub(crate) fn click_settings(&self) -> ClickSettings {
        let seconds = NSEvent::doubleClickInterval();
        // Two points either way, as Windows' default box in pixels.
        let half = (2.0 * self.backing.get()).round() as i32;
        ClickSettings {
            interval_ms: (seconds * 1000.0) as u32,
            half_width: half,
            half_height: half,
        }
    }

    /// The handler answered an event made from a system event.
    pub(crate) fn answered(&self, origin: Origin, handled: Handled) {
        let unused = self
            .pass_on
            .borrow_mut()
            .answer(origin.wparam, handled == Handled::Yes);
        if let Some((method, event)) = unused {
            self.pass(method, &event);
        }
    }

    /// Sends `event` up the responder chain, to the host.
    fn pass(&self, method: Method, event: &NSEvent) {
        let Some(view) = self.view() else { return };
        let before = self.passing.replace(true);
        view::to_next_responder(&view, method, event);
        self.passing.set(before);
    }

    pub(crate) fn show_caret(&self, caret: Option<IRect>) {
        self.caret.set(caret);
        if let Some(context) = self.view().and_then(|view| view.inputContext()) {
            context.invalidateCharacterCoordinates();
        }
    }

    /// Makes `view` silent: no state, timers, tracking or keyboard.
    fn silence(&self, view: &NSView) {
        if !self.open.replace(false) {
            return;
        }
        let state = view::take_state(view);
        // A redisplay still waiting in the run loop, which holds the view.
        // SAFETY: the view is the target of the requests the crate made, and
        // cancelling asks nothing else of it.
        unsafe { NSObject::cancelPreviousPerformRequestsWithTarget(view) };
        let timers = std::mem::take(&mut *self.timers.borrow_mut());
        for (_, timer) in timers {
            timer.invalidate();
        }
        let tracking = self.tracking.borrow_mut().take();
        if let Some(tracking) = tracking {
            view.removeTrackingArea(&tracking);
        }
        self.pass_on.borrow_mut().clear();
        if view::is_first_responder(view) {
            if let Some(window) = view.window() {
                window.makeFirstResponder(None);
            }
        }
        // The view's reference to the shared state, released last.
        drop(state);
    }

    /// Takes the view out of its parent now; the shared state hears of it.
    pub(crate) fn close(&self, inner: &Inner) {
        autoreleasepool(|_| {
            if let Some(view) = self.view() {
                // The view's `viewDidMoveToSuperview` closes the window.
                view.removeFromSuperview();
                self.silence(&view);
            }
        });
        inner.on_destroyed();
    }

    pub(crate) fn destroy(&self) {
        autoreleasepool(|_| {
            let view = self.view.borrow_mut().take();
            if let Some(view) = view {
                self.silence(&view);
                view.removeFromSuperview();
            }
        });
    }

    pub(crate) fn read_clipboard(&self) -> Result<Option<String>, WindowError> {
        Err(missing("the pasteboard, not implemented on macOS yet,"))
    }

    pub(crate) fn write_clipboard(&self) -> Result<(), WindowError> {
        Err(missing("the pasteboard, not implemented on macOS yet,"))
    }

    pub(crate) fn dialog(
        &self,
        _kind: DialogKind,
        _dialog: &FileDialog,
    ) -> Result<Option<Vec<PathBuf>>, WindowError> {
        Err(missing("a file dialog, not implemented on macOS yet,"))
    }
}
