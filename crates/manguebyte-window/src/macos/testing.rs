//! Helpers for tests of the macOS backend: a window that hosts a view, as a
//! host's does, events sent to the view, and what the view draws read back.
//! Compiled only with the crate's `testing` feature.
//!
//! Everything here must be called on the process's first thread, as AppKit
//! requires: a test that uses it has a `main` of its own (`harness = false`).
//!
//! Nothing is read from the screen, so no permission to record it is needed.
//! [`view_pixels`] asks the view to draw into a bitmap, [`layer_pixels`]
//! reads the image its layer was given, and [`layer_rendering`] has Core
//! Animation draw the layer. None of them shows what the window server put
//! on the display.
//!
//! The events are real `NSEvent`s. Presses, drags and releases of the
//! primary and secondary buttons and keys go through the window, as the
//! system's would; the other buttons, moves, exits and wheel turns, which
//! AppKit routes from the pointer's real position or which no window event
//! can express, are sent to the view itself.
//!
//! [`Host::with_backing_scale_of_2`] makes a window that answers 2 when
//! asked for its backing scale factor, whatever the screen's is. That tells
//! apart points and pixels in the backend; the screen, and what the window
//! server shows, stay at the machine's scale.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::ptr;
use std::time::{Duration, Instant};

use manguebyte_gfx::Pixmap;
use manguebyte_plugin::editor::ParentWindow;
use objc2::rc::{Allocated, Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, ClassBuilder, Sel};
use objc2::{AllocAnyThread, ClassType, MainThreadMarker, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBitmapImageRep,
    NSColorSpace, NSDeviceRGBColorSpace, NSEvent, NSEventModifierFlags, NSEventTrackingRunLoopMode,
    NSEventType, NSScreen, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGColorSpace, CGContext, CGDataProvider, CGEvent, CGEventField,
    CGEventType, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo, CGMouseButton, CGScrollEventUnit,
    kCGColorSpaceSRGB,
};
use objc2_foundation::{NSDate, NSProcessInfo, NSRunLoop, NSString};

use super::anchor;
use crate::cocoa;
use crate::event::{Button, Modifiers, PhysicalPoint};
use crate::window::{Platform, Window};

unsafe extern "C" {
    /// The type of a Core Foundation object (`CFBase.h`).
    fn CFGetTypeID(object: *const c_void) -> usize;
}

/// The instance variables of the recording view, one count each.
const COUNTS: [&CStr; 5] = [
    c"keyDowns",
    c"keyUps",
    c"flagChanges",
    c"scrolls",
    c"otherButtons",
];

/// How many events reached the host's view from the views inside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassedOn {
    /// Keys going down.
    pub key_downs: usize,
    /// Keys coming up.
    pub key_ups: usize,
    /// Modifier keys changing.
    pub flag_changes: usize,
    /// Wheel turns.
    pub scrolls: usize,
    /// Presses, drags and releases of buttons other than the primary and the
    /// secondary.
    pub other_buttons: usize,
}

/// Which responder method of the recording view was called.
fn count(view: &NSView, which: usize) {
    let object: &AnyObject = view;
    let Some(variable) = object.class().instance_variable(COUNTS[which]) else {
        return;
    };
    // SAFETY: a `usize` variable of this view, touched on the main thread
    // only.
    unsafe {
        let slot = variable.load_ptr::<usize>(object);
        slot.write(slot.read() + 1);
    }
}

extern "C-unwind" fn host_key_down(view: &NSView, _: Sel, _: &NSEvent) {
    count(view, 0);
}

extern "C-unwind" fn host_key_up(view: &NSView, _: Sel, _: &NSEvent) {
    count(view, 1);
}

extern "C-unwind" fn host_flags_changed(view: &NSView, _: Sel, _: &NSEvent) {
    count(view, 2);
}

extern "C-unwind" fn host_scroll_wheel(view: &NSView, _: Sel, _: &NSEvent) {
    count(view, 3);
}

extern "C-unwind" fn host_other_button(view: &NSView, _: Sel, _: &NSEvent) {
    count(view, 4);
}

