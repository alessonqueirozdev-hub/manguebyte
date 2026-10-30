//! Widgets that float over their siblings: placed where they ask inside
//! their parent, drawn and hit in the order they were raised, and given
//! back the focus they took when what is inside them is hidden.
//!
//! A widget floats once it asks with
//! [`UpdateCx::float_at`](crate::UpdateCx::float_at). Layout places it as
//! any child; after layout the root moves it, and everything inside it, by
//! whole device pixels to where it asked, kept inside its parent's
//! rectangle, as scrolling moves a subtree (ADR 0009). Arena order is not
//! changed: the root keeps a drawing order, the arena in runs, in which a
//! node's floating children come after its other children, in the order
//! they were last raised. Painting, the back buffer and hit testing follow
//! it; the focus order stays the arena's.

use manguebyte_gfx::{Point, Rect, Size};

use super::Root;
use crate::layout::Placement;
use crate::tree::NodeId;

/// Where a floating widget is (see
/// [`UpdateCx::float_at`](crate::UpdateCx::float_at)), as of the last
/// layout, for the widgets inside it that move it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FloatMetrics {
    /// Where its top-left corner is, from the area's: where it asked to
    /// be, or as near as it fits inside the area.
    pub position: Point,
    /// The rectangle it is kept inside, in logical pixels: its parent's.
    pub area: Rect,
    /// Its size, in logical pixels.
    pub size: Size,
}

impl FloatMetrics {
    /// `position`, from the area's top-left corner, moved as little as
    /// keeps a widget of this size inside the area: against its left or
    /// top edge where the widget is larger than the area. A coordinate
    /// that is not a number counts as 0.
    ///
    /// ```
    /// use manguebyte_gfx::{Point, Rect, Size};
    /// use manguebyte_ui::FloatMetrics;
    ///
    /// let metrics = FloatMetrics {
    ///     position: Point::ZERO,
    ///     area: Rect::new(0.0, 0.0, 400.0, 300.0),
    ///     size: Size::new(100.0, 80.0),
    /// };
    /// assert_eq!(metrics.clamp(Point::new(350.0, -20.0)), Point::new(300.0, 0.0));
    /// assert_eq!(metrics.clamp(Point::new(f32::NAN, 50.0)), Point::new(0.0, 50.0));
    /// ```
    pub fn clamp(&self, position: Point) -> Point {
        let along = |at: f32, room: f32, length: f32| {
            let most = (room - length).max(0.0);
            if at.is_nan() {
                0.0
            } else {
                at.clamp(0.0, most)
            }
        };
        Point::new(
            along(position.x, self.area.width, self.size.width),
            along(position.y, self.area.height, self.size.height),
        )
    }
}

/// What the root keeps about a floating widget.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Float {
    /// Where it asked to be, from its parent's top-left corner.
    pub(crate) asked: Point,
    /// Where it is, as of the last layout.
    pub(crate) metrics: FloatMetrics,
    /// The widget that had the focus, or was pressed last, when focus was
    /// moved inside it: it gets the focus back when what is inside is
    /// hidden.
    pub(crate) opener: Option<NodeId>,
}

impl Root {
    /// Floats widget `index` at `asked` from its parent's corner, from
    /// now on.
    pub(super) fn set_float(&mut self, index: usize, asked: Point) {
        let Some(node) = self.tree.nodes.get_mut(index) else {
            return;
        };
        match &mut node.float {
            Some(float) => {
                if float.asked != asked {
                    float.asked = asked;
                    self.needs_layout = true;
                }
            }
            None => {
                node.float = Some(Float {
                    asked,
                    ..Float::default()
                });
                self.needs_layout = true;
                self.floats.push(NodeId::from_index(index));
                self.rebuild_order();
            }
        }
    }

    /// Brings floating widget `id` in front of its floating siblings, and
    /// has what it covers drawn again. Nothing for a widget that does not
    /// float or is in front already.
    pub(super) fn raise(&mut self, id: NodeId) {
        let Some(at) = self.floats.iter().position(|&float| float == id) else {
            return;
        };
        let parent = self.tree.get(id).and_then(|node| node.parent);
        let in_front = self.floats[at + 1..]
            .iter()
            .all(|&later| self.tree.get(later).and_then(|node| node.parent) != parent);
        if in_front {
            return;
        }
        let float = self.floats.remove(at);
        self.floats.push(float);
        self.rebuild_order();
        // What it covers is now drawn over by it.
        let end = self.tree.get(id).map_or(0, |node| node.end);
        for node in &self.tree.nodes[id.index()..end.min(self.tree.main)] {
            self.removed.extend(node.painted);
        }
    }

    /// Raises every floating widget `id` is inside, itself included.
    pub(super) fn raise_around(&mut self, id: NodeId) {
        if self.floats.is_empty() {
            return;
        }
        let mut at = Some(id);
        while let Some(node) = at.filter(|id| id.index() < self.tree.main) {
            at = self.tree.get(node).and_then(|node| node.parent);
            if self.tree.get(node).is_some_and(|node| node.float.is_some()) {
                self.raise(node);
            }
        }
    }

    /// Whether `a` and `b` are inside the same floating widget.
    pub(super) fn same_float(&self, a: NodeId, b: NodeId) -> bool {
        let Some(float) = self.float_of(a) else {
            return false;
        };
        let end = self.tree.get(float).map_or(0, |node| node.end);
        (float.index()..end).contains(&b.index())
    }

