//! Layout: rows, columns and stacks, with padding, spacing, flexible
//! weights, and minimum and maximum sizes.
//!
//! Layout is a pure function from a tree of [`LayoutNode`]s and an area to
//! one [`Placement`] per node: nothing here knows about widgets, windows or
//! drawing, so every rule can be tested with exact rectangles.
//!
//! It works in two passes. Sizes come up: each node's natural size is its
//! content (for a leaf) or what its children need (for a container), plus
//! its padding, kept within its minimum and maximum. Rectangles go down:
//! each container divides its rectangle, less its padding, among its
//! children. A leaf's padding is space between its edges and its content:
//! [`Placement::inset`] gives the rectangle the content goes in.
//!
//! - A **row** places children left to right and a **column** top to
//!   bottom, with [`spacing`](Style::spacing) between them. Along that
//!   axis, children with no [`flex`](Style::flex) weight get their natural
//!   size; children with a weight start from their minimum and share what
//!   room is left in proportion to their weights, none growing past its
//!   maximum. If even that does not fit, children without a weight shrink
//!   towards their minimum, all by the same fraction of what they can give
//!   up; what still does not fit runs past the end. Room left over is
//!   placed by [`justify`](Style::justify).
//! - Across that axis, and on both axes of a **stack** (children on top of
//!   each other), a child is placed by its alignment: stretched to the
//!   room there is, or at its natural size against the start, the centre
//!   or the end. Its minimum and maximum always hold.
//! - A **scroll** node shows its children through a viewport: its
//!   rectangle less its padding and the room for its scroll bars (see
//!   [`Arrange::Scroll`] and [`scroll_viewport`]). Along an axis it scrolls
//!   on, each child is as long as its natural size or the viewport,
//!   whichever is longer; along an axis it does not, as long as the
//!   viewport. Children start at the viewport's top-left corner: moving
//!   them by the scroll offset and clipping them to the viewport is the
//!   [`Root`](crate::Root)'s work, not layout's.
//!
//! Finally every edge is snapped to the nearest device pixel at the scale
//! given, so that edges stay sharp at any scale and two neighbours that
//! touch still touch: [`Placement::device`] is the rectangle in whole
//! device pixels and [`Placement::rect`] the same rectangle in logical
//! pixels.
//!
//! ```
//! use manguebyte_gfx::{IRect, Rect, Size};
//! use manguebyte_ui::layout::{solve, Insets, LayoutNode, Style};
//!
//! // A row 300 wide with 10 of padding and 10 between two children: one
//! // 40 wide, and one that takes the rest.
//! let nodes = vec![
//!     LayoutNode::new(Style::row().with_padding(Insets::all(10.0)).with_spacing(10.0), Size::default(), vec![1, 2]),
//!     LayoutNode::leaf(Style::leaf(), Size::new(40.0, 20.0)),
//!     LayoutNode::leaf(Style::leaf().with_flex(1.0), Size::new(0.0, 20.0)),
//! ];
//! let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 300.0, 60.0), 1.5);
//! assert_eq!(placed[1].rect, Rect::new(10.0, 10.0, 40.0, 40.0));
//! assert_eq!(placed[2].rect, Rect::new(60.0, 10.0, 230.0, 40.0));
//! // At 1.5x the second child covers device pixels 90 to 435.
//! assert_eq!(placed[2].device, IRect::new(90, 15, 345, 60));
//! ```

use manguebyte_gfx::{IRect, Rect, Size};

/// Space kept free inside a node's edges, in logical pixels: between a
/// container's edges and its children, or between a leaf's edges and its
/// content.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Insets {
    /// Space at the left edge.
    pub left: f32,
    /// Space at the top edge.
    pub top: f32,
    /// Space at the right edge.
    pub right: f32,
    /// Space at the bottom edge.
    pub bottom: f32,
}

impl Insets {
    /// No space at all.
    pub const ZERO: Insets = Insets::all(0.0);

