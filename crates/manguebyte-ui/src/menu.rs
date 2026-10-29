//! Popup menus: what is in one, and the widgets the root shows it with.
//!
//! A widget opens a [`Menu`] with [`EventCx::open_menu`] (below or above
//! itself, as there is room) or [`EventCx::open_menu_at`] (at a point, as a
//! context menu). The [`Root`](crate::Root) shows it over everything else,
//! drawn by the interface's own renderer inside the editor's window: no
//! native menu, no window of its own. While it is open the menu has the
//! pointer and the keyboard; when it closes, the widget that opened it
//! hears what was chosen as [`Event::Menu`](crate::Event::Menu).
//!
//! - **Placement**: below the widget if the menu fits there, above it if it
//!   fits there instead, and otherwise against the top of the interface; at
//!   least as wide as the widget; moved left to stay inside the interface.
//!   A submenu opens beside its item, on the right, or on the left where
//!   there is no room on the right.
//! - **The pointer**: moving over an item highlights it and opens its
//!   submenu; a click on an item, or a release over one after the pointer
//!   has moved since the menu opened (press, drag, release), chooses it. A
//!   press outside every open menu closes them all, and does nothing else.
//! - **Keys**: Up and Down move the highlight over the items that can be
//!   chosen, round at the ends; Home and End go to the first and the last;
//!   Enter and Space choose, or open a submenu at its first item; Right
//!   opens a submenu, Left and Escape close one; Escape in the outermost
//!   menu, and Tab, close everything. A typed character goes to the next
//!   item whose name starts with it. Every key is the menu's while it is
//!   open, so none reaches the host.
//! - **The keyboard focus** moves to the highlighted item, so assistive
//!   technology follows it: the checked item of a menu, or else its first
//!   item, when it opens. When the menu closes the focus goes back to the
//!   widget that had it. The window losing the keyboard closes the menu.
//! - **Assistive technology** sees a [`Menu`](crate::Role::Menu) named by
//!   the menu's title, holding [`MenuItem`](crate::Role::MenuItem)s and
//!   [`Separator`](crate::Role::Separator)s; an item says whether it is
//!   checked, whether its submenu is open, and whether it is disabled, and
//!   [`Action::Press`](crate::Action::Press) chooses it.
//!
//! Menus do not scroll: one taller than the interface runs past its bottom
//! edge.
//!
//! [`EventCx::open_menu`]: crate::EventCx::open_menu
//! [`EventCx::open_menu_at`]: crate::EventCx::open_menu_at

use std::cell::Cell;
use std::fmt;
use std::rc::Rc;

use manguebyte_gfx::{Align, Path, Point, Rect, RoundedRect, Scene, Size, Stroke, TextStyle};

use crate::access::{AccessNode, Actions, Role};
use crate::layout::{Insets, Style};
use crate::view::View;
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};
use crate::widgets::{Cached, floating, floating_overflow, line, line_height, text_width};

/// What a popup menu holds: a title for assistive technology, and items.
///
/// ```
/// use manguebyte_ui::{Menu, MenuItem};
///
/// let menu = Menu::new("Preset")
///     .item(MenuItem::new(1, "Save"))
///     .item(MenuItem::new(2, "Save as…").disabled())
///     .item(MenuItem::separator())
///     .item(MenuItem::submenu(
///         "Factory",
///         Menu::new("Factory")
///             .item(MenuItem::new(10, "Init"))
///             .item(MenuItem::new(11, "Warm pad").checked(true)),
///     ));
/// assert_eq!(menu.items().len(), 4);
/// assert_eq!(menu.items()[3].menu().map(|m| m.items().len()), Some(2));
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Menu {
    title: String,
    items: Vec<MenuItem>,
}

impl Menu {
    /// An empty menu called `title` by assistive technology and tests.
    pub fn new(title: impl Into<String>) -> Menu {
        Menu {
            title: title.into(),
            items: Vec::new(),
        }
    }

    /// The same menu with `item` after its others.
    #[must_use]
    pub fn item(mut self, item: MenuItem) -> Menu {
        self.items.push(item);
        self
    }

    /// Puts `item` after the others.
    pub fn push(&mut self, item: MenuItem) {
        self.items.push(item);
    }

    /// The title.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// The items, in order.
    pub fn items(&self) -> &[MenuItem] {
        &self.items
    }
}

/// What kind of item.
#[derive(Debug, Clone, PartialEq)]
enum Kind {
    /// Chosen, with this id.
    Choice(usize),
    /// Opens a menu of its own.
    Submenu(Menu),
    /// A line between groups.
    Separator,
}

/// One item of a [`Menu`]: something to choose, a submenu, or a line
/// between groups.
#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    kind: Kind,
    label: String,
    enabled: bool,
    checked: Option<bool>,
}

impl MenuItem {
    /// An item named `label` that, chosen, tells the widget that opened
    /// the menu `id` ([`MenuEvent::Chosen`](crate::MenuEvent::Chosen)).
    pub fn new(id: usize, label: impl Into<String>) -> MenuItem {
        MenuItem {
            kind: Kind::Choice(id),
            label: label.into(),
            enabled: true,
            checked: None,
        }
    }

