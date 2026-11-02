//! `responsive`: which child shows at which room, the margin both ways,
//! what stays across a switch, and that no frame shows the wrong child.
//!
//! The goldens are blessed as those of `tests/golden.rs` are.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use manguebyte_gfx::snapshot::Snapshots;
use manguebyte_gfx::{Color, Scene, Size};
use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::editor::testing::HostCall;
use manguebyte_ui::access::AccessNode;
use manguebyte_ui::layout::{Align, Arrange, Style};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    AccessCx, IntoView, MeasureCx, PaintCx, ParamHandle, RESPONSIVE_MARGIN, Role, Shown, State,
    UpdateCx, View, Widget, card, column, combo, knob, label, responsive, row, scroll_view, tabs,
    text_field,
};

/// How often a [`Probe`] was asked to do things, and whether it was dropped.
#[derive(Default)]
struct Counts {
    paints: Cell<u32>,
    updates: Cell<u32>,
    drops: Cell<u32>,
}

/// A widget that counts what is asked of it.
struct Probe {
    name: &'static str,
    counts: Rc<Counts>,
}

impl Probe {
    fn new(name: &'static str) -> (Probe, Rc<Counts>) {
        let counts = Rc::new(Counts::default());
        (
            Probe {
                name,
                counts: Rc::clone(&counts),
            },
            counts,
        )
    }
}

impl Widget for Probe {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, self.name)
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(20.0, 20.0)
    }

    fn update(&mut self, _cx: &mut UpdateCx<'_>) {
        self.counts.updates.set(self.counts.updates.get() + 1);
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        self.counts.paints.set(self.counts.paints.get() + 1);
        scene.fill(cx.rect(), Color::rgb(40, 90, 200));
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.drops.set(self.counts.drops.get() + 1);
    }
}

fn shows(ui: &Harness, name: &str) -> bool {
    ui.find(Role::Label, name).is_some()
}

/// Three layouts by width and height.
fn three() -> View {
    responsive((label("Large"), label("Medium"), label("Small")))
        .below(Size::new(800.0, 400.0))
        .below(Size::new(500.0, 200.0))
        .into_view()
}

#[test]
fn the_first_child_that_fits_shows_at_each_size() {
    for (width, height, name) in [
        (1000.0, 600.0, "Large"),
        (800.0, 400.0, "Large"),
        (799.0, 600.0, "Medium"),
        (1000.0, 399.0, "Medium"),
        (500.0, 200.0, "Medium"),
        (499.0, 600.0, "Small"),
        (1000.0, 199.0, "Small"),
        (20.0, 20.0, "Small"),
    ] {
        // On the first frame: there is no earlier child to hold on to.
        let mut ui = Harness::new(three(), Size::new(width, height), 1.0);
        ui.frame();
        for other in ["Large", "Medium", "Small"] {
            assert_eq!(
                shows(&ui, other),
                other == name,
                "{other} at {width} by {height}"
            );
        }
    }
}

#[test]
fn the_margin_holds_a_child_on_the_way_down_and_nothing_on_the_way_up() {
    let height = 600.0;
    let mut ui = Harness::new(three(), Size::new(900.0, height), 1.0);
    let at = |ui: &mut Harness, width: f32, name: &str| {
        ui.resize(Size::new(width, height), 1.0);
        ui.frame();
        assert!(shows(ui, name), "{name} at a width of {width}");
    };
    // Down: the child showing stays until the margin is passed.
    at(&mut ui, 800.0, "Large");
    at(&mut ui, 800.0 - RESPONSIVE_MARGIN, "Large");
    at(&mut ui, 800.0 - RESPONSIVE_MARGIN - 1.0, "Medium");
    // Back and forth inside the margin: no change either way.
    at(&mut ui, 795.0, "Medium");
    at(&mut ui, 799.0, "Medium");
    // Up: it comes in at its smallest size, not past it.
    at(&mut ui, 800.0, "Large");
    at(&mut ui, 795.0, "Large");
    at(&mut ui, 790.0, "Large");

    // A margin of its own, and none.
    let view = responsive((label("Wide"), label("Narrow")))
        .below(Size::new(300.0, 0.0))
        .margin(50.0);
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    let at = |ui: &mut Harness, width: f32, name: &str| {
        ui.resize(Size::new(width, 100.0), 1.0);
        ui.frame();
        assert!(shows(ui, name), "{name} at a width of {width}");
    };
    at(&mut ui, 400.0, "Wide");
    at(&mut ui, 250.0, "Wide");
    at(&mut ui, 249.0, "Narrow");
    let view = responsive((label("Wide"), label("Narrow")))
        .below(Size::new(300.0, 0.0))
        .margin(0.0);
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    at(&mut ui, 300.0, "Wide");
    at(&mut ui, 299.0, "Narrow");
    at(&mut ui, 300.0, "Wide");
}

