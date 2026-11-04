//! The editor spec a plugin author writes: a size, resize rules and a
//! function that builds the view.

use std::fmt;
use std::sync::Arc;

use manguebyte_gfx::Size;
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorSpec, HostSize, OpenEditor, ParentWindow, Resize, WindowApi,
};
use manguebyte_ui::{IntoView, Theme, View};

use crate::open::{Backend, cap, open_with};
use crate::zoom::{self, ZoomRules};

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
///   focus, in logical pixels (of the design, for an editor that
///   [zooms to fit](Editor::with_zoom_to_fit));
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
/// The editor opens hidden, with no frame timer, and is shown when the
/// host says so through [`OpenEditor::set_visible`]: CLAP's `show` and
/// `hide` reach it that way, and the VST 3 wrapper, since VST 3 has no
/// call that shows a view, shows it once `attached` has succeeded and hides
/// it at the start of `removed`.
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
    pub(crate) accepts_drops: bool,
    pub(crate) min_zoom: Option<f32>,
    pub(crate) real_below: Option<Size>,
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
            accepts_drops: false,
            min_zoom: None,
            real_below: None,
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

    /// The same editor, taking files and text dragged onto its window from
    /// other programs, which its widgets hear as drags whose payload is a
    /// [`DropData`](manguebyte_ui::DropData) (see
    /// [`Root::file_drop`](manguebyte_ui::Root::file_drop)). Off by
    /// default: inside a host, a window that takes drops hides the host's
    /// own drop target under it, and on Win32 it takes them only where the
    /// host's main thread has OLE, which the editor never initialises for
    /// it. An application window ([`open_in_app`](Self::open_in_app))
    /// always takes them.
    #[must_use]
    pub fn with_file_drops(mut self) -> Editor {
        self.accepts_drops = true;
        self
    }

    /// The same editor, keeping its layout at its own size, its design
    /// size, and zooming to fit whatever window it gets, in a host or in an
    /// application's window ([`open_in_app`](Self::open_in_app)).
    ///
    /// The zoom is the smaller of the window's width over the design's and
    /// its height over the design's (in logical pixels, so at the system's
    /// or the host's scale), and the interface is drawn at that scale times
    /// the zoom: the layout does not change, however the window does, only
    /// how large it is drawn. With [`Resize::Free`] or [`Resize::Ratio`],
    /// resizing changes the zoom and does not reflow. Everything follows
    /// from that one scale: painting, hit testing, the pointer's position,
    /// where assistive technology is told a widget is and the text caret
    /// are all in the same space. A window of the design size has no zoom
    /// at all.
    ///
    /// The layout sits in the middle of the window, with bars in the
    /// theme's background colour on the axis with room to spare. The bars
    /// are not part of the interface: a press or a wheel turn there reaches
    /// nothing and leaves the focus where it is. A drag that goes over them
    /// goes on as a drag outside a window does.
    ///
    /// Below `min_zoom` the zoom stops there, so that nothing gets too small
    /// to read, and the layout goes back to reflowing in the room there is,
    /// which is the whole window at that zoom, with no bars. That changes
    /// at once on the axis with room to spare, which the bars had: a window
    /// tall for its width goes from the design's height to all of its own.
    /// Within what the interface can draw (scales `0.25..=8` with the
    /// system's scale counted in, and
    /// [`MAX_BUFFER_SIDE`](manguebyte_ui::MAX_BUFFER_SIDE) device pixels a
    /// side): past that, the bars grow. A `min_zoom` that is not a positive
    /// number asks only for that limit.
    ///
    /// Each resize draws everything again at the new scale, as any change
    /// of scale does; nothing is built again.
    ///
    /// ```
    /// # use std::cell::RefCell;
    /// # use std::rc::Rc;
    /// use manguebyte_gfx::Size;
    /// use manguebyte_gui::{Editor, HeadlessEditor};
    /// use manguebyte_plugin::editor::testing::RecordingEditorHost;
    /// use manguebyte_plugin::editor::{EditorContext, EditorSpec, HostSize, ParentWindow, Resize};
    /// use manguebyte_ui::{Role, label};
    /// use manguebyte_window::PhysicalSize;
    ///
    /// thread_local! {
    ///     static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
    /// }
    /// let spec = Editor::new(HostSize::new(400, 300), || label("Console"))
    ///     .with_resize(Resize::Free { min: HostSize::new(100, 100), max: HostSize::new(800, 600) })
    ///     .with_zoom_to_fit(0.5)
    ///     .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
    /// let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    /// let parent = ParentWindow::win32(0x10).expect("not null");
    /// let mut editor = spec.open(parent, Some(1.0), context).expect("headless windows open");
    /// let ui = OPENED.with(|slot| slot.borrow_mut().take()).expect("the hook ran");
    /// editor.set_visible(true);
    ///
    /// // A window wide and short: three quarters, with bars at the sides.
    /// ui.window().resize(PhysicalSize::new(500, 225));
    /// assert_eq!(ui.layout(), Some((Size::new(400.0, 300.0), 0.75)));
    /// assert_eq!(ui.origin(), Some((100, 0)));
    /// assert_eq!(ui.pixels().map(|buffer| buffer.width()), Some(300));
    /// assert_eq!(ui.shown().map(|shown| shown.width()), Some(500));
    /// // Tests still speak in the design's pixels.
    /// assert_eq!(ui.find(Role::Label, "Console").map(|found| found.bounds.width), Some(400.0));
    /// # drop(editor);
    /// # drop(link);
    /// ```
    #[must_use]
    pub fn with_zoom_to_fit(mut self, min_zoom: f32) -> Editor {
        self.min_zoom = Some(zoom::clean_min_zoom(min_zoom));
        self
    }

    /// The same editor, with no zoom at all while the window is narrower
    /// or shorter than `window`, in logical pixels: the layout then gets
    /// the whole window, at the system's or the host's scale. Only for an
    /// editor with [`with_zoom_to_fit`](Self::with_zoom_to_fit), which it
    /// takes precedence over.
    ///
    /// This is how a program has its full layout zoomed and a compact one at
    /// its real size: a [`responsive`](manguebyte_ui::responsive) at the
    /// root, whose first child has `window` as its smallest size. Zoomed,
    /// the room is the design size, so the first child shows; below `window`,
    /// the room is the window, so the second does. The window has the same
    /// margin as `responsive`
    /// ([`RESPONSIVE_MARGIN`](manguebyte_ui::RESPONSIVE_MARGIN)): back to
    /// zoom at `window` itself, and away from it that much below, so the two
    /// change as one, in the same frame.
    ///
    /// ```
    /// # use std::cell::RefCell;
    /// # use std::rc::Rc;
    /// use manguebyte_gfx::Size;
    /// use manguebyte_gui::{Editor, HeadlessEditor};
    /// use manguebyte_plugin::editor::testing::RecordingEditorHost;
    /// use manguebyte_plugin::editor::{EditorContext, EditorSpec, HostSize, ParentWindow, Resize};
    /// use manguebyte_ui::{Role, label, responsive};
    /// use manguebyte_window::PhysicalSize;
    ///
    /// thread_local! {
    ///     static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
    /// }
    /// let breakpoint = Size::new(1100.0, 700.0);
    /// let spec = Editor::new(HostSize::new(1600, 980), move || {
    ///     responsive((label("Console"), label("Summary"))).below(breakpoint)
    /// })
    /// .with_resize(Resize::Free { min: HostSize::new(400, 300), max: HostSize::new(3200, 2000) })
    /// .with_zoom_to_fit(0.5)
    /// .with_real_size_below(breakpoint)
    /// .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
    /// let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    /// let parent = ParentWindow::win32(0x10).expect("not null");
    /// let mut editor = spec.open(parent, Some(1.0), context).expect("headless windows open");
    /// let ui = OPENED.with(|slot| slot.borrow_mut().take()).expect("the hook ran");
    /// editor.set_visible(true);
    /// ui.tick();
    ///
    /// // A short screen: the whole console, smaller.
    /// ui.window().resize(PhysicalSize::new(1536, 840));
    /// assert!(ui.find(Role::Label, "Console").is_some());
    /// assert!(ui.layout().is_some_and(|(_, scale)| scale < 1.0));
    ///
    /// // Half of it, beside a score: the summary, at its real size.
    /// ui.window().resize(PhysicalSize::new(768, 830));
    /// assert!(ui.find(Role::Label, "Summary").is_some());
    /// assert_eq!(ui.layout(), Some((Size::new(768.0, 830.0), 1.0)));
    /// # drop(editor);
    /// # drop(link);
    /// ```
    #[must_use]
    pub fn with_real_size_below(mut self, window: Size) -> Editor {
        self.real_below = Some(window);
        self
    }

    /// How it zooms to fit, if it does.
    pub(crate) fn zoom_rules(&self) -> Option<ZoomRules> {
        self.min_zoom.map(|min_zoom| ZoomRules {
            min_zoom,
            real_below: self.real_below,
        })
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
