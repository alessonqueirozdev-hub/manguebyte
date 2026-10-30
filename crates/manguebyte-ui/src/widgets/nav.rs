//! Navigation: a tab bar or a sidebar that chooses which page fills a
//! content area.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use manguebyte_gfx::{Align, Point, Rect, RoundedRect, Scene, Size, TextStyle};

use super::icon::Icon;
use super::{
    Cached, device_px, draw_text, focus_ring, hline, line, line_height, snap_px, text_width, vline,
};
use crate::access::{AccessItem, AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::layout::Style;
use crate::motion::{Animated, Lerp};
use crate::source::{State, TextSource};
use crate::theme::Theme;
use crate::view::{IntoView, View};
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, Shown, UpdateCx, Widget};

/// The height of a tab bar.
const BAR_HEIGHT: f32 = 40.0;

/// Space between a tab's edges and its text.
const TAB_PAD: f32 = 14.0;

/// Space between a tab bar's or a sidebar's edge and its first entry.
const EDGE: f32 = 8.0;

/// Space between neighbouring entries.
const ENTRY_GAP: f32 = 2.0;

/// Space before a section that is not the first.
const SECTION_GAP: f32 = 12.0;

/// The height of a sidebar's page and of its section headers.
const ROW_HEIGHT: f32 = 32.0;
const HEADER_HEIGHT: f32 = 26.0;

/// Where a sidebar row's icon or text starts, clear of the indicator's
/// bar.
const ROW_INSET: f32 = 15.0;

/// The narrowest a sidebar is.
const SIDEBAR_WIDTH: f32 = 184.0;

/// The side of an icon, and the space after it.
const ICON: f32 = 16.0;
const ICON_GAP: f32 = 8.0;

/// The thickness of the selection indicator: a tab's underline, a
/// sidebar page's bar.
const INDICATOR: f32 = 2.0;

/// Where the keys of section headers start among the bar's items, after
/// those of the pages.
const SECTION_KEY: usize = 1 << 24;

/// One entry of a bar: a page, or a section header.
struct Entry {
    text: TextSource,
    icon: Option<Icon>,
    /// The page it chooses; `None` for a section header.
    page: Option<usize>,
}

/// What the bar, the pages and their panels share: the pages' names, as
/// last read, so that a panel is named as its tab is.
struct Shared {
    titles: RefCell<Vec<String>>,
    /// Counts up whenever a title changes.
    version: Cell<u64>,
}

impl Shared {
    fn title(&self, page: usize) -> String {
        self.titles.borrow().get(page).cloned().unwrap_or_default()
    }
}

/// A set of pages and the navigation that chooses one: a tab bar
/// ([`tabs`]) or a sidebar ([`sidebar`]). Pages are added with
/// [`page`](Pages::page), each with an [`icon`](Pages::icon) if wanted,
/// and grouped under [`section`](Pages::section) headers.
///
/// The page shown is the one a [`State`] says, from 0: a click on a tab,
/// the keys or assistive technology set it, and the application can set
/// it too, to change page itself. A number past the last page shows the
/// last.
///
/// # Hidden pages
///
/// Every page is built once, with the window, and kept: switching pages
/// never builds anything again, and a hidden page's widgets keep their
/// state (a knob's gesture ends, but a text typed, a list's selection and
/// scrolling, a waveform's zoom all stay). A hidden page is not polled,
/// laid out, drawn, hit, focused or described to assistive technology,
/// and a widget in it that animates asks for no frames: an idle hidden
/// page costs nothing. A switch lays the interface out again and draws the
/// bar and the content area, nothing else. A widget in the page being
/// hidden loses the keyboard focus, which then goes nowhere until Tab or
/// a click gives it again.
///
/// # Keys and assistive technology
///
/// The bar is one stop of Tab, a [`TabList`](Role::TabList) whose tabs are
/// items of role [`Tab`](Role::Tab), each saying whether it is selected,
/// and each page is inside a [`TabPanel`](Role::TabPanel) named as its
/// tab. With the bar focused, the arrows along it (Left and Right for tabs,
/// Up and Down for a sidebar) move to the previous or next page, round at
/// the ends, and Home and End to the first and the last; Control+Tab and
/// Control+Shift+Tab do the same from anywhere inside the pages. Section
/// headers are items of role [`Label`](Role::Label) that the keys pass
/// over.
///
/// The selection indicator slides from the old tab to the new one over the
/// theme's base duration, and jumps with motion turned off.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, State, label, tabs};
///
/// let page = State::new(0_usize);
/// let view = tabs("Sections", page.clone())
///     .page("Synth", label("Oscillators"))
///     .page("Effects", label("Reverb"))
///     .page("Presets", label("Init"));
/// let mut ui = Harness::new(view, Size::new(400.0, 200.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Oscillators").is_some());
/// assert!(ui.find(Role::Label, "Reverb").is_none(), "hidden");
///
/// // A click on a tab, or the arrows.
/// ui.click(ui.get(Role::Tab, "Effects").center());
/// ui.frame();
/// assert_eq!(page.get(), 1);
/// assert!(ui.find(Role::Label, "Reverb").is_some());
/// ui.key(Key::Right);
/// ui.frame();
/// assert_eq!(ui.get(Role::Tab, "Presets").node.state.selected, Some(true));
///
/// // The application changes page itself.
/// page.set(0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Oscillators").is_some());
/// ```
pub struct Pages {
    name: TextSource,
    selected: State<usize>,
    vertical: bool,
    entries: Vec<Entry>,
    views: Vec<View>,
}

