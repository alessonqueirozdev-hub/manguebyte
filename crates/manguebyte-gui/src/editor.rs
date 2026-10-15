//! The editor spec a plugin author writes: a size, resize rules and a
//! function that builds the view.

use std::fmt;
use std::sync::Arc;

use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow, Resize, WindowApi,
};
use manguebyte_ui::{IntoView, Theme, View};

use crate::open::{Backend, cap, open_with};

/// What builds the view each time the editor opens.
pub(crate) type BuildView = Arc<dyn Fn() -> View + Send + Sync>;

/// What a test is handed when an editor opens in headless mode.
#[cfg(feature = "headless")]
pub(crate) type HeadlessHook = Arc<dyn Fn(crate::HeadlessEditor) + Send + Sync>;

/// A plugin editor made of `manguebyte-ui` widgets, as an
/// [`EditorSpec`] a plugin returns from
/// [`Plugin::editor`](manguebyte_plugin::Plugin::editor).
///
/// It is made from what a plugin author writes: the editor's size in
/// logical pixels, how it may be resized, and a function that builds the
/// view, usually from the plugin's parameters, which it shares through an
/// `Arc`. The function runs each time the host opens the editor, on the
/// host's main thread, so every open editor has a tree of its own and
/// nothing is kept from one to the next except what the function captures.
///
/// [`open`](EditorSpec::open) makes a child window inside the host's window
/// with `manguebyte-window` (on Win32; other kinds of parent are refused
/// with [`EditorError::Unsupported`] until their backends exist), builds a
/// [`Root`](manguebyte_ui::Root) on the
/// [`EditorContext`] the wrapper gave, and from then on:
///
/// - forwards the window's input to the root: pointer, wheel, keys and
///   focus, in logical pixels;
/// - runs a frame from a timer the window owns, about every
///   [`FRAME_PERIOD_MS`](crate::FRAME_PERIOD_MS) milliseconds while the
///   editor is visible, and presents only the rectangles the frame drew
///   again. A frame where nothing changed polls the parameters and the
///   widgets' sources, which is how changes from other threads are seen,
///   and then lays out, draws and presents nothing;
/// - answers the system's requests for its contents from the root's back
///   buffer;
/// - follows the sizes and the scale the host sets. A new scale and the
///   size the host grants for it are drawn in one frame. While the editor
///   is hidden, sizes and scales are only taken note of, and drawn when it
///   is shown again.
///
/// The editor is visible and runs frames as soon as it opens, because
/// VST 3 has no call that shows an editor; CLAP's `show` and `hide` reach
/// it as [`OpenEditor::set_visible`].
///
/// If the interface panics (a widget's code failing while it handles an
/// event or draws), the editor drops it at once, which ends any gesture
/// it held, stops its frames and tells the host with
/// [`EditorContext::closed`]. The window stays until the host closes the
/// editor.
///
/// No size it opens at, takes or asks for has a side larger than
/// [`MAX_BUFFER_SIDE`](manguebyte_ui::MAX_BUFFER_SIDE) device pixels, the
/// most the interface draws: the resize limits are capped there at every
/// scale, and so is [`size`](EditorSpec::size).
///
/// Dropping the open editor drops the root, which ends any gesture still
/// open, then destroys the window. Nothing here runs on the audio thread
/// or shares anything with it beyond what the plugin's parameters and
/// atomics already share.
///
/// Sizes: on Win32 and X11 the host's unit is the device pixel, so the
/// editor's size at scale `s` is its logical size times `s`
/// ([`HostSize::scale_by`]); resize limits are given at scale 1 too.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gui::Editor;
/// use manguebyte_params::{FloatParam, FloatRange, param_set};
/// use manguebyte_plugin::editor::{EditorSpec, HostSize, Resize};
/// use manguebyte_ui::{IntoView, ParamHandle, column, knob, label};
///
/// struct Params { mix: FloatParam }
/// param_set!(Params { mix });
/// let params = Arc::new(Params {
///     mix: FloatParam::new("mix", "Mix", FloatRange::linear(0.0, 100.0), 50.0),
/// });
///
/// // What `Plugin::editor` returns.
/// let shared = Arc::clone(&params);
/// let editor = Editor::new(HostSize::new(160, 120), move || {
///     column((
///         label("Mix").title(),
///         knob(ParamHandle::new(Arc::clone(&shared), |p| &p.mix)),
///     ))
///     .padding(8.0)
/// })
/// .with_resize(Resize::Free { min: HostSize::new(120, 90), max: HostSize::new(480, 360) });
///
/// // Sizes in the host's unit grow with the scale.
/// assert_eq!(editor.size(1.0), HostSize::new(160, 120));
/// assert_eq!(editor.size(1.5), HostSize::new(240, 180));
/// let spec: Box<dyn EditorSpec> = Box::new(editor);
/// # let _ = spec;
/// ```
#[derive(Clone)]
pub struct Editor {
    pub(crate) size: HostSize,
    pub(crate) resize: Resize,
    pub(crate) theme: Theme,
    pub(crate) view: BuildView,
    #[cfg(feature = "headless")]
    pub(crate) on_headless: Option<HeadlessHook>,
}

