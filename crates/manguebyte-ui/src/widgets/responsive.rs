//! A container that shows the first of its children that fits the room it
//! is given.

use manguebyte_gfx::{Scene, Size};

use crate::access::{AccessNode, Role};
use crate::layout::{Arrange, Style};
use crate::view::{Children, IntoView, View};
use crate::widget::{AccessCx, MeasureCx, PaintCx, Shown, UpdateCx, Widget};

/// How far below its smallest size a child that is showing stays, in
/// logical pixels, unless [`Responsive::margin`] says otherwise.
pub const RESPONSIVE_MARGIN: f32 = 16.0;

/// What a room can come out short of a smallest size it should reach, in
/// logical pixels, from adding up lengths in single precision.
const ROOM_SLACK: f32 = 0.01;

/// Several layouts of one interface, of which the first that fits the room
/// is shown: what [`responsive`] makes.
///
/// Each child but the last has a smallest size, given in order with
/// [`below`](Responsive::below), and fits when the room is at least that on
/// both axes. The last fits whatever the room. A child given no size fits
/// anywhere, so the children after it never show.
///
/// # Built once
///
/// Every child is built once, with the window, and switched with
/// [`Shown`]. Dragging a window's border rebuilds nothing, and a child that
/// is not showing keeps every value, scroll offset and selection inside it,
/// and costs nothing until it shows again. What is bound to the same
/// [`State`](crate::State) or parameter in two children stays the same in
/// both.
///
/// # The margin
///
/// A child comes in as soon as the room reaches its smallest size, and
/// once showing stays until the room falls [`RESPONSIVE_MARGIN`] logical
/// pixels below that (see [`margin`](Responsive::margin)), so a border held
/// at the boundary does not flicker. So give a size the child can bear being
/// that much less than. A window that jumps straight to a smallest size,
/// as a layout zoomed to fit does at its design size, is the child's.
///
/// # What decides, and when
///
/// It decides from the room layout gives it (see [`Widget::adapt`]), not
/// from the window, so it works inside a panel, a row or another
/// `responsive`. It decides during layout, before anything is drawn, so no
/// frame shows a child in a room it does not fit, the first frame included.
/// The child that comes in is polled in that same frame.
///
/// The choice does not depend on the child showing. The room it is made
/// from is worked out as if, on each axis that has a smallest size, the
/// natural size were the largest smallest size, whichever child shows (see
/// [`rooms`](crate::layout::rooms)); it is not snapped to device pixels, so it
/// does not depend on the scale either. So a row or column that gives it no
/// more than its natural size does not strand it in the small layout, and a
/// choice stays made however often the interface is laid out. Where it is then
/// placed is by the natural size of the child showing, with that smallest size
/// as a floor, which can be more than the room it was chosen by: a child given
/// less room than it asks for shrinks as any other does.
///
/// A parent that aligns it to its natural size gives it no less than that
/// floor, so give it [`flex`](IntoView::flex) or a stretching alignment along
/// an axis that has a smallest size. On an axis with no smallest size its
/// natural size is the child's, as for a stack, and that can change the room
/// of a second `responsive` beside it.
///
/// # What goes with a child that is hidden
///
/// A widget in the child being hidden loses the keyboard focus, and the
/// focus then goes nowhere: Tab brings it to the first control of the child
/// that came in, and a text input takes what was typed, as when it loses the
/// focus any other way. A control held by the pointer has its gesture ended.
/// Open menus and tooltips close, since they hang from the layout that is
/// going.
///
/// # With zoom to fit
///
/// `manguebyte_gui`'s `Editor::with_zoom_to_fit` draws the layout at the
/// editor's design size, so the room here is the design size whatever the
/// window, and the first child stays; below its `min_zoom` the room shrinks,
/// and so the second comes in, at that zoom. For a compact layout at its real
/// size, give the editor `with_real_size_below` the same size as `below` here:
/// below it the zoom is 1 and the room is the window.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{IntoView, Role, label, responsive};
///
/// let view = responsive((label("Full console"), label("Summary"))).below(Size::new(800.0, 500.0));
/// let mut ui = Harness::new(view, Size::new(1000.0, 600.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Full console").is_some());
/// assert!(ui.find(Role::Label, "Summary").is_none(), "hidden, so not described");
///
/// // Half a screen: too narrow for the first child, in one frame.
/// ui.resize(Size::new(700.0, 600.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Full console").is_none());
/// assert!(ui.find(Role::Label, "Summary").is_some());
///
/// // The way back needs the whole of the smallest size.
/// ui.resize(Size::new(800.0, 600.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Full console").is_some());
/// // And the way down the margin below it.
/// ui.resize(Size::new(790.0, 600.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Full console").is_some());
/// ```
pub struct Responsive {
    children: Vec<View>,
    smallest: Vec<Size>,
    margin: f32,
}

