//! The widget trait and the contexts its methods are given.

use std::time::Duration;

use manguebyte_gfx::{Point, Rect, Scene, Size};
use manguebyte_plugin::editor::EditorContext;

use crate::access::{AccessItem, AccessNode, Action};
use crate::clipboard::Clipboard;
use crate::drag::{DragEvent, DragPayload};
use crate::event::{Event, Handled};
use crate::menu::Menu;
use crate::root::{Anchor, MenuRequest};
use crate::scroll::ScrollMetrics;
use crate::theme::Theme;
use crate::tree::NodeId;

/// A node of the interface: something that is laid out, drawn, receives
/// input and describes itself.
///
/// The [`Root`](crate::Root) owns every widget in an arena, in tree order;
/// containers do not own their children, so hit testing, focus, damage,
/// the accessibility tree and test queries are generic code that no widget
/// has to take part in. A widget keeps its own state in its struct and is
/// called on the interface's thread only:
///
/// - [`update`](Widget::update) once per frame, to poll the
///   [`Source`](crate::Source)s and parameters it shows and to ask to be
///   drawn again if one changed;
/// - [`measure`](Widget::measure) when layout runs, for its natural size;
/// - [`paint`](Widget::paint) when it asked to be drawn again or moved,
///   to record what it looks like into a [`Scene`];
/// - [`event`](Widget::event) and [`act`](Widget::act) for input and
///   for requests from assistive technology;
/// - [`access`](Widget::access) whenever its description is wanted.
///
/// `access`, `measure` and `paint` have no default: a widget cannot be
/// written without saying what it is, how large it wants to be and how it
/// looks.
///
/// A widget of one's own: a light that shows whether a value is above a
/// threshold, read from a [`Source`](crate::Source) once per frame.
///
/// ```
/// use std::cell::Cell;
/// use std::rc::Rc;
/// use manguebyte_gfx::{Circle, Scene, Size};
/// use manguebyte_ui::access::{AccessNode, Role};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{AccessCx, MeasureCx, PaintCx, Source, UpdateCx, Widget};
///
/// struct Light<S> { name: &'static str, level: S, lit: bool }
///
/// impl<S: Source<f32>> Widget for Light<S> {
///     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
///         AccessNode::new(Role::Image, self.name).with_value(if self.lit { "on" } else { "off" })
///     }
///
///     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
///         Size::new(16.0, 16.0)
///     }
///
///     fn update(&mut self, cx: &mut UpdateCx<'_>) {
///         let lit = self.level.read() > 0.5;
///         if lit != self.lit {
///             self.lit = lit;
///             cx.request_paint();
///         }
///     }
///
///     fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
///         let palette = &cx.theme().palette;
///         let color = if self.lit { palette.accent } else { palette.track };
///         let rect = cx.rect();
///         scene.fill(Circle::new(rect.center(), rect.width.min(rect.height) * 0.4), color);
///     }
/// }
///
/// let level = Rc::new(Cell::new(0.0_f32));
/// let source = Rc::clone(&level);
/// let light = Light { name: "Clip", level: move || source.get(), lit: false };
/// let mut ui = Harness::new(light, Size::new(16.0, 16.0), 1.0);
/// ui.frame();
/// assert_eq!(ui.get(Role::Image, "Clip").value_text(), Some("off"));
/// level.set(0.9);
/// assert_eq!(ui.frame().damaged, 1);
/// assert_eq!(ui.get(Role::Image, "Clip").value_text(), Some("on"));
/// assert_eq!(ui.frame().damaged, 0, "nothing changed since");
/// ```
///
/// Leaving out the description does not compile:
///
/// ```compile_fail,E0046
/// use manguebyte_gfx::{Scene, Size};
/// use manguebyte_ui::{MeasureCx, PaintCx, Widget};
///
/// struct Silent;
///
/// impl Widget for Silent {
///     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
///         Size::new(10.0, 10.0)
///     }
///     fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
/// }
/// ```
pub trait Widget: 'static {
    /// How the widget describes itself to assistive technology and to
    /// tests. The root fills in [`focused`](crate::access::AccessState).
    fn access(&self, cx: &AccessCx<'_>) -> AccessNode;

    /// The natural size of the widget's content, in logical pixels.
    /// Containers take their size from their children, so theirs is not
    /// used.
    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size;

    /// Records how the widget looks into `scene`, which is empty, in the
    /// interface's logical coordinates, inside [`PaintCx::rect`]. The root
    /// clips whatever reaches past the widget's place in the layout, so it
    /// never covers a neighbour, and repaints the device pixels the
    /// scene's [`bounds`](Scene::bounds) cover, before and after a change.
    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene);

    /// Called once per frame before anything is laid out or drawn. Polls
    /// what the widget shows and asks for a repaint if it changed. Nothing
    /// unless overridden.
    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let _ = cx;
    }

    /// Handles input. Pointer events reach the widget under the pointer
    /// first and then the widgets that contain it, until one returns
    /// [`Handled::Yes`]; while a widget holds the pointer (see
    /// [`EventCx::capture_pointer`]) every move and the release go to it.
    /// Keys, typed text and an input method's compositions go to the
    /// widget that holds the pointer, then to the one with the focus.
    /// Ignores everything unless overridden.
    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let _ = (cx, event);
        Handled::No
    }

    /// Carries out `action`, which the widget's
    /// [`AccessNode::actions`] allow (the root checks), and returns whether
    /// it did. The root itself handles [`Action::Focus`]. Does nothing
    /// unless overridden.
    fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        let _ = (cx, action);
        false
    }

    /// The widget gained (`true`) or lost (`false`) the keyboard focus:
    /// by a press, Tab, assistive technology, a menu opening over it, or
    /// the window losing the keyboard. A text input takes what was typed
    /// when it loses it. Asking for the focus from here does nothing.
    /// Nothing unless overridden.
    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        let _ = (cx, focused);
    }

    /// The pointer came over the widget (`true`), with no button held, or
    /// went off it (`false`): only the innermost widget under the pointer
    /// is hovered. It is drawn again either way, and can show it with
    /// [`PaintCx::is_hovered`]. Nothing unless overridden.
    fn hover_changed(&mut self, cx: &mut EventCx<'_>, hovered: bool) {
        let _ = (cx, hovered);
    }

    /// Where the text caret is, in logical pixels, while the widget is
    /// being typed into: the window layer tells the system, so that an
    /// input method opens its window beside it and a screen magnifier
    /// follows it. `None`, the default, when the widget takes no text now.
    fn caret(&self, cx: &AccessCx<'_>) -> Option<Rect> {
        let _ = cx;
        None
    }

    /// Takes part in a drag (see [`crate::drag`]): as a possible target,
    /// [`DragEvent::Over`] is accepted with [`Handled::Yes`], and
    /// [`DragEvent::Drop`] taken the same way; the widget that started the
    /// drag hears [`DragEvent::Ended`]. Refuses everything unless
    /// overridden.
    fn drag(&mut self, cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let _ = (cx, event);
        Handled::No
    }

    /// Adds the parts of the widget that assistive technology should see
    /// as nodes of their own to `items`, such as the rows of a list that
    /// are shown, each after the widget in the accessibility tree and
    /// inside it (see [`AccessItem`]). Nothing unless overridden: most
    /// widgets are one node.
    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let _ = (cx, items);
    }

    /// Carries out `action` on the item with `key` (see
    /// [`access_items`](Widget::access_items)), which the item's node
    /// allows (the root checks), and returns whether it did. For
    /// [`Action::Focus`] the root gives the widget itself the focus first.
    /// Does nothing unless overridden.
    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        let _ = (cx, key, action);
        false
    }
}

