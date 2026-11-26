//! The view: its Objective-C class and what each of its methods does.
//!
//! An instance holds one strong reference to the window's shared state, in
//! an instance variable, from [`attach`] until [`take_state`]. A method
//! takes a reference of its own for as long as it runs, and one on the view,
//! so that a handler may drop the window while it is being called.

use std::ffi::{CStr, CString, c_void};
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::ptr;
use std::rc::Rc;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, AnyProtocol, Bool, ClassBuilder, Sel};
use objc2::{ClassType, MainThreadMarker, Message, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSEvent, NSEventModifierFlags, NSEventType, NSGraphicsContext,
    NSScreen, NSTrackingArea, NSTrackingAreaOptions, NSView, NSViewLayerContentsRedrawPolicy,
    NSWindow,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGContext, CGInterpolationQuality};
use objc2_foundation::{
    NSArray, NSAttributedString, NSNumber, NSRange, NSRunLoopCommonModes, NSString, NSTimer,
};

use super::{MacPlatform, Method, anchor, image, missing};
use crate::cocoa;
use crate::dispatch::{Origin, drop_quietly};
use crate::event::{Button, Handled, KeyEvent, PhysicalPoint, RawInput, TimerId};
use crate::window::{Inner, Platform, WindowError, WindowOptions};

/// The instance variable that holds the window's shared state.
const STATE: &CStr = c"mangueState";

/// `NSNotFound`, as a text range's location.
const NOT_FOUND: usize = isize::MAX as usize;

unsafe extern "C" {
    /// The size of the heap block that starts at `ptr`, or 0 if none does.
    /// From the system's allocator (`malloc/malloc.h`).
    fn malloc_size(ptr: *const c_void) -> usize;
}

/// The view at `address`, if a live Objective-C object of `NSView` or a
/// subclass of it is there.
pub(super) fn parent_at(address: usize) -> Result<Retained<NSView>, WindowError> {
    const REFUSED: WindowError = WindowError::InvalidParent("not an NSView");
    if address % mem::align_of::<usize>() != 0 {
        return Err(REFUSED);
    }
    let pointer: *mut AnyObject = ptr::with_exposed_provenance_mut(address);
    // SAFETY: `malloc_size` takes any address and answers 0 for one that is
    // not the start of a block of the system's allocator.
    if unsafe { malloc_size(pointer.cast_const().cast()) } < mem::size_of::<usize>() {
        return Err(REFUSED);
    }
    // SAFETY: the address is the start of a heap block at least a word
    // long, and `object_getClass` reads no more than that first word; what
    // it returns is only compared until it is known to be a class.
    let found = unsafe { objc2::ffi::object_getClass(pointer) };
    let classes = AnyClass::classes();
    let mut class = classes.iter().copied().find(|known| ptr::eq(*known, found));
    while let Some(known) = class {
        if ptr::eq(known, NSView::class()) {
            // SAFETY: an object whose class descends from NSView.
            return unsafe { Retained::retain(pointer.cast::<NSView>()) }.ok_or(REFUSED);
        }
        class = known.superclass();
    }
    Err(REFUSED)
}

/// The view's class, declared on the first call in this process.
fn view_class() -> Result<&'static AnyClass, WindowError> {
    let name = CString::new(cocoa::class_name("View", anchor()))
        .map_err(|_| missing("naming the view's class"))?;
    // Only the main thread gets here, so nothing declares the class between
    // the look and the declaration.
    match AnyClass::get(&name) {
        Some(class) if is_ours(class) => Ok(class),
        Some(_) => Err(missing(
            "declaring the view's class, whose name other code holds,",
        )),
        None => declare(&name),
    }
}

/// Whether `class` draws with this copy of the crate's code.
fn is_ours(class: &AnyClass) -> bool {
    class.instance_variable(STATE).is_some()
        && class
            .instance_method(sel!(drawRect:))
            .is_some_and(|method| {
                method.implementation() as usize == draw_rect as *const () as usize
            })
}