    /// `value` on every side.
    pub const fn all(value: f32) -> Self {
        Insets {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    /// `horizontal` at the left and right, `vertical` at the top and
    /// bottom.
    pub const fn symmetric(horizontal: f32, vertical: f32) -> Self {
        Insets {
            left: horizontal,
            top: vertical,
            right: horizontal,
            bottom: vertical,
        }
    }

    /// Each side given separately.
    pub const fn new(left: f32, top: f32, right: f32, bottom: f32) -> Self {
        Insets {
            left,
            top,
            right,
            bottom,
        }
    }

    /// The same insets with every side that is negative or not finite
    /// replaced by 0.
    fn cleaned(self) -> Insets {
        Insets::new(
            length(self.left),
            length(self.top),
            length(self.right),
            length(self.bottom),
        )
    }
}

/// How a node places its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Arrange {
    /// It has none: its natural size is its content's.
    #[default]
    Leaf,
    /// Left to right.
    Row,
    /// Top to bottom.
    Column,
    /// On top of each other, each placed in the whole area.
    Stack,
    /// Through a viewport that scrolls on the axes set to `true`, as a
    /// [`scroll_view`](crate::scroll_view) does. The node's content size
    /// ([`LayoutNode::content`]) is the room kept for its scroll bars: its
    /// width at the right edge when it scrolls up and down, its height at
    /// the bottom edge when it scrolls sideways. The room is kept whether
    /// or not the children overflow, so that they do not move when they
    /// grow past the viewport.
    Scroll {
        /// It scrolls sideways.
        x: bool,
        /// It scrolls up and down.
        y: bool,
    },
}

/// Where a child sits across a row or column, or on either axis of a
/// stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Align {
    /// At its natural size, against the start (left or top).
    Start,
    /// At its natural size, centred.
    Center,
    /// At its natural size, against the end (right or bottom).
    End,
    /// As large as the room there is, within its minimum and maximum, and
    /// against the start if its maximum stops it.
    #[default]
    Stretch,
}

/// Where room left over along a row or column goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Justify {
    /// After the last child.
    #[default]
    Start,
    /// Half before the first child and half after the last.
    Center,
    /// Before the first child.
    End,
    /// Shared equally between the gaps; a single child stays at the
    /// start.
    SpaceBetween,
}

/// A size with no limit, the default maximum.
pub const UNBOUNDED: Size = Size::new(f32::INFINITY, f32::INFINITY);

/// How one node is laid out.
///
/// Lengths are logical pixels. Values that make no sense are read as the
/// nearest that does: a negative or non-finite padding, spacing, weight or
/// minimum counts as 0, a maximum that is not a number as no limit, and a
/// maximum below the minimum as the minimum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Style {
    /// How the node places its children.
    pub arrange: Arrange,
    /// Space kept free inside the node's edges: around its children, or,
    /// for a leaf, around its content.
    pub padding: Insets,
    /// Space between neighbouring children of a row or column.
    pub spacing: f32,
    /// The node's share of the room left in the row or column it is in; 0
    /// takes its natural size.
    pub flex: f32,
    /// The smallest the node may be.
    pub min: Size,
    /// The largest the node may be.
    pub max: Size,
    /// Where the node's children sit across its row or column, or on both
    /// axes of its stack, unless a child says otherwise.
    pub align: Align,
    /// Where the node itself sits, overriding its parent's
    /// [`align`](Style::align).
    pub align_self: Option<Align>,
    /// Where room left over along the node's row or column goes.
    pub justify: Justify,
}

impl Style {
    /// A leaf: no children, natural size from its content, no weight, no
    /// limits.
    pub const fn leaf() -> Style {
        Style {
            arrange: Arrange::Leaf,
            padding: Insets::ZERO,
            spacing: 0.0,
            flex: 0.0,
            min: Size::new(0.0, 0.0),
            max: UNBOUNDED,
            align: Align::Stretch,
            align_self: None,
            justify: Justify::Start,
        }
    }

    /// A row with no padding or spacing.
    pub const fn row() -> Style {
        Style {
            arrange: Arrange::Row,
            ..Style::leaf()
        }
    }

    /// A column with no padding or spacing.
    pub const fn column() -> Style {
        Style {
            arrange: Arrange::Column,
            ..Style::leaf()
        }
    }

    /// A stack with no padding.
    pub const fn stack() -> Style {
        Style {
            arrange: Arrange::Stack,
            ..Style::leaf()
        }
    }

    /// The same style with `padding`.
    pub const fn with_padding(self, padding: Insets) -> Style {
        Style { padding, ..self }
    }

    /// The same style with `spacing` between children.
    pub const fn with_spacing(self, spacing: f32) -> Style {
        Style { spacing, ..self }
    }

    /// The same style with the weight `flex`.
    pub const fn with_flex(self, flex: f32) -> Style {
        Style { flex, ..self }
    }

    /// The same style with the minimum size `min`.
    pub const fn with_min(self, min: Size) -> Style {
        Style { min, ..self }
    }

    /// The same style with the maximum size `max`.
    pub const fn with_max(self, max: Size) -> Style {
        Style { max, ..self }
    }

    /// The same style with children aligned by `align`.
    pub const fn with_align(self, align: Align) -> Style {
        Style { align, ..self }
    }

