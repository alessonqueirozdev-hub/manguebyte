//! An interface in an application's own window: a standalone program, or
//! a plugin's editor run as one.

use std::cell::Cell;
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use manguebyte_gfx::Size;
use manguebyte_params::{ParamId, ParamSet};
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorHost, HostLink, HostLoop, HostSize, Resize,
};
use manguebyte_ui::{IntoView, Theme};
use manguebyte_window::{App, AppWindowOptions, Dialogs, Window, WindowCx};

use crate::editor::Editor;
use crate::open::{Clock, FRAME_PERIOD_MS, FRAME_TIMER, Glue, Shared, attach_root, editor_error};

/// The host of an application's own interface: there is none, so the
/// interface's gestures set the parameters directly, as a standalone
/// program does, and every resize it asks for is granted.
struct StandaloneHost {
    params: Option<Arc<dyn ParamSet>>,
    closed: Cell<bool>,
}

impl EditorHost for StandaloneHost {
    fn begin_gesture(&self, param: ParamId) {
        let _ = param;
    }

    fn perform(&self, param: ParamId, normalized: f64) {
        if let Some(target) = self
            .params
            .as_ref()
            .and_then(|params| params.find_param(param))
        {
            target.set_normalized(normalized);
        }
    }

    fn end_gesture(&self, param: ParamId) {
        let _ = param;
    }

    fn values_changed(&self) {}

    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        // The window is the application's: the glue resizes it.
        Some(size)
    }

    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        None
    }

    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        None
    }

    fn closed(&self) {
        self.closed.set(true);
    }
}

/// An interface open in an application window of an [`App`]: what
/// [`open_app_window`] and [`Editor::open_in_app`] return.
///
/// It runs the same glue as an editor inside a host: frames about 60
/// times a second from a timer the window owns, presenting only what
/// changed, input to the [`Root`](manguebyte_ui::Root), the system's
/// clipboard. What differs is that there is no host: the interface's
/// gestures set the parameters given, if any, as a standalone program's
/// host would; the window follows the system's scale for its monitor by
/// itself; a close request closes the window unless
/// [`on_close_request`](Self::on_close_request) says otherwise; and the
/// window always takes files and text dragged onto it, which the widgets
/// hear as drags carrying a [`DropData`](manguebyte_ui::DropData).
///
/// Dropping it closes the window, the interface first.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_gui::open_app_window;
/// use manguebyte_ui::{Role, Theme, label};
/// use manguebyte_window::{App, AppWindowOptions, RunEnd};
///
/// let (app, test) = App::headless();
/// let window = open_app_window(
///     &app,
///     AppWindowOptions::new("Hello", Size::new(160.0, 60.0)),
///     Theme::default(),
///     None,
///     label("Hello"),
/// )
/// .expect("headless windows always open");
/// let ui = window.headless().expect("a headless application");
/// assert_eq!(ui.tick(), 1, "the frame timer runs at once");
/// assert!(ui.find(Role::Label, "Hello").is_some());
///
/// // The person closes the window while the loop runs.
/// let controller = ui.window().clone();
/// test.post(move || {
///     controller.request_close();
/// });
/// assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
/// assert!(!window.is_open());
/// ```
pub struct AppEditor {
    shared: Rc<Shared>,
    window: Window,
    host: Rc<StandaloneHost>,
    _link: HostLink,
}

impl AppEditor {
    /// The application window.
    pub fn window(&self) -> &Window {
        &self.window
    }

    /// The system's file dialogs for the window, for the interface's
    /// buttons to keep.
    pub fn dialogs(&self) -> Dialogs {
        self.window.dialogs()
    }

    /// Whether the window is open and the interface alive.
    pub fn is_open(&self) -> bool {
        self.window.is_open() && !self.host.closed.get()
    }