#[test]
fn state_stays_across_a_switch() {
    let text = State::new(String::new());
    let rows: Vec<_> = (0..40)
        .map(|i| label(format!("Row {i}")).height(20.0))
        .collect();
    let list = scroll_view("Rows", column(rows));
    let wide = column((text_field("Title", text.clone()), list.flex(1.0)));
    let view = responsive((wide, label("Summary"))).below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 200.0), 1.0);
    ui.frame();

    let field = ui.get(Role::TextInput, "Title");
    ui.click(field.center());
    ui.type_text("Fugue");
    let rows = ui.get(Role::ScrollView, "Rows");
    ui.wheel(rows.center(), -3.0);
    ui.frame();
    let scrolled = ui.root().scroll_metrics(rows.id).expect("scrolls").offset;
    assert!(scrolled.y > 0.0, "{scrolled:?}");

    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Summary"));
    assert!(ui.find(Role::TextInput, "Title").is_none());

    ui.resize(Size::new(400.0, 200.0), 1.0);
    ui.frame();
    assert_eq!(ui.get(Role::TextInput, "Title").value_text(), Some("Fugue"));
    assert_eq!(text.get(), "Fugue", "the input took what was typed");
    let rows = ui.get(Role::ScrollView, "Rows");
    assert_eq!(
        ui.root().scroll_metrics(rows.id).expect("scrolls").offset,
        scrolled
    );
}

#[test]
fn nothing_is_built_again_and_a_hidden_child_costs_nothing() {
    let (wide, wide_counts) = Probe::new("Wide");
    let (narrow, narrow_counts) = Probe::new("Narrow");
    let view = responsive((wide, narrow)).below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    ui.frame();
    let widgets = ui.root().len();
    let (wide_id, narrow_id) = (ui.get(Role::Image, "Wide").id, {
        ui.resize(Size::new(200.0, 100.0), 1.0);
        ui.frame();
        ui.get(Role::Image, "Narrow").id
    });

    // A border dragged over the boundary many times.
    for round in 0..50 {
        let width = if round % 2 == 0 { 400.0 } else { 200.0 };
        ui.resize(Size::new(width, 100.0), 1.0);
        ui.frame();
    }
    ui.resize(Size::new(400.0, 100.0), 1.0);
    ui.frame();
    assert_eq!(ui.root().len(), widgets);
    assert_eq!(wide_counts.drops.get(), 0, "built again");
    assert_eq!(narrow_counts.drops.get(), 0, "built again");
    assert_eq!(ui.get(Role::Image, "Wide").id, wide_id);

    // Hidden, the other is not polled or drawn.
    let (paints, updates) = (narrow_counts.paints.get(), narrow_counts.updates.get());
    for _ in 0..5 {
        ui.advance_ms(16);
    }
    assert_eq!(narrow_counts.paints.get(), paints);
    assert_eq!(narrow_counts.updates.get(), updates);
    assert!(ui.root().bounds(narrow_id).is_some(), "still there");
}

