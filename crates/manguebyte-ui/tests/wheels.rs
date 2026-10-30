//! The pitch bend and modulation wheels: dragging, the spring back, the
//! mouse wheel, the keys, a double click, parameters and polled states,
//! assistive technology, names that follow the language, and drawing that
//! touches only the wheel and a partial repaint reproduces exactly.

use std::sync::Arc;

use manguebyte_gfx::{IRect, Point, Size};
use manguebyte_params::{FloatParam, FloatRange, Param, ParamSet, param_set};
use manguebyte_plugin::editor::testing::HostCall;
use manguebyte_ui::testing::{Found, Harness};
use manguebyte_ui::{
    Action, BEND_RETURN_AFTER, Event, IntoView, Key, KeyEvent, KeySpan, KeyboardState, Modifiers,
    ParamHandle, Role, State, TextSource, WheelState, mod_wheel, piano_keyboard, pitch_wheel, row,
};

struct Params {
    bend: FloatParam,
    modulation: FloatParam,
}

param_set!(Params { bend, modulation });

fn params() -> Arc<Params> {
    Arc::new(Params {
        bend: FloatParam::new("bend", "Bend", FloatRange::linear(-2.0, 2.0), 0.0)
            .with_unit("st")
            .with_precision(2),
        modulation: FloatParam::new("mod", "Mod", FloatRange::linear(0.0, 100.0), 0.0)
            .with_unit("%")
            .with_precision(0),
    })
}

/// Two wheels bound to states, left of a keyboard, as a synth's screen
/// has them.
fn wheels(scale: f32) -> (Harness, WheelState, WheelState) {
    let (bend, modulation) = (WheelState::new(), WheelState::new());
    let view = row((
        pitch_wheel("Pitch", bend.clone()),
        mod_wheel("Mod", modulation.clone()),
        piano_keyboard("Keys", KeySpan::new(48, 72), KeyboardState::new()).flex(1.0),
    ))
    .spacing(6.0)
    .padding(8.0);
    let mut ui = Harness::new(view, Size::new(420.0, 170.0), scale);
    ui.frame();
    (ui, bend, modulation)
}

fn key_down(ui: &mut Harness, key: Key) -> bool {
    ui.send(Event::Key(KeyEvent {
        key,
        modifiers: Modifiers::NONE,
        pressed: true,
    }))
}

fn key_up(ui: &mut Harness, key: Key) -> bool {
    ui.send(Event::Key(KeyEvent {
        key,
        modifiers: Modifiers::NONE,
        pressed: false,
    }))
}

fn damage_inside(ui: &Harness, rect: IRect) -> bool {
    ui.last_damage()
        .iter()
        .all(|damage| rect.contains_rect(damage))
}

/// The slot's height, the whole travel of a drag, from the wheel's bounds.
fn travel(found: &Found) -> f32 {
    // The name's line, the margins and the room for the glow.
    found.bounds.height - 2.0 * (2.0 + 6.0) - 2.0 - 15.0
}

#[test]
fn the_pitch_bend_wheel_springs_back_on_the_theme_spring() {
    let (mut ui, bend, _) = wheels(1.0);
    ui.set_motion(true);
    let wheel = ui.get(Role::Slider, "Pitch");
    ui.press(wheel.center());
    ui.move_to(wheel.center() - Point::new(0.0, 40.0));
    let up = bend.take().expect("moved");
    assert!(up > 0.5, "{up}");
    assert!(bend.is_held());
    ui.release(wheel.center() - Point::new(0.0, 40.0));
    // It travels back over several frames, each reported to the
    // application and drawn inside the wheel only, past the middle a
    // little on the spring, and then rests at 0.
    let mut seen = vec![];
    let mut frames = 0;
    while ui.frame().animating {
        frames += 1;
        assert!(damage_inside(&ui, wheel.device), "{:?}", ui.last_damage());
        if let Some(value) = bend.take() {
            seen.push(value);
        }
        ui.advance_ms(16);
        assert!(frames < 40, "it never settled");
    }
    if let Some(value) = bend.take() {
        seen.push(value);
    }
    assert!(seen.len() >= 5, "{seen:?}");
    assert!(seen.iter().any(|&v| v > 0.0 && v < up), "{seen:?}");
    assert!(
        seen.iter().any(|&v| v < 0.0),
        "the spring overshoots: {seen:?}"
    );
    assert_eq!(seen.last(), Some(&0.0));
    assert!(!bend.is_held());
    assert_eq!(ui.get(Role::Slider, "Pitch").value_text(), Some("0.00"));
    // Settled: nothing more is drawn.
    assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(300)), 0);
}

