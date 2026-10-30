//! A list of rows the application keeps, of which only those shown are
//! drawn or described.

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use manguebyte_gfx::{Align, Point, Rect, RoundedRect, Scene, Size, Stroke, TextStyle};

use crate::access::{AccessItem, AccessNode, Action, Actions, Role};
use crate::drag::{DragEvent, DragPayload};
use crate::event::{Button, Event, Handled, Key, KeyEvent, Modifiers};
use crate::motion::{Animated, Lerp};
use crate::scroll::{AUTO_STEP, Bars, Geometry, auto_scroll, follow_bars, paint_bars, snap};
use crate::source::TextSource;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};
use crate::widgets::{focus_ring, line};

/// The rows of a [`ListView`], as the application keeps them.
///
/// The list asks only for the rows it shows, so the application's rows
/// can be as many as it likes: a list of a hundred thousand samples costs
/// what the twenty on screen cost. Everything is called on the
/// interface's thread, from the list's own calls.
///
/// ```
/// use std::sync::{Arc, Mutex};
/// use manguebyte_ui::ListData;
///
/// /// Sample names, shared with the rest of the application.
/// struct Samples(Arc<Mutex<(u64, Vec<String>)>>);
///
/// impl ListData for Samples {
///     fn len(&self) -> usize {
///         self.0.lock().unwrap().1.len()
///     }
///     fn name(&self, row: usize) -> String {
///         self.0.lock().unwrap().1.get(row).cloned().unwrap_or_default()
///     }
///     fn revision(&self) -> u64 {
///         // Bumped by whoever changes the names.
///         self.0.lock().unwrap().0
///     }
/// }
/// # let _ = Samples(Arc::default());
/// ```
pub trait ListData: 'static {
    /// How many rows there are.
    fn len(&self) -> usize;

    /// Whether there are none.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The name of `row`, below [`len`](ListData::len): what assistive
    /// technology reads, what typing the first letters finds, and what
    /// [`paint_row`](ListData::paint_row) draws unless it is overridden.
    fn name(&self, row: usize) -> String;

    /// A number that changes whenever a row changes, such as a version the
    /// application bumps: the list polls it once a frame and draws its rows
    /// again when it moves. A change in the number of rows is seen without
    /// it.
    fn revision(&self) -> u64;

    /// Which of the theme's categorical [`hues`](crate::Palette::hues)
    /// to show before `row`'s name, as a small rounded swatch: the colour
    /// of the zones a sample plays, say (see
    /// [`Zone::with_hue`](crate::Zone::with_hue)). Taken modulo the number
    /// of hues. `None`, the default, for no swatch.
    fn hue(&self, row: usize) -> Option<u8> {
        let _ = row;
        None
    }

    /// Text shown at the end of `row`, small and muted, with tabular
    /// figures: the keys a sample is mapped to, a length. `None`, the
    /// default, for none.
    fn detail(&self, row: usize) -> Option<String> {
        let _ = row;
        None
    }

    /// Draws the content of `row` into `scene`, inside
    /// [`RowCx::rect`]. The list has already drawn the row's hover and
    /// selection behind it. Draws the row's [`hue`](ListData::hue),
    /// name and [`detail`](ListData::detail) unless overridden.
    fn paint_row(&self, row: usize, cx: &RowCx<'_>, scene: &mut Scene) {
        let style = cx.text_style();
        let rect = cx.rect();
        let pad = ROW_PAD.min(rect.width * 0.5);
        let mut left = rect.x + pad;
        let mut right = rect.right() - pad;
        if let Some(hue) = self.hue(row) {
            let hues = &cx.theme().palette.hues;
            let color = hues[usize::from(hue) % hues.len()].color;
            let height = (rect.height - 12.0).clamp(4.0, 22.0);
            let swatch = Rect::new(left, rect.y + (rect.height - height) * 0.5, 6.0, height);
            scene.fill(RoundedRect::new(swatch, 3.0), color);
            left += 6.0 + 9.0;
        }
        if let Some(detail) = self.detail(row) {
            let theme = cx.theme();
            let caption = theme
                .typography
                .caption
                .with_color(theme.palette.text_muted)
                .with_align(Align::End);
            let room = ((right - left) * 0.5).max(0.0);
            let text = line(&detail, &caption, room);
            let y = rect.y + (rect.height - text.size().height) * 0.5;
            scene.text(&text, Point::new(right - room, y));
            right -= text.size().width.min(room) + 8.0;
        }
        let width = (right - left).max(0.0);
        let text = line(&self.name(row), &style, width);
        let y = rect.y + (rect.height - text.size().height) * 0.5;
        scene.text(&text, Point::new(left, y));
    }

    /// What dragging `rows` carries, in tree order of the rows: `None`,
    /// the default, if they cannot be dragged.
    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let _ = rows;
        None
    }

    /// Whether `payload` can be dropped at `index`, between rows: 0 is
    /// before the first, [`len`](ListData::len) after the last. Nothing
    /// can be dropped unless overridden.
    fn accepts(&self, payload: &DragPayload, index: usize) -> bool {
        let _ = (payload, index);
        false
    }
}

/// What [`ListData::paint_row`] is given.
pub struct RowCx<'a> {
    theme: &'a Theme,
    rect: Rect,
    scale: f32,
    selected: bool,
    cursor: bool,
    hover: f32,
}