/// Pages chosen by a tab bar along their top, named `name` for assistive
/// technology, showing the page `selected` says (see [`Pages`]).
pub fn tabs(name: impl Into<TextSource>, selected: State<usize>) -> Pages {
    Pages {
        name: name.into(),
        selected,
        vertical: false,
        entries: Vec::new(),
        views: Vec::new(),
    }
}

/// Pages chosen by a sidebar down their left, named `name` for assistive
/// technology, showing the page `selected` says (see [`Pages`]).
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, State, label, sidebar};
///
/// let division = State::new(0_usize);
/// let view = sidebar("Divisions", division.clone())
///     .section("Manuals")
///     .page("Great", label("Great stops"))
///     .page("Swell", label("Swell stops"))
///     .section("Pedal")
///     .page("Pedal", label("Pedal stops"));
/// let mut ui = Harness::new(view, Size::new(480.0, 260.0), 1.0);
/// ui.frame();
/// assert!(ui.find(Role::Label, "Manuals").is_some(), "a section header");
/// let list = ui.get(Role::TabList, "Divisions");
/// ui.act(&list, manguebyte_ui::Action::Focus);
/// // Down twice passes over the header.
/// ui.key(Key::Down);
/// ui.key(Key::Down);
/// ui.frame();
/// assert_eq!(division.get(), 2);
/// assert!(ui.find(Role::Label, "Pedal stops").is_some());
/// ```
pub fn sidebar(name: impl Into<TextSource>, selected: State<usize>) -> Pages {
    Pages {
        vertical: true,
        ..tabs(name, selected)
    }
}

impl Pages {
    /// The same pages with one more, named `title` (fixed, or a
    /// [`TextSource`] that follows the language), showing `view`.
    #[must_use]
    pub fn page(mut self, title: impl Into<TextSource>, view: impl IntoView) -> Pages {
        self.entries.push(Entry {
            text: title.into(),
            icon: None,
            page: Some(self.views.len()),
        });
        self.views.push(view.into_view());
        self
    }

    /// The same pages with `icon` before the name of the last page added.
    /// Nothing if no page has been added yet.
    #[must_use]
    pub fn icon(mut self, icon: Icon) -> Pages {
        if let Some(entry) = self.entries.iter_mut().rev().find(|e| e.page.is_some()) {
            entry.icon = Some(icon);
        }
        self
    }

    /// The same pages with a section header, `title`, before the pages
    /// added after it.
    #[must_use]
    pub fn section(mut self, title: impl Into<TextSource>) -> Pages {
        self.entries.push(Entry {
            text: title.into(),
            icon: None,
            page: None,
        });
        self
    }
}