/// The class of the host's view, which counts what its subviews pass on.
fn host_class() -> Result<&'static AnyClass, String> {
    let name = CString::new(cocoa::class_name("TestHost", anchor())).map_err(|e| e.to_string())?;
    if let Some(class) = AnyClass::get(&name) {
        return Ok(class);
    }
    let mut builder =
        ClassBuilder::new(&name, NSView::class()).ok_or("the class cannot be declared")?;
    for variable in COUNTS {
        builder.add_ivar::<usize>(variable);
    }
    // SAFETY: each function takes the receiver, the selector and the event
    // and returns nothing, as the responder methods do.
    unsafe {
        builder.add_method(
            sel!(keyDown:),
            host_key_down as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(sel!(keyUp:), host_key_up as extern "C-unwind" fn(_, _, _));
        builder.add_method(
            sel!(flagsChanged:),
            host_flags_changed as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(scrollWheel:),
            host_scroll_wheel as extern "C-unwind" fn(_, _, _),
        );
        for selector in [
            sel!(otherMouseDown:),
            sel!(otherMouseDragged:),
            sel!(otherMouseUp:),
        ] {
            builder.add_method(selector, host_other_button as extern "C-unwind" fn(_, _, _));
        }
    }
    Ok(builder.register())
}

extern "C-unwind" fn scale_of_2(_: &NSWindow, _: Sel) -> CGFloat {
    2.0
}

/// The class of a window that says its backing scale factor is 2.
fn window_class_of_scale_2() -> Result<&'static AnyClass, String> {
    let name =
        CString::new(cocoa::class_name("TestWindow2x", anchor())).map_err(|e| e.to_string())?;
    if let Some(class) = AnyClass::get(&name) {
        return Ok(class);
    }
    let mut builder =
        ClassBuilder::new(&name, NSWindow::class()).ok_or("the class cannot be declared")?;
    // SAFETY: the function takes the receiver and the selector and returns a
    // `CGFloat`, as `backingScaleFactor` does.
    unsafe {
        builder.add_method(
            sel!(backingScaleFactor),
            scale_of_2 as extern "C-unwind" fn(_, _) -> _,
        );
    }
    Ok(builder.register())
}

fn main_thread() -> Result<MainThreadMarker, String> {
    MainThreadMarker::new().ok_or_else(|| "not on the main thread".to_string())
}

/// A visible titled window whose content view stands for a host's view.
pub struct Host {
    window: Retained<NSWindow>,
    parent: Cell<Option<Retained<NSView>>>,
    events: Cell<isize>,
}

impl Host {
    /// A window whose content is `width` by `height` points, shown without
    /// taking the person's keyboard.
    ///
    /// # Errors
    ///
    /// Off the main thread, or if the host view's class cannot be declared.
    pub fn new(width: f64, height: f64) -> Result<Host, String> {
        Host::of_class(NSWindow::class, width, height)
    }

    /// As [`new`](Self::new), in a window that says its backing scale factor
    /// is 2.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new).
    pub fn with_backing_scale_of_2(width: f64, height: f64) -> Result<Host, String> {
        let class = window_class_of_scale_2()?;
        Host::of_class(|| class, width, height)
    }