    /// The same style, the node itself aligned by `align`.
    pub const fn with_align_self(self, align: Align) -> Style {
        Style {
            align_self: Some(align),
            ..self
        }
    }

    /// The same style with room left over placed by `justify`.
    pub const fn with_justify(self, justify: Justify) -> Style {
        Style { justify, ..self }
    }
}

impl Default for Style {
    fn default() -> Self {
        Style::leaf()
    }
}

/// One node of the tree that [`solve`] lays out.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutNode {
    /// How it is laid out.
    pub style: Style,
    /// The natural size of a leaf's content, such as the width and height
    /// of a label's text. Containers ignore it.
    pub content: Size,
    /// Its children, as indices into the same slice, in order.
    pub children: Vec<usize>,
}

impl LayoutNode {
    /// A node with `style`, `content` and `children`.
    pub fn new(style: Style, content: Size, children: Vec<usize>) -> Self {
        LayoutNode {
            style,
            content,
            children,
        }
    }

    /// A node with no children.
    pub fn leaf(style: Style, content: Size) -> Self {
        LayoutNode::new(style, content, Vec::new())
    }
}

/// Where [`solve`] put one node.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Placement {
    /// The node's rectangle in logical pixels, its edges on device pixels.
    pub rect: Rect,
    /// The same rectangle in whole device pixels.
    pub device: IRect,
}

impl Placement {
    /// The part of this placement that `padding` leaves free, its edges
    /// snapped to the nearest device pixel at `scale` as [`solve`] snaps
    /// them: where a leaf's content goes. It never reaches outside the
    /// placement; padding that does not fit leaves an empty rectangle.
    /// Padding is cleaned as [`Style`] says, and a scale that is not a
    /// positive finite number counts as 1.
    ///
    /// ```
    /// use manguebyte_gfx::{IRect, Rect};
    /// use manguebyte_ui::layout::{Insets, Placement};
    ///
    /// let placed = Placement { rect: Rect::new(10.0, 10.0, 40.0, 20.0), device: IRect::new(20, 20, 80, 40) };
    /// let inner = placed.inset(Insets::symmetric(5.0, 2.25), 2.0);
    /// assert_eq!(inner.device, IRect::new(30, 25, 60, 31));
    /// assert_eq!(inner.rect, Rect::new(15.0, 12.5, 30.0, 15.5));
    /// ```
    pub fn inset(&self, padding: Insets, scale: f32) -> Placement {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        let padding = padding.cleaned();
        let rect = self.rect;
        let left = padding.left.min(rect.width);
        let top = padding.top.min(rect.height);
        let inner = Rect::new(
            rect.x + left,
            rect.y + top,
            length(rect.width - left - padding.right),
            length(rect.height - top - padding.bottom),
        );
        snap(inner, scale)
    }
}

/// Lays out the tree under `root` in `area`, at `scale` device pixels per
/// logical pixel, and returns one placement per node of `nodes`, at the
/// same index.
///
/// The root is given `area` whatever its own limits say, as a window is
/// whatever size it is. Nodes that cannot be reached from `root` get an
/// empty placement at the origin; so do indices that are out of range and
/// a node reached a second time (in a tree with a cycle or a shared
/// child), which are skipped. A scale that is not a positive finite number
/// counts as 1. Nothing here panics, whatever the input.
pub fn solve(nodes: &[LayoutNode], root: usize, area: Rect, scale: f32) -> Vec<Placement> {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let mut placements = vec![Placement::default(); nodes.len()];
    if root >= nodes.len() {
        return placements;
    }
    let order = preorder(nodes, root);
    let mut natural = vec![Size::default(); nodes.len()];
    let mut rects = vec![Rect::default(); nodes.len()];
    // Children come after their parents, so going backwards measures each
    // child before its parent.
    for &index in order.nodes.iter().rev() {
        natural[index] = measure(nodes, &order, index, &natural);
    }
    rects[root] = clean_rect(area);
    let mut tracks = Vec::new();
    let mut sizes = Vec::new();
    for &index in &order.nodes {
        arrange(
            nodes,
            &order,
            index,
            &natural,
            &mut rects,
            &mut tracks,
            &mut sizes,
        );
    }
    for &index in &order.nodes {
        placements[index] = snap(rects[index], scale);
    }
    placements
}