#[test]
fn with_motion_off_the_pitch_bend_wheel_jumps_back() {
    let (mut ui, bend, _) = wheels(1.0);
    let wheel = ui.get(Role::Slider, "Pitch");
    ui.drag(wheel.center(), (0.0, 30.0), 3);
    assert_eq!(bend.value(), 0.0, "back at once");
    let summary = ui.frame();
    assert!(!summary.animating);
    assert_eq!(bend.take(), Some(0.0));
}

#[test]
fn the_modulation_wheel_stays_where_it_is_left() {
    let (mut ui, _, modulation) = wheels(1.0);
    let wheel = ui.get(Role::Slider, "Mod");
    let travel = travel(&wheel);
    ui.drag(wheel.center(), (0.0, -travel * 0.5), 4);
    ui.advance_ms(500);
    let value = modulation.value();
    assert!((value - 0.5).abs() < 0.02, "{value}");
    assert_eq!(modulation.take(), Some(value));
    assert!(!modulation.is_held());
    // Dragging below the bottom stops at 0.
    ui.drag(wheel.center(), (0.0, travel * 2.0), 4);
    assert_eq!(modulation.value(), 0.0);
}

#[test]
fn the_keys_move_a_wheel_and_the_bend_comes_back_when_the_key_comes_up() {
    let (mut ui, bend, modulation) = wheels(1.0);
    let wheel = ui.get(Role::Slider, "Mod");
    assert!(ui.act(&wheel, Action::Focus));
    assert!(ui.key(Key::Up));
    assert_eq!(modulation.value(), 1.0 / 32.0);
    assert!(ui.key(Key::PageUp));
    assert_eq!(modulation.value(), 1.0 / 32.0 + 0.25);
    assert!(ui.key(Key::End));
    assert_eq!(modulation.value(), 1.0);
    assert!(ui.key(Key::Down));
    assert_eq!(modulation.value(), 1.0 - 1.0 / 32.0);
    assert!(ui.key(Key::Home));
    assert_eq!(modulation.value(), 0.0);
    // The pitch bend wheel is held while the key is down, the key's
    // repeats moving it further, and goes back when it comes up.
    assert!(ui.act(&ui.get(Role::Slider, "Pitch"), Action::Focus));
    assert!(ui.focused().is_some_and(|found| found.name() == "Pitch"));
    assert!(key_down(&mut ui, Key::Down));
    assert!(key_down(&mut ui, Key::Down));
    assert_eq!(bend.value(), -2.0 * 2.0 / 32.0);
    assert!(bend.is_held());
    assert!(key_down(&mut ui, Key::End));
    assert_eq!(bend.value(), 1.0);
    // The release of another key changes nothing.
    key_up(&mut ui, Key::Down);
    assert_eq!(bend.value(), 1.0);
    assert!(key_up(&mut ui, Key::End));
    ui.frame();
    assert_eq!(bend.value(), 0.0);
    assert!(!bend.is_held());
    // Leaving with a key down lets it go too.
    assert!(key_down(&mut ui, Key::Up));
    assert!(bend.value() > 0.0);
    assert!(ui.tab());
    ui.frame();
    assert_eq!(bend.value(), 0.0);
}