impl RowCx<'_> {
    /// The theme.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// The row's rectangle, in logical pixels.
    pub fn rect(&self) -> Rect {
        self.rect
    }

    /// Device pixels per logical pixel.
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// Whether the row is selected.
    pub fn is_selected(&self) -> bool {
        self.selected
    }

    /// Whether the row is where the keyboard is, in a list with the focus.
    pub fn is_cursor(&self) -> bool {
        self.cursor
    }

    /// How far the pointer's hover over the row has come in, from 0 to 1.
    pub fn hover(&self) -> f32 {
        self.hover
    }

    /// The style of a row's text: the theme's label type, in its secondary
    /// text colour, brightening to its text colour as the row is hovered
    /// or when it is selected. Both read on the row's selection.
    pub fn text_style(&self) -> TextStyle {
        let palette = &self.theme.palette;
        let lit = if self.selected { 1.0 } else { self.hover };
        self.theme
            .typography
            .label
            .with_color(palette.text_secondary.lerp(palette.text, lit))
            .with_align(Align::Start)
    }
}

/// Something that happened in a list, for the application to act on: it
/// takes them from the list's [`ListState`] with
/// [`take_events`](ListState::take_events).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ListEvent {
    /// The row was activated: double-clicked, or Enter pressed on it.
    Activated(usize),
    /// `payload` was dropped at `index`, between rows: 0 is before the
    /// first. What it means, such as moving the rows it names, is the
    /// application's to do.
    Dropped {
        /// Where, between rows.
        index: usize,
        /// What was dropped.
        payload: DragPayload,
    },
}

/// Rows chosen, as sorted ranges that neither touch nor overlap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Selection {
    ranges: Vec<Range<usize>>,
}

impl Selection {
    fn contains(&self, row: usize) -> bool {
        let at = self.ranges.partition_point(|range| range.end <= row);
        self.ranges.get(at).is_some_and(|range| range.start <= row)
    }

    fn count(&self) -> usize {
        self.ranges.iter().map(ExactSizeIterator::len).sum()
    }

    fn rows(&self) -> impl Iterator<Item = usize> + '_ {
        self.ranges.iter().flat_map(Clone::clone)
    }

    fn clear(&mut self) {
        self.ranges.clear();
    }

    fn add(&mut self, rows: Range<usize>) {
        if rows.is_empty() {
            return;
        }
        // Every range that touches or overlaps `rows` merges into it.
        let first = self.ranges.partition_point(|range| range.end < rows.start);
        let last = self.ranges.partition_point(|range| range.start <= rows.end);
        let mut merged = rows;
        for range in &self.ranges[first..last] {
            merged.start = merged.start.min(range.start);
            merged.end = merged.end.max(range.end);
        }
        self.ranges.splice(first..last, [merged]);
    }

    fn remove(&mut self, row: usize) {
        let at = self.ranges.partition_point(|range| range.end <= row);
        let Some(range) = self.ranges.get(at).cloned() else {
            return;
        };
        if range.start > row {
            return;
        }
        let parts = [range.start..row, row + 1..range.end];
        let kept: Vec<Range<usize>> = parts.into_iter().filter(|part| !part.is_empty()).collect();
        self.ranges.splice(at..=at, kept);
    }

    /// Forgets the rows from `len` on.
    fn truncate(&mut self, len: usize) {
        self.ranges.retain_mut(|range| {
            range.end = range.end.min(len);
            range.start < range.end
        });
    }
}

#[derive(Debug, Default)]
struct ListInner {
    selection: Selection,
    /// Where Shift extends a selection from.
    anchor: Option<usize>,
    /// The row the keyboard is on.
    cursor: Option<usize>,
    /// How far the list is scrolled, in logical pixels from the top of the
    /// first row.
    offset: f64,
    /// A row the application asked to bring into sight.
    reveal: Option<usize>,
    events: Vec<ListEvent>,
    version: u64,
}

/// What a [`ListView`] shares with the application: the selection, the row
/// the keyboard is on, how far it is scrolled, and what happened in it.
///
/// Both read and write it: the list when the person selects, scrolls,
/// activates or drops, the application when it selects rows itself or
/// moves the rows the list shows. Its [`version`](ListState::version)
/// counts every change, so either can tell that something changed without
/// reading it all. It is `Send`, so a plugin's editor spec can keep it and
/// a reopened editor finds the selection and the scroll where they were;
/// the events it holds stay until the application takes them.
///
/// ```
/// use manguebyte_ui::ListState;
///
/// let state = ListState::new();
/// state.set_selected([4, 2, 3, 9]);
/// assert_eq!(state.selected(), vec![2, 3, 4, 9]);
/// assert!(state.is_selected(3) && !state.is_selected(5));
/// let seen = state.version();
/// state.clear_selection();
/// assert!(state.version() > seen);
/// assert!(state.take_events().is_empty());
/// ```
#[derive(Clone, Default)]
pub struct ListState {
    inner: Arc<Mutex<ListInner>>,
}

impl ListState {
    /// Nothing selected, at the top.
    pub fn new() -> ListState {
        ListState::default()
    }

