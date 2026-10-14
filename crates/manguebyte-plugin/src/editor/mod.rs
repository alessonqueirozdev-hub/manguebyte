//! The contract between a plugin's editor and whatever shows it.
//!
//! A plugin offers an editor by returning an [`EditorSpec`] from
//! [`Plugin::editor`](crate::Plugin::editor). A format wrapper (CLAP, VST 3)
//! or a standalone program uses the spec to open the editor inside a window
//! the host gives it, as a [`ParentWindow`], and from then on drives the
//! open editor through [`OpenEditor`].
//!
//! The two sides talk through a pair of types. The wrapper implements
//! [`EditorHost`] and attaches an [`EditorContext`] to it; the editor gets
//! the context, and the wrapper keeps the [`HostLink`] that came with it.
//! Through the context the editor reports a person's edits as
//! [`Gesture`]s, asks for a new size, registers timers and file descriptors
//! on the host's loop, and wakes itself on the main thread from other
//! threads. The context keeps the promises plugin formats need, whatever
//! the editor does: gestures are well formed, nothing it registers outlives
//! it, and the host is never called after the link is dropped.
//!
//! The contract names no toolkit and has no dependency beyond the
//! framework's own crates. The interface crates implement it, and an author
//! who prefers another toolkit can implement it with that.
//!
//! # Threads and lifetimes
//!
//! - [`Plugin::editor`](crate::Plugin::editor) is called once, when the
//!   plugin is created, right after `params`, before any audio runs. The
//!   spec it returns is `Send + Sync + 'static`, so it can hold `Arc`s of
//!   what it shares with the plugin (parameters, meter readings) and
//!   nothing that points at the plugin itself: the compiler refuses a
//!   borrow of the plugin, an `Rc`, a `Cell` or a raw pointer (see
//!   [`EditorSpec`]). The editor never sees the plugin again.
//! - Everything else here runs on the host's main thread, except
//!   [`MainThreadWaker`], which other threads use to get back there. An
//!   open editor is not `Send`, and it is closed by dropping it, then its
//!   [`HostLink`]. A host may open and close it any number of times while
//!   audio runs; the audio thread is not told and does not care.
//! - The audio thread touches nothing in this module except
//!   [`GestureReader`], which is there for a CLAP wrapper to turn
//!   [`GestureCounters`] into gesture events from `process` or `flush`.
//! - No call goes back into an editor while the editor is calling out: a
//!   wrapper never calls an [`OpenEditor`] method, nor drops the editor,
//!   from inside one of its own [`EditorHost`] methods.
//!
//! # Parameters
//!
//! An editor reads parameters straight from their atomics, polling them on
//! its own timer, as ADR 0003 prescribes; there are no listeners. It
//! changes them only through the host, with a [`Gesture`]: begin when a
//! control is taken hold of, values while it moves, end when it is let go,
//! and cancel to put the starting value back. What a wrapper does with
//! those calls depends on the format, and the editor does not need to know.
//!
//! # Sizes and scale
//!
//! Sizes are [`HostSize`]s: device pixels for Win32 and X11 parents, points
//! for Cocoa ones. A scale given to an editor is always finite and within
//! [`MIN_SCALE`]`..=`[`MAX_SCALE`]; wrappers refuse anything else with
//! [`checked_scale`].
//!
//! # Example
//!
//! A plugin with an editor, opened the way a wrapper opens it, with a
//! [`RecordingEditorHost`](testing::RecordingEditorHost) in place of a host.
//! The editor here draws nothing; a real one would come from a toolkit.
//!
//! ```
//! use std::rc::Rc;
//! use std::sync::Arc;
//! # use manguebyte_core::AudioBuffer;
//! # use manguebyte_dsp::{ProcessContext, ProcessSpec};
//! use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
//! use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
//! use manguebyte_plugin::editor::{
//!     EditorContext, EditorError, EditorSpec, Gesture, HostSize, Key, KeyEvent, Modifiers,
//!     OpenEditor, ParentWindow,
//! };
//! use manguebyte_plugin::Plugin;
//! # use manguebyte_plugin::{AudioLayout, PluginInfo, PluginKind};
//!
//! struct GainParams { gain: FloatParam }
//! param_set!(GainParams { gain });
//!
//! struct Gain { params: Arc<GainParams> }
//!
//! impl Plugin for Gain {
//! #   const INFO: PluginInfo = PluginInfo {
//! #       id: "org.example.gain", name: "Gain", vendor: "Example", version: "1.0.0",
//! #       description: "Turns things up and down", kind: PluginKind::Effect,
//! #   };
//! #   const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
//! #   type Params = GainParams;
//! #   fn new() -> Self {
//! #       let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
//! #       Self { params: Arc::new(GainParams { gain }) }
//! #   }
//! #   fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
//! #   fn prepare(&mut self, _: &ProcessSpec) {}
//! #   fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
//!     // INFO, LAYOUT, Params, new, params, prepare and process as usual.
//!
//!     fn editor(&self) -> Option<Box<dyn EditorSpec>> {
//!         // Only shared handles go in.
//!         Some(Box::new(GainEditor { params: Arc::clone(&self.params) }))
//!     }
//! }
//!
//! struct GainEditor { params: Arc<GainParams> }
//!
//! impl EditorSpec for GainEditor {
//!     fn size(&self, scale: f64) -> HostSize {
//!         HostSize::new(300, 170).scale_by(scale)
//!     }
//!
//!     fn open(
//!         &self,
//!         _parent: ParentWindow,
//!         scale: Option<f64>,
//!         context: EditorContext,
//!     ) -> Result<Box<dyn OpenEditor>, EditorError> {
//!         let size = self.size(scale.unwrap_or(1.0));
//!         Ok(Box::new(OpenGain { params: Arc::clone(&self.params), context, size }))
//!     }
//! }
//!
//! struct OpenGain { params: Arc<GainParams>, context: EditorContext, size: HostSize }
//!
//! impl OpenEditor for OpenGain {
//!     fn size(&self) -> HostSize { self.size }
//!     fn fit(&self, _wanted: HostSize) -> HostSize { self.size }
//!     fn set_size(&mut self, size: HostSize) -> bool { size == self.size }
//!     fn set_scale(&mut self, _scale: f64) -> bool { false }
//!     fn set_visible(&mut self, _visible: bool) {}
//!
//!     fn key(&mut self, event: KeyEvent) -> bool {
//!         // Arrow up raises the gain by a tenth of its range, as one edit.
//!         if !(event.pressed && event.key == Key::Up) {
//!             return false;
//!         }
//!         let gain = &self.params.gain;
//!         Gesture::set_once(&self.context, gain.param_id(), gain.normalized() + 0.1);
//!         true
//!     }
//! }
//!
//! // What a wrapper does when the plugin is created...
//! let plugin = Gain::new();
//! let params = plugin.params();
//! let spec = plugin.editor().expect("Gain has an editor");
//! assert_eq!(spec.size(1.5), HostSize::new(450, 255));
//!
//! // ...and when the host opens the editor. This host stores values into
//! // the parameters, as a standalone program does.
//! let recorder = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
//! let (context, link) = EditorContext::attach(recorder.clone());
//! let parent = ParentWindow::win32(0x1F00).expect("not a null handle");
//! let mut editor = spec.open(parent, Some(1.5), context).expect("opens");
//!
//! let up = KeyEvent { key: Key::Up, modifiers: Modifiers::NONE, pressed: true };
//! assert!(editor.key(up));
//! let gain = params.gain.param_id();
//! assert_eq!(recorder.take_calls(), [
//!     HostCall::BeginGesture(gain),
//!     HostCall::Perform(gain, 0.6),
//!     HostCall::EndGesture(gain),
//! ]);
//! assert_eq!(params.gain.value(), 1.2);
//!
//! // Closing is dropping the editor, then its link.
//! drop(editor);
//! drop(link);
//! ```

