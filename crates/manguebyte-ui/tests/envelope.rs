//! The envelope editor: handles dragged as gestures, curves, typed values,
//! the keyboard, assistive technology, automation, and drawing that a
//! partial repaint reproduces exactly at every scale.

use std::sync::Arc;

use manguebyte_gfx::{Point, Size};
use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::editor::testing::HostCall;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, EnvelopeEditor, IntoView, Key, ParamHandle, Role, envelope, label, row,
};

struct Params {
    attack: FloatParam,
    decay: FloatParam,
    sustain: FloatParam,
    release: FloatParam,
    attack_curve: FloatParam,
    release_curve: FloatParam,
}

param_set!(Params {
    attack,
    decay,
    sustain,
    release,
    attack_curve,
    release_curve
});

fn params() -> Arc<Params> {
    let time = |id, name, ms| {
        FloatParam::new(id, name, FloatRange::linear(0.0, 1000.0), ms)
            .with_unit("ms")
            .with_precision(0)
    };
    Arc::new(Params {
        attack: time("attack", "Attack", 100.0),
        decay: time("decay", "Decay", 300.0),
        sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 50.0)
            .with_unit("%")
            .with_precision(0),
        release: time("release", "Release", 500.0),
        attack_curve: FloatParam::new(
            "attack_curve",
            "Attack curve",
            FloatRange::linear(-1.0, 1.0),
            0.0,
        )
        .with_precision(2),
        release_curve: FloatParam::new(
            "release_curve",
            "Release curve",
            FloatRange::linear(-1.0, 1.0),
            0.0,
        )
        .with_precision(2),
    })
}

fn editor(params: &Arc<Params>) -> EnvelopeEditor {
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(params), field);
    envelope(
        "Amp envelope",
        handle(|p| &p.attack),
        handle(|p| &p.decay),
        handle(|p| &p.sustain),
        handle(|p| &p.release),
    )
}