    /// An item named `label` that opens `menu` beside it.
    pub fn submenu(label: impl Into<String>, menu: Menu) -> MenuItem {
        MenuItem {
            kind: Kind::Submenu(menu),
            label: label.into(),
            enabled: true,
            checked: None,
        }
    }

    /// A line between groups of items.
    pub fn separator() -> MenuItem {
        MenuItem {
            kind: Kind::Separator,
            label: String::new(),
            enabled: false,
            checked: None,
        }
    }

    /// The same item with a check mark if `on`, and said to be on or off.
    #[must_use]
    pub fn checked(mut self, on: bool) -> MenuItem {
        self.checked = Some(on);
        self
    }

    /// The same item, shown but not to be chosen.
    #[must_use]
    pub fn disabled(mut self) -> MenuItem {
        self.enabled = false;
        self
    }

    /// Its name.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// The id it tells when chosen, if it is something to choose.
    pub fn id(&self) -> Option<usize> {
        match self.kind {
            Kind::Choice(id) => Some(id),
            Kind::Submenu(_) | Kind::Separator => None,
        }
    }

    /// The menu it opens, if it is a submenu.
    pub fn menu(&self) -> Option<&Menu> {
        match &self.kind {
            Kind::Submenu(menu) => Some(menu),
            Kind::Choice(_) | Kind::Separator => None,
        }
    }

    /// Whether it is a line between groups.
    pub fn is_separator(&self) -> bool {
        self.kind == Kind::Separator
    }

    /// Whether it can be chosen or opened: not disabled, not a separator.
    pub fn is_enabled(&self) -> bool {
        self.enabled && !self.is_separator()
    }

    /// Whether it has a check mark: `None` for an item that is neither on
    /// nor off.
    pub fn is_checked(&self) -> Option<bool> {
        self.checked
    }
}

// The widgets a menu is shown with. The root builds them when it opens a
// menu and handles every input to them itself.

/// Space inside a menu's edge, around its items.
pub(crate) const PANEL_PAD: f32 = 4.0;

/// Room on the left of an item for its check mark, and on the right for a
/// submenu's arrow.
const MARK: f32 = 18.0;

/// Space above and below an item's name.
const ITEM_PAD_Y: f32 = 4.0;

/// The height of a separator.
const SEPARATOR: f32 = 9.0;

/// The menu itself: a panel in the theme's surface colour with a border.
pub(crate) struct MenuPanel {
    title: String,
}

impl Widget for MenuPanel {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Menu, self.title.as_str())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        floating_overflow(cx.theme())
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        floating(scene, cx.rect(), theme.corners.lg, theme, cx.scale());
    }
}

/// One item of an open menu. It is highlighted while it has the keyboard
/// focus, and while its submenu is open.
pub(crate) struct MenuEntry {
    label: String,
    enabled: bool,
    checked: Option<bool>,
    /// Set while its submenu is open; `None` for an item with none.
    expanded: Option<Rc<Cell<bool>>>,
    name: Cached<TextStyle>,
}

fn item_style(cx_theme: &crate::Theme, enabled: bool) -> TextStyle {
    let palette = &cx_theme.palette;
    cx_theme
        .typography
        .label
        .with_color(if enabled {
            palette.text
        } else {
            palette.text_secondary
        })
        .with_align(Align::Start)
}

impl Widget for MenuEntry {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let mut node = AccessNode::new(Role::MenuItem, self.label.as_str());
        if let Some(checked) = self.checked {
            node = node.with_checked(checked);
        }
        if let Some(expanded) = &self.expanded {
            node = node.with_expanded(expanded.get());
        }
        if self.enabled {
            node.with_actions(Actions::PRESS)
        } else {
            node.disabled()
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = item_style(cx.theme(), self.enabled);
        Size::new(
            MARK + text_width(&self.label, &style) + MARK,
            line_height(&style) + 2.0 * ITEM_PAD_Y,
        )
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let open = self.expanded.as_ref().is_some_and(|open| open.get());
        if self.enabled && (cx.is_focused() || open) {
            scene.fill(
                RoundedRect::new(rect, theme.corners.sm),
                crate::access::menu_highlight(theme),
            );
        }
        let style = item_style(theme, self.enabled);
        let ink = style.color;
        let height = line_height(&style);
        let middle = rect.y + rect.height * 0.5;
        let width = (rect.width - 2.0 * MARK).max(0.0);
        let label = &self.label;
        let name = self.name.get(width, style, || line(label, &style, width));
        scene.text(name, Point::new(rect.x + MARK, middle - height * 0.5));
        if self.checked == Some(true) {
            // A tick, in the left margin.
            let x = rect.x + MARK * 0.5;
            let mut tick = Path::new();
            tick.move_to(Point::new(x - 4.0, middle))
                .line_to(Point::new(x - 1.0, middle + 3.0))
                .line_to(Point::new(x + 4.0, middle - 3.5));
            scene.stroke(&tick, Stroke::round(1.5), ink);
        }
        if self.expanded.is_some() {
            // An arrow, in the right margin.
            let x = rect.right() - MARK * 0.5;
            let mut arrow = Path::new();
            arrow
                .move_to(Point::new(x - 2.0, middle - 4.0))
                .line_to(Point::new(x + 2.0, middle))
                .line_to(Point::new(x - 2.0, middle + 4.0));
            scene.stroke(&arrow, Stroke::round(1.5), ink);
        }
    }
}