    /// A host whose window is of the class `class` gives: `NSWindow` or a
    /// subclass of it.
    fn of_class(
        class: impl FnOnce() -> &'static AnyClass,
        width: f64,
        height: f64,
    ) -> Result<Host, String> {
        let mtm = main_thread()?;
        autoreleasepool(|_| {
            let application = NSApplication::sharedApplication(mtm);
            application.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
            let content = CGRect::new(CGPoint::new(100.0, 100.0), CGSize::new(width, height));
            // SAFETY: `alloc` on a window class, on the main thread, gives
            // a new, uninitialised window.
            let allocated: Allocated<AnyObject> = unsafe { msg_send![class(), alloc] };
            // SAFETY: the designated initialiser of NSWindow, which the
            // class is or descends from, with a valid style.
            let made: Option<Retained<AnyObject>> = unsafe {
                msg_send![
                    allocated,
                    initWithContentRect: content,
                    styleMask: NSWindowStyleMask::Titled,
                    backing: NSBackingStoreType::Buffered,
                    defer: false
                ]
            };
            // SAFETY: an instance of NSWindow or of a subclass.
            let window: Retained<NSWindow> =
                unsafe { Retained::cast_unchecked(made.ok_or("no window")?) };
            // SAFETY: the value owns the window; closing must not release it
            // as well.
            unsafe { window.setReleasedWhenClosed(false) };
            let class = host_class()?;
            // SAFETY: a new instance of the class.
            let allocated: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
            let frame = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(width, height));
            // SAFETY: the designated initialiser of NSView, which the class
            // descends from.
            let made: Option<Retained<AnyObject>> =
                unsafe { msg_send![allocated, initWithFrame: frame] };
            // SAFETY: an instance of a subclass of NSView.
            let parent: Retained<NSView> =
                unsafe { Retained::cast_unchecked(made.ok_or("no view")?) };
            let content = window.contentView().ok_or("no content view")?;
            content.addSubview(&parent);
            window.makeKeyAndOrderFront(None);
            window.orderFrontRegardless();
            Ok(Host {
                window,
                parent: Cell::new(Some(parent)),
                events: Cell::new(0),
            })
        })
    }

    fn view(&self) -> Option<Retained<NSView>> {
        let view = self.parent.take();
        self.parent.set(view.clone());
        view
    }

    /// The view a window is opened inside.
    ///
    /// # Panics
    ///
    /// After [`destroy_parent`](Self::destroy_parent).
    pub fn parent(&self) -> ParentWindow {
        let view = self.view().expect("the parent view exists");
        ParentWindow::cocoa(Retained::as_ptr(&view).addr()).expect("not null")
    }

    /// The window's backing scale factor.
    pub fn scale(&self) -> f64 {
        self.window.backingScaleFactor()
    }

    /// How many views are inside the host's view.
    pub fn children(&self) -> usize {
        self.view().map_or(0, |view| view.subviews().len())
    }

    /// The address of the window itself, which is an object but no view.
    pub fn window_address(&self) -> usize {
        Retained::as_ptr(&self.window).addr()
    }

    /// Whether the system says the window is on screen.
    pub fn is_visible(&self) -> bool {
        self.window.isVisible()
    }

    /// What reached the host's view since the last call.
    pub fn take_passed_on(&self) -> PassedOn {
        let Some(view) = self.view() else {
            return PassedOn::default();
        };
        let object: &AnyObject = &view;
        let [key_downs, key_ups, flag_changes, scrolls, other_buttons] = COUNTS.map(|name| {
            object
                .class()
                .instance_variable(name)
                // SAFETY: a `usize` variable of this view, on the main
                // thread.
                .map_or(0, |variable| unsafe {
                    let slot = variable.load_ptr::<usize>(object);
                    let seen = slot.read();
                    slot.write(0);
                    seen
                })
        });
        PassedOn {
            key_downs,
            key_ups,
            flag_changes,
            scrolls,
            other_buttons,
        }
    }

    /// Moves the origin of the host view's own coordinates, as a host that
    /// scrolls its view does.
    pub fn set_parent_bounds_origin(&self, x: f64, y: f64) {
        if let Some(view) = self.view() {
            view.setBoundsOrigin(CGPoint::new(x, y));
        }
    }

    /// Resizes the host's view, as a host that resizes its own window does.
    pub fn resize_parent(&self, width: f64, height: f64) {
        if let Some(view) = self.view() {
            view.setFrameSize(CGSize::new(width, height));
        }
    }

    /// Takes the host's view out of its window and releases it, so that it
    /// is destroyed.
    pub fn destroy_parent(&self) {
        autoreleasepool(|_| {
            if let Some(view) = self.parent.take() {
                view.removeFromSuperview();
            }
        });
    }

    /// The point of the screen where the point `(x, y)` of the host's view
    /// is, counted from the view's bottom-left corner.
    pub fn to_screen(&self, x: f64, y: f64) -> [f64; 2] {
        let Some(view) = self.view() else {
            return [0.0, 0.0];
        };
        let in_window = view.convertPoint_toView(CGPoint::new(x, y), None);
        let rect = CGRect::new(in_window, CGSize::new(0.0, 0.0));
        let screen = self.window.convertRectToScreen(rect);
        [screen.origin.x, screen.origin.y]
    }

    /// Presses and releases the primary button, through the window, at the
    /// point `(x, y)` of the host's view, so that whichever view is there
    /// gets it.
    pub fn click(&self, x: f64, y: f64) {
        autoreleasepool(|_| {
            let Some(view) = self.view() else { return };
            let at = view.convertPoint_toView(CGPoint::new(x, y), None);
            for kind in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
                let number = self.events.get() + 1;
                self.events.set(number);
                if let Some(event) = mouse_event(&self.window, kind, at, 0, uptime(), number) {
                    self.window.sendEvent(&event);
                }
            }
        });
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            drop(self.parent.take());
            self.window.orderOut(None);
            self.window.close();
        });
    }
}

