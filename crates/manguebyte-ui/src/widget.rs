//! The widget trait and the contexts its methods are given.

use std::time::Duration;

use manguebyte_gfx::{Point, Rect, Scene, Size};
use manguebyte_plugin::editor::EditorContext;

use crate::access::{AccessItem, AccessNode, Action};
use crate::clipboard::Clipboard;
use crate::drag::{DragEvent, DragPayload};
use crate::event::{Event, Handled};
use crate::menu::Menu;
use crate::motion::{Animated, Lerp, Motion};
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
    /// [`PaintCx::is_hovered`] and [`PaintCx::hover`]. Nothing unless
    /// overridden.
    fn hover_changed(&mut self, cx: &mut EventCx<'_>, hovered: bool) {
        let _ = (cx, hovered);
    }

    /// The pointer moved to `at`, in logical pixels, over the widget while
    /// it is hovered (with no button held): for a widget that lights the
    /// part of itself under the pointer, such as a row, a key or a zone.
    /// It asks to be drawn again if that part changed. Nothing unless
    /// overridden.
    fn hover_moved(&mut self, cx: &mut EventCx<'_>, at: Point) {
        let _ = (cx, at);
    }

    /// How far beyond its rectangle, in logical pixels, the widget draws:
    /// the reach of a shadow or a glow around it, such as a card's. The
    /// root clips what the widget draws to its rectangle grown by this,
    /// and draws that much around it again when it changes, so a shadow
    /// that spills over its neighbours is never cut and never left behind.
    /// Asked whenever layout runs. Zero unless overridden.
    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        let _ = cx;
        0.0
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
    /// The widget's description for assistive technology changed.
    pub(crate) access: bool,
    /// `Some(true)` to take the pointer, `Some(false)` to let it go.
    pub(crate) capture: Option<bool>,
    pub(crate) focus: bool,
    /// A menu to open, hung from the widget or a point.
    pub(crate) menu: Option<MenuRequest>,
    /// Where a scroll node asked to be scrolled to.
    pub(crate) scroll: Option<Point>,
    /// A drag to start.
    pub(crate) drag: Option<DragPayload>,
    /// Which of its children a container shows from now on.
    pub(crate) shown: Option<Shown>,
    /// `Some(Some(child))` to make a child's subtree modal, `Some(None)`
    /// to end it.
    pub(crate) modal: Option<Option<usize>>,
}

/// Which of its children a container shows (see [`UpdateCx::show`]).
///
/// A child that is not shown is kept, with every widget inside it and
/// their state, but the root does not poll it, lay it out, draw it, let
/// it be hit or focused, or describe it to assistive technology: it costs
/// nothing until it is shown again. Pages and tabs are built on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Shown {
    /// Every child, as containers do unless they ask otherwise.
    #[default]
    All,
    /// Only the child at this position among its children, from 0; none
    /// if there is no child there.
    Only(usize),
}

impl Shown {
    /// Whether the child at position `child` is shown.
    pub fn includes(self, child: usize) -> bool {
        match self {
            Shown::All => true,
            Shown::Only(at) => at == child,
        }
    }
}

