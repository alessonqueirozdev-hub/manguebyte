//! Floating panels: cards over the interface that do not stop it, each
//! with a title bar it is moved by and a button that closes it.

use std::cell::{Cell, RefCell};
use std::fmt;
use std::rc::Rc;

use manguebyte_gfx::{Align as TextAlign, Circle, Path, Point, Scene, Size, Stroke, TextStyle};

use super::{
    Cached, Container, draw_text, floating, floating_overflow, focus_ring, hline, line,
    line_height, text_width, top_highlight,
};
use crate::access::{AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::layout::{Align, Insets, Style};
use crate::motion::Lerp;
use crate::root::FloatMetrics;
use crate::source::{State, TextSource};
use crate::view::{IntoView, View};
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, Shown, UpdateCx, Widget};

/// The height of a panel's title bar.
pub const TITLE_BAR_HEIGHT: f32 = 36.0;

/// Space inside a panel, around its content.
const PADDING: f32 = 14.0;

/// Space at the title bar's ends.
const BAR_START: f32 = 14.0;
const BAR_END: f32 = 6.0;

/// The close button's side, and the room around it for its focus ring.
const CLOSE: f32 = 24.0;
const CLOSE_ROOM: f32 = 3.0;

/// The child position a closed panel's slot shows: none.
const NONE: usize = 1;

/// A panel that floats over the interface, inside the window: a card with
/// a title bar and any view inside, put over a view with
/// [`IntoView::with_panels`]. It is not modal: the interface around and
/// under it goes on taking the pointer, the keys and assistive
/// technology, and there is no scrim. It lives inside the window, never
/// in a window of the system's, so it works the same in a standalone
/// program and in a plugin's editor inside a host.
///
/// - **Open and closed** follow a `State<bool>` the application and the
///   panel share, as [`tabs`](crate::tabs) follow a `State<usize>`: set it
///   to open or close the panel; the close button and Escape set it to
///   `false`. A [`Pill`](crate::Pill) can [open](crate::Pill::opens) it.
///   Closed, the panel is hidden (see [`Shown`]): nothing inside it is
///   polled, laid out, drawn or described, and it costs nothing.
/// - **Where it is** follows a `State<Point>`: its top-left corner from
///   the top-left corner of the view it is over, in logical pixels. Its
///   title bar moves it, and writes where it was let go into the state,
///   so a project that keeps the state reopens it there. It is always
///   kept inside the view, which a window smaller than the state says
///   does without changing the state.
/// - **Its size** is its content's natural size, laid out again when the
///   content's changes, as when the language changes its texts.
/// - **Several** can be open at once; a press anywhere on one, or the
///   focus moving into it, brings it in front of the others, and so does
///   opening it.
/// - **The focus** goes, when it opens, to the first widget inside that
///   takes it (Tab goes through the content, then the close button), and
///   when it closes goes back to the widget that had it before, or else
///   the one pressed to open it (a pill, which does not take the focus
///   when clicked), if the focus was inside or nowhere. Escape that the
///   widget with the focus does not use closes the panel it is in, and
///   no other. A panel open when the interface is built does not take
///   the focus.
///
/// For assistive technology it is a [`Region`](Role::Region) named by its
/// title, which is a [`TextSource`] and follows the language; its close
/// button is a [`Button`](Role::Button) named "Close" until given a name
/// in the interface's language with
/// [`close_label`](FloatingPanel::close_label).
///
/// It floats with the theme's large shadow, on the overlay surface, as
/// menus do. Moving it draws again what it covered and what it covers,
/// shadow included, and nothing else; an idle panel draws nothing.
///
/// ```
/// use manguebyte_gfx::{Point, Size};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Key, Role, State, floating_panel, label, pill, row};
///
/// let open = State::new(false);
/// let at = State::new(Point::new(40.0, 60.0));
/// let view = row((pill("Reverb").opens(&open),)).padding(8.0).with_panels([floating_panel(
///     "Reverb",
///     open.clone(),
///     at.clone(),
///     label("Size and decay"),
/// )]);
/// let mut ui = Harness::new(view, Size::new(400.0, 300.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Region, "Reverb").is_none(), "closed");
///
/// ui.click(ui.get(Role::Button, "Reverb").center());
/// ui.frame();
/// let panel = ui.get(Role::Region, "Reverb");
/// assert_eq!((panel.bounds.x, panel.bounds.y), (40.0, 60.0));
/// // Its title bar moves it, and the state remembers where.
/// let bar = Point::new(panel.bounds.x + 30.0, panel.bounds.y + 12.0);
/// ui.drag(bar, (100.0, 20.0), 4);
/// assert_eq!(at.get(), Point::new(140.0, 80.0));
/// ui.frame();
/// assert_eq!(ui.get(Role::Region, "Reverb").bounds.x, 140.0);
///
/// // Its close button has the focus (nothing else inside takes it):
/// // Escape closes it, and the focus goes back to the pill.
/// assert!(ui.get(Role::Button, "Close").is_focused());
/// ui.key(Key::Escape);
/// assert!(!open.get());
/// ui.frame();
/// assert!(ui.find(Role::Region, "Reverb").is_none());
/// assert!(ui.get(Role::Button, "Reverb").is_focused());
/// ```
pub struct FloatingPanel {
    title: TextSource,
    open: State<bool>,
    position: State<Point>,
    content: View,
    close: TextSource,
}