/// What a widget asked for during a call; the root acts on it afterwards.
#[derive(Debug, Default)]
pub(crate) struct Requests {
    pub(crate) paint: bool,
    pub(crate) layout: bool,
    pub(crate) frame: bool,
    /// `Some(true)` to take the pointer, `Some(false)` to let it go.
    pub(crate) capture: Option<bool>,
    pub(crate) focus: bool,
    /// A menu to open, hung from the widget or a point.
    pub(crate) menu: Option<MenuRequest>,
    /// Where a scroll node asked to be scrolled to.
    pub(crate) scroll: Option<Point>,
    /// A drag to start.
    pub(crate) drag: Option<DragPayload>,
}

/// What [`Widget::update`] is given.
pub struct UpdateCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) rect: Rect,
    pub(crate) scroll: Option<ScrollMetrics>,
    pub(crate) scale: f32,
    pub(crate) now: Duration,
    pub(crate) elapsed: Duration,
    pub(crate) requests: &'a mut Requests,
}

impl UpdateCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The rectangle the widget draws in, in logical pixels (see
    /// [`PaintCx::rect`]).
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The time of this frame, on the clock the root is driven by.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// How long since the previous frame; zero on the first.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// Asks for [`Widget::paint`] to be called this frame.
    pub fn request_paint(&mut self) {
        self.requests.paint = true;
    }

    /// Asks for [`Widget::measure`] and layout to run this frame, because
    /// the widget's natural size may have changed. It is painted again too.
    pub fn request_layout(&mut self) {
        self.requests.layout = true;
        self.requests.paint = true;
    }

    /// Says the widget is moving on its own (an animation, a falling
    /// meter), so the window should keep delivering frames.
    pub fn request_frame(&mut self) {
        self.requests.frame = true;
    }

    /// Where the widget is scrolled, if it is laid out as a scroll node
    /// (see [`crate::scroll`]).
    pub fn scroll(&self) -> Option<ScrollMetrics> {
        self.scroll
    }

    /// Scrolls the widget, a scroll node, so that `offset` is at the
    /// viewport's top-left corner: kept within the content and rounded to
    /// whole device pixels, before this frame is laid out. Does nothing for
    /// a widget that does not scroll.
    pub fn scroll_to(&mut self, offset: Point) {
        self.requests.scroll = Some(offset);
    }
}