/// The children given, of which the first that fits the room it is given
/// shows (see [`Responsive`]): usually the largest layout first, each with
/// its [`below`](Responsive::below), and a last one that fits anywhere.
pub fn responsive(children: impl Children) -> Responsive {
    Responsive {
        children: children.into_children(),
        smallest: Vec::new(),
        margin: RESPONSIVE_MARGIN,
    }
}

impl Responsive {
    /// The same container, the next child that has no smallest size yet
    /// given `smallest`: below it the room goes to the children after.
    /// A length of 0 asks nothing of that axis. A side that is negative or
    /// not a number counts as 0.
    #[must_use]
    pub fn below(mut self, smallest: Size) -> Responsive {
        let clean = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
        self.smallest
            .push(Size::new(clean(smallest.width), clean(smallest.height)));
        self
    }

    /// The same container, a child that is showing staying until the room
    /// falls `margin` logical pixels below its smallest size. 0 changes at
    /// the smallest size itself, both ways. Negative or not a number counts
    /// as 0.
    #[must_use]
    pub fn margin(mut self, margin: f32) -> Responsive {
        self.margin = if margin.is_finite() {
            margin.max(0.0)
        } else {
            0.0
        };
        self
    }
}

impl IntoView for Responsive {
    fn into_view(self) -> View {
        let Responsive {
            children,
            smallest,
            margin,
        } = self;
        let chooser = Chooser {
            count: children.len(),
            smallest,
            margin,
            current: 0,
            started: false,
            decided: false,
        };
        let style = Style {
            arrange: Arrange::Adaptive,
            ..Style::leaf()
        };
        View::with_children(chooser, style, children)
    }
}

/// The widget behind [`Responsive`]: a group that draws nothing itself.
struct Chooser {
    count: usize,
    smallest: Vec<Size>,
    margin: f32,
    current: usize,
    started: bool,
    decided: bool,
}

impl Chooser {
    /// The child to show in `room`.
    fn choose(&self, room: Size) -> usize {
        let last = self.count.saturating_sub(1);
        (0..last)
            .find(|&at| {
                let smallest = self.smallest.get(at).copied().unwrap_or_default();
                // Until it has decided, nothing is showing to hold on to.
                let holds = self.decided && at == self.current;
                let margin = if holds { self.margin } else { 0.0 };
                room.width + ROOM_SLACK >= smallest.width - margin
                    && room.height + ROOM_SLACK >= smallest.height - margin
            })
            .unwrap_or(last)
    }
}

impl Widget for Chooser {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    /// The largest smallest size on each axis: a floor under the natural
    /// size, and all of it in the room chosen by (see [`Arrange::Adaptive`]).
    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        self.smallest.iter().fold(Size::default(), |floor, size| {
            Size::new(floor.width.max(size.width), floor.height.max(size.height))
        })
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        // The first frame polls and measures only one child; `adapt` then
        // says which, before anything is drawn.
        if !self.started {
            self.started = true;
            cx.show(Shown::Only(self.current));
        }
    }

    fn adapt(&mut self, _cx: &MeasureCx<'_>, room: Size) -> Option<Shown> {
        // A window with no area (minimised, or not sized yet) says nothing
        // about what fits it.
        if self.count == 0 || room.width <= 0.0 || room.height <= 0.0 {
            return None;
        }
        self.current = self.choose(room);
        self.decided = true;
        Some(Shown::Only(self.current))
    }
}