/// The editor alone at 336 by 140: a graph 320 wide (80 a time), from
/// (8, 8), as tall as [`height`] says.
fn open(scale: f32) -> (Harness, Arc<Params>) {
    let params = params();
    let mut ui = Harness::with_params(
        editor(&params),
        Size::new(336.0, 140.0),
        scale,
        params.clone(),
    );
    ui.frame();
    (ui, params)
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

fn handle_point(ui: &Harness, name: &str) -> Point {
    ui.get(Role::Slider, name).center()
}

/// The height of the graph: the release ends on its bottom edge, which is
/// as far down as a level's whole range.
fn height(ui: &Harness) -> f32 {
    handle_point(ui, "Release").y - 8.0
}

#[test]
fn the_peak_drags_the_attack_as_one_gesture() {
    let (mut ui, params) = open(1.0);
    ui.take_calls();
    let peak = Point::new(16.0, 8.0);
    ui.drag(peak, (40.0, 30.0), 4);
    assert!(
        near(params.attack.value(), 600.0),
        "{}",
        params.attack.value()
    );
    assert!(
        near(params.sustain.value(), 50.0),
        "the peak moves only the attack"
    );
    let id = params.attack.param_id();
    let calls = ui.take_calls();
    assert_eq!(calls.first(), Some(&HostCall::BeginGesture(id)));
    assert_eq!(calls.last(), Some(&HostCall::EndGesture(id)));
    assert!(calls.iter().all(|call| match call {
        HostCall::BeginGesture(p) | HostCall::EndGesture(p) | HostCall::Perform(p, _) => *p == id,
        _ => false,
    }));
    // A click that does not move: a begin and an end, nothing between.
    ui.frame();
    let peak = handle_point(&ui, "Attack");
    ui.click(peak);
    assert_eq!(
        ui.take_calls(),
        [HostCall::BeginGesture(id), HostCall::EndGesture(id)]
    );
}

#[test]
fn the_end_of_the_decay_moves_the_decay_and_the_sustain_together() {
    let (mut ui, params) = open(1.0);
    ui.take_calls();
    // The decay ends 0.3 of 80 after the peak, at half the height.
    let h = height(&ui);
    let corner = Point::new(8.0 + 8.0 + 24.0, 8.0 + h * 0.5);
    ui.drag(corner, (-8.0, -h * 0.25), 2);
    assert!(
        near(params.decay.value(), 200.0),
        "{}",
        params.decay.value()
    );
    assert!(
        near(params.sustain.value(), 75.0),
        "{}",
        params.sustain.value()
    );
    let (decay, sustain) = (params.decay.param_id(), params.sustain.param_id());
    let calls = ui.take_calls();
    assert_eq!(
        calls[..2],
        [
            HostCall::BeginGesture(decay),
            HostCall::BeginGesture(sustain)
        ]
    );
    assert_eq!(
        calls[calls.len() - 2..],
        [HostCall::EndGesture(decay), HostCall::EndGesture(sustain)]
    );
    // The end of the sustain moves only the sustain, up and down.
    ui.frame();
    let held = handle_point(&ui, "Sustain");
    ui.drag(held, (30.0, h * 0.5), 2);
    assert!(near(params.sustain.value(), 25.0));
    assert!(near(params.decay.value(), 200.0));
    // The release's end, sideways, with Shift ten times finer.
    ui.frame();
    let end = handle_point(&ui, "Release");
    ui.set_modifiers(manguebyte_ui::Modifiers {
        shift: true,
        ..manguebyte_ui::Modifiers::NONE
    });
    ui.drag(end, (40.0, 0.0), 2);
    assert!(
        near(params.release.value(), 550.0),
        "{}",
        params.release.value()
    );
}

#[test]
fn escape_puts_back_what_a_drag_changed() {
    let (mut ui, params) = open(1.0);
    let corner = handle_point(&ui, "Decay");
    ui.press(corner);
    ui.move_to(Point::new(corner.x + 20.0, corner.y + 20.0));
    assert!(!near(params.decay.value(), 300.0));
    assert!(ui.key(Key::Escape));
    ui.release(Point::new(corner.x + 20.0, corner.y + 20.0));
    assert!(near(params.decay.value(), 300.0));
    assert!(near(params.sustain.value(), 50.0));
}

#[test]
fn curves_bend_their_segments_and_are_dragged_up_and_down() {
    let params = params();
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(&params), field);
    let view = editor(&params)
        .attack_curve(handle(|p| &p.attack_curve))
        .release_curve(handle(|p| &p.release_curve));
    let mut ui = Harness::with_params(view, Size::new(336.0, 140.0), 1.0, params.clone());
    ui.frame();
    let straight = ui.dump_of(&ui.get(Role::Group, "Amp envelope"));
    let bend = ui.get(Role::Slider, "Release curve");
    assert_eq!(bend.value_text(), Some("0.00"));
    // Up raises the curve: a quarter of the height, a quarter of its range.
    ui.drag(bend.center(), (0.0, -height(&ui) * 0.25), 2);
    assert!(
        near(params.release_curve.value(), 0.5),
        "{}",
        params.release_curve.value()
    );
    ui.frame();
    let bent = ui.dump_of(&ui.get(Role::Group, "Amp envelope"));
    assert_ne!(straight, bent, "the release is drawn bent");
    // Its handle moved off the straight line, towards the corner.
    let moved = ui.get(Role::Slider, "Release curve").center();
    assert!(moved.y > bend.center().y || moved.x < bend.center().x);
    // The keyboard goes through the parts in the order they are drawn.
    let names: Vec<String> = ui
        .everything()
        .into_iter()
        .filter(|found| found.role() == Role::Slider)
        .map(|found| found.name().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "Attack",
            "Attack curve",
            "Decay",
            "Sustain",
            "Release",
            "Release curve"
        ]
    );
    ui.assert_accessible();
}

