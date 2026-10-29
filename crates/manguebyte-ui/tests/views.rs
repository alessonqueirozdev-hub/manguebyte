//! Scroll views, lists, drag and drop, and images, driven headless.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use manguebyte_gfx::{Color, IRect, Image, Pixmap, Point, Rect, Scene, Size};
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::drag::{DragEvent, DragPayload};
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    AccessCx, Button, Event, EventCx, Fit, Handled, IntoView, Key, KeyEvent, ListData, ListEvent,
    ListState, MeasureCx, Modifiers, PaintCx, RowCx, State, View, Widget, column, image, label,
    list_view, row, scroll_view, text_field,
};

const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};
const CONTROL: Modifiers = Modifiers {
    control: true,
    ..Modifiers::NONE
};

/// `count` labels 20 tall, named "Row 0" and on.
fn rows(count: usize) -> View {
    let rows: Vec<_> = (0..count)
        .map(|i| label(format!("Row {i}")).height(20.0))
        .collect();
    column(rows)
}

fn metrics(ui: &Harness, found: &Found) -> manguebyte_ui::ScrollMetrics {
    ui.root()
        .scroll_metrics(found.id)
        .expect("a scroll view has metrics")
}

fn offset_y(ui: &Harness, name: &str) -> f32 {
    metrics(ui, &ui.get(Role::ScrollView, name)).offset.y
}

/// Every damage rectangle of the last frame lies inside `rect`.
fn damage_inside(ui: &Harness, rect: IRect) -> bool {
    !ui.last_damage().is_empty() && ui.last_damage().iter().all(|d| rect.contains_rect(d))
}

// Scroll views.

#[test]
fn a_scroll_view_moves_its_child_and_shows_and_lets_press_only_what_is_in_sight() {
    let mut ui = Harness::new(scroll_view("Rows", rows(20)), Size::new(120.0, 100.0), 1.0);
    ui.frame();
    let view = ui.get(Role::ScrollView, "Rows");
    let m = metrics(&ui, &view);
    // The bar's room is kept at the right.
    assert_eq!(m.viewport, Rect::new(0.0, 0.0, 110.0, 100.0));
    assert_eq!(m.content, Size::new(110.0, 400.0));
    assert_eq!(m.max_offset(), Point::new(0.0, 300.0));
    // A row out of sight draws nothing and cannot be hit.
    let hidden = ui.get(Role::Label, "Row 10");
    assert_eq!(hidden.bounds.y, 200.0);
    assert!(ui.root().widget_scene(hidden.id).expect("there").is_empty());
    let shown = ui.get(Role::Label, "Row 2");
    assert!(!ui.root().widget_scene(shown.id).expect("there").is_empty());
    assert_eq!(ui.root().hit(Point::new(20.0, 50.0)), Some(shown.id));

    ui.root_mut()
        .act(view.id, manguebyte_ui::Action::SetValue(200.0));
    ui.frame();
    let moved = ui.get(Role::Label, "Row 10");
    assert_eq!(moved.bounds.y, 0.0);
    assert_eq!(ui.root().hit(Point::new(20.0, 10.0)), Some(moved.id));
    assert!(!ui.root().widget_scene(moved.id).expect("there").is_empty());
    assert!(ui.root().widget_scene(shown.id).expect("there").is_empty());
    // Half a row at the top edge is cut at the viewport.
    ui.root_mut()
        .act(view.id, manguebyte_ui::Action::SetValue(210.0));
    ui.frame();
    let half = ui.get(Role::Label, "Row 10");
    assert_eq!(half.bounds.y, -10.0);
    let drawn = ui
        .root()
        .widget_scene(half.id)
        .and_then(Scene::bounds)
        .expect("half of it is drawn");
    assert!(drawn.y >= 0.0, "{drawn:?}");
    assert_eq!(ui.root().hit(Point::new(20.0, 5.0)), Some(half.id));
    ui.assert_accessible();
}

#[test]
fn the_wheel_scrolls_by_lines_and_by_the_fractions_a_touchpad_sends() {
    for scale in [1.0, 1.5, 2.0] {
        let mut ui = Harness::new(
            scroll_view("Rows", rows(20)),
            Size::new(120.0, 100.0),
            scale,
        );
        ui.frame();
        let at = ui.get(Role::ScrollView, "Rows").center();
        // One notch is the theme's line.
        assert!(ui.wheel(at, -1.0));
        assert_eq!(offset_y(&ui, "Rows"), 40.0, "{scale}x");
        // Eight eighths of a notch are a notch, exactly, whatever the
        // rounding to device pixels on the way.
        for _ in 0..8 {
            assert!(ui.wheel_by(at, Point::new(0.0, -0.125)));
        }
        assert_eq!(offset_y(&ui, "Rows"), 80.0, "{scale}x");
        for _ in 0..3 {
            ui.wheel_by(at, Point::new(0.0, -0.3));
        }
        let fine = offset_y(&ui, "Rows");
        assert!((fine - 116.0).abs() <= 1.0 / scale, "{scale}x: {fine}");
        assert_eq!((fine * scale).fract(), 0.0, "whole device pixels");
        // Up again, past the top: kept at 0, and the wheel at the top is
        // left to whatever is around.
        assert!(ui.wheel(at, 10.0));
        assert_eq!(offset_y(&ui, "Rows"), 0.0);
        assert!(!ui.wheel(at, 1.0));
    }
}

#[test]
fn a_sideways_view_scrolls_with_a_sideways_wheel_and_with_shift() {
    let wide: Vec<_> = (0..10)
        .map(|i| label(format!("Column {i}")).width(50.0))
        .collect();
    let view = scroll_view("Columns", row(wide)).horizontal();
    let mut ui = Harness::new(view, Size::new(100.0, 40.0), 1.0);
    ui.frame();
    let found = ui.get(Role::ScrollView, "Columns");
    let m = metrics(&ui, &found);
    assert_eq!(m.viewport, Rect::new(0.0, 0.0, 100.0, 30.0));
    assert_eq!(m.max_offset(), Point::new(400.0, 0.0));
    assert!(ui.wheel_by(found.center(), Point::new(1.0, 0.0)));
    assert_eq!(metrics(&ui, &found).offset.x, 40.0);
    ui.set_modifiers(SHIFT);
    assert!(ui.wheel(found.center(), -1.0));
    assert_eq!(metrics(&ui, &found).offset.x, 80.0);
    ui.set_modifiers(Modifiers::NONE);
    // Up and down means nothing to it.
    assert!(!ui.wheel(found.center(), -1.0));
}