/// The size the tree under `root` asks for: its natural size, as [`solve`]
/// works it out before it divides an area, within the root's own minimum
/// and maximum. Zero for a root that is out of range.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::layout::{Insets, LayoutNode, Style, natural_size};
///
/// // A column with 4 of padding and 2 between two children.
/// let nodes = vec![
///     LayoutNode::new(Style::column().with_padding(Insets::all(4.0)).with_spacing(2.0), Size::default(), vec![1, 2]),
///     LayoutNode::leaf(Style::leaf(), Size::new(40.0, 20.0)),
///     LayoutNode::leaf(Style::leaf(), Size::new(60.0, 10.0)),
/// ];
/// assert_eq!(natural_size(&nodes, 0), Size::new(68.0, 40.0));
/// assert_eq!(natural_size(&nodes, 9), Size::default());
/// ```
pub fn natural_size(nodes: &[LayoutNode], root: usize) -> Size {
    if root >= nodes.len() {
        return Size::default();
    }
    let order = preorder(nodes, root);
    let mut natural = vec![Size::default(); nodes.len()];
    for &index in order.nodes.iter().rev() {
        natural[index] = measure(nodes, &order, index, &natural);
    }
    natural[root]
}

/// The tree under the root as layout sees it: each node reached once,
/// parents before children, with the children each node places.
struct Order {
    /// The nodes, parents before children.
    nodes: Vec<usize>,
    /// The children of every node, one after another.
    kids: Vec<usize>,
    /// Where each node's children are in `kids`: start and count.
    ranges: Vec<(usize, usize)>,
}

impl Order {
    /// The children `index` places, in order.
    fn kids(&self, index: usize) -> &[usize] {
        let (start, count) = self.ranges[index];
        &self.kids[start..start + count]
    }
}

/// The nodes reachable from `root`. A child index that is out of range, or
/// names a node already reached (the root, an ancestor, or a node listed
/// twice), is dropped, so every node has one parent and the walk ends.
fn preorder(nodes: &[LayoutNode], root: usize) -> Order {
    let mut seen = vec![false; nodes.len()];
    let mut order = Order {
        nodes: Vec::with_capacity(nodes.len()),
        kids: Vec::with_capacity(nodes.len()),
        ranges: vec![(0, 0); nodes.len()],
    };
    let mut stack = vec![root];
    seen[root] = true;
    while let Some(index) = stack.pop() {
        order.nodes.push(index);
        let start = order.kids.len();
        for &child in &nodes[index].children {
            if child < nodes.len() && !seen[child] {
                seen[child] = true;
                order.kids.push(child);
            }
        }
        order.ranges[index] = (start, order.kids.len() - start);
        // Pushed in reverse so that the first child comes out first.
        stack.extend(order.kids[start..].iter().rev());
    }
    order
}

/// A length that is a finite number of at least 0, or 0.
fn length(value: f32) -> f32 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        0.0
    }
}

/// A maximum: no limit if it is not a number, at least 0 otherwise.
fn limit(value: f32) -> f32 {
    if value.is_nan() {
        f32::INFINITY
    } else {
        value.max(0.0)
    }
}

/// `value` kept within `min ..= max`, the minimum winning if they cross.
fn clamp(value: f32, min: f32, max: f32) -> f32 {
    value.min(max).max(min)
}

fn clean_rect(rect: Rect) -> Rect {
    let finite = |v: f32| if v.is_finite() { v } else { 0.0 };
    Rect::new(
        finite(rect.x),
        finite(rect.y),
        length(rect.width),
        length(rect.height),
    )
}

/// The minimum and maximum of a style, cleaned.
fn limits(style: &Style) -> (Size, Size) {
    let min = Size::new(length(style.min.width), length(style.min.height));
    let max = Size::new(
        limit(style.max.width).max(min.width),
        limit(style.max.height).max(min.height),
    );
    (min, max)
}

/// The natural size of `index`, its children's already known.
fn measure(nodes: &[LayoutNode], order: &Order, index: usize, natural: &[Size]) -> Size {
    let node = &nodes[index];
    let style = &node.style;
    let padding = style.padding.cleaned();
    let spacing = length(style.spacing);
    let (min, max) = limits(style);
    let mut width = 0.0f32;
    let mut height = 0.0f32;
    let mut count = 0usize;
    for &child in order.kids(index) {
        let size = natural[child];
        match style.arrange {
            Arrange::Leaf => {}
            Arrange::Row => {
                width += size.width;
                height = height.max(size.height);
            }
            Arrange::Column => {
                width = width.max(size.width);
                height += size.height;
            }
            Arrange::Stack | Arrange::Scroll { .. } => {
                width = width.max(size.width);
                height = height.max(size.height);
            }
        }
        count += 1;
    }
    let gaps = spacing * count.saturating_sub(1) as f32;
    let (width, height) = match style.arrange {
        Arrange::Leaf => (
            length(node.content.width) + padding.left + padding.right,
            length(node.content.height) + padding.top + padding.bottom,
        ),
        Arrange::Row => (
            width + gaps + padding.left + padding.right,
            height + padding.top + padding.bottom,
        ),
        Arrange::Column => (
            width + padding.left + padding.right,
            height + gaps + padding.top + padding.bottom,
        ),
        Arrange::Stack => (
            width + padding.left + padding.right,
            height + padding.top + padding.bottom,
        ),
        Arrange::Scroll { x, y } => {
            let (bar_x, bar_y) = bars(node, x, y);
            (
                width + bar_y + padding.left + padding.right,
                height + bar_x + padding.top + padding.bottom,
            )
        }
    };
    Size::new(
        clamp(length(width), min.width, max.width),
        clamp(length(height), min.height, max.height),
    )
}