    fn lock(&self) -> MutexGuard<'_, ListInner> {
        // Nothing that can panic runs while the lock is held.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Changes the state and counts the change.
    fn change<R>(&self, change: impl FnOnce(&mut ListInner) -> R) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        inner.version = inner.version.wrapping_add(1);
        result
    }

    /// A number that changes with every change to the state.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// The selected rows, in order.
    pub fn selected(&self) -> Vec<usize> {
        self.lock().selection.rows().collect()
    }

    /// How many rows are selected.
    pub fn selected_count(&self) -> usize {
        self.lock().selection.count()
    }

    /// Whether `row` is selected.
    pub fn is_selected(&self, row: usize) -> bool {
        self.lock().selection.contains(row)
    }

    /// Selects `rows` and nothing else; the last of them is where the
    /// keyboard is, and where Shift extends from.
    pub fn set_selected(&self, rows: impl IntoIterator<Item = usize>) {
        self.change(|inner| {
            inner.selection.clear();
            let mut last = None;
            for row in rows {
                inner.selection.add(row..row.saturating_add(1));
                last = Some(row);
            }
            if last.is_some() {
                inner.cursor = last;
                inner.anchor = last;
            }
        });
    }

    /// Selects nothing.
    pub fn clear_selection(&self) {
        self.change(|inner| inner.selection.clear());
    }

    /// The row the keyboard is on, if any.
    pub fn cursor(&self) -> Option<usize> {
        self.lock().cursor
    }

    /// How far the list is scrolled, in logical pixels from the top of its
    /// first row.
    pub fn scroll_offset(&self) -> f64 {
        self.lock().offset
    }

    /// Scrolls the list to `offset`, kept within its rows on the next
    /// frame.
    pub fn set_scroll_offset(&self, offset: f64) {
        self.change(|inner| {
            inner.offset = if offset.is_finite() {
                offset.max(0.0)
            } else {
                0.0
            }
        });
    }

    /// Scrolls the list on the next frame so that `row` can be seen.
    pub fn scroll_to_row(&self, row: usize) {
        self.change(|inner| inner.reveal = Some(row));
    }

    /// What happened in the list since the application last asked, oldest
    /// first, forgetting it.
    pub fn take_events(&self) -> Vec<ListEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

impl std::fmt::Debug for ListState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("ListState")
            .field("selected", &inner.selection.count())
            .field("cursor", &inner.cursor)
            .field("offset", &inner.offset)
            .field("version", &inner.version)
            .finish_non_exhaustive()
    }
}

/// Space between a row's edges and its text.
const ROW_PAD: f32 = 10.0;

/// How far the pointer moves with the button held before the rows under it
/// are dragged.
const DRAG_START: f32 = 4.0;

/// How long typing waits for the next letter of a row's name before it
/// starts a new search.
const TYPE_AHEAD: Duration = Duration::from_secs(1);

/// The most letters typing searches for.
const TYPE_AHEAD_MAX: usize = 32;

/// A press on a row, until it is released or becomes a drag.
#[derive(Debug, Clone, Copy)]
struct Press {
    at: Point,
    row: usize,
    /// The row was selected already: the selection becomes just this row
    /// on release, unless the press becomes a drag of all of them.
    narrow: bool,
    dragging: bool,
}

/// A list of rows that an application keeps through [`ListData`], with
/// single and multiple selection, the keyboard, activation, scrolling and
/// drag and drop.
///
/// Only the rows that can be seen are drawn and described to assistive
/// technology, each a [`Role::ListItem`] that says whether it is selected;
/// the list itself is a [`Role::List`]. Rows are all
/// [`Sizes::list_row`](crate::Sizes::list_row) tall: rows of different
/// heights are not supported yet.
///
/// - A click selects a row; Ctrl (Command on macOS) and a click adds or
///   takes away one, Shift and a click selects every row from the last
///   one clicked. A double click, or Enter, activates the row.
/// - With the keyboard focus, the arrows move to the next row and select
///   it, Shift and the arrows extend the selection, Ctrl and the arrows
///   move without selecting and Ctrl+Space selects or unselects the row,
///   Page Up, Page Down, Home and End move further, Ctrl+A selects every
///   row, and typing the first letters of a row's name goes to it.
/// - The wheel, the bar and the keys scroll it, and it keeps the row the
///   keyboard is on in sight. At its ends it leaves the wheel to a
///   [`scroll_view`](crate::scroll_view) it is in.
/// - Dragging selected rows starts a drag with the payload
///   [`ListData::drag_payload`] makes; a drop the data
///   [`accepts`](ListData::accepts) between two rows shows a line there
///   and, once dropped, arrives as [`ListEvent::Dropped`]. Dragging near
///   the top or bottom edge scrolls the list.
///
/// What happens is polled from its [`ListState`]: no callback runs.
pub struct ListView {
    name: TextSource,
    data: Box<dyn ListData>,
    state: ListState,
    bars: Bars,
    len: usize,
    revision: Option<u64>,
    version: Option<u64>,
    press: Option<Press>,
    /// Where a drop would go now, while a drag is over the list.
    drop_at: Option<usize>,
    /// The last frame that scrolled the list during a drag.
    auto: Option<Duration>,
    auto_rest: f64,
    typed: String,
    typed_at: Duration,
    /// The row under the pointer and how far its hover has come in, and
    /// the row it left, fading out.
    hover_row: Option<(usize, Animated<f32>)>,
    fading: Option<(usize, Animated<f32>)>,
    /// The row last selected, its tint fading in.
    fresh: Option<(usize, Animated<f32>)>,
    /// The scroll bar: how far it has faded in, and when the list last
    /// scrolled.
    bar: Animated<f32>,
    scrolled_at: Option<Duration>,
    last_offset: Option<f64>,
}

