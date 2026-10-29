//! A view that scrolls over one child larger than itself.

use manguebyte_gfx::{Point, Scene, Size};

use crate::access::{AccessNode, AccessRange, Action, Actions, Role};
use crate::event::{Button, Event, Handled};
use crate::layout::{Arrange, Style};
use crate::scroll::{Bars, Geometry, ScrollMetrics, paint_bars};
use crate::source::State;
use crate::view::{IntoView, View};
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// The widget behind [`scroll_view`]: a viewport onto one child that is
/// larger than it, with scroll bars at its right and bottom edges.
///
/// The [`Root`](crate::Root) moves the child by the offset and clips it to
/// the viewport (see [`crate::scroll`]), so the child and everything in it
/// are ordinary widgets: a press, the wheel, Tab and assistive technology
/// reach them as anywhere else, and only what can be seen is drawn or can
/// be pressed. The view itself takes:
///
/// - the wheel, and a touchpad's finer steps, which arrive as fractions of
///   a notch: [`Sizes::scroll_line`](crate::Sizes::scroll_line) a notch,
///   sideways with Shift held. The wheel is used only while the view can
///   move the way it turns, so a view inside another scrolls until its
///   edge and then leaves the wheel to the outer one;
/// - presses and drags on its bars: dragging the thumb, or a press on the
///   track to move a page towards it;
/// - with the keyboard focus, which it takes when its content overflows:
///   the arrows a line, Page Up and Page Down a page, Home and End to the
///   ends.
///
/// Tab, or assistive technology, giving the focus to a widget scrolled out
/// of sight scrolls the views it is in until it can be seen, and a drag
/// near a viewport's edge scrolls it (see [`crate::drag`]).
///
/// The room for the bars is kept whether or not the child overflows, so
/// that the child does not move when it grows past the viewport. Bars are
/// drawn in the theme's colours, in the focus colour while the view has
/// the focus.
pub struct ScrollView {
    name: String,
    bars: Bars,
    /// Where the application keeps the offset, if it does.
    position: Option<State<Point>>,
    /// The version of `position` last acted on.
    seen: Option<u64>,
    /// The offset last written into `position`, or asked of it.
    written: Point,
}

/// A view scrolling over `child`, up and down, named `name` for assistive
/// technology.
///
/// ```
/// use manguebyte_gfx::{Point, Size};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Key, Role, State, column, label, scroll_view};
///
/// let rows: Vec<_> = (0..20).map(|i| label(format!("Row {i}")).height(20.0)).collect();
/// let offset = State::new(Point::ZERO);
/// let view = scroll_view("Rows", column(rows)).position(offset.clone());
/// let mut ui = Harness::new(view, Size::new(120.0, 100.0), 1.0);
/// ui.frame();
/// let rows = ui.get(Role::ScrollView, "Rows");
/// assert_eq!(rows.value_text(), Some("0 %"));
///
/// // One notch down scrolls one line of the theme, 40 pixels.
/// ui.wheel(rows.center(), -1.0);
/// ui.frame();
/// ui.frame();
/// assert_eq!(offset.get(), Point::new(0.0, 40.0));
/// assert_eq!(ui.get(Role::Label, "Row 2").bounds.y, 0.0);
///
/// // The application scrolls it by setting the state.
/// offset.set(Point::new(0.0, 300.0));
/// ui.frame();
/// assert_eq!(ui.get(Role::ScrollView, "Rows").value_text(), Some("100 %"));
/// ```
pub fn scroll_view(name: impl Into<String>, child: impl IntoView) -> ScrollViewBuilder {
    ScrollViewBuilder {
        name: name.into(),
        child: child.into_view(),
        x: false,
        y: true,
        position: None,
    }
}

/// A [`scroll_view`] being set up: which ways it scrolls and where its
/// offset is kept. It is a view like any other.
pub struct ScrollViewBuilder {
    name: String,
    child: View,
    x: bool,
    y: bool,
    position: Option<State<Point>>,
}

impl ScrollViewBuilder {
    /// It scrolls sideways only.
    #[must_use]
    pub fn horizontal(mut self) -> Self {
        (self.x, self.y) = (true, false);
        self
    }

    /// It scrolls both ways.
    #[must_use]
    pub fn both(mut self) -> Self {
        (self.x, self.y) = (true, true);
        self
    }

    /// Keeps the offset in `position`, in logical pixels from the child's
    /// top-left corner: the view writes it whenever it scrolls, and scrolls
    /// to whatever the application sets there, on the next frame, kept
    /// within the child. A plugin's editor spec can keep the state, so
    /// that the view opens where it was.
    #[must_use]
    pub fn position(mut self, position: State<Point>) -> Self {
        self.position = Some(position);
        self
    }
}

