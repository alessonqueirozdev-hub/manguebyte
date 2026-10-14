//! The plugin's editor as a VST 3 `IPlugView`, and the host's side of the
//! editor contract.
//!
//! A plugin whose [`Plugin::editor`](manguebyte_plugin::Plugin::editor)
//! returns a spec answers `createView("editor")` with a [`View`]: an
//! `IPlugView` with `IPlugViewContentScaleSupport`. The host attaches it to
//! a window of its own and the view opens the editor inside, passing the
//! handle on as a [`ParentWindow`]; `removed` closes it again. A view may be
//! attached and removed any number of times, and the host may make several.
//!
//! [`ViewHost`] is what the open editor reaches the host through:
//!
//! - its gestures become `beginEdit`, `performEdit` and `endEdit` on the
//!   host's `IComponentHandler` ([`Handler`]), with the value the control
//!   set, which also becomes what the controller reports (see the wrapper's
//!   module documentation for when it is stored into the parameter);
//! - values it changed without gestures make the host read every value
//!   again (`restartComponent` with `kParamValuesChanged`);
//! - its resize requests go to the host's `IPlugFrame::resizeView`;
//! - its timers and descriptors go to the host's `IRunLoop`, which a host
//!   on Linux offers through its frame ([`RunLoop`]), and so does its way
//!   back to the main thread from other threads, on systems with Unix
//!   sockets.
//!
//! VST 3 gives a plugin no way to tell the host its editor failed. An editor
//! that says so ([`EditorHost::closed`]) is dropped the next time the host
//! calls its view, and, where the host's loop is there, as soon as the loop
//! calls back.
//!
//! # Lifetimes
//!
//! A view does not keep the component alive, and may outlive it. When the
//! component is terminated or goes away, every editor it has open is closed
//! first, then the handler is released; the views stay valid, refuse to be
//! attached again, and answer everything else without an editor.
//!
//! # Calls that come back
//!
//! Hosts answer some calls by calling the plugin before they return: a host
//! calls `onSize` from inside `resizeView`, and may call `removed`, release
//! the view or terminate the component from inside `performEdit`, or run its
//! own loop. The editor contract forbids calling into an editor, or dropping
//! it, while the editor is calling out, so [`Busy`] notes every call out in
//! progress, and while one is:
//!
//! - `onSize` from inside `resizeView` is that request's answer;
//! - `getSize` and `checkSizeConstraint` are answered without the editor:
//!   with the size being asked for, or the size last learned, and from the
//!   spec's [`Resize`](manguebyte_plugin::editor::Resize) rules;
//! - `removed` is carried out as soon as nothing is in progress: at the
//!   start or the end of the next call from the host into the view;
//! - timers and descriptors do not fire, a wake is asked for again when the
//!   call returns, and any other call that would reach the editor returns
//!   `kResultFalse`.
//!
//! The same holds while the editor is being opened or closed, while one of
//! its methods runs, and while one of its timers or descriptors fires.
//!
//! A view released, or a component terminated, from inside such a call cuts
//! the editor off at once: its timers and descriptors are unregistered, its
//! gestures ended, its waker silenced, and nothing calls the host for it
//! again. A view that is still there drops the editor as soon as nothing is
//! in progress. A view released while the editor is calling out from its own
//! window leaks the editor instead: it cannot be dropped under its own
//! frame, and nothing calls it again.
//!
//! Main thread only, as VST 3 requires of every call here.

// See the note at the top of `wrapper.rs`.
#![allow(clippy::unnecessary_cast)]

use std::cell::{Cell, RefCell};
use std::ffi::{CStr, c_void};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use manguebyte_core::ParamId;
use manguebyte_plugin::editor::{
    EditorContext, EditorHost, EditorSpec, HostLink, HostLoop, HostSize, Key, KeyEvent, Modifiers,
    OpenEditor, ParentWindow, Resize, WindowApi, checked_scale,
};
use vst3::Steinberg::Linux::IRunLoop;
use vst3::Steinberg::Vst::RestartFlags_::kParamValuesChanged;
use vst3::Steinberg::{
    self as sb, FIDString, IPlugFrame, IPlugFrameTrait, IPlugView, IPlugViewContentScaleSupport,
    IPlugViewContentScaleSupportTrait, IPlugViewTrait, TBool, ViewRect, char16, int16,
    kInvalidArgument, kResultFalse, kResultOk, kResultTrue, tresult,
};
use vst3::{Class, ComPtr, ComRef, ComWrapper};

use crate::handler::Handler;
use crate::run_loop::{RunLoop, Wake};
use crate::wrapper::{Controls, guard};

// --- What is in progress -----------------------------------------------------