#[test]
fn the_thumb_drags_and_a_press_on_the_track_moves_a_page() {
    let mut ui = Harness::new(scroll_view("Rows", rows(20)), Size::new(120.0, 100.0), 1.0);
    ui.frame();
    // The thumb is a quarter of the track: 25 of 100, with 75 to move in
    // for 300 of offset.
    ui.drag(Point::new(115.0, 10.0), (0.0, 25.0), 5);
    assert_eq!(offset_y(&ui, "Rows"), 100.0);
    ui.frame();
    // Below the thumb, which is now at 25 to 50: a page down.
    ui.click(Point::new(115.0, 90.0));
    assert_eq!(offset_y(&ui, "Rows"), 200.0);
    ui.click(Point::new(115.0, 5.0));
    assert_eq!(offset_y(&ui, "Rows"), 100.0);
    // Dragging past the end stops there.
    ui.frame();
    ui.drag(Point::new(115.0, 40.0), (0.0, 400.0), 4);
    assert_eq!(offset_y(&ui, "Rows"), 300.0);
}

#[test]
fn a_focused_scroll_view_scrolls_with_the_keys() {
    let mut ui = Harness::new(scroll_view("Rows", rows(20)), Size::new(120.0, 100.0), 1.0);
    ui.frame();
    assert!(!ui.key(Key::Down), "nothing has the focus");
    assert!(ui.tab());
    assert_eq!(ui.focused().map(|f| f.node.role), Some(Role::ScrollView));
    assert!(ui.key(Key::Down));
    assert_eq!(offset_y(&ui, "Rows"), 40.0);
    assert!(ui.key(Key::PageDown));
    assert_eq!(offset_y(&ui, "Rows"), 140.0);
    assert!(ui.key(Key::Up));
    assert_eq!(offset_y(&ui, "Rows"), 100.0);
    assert!(ui.key(Key::End));
    assert_eq!(offset_y(&ui, "Rows"), 300.0);
    assert!(ui.key(Key::PageUp));
    assert_eq!(offset_y(&ui, "Rows"), 200.0);
    assert!(ui.key(Key::Home));
    assert_eq!(offset_y(&ui, "Rows"), 0.0);
    // It cannot scroll sideways, so Left is not its.
    assert!(!ui.key(Key::Left));
    ui.frame();
    assert_eq!(ui.get(Role::ScrollView, "Rows").value_text(), Some("0 %"));
}

#[test]
fn a_view_with_nothing_to_scroll_takes_no_focus_and_no_wheel() {
    let mut ui = Harness::new(scroll_view("Rows", rows(3)), Size::new(120.0, 100.0), 1.0);
    ui.frame();
    let view = ui.get(Role::ScrollView, "Rows");
    assert!(!view.node.actions.focus);
    assert!(!ui.tab());
    assert!(!ui.wheel(view.center(), -1.0));
    ui.assert_accessible();
}

#[test]
fn tab_into_a_field_out_of_sight_scrolls_to_it_and_shift_tab_back() {
    let fields: Vec<_> = (0..12)
        .map(|i| text_field(format!("Field {i}"), State::new(String::new())))
        .collect();
    let view = scroll_view("Form", column(fields).spacing(4.0));
    let mut ui = Harness::new(view, Size::new(160.0, 70.0), 1.25);
    ui.frame();
    let viewport = metrics(&ui, &ui.get(Role::ScrollView, "Form")).viewport;
    // The view first, then each field.
    assert!(ui.tab());
    for i in 0..12 {
        assert!(ui.tab());
        ui.frame();
        let focused = ui.focused().expect("a field has the focus");
        assert_eq!(focused.name(), format!("Field {i}"));
        assert!(
            focused.bounds.y >= viewport.y - 0.01
                && focused.bounds.bottom() <= viewport.bottom() + 0.01,
            "{i}: {:?} not in {viewport:?}",
            focused.bounds
        );
    }
    assert!(offset_y(&ui, "Form") > 0.0);
    for i in (0..11).rev() {
        assert!(ui.shift_tab());
        ui.frame();
        let focused = ui.focused().expect("a field has the focus");
        assert_eq!(focused.name(), format!("Field {i}"));
        assert!(focused.bounds.y >= viewport.y - 0.01, "{i}");
    }
    // So does assistive technology giving the focus.
    let last = ui.get(Role::TextInput, "Field 11");
    assert!(ui.act(&last, manguebyte_ui::Action::Focus));
    ui.frame();
    let last = ui.get(Role::TextInput, "Field 11");
    assert!(last.bounds.bottom() <= viewport.bottom() + 0.01);
    ui.assert_accessible();
}

#[test]
fn a_view_inside_another_takes_the_wheel_until_its_edge_then_the_outer_one_does() {
    let view = scroll_view(
        "Outer",
        column((
            label("Top").height(50.0),
            scroll_view("Inner", rows(10)).height(100.0),
            label("Bottom").height(300.0),
        )),
    );
    let mut ui = Harness::new(view, Size::new(160.0, 200.0), 1.0);
    ui.frame();
    let inner_at = |ui: &Harness| ui.get(Role::ScrollView, "Inner").center();
    // The inner view: 200 of rows in 100, so 100 to go, in 40, 40 and 20.
    for expected in [40.0, 80.0, 100.0] {
        let at = inner_at(&ui);
        assert!(ui.wheel(at, -1.0));
        ui.frame();
        assert_eq!(offset_y(&ui, "Inner"), expected);
        assert_eq!(offset_y(&ui, "Outer"), 0.0);
    }
    // At its end, the outer view scrolls.
    let at = inner_at(&ui);
    assert!(ui.wheel(at, -1.0));
    ui.frame();
    assert_eq!(offset_y(&ui, "Inner"), 100.0);
    assert_eq!(offset_y(&ui, "Outer"), 40.0);
    // Back up: the inner one first again.
    let at = inner_at(&ui);
    assert!(ui.wheel(at, 1.0));
    ui.frame();
    assert_eq!(
        (offset_y(&ui, "Inner"), offset_y(&ui, "Outer")),
        (60.0, 40.0)
    );
    // The inner view is clipped by the outer one: its rows above the
    // outer viewport cannot be hit there.
    let inner = ui.get(Role::ScrollView, "Inner");
    assert_eq!(inner.bounds.y, 10.0);
    ui.assert_accessible();
}