/// One child along the axis of a row or column.
#[derive(Debug, Clone, Copy)]
struct Track {
    natural: f32,
    min: f32,
    max: f32,
    flex: f32,
}

/// Places the children of `index` inside its rectangle.
fn arrange(
    nodes: &[LayoutNode],
    order: &Order,
    index: usize,
    natural: &[Size],
    rects: &mut [Rect],
    tracks: &mut Vec<Track>,
    sizes: &mut Vec<f32>,
) {
    let style = &nodes[index].style;
    let rect = rects[index];
    let padding = style.padding.cleaned();
    let inner = Rect::new(
        rect.x + padding.left,
        rect.y + padding.top,
        length(rect.width - padding.left - padding.right),
        length(rect.height - padding.top - padding.bottom),
    );
    let children = || order.kids(index).iter().copied();
    match style.arrange {
        Arrange::Leaf => {}
        Arrange::Scroll { x, y } => {
            let viewport = viewport_of(&nodes[index], inner, x, y);
            for child in children() {
                let size = natural[child];
                let width = if x {
                    size.width.max(viewport.width)
                } else {
                    viewport.width
                };
                let height = if y {
                    size.height.max(viewport.height)
                } else {
                    viewport.height
                };
                rects[child] = Rect::new(viewport.x, viewport.y, width, height);
            }
        }
        Arrange::Stack => {
            for child in children() {
                let child_style = &nodes[child].style;
                let align = child_style.align_self.unwrap_or(style.align);
                let (min, max) = limits(child_style);
                let (x, width) = across(
                    inner.x,
                    inner.width,
                    natural[child].width,
                    min.width,
                    max.width,
                    align,
                );
                let (y, height) = across(
                    inner.y,
                    inner.height,
                    natural[child].height,
                    min.height,
                    max.height,
                    align,
                );
                rects[child] = Rect::new(x, y, width, height);
            }
        }
        Arrange::Row | Arrange::Column => {
            let row = style.arrange == Arrange::Row;
            tracks.clear();
            for child in children() {
                let (min, max) = limits(&nodes[child].style);
                let size = natural[child];
                tracks.push(if row {
                    Track {
                        natural: size.width,
                        min: min.width,
                        max: max.width,
                        flex: length(nodes[child].style.flex),
                    }
                } else {
                    Track {
                        natural: size.height,
                        min: min.height,
                        max: max.height,
                        flex: length(nodes[child].style.flex),
                    }
                });
            }
            let (main_start, main_room) = if row {
                (inner.x, inner.width)
            } else {
                (inner.y, inner.height)
            };
            let (offset, gap) = distribute(
                tracks,
                main_room,
                length(style.spacing),
                style.justify,
                sizes,
            );
            let mut at = main_start + offset;
            for (slot, child) in children().enumerate() {
                let child_style = &nodes[child].style;
                let align = child_style.align_self.unwrap_or(style.align);
                let (min, max) = limits(child_style);
                let main = sizes[slot];
                let rect = if row {
                    let (y, height) = across(
                        inner.y,
                        inner.height,
                        natural[child].height,
                        min.height,
                        max.height,
                        align,
                    );
                    Rect::new(at, y, main, height)
                } else {
                    let (x, width) = across(
                        inner.x,
                        inner.width,
                        natural[child].width,
                        min.width,
                        max.width,
                        align,
                    );
                    Rect::new(x, at, width, main)
                };
                rects[child] = rect;
                let end = at + main;
                at = end + gap;
            }
        }
    }
}

