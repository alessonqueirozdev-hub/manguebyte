//! What a frame draws again: only what changed, and nothing when nothing
//! did.

mod common;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_core::AtomicF32;
use manguebyte_gfx::{Color, IRect, Rect, Size};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    AtomicSource, IntoView, Label, Role, State, canvas, column, label, meter, row,
};

use common::{Levels, PANEL, PanelParams, levels, panel_view, params};

fn open(scale: f32) -> (Harness, Arc<PanelParams>, Levels) {
    let params = params();
    let levels = levels();
    let mut ui = Harness::with_params(
        panel_view(&params, &levels),
        PANEL,
        scale,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    let first = ui.frame();
    assert!(first.laid_out);
    assert_eq!(first.damaged, 1);
    assert_eq!(first.painted, 13, "every widget was drawn");
    (ui, params, levels)
}

/// Every damage rectangle of the last frame lies inside `rect`.
fn damage_inside(ui: &Harness, rect: IRect) -> bool {
    !ui.last_damage().is_empty() && ui.last_damage().iter().all(|d| rect.contains_rect(d))
}

#[test]
fn the_first_frame_draws_the_whole_buffer_and_a_steady_panel_draws_nothing() {
    for scale in [1.0, 1.25, 2.0] {
        let (mut ui, _params, _levels) = open(scale);
        let whole = IRect::new(
            0,
            0,
            (PANEL.width * scale).round() as u32,
            (PANEL.height * scale).round() as u32,
        );
        assert_eq!(ui.pixels().bounds(), whole);
        // A second of frames with nothing changing.
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(1_000)), 0);
        let summary = ui.frame();
        assert_eq!(
            (summary.damaged, summary.painted, summary.laid_out),
            (0, 0, false)
        );
        assert!(!summary.animating);
        assert!(ui.last_damage().is_empty());
    }
}

#[test]
fn a_meter_whose_level_changes_damages_only_its_own_rectangle() {
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let (mut ui, _params, levels) = open(scale);
        ui.advance_ms(100);
        let left = ui.get(Role::Meter, "Left");
        let right = ui.get(Role::Meter, "Right");
        for level in [-20.0, -6.0, -59.5, 0.0, f32::NAN, 12.0, -1e30] {
            levels.left.store(level, Ordering::Relaxed);
            let summary = ui.frame();
            assert_eq!(summary.painted, 1, "{scale}x, {level}");
            assert!(
                damage_inside(&ui, left.device),
                "{scale}x, {level}: {:?} outside {:?}",
                ui.last_damage(),
                left.device
            );
            assert!(
                !ui.last_damage()
                    .iter()
                    .any(|d| d.intersect(&right.device).is_some())
            );
            // The same level again: nothing to draw.
            assert_eq!(ui.frame().damaged, 0, "{scale}x, {level}");
        }
        // Storing the value it already shows draws nothing either.
        levels.left.store(-20.0, Ordering::Relaxed);
        ui.frame();
        levels.left.store(-20.0, Ordering::Relaxed);
        assert_eq!(ui.frame().damaged, 0);
        assert_eq!(ui.get(Role::Meter, "Left").value_text(), Some("-20.0 dB"));
        // Whatever was drawn in parts adds up to one whole render.
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x");
    }
}

#[test]
fn a_falling_meter_animates_until_it_lands_then_stops_drawing() {
    let level = Arc::new(AtomicF32::new(0.0));
    let view = meter("Level", AtomicSource::new(Arc::clone(&level)))
        .range(-60.0, 0.0)
        .unit("dB")
        .precision(1)
        .release(24.0);
    let mut ui = Harness::new(view, Size::new(60.0, 140.0), 2.0);
    ui.frame();
    assert_eq!(ui.get(Role::Meter, "Level").value_text(), Some("0.0 dB"));
    // It rises at once and falls at 24 dB a second.
    level.store(-24.0, Ordering::Relaxed);
    ui.set_period(std::time::Duration::from_millis(10));
    let summary = ui.frame();
    assert!(summary.animating, "still falling");
    assert_eq!(summary.damaged, 0, "no time has passed");
    ui.advance_ms(500);
    let halfway = ui.get(Role::Meter, "Level").numeric().expect("a level");
    assert!((halfway + 12.0).abs() < 1e-3, "{halfway}");
    // A little over another half second: the frame that would pass the
    // level lands on it exactly.
    ui.advance_ms(520);
    assert_eq!(ui.get(Role::Meter, "Level").numeric(), Some(-24.0));
    // Landed: one more frame says so, and then nothing is drawn.
    let summary = ui.frame();
    assert!(!summary.animating);
    assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(1_000)), 0);
    // A rise is shown at once.
    level.store(-3.0, Ordering::Relaxed);
    ui.advance_ms(10);
    assert_eq!(ui.get(Role::Meter, "Level").value_text(), Some("-3.0 dB"));
}