mod context;
mod counters;
mod gesture;
mod key;
mod size;
pub mod testing;

use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};
use std::rc::Rc;

use manguebyte_core::ParamId;

pub use context::{EditorContext, HostLink, LoopHandle, MainThreadWaker};
pub use counters::{GestureCounters, GestureEdges, GestureReader};
pub use gesture::{Gesture, OpenGestures};
pub use key::{Key, KeyEvent, Modifiers};
pub use size::{HostSize, Resize, ResizeHints};

/// The smallest scale an editor is given.
pub const MIN_SCALE: f64 = 0.25;

/// The largest scale an editor is given.
pub const MAX_SCALE: f64 = 8.0;

/// `scale` if an editor may be given it: finite and within
/// [`MIN_SCALE`]`..=`[`MAX_SCALE`].
///
/// Hosts pass scales they read from the system or make up; a wrapper checks
/// each one with this and refuses the rest, so an editor never has to
/// handle a zero, negative, infinite or absurd scale.
///
/// ```
/// use manguebyte_plugin::editor::checked_scale;
///
/// assert_eq!(checked_scale(1.5), Some(1.5));
/// assert_eq!(checked_scale(0.0), None);
/// assert_eq!(checked_scale(f64::NAN), None);
/// assert_eq!(checked_scale(1e9), None);
/// ```
pub fn checked_scale(scale: f64) -> Option<f64> {
    (MIN_SCALE..=MAX_SCALE).contains(&scale).then_some(scale)
}