#[test]
fn the_keyboard_chooses_a_part_moves_it_and_types_its_value() {
    let (mut ui, params) = open(1.0);
    assert!(ui.tab());
    assert!(ui.key(Key::Up));
    assert!(
        near(params.attack.value(), 110.0),
        "a hundredth of the range"
    );
    assert!(ui.key(Key::Right));
    assert!(ui.key(Key::Right));
    ui.frame();
    assert!(ui.get(Role::Slider, "Sustain").is_focused());
    assert!(ui.key(Key::PageDown));
    assert!(near(params.sustain.value(), 40.0));
    assert!(ui.key(Key::Right));
    assert!(ui.key(Key::Enter));
    ui.frame();
    let entry = ui.get(Role::TextInput, "Release");
    assert_eq!(entry.value_text(), Some("500 ms"));
    ui.type_text("750");
    assert!(ui.key(Key::Enter));
    assert!(near(params.release.value(), 750.0));
    ui.frame();
    assert_eq!(
        ui.get(Role::Group, "Amp envelope").value_text(),
        Some("Attack 110 ms, Decay 300 ms, Sustain 40 %, Release 750 ms")
    );
    // Delete puts the default back.
    assert!(ui.key(Key::Delete));
    assert!(near(params.release.value(), 500.0));
}

#[test]
fn a_double_click_types_and_a_modified_click_resets() {
    let (mut ui, params) = open(1.0);
    let peak = handle_point(&ui, "Attack");
    ui.drag(peak, (16.0, 0.0), 1);
    assert!(near(params.attack.value(), 300.0));
    ui.frame();
    let peak = handle_point(&ui, "Attack");
    ui.set_modifiers(manguebyte_ui::Modifiers {
        alt: true,
        ..manguebyte_ui::Modifiers::NONE
    });
    ui.click(peak);
    assert!(near(params.attack.value(), 100.0));
    ui.set_modifiers(manguebyte_ui::Modifiers::NONE);
    ui.frame();
    ui.double_click(handle_point(&ui, "Attack"));
    ui.frame();
    assert!(ui.find(Role::TextInput, "Attack").is_some());
    ui.type_text("20 ms\n");
    assert!(near(params.attack.value(), 20.0));
}

#[test]
fn assistive_technology_reads_and_sets_each_part() {
    let (mut ui, params) = open(1.0);
    ui.assert_accessible();
    let sustain = ui.get(Role::Slider, "Sustain");
    assert_eq!(sustain.value_text(), Some("50 %"));
    assert_eq!(sustain.numeric(), Some(50.0));
    assert!(ui.act(&sustain, Action::SetValue(80.0)));
    assert!(near(params.sustain.value(), 80.0));
    let decay = ui.get(Role::Slider, "Decay");
    assert!(ui.act(&decay, Action::Increment));
    assert!(near(params.decay.value(), 310.0));
    assert!(ui.act(&decay, Action::Focus));
    ui.frame();
    assert!(ui.get(Role::Slider, "Decay").is_focused());
}

#[test]
fn host_automation_moves_the_handles() {
    let (mut ui, params) = open(1.0);
    let before = handle_point(&ui, "Attack");
    params.attack.set_normalized(0.5);
    assert!(ui.frame().painted > 0);
    let after = handle_point(&ui, "Attack");
    assert!(
        (after.x - before.x - 32.0).abs() < 1e-3,
        "{before:?} {after:?}"
    );
}

#[test]
fn drags_draw_only_the_editor_and_equal_a_whole_repaint() {
    for scale in [1.0, 1.25, 2.0] {
        let params = params();
        let view = row((label("Beside").width(60.0), editor(&params).flex(1.0)));
        let mut ui = Harness::with_params(view, Size::new(396.0, 140.0), scale, params.clone());
        ui.frame();
        let found = ui.get(Role::Group, "Amp envelope");
        for name in ["Attack", "Decay", "Sustain", "Release"] {
            let at = handle_point(&ui, name);
            ui.drag(at, (7.3, -5.1), 3);
            let summary = ui.frame();
            assert!(summary.painted > 0, "{scale}x {name}");
            assert!(
                ui.last_damage()
                    .iter()
                    .all(|damage| found.device.contains_rect(damage)),
                "{scale}x {name}: {:?}",
                ui.last_damage()
            );
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after {name}");
        }
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(400)), 0);
    }
}