/// Runs the main run loop for `time`: AppKit displays what asked for it, and
/// timers fire.
pub fn pump(time: Duration) {
    autoreleasepool(|_| {
        let until = NSDate::dateWithTimeIntervalSinceNow(time.as_secs_f64());
        NSRunLoop::currentRunLoop().runUntilDate(&until);
    });
}

/// As [`pump`], in the mode AppKit runs the loop in while it tracks a menu,
/// a slider or a resize.
pub fn pump_tracking(time: Duration) {
    autoreleasepool(|_| {
        // SAFETY: a constant string AppKit exports, never written.
        let mode = unsafe { NSEventTrackingRunLoopMode };
        let end = Instant::now() + time;
        while Instant::now() < end {
            let soon = NSDate::dateWithTimeIntervalSinceNow(0.005);
            NSRunLoop::currentRunLoop().runMode_beforeDate(mode, &soon);
        }
    });
}

/// The view under `window`, while it serves it.
fn view_of(window: &Window) -> Option<Retained<NSView>> {
    match window.cx().platform() {
        Platform::MacOs(platform) => platform.view(),
        Platform::Headless(_) => None,
    }
}

/// Has AppKit display the view now if it needs it.
pub fn display(window: &Window) {
    autoreleasepool(|_| {
        if let Some(view) = view_of(window) {
            view.displayIfNeeded();
        }
    });
}

/// The name of the view's class.
pub fn view_class_name(window: &Window) -> Option<String> {
    let view = view_of(window)?;
    let object: &AnyObject = &view;
    Some(object.class().name().to_string_lossy().into_owned())
}

/// The view's frame in its parent, in points: left, bottom, width, height.
pub fn view_frame(window: &Window) -> Option<[f64; 4]> {
    let frame = view_of(window)?.frame();
    Some([
        frame.origin.x,
        frame.origin.y,
        frame.size.width,
        frame.size.height,
    ])
}

/// Whether the view is hidden.
pub fn view_is_hidden(window: &Window) -> Option<bool> {
    Some(view_of(window)?.isHidden())
}

/// Whether the view is its window's first responder.
pub fn view_is_first_responder(window: &Window) -> bool {
    view_of(window).is_some_and(|view| super::view::is_first_responder(&view))
}

/// Gives the keyboard to nothing in the view's window.
pub fn resign_view(window: &Window) {
    if let Some(host) = view_of(window).and_then(|view| view.window()) {
        host.makeFirstResponder(None);
    }
}

/// Resizes the view as a host that resizes it itself does.
pub fn resize_view_as_host(window: &Window, width: f64, height: f64) {
    autoreleasepool(|_| {
        if let Some(view) = view_of(window) {
            view.setFrameSize(CGSize::new(width, height));
        }
    });
}

