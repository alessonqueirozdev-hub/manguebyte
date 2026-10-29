//! What rows, columns, stacks and panels are.

use manguebyte_gfx::{Color, RoundedRect, Scene, Size};

use crate::access::{AccessNode, Role};
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};

/// What a container is filled with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fill {
    Nothing,
    Color(Color),
    /// The theme's surface colour, with its large corners.
    Panel,
}

/// The widget behind [`row`](crate::row), [`column`](crate::column),
/// [`stack`](crate::stack), [`panel`](crate::panel) and
/// [`filled`](crate::filled): a group that may draw a background behind
/// its children. Its size and its children's places come from layout.
///
/// Its accessible name is empty unless the view is
/// [`named`](crate::IntoView::named).
#[derive(Debug, Clone)]
pub struct Container {
    fill: Fill,
}

impl Container {
    /// A container that draws nothing, or a rectangle of `fill`.
    pub fn new(fill: Option<Color>) -> Container {
        Container {
            fill: fill.map_or(Fill::Nothing, Fill::Color),
        }
    }

    /// A container drawn as a panel: the theme's surface colour, with its
    /// large corners. [`panel`](crate::panel) puts one view on it; with
    /// [`View::with_children`](crate::View::with_children) it can hold a
    /// row of them.
    ///
    /// ```
    /// use manguebyte_gfx::Size;
    /// use manguebyte_ui::layout::{Insets, Style};
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{Container, IntoView, Role, View, label};
    ///
    /// let style = Style::row().with_spacing(6.0).with_padding(Insets::all(4.0));
    /// let view = View::with_children(Container::panel(), style, (label("In"), label("Out")))
    ///     .named("Routing");
    /// let mut ui = Harness::new(view, Size::new(100.0, 30.0), 1.0);
    /// ui.frame();
    /// let group = ui.get(Role::Group, "Routing");
    /// let (inward, out) = (ui.get(Role::Label, "In"), ui.get(Role::Label, "Out"));
    /// assert_eq!(inward.bounds.x, 4.0);
    /// assert!(out.bounds.x >= inward.bounds.right() + 5.0);
    /// assert!(group.bounds.contains(out.center()));
    /// assert!(ui.dump().contains("rounded_rect(0 0 100 30 radii 18 18 18 18)"));
    /// ```
    pub fn panel() -> Container {
        Container { fill: Fill::Panel }
    }
}

impl Widget for Container {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        match self.fill {
            Fill::Nothing => {}
            Fill::Color(color) => scene.fill(rect, color),
            Fill::Panel => {
                let theme = cx.theme();
                scene.fill(
                    RoundedRect::new(rect, theme.corners.xl),
                    theme.palette.surface,
                );
            }
        }
    }
}