/// A panel titled `title`, showing `content`, open while `open` is
/// `true`, its top-left corner at `position` (see [`FloatingPanel`]).
pub fn floating_panel(
    title: impl Into<TextSource>,
    open: State<bool>,
    position: State<Point>,
    content: impl IntoView,
) -> FloatingPanel {
    FloatingPanel {
        title: title.into(),
        open,
        position,
        content: content.into_view(),
        close: TextSource::fixed("Close"),
    }
}

impl FloatingPanel {
    /// The same panel with its close button named `label`, in the
    /// interface's language.
    #[must_use]
    pub fn close_label(mut self, label: impl Into<TextSource>) -> FloatingPanel {
        self.close = label.into();
        self
    }

    /// The panel as a floating child of the view it is put over.
    fn into_view(self) -> View {
        let FloatingPanel {
            title,
            open,
            position,
            content,
            close,
        } = self;
        let shared = Rc::new(Shared {
            title: RefCell::new(title.as_str().to_owned()),
            version: Cell::new(0),
        });
        let body = View::with_children(
            Container::new(None),
            Style::column().with_padding(Insets::new(
                PADDING,
                TITLE_BAR_HEIGHT + PADDING * 0.5,
                PADDING,
                PADDING,
            )),
            (content,),
        );
        let bar = View::with_children(
            TitleBar {
                position: position.clone(),
                grab: None,
            },
            Style::row()
                .with_align(Align::Center)
                .with_spacing(8.0)
                .with_padding(Insets::new(BAR_START, 0.0, BAR_END, 0.0)),
            (
                View::new(Title {
                    shared: Rc::clone(&shared),
                    seen: 0,
                    layout: Cached::new(),
                })
                .flex(1.0),
                CloseButton {
                    open: open.clone(),
                    label: close,
                    pressed: false,
                },
            ),
        )
        .height(TITLE_BAR_HEIGHT);
        let card = View::with_children(Card { title, shared }, Style::stack(), (body, bar));
        View::with_children(
            Slot {
                open,
                position,
                seen_open: None,
                seen_position: None,
                shown: false,
            },
            Style::stack().with_align_self(Align::Start),
            (card,),
        )
    }
}

impl fmt::Debug for FloatingPanel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FloatingPanel")
            .field("title", &self.title.as_str())
            .field("open", &self.open.get())
            .field("position", &self.position.get())
            .finish_non_exhaustive()
    }
}