/// How to open a plugin's editor, any number of times.
///
/// A wrapper takes the spec from [`Plugin::editor`](crate::Plugin::editor)
/// once, keeps it for the life of the plugin instance, and calls it on the
/// host's main thread.
///
/// # What a spec may hold
///
/// The spec lives on the main thread while the plugin runs on the audio
/// thread, so it must be `Send + Sync + 'static`. It may hold `Arc`s of what
/// it shares with the plugin, such as the parameters, and nothing that
/// points at the plugin. A toolkit-style spec built from a closure that
/// shares the parameters compiles:
///
/// ```
/// # use std::sync::Arc;
/// # use manguebyte_core::AudioBuffer;
/// # use manguebyte_dsp::{ProcessContext, ProcessSpec};
/// # use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// # use manguebyte_plugin::{AudioLayout, Plugin, PluginInfo, PluginKind};
/// # struct GainParams { gain: FloatParam }
/// # param_set!(GainParams { gain });
/// # struct Gain { params: Arc<GainParams> }
/// /// What a toolkit might offer: a spec made from a closure.
/// struct FromFn<F>(F);
///
/// impl<F: Fn(f64) -> HostSize + Send + Sync + 'static> EditorSpec for FromFn<F> {
///     fn size(&self, scale: f64) -> HostSize { (self.0)(scale) }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
///
/// impl Plugin for Gain {
/// #   const INFO: PluginInfo = PluginInfo {
/// #       id: "org.example.gain", name: "Gain", vendor: "Example", version: "1.0.0",
/// #       description: "", kind: PluginKind::Effect,
/// #   };
/// #   const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
/// #   type Params = GainParams;
/// #   fn new() -> Self {
/// #       let gain = FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 2.0), 1.0);
/// #       Self { params: Arc::new(GainParams { gain }) }
/// #   }
/// #   fn params(&self) -> Arc<GainParams> { Arc::clone(&self.params) }
/// #   fn prepare(&mut self, _: &ProcessSpec) {}
/// #   fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
///     fn editor(&self) -> Option<Box<dyn EditorSpec>> {
///         let params = Arc::clone(&self.params);
///         Some(Box::new(FromFn(move |scale| {
///             let wide = params.gain.value() > 1.0;
///             HostSize::new(if wide { 400 } else { 300 }, 170).scale_by(scale)
///         })))
///     }
/// }
/// # assert_eq!(Gain::new().editor().unwrap().size(2.0), HostSize::new(600, 340));
/// ```
///
/// The trait itself refuses the rest, wherever the spec is used. A spec
/// that borrows the plugin is refused because it is not `'static`:
///
/// ```compile_fail
/// # // No error code here: rustc reports this one as E0477 or as E0478
/// # // depending on its version. With `'static` removed from the trait,
/// # // this example compiles.
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// struct Gain { wide: bool }
///
/// struct Spec<'a> { plugin: &'a Gain }
///
/// impl EditorSpec for Spec<'_> {
///     fn size(&self, scale: f64) -> HostSize {
///         HostSize::new(if self.plugin.wide { 400 } else { 300 }, 170).scale_by(scale)
///     }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
/// ```
///
/// A spec holding an `Rc`, which is neither `Send` nor `Sync`:
///
/// ```compile_fail,E0277
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// use std::rc::Rc;
///
/// struct Spec { wide: Rc<bool> }
///
/// impl EditorSpec for Spec {
///     fn size(&self, scale: f64) -> HostSize {
///         HostSize::new(if *self.wide { 400 } else { 300 }, 170).scale_by(scale)
///     }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
/// ```
///
/// A spec holding a `Cell`, which is `Send` but not `Sync`, so that two
/// threads could change it at once:
///
/// ```compile_fail,E0277
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// use std::cell::Cell;
///
/// struct Spec { width: Cell<u32> }
///
/// impl EditorSpec for Spec {
///     fn size(&self, scale: f64) -> HostSize {
///         HostSize::new(self.width.get(), 170).scale_by(scale)
///     }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
/// ```
///
/// A spec holding a lock guard, which is `Sync` but not `Send`: a mutex
/// must be unlocked on the thread that locked it, and the spec is dropped
/// wherever the plugin instance is:
///
/// ```compile_fail,E0277
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// use std::sync::MutexGuard;
///
/// struct Spec { width: MutexGuard<'static, u32> }
///
/// impl EditorSpec for Spec {
///     fn size(&self, scale: f64) -> HostSize {
///         HostSize::new(*self.width, 170).scale_by(scale)
///     }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
/// ```
///
/// And a spec holding a raw pointer to the plugin, which is neither `Send`
/// nor `Sync`; the pointer is refused before it could ever be
/// dereferenced:
///
/// ```compile_fail,E0277
/// # use manguebyte_plugin::editor::{EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow};
/// struct Gain;
///
/// struct Spec { plugin: *const Gain }
///
/// impl EditorSpec for Spec {
///     fn size(&self, scale: f64) -> HostSize {
///         let width = if self.plugin.is_null() { 0 } else { 300 };
///         HostSize::new(width, 170).scale_by(scale)
///     }
/// #   fn open(&self, parent: ParentWindow, _: Option<f64>, _: EditorContext)
/// #       -> Result<Box<dyn OpenEditor>, EditorError> { Err(EditorError::Unsupported(parent.api())) }
/// }
/// ```
pub trait EditorSpec: Send + Sync + 'static {
    /// The size the editor opens at, at `scale`.
    ///
    /// On Windows and X11 the size is in device pixels and grows with the
    /// scale; on macOS it is in points and the wrapper passes 1. When the
    /// host has not said what scale to use, the wrapper passes 1 and opens
    /// the editor with no scale; once the editor knows its own scale it
    /// asks for its size with [`EditorContext::request_resize`], which it
    /// may do from inside [`open`](Self::open).
    fn size(&self, scale: f64) -> HostSize;

    /// How the editor may be resized, with limits at scale 1. Fixed unless
    /// overridden.
    fn resize(&self) -> Resize {
        Resize::Fixed
    }

    /// Whether the editor can live in a window of this kind. All kinds
    /// unless overridden; a wrapper asks before it offers an editor to a
    /// host.
    fn supports(&self, api: WindowApi) -> bool {
        let _ = api;
        true
    }

    /// Opens the editor inside `parent`, ready to be shown.
    ///
    /// `scale` is the host's scale if it gave one (always accepted by
    /// [`checked_scale`]), and `None` if the editor should ask the system.
    /// `context` is how the editor reaches the host for as long as it is
    /// open.
    ///
    /// The editor is closed by dropping what this returns. Dropping it
    /// should leave nothing behind: no window, no timer, no watch on a
    /// descriptor, and no gesture left open. [`Gesture`]s and
    /// [`LoopHandle`]s see to the last three themselves, and the wrapper's
    /// [`HostLink`] ends whatever an editor leaks.
    fn open(
        &self,
        parent: ParentWindow,
        scale: Option<f64>,
        context: EditorContext,
    ) -> Result<Box<dyn OpenEditor>, EditorError>;
}