impl IntoView for Pages {
    fn into_view(self) -> View {
        let Pages {
            name,
            selected,
            vertical,
            entries,
            views,
        } = self;
        let count = views.len();
        let shared = Rc::new(Shared {
            titles: RefCell::new(
                entries
                    .iter()
                    .filter(|entry| entry.page.is_some())
                    .map(|entry| entry.text.as_str().to_owned())
                    .collect(),
            ),
            version: Cell::new(0),
        });
        let pages: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.page.is_some())
            .map(|(at, _)| at)
            .collect();
        let entry_count = entries.len();
        let bar = TabBar {
            name,
            entries,
            pages,
            selected: selected.clone(),
            seen: None,
            current: 0,
            vertical,
            shared: Rc::clone(&shared),
            widths: vec![0.0; entry_count],
            hover: None,
            fades: vec![Animated::new(0.0); entry_count],
            pressed: None,
            progress: Animated::new(1.0),
            from: None,
            drawn: None,
            layouts: (0..entry_count).map(|_| Cached::new()).collect(),
        };
        let panels: Vec<View> = views
            .into_iter()
            .enumerate()
            .map(|(index, view)| {
                View::with_children(
                    TabPanel {
                        index,
                        shared: Rc::clone(&shared),
                        seen: 0,
                    },
                    Style::stack(),
                    (view,),
                )
            })
            .collect();
        let content = View::with_children(
            PageStack {
                selected: selected.clone(),
                seen: None,
                count,
            },
            Style::stack().with_flex(1.0),
            panels,
        );
        let style = if vertical {
            Style::row()
        } else {
            Style::column()
        };
        View::with_children(
            Navigation { selected, count },
            style,
            (View::new(bar), content),
        )
    }
}

/// `value` within the pages there are: the last for a number past it.
fn clamp_page(value: usize, count: usize) -> usize {
    value.min(count.saturating_sub(1))
}

/// The outermost widget of a set of pages: it takes Control+Tab from
/// anywhere inside them.
struct Navigation {
    selected: State<usize>,
    count: usize,
}

impl Widget for Navigation {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn event(&mut self, _cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::Key(KeyEvent {
                key: Key::Tab,
                modifiers,
                pressed,
            }) if modifiers.control && !modifiers.alt && !modifiers.meta => {
                if pressed && self.count > 0 {
                    let at = clamp_page(self.selected.get(), self.count);
                    let next = if modifiers.shift {
                        (at + self.count - 1) % self.count
                    } else {
                        (at + 1) % self.count
                    };
                    self.selected.set(next);
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

/// The content area: shows the page chosen, and only that one.
struct PageStack {
    selected: State<usize>,
    seen: Option<u64>,
    count: usize,
}

impl Widget for PageStack {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.selected.version();
        if self.seen == Some(version) {
            return;
        }
        self.seen = Some(version);
        cx.show(Shown::Only(clamp_page(self.selected.get(), self.count)));
    }
}

/// What one page is in: a panel named as its tab.
struct TabPanel {
    index: usize,
    shared: Rc<Shared>,
    seen: u64,
}

impl Widget for TabPanel {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::TabPanel, self.shared.title(self.index))
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::default()
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.shared.version.get();
        if version != self.seen {
            self.seen = version;
            cx.request_access_update();
        }
    }
}

/// The tab bar, or the sidebar: the entries, the selection and its
/// indicator.
struct TabBar {
    name: TextSource,
    entries: Vec<Entry>,
    /// The entry of each page.
    pages: Vec<usize>,
    selected: State<usize>,
    seen: Option<u64>,
    /// The page shown, as last read.
    current: usize,
    vertical: bool,
    shared: Rc<Shared>,
    /// Each entry's text width, as last measured.
    widths: Vec<f32>,
    /// The entry under the pointer.
    hover: Option<usize>,
    /// How far each entry's hover has come in.
    fades: Vec<Animated<f32>>,
    /// The page a press went down on.
    pressed: Option<usize>,
    /// How far the indicator has come from `from` to the page shown.
    progress: Animated<f32>,
    /// Where the indicator started its move, relative to the bar.
    from: Option<Rect>,
    /// Where the indicator was last drawn, relative to the bar.
    drawn: Option<Rect>,
    layouts: Vec<Cached<TextStyle>>,
}

impl TabBar {
    fn style(&self, theme: &Theme, entry: usize) -> TextStyle {
        if self.entries[entry].page.is_some() {
            theme.typography.label.with_align(Align::Start)
        } else {
            theme
                .typography
                .caption
                .with_color(theme.palette.text_muted)
                .with_align(Align::Start)
        }
    }

