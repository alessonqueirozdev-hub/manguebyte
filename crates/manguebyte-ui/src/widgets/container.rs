//! What rows, columns, stacks and panels are.

use manguebyte_gfx::{Color, RoundedRect, Scene, Size};

use super::{drop_shadow, hairline, top_highlight, vertical};
use crate::access::{AccessNode, Role};
use crate::theme::Theme;
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};

/// What a container is filled with.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Fill {
    Nothing,
    Color(Color),
    /// A card: the theme's raised surface fading to its surface, a
    /// hairline, the light along its top and its shadow.
    Panel,
}

/// How far a card's shadow reaches beyond it in `theme`.
fn card_overflow(theme: &Theme) -> f32 {
    let shadow = theme.elevation.md;
    if shadow.color.a == 0 {
        return 0.0;
    }
    let offset = shadow.offset.x.abs().max(shadow.offset.y.abs());
    (shadow.reach_exact() + offset).max(0.0).ceil()
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

    /// A container drawn as a card: the theme's raised surface fading down
    /// to its surface, with the card radius, a hairline border, the light
    /// along its top edge and the card shadow, which reaches over what is
    /// around it (see [`Widget::overflow`]). [`panel`](crate::panel) and
    /// [`card`](crate::card) put one view on it; with
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
    /// // A card: a shadow, the surface, rounded to half its height at most.
    /// assert!(ui.dump().contains("shadow rounded_rect(0 0 100 30 radii 15 15 15 15)"));
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

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        match self.fill {
            Fill::Panel => card_overflow(cx.theme()),
            Fill::Nothing | Fill::Color(_) => 0.0,
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        match self.fill {
            Fill::Nothing => {}
            Fill::Color(color) => scene.fill(rect, color),
            Fill::Panel => {
                let theme = cx.theme();
                let palette = &theme.palette;
                let radius = theme
                    .corners
                    .xl
                    .min(rect.width * 0.5)
                    .min(rect.height * 0.5);
                let shape = RoundedRect::new(rect, radius);
                drop_shadow(scene, shape, theme.elevation.md);
                vertical(
                    scene,
                    shape,
                    (rect.y, rect.bottom()),
                    palette.raised,
                    palette.surface,
                );
                hairline(scene, rect, radius, cx.scale(), palette.hairline);
                top_highlight(scene, rect, radius, cx.scale(), palette.highlight);
            }
        }
    }
}