fn declare(name: &CStr) -> Result<&'static AnyClass, WindowError> {
    let text_input = AnyProtocol::get(c"NSTextInputClient")
        .ok_or_else(|| missing("finding the NSTextInputClient protocol"))?;
    let mut builder = ClassBuilder::new(name, NSView::class())
        .ok_or_else(|| missing("declaring the view's class"))?;
    builder.add_ivar::<*mut c_void>(STATE);
    builder.add_protocol(text_input);
    // SAFETY: every function takes the receiver, the selector and then the
    // arguments, and returns the value, that AppKit's documentation gives
    // for the method of that selector (`mangueTimer:` and `mangueRedraw` are
    // this class's own); none unwinds.
    unsafe {
        builder.add_method(sel!(drawRect:), draw_rect as extern "C-unwind" fn(_, _, _));
        builder.add_method(
            sel!(wantsUpdateLayer),
            yes as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(updateLayer),
            update_layer as extern "C-unwind" fn(_, _),
        );
        builder.add_method(sel!(isOpaque), yes as extern "C-unwind" fn(_, _) -> _);
        builder.add_method(sel!(mangueRedraw), redraw as extern "C-unwind" fn(_, _));
        builder.add_method(sel!(mangueTimer:), timer as extern "C-unwind" fn(_, _, _));

        builder.add_method(
            sel!(setFrameSize:),
            set_frame_size as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(viewDidChangeBackingProperties),
            backing_changed as extern "C-unwind" fn(_, _),
        );
        builder.add_method(
            sel!(viewWillMoveToWindow:),
            will_move_to_window as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(viewDidMoveToSuperview),
            did_move_to_superview as extern "C-unwind" fn(_, _),
        );
        builder.add_method(sel!(viewDidHide), did_hide as extern "C-unwind" fn(_, _));

        builder.add_method(
            sel!(acceptsFirstResponder),
            accepts_first_responder as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(becomeFirstResponder),
            become_first_responder as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(resignFirstResponder),
            resign_first_responder as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(acceptsFirstMouse:),
            accepts_first_mouse as extern "C-unwind" fn(_, _, _) -> _,
        );
        builder.add_method(
            sel!(mouseDownCanMoveWindow),
            no as extern "C-unwind" fn(_, _) -> _,
        );

        for selector in [
            sel!(mouseDown:),
            sel!(rightMouseDown:),
            sel!(otherMouseDown:),
        ] {
            builder.add_method(selector, button_down as extern "C-unwind" fn(_, _, _));
        }
        for selector in [sel!(mouseUp:), sel!(rightMouseUp:), sel!(otherMouseUp:)] {
            builder.add_method(selector, button_up as extern "C-unwind" fn(_, _, _));
        }
        for selector in [
            sel!(mouseDragged:),
            sel!(rightMouseDragged:),
            sel!(otherMouseDragged:),
            sel!(mouseEntered:),
        ] {
            builder.add_method(selector, dragged as extern "C-unwind" fn(_, _, _));
        }
        builder.add_method(sel!(mouseMoved:), moved as extern "C-unwind" fn(_, _, _));
        builder.add_method(sel!(mouseExited:), exited as extern "C-unwind" fn(_, _, _));
        builder.add_method(
            sel!(scrollWheel:),
            scroll_wheel as extern "C-unwind" fn(_, _, _),
        );

        builder.add_method(sel!(keyDown:), key_down as extern "C-unwind" fn(_, _, _));
        builder.add_method(sel!(keyUp:), key_up as extern "C-unwind" fn(_, _, _));
        builder.add_method(
            sel!(flagsChanged:),
            flags_changed as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(performKeyEquivalent:),
            key_equivalent as extern "C-unwind" fn(_, _, _) -> _,
        );

        builder.add_method(
            sel!(insertText:replacementRange:),
            insert_text as extern "C-unwind" fn(_, _, _, _),
        );
        builder.add_method(
            sel!(doCommandBySelector:),
            do_command as extern "C-unwind" fn(_, _, _),
        );
        builder.add_method(
            sel!(setMarkedText:selectedRange:replacementRange:),
            set_marked_text as extern "C-unwind" fn(_, _, _, _, _),
        );
        builder.add_method(sel!(unmarkText), unmark_text as extern "C-unwind" fn(_, _));
        builder.add_method(
            sel!(selectedRange),
            selected_range as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(markedRange),
            marked_range as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(hasMarkedText),
            has_marked_text as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(attributedSubstringForProposedRange:actualRange:),
            substring as extern "C-unwind" fn(_, _, _, _) -> _,
        );
        builder.add_method(
            sel!(validAttributesForMarkedText),
            marked_attributes as extern "C-unwind" fn(_, _) -> _,
        );
        builder.add_method(
            sel!(firstRectForCharacterRange:actualRange:),
            first_rect as extern "C-unwind" fn(_, _, _, _) -> _,
        );
        builder.add_method(
            sel!(characterIndexForPoint:),
            character_index as extern "C-unwind" fn(_, _, _) -> _,
        );
    }
    Ok(builder.register())
}

