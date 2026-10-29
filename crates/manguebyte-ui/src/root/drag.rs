//! A drag in progress: who started it, what it carries, which widget
//! accepts it, and the ghost drawn under the pointer (see
//! [`crate::drag`]).
//!
//! The ghost is an ordinary widget appended after the interface's own, as
//! open menus and tooltips are, so it is drawn over everything, costs the
//! damage of its old and new places when it moves, and is found by
//! assistive technology and tests like any other widget. Nothing else is
//! shown over the interface during a drag: a tooltip is taken away when
//! it starts and none shows until it ends, and a widget cannot open a
//! menu meanwhile.
//!
//! The source is optional so that something dragged in from outside the
//! window, which has no source widget, can be offered to the same targets
//! the same way: files and text from other programs come in so, through
//! `files.rs`, with no ghost, since the system draws its own.

use std::time::Duration;

use manguebyte_gfx::{Align, Point, Rect, RoundedRect, Scene, Size, Stroke, TextStyle};

use super::Root;
use crate::access::{AccessNode, Role};
use crate::drag::{DragEvent, DragOutcome, DragPayload};
use crate::event::{Event, Handled, Key, MenuEvent};
use crate::scroll::{auto_scroll, snap};
use crate::tree::NodeId;
use crate::view::View;
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};
use crate::widgets::{Cached, line, line_height, text_width};

/// A drag in progress.
pub(crate) struct DragState {
    /// The widget that started it, which hears how it ended.
    source: Option<NodeId>,
    payload: DragPayload,
    /// The widget that accepted the last offer.
    target: Option<NodeId>,
    /// Where the pointer is.
    pub(super) at: Point,
    /// The ghost's node; none for a drag from outside the window, which
    /// the system draws (see `files.rs`).
    ghost: Option<usize>,
    /// Whether the drag came from outside the window.
    pub(super) external: bool,
    /// The ghost has to be placed again.
    pub(super) moved: bool,
    /// What auto-scrolling moved by less than a device pixel.
    rest: Point,
}

impl Root {
    /// What is being dragged, if a drag is running.
    pub fn dragging(&self) -> Option<&DragPayload> {
        self.drag.as_ref().map(|drag| &drag.payload)
    }

    /// The widget that accepts the drag running now at the pointer, if
    /// any: where a release would drop it.
    pub fn drop_target(&self) -> Option<NodeId> {
        self.drag.as_ref().and_then(|drag| drag.target)
    }

    /// Starts dragging `payload` from `source` at the pointer.
    pub(super) fn begin_drag(&mut self, source: Option<NodeId>, payload: DragPayload) {
        let at = self.pointer.unwrap_or_default();
        self.begin_drag_at(source, payload, at, false);
    }

    /// Starts dragging `payload` from `source` at `at`; with a ghost of
    /// its own unless it comes from outside the window.
    pub(super) fn begin_drag_at(
        &mut self,
        source: Option<NodeId>,
        payload: DragPayload,
        at: Point,
        external: bool,
    ) {
        if self.drag.is_some() {
            return;
        }
        // Nothing else is over the interface while something is dragged.
        self.hide_tooltip();
        if self.menus.is_some() {
            self.close_menus(MenuEvent::Closed);
        }
        self.capture = None;
        let ghost = (!external).then(|| {
            self.tree
                .add(View::new(Ghost::new(payload.description())), None)
                .index()
        });
        self.drag = Some(DragState {
            source,
            payload,
            target: None,
            at,
            ghost,
            external,
            moved: true,
            rest: Point::ZERO,
        });
        self.offer_drag(at);
    }

    /// Takes the events a running drag takes, and returns whether they
    /// were used; `None` for the events that go on as usual (the wheel,
    /// keys other than Escape, typed text).
    pub(super) fn drag_event(&mut self, event: &Event) -> Option<Handled> {
        match *event {
            Event::PointerMove { at, .. } => {
                if at.is_finite() {
                    if let Some(drag) = &mut self.drag {
                        drag.at = at;
                        drag.moved = true;
                    }
                    self.offer_drag(at);
                }
                Some(Handled::Yes)
            }
            Event::PointerUp { at, .. } => {
                let at = Some(at).filter(|at| at.is_finite());
                self.finish_drag(at);
                self.hover_to(at);
                Some(Handled::Yes)
            }
            // A second button cancels, and does not press anything.
            Event::PointerDown { .. } | Event::PointerCancel => {
                self.finish_drag(None);
                Some(Handled::Yes)
            }
            Event::Key(key) if key.pressed && key.key == Key::Escape => {
                self.finish_drag(None);
                self.remember_key(key.key);
                Some(Handled::Yes)
            }
            // The window losing the keyboard cancels, then goes on as
            // usual.
            Event::Focus(false) => {
                self.finish_drag(None);
                None
            }
            _ => None,
        }
    }