/// The room a scroll node keeps for its bars: the height of the
/// horizontal one and the width of the vertical one, 0 for an axis it does
/// not scroll on.
fn bars(node: &LayoutNode, x: bool, y: bool) -> (f32, f32) {
    let bar_x = if x { length(node.content.height) } else { 0.0 };
    let bar_y = if y { length(node.content.width) } else { 0.0 };
    (bar_x, bar_y)
}

/// The viewport of a scroll node whose rectangle less its padding is
/// `inner`.
fn viewport_of(node: &LayoutNode, inner: Rect, x: bool, y: bool) -> Rect {
    let (bar_x, bar_y) = bars(node, x, y);
    Rect::new(
        inner.x,
        inner.y,
        length(inner.width - bar_y),
        length(inner.height - bar_x),
    )
}

/// The viewport of `node`, a scroll node placed at `placed`: the part of
/// its rectangle its children show through, less its padding and the room
/// for its bars, snapped to device pixels at `scale` as [`solve`] snaps.
/// `None` if the node does not scroll.
///
/// ```
/// use manguebyte_gfx::{Rect, Size};
/// use manguebyte_ui::layout::{Arrange, Insets, LayoutNode, Style, scroll_viewport, solve};
///
/// // A view 100 by 60 that scrolls up and down, with 10 for its bar and 4
/// // of padding, over a child 200 tall.
/// let style = Style { arrange: Arrange::Scroll { x: false, y: true }, ..Style::leaf() }
///     .with_padding(Insets::all(4.0));
/// let nodes = vec![
///     LayoutNode::new(style, Size::new(10.0, 10.0), vec![1]),
///     LayoutNode::leaf(Style::leaf(), Size::new(50.0, 200.0)),
/// ];
/// let placed = solve(&nodes, 0, Rect::new(0.0, 0.0, 100.0, 60.0), 1.0);
/// let viewport = scroll_viewport(&nodes[0], placed[0], 1.0).expect("it scrolls");
/// assert_eq!(viewport.rect, Rect::new(4.0, 4.0, 82.0, 52.0));
/// // The child is as wide as the viewport and as tall as it wants.
/// assert_eq!(placed[1].rect, Rect::new(4.0, 4.0, 82.0, 200.0));
/// ```
pub fn scroll_viewport(node: &LayoutNode, placed: Placement, scale: f32) -> Option<Placement> {
    let Arrange::Scroll { x, y } = node.style.arrange else {
        return None;
    };
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let inner = placed.inset(node.style.padding, scale).rect;
    Some(snap(viewport_of(node, inner, x, y), scale))
}

impl Placement {
    /// The placement of the device rectangle `device` at `scale`, with
    /// the same rectangle in logical pixels beside it, as [`solve`] gives
    /// them. A scale that is not a positive finite number counts as 1.
    ///
    /// ```
    /// use manguebyte_gfx::{IRect, Rect};
    /// use manguebyte_ui::layout::Placement;
    ///
    /// let placed = Placement::from_device(IRect::new(3, 4, 10, 6), 2.0);
    /// assert_eq!(placed.rect, Rect::new(1.5, 2.0, 5.0, 3.0));
    /// ```
    pub fn from_device(device: IRect, scale: f32) -> Placement {
        let scale = if scale.is_finite() && scale > 0.0 {
            f64::from(scale)
        } else {
            1.0
        };
        let logical = |pixels: f64| (pixels / scale) as f32;
        Placement {
            rect: Rect::new(
                logical(f64::from(device.x)),
                logical(f64::from(device.y)),
                logical(f64::from(device.width)),
                logical(f64::from(device.height)),
            ),
            device,
        }
    }
}

/// Where a child goes across the room from `start`, `room` long, given its
/// natural length and limits: its start and its length.
fn across(start: f32, room: f32, natural: f32, min: f32, max: f32, align: Align) -> (f32, f32) {
    let size = match align {
        Align::Stretch => clamp(room, min, max),
        Align::Start | Align::Center | Align::End => clamp(natural.min(room), min, max),
    };
    let position = match align {
        Align::Start | Align::Stretch => start,
        Align::Center => start + (room - size) * 0.5,
        Align::End => start + (room - size),
    };
    (position, size)
}