/// Makes the view for the window `inner` and puts it into `parent`.
pub(super) fn attach(
    inner: &Rc<Inner>,
    parent: &NSView,
    options: &WindowOptions,
    mtm: MainThreadMarker,
) -> Result<(), WindowError> {
    let class = view_class()?;
    let Platform::MacOs(platform) = &inner.platform else {
        return Err(missing("making a view for another backend"));
    };
    // The window's starting size and scale are taken without events.
    inner.quiet.set(true);
    let scale = parent
        .window()
        .map(|window| window.backingScaleFactor())
        .or_else(|| NSScreen::mainScreen(mtm).map(|screen| screen.backingScaleFactor()))
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .unwrap_or(1.0);
    platform.backing.set(scale);
    inner.init_system_scale(scale);

    let size = CGSize::new(
        f64::from(options.size.width) / scale,
        f64::from(options.size.height) / scale,
    );
    // The parent's top-left corner, and what keeps the view there.
    let (y, bottom_margin) = if parent.isFlipped() {
        (0.0, NSAutoresizingMaskOptions::ViewMaxYMargin)
    } else {
        (
            parent.bounds().size.height - size.height,
            NSAutoresizingMaskOptions::ViewMinYMargin,
        )
    };
    let frame = CGRect::new(CGPoint::new(0.0, y), size);
    // SAFETY: `alloc` on a class gives a new, uninitialised instance.
    let allocated: Allocated<AnyObject> = unsafe { msg_send![class, alloc] };
    // SAFETY: the class descends from NSView, whose designated initialiser
    // this is.
    let made: Option<Retained<AnyObject>> = unsafe { msg_send![allocated, initWithFrame: frame] };
    let made = made.ok_or_else(|| missing("initWithFrame:"))?;
    // SAFETY: an instance of a subclass of NSView.
    let view: Retained<NSView> = unsafe { Retained::cast_unchecked(made) };

    put_state(&view, Rc::clone(inner));
    *platform.view.borrow_mut() = Some(view.clone());
    platform.open.set(true);

    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
    view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMaxXMargin | bottom_margin);
    let owner: &AnyObject = &view;
    // SAFETY: the owner is the view, which declares the mouse methods the
    // options ask for and removes the area before it stops serving; no user
    // info is given.
    let tracking = unsafe {
        NSTrackingArea::initWithRect_options_owner_userInfo(
            mtm.alloc(),
            CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0)),
            NSTrackingAreaOptions::MouseEnteredAndExited
                | NSTrackingAreaOptions::MouseMoved
                | NSTrackingAreaOptions::ActiveAlways
                | NSTrackingAreaOptions::InVisibleRect,
            Some(owner),
            None,
        )
    };
    view.addTrackingArea(&tracking);
    *platform.tracking.borrow_mut() = Some(tracking);
    if !options.visible {
        view.setHidden(true);
    }
    parent.addSubview(&view);
    platform.attached.set(true);

    if let Some(window) = view.window() {
        take_scale(inner, platform, &window);
    }
    follow_frame(inner, platform, &view);
    inner.quiet.set(false);
    view.setNeedsDisplay(true);
    Ok(())
}

/// Gives the view its reference to the shared state.
fn put_state(view: &NSView, inner: Rc<Inner>) {
    if let Some(slot) = slot(view) {
        // SAFETY: the slot is this view's instance variable, empty until
        // now, and only the main thread touches it.
        unsafe { slot.write(Rc::into_raw(inner).cast_mut().cast()) };
    }
}

/// Where the view keeps the shared state.
fn slot(view: &NSView) -> Option<*mut *mut c_void> {
    let object: &AnyObject = view;
    let variable = object.class().instance_variable(STATE)?;
    // SAFETY: the variable was declared with this type, on the view's class
    // or one it descends from.
    Some(unsafe { variable.load_ptr::<*mut c_void>(object) })
}