    /// Offers the payload at `at` to the widget there and those containing
    /// it, until one accepts; the one that accepted before and does not
    /// now hears that the drag left it.
    pub(super) fn offer_drag(&mut self, at: Point) {
        let Some(payload) = self.drag.as_ref().map(|drag| drag.payload.clone()) else {
            return;
        };
        let mut accepted = None;
        if let Some(hit) = self.hit_main(at) {
            let over = DragEvent::Over {
                at,
                payload: &payload,
            };
            for target in self.ancestry(hit) {
                if self.deliver(target, |widget, cx| widget.drag(cx, &over)) == Handled::Yes {
                    accepted = Some(target);
                    break;
                }
            }
        }
        let before = self
            .drag
            .as_mut()
            .and_then(|drag| std::mem::replace(&mut drag.target, accepted));
        if let Some(before) = before.filter(|&before| Some(before) != accepted) {
            self.deliver(before, |widget, cx| widget.drag(cx, &DragEvent::Left));
        }
    }

    /// Ends the drag: drops it at `at` on the widget that accepts it there,
    /// or cancels it if `at` is `None` or nothing takes it. The target
    /// hears the drop, or that the drag left it, then the source how it
    /// ended. Returns how it ended, or `None` if no drag was running.
    pub(super) fn finish_drag(&mut self, at: Option<Point>) -> Option<DragOutcome> {
        if let Some(at) = at {
            // Where the release is, which can differ from the last move.
            self.offer_drag(at);
        }
        let drag = self.drag.take()?;
        let mut outcome = DragOutcome::Cancelled;
        if let Some(target) = drag.target {
            if let Some(at) = at {
                let drop = DragEvent::Drop {
                    at,
                    payload: &drag.payload,
                };
                if self.deliver(target, |widget, cx| widget.drag(cx, &drop)) == Handled::Yes {
                    outcome = DragOutcome::Dropped;
                }
            }
            if outcome == DragOutcome::Cancelled {
                self.deliver(target, |widget, cx| widget.drag(cx, &DragEvent::Left));
            }
        }
        if let Some(ghost) = drag.ghost {
            self.cut_tree(ghost);
        }
        if let Some(source) = drag.source.filter(|id| id.index() < self.tree.main) {
            let ended = DragEvent::Ended(outcome);
            self.deliver(source, |widget, cx| widget.drag(cx, &ended));
        }
        Some(outcome)
    }

    /// One frame of a running drag: scrolls the innermost scroll view
    /// whose edge the pointer is near and that can move that way, and
    /// offers the payload again, as what is under the pointer may have
    /// moved. Returns whether a drag is running, which wants frames.
    pub(super) fn tick_drag(&mut self, elapsed: Duration) -> bool {
        let Some(at) = self.drag.as_ref().map(|drag| drag.at) else {
            return false;
        };
        if let Some(hit) = self.hit_main(at) {
            let scale = self.scale;
            for id in self.ancestry(hit) {
                let Some(metrics) = self.tree.nodes[id.index()].scroll else {
                    continue;
                };
                let by = auto_scroll(metrics.viewport, at, elapsed);
                if by == Point::ZERO {
                    continue;
                }
                let rest = self.drag.as_ref().map_or(Point::ZERO, |drag| drag.rest);
                let exact = metrics.clamp(Point::new(
                    metrics.offset.x + by.x + rest.x,
                    metrics.offset.y + by.y + rest.y,
                ));
                if exact == metrics.offset {
                    // At its end that way: perhaps a view around it can go.
                    continue;
                }
                // Steps of less than a device pixel add up.
                let snapped = metrics.clamp(Point::new(
                    snap(f64::from(exact.x), scale) as f32,
                    snap(f64::from(exact.y), scale) as f32,
                ));
                if let Some(drag) = &mut self.drag {
                    drag.rest = Point::new(exact.x - snapped.x, exact.y - snapped.y);
                }
                self.set_scroll(id, snapped);
                break;
            }
        }
        self.offer_drag(at);
        true
    }