#[test]
fn a_knob_drag_damages_only_the_knob() {
    let (mut ui, params, _levels) = open(2.0);
    let knob = ui.get(Role::Slider, "Resonance");
    let at = knob.center();
    ui.press(at);
    ui.frame();
    assert!(damage_inside(&ui, knob.device), "the focus ring");
    for step in 1..=5 {
        ui.move_to(manguebyte_gfx::Point::new(at.x, at.y - 7.0 * step as f32));
        let summary = ui.frame();
        assert_eq!(summary.painted, 1);
        assert!(damage_inside(&ui, knob.device), "{:?}", ui.last_damage());
    }
    ui.release(manguebyte_gfx::Point::new(at.x, at.y - 35.0));
    ui.frame();
    // Automation from the host moves another knob: only that one is drawn.
    params.drive.set(18.0);
    ui.frame();
    let drive = ui.get(Role::Slider, "Drive");
    assert!(damage_inside(&ui, drive.device));
    assert!(ui.pixels() == &ui.render_fresh());
}

/// Changes every control of the panel in turn, by automation and by
/// input, and checks after each frame that the damage stayed inside the
/// control that changed.
fn each_change_damages_only_its_control(ui: &mut Harness, params: &PanelParams, levels: &Levels) {
    let size = ui.root().size();
    let scale = ui.root().scale();
    let check = |ui: &mut Harness, role: Role, name: &str, change: &dyn Fn(&mut Harness)| {
        let found = ui.get(role, name);
        change(ui);
        ui.frame();
        let damage = ui.last_damage().to_vec();
        assert!(
            damage.iter().all(|d| found.device.contains_rect(d)),
            "{size:?} at {scale}x: {name} at {:?} damaged {damage:?}",
            found.device
        );
    };
    check(ui, Role::Switch, "Bypass", &|_| params.bypass.set(true));
    check(ui, Role::Switch, "Bypass", &|ui| {
        let switch = ui.get(Role::Switch, "Bypass");
        ui.click(switch.center());
    });
    check(ui, Role::Slider, "Mix", &|_| params.mix.set(37.0));
    check(ui, Role::Slider, "Mix", &|ui| {
        let mix = ui.get(Role::Slider, "Mix");
        ui.wheel(mix.center(), -5.0);
    });
    check(ui, Role::Slider, "Cutoff", &|_| params.cutoff.set(5_000.0));
    check(ui, Role::Slider, "Resonance", &|_| {
        params.resonance.set(0.9)
    });
    check(ui, Role::Slider, "Drive", &|_| params.drive.set(23.0));
    check(ui, Role::Meter, "Left", &|_| {
        levels.left.store(-3.0, Ordering::Relaxed);
    });
    check(ui, Role::Meter, "Right", &|_| {
        levels.right.store(-40.0, Ordering::Relaxed);
    });
    assert!(ui.pixels() == &ui.render_fresh(), "{size:?} at {scale}x");
}

#[test]
fn every_control_damages_only_its_own_rectangle_at_its_natural_size() {
    for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
        let (mut ui, params, levels) = open(scale);
        each_change_damages_only_its_control(&mut ui, &params, &levels);
        // At its natural size nothing reaches past its rectangle, so no
        // control's scene needed cutting.
        for (role, name) in [
            (Role::Switch, "Bypass"),
            (Role::Slider, "Mix"),
            (Role::Slider, "Cutoff"),
            (Role::Meter, "Left"),
        ] {
            let found = ui.get(role, name);
            assert!(
                !ui.dump_of(&found).starts_with("clip"),
                "{scale}x: {name} was clipped"
            );
        }
    }
}

