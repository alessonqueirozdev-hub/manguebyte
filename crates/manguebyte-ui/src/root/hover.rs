//! What the pointer rests on: the hovered widget, and tooltips.
//!
//! The widget under the pointer, while no button is held, is the hovered
//! one; it hears when it becomes so and when it stops
//! ([`Widget::hover_changed`](crate::Widget::hover_changed)). A view given
//! a [`tooltip`](crate::IntoView::tooltip) shows it, in a small panel
//! below the pointer, once the pointer has rested on it, or on a widget
//! inside it, for [`TOOLTIP_DELAY`] on the frame clock. The tooltip goes
//! when the pointer leaves the view or the interface, and with a press, a
//! key, a wheel turn, a menu, or the window losing the keyboard; after a
//! press or a key it comes back only once the pointer has left the view
//! and come back to it.

use std::time::Duration;

use manguebyte_gfx::{Align, Point, Rect, Scene, Size, TextStyle};

use super::Root;
use crate::access::{AccessNode, Role};
use crate::tree::NodeId;
use crate::view::View;
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};
use crate::widgets::{Cached, floating, floating_overflow, line, line_height, text_width};

/// How long the pointer rests on a view before its tooltip shows.
pub const TOOLTIP_DELAY: Duration = Duration::from_millis(500);

/// A tooltip waiting to show, or showing.
pub(crate) struct Tip {
    /// The widget whose tooltip it is.
    owner: NodeId,
    /// When the pointer came to rest on it, on the frame clock.
    since: Duration,
    /// Where the pointer rested.
    at: Point,
    /// The tooltip's node, once it shows.
    pub(crate) node: Option<usize>,
}

impl Root {
    /// The widget with a tooltip that `id` is, or is inside: the innermost.
    fn tooltip_owner(&self, id: Option<NodeId>) -> Option<NodeId> {
        let mut at = id?;
        loop {
            let node = self.tree.get(at)?;
            if node.tooltip.is_some() {
                return Some(at);
            }
            at = node.parent?;
        }
    }

    /// The pointer moved to `at` with no button held, or left the
    /// interface (`None`): the hovered widget and the tooltip follow it.
    pub(crate) fn hover_to(&mut self, at: Option<Point>) {
        let over = at.and_then(|at| self.hit_main(at));
        if over != self.hover {
            let old = std::mem::replace(&mut self.hover, over);
            for (changed, hovered) in [(old, false), (over, true)] {
                if let Some(changed) = changed.filter(|id| id.index() < self.tree.main) {
                    self.tree.nodes[changed.index()].paint = true;
                    self.deliver(changed, |widget, cx| widget.hover_changed(cx, hovered));
                }
            }
        }
        let owner = self.tooltip_owner(over);
        if owner != self.tip.as_ref().map(|tip| tip.owner) {
            self.hide_tooltip();
            if owner != self.tip_quiet {
                self.tip_quiet = None;
            }
            if let (Some(owner), Some(at)) = (owner, at) {
                if self.tip_quiet != Some(owner) {
                    self.tip = Some(Tip {
                        owner,
                        since: self.now,
                        at,
                        node: None,
                    });
                }
            }
        } else if let (Some(tip), Some(at)) = (&mut self.tip, at) {
            // Resting starts again while the pointer moves, until it shows.
            if tip.node.is_none() {
                tip.since = self.now;
                tip.at = at;
            }
        }
        if owner.is_none() {
            self.tip_quiet = None;
        }
    }

    /// Takes the tooltip away, and keeps it away until the pointer leaves
    /// its view: after a press, a key or a wheel turn.
    pub(crate) fn quiet_tooltip(&mut self) {
        if let Some(tip) = &self.tip {
            self.tip_quiet = Some(tip.owner);
        }
        self.hide_tooltip();
    }

    /// Removes the tooltip, shown or waiting.
    pub(crate) fn hide_tooltip(&mut self) {
        if let Some(Tip {
            node: Some(node), ..
        }) = self.tip.take()
        {
            self.cut_tree(node);
        }
    }

    /// Shows the tooltip waiting, if the pointer has rested long enough.
    /// Returns whether one is still waiting.
    pub(crate) fn tick_tooltip(&mut self) -> bool {
        let Some(tip) = &self.tip else {
            return false;
        };
        if tip.node.is_some() {
            return false;
        }
        if self.menus.is_some() || self.capture.is_some() || self.drag.is_some() {
            return true;
        }
        if self.now.saturating_sub(tip.since) < TOOLTIP_DELAY {
            return true;
        }
        let text = self
            .tree
            .get(tip.owner)
            .and_then(|node| node.tooltip.clone())
            .unwrap_or_default();
        let node = self
            .tree
            .add(View::new(TooltipBox::new(&text)), None)
            .index();
        if let Some(tip) = &mut self.tip {
            tip.node = Some(node);
        }
        self.needs_layout = true;
        false
    }

    /// Lays the tooltip out below the pointer.
    pub(crate) fn place_tooltip(&mut self) {
        let Some((node, at)) = self.tip.as_ref().and_then(|tip| Some((tip.node?, tip.at))) else {
            return;
        };
        let natural = crate::layout::natural_size(&self.tree.layout, node);
        let rect = tooltip_place(natural, at, self.size);
        let placed = crate::layout::solve(&self.tree.layout, node, rect, self.scale);
        if let Some(placement) = placed.get(node) {
            self.apply_placement(node, *placement);
        }
    }
}
/// Space between a tooltip's edge and its text.
const TIP_PAD: f32 = 5.0;

/// How far below the pointer a tooltip goes, past the pointer's own
/// picture.
const TIP_BELOW: f32 = 20.0;

/// The most a tooltip's text is wide before it is cut with an ellipsis.
const TIP_WIDTH: f32 = 320.0;

/// A tooltip: a widget's help text, in a small panel near the pointer.
pub(crate) struct TooltipBox {
    text: String,
    layout: Cached<TextStyle>,
}

impl TooltipBox {
    pub(crate) fn new(text: &str) -> TooltipBox {
        TooltipBox {
            text: text.to_owned(),
            layout: Cached::new(),
        }
    }
}

fn tip_style(theme: &crate::Theme) -> TextStyle {
    theme
        .typography
        .value
        .with_color(theme.palette.text)
        .with_align(Align::Start)
        .with_tabular_figures(false)
}

impl Widget for TooltipBox {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Tooltip, self.text.as_str())
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        floating_overflow(cx.theme())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = tip_style(cx.theme());
        Size::new(
            text_width(&self.text, &style).min(TIP_WIDTH) + 2.0 * TIP_PAD,
            line_height(&style) + 2.0 * TIP_PAD,
        )
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        floating(scene, rect, theme.corners.md, theme, cx.scale());
        let style = tip_style(theme);
        let width = (rect.width - 2.0 * TIP_PAD).max(0.0);
        let text = &self.text;
        let layout = self.layout.get(width, style, || line(text, &style, width));
        scene.text(layout, Point::new(rect.x + TIP_PAD, rect.y + TIP_PAD));
    }
}

/// Where a tooltip of `size` goes for the pointer at `at`: below it, or
/// above it where there is no room below; moved left to stay inside.
pub(crate) fn tooltip_place(size: Size, at: Point, area: Size) -> Rect {
    let below = at.y + TIP_BELOW;
    let y = if below + size.height <= area.height {
        below
    } else {
        (at.y - 4.0 - size.height).max(0.0)
    };
    let x = at.x.min(area.width - size.width).max(0.0);
    Rect::new(x, y, size.width, size.height)
}