    /// Lays the ghost out beside the pointer.
    pub(super) fn place_ghost(&mut self) {
        let Some((Some(ghost), at)) = self.drag.as_mut().map(|drag| {
            drag.moved = false;
            (drag.ghost, drag.at)
        }) else {
            return;
        };
        // Measured once, when it is new or the theme or scale changed, so
        // that moving it never lays out the interface.
        if let (Some(node), Some(item)) = (
            self.tree.nodes.get_mut(ghost),
            self.tree.layout.get_mut(ghost),
        ) {
            if node.measure {
                node.measure = false;
                let cx = MeasureCx {
                    theme: &self.theme,
                    scale: self.scale,
                };
                item.content = node.widget.measure(&cx);
            }
        }
        let natural = crate::layout::natural_size(&self.tree.layout, ghost);
        let rect = ghost_place(natural, at, self.size);
        let placed = crate::layout::solve(&self.tree.layout, ghost, rect, self.scale);
        if let Some(placement) = placed.get(ghost) {
            self.apply_placement(ghost, *placement);
        }
    }
}

/// How far right of and below the pointer the ghost's corner is, clear of
/// the pointer's own picture.
const GHOST_OFFSET: f32 = 14.0;

/// Space between the ghost's edge and its text.
const GHOST_PAD: f32 = 5.0;

/// The most the ghost's text is wide before it is cut with an ellipsis.
const GHOST_WIDTH: f32 = 240.0;

/// Where a ghost of `size` goes for the pointer at `at`: below and right of
/// it, moved to stay inside `area` where it can.
fn ghost_place(size: Size, at: Point, area: Size) -> Rect {
    let x = (at.x + GHOST_OFFSET).min(area.width - size.width).max(0.0);
    let below = at.y + GHOST_OFFSET;
    let y = if below + size.height <= area.height {
        below
    } else {
        (at.y - GHOST_OFFSET - size.height).max(0.0)
    };
    Rect::new(x, y, size.width, size.height)
}

/// What is being dragged, drawn under the pointer: its description in a
/// small panel edged in the accent colour.
struct Ghost {
    text: String,
    layout: Cached<TextStyle>,
}

impl Ghost {
    fn new(text: &str) -> Ghost {
        Ghost {
            text: text.to_owned(),
            layout: Cached::new(),
        }
    }
}

fn ghost_style(theme: &crate::Theme) -> TextStyle {
    theme
        .typography
        .label
        .with_color(theme.palette.text)
        .with_align(Align::Start)
}

impl Widget for Ghost {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::DragItem, self.text.as_str())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = ghost_style(cx.theme());
        Size::new(
            text_width(&self.text, &style).min(GHOST_WIDTH) + 2.0 * GHOST_PAD,
            line_height(&style) + 2.0 * GHOST_PAD,
        )
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let radius = theme.corners.medium;
        scene.fill(RoundedRect::new(rect, radius), theme.palette.surface);
        let edge = Rect::new(
            rect.x + 0.75,
            rect.y + 0.75,
            (rect.width - 1.5).max(0.0),
            (rect.height - 1.5).max(0.0),
        );
        scene.stroke(
            RoundedRect::new(edge, radius),
            Stroke::new(1.5),
            theme.palette.accent,
        );
        let style = ghost_style(theme);
        let width = (rect.width - 2.0 * GHOST_PAD).max(0.0);
        let text = &self.text;
        let layout = self.layout.get(width, style, || line(text, &style, width));
        scene.text(layout, Point::new(rect.x + GHOST_PAD, rect.y + GHOST_PAD));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ghost_stays_beside_the_pointer_and_inside_the_interface() {
        let area = Size::new(200.0, 100.0);
        let size = Size::new(60.0, 20.0);
        assert_eq!(
            ghost_place(size, Point::new(10.0, 10.0), area),
            Rect::new(24.0, 24.0, 60.0, 20.0)
        );
        // Near the right and bottom edges it moves left and goes above.
        assert_eq!(
            ghost_place(size, Point::new(190.0, 90.0), area),
            Rect::new(140.0, 56.0, 60.0, 20.0)
        );
    }
}