#[test]
fn the_application_reads_and_sets_the_offset_through_a_state() {
    let position = State::new(Point::new(0.0, 1_000.0));
    let view = scroll_view("Rows", rows(20)).position(position.clone());
    let mut ui = Harness::new(view, Size::new(120.0, 100.0), 1.0);
    // Asked before the first layout: kept within the rows once laid out,
    // and the state says where it really is.
    ui.frame();
    assert_eq!(offset_y(&ui, "Rows"), 300.0);
    ui.frame();
    assert_eq!(position.get(), Point::new(0.0, 300.0));
    position.set(Point::new(0.0, 123.4));
    ui.frame();
    assert_eq!(offset_y(&ui, "Rows"), 123.0);
    let at = ui.get(Role::ScrollView, "Rows").center();
    ui.wheel(at, 1.0);
    ui.frame();
    assert_eq!(position.get(), Point::new(0.0, 83.0));
}

#[test]
fn scrolling_draws_only_the_view_again_and_equals_a_whole_repaint() {
    for scale in [1.0, 1.25, 2.0] {
        let view = row((
            label("Outside").width(60.0),
            scroll_view("Rows", rows(30)).flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(200.0, 120.0), scale);
        ui.frame();
        let found = ui.get(Role::ScrollView, "Rows");
        let at = found.center();
        let steps = [
            Point::new(0.0, -1.0),
            Point::new(0.0, -0.37),
            Point::new(0.0, -2.5),
            Point::new(0.0, 0.8),
            Point::new(0.0, -20.0),
        ];
        for delta in steps {
            assert!(ui.wheel_by(at, delta));
            let summary = ui.frame();
            assert!(summary.painted > 0);
            assert!(
                damage_inside(&ui, found.device),
                "{scale}x: {:?} outside {:?}",
                ui.last_damage(),
                found.device
            );
            assert!(
                ui.pixels() == &ui.render_fresh(),
                "{scale}x after {delta:?}"
            );
        }
        // The thumb and a page, the same.
        ui.click(Point::new(195.0, 10.0));
        ui.frame();
        assert!(damage_inside(&ui, found.device));
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after a page");
        // Nothing moving, nothing drawn.
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(200)), 0);
    }
}

// Lists.

/// Rows the application keeps, shared with the test.
#[derive(Clone)]
struct Shelf {
    rows: Rc<RefCell<Vec<String>>>,
    revision: Rc<Cell<u64>>,
}

/// What dragging rows of a shelf carries.
struct Moving {
    from: &'static str,
    rows: Vec<usize>,
}

impl Shelf {
    fn new(names: &[&str]) -> Shelf {
        Shelf {
            rows: Rc::new(RefCell::new(names.iter().map(|&n| n.to_owned()).collect())),
            revision: Rc::new(Cell::new(0)),
        }
    }

    fn names(&self) -> Vec<String> {
        self.rows.borrow().clone()
    }

    fn bump(&self) {
        self.revision.set(self.revision.get() + 1);
    }
}

/// A shelf as the list of one name, which drags its rows and takes rows
/// from any shelf.
struct ShelfList {
    name: &'static str,
    shelf: Shelf,
}

impl ListData for ShelfList {
    fn len(&self) -> usize {
        self.shelf.rows.borrow().len()
    }
    fn name(&self, row: usize) -> String {
        self.shelf
            .rows
            .borrow()
            .get(row)
            .cloned()
            .unwrap_or_default()
    }
    fn revision(&self) -> u64 {
        self.shelf.revision.get()
    }
    fn drag_payload(&self, rows: &[usize]) -> Option<DragPayload> {
        let description = match rows {
            [row] => self.name(*row),
            rows => format!("{} samples", rows.len()),
        };
        Some(DragPayload::new(
            Moving {
                from: self.name,
                rows: rows.to_vec(),
            },
            description,
        ))
    }
    fn accepts(&self, payload: &DragPayload, _index: usize) -> bool {
        payload.is::<Moving>()
    }
}