/// Shares `room` along a row or column among `tracks`, writing their
/// lengths into `sizes`, and returns where the first one starts (from the
/// start of the room) and the gap between neighbours.
fn distribute(
    tracks: &[Track],
    room: f32,
    spacing: f32,
    justify: Justify,
    sizes: &mut Vec<f32>,
) -> (f32, f32) {
    sizes.clear();
    let count = tracks.len();
    if count == 0 {
        return (0.0, spacing);
    }
    let room = room - spacing * (count - 1) as f32;
    // Children with a weight start from their minimum.
    sizes.extend(tracks.iter().map(|track| {
        if track.flex > 0.0 {
            track.min
        } else {
            clamp(track.natural, track.min, track.max)
        }
    }));
    let mut free = room - sizes.iter().sum::<f32>();

    // Grow the weighted ones. Each round either places them all or stops
    // at least one at its maximum, so there are at most `count` rounds.
    if free > 0.0 {
        let mut growing: Vec<bool> = tracks.iter().map(|track| track.flex > 0.0).collect();
        for _ in 0..count {
            let weight: f32 = tracks
                .iter()
                .zip(&growing)
                .filter(|&(_, &grows)| grows)
                .map(|(track, _)| track.flex)
                .sum();
            if !(free > 0.0 && weight > 0.0) {
                break;
            }
            let mut stopped = false;
            for (slot, track) in tracks.iter().enumerate() {
                if growing[slot] && sizes[slot] + free * track.flex / weight >= track.max {
                    stopped = true;
                }
            }
            if !stopped {
                for (slot, track) in tracks.iter().enumerate() {
                    if growing[slot] {
                        sizes[slot] += free * track.flex / weight;
                    }
                }
                free = 0.0;
                break;
            }
            for (slot, track) in tracks.iter().enumerate() {
                if growing[slot] && sizes[slot] + free * track.flex / weight >= track.max {
                    growing[slot] = false;
                }
            }
            for (slot, track) in tracks.iter().enumerate() {
                if !growing[slot] && track.flex > 0.0 && sizes[slot] < track.max {
                    free -= track.max - sizes[slot];
                    sizes[slot] = track.max;
                }
            }
        }
    }

    // Shrink everything towards its minimum when even that does not fit.
    if free < 0.0 {
        let give: f32 = sizes
            .iter()
            .zip(tracks)
            .map(|(&size, track)| size - track.min)
            .sum();
        if give > 0.0 {
            let fraction = (-free / give).min(1.0);
            for (size, track) in sizes.iter_mut().zip(tracks) {
                *size -= (*size - track.min) * fraction;
            }
        }
        free = 0.0;
    }

    match justify {
        Justify::Start => (0.0, spacing),
        Justify::Center => (free * 0.5, spacing),
        Justify::End => (free, spacing),
        Justify::SpaceBetween if count > 1 => (0.0, spacing + free / (count - 1) as f32),
        Justify::SpaceBetween => (0.0, spacing),
    }
}