/// A list called `name` of the rows `data` keeps, sharing its selection and
/// what happens in it through `state`.
///
/// ```
/// use std::cell::Cell;
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, ListData, ListEvent, ListState, Role, list_view};
///
/// struct Notes;
/// impl ListData for Notes {
///     fn len(&self) -> usize { 12 }
///     fn name(&self, row: usize) -> String {
///         ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"][row].to_owned()
///     }
///     fn revision(&self) -> u64 { 0 }
/// }
///
/// let state = ListState::new();
/// let mut ui = Harness::new(list_view("Notes", Notes, state.clone()), Size::new(120.0, 100.0), 1.0);
/// ui.frame();
/// // Only the rows that can be seen are there: three of 32 pixels, and part of a fourth.
/// assert_eq!(ui.find_all(manguebyte_ui::Role::ListItem, "D#").len(), 1);
/// assert!(ui.find(Role::ListItem, "E").is_none());
/// assert!(ui.find(Role::ListItem, "B").is_none());
///
/// ui.click(ui.get(Role::ListItem, "D").center());
/// assert_eq!(state.selected(), vec![2]);
/// // Down moves the selection, and End goes to the last row, scrolling to it.
/// assert!(ui.key(Key::Down));
/// assert!(ui.key(Key::End));
/// ui.frame();
/// assert_eq!(state.selected(), vec![11]);
/// let b = ui.get(Role::ListItem, "B");
/// assert_eq!(b.node.state.selected, Some(true));
/// // Enter activates it.
/// assert!(ui.key(Key::Enter));
/// assert!(matches!(state.take_events().as_slice(), [ListEvent::Activated(11)]));
/// ```
pub fn list_view(name: impl Into<String>, data: impl ListData, state: ListState) -> ListView {
    list_view_named(TextSource::fixed(name), data, state)
}

/// A list as [`list_view`] makes, named by `name`, fixed or read once per
/// frame (see [`TextSource`]): a list whose name follows the language,
/// say. Its name is not drawn: a new one is only sent to assistive
/// technology.
pub fn list_view_named(
    name: impl Into<TextSource>,
    data: impl ListData,
    state: ListState,
) -> ListView {
    ListView {
        name: name.into(),
        data: Box::new(data),
        state,
        bars: Bars::default(),
        len: 0,
        revision: None,
        version: None,
        press: None,
        drop_at: None,
        auto: None,
        auto_rest: 0.0,
        typed: String::new(),
        typed_at: Duration::ZERO,
        hover_row: None,
        fading: None,
        fresh: None,
        bar: Animated::new(0.0),
        scrolled_at: None,
        last_offset: None,
    }
}

/// Whether Ctrl, or Command on macOS, is held.
fn command(modifiers: Modifiers) -> bool {
    modifiers.control || modifiers.meta
}

impl ListView {
    /// Moves the hover to `row`: the row it leaves fades out.
    fn hover_to(&mut self, cx: &mut EventCx<'_>, row: Option<usize>) {
        if self.hover_row.map(|(at, _)| at) == row {
            return;
        }
        let (now, fast) = (cx.now(), cx.motion().fast());
        if let Some((left, mut amount)) = self.hover_row.take() {
            amount.transition(0.0, now, fast);
            self.fading = Some((left, amount));
        }
        self.hover_row = row.map(|row| {
            let mut amount = Animated::new(0.0);
            amount.transition(1.0, now, fast);
            (row, amount)
        });
        cx.request_paint();
    }

    /// The state it shares with the application.
    pub fn state(&self) -> &ListState {
        &self.state
    }

    fn row_height(theme: &Theme) -> f64 {
        f64::from(theme.sizes.list_row.max(1.0))
    }

    /// The scroll bar's geometry in `rect`, scrolled to `offset`.
    fn geometry(&self, rect: Rect, theme: &Theme, offset: f64) -> Geometry {
        let bar = theme.sizes.scroll_bar.max(0.0).min(rect.width);
        let viewport = Rect::new(rect.x, rect.y, (rect.width - bar).max(0.0), rect.height);
        Geometry {
            viewport,
            offset: (0.0, offset),
            content: (
                f64::from(viewport.width),
                self.len as f64 * Self::row_height(theme),
            ),
            x: false,
            y: true,
            bar,
        }
    }

    /// The row at height `y`, if there is one there.
    fn row_at(geometry: &Geometry, y: f32, height: f64, len: usize) -> Option<usize> {
        let from_top = f64::from(y - geometry.viewport.y);
        if !(0.0..f64::from(geometry.viewport.height)).contains(&from_top) {
            return None;
        }
        let row = ((from_top + geometry.offset.1) / height).floor();
        (row >= 0.0 && (row as usize) < len).then_some(row as usize)
    }

    /// The place between rows nearest height `y`.
    fn gap_at(geometry: &Geometry, y: f32, height: f64, len: usize) -> usize {
        let from_top = f64::from(y - geometry.viewport.y) + geometry.offset.1;
        let gap = (from_top / height).round();
        if gap.is_finite() {
            (gap.max(0.0) as usize).min(len)
        } else {
            0
        }
    }

    /// The rectangle of `row`, which may lie outside the viewport.
    fn row_rect(geometry: &Geometry, row: usize, height: f64) -> Rect {
        let y = row as f64 * height - geometry.offset.1;
        Rect::new(
            geometry.viewport.x,
            geometry.viewport.y + y as f32,
            geometry.viewport.width,
            height as f32,
        )
    }