/// What is in progress on the main thread for one view, and what the wrapper
/// knows without asking the editor, shared by the [`ViewState`], its
/// [`ViewHost`] and its [`RunLoop`].
#[derive(Default)]
pub(crate) struct Busy {
    /// Calls the editor made to the host that have not returned.
    calling_out: Cell<u32>,
    /// Editor methods, and the editor's timer and descriptor callbacks, that
    /// have not returned.
    in_editor: Cell<u32>,
    /// The editor is being opened.
    opening: Cell<u32>,
    /// The editor is being closed.
    closing: Cell<u32>,
    /// `resizeView` calls waiting for the host.
    resizing: Cell<u32>,
    /// The size the innermost `resizeView` asks for.
    requested: Cell<Option<HostSize>>,
    /// The size the host set from inside the innermost `resizeView`.
    resize_answer: Cell<Option<HostSize>>,
    /// How many resize requests the host has granted.
    granted: Cell<u64>,
    /// The editor's size as last learned: from the editor itself, a size it
    /// took, or a request the host granted. `None` when not known.
    size: Cell<Option<HostSize>>,
    /// The host removed the view when the editor could not be dropped.
    remove_pending: Cell<bool>,
    /// The editor said it cannot go on, or was cut off, when it could not
    /// be dropped.
    close_pending: Cell<bool>,
    /// The host's loop called back while the editor could not be reached;
    /// another call is asked for when it can be.
    wake_owed: Cell<bool>,
    /// Asks the host's loop for a call back, while an editor is open on a
    /// host that has a loop.
    resignal: RefCell<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Busy {
    /// Whether the editor cannot be called, opened, closed or dropped now.
    pub(crate) fn is_busy(&self) -> bool {
        self.calling_out.get() > 0
            || self.in_editor.get() > 0
            || self.opening.get() > 0
            || self.closing.get() > 0
    }

    /// Notes a call out of the editor until the guard is dropped.
    pub(crate) fn calling_out(&self) -> Mark<'_> {
        Mark::new(self, &self.calling_out)
    }

    /// Notes editor code running until the guard is dropped.
    pub(crate) fn in_editor(&self) -> Mark<'_> {
        Mark::new(self, &self.in_editor)
    }

    /// Asks for the wake that could not be delivered, once nothing is in
    /// progress.
    fn after(&self) {
        if self.is_busy() || !self.wake_owed.get() {
            return;
        }
        let signal = self.resignal.borrow().clone();
        if let Some(signal) = signal {
            self.wake_owed.set(false);
            signal();
        }
    }
}

/// Counts something in progress until dropped, even while unwinding.
pub(crate) struct Mark<'a> {
    busy: &'a Busy,
    counter: &'a Cell<u32>,
}

impl<'a> Mark<'a> {
    fn new(busy: &'a Busy, counter: &'a Cell<u32>) -> Self {
        counter.set(counter.get().saturating_add(1));
        Self { busy, counter }
    }
}

impl Drop for Mark<'_> {
    fn drop(&mut self) {
        self.counter.set(self.counter.get().saturating_sub(1));
        self.busy.after();
    }
}

/// A `resizeView` waiting for the host, with room for the size the host
/// sets before it returns.
struct Resizing<'a> {
    busy: &'a Busy,
    /// What a request this one is nested in asked for and was answered, put
    /// back afterwards.
    outer_requested: Option<HostSize>,
    outer_answer: Option<HostSize>,
}

impl<'a> Resizing<'a> {
    fn new(busy: &'a Busy, size: HostSize) -> Self {
        busy.resizing.set(busy.resizing.get().saturating_add(1));
        Self {
            busy,
            outer_requested: busy.requested.replace(Some(size)),
            outer_answer: busy.resize_answer.take(),
        }
    }

    fn answer(&self) -> Option<HostSize> {
        self.busy.resize_answer.get()
    }
}

impl Drop for Resizing<'_> {
    fn drop(&mut self) {
        let busy = self.busy;
        busy.resizing.set(busy.resizing.get().saturating_sub(1));
        busy.requested.set(self.outer_requested);
        busy.resize_answer.set(self.outer_answer);
    }
}

// --- The plugin's editors ----------------------------------------------------

/// What every view of one plugin instance shares: the spec, the controller's
/// values, the host's handler, and the views themselves, so that the
/// component can close their editors.
pub(crate) struct Editors {
    spec: Box<dyn EditorSpec>,
    controls: Arc<Controls>,
    handler: Rc<Handler>,
    views: RefCell<Vec<Weak<ViewState>>>,
    /// Set by `terminate` and when the component goes away: no editor opens
    /// until the component is initialized again.
    shut: Cell<bool>,
}

impl Editors {
    /// The editors of a new plugin instance, opened from `spec`.
    pub(crate) fn new(
        spec: Box<dyn EditorSpec>,
        controls: Arc<Controls>,
        handler: Rc<Handler>,
    ) -> Rc<Self> {
        Rc::new(Self {
            spec,
            controls,
            handler,
            views: RefCell::new(Vec::new()),
            shut: Cell::new(false),
        })
    }

    /// A new view for `createView`, with one reference that the caller
    /// owns, or null if `name` is not `"editor"` or the component is shut.
    ///
    /// # Safety
    ///
    /// `name` must be null or a NUL-terminated string.
    pub(crate) unsafe fn create_view(self: &Rc<Self>, name: FIDString) -> *mut IPlugView {
        if name.is_null() || self.shut.get() {
            return std::ptr::null_mut();
        }
        // SAFETY: per this function's contract; and a constant of the
        // bindings, a literal ending in a zero.
        let (name, editor) = unsafe { (CStr::from_ptr(name), CStr::from_ptr(sb::Vst::ViewType::kEditor)) };
        if name != editor {
            return std::ptr::null_mut();
        }
        let state = Rc::new(ViewState::new(Rc::clone(self)));
        {
            let mut views = self.views.borrow_mut();
            views.retain(|view| view.strong_count() > 0);
            views.push(Rc::downgrade(&state));
        }
        let view = ComWrapper::new(View {
            state: Rc::clone(&state),
        });
        let Some(pointer) = view.to_com_ptr::<IPlugView>() else {
            return std::ptr::null_mut();
        };
        state.this.set(pointer.as_ptr());
        pointer.into_raw()
    }

    /// Closes every open editor, or cuts it off if it cannot be dropped
    /// now, and refuses new ones until [`reopen`](Self::reopen).
    pub(crate) fn shutdown(&self) {
        self.shut.set(true);
        let views: Vec<Rc<ViewState>> = self
            .views
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for view in views {
            view.cut_off();
        }
    }