/// A reference of its own to the view's shared state, if it still has one.
fn state(view: &NSView) -> Option<Rc<Inner>> {
    // SAFETY: the slot is a pointer-sized variable of a live view, read on
    // the main thread.
    let raw = unsafe { slot(view)?.read() }.cast_const().cast::<Inner>();
    if raw.is_null() {
        return None;
    }
    // SAFETY: a non-null slot holds a pointer from `Rc::into_raw` whose
    // reference the view still owns; another one is taken here.
    unsafe {
        Rc::increment_strong_count(raw);
        Some(Rc::from_raw(raw))
    }
}

/// Takes the view's reference to the shared state away from it, so that no
/// method finds it again.
pub(super) fn take_state(view: &NSView) -> Option<Rc<Inner>> {
    let slot = slot(view)?;
    // SAFETY: as in `state`.
    let raw = unsafe { slot.read() }.cast_const().cast::<Inner>();
    if raw.is_null() {
        return None;
    }
    // SAFETY: the slot is cleared, so the view's reference is taken back
    // once.
    unsafe {
        slot.write(ptr::null_mut());
        Some(Rc::from_raw(raw))
    }
}

/// Runs `method` for a view that serves a window; answers `otherwise` for
/// one that does not, or if it panics.
fn serve<R: Copy>(
    view: &NSView,
    otherwise: R,
    method: impl FnOnce(&Inner, &MacPlatform, &NSView) -> R,
) -> R {
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        let inner = state(view)?;
        let view = view.retain();
        let Platform::MacOs(platform) = &inner.platform else {
            return None;
        };
        Some(method(&inner, platform, &view))
    }));
    match result {
        Ok(answer) => answer.unwrap_or(otherwise),
        Err(payload) => {
            drop_quietly(payload);
            otherwise
        }
    }
}

/// Whether the view is its window's first responder.
pub(super) fn is_first_responder(view: &NSView) -> bool {
    view.window()
        .and_then(|window| window.firstResponder())
        .is_some_and(|responder| Retained::as_ptr(&responder).addr() == ptr::from_ref(view).addr())
}

/// Does what `NSView` does with `event`: hands it to the next responder.
pub(super) fn to_next_responder(view: &NSView, method: Method, event: &NSEvent) {
    // SAFETY: each is a method of NSResponder that takes the event and
    // returns nothing, sent to the superclass of the view's own class.
    unsafe {
        match method {
            Method::KeyDown => {
                let _: () = msg_send![super(view, NSView::class()), keyDown: event];
            }
            Method::KeyUp => {
                let _: () = msg_send![super(view, NSView::class()), keyUp: event];
            }
            Method::FlagsChanged => {
                let _: () = msg_send![super(view, NSView::class()), flagsChanged: event];
            }
            Method::ScrollWheel => {
                let _: () = msg_send![super(view, NSView::class()), scrollWheel: event];
            }
        }
    }
}

extern "C-unwind" fn yes(_: &NSView, _: Sel) -> Bool {
    Bool::YES
}

extern "C-unwind" fn no(_: &NSView, _: Sel) -> Bool {
    Bool::NO
}

// Drawing.

extern "C-unwind" fn draw_rect(view: &NSView, _: Sel, _dirty: CGRect) {
    serve(view, (), |inner, _, view| {
        let Some(context) = NSGraphicsContext::currentContext() else {
            return;
        };
        let context = context.CGContext();
        let Ok(back) = inner.back().try_borrow() else {
            return;
        };
        let Some(image) = image(&back) else { return };
        CGContext::set_interpolation_quality(Some(&context), CGInterpolationQuality::None);
        CGContext::draw_image(Some(&context), view.bounds(), Some(&image));
    });
}

extern "C-unwind" fn update_layer(view: &NSView, _: Sel) {
    serve(view, (), |inner, platform, view| {
        if platform.updating.replace(true) {
            return;
        }
        inner.paint_request();
        platform.updating.set(false);
        if !platform.open.get() {
            return;
        }
        show(inner, platform, view);
        // What the handler invalidated while it painted needs another
        // display, which AppKit may not give a request made during this one.
        if !inner.pending().is_empty() {
            // SAFETY: a constant string Foundation exports, never written.
            let modes = NSArray::from_slice(&[unsafe { NSRunLoopCommonModes }]);
            let nothing: Option<&AnyObject> = None;
            // SAFETY: the view declares `mangueRedraw`, which takes nothing;
            // the run loop keeps the view until it has sent it.
            unsafe {
                let _: () = msg_send![
                    view,
                    performSelector: sel!(mangueRedraw),
                    withObject: nothing,
                    afterDelay: 0.0f64,
                    inModes: &*modes
                ];
            }
        }
    });
}

