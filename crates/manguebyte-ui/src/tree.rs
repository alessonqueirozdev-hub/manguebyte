//! The retained tree: every widget of an interface in one arena, in tree
//! order.

use manguebyte_gfx::{IRect, Rect, Scene};

use crate::layout::{Arrange, LayoutNode};
use crate::motion::NodeMotion;
use crate::scroll::ScrollMetrics;
use crate::view::View;
use crate::widget::{Shown, Widget};

/// A widget in a [`Root`](crate::Root).
///
/// Ids are positions in tree order: a parent comes before its children,
/// and a widget before its later siblings, so this is also the order
/// widgets are drawn in (back to front) and the order keyboard focus moves
/// in. An id is only meaningful for the root that gave it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(u32);

impl NodeId {
    pub(crate) fn from_index(index: usize) -> NodeId {
        NodeId(u32::try_from(index).unwrap_or(u32::MAX))
    }

    /// The widget's position in tree order, from 0 for the outermost.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One widget and what the root keeps about it.
pub(crate) struct Node {
    pub(crate) widget: Box<dyn Widget>,
    pub(crate) parent: Option<NodeId>,
    /// The accessible name given with [`IntoView::named`](crate::IntoView::named).
    pub(crate) name: Option<String>,
    /// The help text given with [`IntoView::tooltip`](crate::IntoView::tooltip).
    pub(crate) tooltip: Option<String>,
    /// Where layout put it.
    pub(crate) rect: Rect,
    pub(crate) device: IRect,
    /// Where the widget itself draws: `rect`, less the padding of a widget
    /// with no children.
    pub(crate) content: Rect,
    /// What it drew last, and a second scene to record into, so that a
    /// repaint that draws the same thing is seen and costs no damage.
    pub(crate) scene: Scene,
    pub(crate) spare: Scene,
    /// The device pixels `scene` covers in the back buffer.
    pub(crate) painted: Option<IRect>,
    /// It asked to be painted again.
    pub(crate) paint: bool,
    /// It asked to be measured again.
    pub(crate) measure: bool,
    /// The index after the last widget inside it: its subtree is
    /// `id .. end`, in tree order.
    pub(crate) end: usize,
    /// Where the scroll views it is inside let it be seen, in logical and
    /// device pixels: `None` outside any.
    pub(crate) clip: Option<(Rect, IRect)>,
    /// For a node laid out as a scroll node, where it is scrolled.
    pub(crate) scroll: Option<ScrollMetrics>,
    /// How far its hover, press and focus ring have come in.
    pub(crate) motion: NodeMotion,
    /// How far beyond its rectangle it draws (see `Widget::overflow`).
    pub(crate) overflow: f32,
    /// Its children, all of them, in order: the layout tree lists only
    /// those it shows (see `UpdateCx::show`).
    pub(crate) kids: Vec<usize>,
    /// Which of its children it shows.
    pub(crate) shown: Shown,
    /// It is inside a child that a container does not show: it is not
    /// polled, laid out, drawn, hit or described, and costs nothing.
    pub(crate) hidden: bool,
}

impl Node {
    /// Whether the node shows its child `child`, a node index.
    pub(crate) fn shows(&self, child: usize) -> bool {
        match self.shown {
            Shown::All => true,
            Shown::Only(at) => self.kids.get(at) == Some(&child),
        }
    }
}

/// Every widget, in tree order, with the layout tree beside it.
///
/// The interface's own widgets come first, `main` of them. What is shown
/// over them for a while (open menus, a tooltip) is added after them, each
/// a tree of its own with no parent, and taken away by cutting the arena
/// back.
pub(crate) struct Tree {
    pub(crate) nodes: Vec<Node>,
    pub(crate) layout: Vec<LayoutNode>,
    /// How many of the widgets are the interface's own.
    pub(crate) main: usize,
}

impl Tree {
    /// The tree a view describes.
    pub(crate) fn build(view: View) -> Tree {
        let mut tree = Tree {
            nodes: Vec::new(),
            layout: Vec::new(),
            main: 0,
        };
        tree.add(view, None);
        tree.main = tree.nodes.len();
        tree
    }

    /// Removes the widgets from `len` on, which must be ones added after
    /// the interface's own.
    pub(crate) fn truncate(&mut self, len: usize) {
        let len = len.max(self.main);
        self.nodes.truncate(len);
        self.layout.truncate(len);
    }

    /// Adds the widgets `view` describes after the others, inside
    /// `parent`, and returns the outermost.
    pub(crate) fn add(&mut self, view: View, parent: Option<NodeId>) -> NodeId {
        let id = NodeId::from_index(self.nodes.len());
        let View {
            widget,
            style,
            children,
            name,
            tooltip,
        } = view;
        self.nodes.push(Node {
            widget,
            parent,
            name,
            tooltip,
            rect: Rect::default(),
            device: IRect::default(),
            content: Rect::default(),
            scene: Scene::new(),
            spare: Scene::new(),
            painted: None,
            paint: true,
            measure: true,
            end: 0,
            clip: None,
            scroll: match style.arrange {
                Arrange::Scroll { x, y } => Some(ScrollMetrics {
                    x,
                    y,
                    ..ScrollMetrics::default()
                }),
                _ => None,
            },
            motion: NodeMotion::new(),
            overflow: 0.0,
            kids: Vec::new(),
            shown: Shown::All,
            hidden: false,
        });
        self.layout
            .push(LayoutNode::new(style, Default::default(), Vec::new()));
        for child in children {
            let child = self.add(child, Some(id));
            self.layout[id.index()].children.push(child.index());
            self.nodes[id.index()].kids.push(child.index());
        }
        self.nodes[id.index()].end = self.nodes.len();
        id
    }

    /// The node `id` names, if it is in this tree.
    pub(crate) fn get(&self, id: NodeId) -> Option<&Node> {
        self.nodes.get(id.index())
    }
}