    /// Accepts new editors again, after `initialize`.
    pub(crate) fn reopen(&self) {
        self.shut.set(false);
    }
}

// --- The editor's host -------------------------------------------------------

/// What an editor reaches a VST 3 host through. Made for each editor opened.
pub(crate) struct ViewHost {
    view: Weak<ViewState>,
    editors: Rc<Editors>,
    busy: Rc<Busy>,
    run_loop: Option<Rc<RunLoop>>,
    wake: Option<Wake>,
}

impl ViewHost {
    /// The function that asks the host's loop for a call back, if there is
    /// a way.
    fn signal(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        self.wake.as_ref().map(Wake::signal)
    }

    /// Unregisters what the editor's link did not: the wake, and anything
    /// left on the host's loop. Called once the link has been dropped.
    fn release(&self) {
        if let Some(wake) = &self.wake {
            wake.release();
        }
        if let Some(run_loop) = &self.run_loop {
            run_loop.cancel_all();
        }
    }
}

impl EditorHost for ViewHost {
    fn begin_gesture(&self, param: ParamId) {
        let _out = self.busy.calling_out();
        let controls = &self.editors.controls;
        if let Some(id) = controls
            .index_of_param(param)
            .and_then(|index| controls.vst_id(index))
        {
            self.editors.handler.begin_edit(id);
        }
    }

    fn perform(&self, param: ParamId, normalized: f64) {
        let _out = self.busy.calling_out();
        let controls = &self.editors.controls;
        let Some(index) = controls.index_of_param(param) else {
            return;
        };
        // Reported by the controller at once, and stored into the
        // parameter at once only while no blocks run; while they run, the
        // host's echo in `process` carries the value to the plugin.
        controls.set_normalized(index, normalized);
        if let Some(id) = controls.vst_id(index) {
            self.editors.handler.perform_edit(id, normalized);
        }
    }

    fn end_gesture(&self, param: ParamId) {
        let _out = self.busy.calling_out();
        let controls = &self.editors.controls;
        if let Some(id) = controls
            .index_of_param(param)
            .and_then(|index| controls.vst_id(index))
        {
            self.editors.handler.end_edit(id);
        }
    }

    fn values_changed(&self) {
        let _out = self.busy.calling_out();
        self.editors.controls.show_current_values();
        self.editors.handler.restart(kParamValuesChanged);
    }

    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        let view = self.view.upgrade()?;
        let frame = view.frame.borrow().clone()?;
        let this = view.this.get();
        if this.is_null() {
            return None;
        }
        let busy = &*self.busy;
        let _out = busy.calling_out();
        let resizing = Resizing::new(busy, size);
        let mut rect = rect_at(0, 0, size);
        // SAFETY: the host's own frame, kept alive by our reference, on the
        // main thread; our own view, which is alive (`this` is cleared when
        // it goes); and a rectangle that lives for the call.
        let result = unsafe { frame.resizeView(this, &raw mut rect) };
        // A size the host set before returning is what the window has,
        // whatever it returned.
        let answer = resizing
            .answer()
            .or((result == kResultTrue).then_some(size));
        drop(resizing);
        if answer.is_some() {
            busy.size.set(answer);
            busy.granted.set(busy.granted.get().wrapping_add(1));
        }
        answer
    }

    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        let signal = self.signal()?;
        Some(Box::new(move || signal()))
    }

    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        self.run_loop
            .clone()
            .map(|run_loop| run_loop as Rc<dyn HostLoop>)
    }

    fn closed(&self) {
        if let Some(view) = self.view.upgrade() {
            // VST 3 has no way to tell the host; the editor is dropped the
            // next time the host calls the view, or the loop calls back.
            view.busy.close_pending.set(true);
            if let Some(signal) = self.signal() {
                signal();
            }
        }
    }
}

// --- The view ----------------------------------------------------------------

/// The window API a VST 3 platform type names, or `None`.
///
/// # Safety
///
/// `kind` must be null or a NUL-terminated string.
unsafe fn api_of(kind: FIDString) -> Option<WindowApi> {
    if kind.is_null() {
        return None;
    }
    // SAFETY: per this function's contract.
    let kind = unsafe { CStr::from_ptr(kind) };
    [
        (sb::kPlatformTypeHWND, WindowApi::Win32),
        (sb::kPlatformTypeNSView, WindowApi::Cocoa),
        (sb::kPlatformTypeX11EmbedWindowID, WindowApi::X11),
    ]
    .into_iter()
    // SAFETY: constants of the bindings, literals ending in a zero.
    .find(|&(name, _)| unsafe { CStr::from_ptr(name) } == kind)
    .map(|(_, api)| api)
}

/// The window a host passes to `attached`, as a number. VST 3 passes an X11
/// window's id in place of the pointer.
fn parent_window(api: WindowApi, parent: usize) -> Option<ParentWindow> {
    match api {
        WindowApi::Win32 => ParentWindow::win32(parent),
        WindowApi::Cocoa => ParentWindow::cocoa(parent),
        WindowApi::X11 => ParentWindow::x11(parent as u64),
        _ => None,
    }
}

/// The size of a rectangle a host passes, or `None` if it is null or inside
/// out.
///
/// # Safety
///
/// `rect` must be null or valid to read.
unsafe fn rect_size(rect: *const ViewRect) -> Option<HostSize> {
    // SAFETY: per this function's contract.
    let rect = unsafe { rect.as_ref() }?;
    let width = i64::from(rect.right) - i64::from(rect.left);
    let height = i64::from(rect.bottom) - i64::from(rect.top);
    Some(HostSize::new(
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
    ))
}