/// Takes the view out of its parent, as a host that closes an editor
/// without being asked does.
pub fn remove_view_as_host(window: &Window) {
    autoreleasepool(|_| {
        if let Some(view) = view_of(window) {
            view.removeFromSuperview();
        }
    });
}

/// Where the view tells an input method the text caret is, on the screen:
/// left, bottom, width, height.
pub fn caret_on_screen(window: &Window) -> Option<[f64; 4]> {
    let view = view_of(window)?;
    let range = objc2_foundation::NSRange {
        location: 0,
        length: 0,
    };
    let nowhere: *mut objc2_foundation::NSRange = ptr::null_mut();
    // SAFETY: the view implements this method of NSTextInputClient, which
    // takes a range and a pointer that may be null.
    let rect: CGRect =
        unsafe { msg_send![&*view, firstRectForCharacterRange: range, actualRange: nowhere] };
    Some([
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ])
}

fn srgb() -> Option<objc2_core_foundation::CFRetained<CGColorSpace>> {
    // SAFETY: a constant string Core Graphics exports, never written.
    CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
}

/// What the view draws when AppKit asks it to draw into a bitmap of its
/// size in device pixels, in sRGB.
pub fn view_pixels(window: &Window) -> Option<Pixmap> {
    autoreleasepool(|_| {
        let view = view_of(window)?;
        let size = window.size();
        let (width, height) = (size.width as usize, size.height as usize);
        // SAFETY: a null list of planes asks the representation to hold the
        // pixels itself; the numbers describe 8-bit RGBA.
        let bitmap = unsafe {
            NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
                NSBitmapImageRep::alloc(),
                ptr::null_mut(),
                isize::try_from(width).ok()?,
                isize::try_from(height).ok()?,
                8,
                4,
                true,
                false,
                NSDeviceRGBColorSpace,
                isize::try_from(width * 4).ok()?,
                32,
            )
        }?;
        // Tagged, not converted: the view's sRGB pixels land unchanged.
        let bitmap =
            bitmap.bitmapImageRepByRetaggingWithColorSpace(&NSColorSpace::sRGBColorSpace())?;
        let bounds = view.bounds();
        bitmap.setSize(bounds.size);
        view.cacheDisplayInRect_toBitmapImageRep(bounds, &bitmap);
        let data = bitmap.bitmapData();
        let stride = usize::try_from(bitmap.bytesPerRow()).ok()?;
        if data.is_null() || stride < width * 4 {
            return None;
        }
        let mut pixels = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            // SAFETY: the representation holds `height` rows of `stride`
            // bytes, at least `width * 4` of them pixels, and outlives the
            // slice.
            let row = unsafe { std::slice::from_raw_parts(data.add(row * stride), width * 4) };
            pixels.extend_from_slice(row);
        }
        Pixmap::from_premultiplied(size.width, size.height, pixels).ok()
    })
}

/// The image the view's layer shows: its pixels as stored, made opaque as
/// the layer shows them.
pub fn layer_pixels(window: &Window) -> Option<Pixmap> {
    autoreleasepool(|_| {
        let layer = view_of(window)?.layer()?;
        // SAFETY: the view sets its layer's contents to an image or nothing.
        let contents = unsafe { layer.contents() }?;
        let pointer = Retained::as_ptr(&contents);
        // SAFETY: every object a layer holds may be asked its Core
        // Foundation type.
        let kind = unsafe { CFGetTypeID(pointer.cast()) };
        if kind != <CGImage as objc2_core_foundation::ConcreteType>::type_id() {
            return None;
        }
        // SAFETY: the type was just checked, and `contents` keeps it alive.
        let image: &CGImage = unsafe { &*pointer.cast::<CGImage>() };
        let width = CGImage::width(Some(image));
        let height = CGImage::height(Some(image));
        let stride = CGImage::bytes_per_row(Some(image));
        let provider = CGImage::data_provider(Some(image))?;
        let data = CGDataProvider::data(Some(&provider))?.to_vec();
        let mut pixels = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let row = data.get(row * stride..row * stride + width * 4)?;
            for bgrx in row.chunks_exact(4) {
                pixels.extend_from_slice(&[bgrx[2], bgrx[1], bgrx[0], 255]);
            }
        }
        Pixmap::from_premultiplied(
            u32::try_from(width).ok()?,
            u32::try_from(height).ok()?,
            pixels,
        )
        .ok()
    })
}