/// Gives the view's layer an image of the back buffer.
fn show(inner: &Inner, platform: &MacPlatform, view: &NSView) {
    let Some(layer) = view.layer() else { return };
    let Ok(back) = inner.back().try_borrow() else {
        return;
    };
    let image = image(&back);
    let contents = image.as_deref().map(|image| {
        // SAFETY: a Core Graphics image is an object a layer's contents
        // takes, and every Core Foundation object may be used as `id`.
        unsafe { &*ptr::from_ref(image).cast::<AnyObject>() }
    });
    layer.setContentsScale(platform.backing.get());
    layer.setOpaque(true);
    // SAFETY: the contents are an image or nothing, and the layer keeps a
    // reference of its own.
    unsafe { layer.setContents(contents) };
}

extern "C-unwind" fn redraw(view: &NSView, _: Sel) {
    serve(view, (), |_, _, view| view.setNeedsDisplay(true));
}

// Timers.

extern "C-unwind" fn timer(view: &NSView, _: Sel, timer: &NSTimer) {
    serve(view, (), |inner, _, _| {
        let id = timer.userInfo().and_then(|info| {
            info.downcast_ref::<NSNumber>()
                .map(NSNumber::unsignedIntValue)
        });
        if let Some(id) = id {
            inner.input(RawInput::Timer(TimerId(id)));
        }
    });
}

// Size, scale and life.

/// Takes the window's backing scale factor.
fn take_scale(inner: &Inner, platform: &MacPlatform, window: &NSWindow) {
    let scale = window.backingScaleFactor();
    if scale.is_finite() && scale > 0.0 {
        platform.backing.set(scale);
        inner.on_system_scale(scale);
    }
}

/// Reports the view's size in device pixels.
fn follow_frame(inner: &Inner, platform: &MacPlatform, view: &NSView) {
    let size = view.frame().size;
    inner.on_size(cocoa::device_size(
        size.width,
        size.height,
        platform.backing.get(),
    ));
}

extern "C-unwind" fn set_frame_size(view: &NSView, _: Sel, size: CGSize) {
    // SAFETY: the superclass's method of the same selector and arguments.
    unsafe {
        let _: () = msg_send![super(view, NSView::class()), setFrameSize: size];
    }
    serve(view, (), |inner, platform, view| {
        if platform.attached.get() {
            follow_frame(inner, platform, view);
            if platform.open.get() {
                view.setNeedsDisplay(true);
            }
        }
    });
}

extern "C-unwind" fn backing_changed(view: &NSView, _: Sel) {
    // SAFETY: the superclass's method of the same selector, with none.
    unsafe {
        let _: () = msg_send![super(view, NSView::class()), viewDidChangeBackingProperties];
    }
    serve(view, (), |inner, platform, view| {
        let Some(window) = view.window() else { return };
        take_scale(inner, platform, &window);
        if platform.attached.get() {
            follow_frame(inner, platform, view);
            if platform.open.get() {
                view.setNeedsDisplay(true);
            }
        }
    });
}

extern "C-unwind" fn will_move_to_window(view: &NSView, _: Sel, to: Option<&NSWindow>) {
    serve(view, (), |inner, platform, view| {
        if !platform.attached.get() {
            return;
        }
        let Some(from) = view.window() else { return };
        if to.is_some_and(|to| ptr::eq(to, &*from)) {
            return;
        }
        // The window keeps neither the keyboard nor the pointer for a view
        // that leaves it.
        if is_first_responder(view) {
            from.makeFirstResponder(None);
        }
        inner.input(RawInput::CaptureLost);
    });
}

extern "C-unwind" fn did_move_to_superview(view: &NSView, _: Sel) {
    serve(view, (), |inner, platform, view| {
        // SAFETY: called by AppKit on the main thread as the superview
        // changes; the reference is only looked at here.
        if platform.attached.get() && unsafe { view.superview() }.is_none() {
            platform.silence(view);
            inner.on_destroyed();
        }
    });
}

extern "C-unwind" fn did_hide(view: &NSView, _: Sel) {
    serve(view, (), |inner, _, _| {
        inner.input(RawInput::CaptureLost);
    });
}

// The keyboard's focus.

extern "C-unwind" fn accepts_first_responder(view: &NSView, _: Sel) -> Bool {
    serve(view, Bool::NO, |inner, _, _| {
        Bool::new(inner.focus_on_click())
    })
}