/// What the application does with a drop: moves the rows named, from
/// whichever shelf, to `index` of `to`, and selects them there.
fn apply_drop(
    shelves: &[(&'static str, &Shelf, &ListState)],
    to: &'static str,
    index: usize,
    payload: &DragPayload,
) {
    let moving = payload.get::<Moving>().expect("rows of a shelf");
    let (_, from, from_state) = shelves
        .iter()
        .find(|(name, _, _)| *name == moving.from)
        .expect("a known shelf");
    // The rows the source had selected are gone from it.
    from_state.clear_selection();
    let (_, target, state) = shelves
        .iter()
        .find(|(name, _, _)| *name == to)
        .expect("a known shelf");
    let mut taken = Vec::new();
    let mut index = index;
    for &row in moving.rows.iter().rev() {
        taken.push(from.rows.borrow_mut().remove(row));
        if moving.from == to && row < index {
            index -= 1;
        }
    }
    taken.reverse();
    let count = taken.len();
    {
        let mut rows = target.rows.borrow_mut();
        for (at, name) in taken.into_iter().enumerate() {
            rows.insert(index + at, name);
        }
    }
    from.bump();
    target.bump();
    state.set_selected(index..index + count);
}

fn list_of(names: &[&str], state: &ListState) -> (View, Shelf) {
    let shelf = Shelf::new(names);
    let data = ShelfList {
        name: "Samples",
        shelf: shelf.clone(),
    };
    (list_view("Samples", data, state.clone()).into_view(), shelf)
}

const DRUMS: [&str; 8] = [
    "Kick", "Snare", "Clap", "Hat", "Ride", "Crash", "Tom", "Rim",
];

fn row_center(ui: &Harness, name: &str) -> Point {
    ui.get(Role::ListItem, name).center()
}

#[test]
fn clicks_select_one_row_ctrl_adds_and_shift_extends() {
    let state = ListState::new();
    let (view, _shelf) = list_of(&DRUMS, &state);
    let mut ui = Harness::new(view, Size::new(160.0, 120.0), 1.0);
    ui.frame();
    // 120 tall: five rows of 24.
    assert_eq!(
        ui.everything()
            .iter()
            .filter(|f| f.role() == Role::ListItem)
            .count(),
        5
    );
    ui.click(row_center(&ui, "Snare"));
    assert_eq!(state.selected(), vec![1]);
    ui.set_modifiers(CONTROL);
    ui.click(row_center(&ui, "Hat"));
    assert_eq!(state.selected(), vec![1, 3]);
    ui.click(row_center(&ui, "Snare"));
    assert_eq!(state.selected(), vec![3]);
    // Shift extends from the row clicked last, with or without Ctrl.
    ui.set_modifiers(SHIFT);
    ui.click(row_center(&ui, "Kick"));
    assert_eq!(state.selected(), vec![0, 1]);
    ui.click(row_center(&ui, "Hat"));
    assert_eq!(state.selected(), vec![1, 2, 3]);
    ui.set_modifiers(Modifiers::NONE);
    ui.frame();
    let snare = ui.get(Role::ListItem, "Snare");
    assert_eq!(snare.node.state.selected, Some(true));
    assert_eq!(snare.value_text(), Some("2 of 8"));
    assert_eq!(
        ui.get(Role::ListItem, "Kick").node.state.selected,
        Some(false)
    );
    assert_eq!(
        ui.get(Role::List, "Samples").value_text(),
        Some("8 rows, 3 selected")
    );
    // A click on one of several selected rows keeps them all until the
    // release, which leaves only that one.
    ui.press(row_center(&ui, "Clap"));
    assert_eq!(state.selected(), vec![1, 2, 3]);
    ui.release(row_center(&ui, "Clap"));
    assert_eq!(state.selected(), vec![2]);
    ui.assert_accessible();
}

#[test]
fn the_keyboard_moves_selects_extends_and_selects_everything() {
    let state = ListState::new();
    let (view, _shelf) = list_of(&DRUMS, &state);
    let mut ui = Harness::new(view, Size::new(160.0, 120.0), 1.0);
    ui.frame();
    assert!(ui.tab());
    assert!(ui.key(Key::Down));
    assert_eq!(state.selected(), vec![0]);
    assert!(ui.key(Key::Down));
    assert_eq!(state.selected(), vec![1]);
    assert!(ui.key_with(Key::Down, SHIFT));
    assert!(ui.key_with(Key::Down, SHIFT));
    assert_eq!(state.selected(), vec![1, 2, 3]);
    assert!(ui.key_with(Key::Up, SHIFT));
    assert_eq!(state.selected(), vec![1, 2]);
    // Ctrl moves without selecting; Ctrl+Space adds.
    assert!(ui.key_with(Key::Down, CONTROL));
    assert!(ui.key_with(Key::Down, CONTROL));
    assert_eq!(state.cursor(), Some(4));
    assert!(ui.key_with(Key::Character(' '), CONTROL));
    assert_eq!(state.selected(), vec![1, 2, 4]);
    // End scrolls to the last row.
    assert!(ui.key(Key::End));
    ui.frame();
    assert_eq!(state.selected(), vec![7]);
    let rim = ui.get(Role::ListItem, "Rim");
    assert!(rim.is_focused(), "the keyboard is on it");
    assert_eq!(rim.bounds.bottom(), 120.0);
    assert!(ui.find(Role::ListItem, "Kick").is_none());
    assert!(ui.key(Key::Home));
    assert!(ui.key(Key::PageDown));
    assert_eq!(
        state.selected(),
        vec![4],
        "a page is the five rows in sight less one"
    );
    assert!(ui.key_with(Key::Character('a'), CONTROL));
    assert_eq!(state.selected_count(), 8);
}

#[test]
fn a_double_click_and_enter_activate_and_typing_finds_a_row() {
    let state = ListState::new();
    let (view, _shelf) = list_of(&DRUMS, &state);
    let mut ui = Harness::new(view, Size::new(160.0, 120.0), 1.0);
    ui.frame();
    ui.double_click(row_center(&ui, "Clap"));
    assert!(matches!(
        state.take_events().as_slice(),
        [ListEvent::Activated(2)]
    ));
    assert!(ui.key(Key::Enter));
    assert!(matches!(
        state.take_events().as_slice(),
        [ListEvent::Activated(2)]
    ));
    // "r" goes to Ride, then "ri" stays there, and "rim" goes on to Rim.
    assert!(ui.type_text("r"));
    assert_eq!(state.cursor(), Some(4));
    assert!(ui.type_text("im"));
    assert_eq!(state.cursor(), Some(7));
    assert_eq!(state.selected(), vec![7]);
    // After a pause, a new search, from the next row, round to the top.
    ui.advance_ms(1_200);
    assert!(ui.type_text("k"));
    assert_eq!(state.cursor(), Some(0));
}

#[test]
fn assistive_technology_selects_a_row_and_moves_the_keyboard_to_one() {
    let state = ListState::new();
    let (view, _shelf) = list_of(&DRUMS, &state);
    let mut ui = Harness::new(view, Size::new(160.0, 120.0), 1.0);
    ui.frame();
    let hat = ui.get(Role::ListItem, "Hat");
    assert!(hat.node.actions.press && hat.node.actions.focus);
    assert!(ui.act(&hat, manguebyte_ui::Action::Press));
    assert_eq!(state.selected(), vec![3]);
    let ride = ui.get(Role::ListItem, "Ride");
    assert!(ui.act(&ride, manguebyte_ui::Action::Focus));
    assert_eq!(ui.focused().map(|f| f.node.role), Some(Role::List));
    assert_eq!(state.cursor(), Some(4));
    assert_eq!(state.selected(), vec![3], "focus moves, it does not select");
    ui.frame();
    assert!(ui.get(Role::ListItem, "Ride").is_focused());
}

#[test]
fn a_list_in_a_scroll_view_leaves_the_wheel_to_it_at_its_ends() {
    let state = ListState::new();
    let (list, _shelf) = list_of(&DRUMS, &state);
    let view = scroll_view(
        "Page",
        column((list.height(96.0), label("Below").height(300.0))),
    );
    let mut ui = Harness::new(view, Size::new(160.0, 200.0), 1.0);
    ui.frame();
    let list = ui.get(Role::List, "Samples").center();
    // 8 rows of 24 in 96: 96 to go.
    ui.wheel(list, -1.0);
    ui.wheel(list, -2.0);
    assert_eq!(state.scroll_offset(), 96.0);
    assert_eq!(offset_y(&ui, "Page"), 0.0);
    ui.wheel(list, -1.0);
    assert_eq!(offset_y(&ui, "Page"), 40.0);
}

/// A hundred thousand rows that count what is asked of them.
struct Many {
    names: Rc<Cell<usize>>,
    paints: Rc<Cell<usize>>,
}

const MANY: usize = 100_000;

impl ListData for Many {
    fn len(&self) -> usize {
        MANY
    }
    fn name(&self, row: usize) -> String {
        self.names.set(self.names.get() + 1);
        format!("Sample {row:06}")
    }
    fn revision(&self) -> u64 {
        0
    }
    fn paint_row(&self, row: usize, cx: &RowCx<'_>, scene: &mut Scene) {
        self.paints.set(self.paints.get() + 1);
        // A bar as long as the row's number, as cheap as drawing gets.
        let rect = cx.rect();
        let width = rect.width * (row as f32 / MANY as f32);
        scene.fill(
            Rect::new(rect.x, rect.y + 4.0, width, rect.height - 8.0),
            cx.theme().palette.accent,
        );
    }
}

#[test]
fn a_hundred_thousand_rows_cost_what_the_rows_in_sight_cost() {
    let names = Rc::new(Cell::new(0));
    let paints = Rc::new(Cell::new(0));
    let state = ListState::new();
    let data = Many {
        names: names.clone(),
        paints: paints.clone(),
    };
    // At 2x, as rows are drawn on a high-density screen. Most of a
    // frame's time here is drawing the list's pixels, which an unoptimised
    // build does slowly and which do not depend on the number of rows.
    let started = Instant::now();
    let mut ui = Harness::new(
        list_view("Library", data, state.clone()),
        Size::new(240.0, 240.0),
        2.0,
    );
    ui.frame();
    let built = started.elapsed();
    // Ten rows of 24 in 240: only those are drawn and described.
    assert_eq!(paints.get(), 10);
    let items = |ui: &Harness| {
        ui.everything()
            .into_iter()
            .filter(|f| f.role() == Role::ListItem)
            .count()
    };
    assert_eq!(items(&ui), 10);
    let asked = names.get();
    assert!(asked <= 20, "{asked} names asked for");

    // To the end and back, by the keys and the wheel.
    assert!(ui.tab());
    assert!(ui.key(Key::End));
    ui.frame();
    let last = ui.get(Role::ListItem, "Sample 099999");
    assert_eq!(last.bounds.bottom(), 240.0);
    assert_eq!(last.value_text(), Some("100000 of 100000"));
    assert_eq!(
        state.scroll_offset(),
        (MANY as f64) * 24.0 - 240.0,
        "the offset stays exact at 2.4 million pixels"
    );
    paints.set(0);
    names.set(0);
    let scrolling = Instant::now();
    let at = Point::new(100.0, 120.0);
    for _ in 0..50 {
        ui.wheel(at, 0.7);
        ui.frame();
    }
    let scrolled = scrolling.elapsed();
    // Each frame drew the rows in sight, and one more when a row is cut,
    // and asked for no names: nothing described the rows.
    assert!(paints.get() <= 50 * 11, "{} rows drawn", paints.get());
    assert_eq!(names.get(), 0);
    let expected = (MANY as f64) * 24.0 - 240.0 - 50.0 * 28.0;
    assert_eq!(state.scroll_offset(), expected);
    // Describing the list describes only the rows in sight.
    assert_eq!(items(&ui), 11);
    assert!(names.get() <= 11);
    // Every row selected at once is one range, not a hundred thousand.
    let selecting = Instant::now();
    assert!(ui.key_with(Key::Character('a'), CONTROL));
    ui.frame();
    let selected = selecting.elapsed();
    assert_eq!(state.selected_count(), MANY);
    // Nothing changing draws nothing and asks for nothing.
    let (painted, named) = (paints.get(), names.get());
    assert_eq!(ui.frame().painted, 0);
    assert_eq!((paints.get(), names.get()), (painted, named));
    eprintln!(
        "100 000 rows: built and first frame {built:?}, 50 scroll frames {scrolled:?}, \
         select all and its frame {selected:?}"
    );
    // Generous bounds, for an unoptimised build on a busy machine: what
    // matters is that nothing above grows with the number of rows, which
    // the counts show.
    assert!(built < Duration::from_secs(5), "{built:?}");
    assert!(scrolled < Duration::from_secs(30), "{scrolled:?}");
    assert!(selected < Duration::from_secs(5), "{selected:?}");
}

// Drag and drop.

#[test]
fn rows_dragged_within_a_list_are_reordered_where_they_are_dropped() {
    for scale in [1.0, 1.5] {
        let state = ListState::new();
        let (view, shelf) = list_of(&DRUMS[..5], &state);
        let mut ui = Harness::new(view, Size::new(160.0, 150.0), scale);
        ui.frame();
        let kick = row_center(&ui, "Kick");
        ui.press(kick);
        ui.move_to(Point::new(kick.x, kick.y + 8.0));
        ui.frame();
        assert!(ui.root().dragging().is_some());
        let ghost = ui.get(Role::DragItem, "Kick");
        assert!(ghost.bounds.x > kick.x, "beside the pointer");
        // Between Clap (row 2) and Hat (row 3): the line shows there.
        ui.move_to(Point::new(kick.x, 3.0 * 24.0 + 2.0));
        ui.frame();
        assert!(ui.root().drop_target().is_some());
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x while dragging");
        ui.release(Point::new(kick.x, 3.0 * 24.0 + 2.0));
        let events = state.take_events();
        let [ListEvent::Dropped { index, payload }] = events.as_slice() else {
            panic!("one drop: {events:?}");
        };
        assert_eq!(*index, 3);
        assert_eq!(payload.description(), "Kick");
        apply_drop(&[("Samples", &shelf, &state)], "Samples", *index, payload);
        ui.frame();
        assert_eq!(shelf.names(), ["Snare", "Clap", "Kick", "Hat", "Ride"]);
        assert!(
            ui.find(Role::DragItem, "Kick").is_none(),
            "the ghost is gone"
        );
        assert_eq!(
            ui.get(Role::ListItem, "Kick").node.state.selected,
            Some(true)
        );
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after the drop");
    }
}

#[test]
fn selected_rows_move_from_one_list_to_another() {
    let left_state = ListState::new();
    let right_state = ListState::new();
    let left = Shelf::new(&["Kick", "Snare", "Clap", "Hat"]);
    let right = Shelf::new(&["Pad", "Bass"]);
    let view = row((
        list_view(
            "Drums",
            ShelfList {
                name: "Drums",
                shelf: left.clone(),
            },
            left_state.clone(),
        )
        .flex(1.0),
        list_view(
            "Keys",
            ShelfList {
                name: "Keys",
                shelf: right.clone(),
            },
            right_state.clone(),
        )
        .flex(1.0),
    ))
    .spacing(10.0);
    let mut ui = Harness::new(view, Size::new(330.0, 120.0), 1.0);
    ui.frame();
    ui.click(row_center(&ui, "Snare"));
    ui.set_modifiers(CONTROL);
    ui.click(row_center(&ui, "Hat"));
    ui.set_modifiers(Modifiers::NONE);
    assert_eq!(left_state.selected(), vec![1, 3]);
    // Drag both, from Hat, to between Pad and Bass.
    let hat = row_center(&ui, "Hat");
    let between = Point::new(row_center(&ui, "Pad").x, 24.0);
    ui.press(hat);
    ui.move_to(Point::new(hat.x + 10.0, hat.y));
    ui.frame();
    assert!(ui.find(Role::DragItem, "2 samples").is_some());
    ui.move_to(between);
    ui.frame();
    ui.release(between);
    assert!(
        left_state.take_events().is_empty(),
        "the source hears no drop"
    );
    let events = right_state.take_events();
    let [ListEvent::Dropped { index: 1, payload }] = events.as_slice() else {
        panic!("one drop at 1: {events:?}");
    };
    let shelves = [
        ("Drums", &left, &left_state),
        ("Keys", &right, &right_state),
    ];
    apply_drop(&shelves, "Keys", 1, payload);
    ui.frame();
    assert_eq!(left.names(), ["Kick", "Clap"]);
    assert_eq!(right.names(), ["Pad", "Snare", "Hat", "Bass"]);
    assert_eq!(right_state.selected(), vec![1, 2]);
    // The rows are where the application put them, as the lists show.
    assert!(ui.get(Role::ListItem, "Hat").bounds.x > 160.0);
    assert_eq!(
        ui.get(Role::List, "Drums").value_text(),
        Some("2 rows, 0 selected")
    );
    ui.assert_accessible();
}

/// A widget that can be dragged from and dropped on, and writes down what
/// it hears.
struct Probe {
    name: &'static str,
    accepts: bool,
    log: Rc<RefCell<Vec<String>>>,
}

/// What a probe drags.
struct Token;

impl Widget for Probe {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, self.name)
    }
    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(60.0, 40.0)
    }
    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        scene.fill(cx.rect(), cx.theme().palette.surface);
    }
    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match event {
            Event::PointerDown { .. } => {
                cx.capture_pointer();
                Handled::Yes
            }
            Event::PointerMove { .. } if cx.has_pointer() => {
                cx.start_drag(DragPayload::new(Token, format!("{} token", self.name)));
                Handled::Yes
            }
            Event::PointerUp { .. } => {
                self.log.borrow_mut().push(format!("{}: up", self.name));
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
    fn drag(&mut self, _cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let (what, answer) = match event {
            DragEvent::Over { payload, .. } => {
                let yes = self.accepts && payload.is::<Token>();
                // Offers come every frame; only whether it accepts is kept.
                return if yes { Handled::Yes } else { Handled::No };
            }
            DragEvent::Left => ("left".to_owned(), Handled::Yes),
            DragEvent::Drop { .. } => ("drop".to_owned(), Handled::Yes),
            DragEvent::Ended(outcome) => (format!("ended {outcome:?}"), Handled::Yes),
            _ => ("other".to_owned(), Handled::No),
        };
        self.log.borrow_mut().push(format!("{}: {what}", self.name));
        answer
    }
}

fn probes() -> (Harness, Rc<RefCell<Vec<String>>>) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let probe = |name, accepts| Probe {
        name,
        accepts,
        log: log.clone(),
    };
    let view = row((
        probe("Source", false),
        probe("Target", true),
        probe("Refuser", false),
    ))
    .spacing(20.0);
    let mut ui = Harness::new(view, Size::new(240.0, 40.0), 1.0);
    ui.frame();
    (ui, log)
}