/// An editor on screen, inside a host's window.
///
/// It lives on the host's main thread and is not `Send`. Dropping it closes
/// it. A wrapper calls these methods when the host asks for the matching
/// thing. None of them is called after the editor is dropped, because
/// nothing is left to call, and none is called while the editor is inside
/// a call to its [`EditorContext`] or a [`Gesture`]: what the host does
/// during such a call reaches the editor after it has returned, and a size
/// the host sets during [`EditorContext::request_resize`] is that call's
/// answer.
///
/// ```compile_fail,E0277
/// use manguebyte_plugin::editor::OpenEditor;
///
/// fn close_on_another_thread(editor: Box<dyn OpenEditor>) {
///     std::thread::spawn(move || drop(editor));
/// }
/// ```
pub trait OpenEditor {
    /// The editor's size now.
    fn size(&self) -> HostSize;

    /// The size nearest to `wanted` that the editor accepts. It changes
    /// nothing; wrappers answer CLAP's `adjust_size` and VST 3's
    /// `checkSizeConstraint` with it.
    ///
    /// Usually `resize.scaled(scale).fit(self.size(), wanted)`, with the
    /// rules from [`EditorSpec::resize`] and the scale in use; see
    /// [`Resize::fit`].
    fn fit(&self, wanted: HostSize) -> HostSize;