    /// The rows the viewport shows, in part or whole.
    fn visible(geometry: &Geometry, height: f64, len: usize) -> Range<usize> {
        let top = geometry.offset.1;
        let bottom = top + f64::from(geometry.viewport.height);
        let first = (top / height).floor().max(0.0) as usize;
        let last = (bottom / height).ceil().max(0.0) as usize;
        first.min(len)..last.min(len)
    }

    /// The offset that brings `row` into sight from `offset`, moving as
    /// little as it can.
    fn reveal_offset(geometry: &Geometry, row: usize, height: f64, scale: f32) -> f64 {
        let top = row as f64 * height;
        let view = f64::from(geometry.viewport.height);
        let offset = geometry.offset.1;
        let to = if top < offset || height >= view {
            top
        } else if top + height > offset + view {
            top + height - view
        } else {
            offset
        };
        geometry.clamp((0.0, snap(to, scale))).1
    }

    /// Moves the keyboard to `row`, selecting as the modifiers say, and
    /// scrolls to it.
    fn go_to(&mut self, cx: &mut EventCx<'_>, row: usize, modifiers: Modifiers) {
        let theme = cx.theme();
        let height = Self::row_height(theme);
        let offset = self.state.scroll_offset();
        let geometry = self.geometry(cx.rect(), theme, offset);
        let scale = cx.scale();
        let to = Self::reveal_offset(&geometry, row, height, scale);
        self.state.change(|inner| {
            if modifiers.shift {
                let anchor = inner.anchor.or(inner.cursor).unwrap_or(row);
                if !command(modifiers) {
                    inner.selection.clear();
                }
                inner.selection.add(anchor.min(row)..anchor.max(row) + 1);
                inner.anchor = Some(anchor);
            } else if !command(modifiers) {
                inner.selection.clear();
                inner.selection.add(row..row + 1);
                inner.anchor = Some(row);
            }
            inner.cursor = Some(row);
            inner.offset = to;
        });
        cx.request_paint();
    }

    fn scroll_to(&mut self, cx: &mut EventCx<'_>, offset: f64) {
        self.state.change(|inner| inner.offset = offset);
        cx.request_paint();
    }

    fn key(&mut self, cx: &mut EventCx<'_>, key: Key, modifiers: Modifiers) -> Handled {
        let len = self.len;
        if len == 0 {
            return Handled::No;
        }
        let theme = cx.theme();
        let height = Self::row_height(theme);
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let page = ((f64::from(geometry.viewport.height) / height).floor() as usize).max(2) - 1;
        let cursor = self.state.cursor().filter(|&row| row < len);
        let last = len - 1;
        let row = match key {
            Key::Up => cursor.map_or(last, |row| row.saturating_sub(1)),
            Key::Down => cursor.map_or(0, |row| (row + 1).min(last)),
            Key::PageUp => cursor.map_or(0, |row| row.saturating_sub(page)),
            Key::PageDown => cursor.map_or(0, |row| (row + page).min(last)),
            Key::Home => 0,
            Key::End => last,
            Key::Enter => {
                let Some(row) = cursor else {
                    return Handled::No;
                };
                self.state
                    .change(|inner| inner.events.push(ListEvent::Activated(row)));
                return Handled::Yes;
            }
            Key::Character(' ') => {
                let Some(row) = cursor else {
                    return Handled::No;
                };
                self.state.change(|inner| {
                    if command(modifiers) && inner.selection.contains(row) {
                        inner.selection.remove(row);
                    } else if command(modifiers) {
                        inner.selection.add(row..row + 1);
                    } else {
                        inner.selection.clear();
                        inner.selection.add(row..row + 1);
                    }
                    inner.anchor = Some(row);
                });
                cx.request_paint();
                return Handled::Yes;
            }
            Key::Character(c) if command(modifiers) && c.eq_ignore_ascii_case(&'a') => {
                self.state.change(|inner| {
                    inner.selection.clear();
                    inner.selection.add(0..len);
                });
                cx.request_paint();
                return Handled::Yes;
            }
            // A letter goes to the row it starts, when it arrives as typed
            // text; the key itself is the list's, not the host's.
            Key::Character(_) if !command(modifiers) => return Handled::Yes,
            _ => return Handled::No,
        };
        self.go_to(cx, row, modifiers);
        Handled::Yes
    }

    /// Typing the first letters of a row's name goes to it.
    fn type_ahead(&mut self, cx: &mut EventCx<'_>, c: char) -> Handled {
        if self.len == 0 || c.is_control() {
            return Handled::No;
        }
        let now = cx.now();
        if now.saturating_sub(self.typed_at) > TYPE_AHEAD
            || self.typed.chars().count() >= TYPE_AHEAD_MAX
        {
            self.typed.clear();
        }
        if self.typed.is_empty() && c == ' ' {
            return Handled::No;
        }
        self.typed_at = now;
        self.typed.extend(c.to_lowercase());
        let len = self.len;
        let cursor = self.state.cursor().filter(|&row| row < len);
        // A new search starts after the row the keyboard is on; letters
        // added to a search look at that row first.
        let start = match cursor {
            Some(row) if self.typed.chars().count() == 1 => row + 1,
            Some(row) => row,
            None => 0,
        };
        let found = (0..len)
            .map(|step| (start + step) % len)
            .find(|&row| self.data.name(row).to_lowercase().starts_with(&self.typed));
        match found {
            Some(row) => {
                self.go_to(cx, row, Modifiers::NONE);
                Handled::Yes
            }
            None => Handled::Yes,
        }
    }