    /// Where each entry is, in logical pixels, inside `rect`.
    fn slots(&self, rect: Rect) -> Vec<Rect> {
        let mut slots = Vec::with_capacity(self.entries.len());
        if self.vertical {
            let mut y = rect.y + EDGE;
            let width = (rect.width - 2.0 * EDGE).max(0.0);
            for (at, entry) in self.entries.iter().enumerate() {
                let height = if entry.page.is_some() {
                    ROW_HEIGHT
                } else {
                    if at > 0 {
                        y += SECTION_GAP - ENTRY_GAP;
                    }
                    HEADER_HEIGHT
                };
                slots.push(Rect::new(rect.x + EDGE, y, width, height));
                y += height + ENTRY_GAP;
            }
        } else {
            let mut x = rect.x + EDGE;
            for (at, entry) in self.entries.iter().enumerate() {
                if entry.page.is_none() && at > 0 {
                    x += SECTION_GAP - ENTRY_GAP;
                }
                let width = self.natural_width(at);
                slots.push(Rect::new(x, rect.y, width, rect.height));
                x += width + ENTRY_GAP;
            }
        }
        slots
    }

    /// How wide entry `at` wants to be.
    fn natural_width(&self, at: usize) -> f32 {
        let entry = &self.entries[at];
        let icon = if entry.icon.is_some() {
            ICON + ICON_GAP
        } else {
            0.0
        };
        match (entry.page.is_some(), self.vertical) {
            (true, false) => 2.0 * TAB_PAD + icon + self.widths[at],
            (true, true) => ROW_INSET + 10.0 + icon + self.widths[at],
            (false, false) => self.widths[at] + 4.0,
            (false, true) => ROW_INSET + 10.0 + self.widths[at],
        }
    }

    /// The face of a page's entry: where its hover and selection are
    /// drawn, inside its slot.
    fn face(&self, slot: Rect) -> Rect {
        if self.vertical {
            slot
        } else {
            Rect::new(
                slot.x,
                slot.y + 6.0,
                slot.width,
                (slot.height - 12.0).max(0.0),
            )
        }
    }

    /// The indicator of the page whose slot is `slot`, in logical pixels.
    fn indicator_of(&self, slot: Rect, bar: Rect, scale: f32) -> Rect {
        if self.vertical {
            slot
        } else {
            let thick = device_px((INDICATOR * scale).round().max(1.0), scale);
            Rect::new(
                slot.x + TAB_PAD - 4.0,
                bar.bottom() - thick,
                (slot.width - 2.0 * TAB_PAD + 8.0).max(0.0),
                thick,
            )
        }
    }

    /// The entry at `at`, if it is a page: its page.
    fn page_at(&self, rect: Rect, at: Point) -> Option<usize> {
        self.slots(rect)
            .iter()
            .position(|slot| slot.contains(at))
            .and_then(|entry| self.entries[entry].page)
    }

    /// Chooses page `page`.
    fn choose(&mut self, cx: &mut EventCx<'_>, page: usize) {
        if page < self.pages.len() && page != clamp_page(self.selected.get(), self.pages.len()) {
            self.selected.set(page);
        }
        cx.request_paint();
    }