#[test]
fn in_a_window_smaller_than_the_panel_a_change_damages_only_its_own_control() {
    for size in [
        Size::new(400.0, 150.0),
        Size::new(260.0, 200.0),
        Size::new(150.0, 90.0),
        Size::new(60.0, 40.0),
    ] {
        for scale in [1.0, 1.25, 1.5, 2.0, 3.0] {
            let params = params();
            let levels = levels();
            let mut ui = Harness::with_params(
                panel_view(&params, &levels),
                size,
                scale,
                Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
            );
            ui.frame();
            each_change_damages_only_its_control(&mut ui, &params, &levels);
        }
    }
    // At 400 x 150 the column gives every control less than its natural
    // height: the switch's pill and the slider's track would reach past
    // their rectangles, and are cut to them.
    let params = params();
    let levels = levels();
    let mut ui = Harness::with_params(
        panel_view(&params, &levels),
        Size::new(400.0, 150.0),
        1.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.frame();
    for (role, name) in [(Role::Switch, "Bypass"), (Role::Slider, "Mix")] {
        let found = ui.get(role, name);
        assert!(
            ui.dump_of(&found).starts_with("clip"),
            "{name} at {:?}:\n{}",
            found.device,
            ui.dump_of(&found)
        );
    }
}

#[test]
fn a_label_with_too_little_room_is_cut_to_its_rectangle() {
    let text = State::new(String::from("Short"));
    let view = column((
        label("Above"),
        Label::from_state(text.clone()).title().height(6.0),
        label("Below"),
    ));
    let mut ui = Harness::new(view, Size::new(60.0, 60.0), 2.0);
    ui.frame();
    text.set(String::from("A much longer title"));
    ui.frame();
    let cut = ui.get(Role::Label, "A much longer title");
    assert_eq!(cut.bounds.height, 6.0);
    assert!(
        !ui.last_damage().is_empty()
            && ui.last_damage().iter().all(|d| cut.device.contains_rect(d)),
        "{:?} outside {:?}",
        ui.last_damage(),
        cut.device
    );
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_new_size_or_scale_lays_out_and_draws_everything_again() {
    let (mut ui, _params, _levels) = open(1.0);
    ui.resize(Size::new(500.0, 320.0), 1.5);
    let summary = ui.frame();
    assert!(summary.laid_out);
    assert_eq!(ui.last_damage(), [IRect::new(0, 0, 750, 480)]);
    assert_eq!(ui.pixels().bounds(), IRect::new(0, 0, 750, 480));
    // The same size and scale again change nothing.
    ui.resize(Size::new(500.0, 320.0), 1.5);
    assert_eq!(ui.frame().damaged, 0);
    // A scale that cannot be used keeps the old one.
    ui.resize(Size::new(500.0, 320.0), f32::NAN);
    assert_eq!(ui.root().scale(), 1.5);
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_widget_that_draws_the_same_again_costs_no_damage() {
    let version = Rc::new(Cell::new(0_u64));
    let shade = Rc::new(Cell::new(0x30_u8));
    let source = Rc::clone(&version);
    let colour = Rc::clone(&shade);
    let view = canvas("Swatch", Size::new(40.0, 40.0), move |cx, scene| {
        let level = colour.get();
        scene.fill(cx.bounds(), Color::rgb(level, level, level));
    })
    .redraw_on(move || source.get());
    let mut ui = Harness::new(
        row((view, label("Beside"))).padding(4.0),
        Size::new(120.0, 48.0),
        1.0,
    );
    ui.frame();
    // Asked to draw again, it draws the same: nothing is damaged.
    version.set(1);
    let summary = ui.frame();
    assert_eq!((summary.painted, summary.damaged), (0, 0));
    // A real change damages the canvas, which is clipped to its rectangle.
    shade.set(0x80);
    version.set(2);
    let summary = ui.frame();
    assert_eq!(summary.painted, 1);
    let swatch = ui.get(Role::Image, "Swatch");
    assert_eq!(swatch.bounds, Rect::new(4.0, 4.0, 40.0, 40.0));
    assert!(damage_inside(&ui, swatch.device));
    assert_eq!(
        ui.pixels().pixel(20, 20).map(|p| p.to_array()),
        Some([0x80, 0x80, 0x80, 0xFF])
    );
}

#[test]
fn a_label_whose_text_changes_is_laid_out_again() {
    let text = State::new(String::from("Off"));
    let view = column((Label::from_state(text.clone()).width(100.0), label("Below")));
    let mut ui = Harness::new(view, Size::new(120.0, 60.0), 2.0);
    ui.frame();
    assert_eq!(ui.frame().damaged, 0);
    text.set(String::from("Armed"));
    let summary = ui.frame();
    assert!(summary.laid_out);
    let armed = ui.get(Role::Label, "Armed");
    assert!(damage_inside(&ui, armed.device));
    assert!(ui.find(Role::Label, "Off").is_none());
    // Setting the same text again is a new version, but draws the same.
    text.set(String::from("Armed"));
    assert_eq!(ui.frame().damaged, 0);
}

#[test]
fn a_new_theme_draws_everything_in_its_colours() {
    let (mut ui, _params, _levels) = open(1.0);
    assert!(ui.dump().contains("solid(#eef0f6ff)"), "dark text");
    ui.set_theme(manguebyte_ui::Theme::light());
    let summary = ui.frame();
    assert!(summary.laid_out);
    assert_eq!(ui.last_damage(), [IRect::new(0, 0, 400, 300)]);
    let dump = ui.dump();
    assert!(dump.contains("solid(#16150fff)"), "light text");
    assert!(
        !dump.contains("#eef0f6"),
        "nothing kept the dark theme's colours"
    );
    assert!(ui.pixels() == &ui.render_fresh());
}

#[test]
fn a_label_reads_its_source_every_frame_and_draws_only_changes() {
    let count = Rc::new(Cell::new(1_u32));
    let source = Rc::clone(&count);
    let view = Label::dynamic(move || format!("{} presets", source.get())).width(120.0);
    let mut ui = Harness::new(view, Size::new(120.0, 20.0), 1.0);
    ui.frame();
    assert!(ui.find(Role::Label, "1 presets").is_some());
    assert_eq!(ui.frame().damaged, 0);
    count.set(12);
    let summary = ui.frame();
    assert!(summary.laid_out && summary.damaged > 0);
    assert!(ui.find(Role::Label, "12 presets").is_some());
    assert_eq!(ui.frame().damaged, 0);
}