fn start(ui: &mut Harness) {
    let source = ui.get(Role::Image, "Source").center();
    ui.press(source);
    ui.move_to(Point::new(source.x + 6.0, source.y));
    ui.frame();
    assert!(ui.find(Role::DragItem, "Source token").is_some());
}

fn taken(log: &Rc<RefCell<Vec<String>>>) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

#[test]
fn a_drop_reaches_the_target_and_the_source_hears_it_was_dropped() {
    let (mut ui, log) = probes();
    start(&mut ui);
    let target = ui.get(Role::Image, "Target");
    ui.move_to(target.center());
    assert_eq!(ui.root().drop_target(), Some(target.id));
    ui.release(target.center());
    assert_eq!(
        taken(&log),
        ["Target: drop", "Source: ended Dropped"],
        "the source never hears the release itself"
    );
    assert!(ui.root().dragging().is_none());
}

#[test]
fn escape_a_release_where_nothing_accepts_and_losing_the_pointer_cancel() {
    let (mut ui, log) = probes();
    // Escape over the target: it hears the drag left, the source that it
    // was cancelled, and nothing is dropped when the button comes up.
    start(&mut ui);
    let target = ui.get(Role::Image, "Target").center();
    ui.move_to(target);
    assert!(ui.key(Key::Escape));
    assert_eq!(taken(&log), ["Target: left", "Source: ended Cancelled"]);
    ui.frame();
    assert!(ui.find(Role::DragItem, "Source token").is_none());
    ui.release(target);
    assert_eq!(taken(&log), Vec::<String>::new());

    // Released over a widget that refuses, and outside the interface.
    for at in [
        ui.get(Role::Image, "Refuser").center(),
        Point::new(500.0, -40.0),
    ] {
        start(&mut ui);
        ui.move_to(at);
        assert_eq!(ui.root().drop_target(), None);
        ui.release(at);
        assert_eq!(taken(&log), ["Source: ended Cancelled"], "{at:?}");
    }

    // Over the target, then off it: it hears it left.
    start(&mut ui);
    ui.move_to(target);
    ui.move_to(Point::new(target.x, 200.0));
    assert_eq!(taken(&log), ["Target: left"]);
    ui.release(Point::new(target.x, 200.0));
    assert_eq!(taken(&log), ["Source: ended Cancelled"]);

    // A second button, the system taking the pointer, the window losing
    // the keyboard.
    let cancels = [
        Event::PointerDown {
            at: target,
            button: Button::Secondary,
            clicks: 1,
            modifiers: Modifiers::NONE,
        },
        Event::PointerCancel,
        Event::Focus(false),
    ];
    for cancel in cancels {
        start(&mut ui);
        ui.move_to(target);
        ui.send(cancel);
        assert_eq!(
            taken(&log),
            ["Target: left", "Source: ended Cancelled"],
            "{cancel:?}"
        );
        assert!(ui.root().dragging().is_none());
        ui.release(target);
        ui.frame();
        assert!(ui.find(Role::DragItem, "Source token").is_none());
        taken(&log);
    }
    // The Escape that cancelled was the interface's: its release too.
    start(&mut ui);
    let escape = |pressed| {
        Event::Key(KeyEvent {
            key: Key::Escape,
            modifiers: Modifiers::NONE,
            pressed,
        })
    };
    assert!(ui.send(escape(true)));
    assert!(ui.send(escape(false)));
    assert!(!ui.send(escape(true)), "no drag, and no widget wants it");
}