/// A line between groups of items.
pub(crate) struct MenuSeparator;

impl Widget for MenuSeparator {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Separator, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(1.0, SEPARATOR)
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let y = (rect.y + rect.height * 0.5).floor();
        scene.fill(
            Rect::new(rect.x + 4.0, y, (rect.width - 8.0).max(0.0), 1.0),
            cx.theme().palette.hairline_strong,
        );
    }
}

/// The view an open menu is shown with, and, for each item, whether its
/// submenu is open (for items with one).
pub(crate) fn menu_view(menu: &Menu) -> (View, Vec<Option<Rc<Cell<bool>>>>) {
    let mut expanded = Vec::with_capacity(menu.items.len());
    let children: Vec<View> = menu
        .items
        .iter()
        .map(|item| match &item.kind {
            Kind::Separator => {
                expanded.push(None);
                View::new(MenuSeparator)
            }
            kind => {
                let open = matches!(kind, Kind::Submenu(_)).then(|| Rc::new(Cell::new(false)));
                expanded.push(open.clone());
                View::new(MenuEntry {
                    label: item.label.clone(),
                    enabled: item.enabled,
                    checked: item.checked,
                    expanded: open,
                    name: Cached::new(),
                })
            }
        })
        .collect();
    let style = Style::column().with_padding(Insets::all(PANEL_PAD));
    let panel = MenuPanel {
        title: menu.title.clone(),
    };
    (View::with_children(panel, style, children), expanded)
}

/// Where a menu of `size` goes in an interface of `area`: below `anchor`
/// if it fits, above if it fits there instead, and otherwise against the
/// top; at least as wide as `anchor`; moved left, then right, to stay
/// inside.
pub(crate) fn below_or_above(size: Size, anchor: Rect, area: Size) -> Rect {
    let width = size.width.max(anchor.width);
    let height = size.height;
    let below = anchor.bottom();
    let above = anchor.y - height;
    let y = if below + height <= area.height {
        below
    } else if above >= 0.0 {
        above
    } else {
        0.0
    };
    let x = anchor.x.min(area.width - width).max(0.0);
    Rect::new(x, y, width, height)
}

/// Where a submenu of `size` goes beside its item `anchor`, in a menu
/// whose panel is `parent`: on the right, or on the left where there is no
/// room on the right, its first item level with `anchor`; moved up to stay
/// inside.
pub(crate) fn beside(size: Size, anchor: Rect, parent: Rect, area: Size) -> Rect {
    let right = parent.right() - 2.0;
    let x = if right + size.width <= area.width {
        right
    } else {
        (parent.x + 2.0 - size.width).max(0.0)
    };
    let y = (anchor.y - PANEL_PAD)
        .min(area.height - size.height)
        .max(0.0);
    Rect::new(x, y, size.width, size.height)
}

impl fmt::Debug for MenuEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MenuEntry")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_menu_goes_below_then_above_then_against_the_top() {
        let area = Size::new(300.0, 200.0);
        let size = Size::new(80.0, 60.0);
        let anchor = Rect::new(10.0, 20.0, 100.0, 24.0);
        // Below, as wide as the anchor.
        assert_eq!(
            below_or_above(size, anchor, area),
            Rect::new(10.0, 44.0, 100.0, 60.0)
        );
        // No room below: above.
        let low = Rect::new(10.0, 150.0, 50.0, 24.0);
        assert_eq!(
            below_or_above(size, low, area),
            Rect::new(10.0, 90.0, 80.0, 60.0)
        );
        // Room on neither side: against the top.
        let tall = Size::new(80.0, 190.0);
        assert_eq!(below_or_above(tall, low, area).y, 0.0);
        // Too far right: moved left to stay inside.
        let right = Rect::new(280.0, 20.0, 10.0, 10.0);
        assert_eq!(below_or_above(size, right, area).x, 220.0);
    }

    #[test]
    fn a_submenu_goes_right_or_left_of_its_menu() {
        let area = Size::new(300.0, 200.0);
        let size = Size::new(80.0, 60.0);
        let parent = Rect::new(10.0, 10.0, 100.0, 120.0);
        let item = Rect::new(14.0, 40.0, 92.0, 22.0);
        assert_eq!(
            beside(size, item, parent, area),
            Rect::new(108.0, 36.0, 80.0, 60.0)
        );
        let far = Rect::new(200.0, 10.0, 90.0, 120.0);
        let item = Rect::new(204.0, 180.0, 82.0, 22.0);
        let placed = beside(size, item, far, area);
        assert_eq!(placed.x, 200.0 + 2.0 - 80.0);
        assert_eq!(placed.bottom(), 200.0, "moved up to stay inside");
    }
}