/// `rect` with its edges moved to the nearest device pixel.
fn snap(rect: Rect, scale: f32) -> Placement {
    let scale = f64::from(scale);
    let edge = |value: f32| -> i32 {
        let device = (f64::from(value) * scale).round();
        // `as` maps NaN to 0 and clamps the rest to the range of `i32`.
        device as i32
    };
    let x0 = edge(rect.x);
    let y0 = edge(rect.y);
    let x1 = edge(rect.x + rect.width).max(x0);
    let y1 = edge(rect.y + rect.height).max(y0);
    // `scale` came from an `f32`, so it goes back exactly.
    Placement::from_device(IRect::from_edges(x0, y0, x1, y1), scale as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(width: f32, height: f32) -> LayoutNode {
        LayoutNode::leaf(Style::leaf(), Size::new(width, height))
    }

    #[test]
    fn natural_sizes_add_up_along_the_axis_and_take_the_largest_across() {
        let nodes = vec![
            LayoutNode::new(
                Style::row()
                    .with_padding(Insets::new(1.0, 2.0, 3.0, 4.0))
                    .with_spacing(5.0),
                Size::default(),
                vec![1, 2, 3],
            ),
            leaf(10.0, 7.0),
            leaf(20.0, 9.0),
            leaf(30.0, 8.0),
        ];
        let order = preorder(&nodes, 0);
        assert_eq!(order.nodes, [0, 1, 2, 3]);
        let mut natural = vec![Size::default(); 4];
        for &index in order.nodes.iter().rev() {
            natural[index] = measure(&nodes, &order, index, &natural);
        }
        // 10 + 20 + 30, two gaps of 5, padding 1 and 3; 9 tall plus 2 and 4.
        assert_eq!(natural[0], Size::new(74.0, 15.0));
    }

    #[test]
    fn weights_share_the_room_and_respect_maximums() {
        let tracks = [
            Track {
                natural: 10.0,
                min: 0.0,
                max: f32::INFINITY,
                flex: 1.0,
            },
            Track {
                natural: 10.0,
                min: 0.0,
                max: 30.0,
                flex: 1.0,
            },
            Track {
                natural: 40.0,
                min: 0.0,
                max: f32::INFINITY,
                flex: 0.0,
            },
            Track {
                natural: 0.0,
                min: 5.0,
                max: f32::INFINITY,
                flex: 2.0,
            },
        ];
        let mut sizes = Vec::new();
        // 200 of room: 40 for the one with no weight, and 155 to share
        // 1:1:2 above the minimums 0, 0 and 5. The second would pass its
        // maximum, so it stops at 30, and the 125 left are shared 1:2
        // between the first and the fourth.
        let (offset, gap) = distribute(&tracks, 200.0, 0.0, Justify::Start, &mut sizes);
        assert_eq!((offset, gap), (0.0, 0.0));
        assert_eq!(sizes[1], 30.0);
        assert_eq!(sizes[2], 40.0);
        assert!((sizes[0] - 125.0 / 3.0).abs() < 1e-4, "{sizes:?}");
        assert!((sizes[3] - (5.0 + 250.0 / 3.0)).abs() < 1e-4, "{sizes:?}");
        assert!((sizes.iter().sum::<f32>() - 200.0).abs() < 1e-3);
    }

    #[test]
    fn too_little_room_shrinks_towards_the_minimums() {
        let tracks = [
            Track {
                natural: 100.0,
                min: 50.0,
                max: f32::INFINITY,
                flex: 0.0,
            },
            Track {
                natural: 100.0,
                min: 90.0,
                max: f32::INFINITY,
                flex: 0.0,
            },
        ];
        let mut sizes = Vec::new();
        // 30 too few, out of 60 that can be given up: each gives half.
        distribute(&tracks, 170.0, 0.0, Justify::Start, &mut sizes);
        assert_eq!(sizes, [75.0, 95.0]);
        // Far too few: both at their minimum, running past the end.
        distribute(&tracks, 10.0, 0.0, Justify::Start, &mut sizes);
        assert_eq!(sizes, [50.0, 90.0]);
    }

    #[test]
    fn room_left_over_is_justified() {
        let tracks = [Track {
            natural: 10.0,
            min: 0.0,
            max: f32::INFINITY,
            flex: 0.0,
        }; 3];
        let mut sizes = Vec::new();
        let mut at = |justify| distribute(&tracks, 100.0, 5.0, justify, &mut sizes);
        assert_eq!(at(Justify::Start), (0.0, 5.0));
        assert_eq!(at(Justify::Center), (30.0, 5.0));
        assert_eq!(at(Justify::End), (60.0, 5.0));
        assert_eq!(at(Justify::SpaceBetween), (0.0, 35.0));
    }

    #[test]
    fn snapping_rounds_each_edge_to_the_nearest_device_pixel() {
        let placed = snap(Rect::new(0.3, 0.7, 10.4, 10.2), 2.0);
        // Edges 0.6, 1.4, 21.4 and 21.8 device pixels.
        assert_eq!(placed.device, IRect::new(1, 1, 20, 21));
        assert_eq!(placed.rect, Rect::new(0.5, 0.5, 10.0, 10.5));
    }

    #[test]
    fn hostile_trees_and_numbers_do_not_panic() {
        let nodes = vec![
            LayoutNode::new(
                Style::row()
                    .with_spacing(f32::NAN)
                    .with_padding(Insets::all(f32::INFINITY)),
                Size::new(f32::NAN, -1.0),
                vec![1, 0, 7, 1, 2],
            ),
            LayoutNode::new(
                Style::column()
                    .with_flex(f32::INFINITY)
                    .with_min(Size::new(f32::NAN, 1e30))
                    .with_max(Size::new(-5.0, f32::NAN)),
                Size::new(f32::INFINITY, f32::MAX),
                vec![0, 2],
            ),
            leaf(f32::MAX, f32::MAX),
        ];
        for scale in [f32::NAN, 0.0, -2.0, 1e-30, 1.0, 1e30] {
            for area in [
                Rect::new(0.0, 0.0, 100.0, 100.0),
                Rect::new(f32::NAN, f32::INFINITY, f32::NAN, -10.0),
                Rect::new(-1e30, 1e30, f32::MAX, f32::MAX),
            ] {
                let placed = solve(&nodes, 0, area, scale);
                assert_eq!(placed.len(), 3);
                assert!(
                    placed
                        .iter()
                        .all(|p| p.rect.width >= 0.0 && p.rect.height >= 0.0)
                );
            }
        }
        assert_eq!(
            solve(&nodes, 9, Rect::new(0.0, 0.0, 1.0, 1.0), 1.0),
            vec![Placement::default(); 3]
        );
    }
}