impl Editor {
    /// An editor `size` logical pixels large (its size in the host's unit
    /// at scale 1), fixed in size, in the default [`Theme`], whose view
    /// `view` builds each time it opens.
    pub fn new<V: IntoView>(
        size: HostSize,
        view: impl Fn() -> V + Send + Sync + 'static,
    ) -> Editor {
        Editor {
            size,
            resize: Resize::Fixed,
            theme: Theme::default(),
            view: Arc::new(move || view().into_view()),
            #[cfg(feature = "headless")]
            on_headless: None,
        }
    }

    /// The same editor, resized by the host within `resize`, whose limits
    /// are given at scale 1.
    #[must_use]
    pub fn with_resize(mut self, resize: Resize) -> Editor {
        self.resize = resize;
        self
    }

    /// The same editor drawn in `theme`.
    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Editor {
        self.theme = theme;
        self
    }

    /// The editor's size at scale 1, in logical pixels.
    pub fn logical_size(&self) -> HostSize {
        self.size
    }

    /// The theme it is drawn in.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// The same editor, opened in a headless window whatever the parent,
    /// on every system, and calling `hook` with a controller of each
    /// editor it opens, right before `open` returns. Only with the
    /// `headless` feature, which tests enable; the controller drives the
    /// headless window as a person and a system would and reads what it
    /// shows. Editors not given a hook open in the system's window, with
    /// the feature on or not.
    ///
    /// `hook` must be `Send + Sync`, as the whole spec must; the controller
    /// it is given is not, and stays on the host's main thread. A test
    /// usually keeps it in a thread-local of its own.
    #[cfg(feature = "headless")]
    #[must_use]
    pub fn on_headless_open(
        mut self,
        hook: impl Fn(crate::HeadlessEditor) + Send + Sync + 'static,
    ) -> Editor {
        self.on_headless = Some(Arc::new(hook));
        self
    }
}

impl Editor {
    /// The window backend this editor opens with: headless when a test
    /// gave it a hook with `on_headless_open`, the system's otherwise.
    /// Turning on the `headless` feature therefore changes no editor that
    /// was not asked to be headless.
    pub(crate) fn backend(&self) -> Backend {
        #[cfg(feature = "headless")]
        if self.on_headless.is_some() {
            return Backend::Headless;
        }
        Backend::Native
    }
}

impl EditorSpec for Editor {
    /// The logical size times `scale`, with no side larger than
    /// [`MAX_BUFFER_SIDE`](manguebyte_ui::MAX_BUFFER_SIDE).
    fn size(&self, scale: f64) -> HostSize {
        cap(self.size.scale_by(scale))
    }

    fn resize(&self) -> Resize {
        self.resize
    }

    /// The kinds of parent there is a window backend for: Win32 on
    /// Windows, and every kind for an editor that opens headless.
    fn supports(&self, api: WindowApi) -> bool {
        self.backend().supports(api)
    }

    fn open(
        &self,
        parent: ParentWindow,
        scale: Option<f64>,
        context: EditorContext,
    ) -> Result<Box<dyn OpenEditor>, EditorError> {
        open_with(self, self.backend(), parent, scale, context)
    }
}

impl fmt::Debug for Editor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Editor")
            .field("size", &self.size)
            .field("resize", &self.resize)
            .finish_non_exhaustive()
    }
}