/// What [`Widget::measure`] is given.
pub struct MeasureCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) scale: f32,
}

impl MeasureCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }
}

/// What [`Widget::paint`] is given.
pub struct PaintCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) rect: Rect,
    pub(crate) scroll: Option<ScrollMetrics>,
    pub(crate) scale: f32,
    pub(crate) focused: bool,
    pub(crate) hovered: bool,
    pub(crate) now: Duration,
}

impl PaintCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The rectangle the widget draws in, in logical pixels, with its
    /// edges on device pixels: its place in the layout, less the
    /// [`padding`](crate::IntoView::padding) a widget with no children was
    /// given.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Whether the widget has the keyboard focus, and should show it.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Whether the pointer is over the widget, with no button held (see
    /// [`Widget::hover_changed`]).
    pub fn is_hovered(&self) -> bool {
        self.hovered
    }

    /// The time of this frame.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// Where the widget is scrolled, if it is laid out as a scroll node
    /// (see [`crate::scroll`]).
    pub fn scroll(&self) -> Option<ScrollMetrics> {
        self.scroll
    }
}

/// What [`Widget::access`] is given.
pub struct AccessCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) rect: Rect,
    pub(crate) scroll: Option<ScrollMetrics>,
    pub(crate) focused: bool,
}

impl AccessCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The rectangle the widget draws in, in logical pixels (see
    /// [`PaintCx::rect`]).
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Whether the widget has the keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Where the widget is scrolled, if it is laid out as a scroll node
    /// (see [`crate::scroll`]).
    pub fn scroll(&self) -> Option<ScrollMetrics> {
        self.scroll
    }
}

/// What [`Widget::event`] and [`Widget::act`] are given.
pub struct EventCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) rect: Rect,
    pub(crate) scroll: Option<ScrollMetrics>,
    pub(crate) scale: f32,
    pub(crate) focused: bool,
    pub(crate) captured: bool,
    pub(crate) now: Duration,
    pub(crate) context: &'a EditorContext,
    pub(crate) clipboard: &'a dyn Clipboard,
    pub(crate) id: NodeId,
    pub(crate) requests: &'a mut Requests,
}