/// A rectangle of `size` with its top left corner at `left`, `top`. Sides
/// that do not fit in VST 3's coordinates end at the largest coordinate.
fn rect_at(left: i32, top: i32, size: HostSize) -> ViewRect {
    let end = |origin: i32, length: u32| {
        origin.saturating_add(i32::try_from(length).unwrap_or(i32::MAX))
    };
    ViewRect {
        left,
        top,
        right: end(left, size.width),
        bottom: end(top, size.height),
    }
}

/// A key as VST 3's `onKeyDown` and `onKeyUp` deliver it.
///
/// `character` is the UTF-16 unit the key types, or 0; `code` is one of
/// VST 3's virtual key codes, or 0; `modifiers` holds VST 3's modifier bits.
/// A key with a code of its own (arrows, Enter, function keys) is that key;
/// otherwise the character it types; otherwise, for the codes VST 3 gives
/// digits and capital letters, which start at `VKEY_FIRST_ASCII` with `'0'`,
/// that character, in lower case unless Shift is held. VST 3's command key
/// is Control on Windows and Linux and Command on macOS.
pub(crate) fn key_event(character: char16, code: int16, modifiers: int16, pressed: bool) -> KeyEvent {
    use sb::KeyModifier_ as bits;
    use sb::VirtualKeyCodes_ as codes;

    let bits_held = modifiers as u16 as u32;
    let has = |bit: u32| bits_held & bit != 0;
    let (command, control) = (has(bits::kCommandKey as u32), has(bits::kControlKey as u32));
    let modifiers = Modifiers {
        shift: has(bits::kShiftKey as u32),
        alt: has(bits::kAlternateKey as u32),
        // Windows and Linux hosts send Control as the command key.
        control: if cfg!(target_os = "macos") { control } else { command },
        meta: if cfg!(target_os = "macos") { command } else { control },
    };

    let code = code as u16 as u32;
    let named = match code {
        c if c == codes::KEY_BACK as u32 => Some(Key::Backspace),
        c if c == codes::KEY_TAB as u32 => Some(Key::Tab),
        c if c == codes::KEY_RETURN as u32 || c == codes::KEY_ENTER as u32 => Some(Key::Enter),
        c if c == codes::KEY_ESCAPE as u32 => Some(Key::Escape),
        c if c == codes::KEY_SPACE as u32 => Some(Key::Character(' ')),
        c if c == codes::KEY_END as u32 => Some(Key::End),
        c if c == codes::KEY_HOME as u32 => Some(Key::Home),
        c if c == codes::KEY_LEFT as u32 => Some(Key::Left),
        c if c == codes::KEY_UP as u32 => Some(Key::Up),
        c if c == codes::KEY_RIGHT as u32 => Some(Key::Right),
        c if c == codes::KEY_DOWN as u32 => Some(Key::Down),
        c if c == codes::KEY_PAGEUP as u32 => Some(Key::PageUp),
        c if c == codes::KEY_PAGEDOWN as u32 => Some(Key::PageDown),
        c if c == codes::KEY_INSERT as u32 => Some(Key::Insert),
        c if c == codes::KEY_DELETE as u32 => Some(Key::Delete),
        c if (codes::KEY_F1 as u32..=codes::KEY_F12 as u32).contains(&c) => {
            Some(Key::Function((c - codes::KEY_F1 as u32 + 1) as u8))
        }
        c if (codes::KEY_F13 as u32..=codes::KEY_F24 as u32).contains(&c) => {
            Some(Key::Function((c - codes::KEY_F13 as u32 + 13) as u8))
        }
        _ => None,
    };
    let typed = || {
        char::from_u32(u32::from(character)).filter(|c| *c != '\0' && !c.is_control())
    };
    let from_code = || {
        let first = codes::VKEY_FIRST_ASCII as u32;
        let ascii = code.checked_sub(first)? + u32::from(b'0');
        let c = char::from_u32(ascii).filter(|c| ('0'..='Z').contains(c) && c.is_ascii_alphanumeric())?;
        Some(if modifiers.shift { c } else { c.to_ascii_lowercase() })
    };
    let numpad = || {
        let digit = code.checked_sub(codes::KEY_NUMPAD0 as u32).filter(|&d| d < 10)?;
        char::from_digit(digit, 10)
    };
    let key = named
        .or_else(|| typed().map(Key::Character))
        .or_else(|| numpad().map(Key::Character))
        .or_else(|| from_code().map(Key::Character))
        .unwrap_or(Key::Other);
    KeyEvent {
        key,
        modifiers,
        pressed,
    }
}

/// One view of the plugin's editor, as the host sees it.
///
/// The COM object is a thin shell around a [`ViewState`], so that what the
/// editor's host refers to can stay alive for as long as a call needs it
/// even if the host releases the view meanwhile.
pub(crate) struct View {
    state: Rc<ViewState>,
}

impl Class for View {
    type Interfaces = (IPlugView, IPlugViewContentScaleSupport);
}

impl View {
    /// Runs `work` on the view's state, with anything that had to wait done
    /// before and after it. `fallback` is the answer if `work` panics.
    fn entry<R>(&self, fallback: R, work: impl FnOnce(&Rc<ViewState>) -> R) -> R {
        let state = Rc::clone(&self.state);
        guard(fallback, || {
            state.settle();
            let result = work(&state);
            state.settle();
            result
        })
    }
}

impl Drop for View {
    fn drop(&mut self) {
        guard((), || self.state.view_gone());
    }
}