/// What [`Widget::update`] is given.
pub struct UpdateCx<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) motion: Motion,
    /// The root's amounts for the widget: hover, press, focus ring.
    pub(crate) amounts: (f32, f32, f32),
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

    /// The motion tokens in effect: the theme's, with every transition a
    /// jump when the theme or the system turns motion off.
    pub fn motion(&self) -> Motion {
        self.motion
    }

    /// How far the hover has come in this frame (see [`PaintCx::hover`]).
    /// For a scroll view, the pointer anywhere over its content counts.
    pub fn hover(&self) -> f32 {
        self.amounts.0
    }

    /// How far the focus ring has come in this frame (see
    /// [`PaintCx::focus_ring`]).
    pub fn focus_ring(&self) -> f32 {
        self.amounts.2
    }

    /// Moves `value` on to this frame's time; while it changes the widget
    /// is drawn again, and while it runs the root keeps frames coming.
    /// Returns whether it changed.
    ///
    /// ```
    /// use manguebyte_gfx::{Color, Rect, Scene, Size};
    /// use manguebyte_ui::access::{AccessNode, Role};
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{AccessCx, Animated, MeasureCx, PaintCx, UpdateCx, Widget};
    ///
    /// /// A light that fades in once.
    /// struct Fade(Animated<f32>);
    ///
    /// impl Widget for Fade {
    ///     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
    ///         AccessNode::new(Role::Image, "Fade")
    ///     }
    ///     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
    ///         Size::new(10.0, 10.0)
    ///     }
    ///     fn update(&mut self, cx: &mut UpdateCx<'_>) {
    ///         let (now, base) = (cx.now(), cx.motion().base());
    ///         self.0.transition(1.0, now, base);
    ///         cx.animate(&mut self.0);
    ///     }
    ///     fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
    ///         scene.fill(cx.rect(), Color::WHITE.with_opacity(self.0.value()));
    ///     }
    /// }
    ///
    /// let mut ui = Harness::new(Fade(Animated::new(0.0)), Size::new(10.0, 10.0), 1.0);
    /// ui.set_motion(true);
    /// assert!(ui.frame().animating);
    /// ui.advance_ms(100);
    /// assert!(ui.frame().animating);
    /// ui.advance_ms(100);
    /// // Settled: no more frames wanted, nothing drawn.
    /// assert!(!ui.frame().animating);
    /// assert_eq!(ui.frame().damaged, 0);
    /// ```
    pub fn animate<T: Lerp>(&mut self, value: &mut Animated<T>) -> bool {
        let changed = value.tick(self.now);
        if changed {
            self.requests.paint = true;
        }
        if value.is_running() {
            self.requests.frame = true;
        }
        changed
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

    /// Says the widget's description for assistive technology changed in
    /// a way a screen reader should hear about, such as its name: the
    /// frame lists it in [`Frame::access`](crate::Frame::access), for the
    /// window layer to tell the platform. It draws nothing by itself.
    pub fn request_access_update(&mut self) {
        self.requests.access = true;
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

    /// Shows only some of the widget's children from this frame on (see
    /// [`Shown`]); the others, and everything inside them, are kept with
    /// their state but cost nothing until they are shown again. The
    /// window is not built again: the root lays out again, draws what the
    /// change uncovered and what it hid, and nothing else.
    ///
    /// A widget inside a child that is hidden loses the focus, the
    /// pointer and the hover. Asking for what is already shown changes
    /// nothing; a widget usually asks only when what it shows changed.
    ///
    /// ```
    /// use std::cell::Cell;
    /// use std::rc::Rc;
    /// use manguebyte_gfx::{Scene, Size};
    /// use manguebyte_ui::access::{AccessNode, Role};
    /// use manguebyte_ui::layout::Style;
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{
    ///     AccessCx, MeasureCx, PaintCx, Shown, UpdateCx, View, Widget, label,
    /// };
    ///
    /// /// Shows one of its children, the one `page` says.
    /// struct OneOf(Rc<Cell<usize>>);
    ///
    /// impl Widget for OneOf {
    ///     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
    ///         AccessNode::new(Role::Group, "Pages")
    ///     }
    ///     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
    ///         Size::default()
    ///     }
    ///     fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
    ///     fn update(&mut self, cx: &mut UpdateCx<'_>) {
    ///         cx.show(Shown::Only(self.0.get()));
    ///     }
    /// }
    ///
    /// let page = Rc::new(Cell::new(0));
    /// let view = View::with_children(OneOf(page.clone()), Style::stack(), (label("One"), label("Two")));
    /// let mut ui = Harness::new(view, Size::new(100.0, 20.0), 1.0);
    /// ui.frame();
    /// assert!(ui.find(Role::Label, "One").is_some());
    /// assert!(ui.find(Role::Label, "Two").is_none(), "hidden, so not described");
    /// page.set(1);
    /// ui.frame();
    /// assert!(ui.find(Role::Label, "One").is_none());
    /// assert!(ui.find(Role::Label, "Two").is_some());
    /// ```
    pub fn show(&mut self, shown: Shown) {
        self.requests.shown = Some(shown);
    }

    /// Makes the subtree of the widget's child at position `child` modal,
    /// or ends that with `None`: while it is, only the widgets inside it
    /// can be hit by the pointer, take the focus (Tab goes round inside
    /// it) or be acted on by assistive technology, keys that the widget
    /// with the focus does not use go to that child, and every key is
    /// used. Open menus close, and a drag or a held pointer outside ends.
    /// When it ends, the focus goes back to the widget that had it before.
    /// One subtree is modal at a time; asking for another ends the first.
    pub fn set_modal(&mut self, child: Option<usize>) {
        self.requests.modal = Some(child);
    }

    /// Takes the keyboard focus at the end of this frame's polling, if
    /// the widget accepts [`Action::Focus`] then: what a dialog's default
    /// button does when the dialog opens.
    pub fn request_focus(&mut self) {
        self.requests.focus = true;
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
    /// The root's amounts for the widget: hover, press, focus ring.
    pub(crate) amounts: (f32, f32, f32),
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

    /// How far the hover has come in, from 0 (not hovered) to 1, eased over
    /// the theme's [`fast`](Motion::fast) duration: what a widget blends its
    /// hover look by.
    pub fn hover(&self) -> f32 {
        self.amounts.0
    }

    /// How far a press on the widget has come in, from 0 to 1: the pointer
    /// went down on it and has not been let go.
    pub fn press(&self) -> f32 {
        self.amounts.1
    }

    /// How far the focus ring has come in, from 0 to 1. It comes in only
    /// while the widget has the focus and the focus should be shown (see
    /// [`Root::focus_visible`](crate::Root::focus_visible)).
    pub fn focus_ring(&self) -> f32 {
        self.amounts.2
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
    pub(crate) motion: Motion,
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

    /// The motion tokens in effect (see [`UpdateCx::motion`]).
    pub fn motion(&self) -> Motion {
        self.motion
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