/// How many pixels the view's layer says a point of its contents is.
pub fn layer_contents_scale(window: &Window) -> Option<f64> {
    autoreleasepool(|_| Some(view_of(window)?.layer()?.contentsScale()))
}

/// The view's layer drawn by Core Animation into a bitmap of the view's
/// size in device pixels, in sRGB.
pub fn layer_rendering(window: &Window) -> Option<Pixmap> {
    autoreleasepool(|_| {
        let layer = view_of(window)?.layer()?;
        let size = window.size();
        let (width, height) = (size.width as usize, size.height as usize);
        let mut pixels = vec![0u8; width * height * 4];
        let space = srgb()?;
        let layout = CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;
        {
            // SAFETY: the buffer holds `height` rows of `width * 4` bytes
            // and outlives the context, which is released at the end of this
            // block.
            let context = unsafe {
                CGBitmapContextCreate(
                    pixels.as_mut_ptr().cast(),
                    width,
                    height,
                    8,
                    width * 4,
                    Some(&space),
                    layout,
                )
            }?;
            let [_, _, points_wide, points_high] = view_frame(window)?;
            CGContext::scale_ctm(
                Some(&context),
                width as f64 / points_wide,
                height as f64 / points_high,
            );
            layer.renderInContext(&context);
        }
        Pixmap::from_premultiplied(size.width, size.height, pixels).ok()
    })
}

/// What an [`Input::ExtraButton`] does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonAction {
    /// It goes down.
    Press,
    /// The pointer moves while it is held.
    Drag,
    /// It comes up.
    Release,
}

/// An event for [`send`]. Positions are device pixels of the view.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum Input<'a> {
    /// A button goes down.
    Press {
        /// Where.
        at: PhysicalPoint,
        /// Which button.
        button: Button,
        /// The modifiers held.
        modifiers: Modifiers,
        /// The event's time, in seconds since the system started.
        time: f64,
    },
    /// The pointer moves with a button held.
    Drag {
        /// Where.
        at: PhysicalPoint,
        /// The button held.
        button: Button,
    },
    /// A button comes up.
    Release {
        /// Where.
        at: PhysicalPoint,
        /// Which button.
        button: Button,
    },
    /// The pointer moves with no button held.
    Hover {
        /// Where.
        at: PhysicalPoint,
    },
    /// The pointer leaves the view.
    Leave,
    /// The wheel or a trackpad scrolls, with the system's signs.
    Scroll {
        /// Across.
        dx: i32,
        /// Up.
        dy: i32,
        /// Whether the numbers are a trackpad's points rather than lines.
        precise: bool,
    },
    /// A key goes down or comes up.
    Key {
        /// The virtual key code.
        code: u16,
        /// What it types with the modifiers held.
        characters: &'a str,
        /// The modifiers held.
        modifiers: Modifiers,
        /// Whether it goes down.
        pressed: bool,
        /// Whether it is a repeat of a held key.
        repeat: bool,
    },
    /// A scroll by a fraction of a line up, as a fine wheel's.
    FineScroll {
        /// Lines up.
        dy: f64,
    },
    /// A button past the middle one goes down, is dragged or comes up.
    ExtraButton {
        /// Where.
        at: PhysicalPoint,
        /// What it does.
        action: ButtonAction,
    },
    /// A modifier key changes.
    Modifier {
        /// The virtual key code.
        code: u16,
        /// The modifiers held after the change.
        modifiers: Modifiers,
    },
}

