//! The root of an interface: the tree, its size and scale, the frame and
//! input.

use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use manguebyte_gfx::{Color, IRect, Pixmap, Point, Rect, Renderer, Scene, Size};
use manguebyte_plugin::editor::{EditorContext, MAX_SCALE, MIN_SCALE};

mod drag;
mod files;
mod hover;
mod menus;
mod pages;

pub use hover::TOOLTIP_DELAY;
pub(crate) use menus::{Anchor, MenuRequest};

use crate::access::{AccessEntry, AccessItem, Action};
use crate::clipboard::{Clipboard, MemoryClipboard};
use crate::damage::{MAX_DAMAGE, tidy};
use crate::event::{Event, Handled, Key, MenuEvent};
use crate::layout::{self, Placement};
use crate::motion::Motion;
use crate::scroll::{ScrollMetrics, snap};
use crate::theme::Theme;
use crate::tree::{NodeId, Tree};
use crate::view::IntoView;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, Requests, UpdateCx, Widget};

/// What one call to [`Root::frame`] did.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    /// The device rectangles of the back buffer that were drawn again, at
    /// most [`MAX_DAMAGE`]; a window copies these to the screen. Empty
    /// when nothing changed.
    pub damage: &'a [IRect],
    /// The back buffer, premultiplied RGBA, as large as the interface in
    /// device pixels.
    pub pixels: &'a Pixmap,
    /// Whether something is moving on its own and wants another frame
    /// soon, such as a meter still falling or a transition still running.
    /// False once everything has settled: a window may then stop asking
    /// for frames for motion's sake.
    pub animating: bool,
    /// How many widgets recorded a scene that differs from the one they
    /// had (every widget, when everything is drawn again).
    pub painted: usize,
    /// Whether layout ran.
    pub laid_out: bool,
    /// The widgets whose description for assistive technology changed in
    /// this frame in a way they said a screen reader should hear about
    /// (see [`UpdateCx::request_access_update`]), such as a name that
    /// follows the language, in tree order. A window layer that speaks to
    /// the platform's assistive technology reads each one's new
    /// description with [`Root::access`] and sends it as a change. Empty
    /// when nothing changed.
    pub access: &'a [NodeId],
}

/// An interface: the widget tree, its size and scale, and the back buffer
/// it is drawn into.
///
/// A window layer (or a test) drives it with two calls: [`event`]
/// for each piece of input, and [`frame`] on a clock, about 60 times a
/// second while the interface is visible. A frame
///
/// 1. calls every widget's [`update`](Widget::update), which polls the
///    sources and parameters it shows;
/// 2. lays the tree out again if a widget's size may have changed, or the
///    interface's size or scale did;
/// 3. records a new scene for each widget that asked, and compares it with
///    the one it had: the damage is the device bounds of each scene that
///    differs, before and after, merged into a few rectangles;
/// 4. draws the damaged rectangles of the back buffer again, through
///    `manguebyte-gfx`, and reports them.
///
/// When nothing changed a frame costs one poll per widget, draws nothing
/// and reports no damage. An expose from the system never needs a frame:
/// the back buffer is already up to date.
///
/// A widget's scene that reaches past the widget's rectangle, as a
/// control given less room than its natural size may draw, is clipped to
/// the rectangle, so the damage of any widget stays inside it.
///
/// Everything happens on the thread that made the root, which is the
/// host's main thread inside a plugin. The root holds the editor's
/// [`EditorContext`], through which widgets send their gestures; dropping
/// the root drops the widgets, which ends any gesture still open.
///
/// ```
/// use std::rc::Rc;
/// use std::sync::Arc;
/// use std::time::Duration;
/// use manguebyte_gfx::{IRect, Point, Size};
/// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
/// use manguebyte_plugin::editor::EditorContext;
/// use manguebyte_plugin::editor::testing::RecordingEditorHost;
/// use manguebyte_ui::{Button, Event, Handled, Modifiers, ParamHandle, Root, knob};
///
/// struct Params { mix: FloatParam }
/// param_set!(Params { mix });
/// let params = Arc::new(Params {
///     mix: FloatParam::new("mix", "Mix", FloatRange::linear(0.0, 100.0), 50.0),
/// });
/// // A host that stores what it is sent, as a standalone program does.
/// let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
/// let (context, link) = EditorContext::attach(host);
/// let view = knob(ParamHandle::new(Arc::clone(&params), |p| &p.mix));
/// let mut root = Root::new(view, Size::new(80.0, 100.0), 1.5, context);
///
/// // The first frame lays out and draws everything.
/// assert_eq!(root.frame(Duration::ZERO).damage, [IRect::new(0, 0, 120, 150)]);
///
/// // A window layer turns what the system delivers into events.
/// let at = Point::new(40.0, 50.0);
/// let press = Event::PointerDown { at, button: Button::Primary, clicks: 1, modifiers: Modifiers::NONE };
/// assert_eq!(root.event(press), Handled::Yes);
/// let moved = Event::PointerMove { at: Point::new(40.0, 30.0), modifiers: Modifiers::NONE };
/// assert_eq!(root.event(moved), Handled::Yes);
/// let release = Event::PointerUp { at, button: Button::Primary, modifiers: Modifiers::NONE };
/// assert_eq!(root.event(release), Handled::Yes);
/// // Twenty pixels up is a tenth of the theme's 200-pixel drag range.
/// assert_eq!(params.mix.value(), 60.0);
///
/// // The next frame draws only the knob again.
/// let frame = root.frame(Duration::from_millis(16));
/// assert!(!frame.damage.is_empty() && frame.painted == 1);
///
/// drop(root);
/// drop(link);
/// ```
///
/// A root stays on the thread that made it:
///
/// ```compile_fail,E0277
/// fn close_on_another_thread(root: manguebyte_ui::Root) {
///     std::thread::spawn(move || drop(root));
/// }
/// ```
///
/// [`event`]: Root::event
/// [`frame`]: Root::frame
pub struct Root {
    tree: Tree,
    theme: Theme,
    size: Size,
    scale: f32,
    context: EditorContext,
    renderer: Renderer,
    pixels: Pixmap,
    composite: Scene,
    /// Where a widget's scene that reaches past its rectangle is clipped.
    cut: Scene,
    damage: Vec<IRect>,
    /// The widgets that asked, in the last frame, for their description to
    /// be sent again (see [`Frame::access`]).
    access_updates: Vec<NodeId>,
    focus: Option<NodeId>,
    capture: Option<NodeId>,
    clipboard: Rc<dyn Clipboard>,
    /// Keys whose press a widget used and whose release has not come.
    keys_used: Vec<Key>,
    /// The menus open over the interface, if any.
    menus: Option<menus::Menus>,
    /// The device pixels that widgets taken away since the last frame
    /// covered (closed menus), drawn again then.
    removed: Vec<IRect>,
    /// Where the pointer was last.
    pointer: Option<Point>,
    /// The widget under the pointer while no button is held.
    hover: Option<NodeId>,
    /// A button is held: the hovered widget stays as it is until it is let
    /// go.
    pressed: bool,
    /// The tooltip waiting to show, or showing.
    tip: Option<hover::Tip>,
    /// The drag running, if any.
    drag: Option<drag::DragState>,
    /// The subtree that has all input, if any (see `UpdateCx::set_modal`).
    modal: Option<pages::Modal>,
    /// The widget whose tooltip a press or a key took away, until the
    /// pointer leaves it.
    tip_quiet: Option<NodeId>,
    /// Set while a widget hears that it gained or lost the focus: what it
    /// asks for the focus then is not done, so that focus cannot bounce.
    notifying: bool,
    /// Whether the system lets interfaces animate (see
    /// [`Root::set_system_motion`]).
    system_motion: bool,
    /// The widget a press went down on, until it is let go.
    pressed_on: Option<NodeId>,
    /// Whether the focus is shown: after the keyboard was used, not after
    /// a press.
    focus_visible: bool,
    /// Whether the last frame had something still moving.
    animating: bool,
    needs_layout: bool,
    /// Every widget is drawn again, over the whole buffer, next frame.
    full: bool,
    now: Duration,
    last_frame: Option<Duration>,
}