    /// Takes `size`, which the host has made the window. Returns false, and
    /// keeps the size it has, if `size` is not one [`fit`](Self::fit) would
    /// give.
    fn set_size(&mut self, size: HostSize) -> bool;

    /// Uses `scale`, which the host has decided on, from now on. Returns
    /// false if the editor cannot and will keep choosing its own scale. If
    /// its size changes as a result, the editor asks for the new one with
    /// [`EditorContext::request_resize`] and takes the size that returns.
    fn set_scale(&mut self, scale: f64) -> bool;

    /// Shows or hides the editor. An editor that is hidden does no work.
    fn set_visible(&mut self, visible: bool);

    /// A key that the host delivered itself, as VST 3 hosts may. Returns
    /// true if the editor used it, and false to leave it to the host. Not
    /// used unless overridden.
    fn key(&mut self, event: KeyEvent) -> bool {
        let _ = event;
        false
    }

    /// The host has called back on its main thread after a
    /// [`MainThreadWaker`] woke it. Several wakes may lead to one call, and
    /// a call may come when nothing was asked for, so the editor checks
    /// what there is to do. Nothing unless overridden.
    fn on_main_thread(&mut self) {}
}

/// What a wrapper implements so that an editor can reach the host.
///
/// The editor never sees this trait. It holds an [`EditorContext`] made by
/// [`EditorContext::attach`], which calls these methods for it, on the
/// thread that attached it, and promises the implementation that:
///
/// - gesture calls are well formed on each parameter: begins and ends
///   alternate, starting with a begin; values come only between them, and
///   are finite and within `0..=1`; every begin is followed by its end,
///   even if the editor panics, is closed in the middle of an edit or
///   leaks a [`Gesture`], at the latest when the [`HostLink`] is dropped;
///   and gestures that overlap on one parameter arrive as one;
/// - every timer and watch the editor registered on the
///   [`host_loop`](Self::host_loop) is cancelled by the time the link has
///   been dropped;
/// - nothing is called once the link has been dropped, and the function
///   from [`main_thread_callback`](Self::main_thread_callback) has been
///   dropped by then too.
///
/// In return an implementation:
///
/// - never calls into the editor ([`OpenEditor`]), nor drops it, from
///   inside one of these methods. Hosts do answer synchronously: a VST 3
///   host calls `onSize` from inside `resizeView`, and a CLAP host may call
///   `set_size` from inside `request_resize`. The size learned that way is
///   what [`request_resize`](Self::request_resize) returns; anything else
///   is held and passed to the editor after the call has returned.
/// - does not panic: these methods run from destructors, including while
///   unwinding.
/// - drops the link before anything it points at goes away, such as the
///   plugin instance or the host's own objects.
///
/// A wrapper reports exactly the gesture calls it is given and never
/// infers an edit by comparing values, which would send host automation and
/// restored state back to the host as if a person had made them. Under
/// VST 3, while the host is delivering audio blocks, its automation points
/// stay the only source of a parameter's value, as the VST 3 wrapper
/// already ensures for values set on the controller.
pub trait EditorHost {
    /// A person has started to change `param`.
    fn begin_gesture(&self, param: ParamId);

    /// The person has set `param` to `normalized`, a position in `0..=1`.
    fn perform(&self, param: ParamId, normalized: f64);