extern "C-unwind" fn become_first_responder(view: &NSView, _: Sel) -> Bool {
    serve(view, Bool::YES, |inner, _, _| {
        inner.input(RawInput::Focus(true));
        Bool::YES
    })
}

extern "C-unwind" fn resign_first_responder(view: &NSView, _: Sel) -> Bool {
    serve(view, Bool::YES, |inner, platform, _| {
        platform.composing.set(0);
        inner.input(RawInput::Focus(false));
        Bool::YES
    })
}

// The pointer.

extern "C-unwind" fn accepts_first_mouse(_: &NSView, _: Sel, _: Option<&NSEvent>) -> Bool {
    Bool::YES
}

/// The device pixel the pointer of `event` is in.
fn location(platform: &MacPlatform, view: &NSView, event: &NSEvent) -> PhysicalPoint {
    let mut at = event.locationInWindow();
    // An event of no window gives its location on the screen.
    if event.windowNumber() == 0 {
        if let Some(window) = view.window() {
            at = window.convertPointFromScreen(at);
        }
    }
    let at = view.convertPoint_fromView(at, None);
    let bounds = view.bounds();
    cocoa::device_point(
        at.x - bounds.origin.x,
        at.y - bounds.origin.y,
        bounds.size.height,
        platform.backing.get(),
    )
}

/// The button of a press or a release, if it is one a window reports.
fn button(event: &NSEvent) -> Option<Button> {
    match event.buttonNumber() {
        0 => Some(Button::Primary),
        1 => Some(Button::Secondary),
        2 => Some(Button::Middle),
        _ => None,
    }
}

/// An event's time in milliseconds since the system started, wrapping.
fn time_ms(event: &NSEvent) -> u32 {
    // A time that is not a positive number becomes 0.
    (event.timestamp() * 1000.0) as u64 as u32
}

extern "C-unwind" fn button_down(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        if let Some(button) = button(event) {
            inner.input(RawInput::PointerDown {
                at: location(platform, view, event),
                button,
                modifiers: cocoa::modifiers(event.modifierFlags().0),
                time_ms: time_ms(event),
            });
        }
    });
}

extern "C-unwind" fn button_up(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        if let Some(button) = button(event) {
            inner.input(RawInput::PointerUp {
                at: location(platform, view, event),
                button,
                modifiers: cocoa::modifiers(event.modifierFlags().0),
            });
        }
    });
}

extern "C-unwind" fn dragged(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        inner.input(RawInput::PointerMove {
            at: location(platform, view, event),
            modifiers: cocoa::modifiers(event.modifierFlags().0),
        });
    });
}

extern "C-unwind" fn moved(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        let at = location(platform, view, event);
        // A window that wants moves sends them to its first responder from
        // anywhere in it.
        if inner
            .size()
            .bounds()
            .contains(i64::from(at.x), i64::from(at.y))
        {
            inner.input(RawInput::PointerMove {
                at,
                modifiers: cocoa::modifiers(event.modifierFlags().0),
            });
        }
    });
}

extern "C-unwind" fn exited(view: &NSView, _: Sel, _: &NSEvent) {
    serve(view, (), |inner, _, _| {
        inner.input(RawInput::PointerLeft);
    });
}

/// The origin of input made from the kept event `number`.
fn origin(number: usize) -> Option<Origin> {
    Some(Origin {
        message: 0,
        wparam: number,
        lparam: 0,
    })
}

/// Nothing more will be made from the kept event `number`.
fn close(platform: &MacPlatform, number: usize) {
    let unused = platform.pass_on.borrow_mut().close(number);
    if let Some((method, event)) = unused {
        platform.pass(method, &event);
    }
}

extern "C-unwind" fn scroll_wheel(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        if platform.passing.get() {
            return;
        }
        let mut wheel = platform.wheel.get();
        let (delta_x, delta_y) = wheel.turn(
            event.scrollingDeltaX(),
            event.scrollingDeltaY(),
            event.hasPreciseScrollingDeltas(),
        );
        platform.wheel.set(wheel);
        if delta_x == 0 && delta_y == 0 {
            // The start or the end of a gesture: the host's, as the handler
            // is told nothing of it.
            platform.pass(Method::ScrollWheel, event);
            return;
        }
        let at = location(platform, view, event);
        let number = {
            let mut pass_on = platform.pass_on.borrow_mut();
            let number = pass_on.keep((Method::ScrollWheel, event.retain()));
            pass_on.expect(number, 1);
            number
        };
        inner.input_from(
            RawInput::Wheel {
                at,
                delta_x,
                delta_y,
                modifiers: cocoa::modifiers(event.modifierFlags().0),
            },
            origin(number),
        );
        close(platform, number);
    });
}