    fn count(&self) -> usize {
        self.pages.len()
    }
}

impl Widget for TabBar {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let value = self.shared.title(self.current);
        AccessNode::new(Role::TabList, self.name.as_str())
            .with_value(value)
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let slots = self.slots(cx.rect());
        let mut section = 0;
        for (entry, slot) in self.entries.iter().zip(slots) {
            match entry.page {
                Some(page) => {
                    let mut node = AccessNode::new(Role::Tab, entry.text.as_str())
                        .with_value(format!("{} of {}", page + 1, self.count()))
                        .with_selected(page == self.current)
                        .with_actions(Actions::PRESS);
                    node.state.focused = cx.is_focused() && page == self.current;
                    items.push(AccessItem::new(page, node, slot));
                }
                None => {
                    items.push(AccessItem::new(
                        SECTION_KEY + section,
                        AccessNode::new(Role::Label, entry.text.as_str()),
                        slot,
                    ));
                    section += 1;
                }
            }
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        if key >= self.count() {
            return false;
        }
        match action {
            Action::Press | Action::Focus => {
                self.choose(cx, key);
                true
            }
            _ => false,
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        for at in 0..self.entries.len() {
            let style = self.style(theme, at);
            self.widths[at] = text_width(self.entries[at].text.as_str(), &style);
        }
        let slots = self.slots(Rect::new(0.0, 0.0, 0.0, 0.0));
        if self.vertical {
            let width = (0..self.entries.len())
                .map(|at| self.natural_width(at))
                .fold(SIDEBAR_WIDTH - 2.0 * EDGE, f32::max);
            let height = slots.last().map_or(0.0, |slot| slot.bottom()) + EDGE;
            Size::new(width + 2.0 * EDGE, height)
        } else {
            let width = slots.last().map_or(0.0, |slot| slot.right()) + EDGE;
            Size::new(width, BAR_HEIGHT)
        }
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.name.poll() {
            cx.request_access_update();
        }
        let mut renamed = false;
        for (at, entry) in self.entries.iter_mut().enumerate() {
            if entry.text.poll() {
                renamed = true;
                self.layouts[at].clear();
                if let Some(page) = entry.page {
                    if let Some(title) = self.shared.titles.borrow_mut().get_mut(page) {
                        title.clear();
                        title.push_str(entry.text.as_str());
                    }
                }
            }
        }
        if renamed {
            self.shared
                .version
                .set(self.shared.version.get().wrapping_add(1));
            cx.request_layout();
            cx.request_access_update();
        }
        let version = self.selected.version();
        if self.seen != Some(version) {
            let first = self.seen.is_none();
            self.seen = Some(version);
            let page = clamp_page(self.selected.get(), self.count());
            if page != self.current || first {
                if !first {
                    // The indicator slides from where it was drawn.
                    self.from = self.drawn;
                    self.progress.jump(0.0);
                    let (now, base) = (cx.now(), cx.motion().base());
                    self.progress.transition(1.0, now, base);
                }
                self.current = page;
                cx.request_paint();
                cx.request_access_update();
            }
        }
        let (now, fast) = (cx.now(), cx.motion().fast());
        for (at, fade) in self.fades.iter_mut().enumerate() {
            let target = if self.hover == Some(at) { 1.0 } else { 0.0 };
            fade.transition(target, now, fast);
            cx.animate(fade);
        }
        cx.animate(&mut self.progress);
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let rect = cx.rect();
        let scale = cx.scale();
        if self.vertical {
            vline(
                scene,
                rect.right() - device_px(1.0, scale),
                (rect.y, rect.bottom()),
                1.0,
                scale,
                palette.hairline,
            );
        } else {
            hline(
                scene,
                rect.bottom() - device_px(1.0, scale),
                (rect.x, rect.right()),
                1.0,
                scale,
                palette.hairline,
            );
        }
        let slots = self.slots(rect);
        let radius = theme.corners.md;
        // The indicator, under the texts in a sidebar.
        let target = self
            .pages
            .get(self.current)
            .and_then(|&entry| slots.get(entry))
            .map(|&slot| self.indicator_of(slot, rect, scale));
        let origin = Point::new(rect.x, rect.y);
        let relative = |r: Rect| Rect::new(r.x - origin.x, r.y - origin.y, r.width, r.height);
        let absolute = |r: Rect| Rect::new(r.x + origin.x, r.y + origin.y, r.width, r.height);
        let indicator = match (target, self.from) {
            (Some(target), Some(from)) if self.progress.value() < 1.0 => {
                Some(absolute(from.lerp(relative(target), self.progress.value())))
            }
            (target, _) => target,
        };
        self.drawn = indicator.map(relative);
        if self.vertical {
            if let Some(face) = indicator {
                let shape = RoundedRect::new(face, radius.min(face.height * 0.5));
                scene.fill(shape, palette.accent_soft);
                // Inside the face, clear of the focus ring.
                let bar = Rect::new(
                    snap_px(face.x + 5.0, scale),
                    face.y + 9.0,
                    device_px((3.0 * scale).round(), scale),
                    (face.height - 18.0).max(0.0),
                );
                scene.fill(RoundedRect::new(bar, bar.width * 0.5), palette.accent);
            }
        }
        for (at, slot) in slots.iter().copied().enumerate() {
            let entry = &self.entries[at];
            let fade = self.fades[at].value();
            let selected = entry.page == Some(self.current);
            let base = self.style(theme, at);
            let ink = match entry.page {
                Some(_) if selected => palette.text,
                Some(_) => palette.text_secondary.lerp(palette.text, fade),
                None => base.color,
            };
            if entry.page.is_some() && fade > 0.0 && !selected {
                let face = self.face(slot);
                scene.fill(
                    RoundedRect::new(face, radius.min(face.height * 0.5)),
                    palette.track.with_opacity(fade),
                );
            }
            let style = base.with_color(ink);
            let mut x = slot.x
                + if self.vertical {
                    ROW_INSET
                } else if entry.page.is_some() {
                    TAB_PAD
                } else {
                    2.0
                };
            let middle = if entry.page.is_none() && self.vertical {
                // A header sits low in its slot, over the pages it heads.
                slot.bottom() - line_height(&style) * 0.5 - 5.0
            } else {
                slot.y + slot.height * 0.5
            };
            if let Some(icon) = &entry.icon {
                let square = Rect::new(x, middle - ICON * 0.5, ICON, ICON);
                icon.draw(scene, square, ink);
                x += ICON + ICON_GAP;
            }
            let room = (slot.right() - x).max(0.0);
            let text = &entry.text;
            let layout = self.layouts[at].get(room, style, || line(text.as_str(), &style, room));
            draw_text(
                scene,
                layout,
                Point::new(x, middle - line_height(&style) * 0.5),
            );
        }
        if !self.vertical {
            if let Some(line) = indicator {
                scene.fill(RoundedRect::new(line, line.height * 0.5), palette.accent);
            }
        }
        // The focus ring, around the page shown.
        if let Some(&slot) = self.pages.get(self.current).and_then(|&e| slots.get(e)) {
            let face = self.face(slot).outset(-1.0);
            focus_ring(
                scene,
                RoundedRect::new(face, (radius - 1.0).min(face.height * 0.5).max(0.0)),
                theme,
                cx.focus_ring(),
            );
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let count = self.count();
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                ..
            } => {
                self.pressed = self.page_at(cx.rect(), at);
                if self.pressed.is_some() {
                    cx.capture_pointer();
                    cx.request_focus();
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            Event::PointerUp {
                at,
                button: Button::Primary,
                ..
            } => {
                let pressed = self.pressed.take();
                cx.release_pointer();
                match pressed {
                    Some(page) if self.page_at(cx.rect(), at) == Some(page) => {
                        self.choose(cx, page);
                        Handled::Yes
                    }
                    Some(_) => Handled::Yes,
                    None => Handled::No,
                }
            }
            Event::PointerCancel => {
                self.pressed = None;
                Handled::No
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed: true,
            }) if count > 0 && !modifiers.control && !modifiers.alt && !modifiers.meta => {
                // From the page chosen last, which a frame may not have
                // shown yet.
                let at = clamp_page(self.selected.get(), count);
                let (back, on) = if self.vertical {
                    (Key::Up, Key::Down)
                } else {
                    (Key::Left, Key::Right)
                };
                let to = match key {
                    k if k == back => (at + count - 1) % count,
                    k if k == on => (at + 1) % count,
                    Key::Home => 0,
                    Key::End => count - 1,
                    Key::Enter | Key::Character(' ') => at,
                    _ => return Handled::No,
                };
                self.choose(cx, to);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn hover_changed(&mut self, cx: &mut EventCx<'_>, hovered: bool) {
        if !hovered && self.hover.take().is_some() {
            cx.request_paint();
        }
    }

    fn hover_moved(&mut self, cx: &mut EventCx<'_>, at: Point) {
        let slots = self.slots(cx.rect());
        let over = slots
            .iter()
            .position(|slot| slot.contains(at))
            .filter(|&entry| self.entries[entry].page.is_some());
        if over != self.hover {
            self.hover = over;
            cx.request_paint();
        }
    }
}