    /// The person has finished changing `param`.
    fn end_gesture(&self, param: ParamId);

    /// The editor has changed many parameters without gestures, such as by
    /// loading a preset of its own. The host should read every value again.
    fn values_changed(&self);

    /// Asks the host to make the editor's window `size`.
    ///
    /// Returns the size the window has after the request, which is `size`
    /// when the host simply agrees, or `None` if the host refused. If the
    /// host sets the size before this returns, that size is the answer, and
    /// it is not passed to [`OpenEditor::set_size`].
    fn request_resize(&self, size: HostSize) -> Option<HostSize>;

    /// A function that any thread except the audio thread may call to ask
    /// the host for a call on its main thread, or `None` if the host offers
    /// no way to ask. When the host calls back, the wrapper calls
    /// [`OpenEditor::on_main_thread`].
    ///
    /// Under CLAP the function calls `clap_host.request_callback`, and the
    /// plugin's `on_main_thread` answers; under VST 3 on Linux it signals
    /// an event handler registered with `IRunLoop`. The function only asks:
    /// it never calls into the editor itself. It is asked for once per
    /// [`EditorContext::attach`], and dropped when the link is.
    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>>;

    /// The host's own loop, for timers and file descriptors: `Some` where
    /// the host offers either (CLAP `timer-support` and `posix-fd-support`,
    /// VST 3 `IRunLoop`), which is usual on Linux and rare elsewhere.
    fn host_loop(&self) -> Option<Rc<dyn HostLoop>>;

    /// The editor cannot go on: its window was destroyed under it, or it
    /// failed. The wrapper tells the host, and drops the editor later, not
    /// from inside this call.
    fn closed(&self);
}

/// A host's event loop, as a wrapper offers it.
///
/// Editors do not call this directly: they register through
/// [`EditorContext::every`] and [`EditorContext::watch_fd`], which return
/// [`LoopHandle`]s that cancel on drop. Callbacks run on the host's main
/// thread.
pub trait HostLoop {
    /// Calls `ready` whenever `fd` can be read, until the returned token is
    /// cancelled. `None` if the host refused.
    fn watch_fd(&self, fd: i32, ready: Box<dyn FnMut()>) -> Option<LoopToken>;

    /// Calls `tick` about every `period_ms` milliseconds, until the
    /// returned token is cancelled. `None` if the host refused. Hosts may
    /// tick more slowly than asked; CLAP promises only about 30 Hz.
    fn every(&self, period_ms: u32, tick: Box<dyn FnMut()>) -> Option<LoopToken>;

    /// Stops a watch or a timer. Its callback is dropped before this
    /// returns, or, if this is called from inside that very callback, as
    /// soon as the callback returns. A token that is unknown or already
    /// cancelled is ignored.
    fn cancel(&self, token: LoopToken);
}

/// A timer or a watch registered with a [`HostLoop`]. The loop chooses the
/// number and never gives the same one twice.
///
/// ```
/// use manguebyte_plugin::editor::LoopToken;
///
/// let token = LoopToken::new(7);
/// assert_eq!(token.get(), 7);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LoopToken(u64);

impl LoopToken {
    /// The token numbered `number`, for a [`HostLoop`] to hand out.
    pub const fn new(number: u64) -> Self {
        Self(number)
    }

    /// The token's number.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The kinds of window an editor can be put inside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WindowApi {
    /// Windows.
    Win32,
    /// macOS.
    Cocoa,
    /// The X Window System.
    X11,
}

/// The window a host puts an editor inside, as plain numbers.
///
/// Holding numbers rather than pointers keeps the type `Send`, `Eq` and
/// free of `unsafe`; only the window code that embeds the editor turns them
/// back into handles. A handle is never zero: the constructors refuse null
/// handles, so a wrapper can pass on whatever a host gives it.
///
/// ```
/// use manguebyte_plugin::editor::{ParentWindow, WindowApi};
///
/// let parent = ParentWindow::win32(0x0004_05A2).expect("a real handle");
/// assert_eq!(parent.api(), WindowApi::Win32);
/// assert_eq!(ParentWindow::win32(0), None);
/// // CLAP passes X11 windows as `unsigned long`; ids are 32-bit values.
/// assert_eq!(ParentWindow::x11(1 << 40), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ParentWindow {
    /// A Win32 window.
    Win32 {
        /// The `HWND`, as a number.
        hwnd: NonZeroUsize,
    },
    /// An AppKit view.
    Cocoa {
        /// The address of the `NSView`.
        ns_view: NonZeroUsize,
    },
    /// An X11 window.
    X11 {
        /// The window's id.
        window: NonZeroU32,
    },
}