// The keyboard.

/// The key of a key event, as the window reports it.
fn key_event(event: &NSEvent, pressed: bool) -> KeyEvent {
    // SAFETY: every object answers `respondsToSelector:`.
    let can_ask: bool =
        unsafe { msg_send![event, respondsToSelector: sel!(charactersByApplyingModifiers:)] };
    // What the key types with no modifier held (macOS 10.15 and later);
    // before that, with every modifier but Shift left out.
    let characters = if can_ask {
        event.charactersByApplyingModifiers(NSEventModifierFlags(0))
    } else {
        event.charactersIgnoringModifiers()
    };
    let unmodified = characters.and_then(|text| text.to_string().chars().next());
    KeyEvent {
        key: cocoa::key(event.keyCode(), unmodified),
        modifiers: cocoa::modifiers(event.modifierFlags().0),
        pressed,
    }
}

/// What tells one key press from another.
fn press_of(event: &NSEvent) -> (u64, u16) {
    (event.timestamp().to_bits(), event.keyCode())
}

fn has_command(event: &NSEvent) -> bool {
    event.modifierFlags().0 & cocoa::FLAG_COMMAND != 0
}

extern "C-unwind" fn key_down(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, view| {
        if platform.passing.get() {
            return;
        }
        if platform.offered.take() == Some(press_of(event)) {
            // The handler declined it as a Command combination, and so did
            // the menus.
            platform.pass(Method::KeyDown, event);
            return;
        }
        let composing = platform.composing.get() > 0;
        let number = platform
            .pass_on
            .borrow_mut()
            .keep((Method::KeyDown, event.retain()));
        if composing {
            // The key is the input method's.
            platform.pass_on.borrow_mut().used(number);
        } else {
            let key = key_event(event, true);
            let repeat = event.isARepeat();
            platform.pass_on.borrow_mut().expect(number, 1);
            inner.input_from(RawInput::Key { key, repeat }, origin(number));
        }
        if platform.open.get() && !has_command(event) {
            let before = platform.typing.replace(Some(number));
            // Calls `insertText:replacementRange:` and its siblings.
            view.interpretKeyEvents(&NSArray::from_slice(&[event]));
            platform.typing.set(before);
        }
        if platform.composing.get() > 0 {
            platform.pass_on.borrow_mut().used(number);
        }
        close(platform, number);
    });
}

extern "C-unwind" fn key_up(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, _| {
        if platform.passing.get() {
            return;
        }
        let number = {
            let mut pass_on = platform.pass_on.borrow_mut();
            let number = pass_on.keep((Method::KeyUp, event.retain()));
            pass_on.expect(number, 1);
            number
        };
        let key = key_event(event, false);
        inner.input_from(RawInput::Key { key, repeat: false }, origin(number));
        close(platform, number);
    });
}

extern "C-unwind" fn flags_changed(view: &NSView, _: Sel, event: &NSEvent) {
    serve(view, (), |inner, platform, _| {
        if platform.passing.get() {
            return;
        }
        let Some(flag) = cocoa::modifier_flag(event.keyCode()) else {
            platform.pass(Method::FlagsChanged, event);
            return;
        };
        let flags = event.modifierFlags().0;
        let key = KeyEvent {
            key: cocoa::key(event.keyCode(), None),
            modifiers: cocoa::modifiers(flags),
            pressed: flags & flag != 0,
        };
        let number = {
            let mut pass_on = platform.pass_on.borrow_mut();
            let number = pass_on.keep((Method::FlagsChanged, event.retain()));
            pass_on.expect(number, 1);
            number
        };
        inner.input_from(RawInput::Key { key, repeat: false }, origin(number));
        close(platform, number);
    });
}