/// A view's state on the main thread.
pub(crate) struct ViewState {
    editors: Rc<Editors>,
    /// This view as the host knows it, for `resizeView`; null once the view
    /// is gone.
    this: Cell<*mut IPlugView>,
    /// The host's frame, from `setFrame`.
    frame: RefCell<Option<Rc<ComPtr<IPlugFrame>>>>,
    /// The window API the view is attached with, from `attached` until
    /// `removed`.
    attached: Cell<Option<WindowApi>>,
    /// The host's content scale, if it gave one that was accepted.
    scale: Cell<Option<f64>>,
    /// A size the host set before the editor was open.
    preset: Cell<Option<HostSize>>,
    /// The open editor. Borrowed only while one of its methods runs.
    editor: RefCell<Option<Box<dyn OpenEditor>>>,
    /// The wrapper's end of the open editor's context.
    link: Cell<Option<HostLink>>,
    /// The open editor's host, for what is undone after the link.
    host: RefCell<Option<Rc<ViewHost>>>,
    busy: Rc<Busy>,
}

impl ViewState {
    fn new(editors: Rc<Editors>) -> Self {
        Self {
            editors,
            this: Cell::new(std::ptr::null_mut()),
            frame: RefCell::new(None),
            attached: Cell::new(None),
            scale: Cell::new(None),
            preset: Cell::new(None),
            editor: RefCell::new(None),
            link: Cell::new(None),
            host: RefCell::new(None),
            busy: Rc::default(),
        }
    }

    fn spec(&self) -> &dyn EditorSpec {
        &*self.editors.spec
    }

    /// Runs `work` on the open editor, if there is one and it can be called
    /// now.
    fn with_editor<R>(&self, work: impl FnOnce(&mut dyn OpenEditor) -> R) -> Option<R> {
        if self.busy.is_busy() {
            return None;
        }
        let mut slot = self.editor.try_borrow_mut().ok()?;
        let editor = slot.as_deref_mut()?;
        let _in = self.busy.in_editor();
        Some(work(editor))
    }

    fn is_open(&self) -> bool {
        self.editor
            .try_borrow()
            .map_or(true, |editor| editor.is_some())
    }

    fn has_link(&self) -> bool {
        let link = self.link.take();
        let present = link.is_some();
        self.link.set(link);
        present
    }

    /// The scale sizes are worked out at. Cocoa sizes are points, which the
    /// system scales.
    fn scale_or_one(&self) -> f64 {
        if self.attached.get() == Some(WindowApi::Cocoa) {
            return 1.0;
        }
        self.scale.get().unwrap_or(1.0)
    }

    /// The spec's resize rules at the scale in use.
    fn rules(&self) -> Resize {
        self.spec().resize().scaled(self.scale_or_one())
    }

    /// The size before the editor is open.
    fn size_before_open(&self) -> HostSize {
        self.preset
            .get()
            .unwrap_or_else(|| self.spec().size(self.scale_or_one()))
    }

    /// The editor's size while it cannot be asked: inside its own resize
    /// request, the size the host set or else the one asked for; otherwise
    /// the size last learned, if any.
    fn size_unasked(&self) -> Option<HostSize> {
        let busy = &self.busy;
        if busy.resizing.get() > 0 {
            if let Some(size) = busy.resize_answer.get().or(busy.requested.get()) {
                return Some(size);
            }
        }
        busy.size.get()
    }

    /// Carries out a `removed`, or drops an editor that cannot go on, once
    /// nothing is in progress. Every entry point calls this when it starts
    /// and before it returns to the host.
    fn settle(&self) {
        let pending = self.busy.remove_pending.get() || self.busy.close_pending.get();
        if pending && !self.busy.is_busy() {
            self.close();
        }
    }

    /// Drops the editor, then its link, then what the link left on the
    /// host's loop.
    fn close(&self) {
        if self.busy.closing.get() > 0 {
            return;
        }
        let _closing = Mark::new(&self.busy, &self.busy.closing);
        self.busy.remove_pending.set(false);
        self.busy.close_pending.set(false);
        self.busy.wake_owed.set(false);
        self.busy.size.set(None);
        self.preset.set(None);
        let signal = self.busy.resignal.borrow_mut().take();
        drop(signal);
        // Taken out first, so that nothing is borrowed while they drop. The
        // link is dropped after the editor, even if dropping the editor
        // panics: it ends whatever gestures, timers and watches the editor
        // left behind.
        let _release = ReleaseAfter(self.host.borrow_mut().take());
        let link = self.link.take();
        let editor = self
            .editor
            .try_borrow_mut()
            .ok()
            .and_then(|mut editor| editor.take());
        drop(editor);
        drop(link);
    }

    /// Cuts the editor off at once, for a component that is shutting down,
    /// and drops it as soon as it can be dropped.
    fn cut_off(&self) {
        if !self.busy.is_busy() {
            self.close();
            return;
        }
        let host = self.host.borrow_mut().take();
        let _release = ReleaseAfter(host);
        drop(self.link.take());
        self.busy.close_pending.set(true);
    }

    /// The view object is going away.
    fn view_gone(&self) {
        self.this.set(std::ptr::null_mut());
        self.attached.set(None);
        if self.busy.is_busy() {
            // Released from inside a call the editor is making: it is cut
            // off now, and leaked, because it cannot be dropped under its
            // own frame. Nothing calls it again.
            let host = self.host.borrow_mut().take();
            let _release = ReleaseAfter(host);
            drop(self.link.take());
            if let Ok(mut slot) = self.editor.try_borrow_mut() {
                std::mem::forget(slot.take());
            }
        } else {
            self.close();
        }
        let frame = self.frame.borrow_mut().take();
        drop(frame);
    }