/// The furthest a widget may draw beyond its rectangle (see
/// `Widget::overflow`), in logical pixels.
const MAX_OVERFLOW: f32 = 128.0;

/// The most keys remembered as held after the interface used their press:
/// more than hands hold at once. A key held beyond these is forgotten,
/// and its release goes on as any other.
const MAX_KEYS_HELD: usize = 16;

/// A scale an interface may be drawn at.
fn clean_scale(scale: f32) -> Option<f32> {
    let (min, max) = (MIN_SCALE as f32, MAX_SCALE as f32);
    (scale.is_finite() && (min..=max).contains(&scale)).then_some(scale)
}

/// A logical size an interface may have: finite and not negative.
fn clean_size(size: Size) -> Size {
    let clean = |v: f32| if v.is_finite() && v > 0.0 { v } else { 0.0 };
    Size::new(clean(size.width), clean(size.height))
}

/// The most device pixels a side of the back buffer has. An interface
/// larger than this is drawn only up to it. It is the largest side for
/// which `manguebyte-gfx` promises that a partial render equals a whole
/// one, and still means a buffer of up to 256 MiB.
pub const MAX_BUFFER_SIDE: u32 = 8191;

/// The back buffer for `size` at `scale`: whole device pixels, at least
/// one each way and at most [`MAX_BUFFER_SIDE`].
fn buffer(size: Size, scale: f32) -> Pixmap {
    let side = |v: f32| -> u32 {
        let pixels = (f64::from(v) * f64::from(scale)).round();
        (pixels as u32).clamp(1, MAX_BUFFER_SIDE)
    };
    let (width, height) = (side(size.width), side(size.height));
    Pixmap::new(width, height)
        .or_else(|| Pixmap::new(1, 1))
        .unwrap_or_else(|| unreachable!("a pixmap of one pixel can always be made"))
}

/// `placement` moved by `by` device pixels.
fn shifted(placement: Placement, by: (i32, i32), scale: f32) -> Placement {
    if by == (0, 0) {
        return placement;
    }
    let device = IRect::new(
        placement.device.x.saturating_add(by.0),
        placement.device.y.saturating_add(by.1),
        placement.device.width,
        placement.device.height,
    );
    Placement::from_device(device, scale)
}

/// Scrolls `metrics` to `offset`, on whole device pixels at `scale`, kept
/// within the content once the node has been laid out (before that the
/// next layout keeps it within). Returns whether the offset changed.
fn set_offset(metrics: &mut ScrollMetrics, offset: Point, scale: f32) -> bool {
    let laid_out = metrics.viewport.width > 0.0 || metrics.viewport.height > 0.0;
    let kept = if laid_out {
        metrics.clamp(offset)
    } else {
        let clean = |v: f32| if v.is_finite() { v.max(0.0) } else { 0.0 };
        Point::new(clean(offset.x), clean(offset.y))
    };
    let mut snapped = Point::new(
        snap(f64::from(kept.x), scale) as f32,
        snap(f64::from(kept.y), scale) as f32,
    );
    if laid_out {
        // Rounding the largest offset up must not pass it.
        let max = metrics.max_offset();
        let down = |v: f32, max: f32| {
            if v > max {
                (f64::from(max) * f64::from(scale)).floor() as f32 / scale
            } else {
                v
            }
        };
        snapped = Point::new(down(snapped.x, max.x), down(snapped.y, max.y));
    }
    if snapped == metrics.offset {
        return false;
    }
    metrics.offset = snapped;
    true
}

impl Root {
    /// The interface `view` describes, `size` logical pixels large, drawn
    /// at `scale` device pixels per logical pixel, in the default
    /// [`Theme`], sending its gestures through `context`.
    ///
    /// A scale outside the editor contract's
    /// [`MIN_SCALE`]`..=`[`MAX_SCALE`], or not a number, counts as 1; a
    /// size that is negative or not finite counts as 0. The back buffer
    /// has at least one pixel each way and at most [`MAX_BUFFER_SIDE`].
    /// Nothing is drawn until the first [`frame`](Root::frame).
    pub fn new(view: impl IntoView, size: Size, scale: f32, context: EditorContext) -> Root {
        let size = clean_size(size);
        let scale = clean_scale(scale).unwrap_or(1.0);
        Root {
            tree: Tree::build(view.into_view()),
            theme: Theme::default(),
            size,
            scale,
            context,
            renderer: Renderer::new(),
            pixels: buffer(size, scale),
            composite: Scene::new(),
            cut: Scene::new(),
            damage: Vec::new(),
            access_updates: Vec::new(),
            focus: None,
            capture: None,
            clipboard: Rc::new(MemoryClipboard::new()),
            keys_used: Vec::with_capacity(MAX_KEYS_HELD),
            menus: None,
            removed: Vec::new(),
            pointer: None,
            hover: None,
            pressed: false,
            tip: None,
            tip_quiet: None,
            drag: None,
            modal: None,
            notifying: false,
            system_motion: true,
            pressed_on: None,
            focus_visible: false,
            animating: false,
            needs_layout: true,
            full: true,
            now: Duration::ZERO,
            last_frame: None,
        }
    }

    /// The same root drawn in `theme`.
    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Root {
        self.set_theme(theme);
        self
    }