extern "C-unwind" fn key_equivalent(view: &NSView, _: Sel, event: &NSEvent) -> Bool {
    let used = serve(view, false, |inner, platform, view| {
        // A handler that is running cannot answer now: the system's own
        // order then applies, and the key comes as a key down. A press
        // offered already is not offered twice.
        if platform.passing.get()
            || platform.offered.get() == Some(press_of(event))
            || event.r#type() != NSEventType::KeyDown
            || !has_command(event)
            || !is_first_responder(view)
            || inner.dispatcher().busy()
        {
            return false;
        }
        platform.offered.set(Some(press_of(event)));
        let key = key_event(event, true);
        let repeat = event.isARepeat();
        inner.input(RawInput::Key { key, repeat }) == Some(Handled::Yes)
    });
    if used {
        return Bool::YES;
    }
    // SAFETY: the superclass's method of the same selector and argument.
    unsafe { msg_send![super(view, NSView::class()), performKeyEquivalent: event] }
}

// Text input.

/// The text of what the input system passes: a string, plain or attributed.
fn plain(string: &AnyObject) -> Option<String> {
    match string.downcast_ref::<NSAttributedString>() {
        Some(attributed) => Some(attributed.string().to_string()),
        None => string.downcast_ref::<NSString>().map(NSString::to_string),
    }
}

extern "C-unwind" fn insert_text(view: &NSView, _: Sel, string: &AnyObject, _: NSRange) {
    serve(view, (), |inner, platform, _| {
        platform.composing.set(0);
        let Some(text) = plain(string) else { return };
        let units = cocoa::typed_units(&text);
        let characters = char::decode_utf16(units.iter().copied()).count();
        let from = platform.typing.get();
        if let Some(number) = from {
            platform.pass_on.borrow_mut().expect(number, characters);
        }
        for unit in units {
            inner.input_from(RawInput::Utf16(unit), from.and_then(origin));
        }
    });
}

/// Commands the input system makes from keys: the keys themselves have
/// already been reported, and doing nothing here keeps AppKit from beeping.
extern "C-unwind" fn do_command(_: &NSView, _: Sel, _: Sel) {}

extern "C-unwind" fn set_marked_text(
    view: &NSView,
    _: Sel,
    string: &AnyObject,
    _: NSRange,
    _: NSRange,
) {
    serve(view, (), |_, platform, _| {
        let length = plain(string).map_or(0, |text| text.encode_utf16().count());
        platform.composing.set(length);
    });
}

extern "C-unwind" fn unmark_text(view: &NSView, _: Sel) {
    serve(view, (), |_, platform, _| platform.composing.set(0));
}

extern "C-unwind" fn selected_range(_: &NSView, _: Sel) -> NSRange {
    NSRange {
        location: NOT_FOUND,
        length: 0,
    }
}

extern "C-unwind" fn marked_range(view: &NSView, _: Sel) -> NSRange {
    let length = serve(view, 0, |_, platform, _| platform.composing.get());
    NSRange {
        location: if length == 0 { NOT_FOUND } else { 0 },
        length,
    }
}

extern "C-unwind" fn has_marked_text(view: &NSView, _: Sel) -> Bool {
    serve(view, Bool::NO, |_, platform, _| {
        Bool::new(platform.composing.get() > 0)
    })
}

/// The window keeps no text of its own to hand back.
extern "C-unwind" fn substring(
    _: &NSView,
    _: Sel,
    _: NSRange,
    _: *mut NSRange,
) -> *mut NSAttributedString {
    ptr::null_mut()
}

extern "C-unwind" fn marked_attributes(_: &NSView, _: Sel) -> *mut NSArray<NSString> {
    Retained::autorelease_return(NSArray::from_slice(&[]))
}

/// Where the text caret is on the screen, for the input method's windows.
extern "C-unwind" fn first_rect(view: &NSView, _: Sel, _: NSRange, _: *mut NSRange) -> CGRect {
    let nowhere = CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(0.0, 0.0));
    serve(view, nowhere, |_, platform, view| {
        let Some(window) = view.window() else {
            return nowhere;
        };
        let caret = platform.caret.get().unwrap_or_default();
        let bounds = view.bounds();
        let [x, y, width, height] =
            cocoa::view_rect(caret, bounds.size.height, platform.backing.get());
        let rect = CGRect::new(
            CGPoint::new(bounds.origin.x + x, bounds.origin.y + y),
            CGSize::new(width, height),
        );
        window.convertRectToScreen(view.convertRect_toView(rect, None))
    })
}

extern "C-unwind" fn character_index(_: &NSView, _: Sel, _: CGPoint) -> usize {
    NOT_FOUND
}