#[test]
fn the_ghost_is_drawn_over_everything_and_moving_it_damages_only_where_it_was_and_is() {
    for scale in [1.0, 1.25, 2.0] {
        let log = Rc::new(RefCell::new(Vec::new()));
        let view = row((
            Probe {
                name: "Source",
                accepts: false,
                log: log.clone(),
            },
            label("Neighbour").flex(1.0),
        ));
        let mut ui = Harness::new(view, Size::new(240.0, 80.0), scale);
        ui.frame();
        start(&mut ui);
        let neighbour = ui.get(Role::Label, "Neighbour");
        let mut at = ui.get(Role::Image, "Source").center();
        for step in 0..6 {
            at = Point::new(at.x + 17.3, at.y + 3.1 - step as f32);
            ui.move_to(at);
            let before = ui.get(Role::DragItem, "Source token").device;
            let summary = ui.frame();
            let after = ui.get(Role::DragItem, "Source token").device;
            // Only the ghost moved.
            assert_eq!(summary.painted, 1, "{scale}x");
            assert!(
                damage_inside(&ui, before.union(&after)),
                "{scale}x: {:?}",
                ui.last_damage()
            );
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x step {step}");
        }
        // Over the neighbour, the ghost is what shows.
        let ghost = ui.get(Role::DragItem, "Source token");
        assert!(ghost.bounds.intersect(&neighbour.bounds).is_some());
        let last = ui.everything().pop().expect("widgets");
        assert_eq!(last.role(), Role::DragItem, "drawn last, over everything");
        ui.release(at);
        ui.frame();
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after");
    }
}