    fn press(
        &mut self,
        cx: &mut EventCx<'_>,
        at: Point,
        clicks: u32,
        modifiers: Modifiers,
    ) -> Handled {
        cx.request_focus();
        cx.request_paint();
        let theme = cx.theme();
        let height = Self::row_height(theme);
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let Some(row) = Self::row_at(&geometry, at.y, height, self.len)
            .filter(|_| geometry.viewport.contains(at))
        else {
            // Below the last row: nothing is selected, unless a modifier
            // keeps the selection.
            if !(modifiers.shift || command(modifiers)) {
                self.state.clear_selection();
            }
            return Handled::Yes;
        };
        let mut narrow = false;
        self.state.change(|inner| {
            if modifiers.shift {
                let anchor = inner.anchor.unwrap_or(row);
                if !command(modifiers) {
                    inner.selection.clear();
                }
                inner.selection.add(anchor.min(row)..anchor.max(row) + 1);
                inner.anchor = Some(anchor);
            } else if command(modifiers) {
                if inner.selection.contains(row) {
                    inner.selection.remove(row);
                } else {
                    inner.selection.add(row..row + 1);
                }
                inner.anchor = Some(row);
            } else if inner.selection.contains(row) && clicks < 2 {
                // Perhaps the start of a drag of every selected row.
                narrow = true;
                inner.anchor = Some(row);
            } else {
                inner.selection.clear();
                inner.selection.add(row..row + 1);
                inner.anchor = Some(row);
            }
            inner.cursor = Some(row);
            if clicks >= 2 && !modifiers.shift && !command(modifiers) {
                inner.events.push(ListEvent::Activated(row));
            }
        });
        self.press = Some(Press {
            at,
            row,
            narrow,
            dragging: false,
        });
        cx.capture_pointer();
        Handled::Yes
    }

    fn pointer_move(&mut self, cx: &mut EventCx<'_>, at: Point) -> Handled {
        let Some(press) = &mut self.press else {
            return Handled::No;
        };
        if press.dragging {
            return Handled::Yes;
        }
        let moved = (at.x - press.at.x).hypot(at.y - press.at.y);
        if moved < DRAG_START {
            return Handled::Yes;
        }
        press.dragging = true;
        let rows = self.state.selected();
        if let Some(payload) = (!rows.is_empty())
            .then(|| self.data.drag_payload(&rows))
            .flatten()
        {
            cx.start_drag(payload);
        }
        Handled::Yes
    }

    fn release(&mut self) {
        if let Some(press) = self.press.take() {
            if press.narrow && !press.dragging {
                let row = press.row;
                self.state.change(|inner| {
                    inner.selection.clear();
                    inner.selection.add(row..row + 1);
                });
            }
        }
    }

    /// Scrolls the list while a drag is near its top or bottom edge.
    fn auto_scroll(&mut self, cx: &mut EventCx<'_>, at: Point) {
        let theme = cx.theme();
        let offset = self.state.scroll_offset();
        let geometry = self.geometry(cx.rect(), theme, offset);
        let now = cx.now();
        let elapsed = self.auto.map_or(Duration::ZERO, |then| {
            now.saturating_sub(then).min(AUTO_STEP)
        });
        let by = f64::from(auto_scroll(geometry.viewport, at, elapsed).y);
        let near = auto_scroll(geometry.viewport, at, AUTO_STEP).y != 0.0;
        self.auto = near.then_some(now);
        if !near {
            self.auto_rest = 0.0;
            return;
        }
        let exact = geometry.clamp((0.0, offset + by + self.auto_rest)).1;
        let snapped = geometry.clamp((0.0, snap(exact, cx.scale()))).1;
        self.auto_rest = exact - snapped;
        if snapped != offset {
            self.scroll_to(cx, snapped);
        }
    }
}