    // --- What the host asks ---------------------------------------------------

    fn is_platform_type_supported(&self, api: Option<WindowApi>) -> tresult {
        match api {
            Some(api) if self.spec().supports(api) => kResultTrue,
            _ => kResultFalse,
        }
    }

    fn attach(self: &Rc<Self>, parent: usize, api: Option<WindowApi>) -> tresult {
        let Some(api) = api.filter(|&api| self.spec().supports(api)) else {
            return kResultFalse;
        };
        if self.attached.get().is_some()
            || self.editors.shut.get()
            || self.busy.is_busy()
            || self.is_open()
        {
            return kResultFalse;
        }
        let Some(parent) = parent_window(api, parent) else {
            return kInvalidArgument;
        };
        // Cocoa sizes are points: the editor opens without the host's scale.
        let scale = self.scale.get().filter(|_| api != WindowApi::Cocoa);

        // The host's loop, from the frame the host gave before `attached`.
        let frame = self.frame.borrow().clone();
        let run_loop = frame
            .and_then(|frame| frame.cast::<IRunLoop>())
            .map(|host| Rc::new(RunLoop::new(host, Rc::clone(&self.busy))));
        let wake = run_loop
            .as_ref()
            .and_then(|run_loop| Wake::new(run_loop, Rc::downgrade(self)));
        let host = Rc::new(ViewHost {
            view: Rc::downgrade(self),
            editors: Rc::clone(&self.editors),
            busy: Rc::clone(&self.busy),
            run_loop,
            wake,
        });
        *self.busy.resignal.borrow_mut() = host.signal();
        let (context, link) = EditorContext::attach(Rc::clone(&host) as Rc<dyn EditorHost>);
        // Kept from now on, where a shutdown can cut the editor off even
        // while it opens.
        self.link.set(Some(link));
        *self.host.borrow_mut() = Some(Rc::clone(&host));
        self.attached.set(Some(api));

        let granted = self.busy.granted.get();
        // The editor opens at the size the spec gives for its scale.
        let opening_size = self.spec().size(scale.unwrap_or(1.0));
        self.busy.size.set(Some(opening_size));
        let opened = {
            let _opening = Mark::new(&self.busy, &self.busy.opening);
            self.spec().open(parent, scale, context)
        };
        let Ok(editor) = opened else {
            // Ends whatever the editor registered before it gave up.
            self.attached.set(None);
            self.close();
            return kResultFalse;
        };
        *self.editor.borrow_mut() = Some(editor);
        if !self.has_link() {
            // Cut off while it opened: the component shut down.
            self.busy.close_pending.set(true);
        }
        // Removed, cut off or failed while it opened: closed now that it
        // is out of `open`.
        self.settle();
        if !self.is_open() || self.attached.get() != Some(api) {
            self.attached.set(None);
            return kResultFalse;
        }
        // A size the host set before there was a window, unless the host
        // granted the editor another while it opened: the host's latest
        // answer is the size of its window.
        let preset = self.preset.take();
        if let Some(preset) = preset.filter(|_| self.busy.granted.get() == granted) {
            self.apply_preset(preset, &host);
        }
        kResultOk
    }

    /// Gives a just-opened editor the size the host set before its window
    /// existed. If the editor refuses it, the host is asked to make its
    /// window the editor's size instead, so that the two agree.
    fn apply_preset(&self, preset: HostSize, host: &ViewHost) {
        let outcome = self.with_editor(|editor| {
            let size = editor.size();
            if size == preset || editor.set_size(preset) {
                Ok(preset)
            } else {
                Err(size)
            }
        });
        match outcome {
            Some(Ok(size)) => self.busy.size.set(Some(size)),
            Some(Err(size)) => {
                self.busy.size.set(Some(size));
                let answer = host.request_resize(size);
                if let Some(other) = answer.filter(|&answer| answer != size) {
                    // The host made its window yet another size: offered to
                    // the editor once.
                    if self.with_editor(|editor| editor.set_size(other)) == Some(true) {
                        self.busy.size.set(Some(other));
                    }
                }
            }
            None => {}
        }
    }

    fn remove(&self) -> tresult {
        if self.attached.take().is_none() {
            return kResultFalse;
        }
        if self.busy.is_busy() {
            // Not now: the host considers the view removed, and the editor
            // is dropped as soon as nothing is in progress.
            self.busy.remove_pending.set(true);
        } else {
            self.close();
        }
        kResultOk
    }

    fn size(&self) -> HostSize {
        if !self.is_open() && !self.busy.is_busy() {
            return self.size_before_open();
        }
        match self.with_editor(|editor| editor.size()) {
            Some(size) => {
                self.busy.size.set(Some(size));
                size
            }
            None => self
                .size_unasked()
                .unwrap_or_else(|| self.size_before_open()),
        }
    }

    fn on_size(&self, size: HostSize) -> tresult {
        if self.busy.resizing.get() > 0 {
            // The host answering a resize request: the editor takes this
            // size as the request's result.
            self.busy.resize_answer.set(Some(size));
            return kResultOk;
        }
        if self.is_open() {
            return match self.with_editor(|editor| editor.set_size(size)) {
                Some(true) => {
                    self.busy.size.set(Some(size));
                    kResultOk
                }
                _ => kResultFalse,
            };
        }
        if self.busy.is_busy() {
            return kResultFalse;
        }
        // No editor yet: kept for when there is one, if it is a size the
        // editor accepts.
        if self.rules().fit(self.size_before_open(), size) != size {
            return kResultFalse;
        }
        self.preset.set(Some(size));
        kResultOk
    }