#[test]
fn no_frame_shows_the_wrong_child() {
    let (wide, wide_counts) = Probe::new("Wide");
    let (narrow, narrow_counts) = Probe::new("Narrow");
    let view = responsive((wide, narrow)).below(Size::new(300.0, 0.0));

    // The first frame in a small window never draws the first child.
    let mut ui = Harness::new(view, Size::new(200.0, 100.0), 1.0);
    ui.frame();
    assert_eq!(
        wide_counts.paints.get(),
        0,
        "drawn in a room it does not fit"
    );
    assert_eq!(narrow_counts.paints.get(), 1);

    // Each way: the one frame after the resize draws the right child, and
    // what the back buffer shows is the scene whole.
    ui.resize(Size::new(400.0, 100.0), 1.0);
    let narrow_paints = narrow_counts.paints.get();
    ui.frame();
    assert_eq!(narrow_counts.paints.get(), narrow_paints, "the old child");
    assert_eq!(wide_counts.paints.get(), 1);
    assert!(ui.pixels() == &ui.render_fresh());
    assert!(ui.dump().contains("rect(0 0 400 100)"));

    ui.resize(Size::new(200.0, 100.0), 1.0);
    let wide_paints = wide_counts.paints.get();
    ui.frame();
    assert_eq!(wide_counts.paints.get(), wide_paints, "the old child");
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_child_that_comes_in_is_polled_in_the_same_frame() {
    // Tabs say which page they show when they are polled: with no poll,
    // both pages would be drawn on the frame the compact layout comes in.
    let view = responsive((
        label("Wide"),
        tabs("Sections", State::new(1_usize))
            .page("One", label("First page"))
            .page("Two", label("Second page")),
    ))
    .below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 200.0), 1.0);
    ui.frame();
    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Second page"));
    assert!(!shows(&ui, "First page"));
    assert!(!ui.dump().contains("First page"), "{}", ui.dump());

    // So is a responsive inside it.
    let inner =
        responsive((label("Inner wide"), label("Inner narrow"))).below(Size::new(150.0, 0.0));
    let view = responsive((label("Outer wide"), inner)).below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Outer wide"));
    ui.resize(Size::new(100.0, 200.0), 1.0);
    let summary = ui.frame();
    assert!(summary.laid_out);
    assert!(shows(&ui, "Inner narrow"));
    assert!(!shows(&ui, "Inner wide"));
    assert!(!shows(&ui, "Outer wide"));
    assert!(ui.pixels() == &ui.render_fresh());
    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Inner wide"));
}