/// `content` with `panels` floating over it.
pub(crate) fn over(content: View, panels: impl IntoIterator<Item = FloatingPanel>) -> View {
    let mut children = vec![content];
    children.extend(panels.into_iter().map(FloatingPanel::into_view));
    View::with_children(Container::new(None), Style::stack(), children)
}

/// What a panel's parts share: its title as last read, for the title bar
/// to draw.
struct Shared {
    title: RefCell<String>,
    /// Counts up when it changes.
    version: Cell<u64>,
}

/// The floating widget: shows the card while the panel is open, and is
/// where the root places it.
struct Slot {
    open: State<bool>,
    position: State<Point>,
    seen_open: Option<u64>,
    seen_position: Option<u64>,
    shown: bool,
}

impl Widget for Slot {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.position.version();
        if self.seen_position != Some(version) {
            self.seen_position = Some(version);
            cx.float_at(self.position.get());
        }
        let version = self.open.version();
        if self.seen_open == Some(version) {
            return;
        }
        let first = self.seen_open.is_none();
        self.seen_open = Some(version);
        let open = self.open.get();
        if open {
            if !self.shown {
                self.shown = true;
                cx.show(Shown::Only(0));
            }
            // Opened, or asked to open while open: in front, and the focus
            // inside, unless it was open from the start.
            cx.raise();
            if !first {
                cx.focus_within();
            }
        } else if self.shown || first {
            self.shown = false;
            cx.show(Shown::Only(NONE));
        }
    }

    fn event(&mut self, _cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::Key(KeyEvent {
                key: Key::Escape,
                pressed: true,
                ..
            }) if self.shown => {
                self.open.set(false);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// The card: the floating surface, and the region assistive technology
/// sees.
struct Card {
    title: TextSource,
    shared: Rc<Shared>,
}

impl Widget for Card {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Region, self.title.as_str())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        floating_overflow(cx.theme())
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.title.poll() {
            let mut kept = self.shared.title.borrow_mut();
            kept.clear();
            kept.push_str(self.title.as_str());
            self.shared
                .version
                .set(self.shared.version.get().wrapping_add(1));
            cx.request_access_update();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let radius = theme.corners.lg;
        floating(scene, rect, radius, theme, cx.scale());
        top_highlight(scene, rect, radius, cx.scale(), theme.palette.highlight);
    }

    fn event(&mut self, _cx: &mut EventCx<'_>, event: &Event) -> Handled {
        // A press or a turn of the wheel on the card's own surface is the
        // panel's: nothing under it hears it.
        match event {
            Event::PointerDown { .. } | Event::Wheel { .. } => Handled::Yes,
            _ => Handled::No,
        }
    }
}

/// The title bar: what moves the panel.
struct TitleBar {
    position: State<Point>,
    /// While it is dragged: where the pointer went down, from the panel's
    /// corner, and where the panel was then.
    grab: Option<(Point, FloatMetrics)>,
}

impl Widget for TitleBar {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let theme = cx.theme();
        // A hairline between the title and the content.
        hline(
            scene,
            rect.bottom() - 1.0 / cx.scale().max(1.0),
            (rect.x + 1.0, rect.right() - 1.0),
            1.0,
            cx.scale(),
            theme.palette.hairline,
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                ..
            } => {
                let Some(metrics) = cx.floating() else {
                    return Handled::No;
                };
                let corner = Point::new(
                    metrics.area.x + metrics.position.x,
                    metrics.area.y + metrics.position.y,
                );
                self.grab = Some((Point::new(at.x - corner.x, at.y - corner.y), metrics));
                cx.capture_pointer();
                Handled::Yes
            }
            Event::PointerMove { at, .. } => {
                let Some((grab, metrics)) = self.grab else {
                    return Handled::No;
                };
                let to = metrics.clamp(Point::new(
                    at.x - metrics.area.x - grab.x,
                    at.y - metrics.area.y - grab.y,
                ));
                if to != self.position.get() {
                    self.position.set(to);
                }
                Handled::Yes
            }
            Event::PointerUp {
                button: Button::Primary,
                ..
            } if self.grab.is_some() => {
                self.grab = None;
                cx.release_pointer();
                Handled::Yes
            }
            Event::PointerCancel => {
                self.grab = None;
                Handled::No
            }
            _ => Handled::No,
        }
    }
}