#[test]
fn a_drag_near_a_scroll_views_edge_scrolls_it_and_stops_away_from_the_edge() {
    let log = Rc::new(RefCell::new(Vec::new()));
    let view = row((
        Probe {
            name: "Source",
            accepts: false,
            log: log.clone(),
        },
        scroll_view("Rows", rows(40)).flex(1.0),
    ));
    let mut ui = Harness::new(view, Size::new(240.0, 120.0), 1.0);
    ui.frame();
    start(&mut ui);
    let view = ui.get(Role::ScrollView, "Rows");
    let viewport = metrics(&ui, &view).viewport;
    // Near the bottom edge, held still: it scrolls on its own, frame after
    // frame, faster nearer the edge.
    ui.move_to(Point::new(viewport.center().x, viewport.bottom() - 12.0));
    ui.advance_ms(500);
    let slow = offset_y(&ui, "Rows");
    assert!(slow > 0.0);
    ui.move_to(Point::new(viewport.center().x, viewport.bottom() - 1.0));
    ui.advance_ms(500);
    let fast = offset_y(&ui, "Rows") - slow;
    assert!(fast > slow, "{slow} then {fast}");
    // In the middle, it stops.
    ui.move_to(viewport.center());
    ui.frame();
    let stopped = offset_y(&ui, "Rows");
    ui.advance_ms(300);
    assert_eq!(offset_y(&ui, "Rows"), stopped);
    // Near the top, back up, until the top.
    ui.move_to(Point::new(viewport.center().x, viewport.y + 2.0));
    ui.advance_ms(3_000);
    assert_eq!(offset_y(&ui, "Rows"), 0.0);
    ui.release(viewport.center());
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_drag_near_a_lists_edge_scrolls_the_list() {
    let names: Vec<String> = (0..50).map(|i| format!("Take {i}")).collect();
    let names: Vec<&str> = names.iter().map(String::as_str).collect();
    let state = ListState::new();
    let (view, _shelf) = list_of(&names, &state);
    let mut ui = Harness::new(view, Size::new(160.0, 120.0), 1.0);
    ui.frame();
    let first = row_center(&ui, "Take 0");
    ui.press(first);
    ui.move_to(Point::new(first.x, first.y + 10.0));
    ui.move_to(Point::new(first.x, 115.0));
    ui.advance_ms(400);
    let scrolled = state.scroll_offset();
    assert!(scrolled > 0.0);
    ui.move_to(Point::new(first.x, 60.0));
    ui.frame();
    let stopped = state.scroll_offset();
    ui.advance_ms(200);
    assert_eq!(state.scroll_offset(), stopped);
    // The drop goes where the rows now are under the pointer.
    ui.release(Point::new(first.x, 60.0));
    let events = state.take_events();
    let [ListEvent::Dropped { index, .. }] = events.as_slice() else {
        panic!("{events:?}");
    };
    assert_eq!(*index, ((60.0 + stopped) / 24.0).round() as usize);
}

// Images.

/// A picture of `width` by `height` in which each pixel has its own colour.
fn pattern(width: u32, height: u32) -> Pixmap {
    let mut rgba = Vec::new();
    for y in 0..height {
        for x in 0..width {
            rgba.extend_from_slice(&[
                (x * 40) as u8,
                (y * 40) as u8,
                ((x + y) % 2 * 255) as u8,
                255,
            ]);
        }
    }
    Pixmap::from_straight(width, height, &rgba).expect("a picture")
}

#[test]
fn a_picture_made_for_twice_the_scale_is_copied_pixel_for_pixel_at_scale_two() {
    let picture = pattern(6, 6);
    let view = image("Logo", Image::new(picture.clone()))
        .density(2.0)
        .fit(Fit::None);
    let mut ui = Harness::new(view, Size::new(10.0, 10.0), 2.0);
    ui.frame();
    let logo = ui.get(Role::Image, "Logo");
    assert_eq!(logo.bounds, Rect::new(0.0, 0.0, 10.0, 10.0));
    // 3 by 3 logical pixels, centred in 10: from 3.5, which is device 7.
    for y in 0..6 {
        for x in 0..6 {
            assert_eq!(
                ui.pixels().pixel(7 + x, 7 + y),
                picture.pixel(x, y),
                "({x}, {y})"
            );
        }
    }
    // Around it, the background.
    let background = Color::hex(0x16181C);
    assert_eq!(
        ui.pixels().pixel(6, 6).map(|p| p.to_array()),
        Some([background.r, background.g, background.b, 255])
    );
    ui.assert_accessible();
}

#[test]
fn the_version_of_a_picture_that_suits_the_scale_is_drawn() {
    let plain = |color: Color, side: u32| {
        let mut pixels = Pixmap::new(side, side).expect("small");
        pixels.fill(color);
        Image::new(pixels)
    };
    let red = Color::hex(0xFF0000);
    let blue = Color::hex(0x0000FF);
    for (scale, expected) in [(1.0, red), (1.5, blue), (2.0, blue), (3.0, blue)] {
        let view = image("Mark", plain(red, 8))
            .with_density(plain(blue, 16), 2.0)
            .fit(Fit::Stretch);
        let mut ui = Harness::new(view, Size::new(8.0, 8.0), scale);
        ui.frame();
        let center = (4.0 * scale) as u32;
        assert_eq!(
            ui.pixels().pixel(center, center).map(|p| p.to_array()),
            Some([expected.r, expected.g, expected.b, 255]),
            "{scale}x"
        );
    }
}

#[test]
fn a_covering_picture_is_cut_at_its_rectangle_and_a_contained_one_leaves_room() {
    let wide = || {
        let mut pixels = Pixmap::new(40, 10).expect("small");
        pixels.fill(Color::hex(0xFF0000));
        Image::new(pixels)
    };
    let background = Color::hex(0x16181C);
    let is = |ui: &Harness, x: u32, y: u32, color: Color| {
        ui.pixels().pixel(x, y).map(|p| p.to_array()) == Some([color.r, color.g, color.b, 255])
    };
    let red = Color::hex(0xFF0000);
    // Cover: fills the 20 by 20 square, cut at its edges.
    let view = row((
        image("Cover", wide()).fit(Fit::Cover).width(20.0),
        label("Next").flex(1.0),
    ));
    let mut ui = Harness::new(view, Size::new(60.0, 20.0), 1.0);
    ui.frame();
    assert!(is(&ui, 1, 1, red) && is(&ui, 18, 18, red));
    assert!(is(&ui, 30, 1, background), "nothing past its rectangle");
    // Contain: the whole of it, 20 by 5, with room above and below.
    let view = row((
        image("Contain", wide()).fit(Fit::Contain).width(20.0),
        label("Next").flex(1.0),
    ));
    let mut ui = Harness::new(view, Size::new(60.0, 20.0), 1.0);
    ui.frame();
    assert!(is(&ui, 1, 10, red) && is(&ui, 18, 10, red));
    assert!(is(&ui, 10, 2, background) && is(&ui, 10, 17, background));
    ui.assert_accessible();
}