    fn check_size(&self, wanted: HostSize) -> HostSize {
        if !self.is_open() && !self.busy.is_busy() {
            return self.rules().fit(self.size_before_open(), wanted);
        }
        if let Some(size) = self.with_editor(|editor| editor.fit(wanted)) {
            return size;
        }
        // The editor cannot be asked now, so the spec's rules answer.
        let current = self
            .size_unasked()
            .unwrap_or_else(|| self.size_before_open());
        self.rules().fit(current, wanted)
    }

    fn set_scale(&self, factor: f32) -> tresult {
        let Some(scale) = checked_scale(f64::from(factor)) else {
            return kResultFalse;
        };
        // Cocoa sizes are points, which the system scales.
        if self.attached.get() == Some(WindowApi::Cocoa) || self.busy.is_busy() {
            return kResultFalse;
        }
        if !self.is_open() {
            self.scale.set(Some(scale));
            // A size set at another scale no longer applies.
            self.preset.set(None);
            return kResultOk;
        }
        let granted = self.busy.granted.get();
        match self.with_editor(|editor| editor.set_scale(scale)) {
            Some(true) => {
                self.scale.set(Some(scale));
                if self.busy.granted.get() == granted {
                    // The size may have changed with the scale, without a
                    // request the host granted.
                    self.busy.size.set(None);
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }

    fn key(&self, event: KeyEvent) -> tresult {
        match self.with_editor(|editor| editor.key(event)) {
            Some(true) => kResultTrue,
            _ => kResultFalse,
        }
    }

    fn set_frame(&self, frame: *mut IPlugFrame) -> tresult {
        // SAFETY: the host's frame, valid for this call, or null. The copy
        // takes a reference of its own, released when the host sets another
        // frame or the view goes away.
        let frame = unsafe { ComRef::from_raw(frame) }.map(|frame| Rc::new(frame.to_com_ptr()));
        let old = self.frame.replace(frame);
        drop(old);
        kResultOk
    }

    /// The host's loop has called back after a wake.
    #[cfg(unix)]
    pub(crate) fn on_wake(&self) {
        self.settle();
        if self.busy.is_busy() {
            self.busy.wake_owed.set(true);
            return;
        }
        self.with_editor(|editor| editor.on_main_thread());
        self.settle();
    }
}

/// Undoes what an editor's host left on the host's loop when dropped, after
/// the link that owns the rest has been dropped.
struct ReleaseAfter(Option<Rc<ViewHost>>);

impl Drop for ReleaseAfter {
    fn drop(&mut self) {
        if let Some(host) = self.0.take() {
            host.release();
        }
    }
}

impl IPlugViewTrait for View {
    unsafe fn isPlatformTypeSupported(&self, kind: FIDString) -> tresult {
        // SAFETY: the host passes a NUL-terminated string, or null.
        let api = unsafe { api_of(kind) };
        self.entry(kResultFalse, |state| state.is_platform_type_supported(api))
    }

    unsafe fn attached(&self, parent: *mut c_void, kind: FIDString) -> tresult {
        // SAFETY: as above.
        let api = unsafe { api_of(kind) };
        self.entry(kResultFalse, |state| state.attach(parent.addr(), api))
    }

    unsafe fn removed(&self) -> tresult {
        self.entry(kResultFalse, |state| state.remove())
    }

    unsafe fn onWheel(&self, _distance: f32) -> tresult {
        // The editor's own window receives the wheel.
        kResultFalse
    }

    unsafe fn onKeyDown(&self, key: char16, code: int16, modifiers: int16) -> tresult {
        let event = key_event(key, code, modifiers, true);
        self.entry(kResultFalse, |state| state.key(event))
    }

    unsafe fn onKeyUp(&self, key: char16, code: int16, modifiers: int16) -> tresult {
        let event = key_event(key, code, modifiers, false);
        self.entry(kResultFalse, |state| state.key(event))
    }

    unsafe fn getSize(&self, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        self.entry(kResultFalse, |state| {
            let rect = rect_at(0, 0, state.size());
            // SAFETY: a rectangle the host owns, checked non-null above.
            unsafe { size.write(rect) };
            kResultOk
        })
    }

    unsafe fn onSize(&self, size: *mut ViewRect) -> tresult {
        // SAFETY: a rectangle the host owns, or null.
        let Some(size) = (unsafe { rect_size(size) }) else {
            return kInvalidArgument;
        };
        self.entry(kResultFalse, |state| state.on_size(size))
    }

    unsafe fn onFocus(&self, _state: TBool) -> tresult {
        self.entry(kResultFalse, |state| {
            if state.is_open() {
                kResultOk
            } else {
                kResultFalse
            }
        })
    }

    unsafe fn setFrame(&self, frame: *mut IPlugFrame) -> tresult {
        self.entry(kResultFalse, |state| state.set_frame(frame))
    }

    unsafe fn canResize(&self) -> tresult {
        self.entry(kResultFalse, |state| {
            if state.rules().can_resize() {
                kResultTrue
            } else {
                kResultFalse
            }
        })
    }

    unsafe fn checkSizeConstraint(&self, rect: *mut ViewRect) -> tresult {
        // SAFETY: a rectangle the host owns, or null.
        let Some(wanted) = (unsafe { rect_size(rect) }) else {
            return kInvalidArgument;
        };
        self.entry(kResultFalse, |state| {
            let size = state.check_size(wanted);
            // SAFETY: a rectangle the host owns, checked non-null by
            // `rect_size`; read before it is written.
            unsafe {
                let (left, top) = ((*rect).left, (*rect).top);
                rect.write(rect_at(left, top, size));
            }
            kResultTrue
        })
    }
}

impl IPlugViewContentScaleSupportTrait for View {
    unsafe fn setContentScaleFactor(&self, factor: f32) -> tresult {
        self.entry(kResultFalse, |state| state.set_scale(factor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use vst3::Steinberg::KeyModifier_ as bits;
    use vst3::Steinberg::VirtualKeyCodes_ as codes;

    fn key(character: char16, code: u32, modifiers: u32) -> KeyEvent {
        key_event(character, code as int16, modifiers as int16, true)
    }

    #[test]
    fn keys_with_codes_of_their_own_are_named() {
        assert_eq!(key(0, codes::KEY_UP as u32, 0).key, Key::Up);
        assert_eq!(key(0, codes::KEY_BACK as u32, 0).key, Key::Backspace);
        assert_eq!(key(13, codes::KEY_RETURN as u32, 0).key, Key::Enter);
        assert_eq!(key(0, codes::KEY_ENTER as u32, 0).key, Key::Enter);
        assert_eq!(key(27, codes::KEY_ESCAPE as u32, 0).key, Key::Escape);
        assert_eq!(key(0, codes::KEY_F1 as u32, 0).key, Key::Function(1));
        assert_eq!(key(0, codes::KEY_F12 as u32, 0).key, Key::Function(12));
        assert_eq!(key(0, codes::KEY_F13 as u32, 0).key, Key::Function(13));
        assert_eq!(key(0, codes::KEY_F24 as u32, 0).key, Key::Function(24));
        assert_eq!(key(0, codes::KEY_PAGEDOWN as u32, 0).key, Key::PageDown);
        assert_eq!(key(u16::from(b' '), codes::KEY_SPACE as u32, 0).key, Key::Character(' '));
        assert_eq!(key(0, codes::KEY_NUMPAD7 as u32, 0).key, Key::Character('7'));
        assert_eq!(key(0, codes::KEY_SHIFT as u32, 0).key, Key::Other);
        assert!(key(0, codes::KEY_UP as u32, 0).pressed);
        assert!(!key_event(0, codes::KEY_UP as int16, 0, false).pressed);
    }

    #[test]
    fn other_keys_are_the_characters_they_type() {
        assert_eq!(key(u16::from(b'a'), 0, 0).key, Key::Character('a'));
        assert_eq!(key(0x00E9, 0, 0).key, Key::Character('\u{E9}'));
        // A lone surrogate, a control character and nothing at all.
        assert_eq!(key(0xD800, 0, 0).key, Key::Other);
        assert_eq!(key(0x0007, 0, 0).key, Key::Other);
        assert_eq!(key(0, 0, 0).key, Key::Other);
        // Only a code: VST 3 numbers digits and capitals from
        // `VKEY_FIRST_ASCII`, starting at '0'.
        let first = codes::VKEY_FIRST_ASCII as u32;
        assert_eq!(key(0, first, 0).key, Key::Character('0'));
        let a = first + u32::from(b'A' - b'0');
        assert_eq!(key(0, a, 0).key, Key::Character('a'));
        assert_eq!(key(0, a, bits::kShiftKey as u32).key, Key::Character('A'));
        assert_eq!(key(0, first + 200, 0).key, Key::Other);
        // Codes that do not fit in the field come out as `Other`.
        assert_eq!(key_event(0, -1, 0, true).key, Key::Other);
    }

    #[test]
    fn modifiers_follow_the_platform() {
        let all = key(0, 0, 0xF).modifiers;
        assert!(all.shift && all.alt && all.control && all.meta);
        assert_eq!(key(0, 0, 0).modifiers, Modifiers::NONE);
        let command = key(0, 0, bits::kCommandKey as u32).modifiers;
        let control = key(0, 0, bits::kControlKey as u32).modifiers;
        if cfg!(target_os = "macos") {
            assert!(command.meta && !command.control);
            assert!(control.control && !control.meta);
        } else {
            assert!(command.control && !command.meta);
            assert!(control.meta && !control.control);
        }
        assert!(key(0, 0, bits::kShiftKey as u32).modifiers.shift);
        assert!(key(0, 0, bits::kAlternateKey as u32).modifiers.alt);
    }

    #[test]
    fn rectangles_convert_both_ways_and_refuse_inside_out_ones() {
        let rect = rect_at(10, 20, HostSize::new(300, 200));
        assert_eq!((rect.left, rect.top, rect.right, rect.bottom), (10, 20, 310, 220));
        // SAFETY: a rectangle on the stack.
        assert_eq!(unsafe { rect_size(&rect) }, Some(HostSize::new(300, 200)));
        let inside_out = ViewRect {
            left: 10,
            top: 0,
            right: 5,
            bottom: 5,
        };
        // SAFETY: as above.
        assert_eq!(unsafe { rect_size(&inside_out) }, None);
        // SAFETY: null is allowed.
        assert_eq!(unsafe { rect_size(std::ptr::null()) }, None);
        let widest = ViewRect {
            left: i32::MIN,
            top: i32::MIN,
            right: i32::MAX,
            bottom: i32::MAX,
        };
        // SAFETY: a rectangle on the stack.
        assert_eq!(unsafe { rect_size(&widest) }, Some(HostSize::new(u32::MAX, u32::MAX)));
        let huge = rect_at(i32::MAX - 5, 0, HostSize::new(u32::MAX, 1));
        assert_eq!((huge.right, huge.bottom), (i32::MAX, 1));
    }
}