    /// Draws the interface in `theme` from the next frame on: everything
    /// is measured, laid out and drawn again.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
        self.invalidate_all();
    }

    /// The theme.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Follows the system's setting for motion: `false` when the person
    /// asked the system for less of it (on Windows, client area animations
    /// turned off), which makes every transition a jump whatever the
    /// theme says. A window layer sets it when it opens the interface.
    pub fn set_system_motion(&mut self, enabled: bool) {
        self.system_motion = enabled;
    }

    /// The motion tokens in effect: the theme's, turned off if the theme or
    /// the system turns motion off.
    pub fn motion(&self) -> Motion {
        if self.system_motion {
            self.theme.motion
        } else {
            self.theme.motion.reduced()
        }
    }

    /// Whether the widget with the focus shows it, as `:focus-visible`
    /// does on the web: after the keyboard was used (Tab, the arrows, any
    /// key) or assistive technology moved the focus, and not after a press
    /// of the pointer, which hides it again. The focus itself, and what
    /// assistive technology is told about it, does not change.
    pub fn focus_visible(&self) -> bool {
        self.focus_visible
    }

    /// Whether the last frame had a transition or another animation still
    /// running (see [`Frame::animating`]).
    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// The size, in logical pixels.
    pub fn size(&self) -> Size {
        self.size
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// How the editor reaches its host.
    pub fn context(&self) -> &EditorContext {
        &self.context
    }

    /// The same root, copying to and pasting from `clipboard`.
    #[must_use]
    pub fn with_clipboard(mut self, clipboard: Rc<dyn Clipboard>) -> Root {
        self.set_clipboard(clipboard);
        self
    }

    /// Copies to and pastes from `clipboard` from now on, in place of the
    /// [`MemoryClipboard`] a root starts with.
    pub fn set_clipboard(&mut self, clipboard: Rc<dyn Clipboard>) {
        self.clipboard = clipboard;
    }

    /// The clipboard widgets copy to and paste from.
    pub fn clipboard(&self) -> &Rc<dyn Clipboard> {
        &self.clipboard
    }

    /// Whether the widget with the keyboard focus takes typed text now: a
    /// text input, or a knob whose value is being typed. A window layer
    /// then asks the system for every key, Tab, Enter and Escape included,
    /// and keeps the keys that type from the host's shortcuts.
    pub fn wants_text(&self) -> bool {
        self.focus
            .and_then(|id| self.access(id))
            .is_some_and(|entry| entry.node.state.editable)
    }

    /// Where the text caret of the widget with the focus is, in logical
    /// pixels, while text is being typed into it (see [`Widget::caret`]).
    pub fn caret(&self) -> Option<Rect> {
        let id = self.focus?;
        let node = self.tree.get(id)?;
        let cx = AccessCx {
            theme: &self.theme,
            rect: node.content,
            scroll: node.scroll,
            focused: true,
        };
        node.widget.caret(&cx)
    }

    /// Changes the size and scale. The back buffer is made again and
    /// everything is laid out and drawn on the next frame. A scale that
    /// cannot be used keeps the one in use; a size that cannot counts as
    /// 0.
    pub fn resize(&mut self, size: Size, scale: f32) {
        let size = clean_size(size);
        let scale = clean_scale(scale).unwrap_or(self.scale);
        if size == self.size && scale == self.scale {
            return;
        }
        self.size = size;
        self.scale = scale;
        self.pixels = buffer(size, scale);
        self.invalidate_all();
    }

    fn invalidate_all(&mut self) {
        for node in &mut self.tree.nodes {
            node.measure = true;
            node.paint = true;
            node.painted = None;
        }
        self.needs_layout = true;
        self.full = true;
    }

    /// The back buffer, as the last frame left it.
    pub fn pixels(&self) -> &Pixmap {
        &self.pixels
    }

    /// The scene the back buffer was last drawn from: the background, then
    /// every widget's scene in tree order. Its [`dump`](Scene::dump) is an
    /// exact description of what is on screen.
    pub fn scene(&self) -> &Scene {
        &self.composite
    }

    /// The scene widget `id` last recorded.
    pub fn widget_scene(&self, id: NodeId) -> Option<&Scene> {
        self.tree.get(id).map(|node| &node.scene)
    }

    /// Where widget `id` is, in logical pixels and in device pixels.
    pub fn bounds(&self, id: NodeId) -> Option<(Rect, IRect)> {
        self.tree.get(id).map(|node| (node.rect, node.device))
    }

    /// The widget with the keyboard focus, if any.
    pub fn focus(&self) -> Option<NodeId> {
        self.focus
    }

    /// The widget holding the pointer during a drag, if any.
    pub fn pointer_holder(&self) -> Option<NodeId> {
        self.capture
    }

    /// The number of widgets in the tree.
    pub fn len(&self) -> usize {
        self.tree.nodes.len()
    }

    /// Whether the tree is empty; it never is, as it has at least the
    /// widget of the view it was made from.
    pub fn is_empty(&self) -> bool {
        self.tree.nodes.is_empty()
    }

    /// How widget `id` describes itself, with the focus filled in and the
    /// name given with [`IntoView::named`] in place of its own.
    ///
    /// `None` for a widget inside a child that a container does not show
    /// (see [`Shown`](crate::Shown)): assistive technology does not see
    /// it.
    pub fn access(&self, id: NodeId) -> Option<AccessEntry> {
        let node = self.tree.get(id).filter(|node| !node.hidden)?;
        let focused = self.focus == Some(id);
        let cx = AccessCx {
            theme: &self.theme,
            rect: node.content,
            scroll: node.scroll,
            focused,
        };
        let mut access = node.widget.access(&cx);
        access.state.focused = focused;
        if let Some(name) = &node.name {
            access.name.clone_from(name);
        }
        if let Some(tooltip) = &node.tooltip {
            access.description = Some(tooltip.clone());
        }
        Some(AccessEntry {
            id,
            item: None,
            parent: node.parent,
            node: access,
            bounds: node.rect,
            device: node.device,
        })
    }

    /// The widgets whose description changed in the last frame in a way
    /// they said assistive technology should hear about (see
    /// [`Frame::access`]).
    pub fn access_updates(&self) -> &[NodeId] {
        &self.access_updates
    }

    /// Every widget's description, in tree order, each followed by its
    /// items (see [`Widget::access_items`]).
    pub fn access_tree(&self) -> Vec<AccessEntry> {
        let mut entries = Vec::with_capacity(self.tree.nodes.len());
        let mut items = Vec::new();
        for index in 0..self.tree.nodes.len() {
            let id = NodeId::from_index(index);
            entries.extend(self.access(id));
            self.items_of(id, &mut items);
            entries.extend(items.drain(..).map(|item| AccessEntry {
                id,
                item: Some(item.key),
                parent: Some(id),
                node: item.node,
                bounds: item.bounds,
                device: item.bounds.to_device(self.scale),
            }));
        }
        entries
    }

    /// The items widget `id` describes, into `items`.
    fn items_of(&self, id: NodeId, items: &mut Vec<AccessItem>) {
        let Some(node) = self.tree.get(id).filter(|node| !node.hidden) else {
            return;
        };
        let cx = AccessCx {
            theme: &self.theme,
            rect: node.content,
            scroll: node.scroll,
            focused: self.focus == Some(id),
        };
        node.widget.access_items(&cx, items);
    }

    /// Carries out `action` on the item `key` of widget `id` (see
    /// [`Widget::access_items`]), as assistive technology asks: only if the
    /// item lists it among its actions. For [`Action::Focus`] the widget
    /// itself takes the focus first. Returns whether it was done. An open
    /// menu closes first.
    pub fn act_item(&mut self, id: NodeId, key: usize, action: Action) -> bool {
        if id.index() >= self.tree.main || !self.reachable(id) {
            return false;
        }
        if self.menus.is_some() {
            self.close_menus(MenuEvent::Closed);
        }
        let mut items = Vec::new();
        self.items_of(id, &mut items);
        let Some(item) = items.into_iter().find(|item| item.key == key) else {
            return false;
        };
        if !item.node.actions.allows(action) {
            return false;
        }
        if action == Action::Focus {
            if !self.focusable(id) {
                return false;
            }
            self.set_focus(Some(id));
            self.reveal(id);
        }
        let done = self.deliver(id, |widget, cx| widget.act_item(cx, key, action));
        done || action == Action::Focus
    }

    /// Where scroll node `id` is scrolled, as of the last layout and the
    /// scrolling since; `None` for a widget that does not scroll.
    pub fn scroll_metrics(&self, id: NodeId) -> Option<ScrollMetrics> {
        self.tree.get(id).and_then(|node| node.scroll)
    }

    /// Scrolls node `id` to `offset`, as [`EventCx::scroll_to`] does.
    pub(crate) fn set_scroll(&mut self, id: NodeId, offset: Point) {
        let scale = self.scale;
        if let Some(metrics) = self
            .tree
            .nodes
            .get_mut(id.index())
            .and_then(|node| node.scroll.as_mut())
        {
            if set_offset(metrics, offset, scale) {
                self.needs_layout = true;
            }
        }
    }

    /// Scrolls the scroll views that `id` is inside, innermost first, as
    /// little as brings it into their viewports: all of it, or its top
    /// left corner when it is larger than a viewport.
    fn reveal(&mut self, id: NodeId) {
        if id.index() >= self.tree.main {
            return;
        }
        let Some(node) = self.tree.get(id) else {
            return;
        };
        let mut rect = node.rect;
        let mut at = node.parent;
        while let Some(parent) = at {
            let Some(node) = self.tree.get(parent) else {
                return;
            };
            at = node.parent;
            let Some(metrics) = node.scroll else {
                continue;
            };
            let view = metrics.viewport;
            let along = |start: f32, length: f32, view_start: f32, view_length: f32| {
                if length >= view_length || start < view_start {
                    start - view_start
                } else if start + length > view_start + view_length {
                    start + length - (view_start + view_length)
                } else {
                    0.0
                }
            };
            let by = Point::new(
                if metrics.x {
                    along(rect.x, rect.width, view.x, view.width)
                } else {
                    0.0
                },
                if metrics.y {
                    along(rect.y, rect.height, view.y, view.height)
                } else {
                    0.0
                },
            );
            if by == Point::ZERO {
                continue;
            }
            let before = metrics.offset;
            self.set_scroll(parent, Point::new(before.x + by.x, before.y + by.y));
            let after = self
                .scroll_metrics(parent)
                .map_or(before, |metrics| metrics.offset);
            rect = rect.translate(Point::new(before.x - after.x, before.y - after.y));
        }
    }

    /// Whether a menu is open over the interface (see [`crate::menu`]): it
    /// has every key and the pointer until it closes.
    pub fn has_menu(&self) -> bool {
        self.menus.is_some()
    }

    /// The topmost widget at `at`, if any, open menus and a tooltip
    /// included.
    pub fn hit(&self, at: Point) -> Option<NodeId> {
        self.hit_among(at, self.tree.nodes.len())
    }

    /// The topmost of the interface's own widgets at `at`, if any.
    fn hit_main(&self, at: Point) -> Option<NodeId> {
        self.hit_among(at, self.tree.main)
    }

    fn hit_among(&self, at: Point, count: usize) -> Option<NodeId> {
        // Later widgets are drawn over earlier ones, and children over
        // their parents.
        // A widget scrolled out of a view's sight is not there to hit.
        // A hidden widget is not there either, and while a subtree is modal
        // nothing of the interface outside it is.
        let (start, end) = match self.modal {
            Some(modal) if count <= self.tree.main => (
                modal.node.index(),
                self.tree.get(modal.node).map_or(0, |node| node.end),
            ),
            _ => (0, count),
        };
        (0..count.min(self.tree.nodes.len()))
            .rev()
            .filter(|&index| index >= self.tree.main || (start..end).contains(&index))
            .find(|&index| {
                let node = &self.tree.nodes[index];
                !node.hidden
                    && node.rect.contains(at)
                    && node.clip.is_none_or(|(clip, _)| clip.contains(at))
            })
            .map(NodeId::from_index)
    }

    /// The widget the pointer is over, with no button held, if any (see
    /// [`Widget::hover_changed`]).
    pub fn hovered(&self) -> Option<NodeId> {
        self.hover
    }

    /// Carries out `action` on widget `id`, as assistive technology asks:
    /// only if the widget lists it among its actions. Returns whether it
    /// was done.
    ///
    /// While a menu is open, an action on one of its items is the menu's
    /// (pressing an item chooses it), and an action on anything else first
    /// closes the menu.
    ///
    /// While a subtree is modal (a dialog is open), only the widgets inside
    /// it can be acted on.
    pub fn act(&mut self, id: NodeId, action: Action) -> bool {
        if self.menus.is_some() {
            if let Some(done) = self.menu_act(id, action) {
                return done;
            }
        }
        if !self.reachable(id) {
            return false;
        }
        let Some(entry) = self.access(id) else {
            return false;
        };
        if !entry.node.actions.allows(action) {
            return false;
        }
        if action == Action::Focus {
            self.focus_visible = true;
            self.set_focus(Some(id));
            self.reveal(id);
            return true;
        }
        self.deliver(id, |widget, cx| widget.act(cx, action))
    }

    /// Moves the keyboard focus to `id`, or takes it away. Only a widget
    /// that accepts [`Action::Focus`] can have it; returns whether the
    /// focus is where it was asked to be.
    pub fn set_focus_to(&mut self, id: Option<NodeId>) -> bool {
        if self.menus.is_some() && id.is_none_or(|id| self.menu_item_of(id).is_none()) {
            // The focus going out of an open menu closes it.
            self.close_menus(MenuEvent::Closed);
        }
        match id {
            Some(id) if !self.focusable(id) || !self.reachable(id) => false,
            _ => {
                self.focus_visible |= id.is_some();
                self.set_focus(id);
                if let Some(id) = id {
                    self.reveal(id);
                }
                true
            }
        }
    }

    fn focusable(&self, id: NodeId) -> bool {
        self.access(id)
            .is_some_and(|entry| entry.node.actions.focus)
    }

    fn set_focus(&mut self, id: Option<NodeId>) {
        if self.focus == id {
            return;
        }
        for changed in [self.focus, id].into_iter().flatten() {
            if let Some(node) = self.tree.nodes.get_mut(changed.index()) {
                node.paint = true;
            }
        }
        let old = std::mem::replace(&mut self.focus, id);
        // Each hears of it once the focus is where it now is, the one that
        // lost it first.
        let notifying = std::mem::replace(&mut self.notifying, true);
        for (changed, focused) in [(old, false), (id, true)] {
            if let Some(changed) = changed.filter(|at| at.index() < self.tree.nodes.len()) {
                self.deliver(changed, |widget, cx| widget.focus_changed(cx, focused));
            }
        }
        self.notifying = notifying;
    }

    /// Moves the focus to the next widget that takes it, in tree order, or
    /// the previous one; round to the first or last at the ends. Returns
    /// false, and changes nothing, if no widget takes the focus.
    fn move_focus(&mut self, forward: bool) -> bool {
        let focusable = self.focus_order();
        if focusable.is_empty() {
            return false;
        }
        let at = self
            .focus
            .and_then(|focus| focusable.iter().position(|&id| id == focus));
        let next = match (at, forward) {
            (None, true) => 0,
            (None, false) => focusable.len() - 1,
            (Some(at), true) => (at + 1) % focusable.len(),
            (Some(at), false) => (at + focusable.len() - 1) % focusable.len(),
        };
        self.set_focus(Some(focusable[next]));
        // Tab into a widget scrolled out of sight scrolls to it.
        self.reveal(focusable[next]);
        true
    }

    /// Calls `call` on widget `id` with an event context, and acts on what
    /// it asked for.
    fn deliver<R>(
        &mut self,
        id: NodeId,
        call: impl FnOnce(&mut dyn Widget, &mut EventCx<'_>) -> R,
    ) -> R {
        let mut requests = Requests::default();
        let motion = self.motion();
        let node = &mut self.tree.nodes[id.index()];
        let mut cx = EventCx {
            theme: &self.theme,
            motion,
            rect: node.content,
            scroll: node.scroll,
            scale: self.scale,
            focused: self.focus == Some(id),
            captured: self.capture == Some(id),
            now: self.now,
            context: &self.context,
            clipboard: &*self.clipboard,
            id,
            requests: &mut requests,
        };
        let result = call(&mut *node.widget, &mut cx);
        if requests.paint {
            node.paint = true;
        }
        if requests.layout {
            node.measure = true;
            self.needs_layout = true;
        }
        match requests.capture {
            Some(true) => self.capture = Some(id),
            Some(false) if self.capture == Some(id) => self.capture = None,
            _ => {}
        }
        if requests.focus && !self.notifying && self.focusable(id) {
            self.set_focus(Some(id));
        }
        if let Some(offset) = requests.scroll {
            self.set_scroll(id, offset);
        }
        if let Some(menu) = requests.menu.take() {
            // No menu opens over a drag.
            if !self.notifying && self.drag.is_none() {
                self.open_menu(id, menu);
            }
        }
        if let Some(payload) = requests.drag.take() {
            if !self.notifying && self.pressed && id.index() < self.tree.main {
                self.begin_drag(Some(id), payload);
            }
        }
        result
    }

    /// Remembers that the press of `key` was used, so that its release is
    /// used too.
    fn remember_key(&mut self, key: Key) {
        if self.keys_used.contains(&key) {
            return;
        }
        if self.keys_used.len() >= MAX_KEYS_HELD {
            self.keys_used.remove(0);
        }
        self.keys_used.push(key);
    }

    /// `id` and the widgets that contain it, innermost first.
    fn ancestry(&self, id: NodeId) -> Vec<NodeId> {
        let mut path = vec![id];
        let mut at = id;
        while let Some(parent) = self.tree.get(at).and_then(|node| node.parent) {
            path.push(parent);
            at = parent;
        }
        path
    }

    /// Offers `event` to the widget holding the pointer, then to the one
    /// with the focus, until one uses it.
    fn offer_to_keyboard(&mut self, event: &Event) -> Handled {
        let mut tried = None;
        for target in [self.capture, self.focus].into_iter().flatten() {
            if tried == Some(target) {
                continue;
            }
            tried = Some(target);
            if self.deliver(target, |widget, cx| widget.event(cx, event)) == Handled::Yes {
                return Handled::Yes;
            }
        }
        Handled::No
    }

    /// Offers `event`, a key the widget with the focus did not use, to the
    /// widgets that contain it, innermost first, until one uses it; with
    /// nothing focused, to every widget shown, in tree order. Only for keys
    /// that belong to a container, such as Control+Tab for tabs.
    fn offer_to_containers(&mut self, event: &Event) -> Handled {
        let targets: Vec<NodeId> = match self.focus {
            Some(focus) => self.ancestry(focus).into_iter().skip(1).collect(),
            None => (0..self.tree.main)
                .map(NodeId::from_index)
                .filter(|&id| !self.is_hidden(id))
                .collect(),
        };
        for target in targets {
            if target.index() >= self.tree.main || !self.reachable(target) {
                continue;
            }
            if self.deliver(target, |widget, cx| widget.event(cx, event)) == Handled::Yes {
                return Handled::Yes;
            }
        }
        Handled::No
    }

    /// Offers `event` to `id` and then to the widgets containing it, until
    /// one uses it.
    fn bubble(&mut self, id: NodeId, event: &Event) -> Handled {
        for target in self.ancestry(id) {
            if self.deliver(target, |widget, cx| widget.event(cx, event)) == Handled::Yes {
                return Handled::Yes;
            }
        }
        Handled::No
    }

    /// Passes a piece of input to the widgets, and returns whether one
    /// used it.
    ///
    /// - A press goes to the topmost widget under the pointer, then to
    ///   those containing it. It takes the keyboard focus away from the
    ///   widget that has it, unless it lands on that widget or inside it;
    ///   a widget that should take the focus when pressed asks for it
    ///   ([`EventCx::request_focus`]), as knobs and sliders do and
    ///   switches do not, so that a click on a switch leaves Space to the
    ///   host. A widget that takes the pointer then receives every move
    ///   and the release, wherever they happen; nothing else receives
    ///   moves. A press while a widget holds the pointer sends that widget
    ///   [`Event::PointerCancel`] first.
    /// - A move with no button held, and [`Event::PointerLeave`], change
    ///   the hovered widget and the tooltip (see
    ///   [`Widget::hover_changed`] and [`IntoView::tooltip`]); they are not
    ///   used. A press, a wheel turn and a key take a tooltip away.
    /// - The wheel goes to the widget under the pointer, and on outwards.
    /// - Keys go to the widget holding the pointer, then to the one with
    ///   the focus. Tab and Shift+Tab that no widget used move the focus
    ///   through the widgets that take it, in tree order, and are used
    ///   only if there is such a widget; otherwise they are left to the
    ///   host. The release of a key whose press was used is used too,
    ///   whatever the widget does with it, so the host never sees half a
    ///   keystroke.
    /// - Control+Tab and Control+Shift+Tab that the widget with the focus
    ///   does not use go to the widgets that contain it, innermost first
    ///   (with nothing focused, to every widget shown), which is how
    ///   [`tabs`](crate::tabs) change page from anywhere inside them; they
    ///   never move the focus.
    /// - While a subtree is modal (a dialog is open, see
    ///   [`UpdateCx::set_modal`]), only widgets inside it can be pressed or
    ///   focused, Tab goes round inside it, keys that the widget with the
    ///   focus does not use go to the subtree's outermost widget, and every
    ///   key is used.
    /// - Typed text and compositions go where keys go.
    /// - Losing the window's focus ends a drag where it is
    ///   ([`Event::PointerCancel`]) and takes the keyboard focus away.
    /// - While a drag runs (see [`crate::drag`]), moves offer it to the
    ///   widgets under the pointer, the release drops it, and a second
    ///   button, Escape, [`Event::PointerCancel`] and losing the window's
    ///   focus cancel it; the wheel, other keys and typed text go on as
    ///   usual.
    /// - Tab, and assistive technology, giving the focus to a widget
    ///   scrolled out of a [`scroll_view`](crate::scroll_view)'s sight
    ///   scroll it into sight. A widget a view hides cannot be pressed
    ///   where it would be.
    /// - While a menu is open, the menu has everything: see
    ///   [`crate::menu`]. Every event is used then.
    /// - [`Event::Menu`] is the root's to send, and is not used.
    ///
    /// Positions that are not numbers hit nothing. Nothing is drawn until
    /// the next frame.
    pub fn event(&mut self, event: Event) -> Handled {
        if let Some(at) = event.position().filter(|at| at.is_finite()) {
            self.pointer = Some(at);
        }
        match event {
            Event::PointerDown { at, .. } => {
                self.pressed = true;
                // A press hides the focus ring, as on the web.
                self.focus_visible = false;
                self.pressed_on = Some(at)
                    .filter(|at| at.is_finite())
                    .and_then(|at| self.hit_main(at));
            }
            Event::PointerUp { .. } | Event::PointerCancel | Event::Focus(false) => {
                self.pressed = false;
                self.pressed_on = None;
            }
            // Using the keyboard shows it.
            Event::Key(key) if key.pressed => self.focus_visible = true,
            _ => {}
        }
        if self.drag.is_some() {
            if let Some(handled) = self.drag_event(&event) {
                return handled;
            }
        }
        if self.menus.is_some() {
            return self.menu_event(event);
        }
        match event {
            Event::PointerDown { .. } | Event::Wheel { .. } => self.quiet_tooltip(),
            Event::Key(key) if key.pressed => self.quiet_tooltip(),
            Event::Focus(false) => self.hide_tooltip(),
            _ => {}
        }
        match event {
            Event::PointerDown { at, .. } => {
                // A press while a widget still holds the pointer (a second
                // button, or a release the system never sent) first ends
                // that hold where it is.
                if self.capture.is_some() {
                    self.event(Event::PointerCancel);
                }
                let Some(target) = self.hit_main(at) else {
                    self.set_focus(None);
                    return Handled::No;
                };
                let inside_focus = self
                    .focus
                    .is_some_and(|focus| self.ancestry(target).contains(&focus));
                // A press on a dialog's scrim leaves the focus in the
                // dialog.
                let on_scrim = self.modal.is_some_and(|modal| modal.node == target);
                if !inside_focus && !on_scrim {
                    self.set_focus(None);
                }
                self.bubble(target, &event)
            }
            Event::PointerMove { at, .. } => match self.capture {
                Some(holder) => self.deliver(holder, |widget, cx| widget.event(cx, &event)),
                None => {
                    if !self.pressed && at.is_finite() {
                        self.hover_to(Some(at));
                        if let Some(hovered) = self.hover {
                            self.deliver(hovered, |widget, cx| widget.hover_moved(cx, at));
                        }
                    }
                    Handled::No
                }
            },
            Event::PointerLeave => {
                self.hover_to(None);
                Handled::No
            }
            Event::PointerUp { .. } | Event::PointerCancel => {
                let handled = match self.capture.take() {
                    Some(holder) => {
                        // Delivered as the holder, then let go whatever it
                        // did.
                        self.capture = Some(holder);
                        let handled = self.deliver(holder, |widget, cx| widget.event(cx, &event));
                        self.capture = None;
                        handled
                    }
                    None => Handled::No,
                };
                // Let go, the pointer hovers whatever it is over.
                if let Event::PointerUp { at, .. } = event {
                    if self.capture.is_none() && self.menus.is_none() {
                        self.hover_to(Some(at).filter(|at| at.is_finite()));
                    }
                }
                handled
            }
            Event::Wheel { at, .. } => match self.hit_main(at) {
                Some(target) => self.bubble(target, &event),
                None => Handled::No,
            },
            Event::Key(key) => {
                // A key whose press was used has its release used too,
                // whatever became of the widget that used it, so that the
                // host never sees half a keystroke.
                let held = self.keys_used.iter().position(|&used| used == key.key);
                if !key.pressed {
                    if let Some(at) = held {
                        self.keys_used.swap_remove(at);
                        self.offer_to_keyboard(&event);
                        return Handled::Yes;
                    }
                }
                let mut handled = self.offer_to_keyboard(&event);
                let control_tab = key.key == Key::Tab && key.modifiers.control;
                if handled == Handled::No && control_tab {
                    handled = self.offer_to_containers(&event);
                }
                if handled == Handled::No {
                    if let Some(modal) = self.modal {
                        handled = self.deliver(modal.node, |widget, cx| widget.event(cx, &event));
                    }
                }
                if handled == Handled::No
                    && key.pressed
                    && key.key == Key::Tab
                    && !key.modifiers.control
                    && self.move_focus(!key.modifiers.shift)
                {
                    handled = Handled::Yes;
                }
                // A modal subtree, a dialog, has every key.
                if self.modal.is_some() {
                    handled = Handled::Yes;
                }
                if handled == Handled::Yes && key.pressed && held.is_none() {
                    self.remember_key(key.key);
                }
                handled
            }
            Event::Text(_) | Event::Composition(_) => self.offer_to_keyboard(&event),
            Event::Menu(_) => Handled::No,
            Event::Focus(gained) => {
                if !gained {
                    if self.capture.is_some() {
                        self.event(Event::PointerCancel);
                    }
                    self.set_focus(None);
                    // Their releases go wherever the keyboard went.
                    self.keys_used.clear();
                }
                Handled::Yes
            }
        }
    }

    /// Runs one frame at time `now` on the clock that drives the
    /// interface: polls, lays out, records and draws what changed (see
    /// [`Root`]). `now` should not go backwards; if it does, no time is
    /// taken to have passed.
    pub fn frame(&mut self, now: Duration) -> Frame<'_> {
        let elapsed = self
            .last_frame
            .map_or(Duration::ZERO, |last| now.saturating_sub(last));
        self.last_frame = Some(now);
        self.now = now;

        let mut animating = false;
        self.access_updates.clear();
        let motion = self.motion();
        let (hover, pressed_on) = (self.hover, self.pressed_on);
        let mut focus_wanted = None;
        let mut index = 0;
        while index < self.tree.nodes.len() {
            let focus = self.focus.filter(|_| self.focus_visible);
            let node = &mut self.tree.nodes[index];
            if node.hidden {
                // A child a container hides is not polled, nor is anything
                // inside it.
                index = node.end.max(index + 1);
                continue;
            }
            // The hover, press and focus ring, eased. A scroll view counts
            // as hovered while the pointer is over anything inside it, so
            // that its bars can show.
            let id = Some(NodeId::from_index(index));
            let inside = node.scroll.is_some()
                && hover.is_some_and(|at| (index..node.end).contains(&at.index()));
            let states = (hover == id || inside, pressed_on == id, focus == id);
            let (moved, running) = node.motion.follow(states, now, &motion);
            if moved {
                node.paint = true;
            }
            animating |= running;
            let mut requests = Requests::default();
            let mut cx = UpdateCx {
                theme: &self.theme,
                motion,
                amounts: (
                    node.motion.hover.value(),
                    node.motion.press.value(),
                    node.motion.focus.value(),
                ),
                rect: node.content,
                scroll: node.scroll,
                scale: self.scale,
                now,
                elapsed,
                requests: &mut requests,
            };
            node.widget.update(&mut cx);
            if requests.paint {
                node.paint = true;
            }
            if requests.layout {
                node.measure = true;
                self.needs_layout = true;
            }
            if requests.access {
                self.access_updates.push(NodeId::from_index(index));
            }
            if let (Some(offset), Some(metrics)) = (requests.scroll, node.scroll.as_mut()) {
                if set_offset(metrics, offset, self.scale) {
                    self.needs_layout = true;
                }
            }
            animating |= requests.frame;
            if requests.focus {
                focus_wanted = Some(NodeId::from_index(index));
            }
            // What it hides is hidden before the loop reaches it.
            if let Some(shown) = requests.shown {
                self.show_children(index, shown);
            }
            if let Some(child) = requests.modal {
                self.set_modal_child(index, child);
            }
            index += 1;
        }
        if let Some(id) = focus_wanted {
            if self.focusable(id) && self.reachable(id) {
                let old = self.focus;
                self.set_focus(Some(id));
                self.reveal(id);
                // Their rings follow in this frame, not the next.
                let shown = self.focus.filter(|_| self.focus_visible);
                for changed in [old, Some(id)].into_iter().flatten() {
                    if let Some(node) = self.tree.nodes.get_mut(changed.index()) {
                        let states = (
                            hover == Some(changed),
                            pressed_on == Some(changed),
                            shown == Some(changed),
                        );
                        let (moved, running) = node.motion.follow(states, now, &motion);
                        node.paint |= moved;
                        animating |= running;
                    }
                }
            }
        }
        // A tooltip waiting for the pointer to rest long enough wants the
        // frames that measure it.
        animating |= self.tick_tooltip();
        // A drag scrolls views near their edges and wants frames to do it.
        animating |= self.tick_drag(elapsed);

        let laid_out = self.needs_layout;
        if self.needs_layout {
            self.layout();
        } else if self.drag.as_ref().is_some_and(|drag| drag.moved) {
            self.place_ghost();
        }
        let painted = self.paint();
        if !self.damage.is_empty() {
            self.render();
        }
        self.animating = animating;
        Frame {
            damage: &self.damage,
            pixels: &self.pixels,
            animating,
            painted,
            laid_out,
            access: &self.access_updates,
        }
    }

    fn layout(&mut self) {
        self.needs_layout = false;
        let cx = MeasureCx {
            theme: &self.theme,
            scale: self.scale,
        };
        for (node, item) in self.tree.nodes.iter_mut().zip(&mut self.tree.layout) {
            if node.hidden {
                // Measured once it is shown again.
                continue;
            }
            if node.measure {
                node.measure = false;
                item.content = node.widget.measure(&cx);
            }
            let overflow = node.widget.overflow(&cx);
            let overflow = if overflow.is_finite() {
                overflow.clamp(0.0, MAX_OVERFLOW)
            } else {
                0.0
            };
            if overflow != node.overflow {
                node.overflow = overflow;
                node.paint = true;
            }
        }
        let area = Rect::new(0.0, 0.0, self.size.width, self.size.height);
        let placed = layout::solve(&self.tree.layout, 0, area, self.scale);
        // The interface's own widgets; what is over them is placed after.
        self.place_main(&placed);
        self.place_menus();
        self.place_tooltip();
        self.place_ghost();
    }

    /// Puts the interface's own widgets where layout placed them, moved by
    /// the offsets of the scroll views they are inside and clipped to their
    /// viewports.
    fn place_main(&mut self, placed: &[Placement]) {
        let main = self.tree.main.min(placed.len());
        let scale = self.scale;
        // For each node, how far its children move, in device pixels, and
        // what they are clipped to.
        let mut shifts: Vec<(i32, i32)> = vec![(0, 0); main];
        let mut clips: Vec<Option<(Rect, IRect)>> = vec![None; main];
        for index in 0..main {
            if self.tree.nodes[index].hidden {
                // Not laid out: it keeps where it was until it is shown.
                continue;
            }
            let (shift, clip) = match self.tree.nodes[index].parent {
                Some(parent) if parent.index() < index => {
                    (shifts[parent.index()], clips[parent.index()])
                }
                _ => ((0, 0), None),
            };
            let placement = shifted(placed[index], shift, scale);
            shifts[index] = shift;
            clips[index] = clip;
            let item = &self.tree.layout[index];
            let viewport = layout::scroll_viewport(item, placement, scale);
            let node = &mut self.tree.nodes[index];
            if let (Some(metrics), Some(viewport)) = (node.scroll.as_mut(), viewport) {
                // The content reaches as far as its children, laid out from
                // the viewport's corner before any scrolling.
                let origin = shifted(viewport, (-shift.0, -shift.1), scale).rect;
                let mut content = viewport.rect.size();
                for &child in &item.children {
                    if let Some(child) = placed.get(child) {
                        content.width = content.width.max(child.rect.right() - origin.x);
                        content.height = content.height.max(child.rect.bottom() - origin.y);
                    }
                }
                let before = *metrics;
                metrics.viewport = viewport.rect;
                metrics.content = content;
                let offset = metrics.offset;
                set_offset(metrics, offset, scale);
                // Its bars show where it is.
                let changed = *metrics != before;
                let device = |v: f32| (f64::from(v) * f64::from(scale)).round() as i32;
                shifts[index] = (
                    shift.0 - device(metrics.offset.x),
                    shift.1 - device(metrics.offset.y),
                );
                clips[index] = Some(match clip {
                    Some((rect, device)) => (
                        rect.intersect(&viewport.rect).unwrap_or(Rect::new(
                            viewport.rect.x,
                            viewport.rect.y,
                            0.0,
                            0.0,
                        )),
                        device.intersect(&viewport.device).unwrap_or(IRect::new(
                            viewport.device.x,
                            viewport.device.y,
                            0,
                            0,
                        )),
                    ),
                    None => (viewport.rect, viewport.device),
                });
                if changed {
                    node.paint = true;
                }
            }
            if node.clip != clip {
                node.clip = clip;
                node.paint = true;
            }
            self.apply_placement(index, placement);
        }
    }

    /// Puts widget `index` where layout placed it, and has it drawn again
    /// if it moved.
    fn apply_placement(&mut self, index: usize, placement: Placement) {
        let (Some(node), Some(item)) =
            (self.tree.nodes.get_mut(index), self.tree.layout.get(index))
        else {
            return;
        };
        // A widget with no children draws inside its padding; a
        // container's padding is around its children, and it draws its
        // background across the whole of its rectangle.
        let content = if item.style.arrange == layout::Arrange::Leaf {
            placement.inset(item.style.padding, self.scale).rect
        } else {
            placement.rect
        };
        if node.rect != placement.rect || node.device != placement.device || node.content != content
        {
            node.rect = placement.rect;
            node.device = placement.device;
            node.content = content;
            node.paint = true;
        }
    }

    /// Records the scenes of the widgets that asked, and collects the
    /// damage. Returns how many recorded a different scene.
    fn paint(&mut self) -> usize {
        self.damage.clear();
        let full = std::mem::take(&mut self.full);
        let mut painted = 0;
        for (index, node) in self.tree.nodes.iter_mut().enumerate() {
            if !node.paint || node.hidden {
                continue;
            }
            node.paint = false;
            node.spare.clear();
            let cx = PaintCx {
                theme: &self.theme,
                rect: node.content,
                scroll: node.scroll,
                scale: self.scale,
                focused: self.focus == Some(NodeId::from_index(index)),
                hovered: self.hover == Some(NodeId::from_index(index)),
                now: self.now,
                amounts: (
                    node.motion.hover.value(),
                    node.motion.press.value(),
                    node.motion.focus.value(),
                ),
            };
            // Where the widget may draw: its rectangle grown by its
            // overflow, and inside a scroll view only what the view lets be
            // seen. A widget scrolled out of sight is not asked to draw at
            // all.
            let (rect, device) = if node.overflow > 0.0 {
                let reach = (f64::from(node.overflow) * f64::from(self.scale)).ceil() as i32;
                let d = node.device;
                (
                    node.rect.outset(node.overflow),
                    IRect::from_edges(
                        d.x.saturating_sub(reach),
                        d.y.saturating_sub(reach),
                        (d.right() as i32).saturating_add(reach),
                        (d.bottom() as i32).saturating_add(reach),
                    ),
                )
            } else {
                (node.rect, node.device)
            };
            let (limit, limit_device) = match node.clip {
                Some((clip, clip_device)) => {
                    (rect.intersect(&clip), device.intersect(&clip_device))
                }
                None => (Some(rect), Some(device)),
            };
            let hidden = node.clip.is_some() && (limit.is_none() || limit_device.is_none());
            if !hidden {
                node.widget.paint(&cx, &mut node.spare);
            }
            let limit = limit.unwrap_or(Rect::new(node.rect.x, node.rect.y, 0.0, 0.0));
            let limit_device =
                limit_device.unwrap_or(IRect::new(node.device.x, node.device.y, 0, 0));
            if !full && node.spare == node.scene {
                // Asked to paint, and drew the same thing: nothing to do.
                continue;
            }
            let scale = self.scale;
            let device = |scene: &Scene| {
                scene
                    .bounds()
                    .map(|bounds| bounds.to_device(scale))
                    .filter(|rect| !rect.is_empty())
            };
            let mut now = device(&node.spare);
            if now.is_some_and(|drawn| !limit_device.contains_rect(&drawn)) {
                // It reached past its rectangle, or the viewport it is
                // seen through: cut it to them, so that it covers nothing
                // of its neighbours.
                self.cut.clear();
                let spare = &node.spare;
                self.cut.clipped(limit, |scene| scene.append(spare));
                std::mem::swap(&mut node.spare, &mut self.cut);
                // The clip's edges are the rectangle's device edges, give or
                // take the rounding of `f32`, which can move the bounds a
                // pixel out but leaves that pixel uncovered.
                now = device(&node.spare).and_then(|drawn| drawn.intersect(&limit_device));
                if !full && node.spare == node.scene {
                    continue;
                }
            }
            std::mem::swap(&mut node.scene, &mut node.spare);
            painted += 1;
            let before = std::mem::replace(&mut node.painted, now);
            if !full {
                self.damage.extend(before);
                self.damage.extend(now);
            }
        }
        if full {
            self.removed.clear();
            self.damage.push(self.pixels.bounds());
        } else {
            // What widgets taken away covered is drawn again, from what is
            // under it.
            self.damage.append(&mut self.removed);
            tidy(&mut self.damage, self.pixels.bounds(), MAX_DAMAGE);
        }
        painted
    }

    /// Draws the damage into the back buffer.
    fn render(&mut self) {
        self.composite.clear();
        // The background covers the whole buffer, whatever rounding made
        // its size.
        let scale = f64::from(self.scale);
        let background = Rect::new(
            0.0,
            0.0,
            (f64::from(self.pixels.width()) / scale) as f32,
            (f64::from(self.pixels.height()) / scale) as f32,
        );
        self.composite
            .fill(background, self.theme.palette.background);
        for node in &self.tree.nodes {
            self.composite.append(&node.scene);
        }
        for &rect in &self.damage {
            self.pixels.fill_rect(rect, Color::TRANSPARENT);
        }
        self.renderer
            .render_damage(&self.composite, &mut self.pixels, self.scale, &self.damage);
    }
}

impl fmt::Debug for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Root")
            .field("widgets", &self.tree.nodes.len())
            .field("size", &self.size)
            .field("scale", &self.scale)
            .field("focus", &self.focus)
            .field("pointer_holder", &self.capture)
            .finish_non_exhaustive()
    }
}
