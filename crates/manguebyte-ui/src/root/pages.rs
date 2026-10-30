//! Children a container hides, and the subtree that is modal.
//!
//! A container shows all of its children unless it asks for fewer
//! ([`UpdateCx::show`](crate::UpdateCx::show)). The others stay in the
//! arena with their state, marked hidden: the frame skips their subtrees
//! when it polls, layout is given only the children shown, and hit
//! testing, focus, the accessibility tree and painting pass them by. What
//! a hidden widget had drawn is taken away and drawn over from what is
//! under it, as when a menu closes; when it is shown again it is laid
//! out and drawn afresh.
//!
//! A modal subtree ([`UpdateCx::set_modal`](crate::UpdateCx::set_modal))
//! is where all input goes while it is set: a dialog over the interface.

use super::Root;
use crate::event::{Event, MenuEvent};
use crate::motion::NodeMotion;
use crate::tree::NodeId;
use crate::widget::Shown;

/// The subtree that has all input, and where the focus was before.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Modal {
    /// The outermost widget of the subtree.
    pub(crate) node: NodeId,
    /// The widget that had the focus when it began, which gets it back.
    pub(crate) focus_before: Option<NodeId>,
}

impl Root {
    /// Shows the children of `id` that `shown` says, from now on.
    pub(super) fn show_children(&mut self, id: usize, shown: Shown) {
        let Some(node) = self.tree.nodes.get_mut(id) else {
            return;
        };
        if node.shown == shown {
            return;
        }
        node.shown = shown;
        node.paint = true;
        // Layout is given only the children shown, so that what is hidden
        // is not measured or placed at all.
        let children = &mut self.tree.layout[id].children;
        children.clear();
        children.extend(
            node.kids
                .iter()
                .enumerate()
                .filter(|&(at, _)| shown.includes(at))
                .map(|(_, &kid)| kid),
        );
        self.needs_layout = true;
        self.refresh_hidden(id);
    }

    /// Works out again which widgets inside `id` are hidden, and acts on
    /// what changed.
    fn refresh_hidden(&mut self, id: usize) {
        let end = self.tree.nodes[id].end.min(self.tree.nodes.len());
        let (mut lost_focus, mut lost_capture, mut lost_hover) = (false, false, false);
        let (mut lost_modal, mut hid) = (false, false);
        for index in id + 1..end {
            let hidden = match self.tree.nodes[index].parent {
                Some(parent) if parent.index() < index => {
                    let parent = &self.tree.nodes[parent.index()];
                    parent.hidden || !parent.shows(index)
                }
                _ => false,
            };
            let node = &mut self.tree.nodes[index];
            if node.hidden == hidden {
                continue;
            }
            node.hidden = hidden;
            let at = Some(NodeId::from_index(index));
            if hidden {
                hid = true;
                // What it drew is drawn over from what is under it.
                self.removed.extend(node.painted.take());
                node.scene.clear();
                node.spare.clear();
                node.motion = NodeMotion::new();
                lost_focus |= self.focus == at;
                lost_capture |= self.capture == at;
                lost_hover |= self.hover == at;
                lost_modal |= self.modal.is_some_and(|modal| Some(modal.node) == at);
                if self.pressed_on == at {
                    self.pressed_on = None;
                }
            } else {
                node.paint = true;
            }
        }
        if lost_capture {
            if let Some(holder) = self.capture {
                self.deliver(holder, |widget, cx| widget.event(cx, &Event::PointerCancel));
            }
            self.capture = None;
        }
        if lost_modal {
            self.end_modal();
        }
        // A floating widget gives the focus back to the widget that had it
        // before it took it.
        let given_back = hid && self.give_back_focus(id, lost_focus);
        if !given_back && lost_focus && self.focus.is_some_and(|id| self.is_hidden(id)) {
            self.set_focus(None);
        }
        if lost_hover {
            if let Some(old) = self.hover.take() {
                self.deliver(old, |widget, cx| widget.hover_changed(cx, false));
            }
            self.hide_tooltip();
            if !self.pressed {
                self.hover_to(self.pointer);
            }
        }
    }