#[test]
fn the_mouse_wheel_turns_a_wheel_and_the_bend_returns_after_a_pause() {
    let (mut ui, bend, modulation) = wheels(1.0);
    let pitch = ui.get(Role::Slider, "Pitch").center();
    assert!(ui.wheel(pitch, 2.0));
    assert_eq!(bend.value(), 2.0 * 2.0 / 32.0);
    assert!(bend.is_held());
    ui.advance_ms(BEND_RETURN_AFTER.as_millis() as u64 / 2);
    assert!(ui.wheel(pitch, 1.0), "another notch starts the pause again");
    ui.advance_ms(BEND_RETURN_AFTER.as_millis() as u64 / 2 + 50);
    assert_eq!(bend.value(), 3.0 * 2.0 / 32.0);
    ui.advance_ms(BEND_RETURN_AFTER.as_millis() as u64);
    assert_eq!(bend.value(), 0.0);
    assert!(!bend.is_held());
    // Then nothing runs.
    assert!(!ui.frame().animating);
    let modwheel = ui.get(Role::Slider, "Mod").center();
    assert!(ui.wheel(modwheel, 3.0));
    ui.advance_ms(1000);
    assert_eq!(modulation.value(), 3.0 / 32.0);
    assert!(ui.wheel(modwheel, -10.0));
    assert_eq!(modulation.value(), 0.0);
}

#[test]
fn a_double_click_puts_a_wheel_back_at_rest() {
    let (mut ui, _, modulation) = wheels(1.0);
    let wheel = ui.get(Role::Slider, "Mod");
    ui.drag(wheel.center(), (0.0, -40.0), 2);
    assert!(modulation.value() > 0.2);
    ui.double_click(wheel.center());
    assert_eq!(modulation.value(), 0.0);
}

#[test]
fn bound_to_parameters_the_wheels_send_gestures() {
    let params = params();
    let view = row((
        pitch_wheel(
            "Pitch",
            ParamHandle::new(Arc::clone(&params), |p: &Params| &p.bend),
        ),
        mod_wheel(
            "Mod",
            ParamHandle::new(Arc::clone(&params), |p: &Params| &p.modulation),
        ),
    ));
    let mut ui = Harness::with_params(
        view,
        Size::new(120.0, 170.0),
        1.0,
        Arc::clone(&params) as Arc<dyn ParamSet>,
    );
    ui.frame();
    let (bend, modulation) = (params.bend.param_id(), params.modulation.param_id());
    assert_eq!(ui.get(Role::Slider, "Pitch").value_text(), Some("0.00 st"));
    // A drag and the spring back are one gesture, which ends at the middle.
    let pitch = ui.get(Role::Slider, "Pitch");
    ui.drag(pitch.center(), (0.0, -20.0), 2);
    ui.frame();
    let calls = ui.take_calls();
    assert_eq!(calls.first(), Some(&HostCall::BeginGesture(bend)));
    assert_eq!(calls.last(), Some(&HostCall::EndGesture(bend)));
    assert_eq!(calls[calls.len() - 2], HostCall::Perform(bend, 0.5));
    assert!(
        calls[1..calls.len() - 2]
            .iter()
            .all(|call| matches!(call, HostCall::Perform(id, v) if *id == bend && *v > 0.5))
    );
    // A key on the modulation wheel is one edit of its own.
    let wheel = ui.get(Role::Slider, "Mod");
    assert!(ui.act(&wheel, Action::Focus));
    assert!(ui.key(Key::PageUp));
    assert_eq!(
        ui.take_calls(),
        [
            HostCall::BeginGesture(modulation),
            HostCall::Perform(modulation, 0.25),
            HostCall::EndGesture(modulation),
        ]
    );
    // Host automation moves the wheel.
    params.modulation.set_normalized(0.75);
    ui.advance_ms(300);
    assert_eq!(ui.get(Role::Slider, "Mod").value_text(), Some("75 %"));
    // Assistive technology sets plain values.
    assert!(ui.act(&ui.get(Role::Slider, "Pitch"), Action::SetValue(1.0)));
    assert_eq!(
        ui.take_calls(),
        [
            HostCall::BeginGesture(bend),
            HostCall::Perform(bend, 0.75),
            HostCall::EndGesture(bend),
        ]
    );
}