impl EventCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The rectangle the widget draws in, in logical pixels (see
    /// [`PaintCx::rect`]).
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The time of the last frame.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// Whether the widget has the keyboard focus.
    pub fn is_focused(&self) -> bool {
        self.focused
    }

    /// Whether the widget holds the pointer.
    pub fn has_pointer(&self) -> bool {
        self.captured
    }

    /// How the editor reaches its host: what
    /// [`Gesture`](manguebyte_plugin::editor::Gesture)s are made on.
    pub fn context(&self) -> &EditorContext {
        self.context
    }

    /// The clipboard: the system's when the window layer gave the root
    /// one, the root's own otherwise (see [`crate::clipboard`]).
    pub fn clipboard(&self) -> &dyn Clipboard {
        self.clipboard
    }

    /// Asks for [`Widget::paint`] to be called on the next frame.
    pub fn request_paint(&mut self) {
        self.requests.paint = true;
    }

    /// Asks for layout to run on the next frame.
    pub fn request_layout(&mut self) {
        self.requests.layout = true;
        self.requests.paint = true;
    }

    /// Takes the pointer: every move and the release go to this widget
    /// until it is released, or until it lets go with
    /// [`release_pointer`](Self::release_pointer).
    pub fn capture_pointer(&mut self) {
        self.requests.capture = Some(true);
    }

    /// Lets go of the pointer.
    pub fn release_pointer(&mut self) {
        self.requests.capture = Some(false);
    }

    /// Takes the keyboard focus, if the widget accepts
    /// [`Action::Focus`]. A press takes the focus away from the widget that
    /// has it unless it lands on that widget; a widget that should have
    /// the focus once pressed asks for it here.
    pub fn request_focus(&mut self) {
        self.requests.focus = true;
    }

    /// Opens `menu` below this widget, or above it where there is no room
    /// below, at least as wide as the widget (see [`crate::menu`]). It
    /// opens once the widget returns, with the keyboard focus on its
    /// checked item, or else its first; a menu open already closes first.
    /// When it closes, with a choice or without, this widget hears it as
    /// [`Event::Menu`](crate::Event::Menu), and the focus goes back to the
    /// widget that had it.
    pub fn open_menu(&mut self, menu: Menu) {
        self.requests.menu = Some(MenuRequest {
            menu,
            anchor: Anchor::Widget(self.id),
        });
    }

    /// Opens `menu` at `at`, in logical pixels, as a context menu: its
    /// top-left corner there, or its bottom-left corner where there is no
    /// room below. Otherwise as [`open_menu`](Self::open_menu).
    pub fn open_menu_at(&mut self, menu: Menu, at: Point) {
        self.requests.menu = Some(MenuRequest {
            menu,
            anchor: Anchor::Point(at),
        });
    }

    /// Where the widget is scrolled, if it is laid out as a scroll node
    /// (see [`crate::scroll`]).
    pub fn scroll(&self) -> Option<ScrollMetrics> {
        self.scroll
    }

    /// Scrolls the widget, a scroll node, so that `offset` is at the
    /// viewport's top-left corner: kept within the content and rounded to
    /// whole device pixels, and laid out on the next frame. Does nothing
    /// for a widget that does not scroll.
    pub fn scroll_to(&mut self, offset: Point) {
        self.requests.scroll = Some(offset);
    }

    /// Starts dragging `payload` (see [`crate::drag`]), if a button is
    /// held and no drag runs already: from when the widget returns, the
    /// root has the pointer, draws the payload's description under it,
    /// and offers the payload to the widgets it passes over. The widget
    /// no longer holds the pointer, and hears how the drag ended as
    /// [`DragEvent::Ended`]. A widget usually starts a drag once the
    /// pointer has moved a few pixels with the button held, so that a
    /// click stays a click.
    pub fn start_drag(&mut self, payload: DragPayload) {
        self.requests.drag = Some(payload);
    }
}