    /// The nearest floating widget around `id`, itself included.
    fn float_of(&self, id: NodeId) -> Option<NodeId> {
        if self.floats.is_empty() {
            return None;
        }
        let mut at = Some(id);
        while let Some(id) = at {
            let node = self.tree.get(id)?;
            if node.float.is_some() {
                return Some(id);
            }
            at = node.parent;
        }
        None
    }

    /// Where the nearest floating widget around `id`, itself included, is.
    pub(super) fn float_around(&self, id: NodeId) -> Option<FloatMetrics> {
        let float = self.float_of(id)?;
        self.tree.get(float)?.float.map(|float| float.metrics)
    }

    /// Works out the drawing order again: the arena in runs, each node's
    /// floating children after its others, in the order they were raised.
    pub(super) fn rebuild_order(&mut self) {
        let mut order = std::mem::take(&mut self.order);
        order.clear();
        if self.tree.main > 0 {
            self.emit(0, &mut order);
        }
        self.order = order;
    }

    /// Adds the subtree of `index` to `order`, in drawing order.
    fn emit(&self, index: usize, order: &mut Vec<(usize, usize)>) {
        let node = &self.tree.nodes[index];
        let end = node.end.min(self.tree.main);
        let push =
            |order: &mut Vec<(usize, usize)>, start: usize, end: usize| match order.last_mut() {
                Some(last) if last.1 == start => last.1 = end,
                _ => order.push((start, end)),
            };
        let inside = |float: &NodeId| (index + 1..end).contains(&float.index());
        if !self.floats.iter().any(inside) {
            push(order, index, end);
            return;
        }
        push(order, index, index + 1);
        for &kid in &node.kids {
            if self.tree.nodes[kid].float.is_none() {
                self.emit(kid, order);
            }
        }
        for float in &self.floats {
            if self.tree.get(*float).and_then(|node| node.parent) == Some(NodeId::from_index(index))
            {
                self.emit(float.index(), order);
            }
        }
    }

    /// Where floating widget `index`, placed by layout at `placement`,
    /// goes: moved by whole device pixels to where it asked, inside its
    /// parent. Returns the placement and how far it moved, in device
    /// pixels.
    pub(super) fn place_float(
        &mut self,
        index: usize,
        placement: Placement,
    ) -> (Placement, (i32, i32)) {
        let scale = self.scale;
        let Some(parent) = self.tree.nodes[index].parent.map(NodeId::index) else {
            return (placement, (0, 0));
        };
        let (area, area_device) = {
            let parent = &self.tree.nodes[parent];
            (parent.rect, parent.device)
        };
        let node = &mut self.tree.nodes[index];
        let Some(float) = node.float.as_mut() else {
            return (placement, (0, 0));
        };
        let size = placement.rect.size();
        let wanted = FloatMetrics {
            position: Point::ZERO,
            area,
            size,
        }
        .clamp(float.asked);
        let device = |v: f32| (f64::from(v) * f64::from(scale)).round() as i32;
        let along = |start: i32, end: i32, length: i32, at: f32| {
            start
                .saturating_add(device(at))
                .min(end.saturating_sub(length))
                .max(start)
        };
        let x = along(
            area_device.x,
            area_device.right() as i32,
            placement.device.width as i32,
            wanted.x,
        );
        let y = along(
            area_device.y,
            area_device.bottom() as i32,
            placement.device.height as i32,
            wanted.y,
        );
        let by = (
            x.saturating_sub(placement.device.x),
            y.saturating_sub(placement.device.y),
        );
        let placed = super::shifted(placement, by, scale);
        float.metrics = FloatMetrics {
            position: Point::new(placed.rect.x - area.x, placed.rect.y - area.y),
            area,
            size,
        };
        (placed, by)
    }

    /// The first widget inside `index` that takes the focus, which is to
    /// have it; for a floating widget, who has it now, or was pressed
    /// last, is remembered, to get it back.
    pub(super) fn focus_within(&mut self, index: usize) -> Option<NodeId> {
        let end = self.tree.nodes.get(index)?.end;
        let first = (index + 1..end.min(self.tree.main))
            .map(NodeId::from_index)
            .find(|&id| self.focusable(id) && self.reachable(id))?;
        let inside = |id: NodeId| (index..end).contains(&id.index());
        let opener = self.focus.filter(|&id| !inside(id)).or(self
            .last_press
            .filter(|&id| !inside(id) && self.focusable(id)));
        if let Some(float) = self.tree.nodes[index].float.as_mut() {
            if opener.is_some() {
                float.opener = opener;
            }
        }
        Some(first)
    }

    /// After what is inside floating widget `index` was hidden: the focus,
    /// if it was inside or is nowhere, goes back to the widget that opened
    /// it. Returns whether it did.
    pub(super) fn give_back_focus(&mut self, index: usize, lost: bool) -> bool {
        let Some(opener) = self
            .tree
            .nodes
            .get_mut(index)
            .and_then(|node| node.float.as_mut())
            .and_then(|float| float.opener.take())
        else {
            return false;
        };
        if !(lost || self.focus.is_none()) || !self.focusable(opener) || !self.reachable(opener) {
            return false;
        }
        self.set_focus(Some(opener));
        self.reveal(opener);
        true
    }
}