    /// Decides what a close request does: `decide` is asked each time, and
    /// the window closes when it returns true, as when there are no unsaved
    /// changes. Without one, every close request closes the window.
    pub fn on_close_request(&self, decide: impl FnMut(&WindowCx<'_>) -> bool + 'static) {
        *self.shared.close_request.borrow_mut() = Some(Box::new(decide));
    }

    /// Shows or hides the window. A hidden interface runs no frames.
    pub fn set_visible(&self, visible: bool) {
        if self.window.set_visible(visible).is_err() {
            return;
        }
        if visible {
            let _ = self.window.set_timer(FRAME_TIMER, FRAME_PERIOD_MS);
        } else {
            self.window.kill_timer(FRAME_TIMER);
        }
    }

    /// The controller of the interface, when the application is headless
    /// (with the `headless` feature).
    #[cfg(feature = "headless")]
    pub fn headless(&self) -> Option<crate::HeadlessEditor> {
        let window = self.window.headless_controller()?;
        Some(crate::HeadlessEditor::new(
            window,
            Rc::downgrade(&self.shared),
        ))
    }
}

impl Drop for AppEditor {
    fn drop(&mut self) {
        self.window.kill_timer(FRAME_TIMER);
        // The interface first, while its parameters are still reachable: it
        // ends any gesture a control holds.
        let root = self
            .shared
            .root
            .try_borrow_mut()
            .ok()
            .and_then(|mut slot| slot.take());
        drop(root);
    }
}

impl fmt::Debug for AppEditor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppEditor")
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

/// Opens `view` in a new window of `app`, made by `options` and drawn in
/// `theme`, whose gestures set `params`, if given. The window takes file
/// drops whatever the options say. Frames start at once if the window is
/// visible.
///
/// # Errors
///
/// [`EditorError::Unsupported`] where there is no backend for application
/// windows, or [`EditorError::Platform`] when the options are refused or
/// the system cannot make the window.
pub fn open_app_window(
    app: &App,
    options: AppWindowOptions,
    theme: Theme,
    params: Option<Arc<dyn ParamSet>>,
    view: impl IntoView,
) -> Result<AppEditor, EditorError> {
    let base = HostSize::new(
        options.size.width.max(1.0).round() as u32,
        options.size.height.max(1.0).round() as u32,
    );
    open(
        app,
        options,
        base,
        Resize::Fixed,
        theme,
        params,
        view.into_view(),
    )
}

impl Editor {
    /// Opens the editor in a new window of `app` titled `title`, of the
    /// editor's size in logical pixels and resizable as its
    /// [`Resize`] says (a ratio is not kept yet: its limits are), whose
    /// gestures set `params`, as a standalone program runs a plugin's
    /// editor. See [`AppEditor`].
    ///
    /// # Errors
    ///
    /// As [`open_app_window`].
    pub fn open_in_app(
        &self,
        app: &App,
        title: &str,
        params: Option<Arc<dyn ParamSet>>,
    ) -> Result<AppEditor, EditorError> {
        let logical = |size: HostSize| Size::new(size.width as f32, size.height as f32);
        let mut options = AppWindowOptions::new(title, logical(self.size));
        options = match self.resize {
            Resize::Fixed => options.fixed(),
            Resize::Free { min, max } | Resize::Ratio { min, max, .. } => options
                .with_min_size(logical(min))
                .with_max_size(logical(max)),
        };
        open(
            app,
            options,
            self.size,
            self.resize,
            self.theme,
            params,
            (self.view)(),
        )
    }
}

fn open(
    app: &App,
    options: AppWindowOptions,
    base: HostSize,
    rules: Resize,
    theme: Theme,
    params: Option<Arc<dyn ParamSet>>,
    view: manguebyte_ui::View,
) -> Result<AppEditor, EditorError> {
    let host = Rc::new(StandaloneHost {
        params,
        closed: Cell::new(false),
    });
    let (context, link) = EditorContext::attach(host.clone());
    let clock = if app.is_headless() {
        Clock::Manual(Cell::new(Duration::ZERO))
    } else {
        Clock::Real(Instant::now())
    };
    let shared = Shared::new(context.clone(), base, rules, None, clock, true);
    let visible = options.visible;
    let window = app
        .open_window(options.with_file_drops(), Glue::new(&shared))
        .map_err(editor_error)?;
    attach_root(&shared, &window, view, theme, context);
    shared.release();
    if visible {
        let _ = window.set_timer(FRAME_TIMER, FRAME_PERIOD_MS);
    }
    Ok(AppEditor {
        shared,
        window,
        host,
        _link: link,
    })
}