fn flags(modifiers: Modifiers) -> usize {
    let mut flags = 0;
    for (held, flag) in [
        (modifiers.shift, cocoa::FLAG_SHIFT),
        (modifiers.control, cocoa::FLAG_CONTROL),
        (modifiers.alt, cocoa::FLAG_OPTION),
        (modifiers.meta, cocoa::FLAG_COMMAND),
    ] {
        if held {
            flags |= flag;
        }
    }
    flags
}

/// Seconds since the system started, as events count time.
pub fn uptime() -> f64 {
    NSProcessInfo::processInfo().systemUptime()
}

/// A mouse event of `window` at the window's point `at`.
fn mouse_event(
    window: &NSWindow,
    kind: NSEventType,
    at: CGPoint,
    flags: usize,
    time: f64,
    number: isize,
) -> Option<Retained<NSEvent>> {
    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        kind,
        at,
        NSEventModifierFlags(flags),
        time,
        window.windowNumber(),
        None,
        number,
        1,
        1.0,
    )
}

/// The middle of the device pixel `at`, in the view's points, by the
/// window's backing scale factor and not by what the backend made of it.
fn view_point(host: &NSWindow, view: &NSView, at: PhysicalPoint) -> CGPoint {
    let scale = host.backingScaleFactor();
    let bounds = view.bounds();
    CGPoint::new(
        bounds.origin.x + (f64::from(at.x) + 0.5) / scale,
        bounds.origin.y + bounds.size.height - (f64::from(at.y) + 0.5) / scale,
    )
}

/// Sends the view of `window` an event made from `input`. Returns whether it
/// was made and sent.
pub fn send(window: &Window, input: Input<'_>) -> bool {
    autoreleasepool(|_| {
        let Some(view) = view_of(window) else {
            return false;
        };
        let Some(host) = view.window() else {
            return false;
        };
        deliver(&host, &view, input).is_some()
    })
}