/// The title, drawn in the title bar.
struct Title {
    shared: Rc<Shared>,
    seen: u64,
    layout: Cached<(u64, TextStyle)>,
}

impl Title {
    fn style(theme: &crate::Theme) -> TextStyle {
        theme
            .typography
            .heading
            .with_weight(600.0)
            .with_color(theme.palette.text)
            .with_align(TextAlign::Start)
    }
}

impl Widget for Title {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Label, self.shared.title.borrow().as_str())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = Title::style(cx.theme());
        Size::new(
            text_width(&self.shared.title.borrow(), &style),
            line_height(&style),
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.shared.version.get();
        if version != self.seen {
            self.seen = version;
            cx.request_layout();
            cx.request_access_update();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let style = Title::style(cx.theme());
        let title = &self.shared.title;
        let layout = self.layout.get(rect.width, (self.seen, style), || {
            line(&title.borrow(), &style, rect.width)
        });
        let y = rect.y + (rect.height - line_height(&style)) * 0.5;
        draw_text(scene, layout, Point::new(rect.x, y));
    }
}

/// The button that closes the panel: an X in a circle that lights under
/// the pointer.
struct CloseButton {
    open: State<bool>,
    label: TextSource,
    pressed: bool,
}

impl Widget for CloseButton {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Button, self.label.as_str()).with_actions(Actions {
            press: true,
            focus: true,
            ..Actions::NONE
        })
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        let side = CLOSE + 2.0 * CLOSE_ROOM;
        Size::new(side, side)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.label.poll() {
            cx.request_access_update();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let rect = cx.rect();
        let center = rect.center();
        let radius = (rect.width.min(rect.height) * 0.5 - CLOSE_ROOM).max(0.0);
        let hover = cx.hover().max(cx.press());
        if hover > 0.0 {
            let face = palette
                .raised
                .lerp(palette.hairline_strong, 0.5 * cx.press());
            scene.fill(Circle::new(center, radius), face.with_opacity(hover));
        }
        let ink = palette.text_secondary.lerp(palette.text, hover);
        let arm = radius * 0.36;
        let mut cross = Path::new();
        cross
            .move_to(Point::new(center.x - arm, center.y - arm))
            .line_to(Point::new(center.x + arm, center.y + arm))
            .move_to(Point::new(center.x + arm, center.y - arm))
            .line_to(Point::new(center.x - arm, center.y + arm));
        scene.stroke(&cross, Stroke::round(1.5), ink);
        focus_ring(
            scene,
            Circle::new(center, radius + 1.0),
            theme,
            cx.focus_ring(),
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                button: Button::Primary,
                ..
            } => {
                self.pressed = true;
                cx.capture_pointer();
                Handled::Yes
            }
            Event::PointerUp {
                at,
                button: Button::Primary,
                ..
            } => {
                if std::mem::take(&mut self.pressed) && cx.rect().contains(at) {
                    self.open.set(false);
                }
                cx.release_pointer();
                Handled::Yes
            }
            Event::PointerCancel => {
                self.pressed = false;
                Handled::No
            }
            Event::Key(KeyEvent {
                key: Key::Character(' ') | Key::Enter,
                pressed: true,
                ..
            }) => {
                self.open.set(false);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, _cx: &mut EventCx<'_>, action: Action) -> bool {
        if action != Action::Press {
            return false;
        }
        self.open.set(false);
        true
    }
}