impl Widget for ListView {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let selected = self.state.selected_count();
        AccessNode::new(Role::List, self.name.as_str())
            .with_value(format!("{} rows, {selected} selected", self.len))
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let theme = cx.theme();
        let height = Self::row_height(theme);
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let inner = self.state.lock();
        for row in Self::visible(&geometry, height, self.len) {
            let bounds = Self::row_rect(&geometry, row, height);
            let Some(bounds) = bounds.intersect(&geometry.viewport) else {
                continue;
            };
            let mut node = AccessNode::new(Role::ListItem, self.data.name(row))
                .with_value(format!("{} of {}", row + 1, self.len))
                .with_selected(inner.selection.contains(row))
                .with_actions(Actions::PRESS);
            node.state.focused = cx.is_focused() && inner.cursor == Some(row);
            items.push(AccessItem::new(row, node, bounds));
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        // Room for a few rows; a list is usually given more by layout.
        let theme = cx.theme();
        Size::new(
            theme.sizes.slider_width + theme.sizes.scroll_bar,
            theme.sizes.list_row * 4.0,
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        super::poll_name(&mut self.name, cx);
        let len = self.data.len();
        let revision = self.data.revision();
        if len != self.len {
            self.len = len;
            self.state.change(|inner| {
                inner.selection.truncate(len);
                inner.cursor = inner.cursor.filter(|&row| row < len);
                inner.anchor = inner.anchor.filter(|&row| row < len);
            });
            cx.request_paint();
        }
        if self.revision != Some(revision) {
            self.revision = Some(revision);
            cx.request_paint();
        }
        // Keep the offset within the rows, and bring a row in sight if the
        // application asked.
        let theme = cx.theme();
        let height = Self::row_height(theme);
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let scale = cx.scale();
        let laid_out = geometry.viewport.height > 0.0;
        let (offset, reveal) = {
            let inner = self.state.lock();
            (inner.offset, inner.reveal)
        };
        if laid_out {
            let wanted = match reveal {
                Some(row) if row < len => Self::reveal_offset(&geometry, row, height, scale),
                _ => geometry.clamp((0.0, snap(offset, scale))).1,
            };
            if wanted != offset || reveal.is_some() {
                self.state.change(|inner| {
                    inner.offset = wanted;
                    inner.reveal = None;
                });
            }
        }
        let version = self.state.version();
        if self.version != Some(version) {
            // A row newly selected where the keyboard is fades its tint in.
            let (cursor, selected) = {
                let inner = self.state.lock();
                let cursor = inner.cursor;
                (
                    cursor,
                    cursor.is_some_and(|row| inner.selection.contains(row)),
                )
            };
            let first = self.version.is_none();
            if let (Some(row), true, false) = (cursor, selected, first) {
                if self.fresh.as_ref().is_none_or(|(fresh, _)| *fresh != row) {
                    let mut tint = Animated::new(0.0);
                    tint.transition(1.0, cx.now(), cx.motion().base());
                    self.fresh = Some((row, tint));
                }
            }
            self.version = Some(version);
            cx.request_paint();
        }
        // Hover and selection tints, and the scroll bar.
        for (_, amount) in [&mut self.hover_row, &mut self.fading, &mut self.fresh]
            .into_iter()
            .flatten()
        {
            cx.animate(amount);
        }
        if self.fading.as_ref().is_some_and(|(_, a)| !a.is_running()) {
            self.fading = None;
        }
        let offset = self.state.scroll_offset();
        if self.last_offset.is_some_and(|last| last != offset) {
            self.scrolled_at = Some(cx.now());
        }
        self.last_offset = Some(offset);
        let held = self.bars.dragging();
        follow_bars(cx, &mut self.bar, self.scrolled_at, held);
    }

    fn hover_moved(&mut self, cx: &mut EventCx<'_>, at: Point) {
        let theme = cx.theme();
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let row = Self::row_at(&geometry, at.y, Self::row_height(theme), self.len)
            .filter(|_| geometry.viewport.contains(at));
        self.hover_to(cx, row);
    }

    fn hover_changed(&mut self, cx: &mut EventCx<'_>, hovered: bool) {
        if !hovered {
            self.hover_to(cx, None);
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let height = Self::row_height(theme);
        let geometry = self.geometry(rect, theme, self.state.scroll_offset());
        let palette = &theme.palette;
        let (hover_row, fading, fresh) = (self.hover_row, self.fading, self.fresh);
        let hover_of = |row: usize| {
            [hover_row, fading]
                .into_iter()
                .flatten()
                .find(|(at, _)| *at == row)
                .map_or(0.0, |(_, amount)| amount.value())
        };
        let ring = cx.focus_ring();
        let (selection, cursor) = {
            let inner = self.state.lock();
            let visible = Self::visible(&geometry, height, self.len);
            let selected: Vec<bool> = visible
                .clone()
                .map(|row| inner.selection.contains(row))
                .collect();
            ((visible, selected), inner.cursor)
        };
        let focused = cx.is_focused();
        let data = &self.data;
        let drop_at = self.drop_at;
        let len = self.len;
        scene.clipped(geometry.viewport, |scene| {
            let (visible, selected) = &selection;
            for (row, &is_selected) in visible.clone().zip(selected) {
                let row_rect = Self::row_rect(&geometry, row, height);
                // A rounded body a little inside the row, so that rows next
                // to each other read apart.
                let body = Rect::new(
                    row_rect.x + 2.0,
                    row_rect.y + 1.0,
                    (row_rect.width - 4.0).max(0.0),
                    (row_rect.height - 2.0).max(0.0),
                );
                let shape = RoundedRect::new(body, theme.corners.md.min(body.height * 0.5));
                let hover = hover_of(row);
                if hover > 0.0 && !is_selected {
                    scene.fill(shape, palette.track.with_opacity(0.6 * hover));
                }
                if is_selected {
                    let tint = fresh
                        .filter(|(at, _)| *at == row)
                        .map_or(1.0, |(_, amount)| amount.value());
                    scene.fill(shape, palette.accent_soft.with_opacity(tint));
                    let edge = Rect::new(
                        body.x + 0.5,
                        body.y + 0.5,
                        (body.width - 1.0).max(0.0),
                        (body.height - 1.0).max(0.0),
                    );
                    scene.stroke(
                        RoundedRect::new(edge, (shape.radii.top_left - 0.5).max(0.0)),
                        Stroke::new(1.0),
                        palette.accent.with_alpha(0x40).with_opacity(tint),
                    );
                }
                let is_cursor = focused && cursor == Some(row);
                let row_cx = RowCx {
                    theme,
                    rect: row_rect,
                    scale: cx.scale(),
                    selected: is_selected,
                    cursor: is_cursor,
                    hover,
                };
                data.paint_row(row, &row_cx, scene);
                if is_cursor {
                    let inner = RoundedRect::new(
                        Rect::new(
                            body.x + 1.0,
                            body.y + 1.0,
                            (body.width - 2.0).max(0.0),
                            (body.height - 2.0).max(0.0),
                        ),
                        (shape.radii.top_left - 1.0).max(0.0),
                    );
                    focus_ring(scene, inner, theme, ring);
                }
            }
            if let Some(gap) = drop_at.filter(|&gap| gap <= len) {
                // Where a drop goes: a line between two rows.
                let y = geometry.viewport.y + (gap as f64 * height - geometry.offset.1) as f32;
                let line = Rect::new(geometry.viewport.x, y - 1.0, geometry.viewport.width, 2.0);
                scene.fill(line, theme.palette.accent);
            }
        });
        paint_bars(
            scene,
            &geometry,
            theme,
            (self.bar.value(), cx.hover(), ring),
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let theme = cx.theme();
        let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
        let line = theme.sizes.scroll_line;
        // The bar, the wheel and the keys that scroll, unless a row is being
        // pressed.
        if self.press.is_none() {
            // The list moves its keyboard row itself, so the bars take no keys.
            let outcome = self.bars.event(&geometry, event, false, line, cx.scale());
            if let Some((_, offset)) = outcome.scroll_to {
                self.scroll_to(cx, offset);
            }
            if outcome.handled.is_yes() {
                if self.bars.dragging() {
                    cx.capture_pointer();
                }
                return Handled::Yes;
            }
        }
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } => self.press(cx, at, clicks, modifiers),
            Event::PointerMove { at, .. } => self.pointer_move(cx, at),
            Event::PointerUp { .. } => {
                let pressed = self.press.is_some();
                self.release();
                if pressed {
                    cx.request_paint();
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            Event::PointerCancel => {
                self.press = None;
                Handled::No
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed: true,
            }) if cx.is_focused() && !modifiers.alt => self.key(cx, key, modifiers),
            Event::Text(c) if cx.is_focused() => self.type_ahead(cx, c),
            _ => Handled::No,
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        if key >= self.len {
            return false;
        }
        match action {
            Action::Press => {
                self.go_to(cx, key, Modifiers::NONE);
                true
            }
            Action::Focus => {
                let theme = cx.theme();
                let geometry = self.geometry(cx.rect(), theme, self.state.scroll_offset());
                let to = Self::reveal_offset(&geometry, key, Self::row_height(theme), cx.scale());
                self.state.change(|inner| {
                    inner.cursor = Some(key);
                    inner.offset = to;
                });
                cx.request_paint();
                true
            }
            _ => false,
        }
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, _focused: bool) {
        self.typed.clear();
        cx.request_paint();
    }

    fn drag(&mut self, cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let theme = cx.theme();
        let height = Self::row_height(theme);
        match *event {
            DragEvent::Over { at, payload } => {
                self.auto_scroll(cx, at);
                let geometry = self.geometry(cx.rect(), cx.theme(), self.state.scroll_offset());
                let gap = Self::gap_at(&geometry, at.y, height, self.len);
                let accepted = geometry.viewport.contains(at) && self.data.accepts(payload, gap);
                let drop_at = accepted.then_some(gap);
                if drop_at != self.drop_at {
                    self.drop_at = drop_at;
                    cx.request_paint();
                }
                if accepted { Handled::Yes } else { Handled::No }
            }
            DragEvent::Left => {
                self.auto = None;
                if self.drop_at.take().is_some() {
                    cx.request_paint();
                }
                Handled::Yes
            }
            DragEvent::Drop { at, payload } => {
                self.auto = None;
                self.drop_at = None;
                cx.request_paint();
                let geometry = self.geometry(cx.rect(), cx.theme(), self.state.scroll_offset());
                let index = Self::gap_at(&geometry, at.y, height, self.len);
                if !self.data.accepts(payload, index) {
                    return Handled::No;
                }
                let payload = payload.clone();
                self.state
                    .change(|inner| inner.events.push(ListEvent::Dropped { index, payload }));
                Handled::Yes
            }
            DragEvent::Ended(_) => {
                self.press = None;
                Handled::Yes
            }
        }
    }
}

impl std::fmt::Debug for ListView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListView")
            .field("name", &self.name.as_str())
            .field("len", &self.len)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(selection: &Selection) -> Vec<usize> {
        selection.rows().collect()
    }

    #[test]
    fn a_selection_merges_touching_rows_and_splits_around_one_taken_away() {
        let mut selection = Selection::default();
        selection.add(5..8);
        selection.add(1..3);
        selection.add(3..5);
        assert_eq!(selection.ranges, vec![1..8]);
        selection.remove(4);
        assert_eq!(selection.ranges, vec![1..4, 5..8]);
        assert!(selection.contains(3) && !selection.contains(4) && selection.contains(7));
        selection.add(10..11);
        selection.add(0..12);
        assert_eq!(selection.ranges, vec![0..12]);
        selection.truncate(6);
        assert_eq!(rows(&selection), vec![0, 1, 2, 3, 4, 5]);
        assert_eq!(selection.count(), 6);
    }

    #[test]
    fn selecting_a_hundred_thousand_rows_is_one_range() {
        let mut selection = Selection::default();
        selection.add(0..100_000);
        selection.remove(50_000);
        assert_eq!(selection.ranges.len(), 2);
        assert_eq!(selection.count(), 99_999);
        assert!(!selection.contains(50_000) && selection.contains(99_999));
    }
}