fn deliver(host: &NSWindow, view: &NSView, input: Input<'_>) -> Option<()> {
    let in_window = |at| view.convertPoint_toView(view_point(host, view, at), None);
    match input {
        Input::Press {
            at,
            button: Button::Middle,
            ..
        } => {
            let event = other_event(host, in_window(at), CGEventType::OtherMouseDown, MIDDLE)?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, otherMouseDown: &*event] };
        }
        Input::Release {
            at,
            button: Button::Middle,
        } => {
            let event = other_event(host, in_window(at), CGEventType::OtherMouseUp, MIDDLE)?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, otherMouseUp: &*event] };
        }
        Input::Press {
            at,
            button,
            modifiers,
            time,
        } => {
            let kind = match button {
                Button::Secondary => NSEventType::RightMouseDown,
                _ => NSEventType::LeftMouseDown,
            };
            let event = mouse_event(host, kind, in_window(at), flags(modifiers), time, 1)?;
            host.sendEvent(&event);
        }
        Input::Drag { at, button } => {
            let kind = match button {
                Button::Secondary => NSEventType::RightMouseDragged,
                _ => NSEventType::LeftMouseDragged,
            };
            let event = mouse_event(host, kind, in_window(at), 0, uptime(), 2)?;
            host.sendEvent(&event);
        }
        Input::Release { at, button } => {
            let kind = match button {
                Button::Secondary => NSEventType::RightMouseUp,
                _ => NSEventType::LeftMouseUp,
            };
            let event = mouse_event(host, kind, in_window(at), 0, uptime(), 3)?;
            host.sendEvent(&event);
        }
        Input::Hover { at } => {
            let event = mouse_event(host, NSEventType::MouseMoved, in_window(at), 0, uptime(), 4)?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, mouseMoved: &*event] };
        }
        Input::Leave => {
            // SAFETY: no user data is given.
            let event = unsafe {
                NSEvent::enterExitEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_trackingNumber_userData(
                    NSEventType::MouseExited,
                    CGPoint::new(-1.0, -1.0),
                    NSEventModifierFlags(0),
                    uptime(),
                    host.windowNumber(),
                    None,
                    5,
                    0,
                    ptr::null_mut(),
                )
            }?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, mouseExited: &*event] };
        }
        Input::Scroll { dx, dy, precise } => {
            let unit = if precise {
                CGScrollEventUnit::Pixel
            } else {
                CGScrollEventUnit::Line
            };
            let event = CGEvent::new_scroll_wheel_event2(None, unit, 2, dy, dx, 0)?;
            let event = NSEvent::eventWithCGEvent(&event)?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, scrollWheel: &*event] };
        }
        Input::Key {
            code,
            characters,
            modifiers,
            pressed,
            repeat,
        } => {
            let kind = if pressed {
                NSEventType::KeyDown
            } else {
                NSEventType::KeyUp
            };
            let event = key_event(host, kind, code, characters, flags(modifiers), repeat)?;
            if pressed && modifiers.meta {
                // As the application does with a Command combination: the
                // views are offered it before it goes as a key down.
                // SAFETY: NSWindow answers whether one of its views took it.
                let taken: bool = unsafe { msg_send![host, performKeyEquivalent: &*event] };
                if taken {
                    return Some(());
                }
            }
            host.sendEvent(&event);
        }
        Input::FineScroll { dy } => {
            let event =
                CGEvent::new_scroll_wheel_event2(None, CGScrollEventUnit::Line, 1, 0, 0, 0)?;
            CGEvent::set_double_value_field(
                Some(&event),
                CGEventField::ScrollWheelEventFixedPtDeltaAxis1,
                dy,
            );
            let event = NSEvent::eventWithCGEvent(&event)?;
            // SAFETY: the view's own method, with an event.
            let _: () = unsafe { msg_send![view, scrollWheel: &*event] };
        }
        Input::ExtraButton { at, action } => {
            let kind = match action {
                ButtonAction::Press => CGEventType::OtherMouseDown,
                ButtonAction::Drag => CGEventType::OtherMouseDragged,
                ButtonAction::Release => CGEventType::OtherMouseUp,
            };
            // The fourth button: they are counted from 0.
            let event = other_event(host, in_window(at), kind, CGMouseButton(3))?;
            match action {
                // SAFETY: the view's own method, with an event.
                ButtonAction::Press => unsafe { msg_send![view, otherMouseDown: &*event] },
                // SAFETY: as above.
                ButtonAction::Drag => unsafe { msg_send![view, otherMouseDragged: &*event] },
                // SAFETY: as above.
                ButtonAction::Release => unsafe { msg_send![view, otherMouseUp: &*event] },
            }
        }
        Input::Modifier { code, modifiers } => {
            let event = key_event(
                host,
                NSEventType::FlagsChanged,
                code,
                "",
                flags(modifiers),
                false,
            )?;
            host.sendEvent(&event);
        }
    }
    Some(())
}

/// The middle button.
const MIDDLE: CGMouseButton = CGMouseButton::Center;

/// An event of a button other than the primary and the secondary. An event
/// made for a window cannot say which of those it is, so this one is Core
/// Graphics', with a place on the screen, counted from the primary screen's
/// top.
fn other_event(
    window: &NSWindow,
    at: CGPoint,
    kind: CGEventType,
    button: CGMouseButton,
) -> Option<Retained<NSEvent>> {
    let point = window.convertRectToScreen(CGRect::new(at, CGSize::new(0.0, 0.0)));
    let screens = NSScreen::screens(MainThreadMarker::new()?);
    let primary = screens.firstObject()?.frame().size.height;
    let event = CGEvent::new_mouse_event(
        None,
        kind,
        CGPoint::new(point.origin.x, primary - point.origin.y),
        button,
    )?;
    NSEvent::eventWithCGEvent(&event)
}

fn key_event(
    window: &NSWindow,
    kind: NSEventType,
    code: u16,
    characters: &str,
    flags: usize,
    repeat: bool,
) -> Option<Retained<NSEvent>> {
    let characters = NSString::from_str(characters);
    NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
        kind,
        CGPoint::new(0.0, 0.0),
        NSEventModifierFlags(flags),
        uptime(),
        window.windowNumber(),
        None,
        &characters,
        &characters,
        repeat,
        code,
    )
}