#[test]
fn assistive_technology_reads_and_sets_the_wheels() {
    let (mut ui, bend, modulation) = wheels(1.0);
    ui.assert_accessible();
    let pitch = ui.get(Role::Slider, "Pitch");
    assert_eq!(pitch.value_text(), Some("0.00"));
    let range = pitch.node.range.expect("a range");
    assert_eq!((range.min, range.max), (-1.0, 1.0));
    assert!(ui.act(&pitch, Action::SetValue(-0.5)));
    assert_eq!(bend.value(), -0.5, "set this way, it stays");
    ui.advance_ms(1000);
    assert_eq!(bend.value(), -0.5);
    assert_eq!(ui.get(Role::Slider, "Pitch").value_text(), Some("-0.50"));
    let wheel = ui.get(Role::Slider, "Mod");
    assert!(ui.act(&wheel, Action::Increment));
    ui.frame();
    assert_eq!(modulation.value(), 1.0 / 32.0);
    assert_eq!(ui.get(Role::Slider, "Mod").value_text(), Some("3 %"));
    assert_eq!(ui.get(Role::Slider, "Mod").numeric(), Some(1.0 / 32.0));
    assert_eq!(modulation.take(), Some(1.0 / 32.0));
    // What the application sets from MIDI it receives is shown, and not
    // reported back as the person's.
    modulation.set(0.5);
    ui.frame();
    assert_eq!(ui.get(Role::Slider, "Mod").value_text(), Some("50 %"));
    assert_eq!(modulation.take(), None);
}

#[test]
fn the_wheels_follow_the_language() {
    let language = State::new(String::from("en"));
    let name = |texts: [&'static str; 2]| {
        let language = language.clone();
        TextSource::dynamic(move || {
            if language.with(|l| l == "pt") {
                texts[1]
            } else {
                texts[0]
            }
        })
    };
    let view = row((
        pitch_wheel(name(["Pitch", "Afinação"]), WheelState::new()),
        mod_wheel(name(["Mod", "Modulação"]), WheelState::new()),
    ));
    let mut ui = Harness::new(view, Size::new(200.0, 170.0), 2.0);
    ui.frame();
    ui.advance_ms(50);
    language.set(String::from("pt"));
    let summary = ui.frame();
    assert_eq!(summary.painted, 2, "both draw their names");
    assert_eq!(ui.access_updates().len(), 2);
    let pitch = ui.get(Role::Slider, "Afinação");
    assert!(ui.dump_of(&pitch).contains("text \"Afinação\""));
    assert!(ui.find(Role::Slider, "Modulação").is_some());
    assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(100)), 0);
}

#[test]
fn every_change_draws_only_the_wheel_and_equals_a_whole_repaint() {
    for scale in [1.0, 1.25, 2.0] {
        let (mut ui, bend, modulation) = wheels(scale);
        ui.set_motion(true);
        assert!(ui.pixels() == &ui.render_fresh(), "{scale}x at first");
        let pitch = ui.get(Role::Slider, "Pitch");
        let wheel = ui.get(Role::Slider, "Mod");
        let check = |ui: &mut Harness, found: &Found, what: &str| {
            let summary = ui.frame();
            assert!(summary.painted > 0, "{scale}x {what}: nothing drawn");
            assert!(
                damage_inside(ui, found.device),
                "{scale}x {what}: {:?} outside {:?}",
                ui.last_damage(),
                found.device
            );
            assert!(ui.pixels() == &ui.render_fresh(), "{scale}x after {what}");
        };
        ui.press(pitch.center());
        ui.move_to(pitch.center() - Point::new(0.0, 25.3));
        check(&mut ui, &pitch, "a bend");
        ui.release(pitch.center());
        for step in 0..20 {
            ui.advance_ms(16);
            assert!(damage_inside(&ui, pitch.device), "{scale}x spring {step}");
            if step % 5 == 0 {
                assert!(ui.pixels() == &ui.render_fresh(), "{scale}x spring {step}");
            }
        }
        modulation.set(0.37);
        check(&mut ui, &wheel, "a value from the application");
        ui.wheel(wheel.center(), 1.0);
        check(&mut ui, &wheel, "a notch");
        assert!(ui.act(&wheel, Action::Focus));
        ui.key(Key::Up);
        check(&mut ui, &wheel, "a key with the focus ring");
        // Nothing moving, nothing drawn.
        ui.advance_ms(300);
        assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(500)), 0);
        assert_eq!(bend.value(), 0.0);
    }
}