#[test]
fn it_decides_from_the_room_it_is_given_not_the_window() {
    // A sidebar takes 150 of the window, so 400 leaves 250.
    let view = row((
        label("Sidebar").width(150.0),
        card(responsive((label("Wide"), label("Narrow"))).below(Size::new(280.0, 0.0))).flex(1.0),
    ));
    let mut ui = Harness::new(view, Size::new(500.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Wide"), "500 less 150 less the card's padding");
    ui.resize(Size::new(400.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Narrow"), "a window wider than 280, a room less");

    // Its own padding is not room for its children.
    let view = responsive((label("Wide"), label("Narrow")))
        .below(Size::new(300.0, 0.0))
        .margin(0.0)
        .padding(30.0);
    let mut ui = Harness::new(view, Size::new(350.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Narrow"));
    ui.resize(Size::new(360.0, 200.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Wide"));
}

#[test]
fn a_parent_that_gives_only_its_natural_size_does_not_strand_it() {
    // In a column, with no weight, the height it is given follows its
    // natural size. Were that the small child's, it could never come back.
    let compact = label("Compact");
    let view = column((
        responsive((label("Full").min_size(Size::new(0.0, 400.0)), compact))
            .below(Size::new(0.0, 300.0)),
        label("Footer").height(20.0),
    ));
    let mut ui = Harness::new(view, Size::new(200.0, 500.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Full"));
    ui.resize(Size::new(200.0, 250.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Compact"));
    assert!(ui.get(Role::Label, "Compact").bounds.height < 300.0);
    ui.resize(Size::new(200.0, 500.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Full"), "stranded in the small child");
    assert_eq!(ui.get(Role::Label, "Full").bounds.height, 400.0);
}

#[test]
fn a_room_of_exactly_the_smallest_size_fits_at_any_scale() {
    // Layout snaps to device pixels, so a room of 1601 can come out a
    // fraction short of it.
    for scale in [0.5_f32, 0.7, 0.77, 0.8125, 1.0, 1.25, 1.3, 1.37, 2.0] {
        let view = responsive((label("Wide"), label("Narrow")))
            .below(Size::new(1601.0, 979.0))
            .margin(0.0);
        let mut ui = Harness::new(view, Size::new(1601.0, 979.0), scale);
        ui.frame();
        assert!(shows(&ui, "Wide"), "at {scale}");
        // And the frame after a shrink and back.
        ui.resize(Size::new(800.0, 979.0), scale);
        ui.frame();
        assert!(shows(&ui, "Narrow"), "at {scale}");
        ui.resize(Size::new(1601.0, 979.0), scale);
        ui.frame();
        assert!(shows(&ui, "Wide"), "back at {scale}");
    }
}

#[test]
fn a_window_with_no_area_decides_nothing() {
    let mut ui = Harness::new(three(), Size::new(1000.0, 600.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Large"));
    ui.resize(Size::new(0.0, 0.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "Large"), "minimised is not small");
}

#[test]
fn assistive_technology_sees_only_the_shown_child() {
    let view = responsive((
        column((label("Wide title"), label("Wide note"))),
        column((label("Narrow title"),)),
    ))
    .below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 200.0), 1.0);
    ui.frame();
    ui.assert_accessible();
    let names = |ui: &Harness| -> Vec<String> {
        ui.everything()
            .iter()
            .filter(|found| found.role() == Role::Label)
            .map(|found| found.name().to_owned())
            .collect()
    };
    assert_eq!(names(&ui), ["Wide title", "Wide note"]);
    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    ui.assert_accessible();
    assert_eq!(names(&ui), ["Narrow title"]);
}

#[test]
fn a_quiet_frame_lays_out_and_paints_nothing() {
    let (wide, wide_counts) = Probe::new("Wide");
    let view = responsive((wide, label("Narrow"))).below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    assert!(ui.frame().damaged > 0);
    let paints = wide_counts.paints.get();
    for _ in 0..5 {
        ui.advance_ms(16);
        let summary = ui.frame();
        assert_eq!(summary.damaged, 0);
        assert_eq!(summary.painted, 0);
        assert!(!summary.laid_out);
    }
    assert_eq!(wide_counts.paints.get(), paints);
    // After a switch too.
    ui.resize(Size::new(200.0, 100.0), 1.0);
    assert!(ui.frame().laid_out);
    let summary = ui.frame();
    assert_eq!((summary.damaged, summary.laid_out), (0, false));
}

#[test]
fn a_switch_inside_a_steady_window_draws_only_what_changed() {
    // The room changes through the program: a row's other child grows.
    let wide = State::new(100.0_f32);
    struct Grows(State<f32>, f32);
    impl Widget for Grows {
        fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
            AccessNode::new(Role::Image, "Other")
        }
        fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
            Size::new(self.1, 20.0)
        }
        fn update(&mut self, cx: &mut UpdateCx<'_>) {
            if self.0.get() != self.1 {
                self.1 = self.0.get();
                cx.request_layout();
            }
        }
        fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
    }
    let (first, _first_counts) = Probe::new("First");
    let (second, _second_counts) = Probe::new("Second");
    let view = column((
        row((
            Grows(wide.clone(), 100.0).into_view(),
            responsive((first, second))
                .below(Size::new(200.0, 0.0))
                .flex(1.0),
        ))
        .height(50.0),
        label("Below").height(50.0),
    ));
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    ui.frame();
    assert!(ui.find(Role::Image, "First").is_some());
    let before = ui.pixels().clone();
    wide.set(300.0);
    ui.frame();
    assert!(ui.find(Role::Image, "First").is_none());
    assert!(ui.find(Role::Image, "Second").is_some());
    // The row was drawn again, not what is below it, and the buffer holds
    // what a whole render does.
    let damage = ui.last_damage_bounds().expect("something changed");
    assert!(damage.bottom() <= 50, "{damage:?}");
    assert!(ui.pixels() == &ui.render_fresh());
    assert_ne!(before.data(), ui.pixels().data());
}

#[test]
fn the_focus_goes_with_a_hidden_child_and_tab_finds_the_one_that_came() {
    let text = State::new(String::new());
    let wide = text_field("Wide field", text.clone());
    let narrow = text_field("Narrow field", text.clone());
    let view = responsive((wide, narrow)).below(Size::new(300.0, 0.0));
    let mut ui = Harness::new(view, Size::new(400.0, 100.0), 1.0);
    ui.frame();
    ui.click(ui.get(Role::TextInput, "Wide field").center());
    ui.type_text("Tierce");
    assert_eq!(
        ui.focused().map(|f| f.name().to_owned()).as_deref(),
        Some("Wide field")
    );
    assert!(ui.root().wants_text());

    ui.resize(Size::new(200.0, 100.0), 1.0);
    ui.frame();
    assert_eq!(ui.focused(), None, "the focus goes nowhere");
    assert!(!ui.root().wants_text(), "typing ended");
    assert_eq!(text.get(), "Tierce", "what was typed was taken");
    // What is bound to the same state shows the same in the other child.
    assert_eq!(
        ui.get(Role::TextInput, "Narrow field").value_text(),
        Some("Tierce")
    );
    ui.tab();
    ui.frame();
    assert_eq!(
        ui.focused().map(|f| f.name().to_owned()).as_deref(),
        Some("Narrow field")
    );
}

struct Params {
    gain: FloatParam,
    shape: manguebyte_params::ChoiceParam,
}
param_set!(Params { gain, shape });

fn params() -> Arc<Params> {
    Arc::new(Params {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5),
        shape: manguebyte_params::ChoiceParam::new("shape", "Shape", &["Sine", "Saw"], 0),
    })
}

#[test]
fn a_hidden_child_gives_up_the_pointer_and_its_menu() {
    // A knob held while the window changes under it: its gesture ends.
    let params = params();
    let view = responsive((
        knob(ParamHandle::new(Arc::clone(&params), |p| &p.gain)),
        label("Narrow"),
    ))
    .below(Size::new(300.0, 0.0));
    let mut ui = Harness::with_params(
        view,
        Size::new(400.0, 200.0),
        1.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.frame();
    let at = ui.get(Role::Slider, "Gain").center();
    ui.press(at);
    ui.take_calls();
    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    assert_eq!(ui.calls(), [HostCall::EndGesture(params.gain.param_id())]);
    assert_eq!(ui.root().pointer_holder(), None);
    ui.release(at);

    // A menu open when the layout goes closes with it, and is not left
    // hanging from a widget that is not there.
    let view = responsive((
        combo(ParamHandle::new(Arc::clone(&params), |p| &p.shape)),
        label("Narrow"),
    ))
    .below(Size::new(300.0, 0.0));
    let mut ui = Harness::with_params(
        view,
        Size::new(400.0, 200.0),
        1.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.frame();
    ui.click(ui.get(Role::ComboBox, "Shape").center());
    ui.frame();
    assert!(ui.root().has_menu());
    ui.resize(Size::new(200.0, 200.0), 1.0);
    ui.frame();
    assert!(!ui.root().has_menu());
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_widget_of_ones_own_can_choose_its_children_by_room() {
    /// Shows every child up to as many as fit at 100 pixels each.
    struct Columns;
    impl Widget for Columns {
        fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
            AccessNode::new(Role::Group, "Columns")
        }
        fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
            Size::default()
        }
        fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
        fn adapt(&mut self, _cx: &MeasureCx<'_>, room: Size) -> Option<Shown> {
            Some(Shown::First((room.width / 100.0) as usize))
        }
    }
    let style = Style {
        arrange: Arrange::Adaptive,
        align: Align::Start,
        ..Style::leaf()
    };
    let view = View::with_children(Columns, style, (label("A"), label("B"), label("C")));
    let mut ui = Harness::new(view, Size::new(250.0, 40.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "A") && shows(&ui, "B") && !shows(&ui, "C"));
    ui.resize(Size::new(350.0, 40.0), 1.0);
    ui.frame();
    assert!(shows(&ui, "C"));
}

fn snapshots() -> Snapshots {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-golden-failures");
    Snapshots::new(dir, failures).with_tolerance(1)
}

#[test]
fn each_layout_matches_its_golden() {
    let view = || {
        responsive((
            row((
                card(label("Great").heading()).flex(1.0),
                card(label("Swell").heading()).flex(1.0),
                card(label("Pedal").heading()).flex(1.0),
            ))
            .spacing(8.0)
            .padding(8.0),
            column((
                label("Great: 8 stops"),
                label("Swell: 6 stops"),
                label("Pedal: 4 stops"),
            ))
            .spacing(4.0)
            .padding(8.0),
        ))
        .below(Size::new(320.0, 0.0))
    };
    let snapshots = snapshots();
    for (name, width) in [("responsive_wide", 400.0), ("responsive_compact", 240.0)] {
        // Reached by a switch, and straight: the same pixels.
        let mut ui = Harness::new(
            view(),
            Size::new(if width > 300.0 { 240.0 } else { 400.0 }, 120.0),
            1.0,
        );
        ui.frame();
        ui.resize(Size::new(width, 120.0), 1.0);
        ui.frame();
        let mut straight = Harness::new(view(), Size::new(width, 120.0), 1.0);
        straight.frame();
        assert!(ui.pixels() == straight.pixels(), "{name}");
        assert_eq!(ui.dump(), straight.dump(), "{name}");
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
}