impl ParentWindow {
    /// A Win32 window from its `HWND`, or `None` if the handle is null.
    pub fn win32(hwnd: usize) -> Option<Self> {
        NonZeroUsize::new(hwnd).map(|hwnd| Self::Win32 { hwnd })
    }

    /// An AppKit view from its address, or `None` if the address is null.
    pub fn cocoa(ns_view: usize) -> Option<Self> {
        NonZeroUsize::new(ns_view).map(|ns_view| Self::Cocoa { ns_view })
    }

    /// An X11 window from its id, or `None` if the id is zero or does not
    /// fit in 32 bits, which no X11 id does.
    pub fn x11(window: u64) -> Option<Self> {
        u32::try_from(window)
            .ok()
            .and_then(NonZeroU32::new)
            .map(|window| Self::X11 { window })
    }

    /// The kind of window this is.
    pub fn api(&self) -> WindowApi {
        match self {
            Self::Win32 { .. } => WindowApi::Win32,
            Self::Cocoa { .. } => WindowApi::Cocoa,
            Self::X11 { .. } => WindowApi::X11,
        }
    }
}

/// Why an editor did not open.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EditorError {
    /// The editor cannot live in a window of this kind.
    Unsupported(WindowApi),
    /// The editor needs the host's loop and the host offers none (see
    /// [`EditorContext::has_host_loop`]).
    NoHostLoop,
    /// The system refused, for the reason given.
    Platform(String),
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(api) => {
                let parent = match api {
                    WindowApi::Win32 => "a Win32 window",
                    WindowApi::Cocoa => "an AppKit view",
                    WindowApi::X11 => "an X11 window",
                };
                write!(f, "the editor cannot be put inside {parent}")
            }
            Self::NoHostLoop => f.write_str(
                "the editor needs the host's timers or file descriptors, and the host offers neither",
            ),
            Self::Platform(reason) => write!(f, "the system refused to open the editor: {reason}"),
        }
    }
}

impl std::error::Error for EditorError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scales_outside_the_range_are_refused() {
        for accepted in [MIN_SCALE, 1.0, 1.25, 2.0, MAX_SCALE] {
            assert_eq!(checked_scale(accepted), Some(accepted));
        }
        for refused in [
            0.0,
            -0.0,
            -1.0,
            0.249,
            8.001,
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MIN_POSITIVE,
            f64::MAX,
        ] {
            assert_eq!(checked_scale(refused), None, "{refused}");
        }
    }

    #[test]
    fn parent_windows_refuse_null_and_oversized_handles() {
        assert_eq!(ParentWindow::win32(0), None);
        assert_eq!(ParentWindow::cocoa(0), None);
        assert_eq!(ParentWindow::x11(0), None);
        assert_eq!(ParentWindow::x11(u64::from(u32::MAX) + 1), None);
        assert_eq!(
            ParentWindow::x11(u64::from(u32::MAX)).map(|parent| parent.api()),
            Some(WindowApi::X11)
        );
        assert_eq!(
            ParentWindow::win32(usize::MAX),
            NonZeroUsize::new(usize::MAX).map(|hwnd| ParentWindow::Win32 { hwnd })
        );
        assert_eq!(
            ParentWindow::cocoa(8).map(|parent| parent.api()),
            Some(WindowApi::Cocoa)
        );
    }

    #[test]
    fn errors_say_what_went_wrong() {
        assert_eq!(
            EditorError::Unsupported(WindowApi::X11).to_string(),
            "the editor cannot be put inside an X11 window"
        );
        assert_eq!(
            EditorError::Platform("no more window handles".into()).to_string(),
            "the system refused to open the editor: no more window handles"
        );
        assert!(EditorError::NoHostLoop.to_string().contains("neither"));
    }
}