impl IntoView for ScrollViewBuilder {
    fn into_view(self) -> View {
        let widget = ScrollView {
            name: self.name,
            bars: Bars::default(),
            position: self.position,
            seen: None,
            written: Point::ZERO,
        };
        let style = Style {
            arrange: Arrange::Scroll {
                x: self.x,
                y: self.y,
            },
            ..Style::leaf()
        };
        View::with_children(widget, style, (self.child,))
    }
}

impl ScrollView {
    fn geometry(metrics: &ScrollMetrics, cx_bar: f32) -> Geometry {
        Geometry::from_metrics(metrics, cx_bar)
    }

    /// The axis assistive technology sees: up and down, unless the view
    /// only overflows sideways.
    fn vertical(metrics: &ScrollMetrics) -> bool {
        metrics.overflows_y() || !metrics.overflows_x()
    }

    fn apply(cx: &mut EventCx<'_>, to: (f64, f64)) {
        cx.scroll_to(Point::new(to.0 as f32, to.1 as f32));
        cx.request_paint();
    }
}

impl Widget for ScrollView {
    fn access(&self, cx: &AccessCx<'_>) -> AccessNode {
        let mut node = AccessNode::new(Role::ScrollView, self.name.as_str());
        let Some(metrics) = cx.scroll() else {
            return node;
        };
        let vertical = Self::vertical(&metrics);
        let max = metrics.max_offset();
        let (at, max) = if vertical {
            (metrics.offset.y, max.y)
        } else {
            (metrics.offset.x, max.x)
        };
        let percent = if max > 0.0 {
            (at / max * 100.0).round()
        } else {
            0.0
        };
        node = node.with_value(format!("{percent} %"));
        if max > 0.0 {
            node = node
                .with_numeric(f64::from(at), AccessRange::new(0.0, f64::from(max), None))
                .with_actions(Actions::ADJUST);
        }
        node
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        // The room for the bars, which layout keeps at the edges it
        // scrolls along.
        let bar = cx.theme().sizes.scroll_bar.max(0.0);
        Size::new(bar, bar)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let (Some(position), Some(metrics)) = (&self.position, cx.scroll()) else {
            return;
        };
        let version = position.version();
        if self.seen != Some(version) {
            // The application set it, or this is the first frame.
            self.seen = Some(version);
            let wanted = position.get();
            self.written = wanted;
            if wanted != metrics.offset {
                cx.scroll_to(wanted);
            }
        } else if metrics.offset != self.written {
            // It scrolled: by the person, by Tab, by a drag, or kept within
            // a child that changed size.
            self.written = metrics.offset;
            position.set(metrics.offset);
            self.seen = Some(position.version());
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        if let Some(metrics) = cx.scroll() {
            let geometry = Self::geometry(&metrics, cx.theme().sizes.scroll_bar);
            paint_bars(scene, &geometry, cx.theme(), cx.is_focused());
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let Some(metrics) = cx.scroll() else {
            return Handled::No;
        };
        let theme = cx.theme();
        let geometry = Self::geometry(&metrics, theme.sizes.scroll_bar);
        let line = theme.sizes.scroll_line;
        let outcome = self
            .bars
            .event(&geometry, event, cx.is_focused(), line, cx.scale());
        if let Some(to) = outcome.scroll_to {
            Self::apply(cx, to);
        }
        if let Event::PointerDown {
            button: Button::Primary,
            ..
        } = event
        {
            if self.bars.dragging() {
                cx.capture_pointer();
            }
            // A press on the view itself, not on what it shows, gives it
            // the keyboard, if it scrolls.
            cx.request_focus();
            cx.request_paint();
            return Handled::Yes;
        }
        outcome.handled
    }

    fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        let Some(metrics) = cx.scroll() else {
            return false;
        };
        let vertical = Self::vertical(&metrics);
        let line = cx.theme().sizes.scroll_line;
        let offset = metrics.offset;
        let along = |by: f32| {
            if vertical {
                Point::new(offset.x, offset.y + by)
            } else {
                Point::new(offset.x + by, offset.y)
            }
        };
        let to = match action {
            Action::Increment => along(line),
            Action::Decrement => along(-line),
            Action::SetValue(value) if value.is_finite() => {
                if vertical {
                    Point::new(offset.x, value as f32)
                } else {
                    Point::new(value as f32, offset.y)
                }
            }
            _ => return false,
        };
        cx.scroll_to(to);
        cx.request_paint();
        true
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, _focused: bool) {
        cx.request_paint();
    }
}