    /// Whether widget `id` is inside a child a container hides.
    pub(super) fn is_hidden(&self, id: NodeId) -> bool {
        self.tree.get(id).is_some_and(|node| node.hidden)
    }

    /// Whether widget `id` may take input while a subtree is modal: it is
    /// inside that subtree, or there is none, or it is over the interface
    /// (an open menu).
    pub(super) fn reachable(&self, id: NodeId) -> bool {
        let Some(modal) = self.modal else {
            return true;
        };
        let index = id.index();
        if index >= self.tree.main {
            return true;
        }
        let start = modal.node.index();
        let end = self.tree.get(modal.node).map_or(start, |node| node.end);
        (start..end).contains(&index)
    }

    /// The subtree that has all input, if one has.
    pub fn modal(&self) -> Option<NodeId> {
        self.modal.map(|modal| modal.node)
    }

    /// Makes the subtree of the child at position `child` of `id` modal,
    /// or ends the modal subtree `id`'s child is.
    pub(super) fn set_modal_child(&mut self, id: usize, child: Option<usize>) {
        let Some(child) = child else {
            // Only the container that began it ends it.
            let ours = self.modal.is_some_and(|modal| {
                self.tree.get(modal.node).and_then(|node| node.parent)
                    == Some(NodeId::from_index(id))
            });
            if ours {
                self.end_modal();
            }
            return;
        };
        let Some(&kid) = self
            .tree
            .nodes
            .get(id)
            .and_then(|node| node.kids.get(child))
        else {
            return;
        };
        let node = NodeId::from_index(kid);
        if self.modal.is_some_and(|modal| modal.node == node) {
            return;
        }
        if self.modal.is_some() {
            self.end_modal();
        }
        if self.menus.is_some() {
            self.close_menus(MenuEvent::Closed);
        }
        self.quiet_tooltip();
        let focus_before = self.focus;
        self.modal = Some(Modal { node, focus_before });
        // What happens outside ends.
        if self.drag.is_some() {
            self.event(Event::PointerCancel);
        }
        if let Some(holder) = self.capture.filter(|&holder| !self.reachable(holder)) {
            self.deliver(holder, |widget, cx| widget.event(cx, &Event::PointerCancel));
            self.capture = None;
        }
        if let Some(old) = self.hover.filter(|&hover| !self.reachable(hover)) {
            self.hover = None;
            self.tree.nodes[old.index()].paint = true;
            self.deliver(old, |widget, cx| widget.hover_changed(cx, false));
        }
        if self.focus.is_some_and(|focus| !self.reachable(focus)) {
            self.set_focus(None);
        }
    }

    /// Ends the modal subtree, and gives the focus back to the widget that
    /// had it before, if it can still take it.
    pub(super) fn end_modal(&mut self) {
        let Some(modal) = self.modal.take() else {
            return;
        };
        let back = modal
            .focus_before
            .filter(|&id| id.index() < self.tree.main && self.focusable(id));
        if back.is_some() || self.focus.is_none_or(|id| self.is_hidden(id)) {
            self.set_focus(back);
            if let Some(back) = back {
                self.reveal(back);
            }
        }
        // The pointer hovers what it is over, now that it can.
        if !self.pressed {
            self.hover_to(self.pointer);
        }
    }

    /// The widgets the focus moves through: those that take it, inside
    /// the modal subtree if there is one.
    pub(super) fn focus_order(&self) -> Vec<NodeId> {
        let (start, end) = match self.modal {
            Some(modal) => (
                modal.node.index(),
                self.tree.get(modal.node).map_or(0, |node| node.end),
            ),
            None => (0, self.tree.nodes.len()),
        };
        (start..end.min(self.tree.nodes.len()))
            .map(NodeId::from_index)
            .filter(|&id| self.focusable(id))
            .collect()
    }
}
