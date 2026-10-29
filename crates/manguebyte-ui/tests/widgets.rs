//! What the widgets do with input, and what the host is told.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

use manguebyte_gfx::{Point, Size};
use manguebyte_params::Param;
use manguebyte_plugin::editor::testing::HostCall::{self, BeginGesture, EndGesture, Perform};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, AtomicSource, IntoView, Key, Modifiers, ParamBinding, ParamHandle, Role, Theme, column,
    knob, label, meter, row,
};

use common::{PANEL, PanelParams, levels, panel_view, params};

fn open(params: &Arc<PanelParams>, scale: f32) -> Harness {
    let mut ui = Harness::with_params(
        panel_view(params, &levels()),
        PANEL,
        scale,
        Arc::clone(params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.frame();
    ui
}

/// The values performed on `id` between the first begin and the last end.
fn performed(calls: &[HostCall], id: manguebyte_core::ParamId) -> Vec<f64> {
    calls
        .iter()
        .filter_map(|call| match *call {
            Perform(param, value) if param == id => Some(value),
            _ => None,
        })
        .collect()
}

const SHIFT: Modifiers = Modifiers {
    shift: true,
    ..Modifiers::NONE
};

#[test]
fn a_knob_drag_sends_one_gesture_of_rising_values_within_the_range() {
    let params = params();
    let mut ui = open(&params, 2.0);
    let knob = ui.get(Role::Slider, "Resonance");
    let start = params.resonance.normalized();
    assert_eq!(start, 0.25);

    // 60 pixels up in 12 moves of 5: each a fortieth of the 200-pixel drag
    // range.
    ui.drag(knob.center(), (0.0, -60.0), 12);
    ui.frame();
    let id = params.resonance.param_id();
    let calls = ui.take_calls();
    assert_eq!(calls.first(), Some(&BeginGesture(id)));
    assert_eq!(calls.last(), Some(&EndGesture(id)));
    assert_eq!(calls.len(), 14, "{calls:?}");
    let values = performed(&calls, id);
    assert_eq!(values.len(), 12);
    for (step, value) in values.iter().enumerate() {
        let expected = start + (step + 1) as f64 * 5.0 / 200.0;
        assert!(
            (value - expected).abs() < 1e-6,
            "{step}: {value} {expected}"
        );
        assert!((0.0..=1.0).contains(value));
    }
    assert!(
        values.windows(2).all(|pair| pair[0] < pair[1]),
        "{values:?}"
    );
    assert!((params.resonance.normalized() - 0.55).abs() < 1e-6);
    assert_eq!(ui.host().well_formed(), Ok(()));
    // The knob shows the parameter's value in its own words.
    let after = ui.get(Role::Slider, "Resonance");
    assert_eq!(after.value_text(), Some("0.55"));
    assert_eq!(
        after.value_text(),
        Some(params.resonance.display().as_str())
    );
}

#[test]
fn a_drag_past_the_end_stops_there_and_sends_each_value_once() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Resonance");
    ui.drag(knob.center(), (0.0, -1000.0), 10);
    let id = params.resonance.param_id();
    let calls = ui.take_calls();
    // 100 pixels is half the range: 0.75, then the top, then nothing new.
    assert_eq!(
        calls,
        [
            BeginGesture(id),
            Perform(id, 0.75),
            Perform(id, 1.0),
            EndGesture(id)
        ]
    );
    // And down past the bottom.
    let knob = ui.get(Role::Slider, "Resonance");
    ui.drag(knob.center(), (0.0, 1000.0), 2);
    assert_eq!(
        ui.take_calls(),
        [BeginGesture(id), Perform(id, 0.0), EndGesture(id)]
    );
    assert_eq!(params.resonance.value(), 0.0);
}

#[test]
fn shift_makes_a_drag_finer_without_a_jump() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Resonance");
    let at = knob.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 20.0));
    ui.set_modifiers(SHIFT);
    // The speed changes where the pointer is; 20 pixels more with Shift
    // are a tenth of 20 pixels without.
    ui.move_to(Point::new(at.x, at.y - 20.0));
    ui.move_to(Point::new(at.x, at.y - 40.0));
    ui.set_modifiers(Modifiers::NONE);
    ui.release(Point::new(at.x, at.y - 40.0));
    let id = params.resonance.param_id();
    let values = performed(&ui.take_calls(), id);
    assert_eq!(values.len(), 2, "{values:?}");
    assert!((values[0] - 0.35).abs() < 1e-6);
    assert!((values[1] - 0.36).abs() < 1e-6);
}

#[test]
fn a_knob_shows_the_value_it_is_sending_while_the_gesture_is_open() {
    let params = params();
    // This host records and stores nothing, as a host that echoes late.
    let mut ui = Harness::new(panel_view(&params, &levels()), PANEL, 1.5);
    ui.frame();
    let knob = ui.get(Role::Slider, "Drive");
    assert_eq!(knob.value_text(), Some("6.0 dB"));
    let at = knob.center();
    ui.press(at);
    // 50 pixels: a quarter of the range, from a quarter to a half.
    ui.move_to(Point::new(at.x, at.y - 50.0));
    ui.frame();
    let held = ui.get(Role::Slider, "Drive");
    assert_eq!(held.value_text(), Some("12.0 dB"));
    assert_eq!(params.drive.value(), 6.0, "the host stored nothing");
    assert!(
        ui.last_damage_bounds()
            .is_some_and(|d| knob.device.contains_rect(&d))
    );
    ui.release(Point::new(at.x, at.y - 50.0));
    ui.frame();
    // Let go, it keeps the value it sent while it waits for the echo,
    // rather than jumping back to 6 dB and then forward again.
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("12.0 dB"));
    ui.advance(ParamBinding::ECHO_WAIT - Duration::from_millis(1));
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("12.0 dB"));
    // No echo ever came: the parameter's own value again.
    ui.advance_ms(1);
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("6.0 dB"));
}

#[test]
fn whatever_the_host_stores_after_an_edit_is_shown_at_once() {
    let params = params();
    let mut ui = Harness::new(panel_view(&params, &levels()), PANEL, 1.0);
    ui.frame();
    let knob = ui.get(Role::Slider, "Drive");
    ui.drag(knob.center(), (0.0, -50.0), 5);
    ui.frame();
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("12.0 dB"));
    // The host's echo, or automation that overrides the edit: either way
    // the parameter no longer reads what it read before, and wins.
    params.drive.set(18.0);
    ui.frame();
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("18.0 dB"));
    // The next edit moves on from there.
    assert!(ui.key(Key::Down));
    assert_eq!(
        performed(&ui.take_calls(), params.drive.param_id()).last(),
        Some(&(0.75 - 0.01))
    );
}

#[test]
fn wheel_notches_sent_before_the_host_echoes_add_up() {
    let params = params();
    // Stores nothing: the VST 3 wrapper while the host delivers blocks,
    // which stores an edit only when the host echoes it.
    let mut ui = Harness::new(panel_view(&params, &levels()), PANEL, 1.0);
    ui.frame();
    let knob = ui.get(Role::Slider, "Resonance");
    // Three notches inside one audio block, as a touchpad sends them.
    for _ in 0..3 {
        assert!(ui.wheel(knob.center(), 1.0));
    }
    let sent = performed(&ui.take_calls(), params.resonance.param_id());
    assert_eq!(sent, [0.26, 0.27, 0.28]);
    ui.frame();
    assert_eq!(ui.get(Role::Slider, "Resonance").value_text(), Some("0.28"));
}

#[test]
fn keys_and_assistive_technology_add_up_before_the_host_echoes() {
    let params = params();
    let mut ui = Harness::new(panel_view(&params, &levels()), PANEL, 1.0);
    ui.frame();
    let drive = ui.get(Role::Slider, "Drive");
    assert!(ui.root_mut().set_focus_to(Some(drive.id)));
    assert!(ui.key(Key::Up));
    assert!(ui.key(Key::Up));
    assert!(ui.act(&drive, Action::Increment));
    assert!(ui.key(Key::PageUp));
    let sent = performed(&ui.take_calls(), params.drive.param_id());
    let expected = [0.26, 0.27, 0.28, 0.38];
    assert_eq!(sent.len(), expected.len(), "{sent:?}");
    for (value, want) in sent.iter().zip(expected) {
        assert!((value - want).abs() < 1e-9, "{sent:?}");
    }
}

#[test]
fn a_switch_turned_twice_before_the_host_echoes_ends_up_off() {
    let params = params();
    let mut ui = Harness::new(panel_view(&params, &levels()), PANEL, 1.0);
    ui.frame();
    // The keyboard focus, from Tab, so that Space reaches the switch.
    assert!(ui.tab());
    let switch = ui.get(Role::Switch, "Bypass");
    assert!(switch.is_focused());
    // On with a click, then off again with Space, inside one audio block.
    ui.click(switch.center());
    assert!(ui.key(Key::Character(' ')));
    // And twice more with clicks alone.
    ui.click(switch.center());
    ui.click(switch.center());
    let sent = performed(&ui.take_calls(), params.bypass.param_id());
    assert_eq!(sent, [1.0, 0.0, 1.0, 0.0]);
}

#[test]
fn escape_puts_back_the_value_the_parameter_had_when_the_drag_began() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Drive");
    // Host automation moves Drive from 6 dB (0.25) to 18 dB (0.75) after
    // the last frame; the person grabs the knob before the next one.
    params.drive.set(18.0);
    let at = knob.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 10.0));
    assert!(ui.key(Key::Escape));
    let id = params.drive.param_id();
    // Ten pixels up is 0.05 of the range, from 0.75; Escape restores 0.75.
    assert_eq!(
        ui.take_calls(),
        [
            BeginGesture(id),
            Perform(id, 0.75 + 10.0 / 200.0),
            Perform(id, 0.75),
            EndGesture(id)
        ]
    );
    assert_eq!(params.drive.normalized(), 0.75);
}

#[test]
fn a_key_moves_on_from_the_parameters_value_not_the_last_frames() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let drive = ui.get(Role::Slider, "Drive");
    assert!(ui.root_mut().set_focus_to(Some(drive.id)));
    // Automation moves Drive to 12 dB (0.5) between two frames.
    params.drive.set(12.0);
    assert!(ui.key(Key::Up));
    assert_eq!(performed(&ui.take_calls(), params.drive.param_id()), [0.51]);
    // So does a switch: turned over from what the parameter is now.
    params.bypass.set(true);
    let bypass = ui.get(Role::Switch, "Bypass");
    assert!(ui.act(&bypass, Action::Press));
    assert_eq!(performed(&ui.take_calls(), params.bypass.param_id()), [0.0]);
}

#[test]
fn escape_during_a_drag_puts_the_starting_value_back() {
    let params = params();
    let mut ui = open(&params, 2.0);
    let knob = ui.get(Role::Slider, "Drive");
    let at = knob.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 40.0));
    ui.frame();
    assert!((params.drive.value() - 10.8).abs() < 1e-5);
    assert!(ui.key(Key::Escape));
    // The release that follows does nothing more.
    ui.release(Point::new(at.x, at.y - 40.0));
    ui.frame();
    let id = params.drive.param_id();
    assert_eq!(
        ui.take_calls(),
        [
            BeginGesture(id),
            Perform(id, 0.25 + 40.0 / 200.0),
            Perform(id, 0.25),
            EndGesture(id)
        ]
    );
    assert_eq!(params.drive.value(), 6.0);
    assert_eq!(ui.get(Role::Slider, "Drive").value_text(), Some("6.0 dB"));
    assert_eq!(ui.root().pointer_holder(), None);
}

#[test]
fn closing_in_the_middle_of_a_drag_ends_the_gesture() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Cutoff");
    let at = knob.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 10.0));
    let id = params.cutoff.param_id();
    let calls = ui.close();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], BeginGesture(id));
    assert!(matches!(calls[1], Perform(param, _) if param == id));
    assert_eq!(calls[2], EndGesture(id));
}

#[test]
fn losing_the_window_focus_ends_a_drag_where_it_is() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Resonance");
    let at = knob.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 20.0));
    ui.send(manguebyte_ui::Event::Focus(false));
    let id = params.resonance.param_id();
    let calls = ui.take_calls();
    assert_eq!(calls.last(), Some(&EndGesture(id)));
    assert!((params.resonance.normalized() - 0.35).abs() < 1e-6);
    assert_eq!(ui.root().pointer_holder(), None);
    assert_eq!(ui.root().focus(), None);
}

#[test]
fn the_keyboard_changes_the_focused_knob_one_edit_per_key() {
    let params = params();
    let mut ui = open(&params, 1.0);
    // Focus moves through the widgets that take it, in tree order.
    assert!(ui.tab());
    assert_eq!(ui.focused().map(|f| f.node.name), Some("Bypass".into()));
    ui.tab();
    ui.tab();
    let focused = ui.focused().expect("a widget has the focus");
    assert_eq!(
        (focused.role(), focused.name()),
        (Role::Slider, "Resonance")
    );
    ui.take_calls();

    let id = params.resonance.param_id();
    let check = |ui: &mut Harness, key: Key, modifiers: Modifiers, expected: f64| {
        assert!(ui.key_with(key, modifiers), "{key:?} was not used");
        assert_eq!(
            ui.take_calls(),
            [BeginGesture(id), Perform(id, expected), EndGesture(id)],
            "{key:?}"
        );
    };
    let now = |p: &PanelParams| p.resonance.normalized();
    check(&mut ui, Key::Up, Modifiers::NONE, now(&params) + 0.01);
    check(&mut ui, Key::Right, Modifiers::NONE, now(&params) + 0.01);
    check(&mut ui, Key::Down, Modifiers::NONE, now(&params) - 0.01);
    check(&mut ui, Key::Up, SHIFT, now(&params) + 0.001);
    check(&mut ui, Key::PageUp, Modifiers::NONE, now(&params) + 0.1);
    check(&mut ui, Key::Home, Modifiers::NONE, 0.0);
    check(&mut ui, Key::End, Modifiers::NONE, 1.0);
    check(&mut ui, Key::PageDown, Modifiers::NONE, now(&params) - 0.1);
    check(&mut ui, Key::Delete, Modifiers::NONE, 0.25);
    assert_eq!(params.resonance.value(), 0.25);

    // A key the knob does not use reaches nothing, and keys never reach a
    // knob without the focus.
    assert!(!ui.key(Key::Character('x')));
    let drive = params.drive.value();
    ui.tab();
    ui.key(Key::Up);
    assert_ne!(params.drive.value(), drive, "the focus is on Drive now");
    assert!((params.resonance.value() - 0.25).abs() < 1e-9);
}

#[test]
fn each_notch_of_the_wheel_is_one_step_and_one_edit() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let knob = ui.get(Role::Slider, "Resonance");
    let id = params.resonance.param_id();
    assert!(ui.wheel(knob.center(), 1.0));
    assert_eq!(
        ui.take_calls(),
        [BeginGesture(id), Perform(id, 0.25 + 0.01), EndGesture(id)]
    );
    let before = params.resonance.normalized();
    assert!(ui.wheel(knob.center(), -3.0));
    assert_eq!(performed(&ui.take_calls(), id), [before + 0.01 * -3.0]);
    // A turn that is no number is not used.
    assert!(!ui.wheel(knob.center(), f32::NAN));
    assert!(!ui.wheel(knob.center(), f32::INFINITY));
    assert!(ui.take_calls().is_empty());
}

#[test]
fn a_click_with_alt_control_or_command_puts_the_default_back() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let id = params.drive.param_id();
    let held = [
        Modifiers {
            alt: true,
            ..Modifiers::NONE
        },
        Modifiers {
            control: true,
            ..Modifiers::NONE
        },
        Modifiers {
            meta: true,
            ..Modifiers::NONE
        },
    ];
    for modifiers in held {
        params.drive.set(20.0);
        ui.frame();
        let knob = ui.get(Role::Slider, "Drive");
        assert_eq!(knob.value_text(), Some("20.0 dB"));
        ui.set_modifiers(modifiers);
        ui.click(knob.center());
        ui.set_modifiers(Modifiers::NONE);
        // One edit, and no drag after it.
        assert_eq!(
            ui.take_calls(),
            [BeginGesture(id), Perform(id, 0.25), EndGesture(id)],
            "{modifiers:?}"
        );
        assert_eq!(params.drive.value(), 6.0);
    }
}

#[test]
fn a_slider_follows_the_pointer_along_its_track() {
    let params = params();
    let mut ui = open(&params, 2.0);
    let slider = ui.get(Role::Slider, "Mix");
    assert_eq!(slider.value_text(), Some("100 %"));
    // The track runs between the thumb's centres at each end: the
    // rectangle less a margin of 2, room of 4 for the thumb's shadow and
    // half the 12-pixel thumb each side.
    let track = slider.bounds.width - 2.0 * (2.0 + 4.0 + 6.0);
    ui.drag(slider.center(), (-track * 0.5, 0.0), 5);
    ui.frame();
    assert!(
        (params.mix.value() - 50.0).abs() < 1e-3,
        "{}",
        params.mix.value()
    );
    assert_eq!(ui.get(Role::Slider, "Mix").value_text(), Some("50 %"));
    assert_eq!(ui.host().well_formed(), Ok(()));
}

#[test]
fn a_switch_turns_over_on_a_click_and_on_space_and_enter() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let switch = ui.get(Role::Switch, "Bypass");
    assert_eq!(switch.node.state.checked, Some(false));
    assert_eq!(switch.value_text(), Some("Off"));
    ui.click(switch.center());
    let id = params.bypass.param_id();
    assert_eq!(
        ui.take_calls(),
        [BeginGesture(id), Perform(id, 1.0), EndGesture(id)]
    );
    assert!(params.bypass.value());
    ui.frame();
    let switch = ui.get(Role::Switch, "Bypass");
    assert_eq!(switch.node.state.checked, Some(true));
    assert!(
        !switch.is_focused(),
        "a click leaves the keyboard focus alone"
    );
    // With the focus from Tab, Space and Enter turn it over.
    assert!(ui.tab());
    assert!(ui.get(Role::Switch, "Bypass").is_focused());
    assert!(ui.key(Key::Character(' ')));
    assert!(!params.bypass.value());
    assert!(ui.key(Key::Enter));
    assert!(params.bypass.value());
    assert_eq!(performed(&ui.take_calls(), id), [0.0, 1.0]);
    // A click on the switch that has the focus keeps it there.
    ui.click(switch.center());
    assert!(ui.get(Role::Switch, "Bypass").is_focused());
    assert!(ui.key(Key::Character(' ')));
    assert_eq!(performed(&ui.take_calls(), id), [0.0, 1.0]);
}

#[test]
fn a_click_on_a_switch_does_not_take_the_hosts_space_bar() {
    let params = params();
    let mut ui = open(&params, 1.0);
    // Even when a knob had the focus before.
    let cutoff = ui.get(Role::Slider, "Cutoff");
    ui.click(cutoff.center());
    assert_eq!(ui.focused().map(|f| f.id), Some(cutoff.id));
    let switch = ui.get(Role::Switch, "Bypass");
    ui.click(switch.center());
    assert!(params.bypass.value());
    assert_eq!(ui.focused(), None);
    // The person clicked the switch with the mouse and now presses Space
    // to start the host's transport.
    let used = ui.key(Key::Character(' '));
    assert!(
        !used && params.bypass.value(),
        "Space after a mouse click flipped the switch back (used: {used})"
    );
}

#[test]
fn tab_is_left_to_the_host_when_nothing_takes_the_focus() {
    let level = Arc::new(manguebyte_core::AtomicF32::new(0.0));
    let mut ui = Harness::new(
        meter("Level", AtomicSource::new(level)),
        Size::new(60.0, 140.0),
        1.0,
    );
    ui.frame();
    assert!(
        !ui.tab(),
        "Tab moved nothing, yet it was reported as used and the host never sees it"
    );
    assert!(!ui.shift_tab());
    assert_eq!(ui.focused(), None);
}

#[test]
fn every_widget_can_be_found_by_role_and_name() {
    let params = params();
    let ui = open(&params, 1.0);
    for (role, name) in [
        (Role::Group, "Controls"),
        (Role::Label, "Filter"),
        (Role::Switch, "Bypass"),
        (Role::Slider, "Cutoff"),
        (Role::Slider, "Resonance"),
        (Role::Slider, "Drive"),
        (Role::Meter, "Left"),
        (Role::Meter, "Right"),
        (Role::Slider, "Mix"),
        (Role::Image, "Scope"),
    ] {
        let found = ui.get(role, name);
        assert!(!found.bounds.is_empty(), "{name} has no room");
    }
    ui.assert_accessible();
    // Anything with a name is found by its role and name alone.
    let everything = ui.everything();
    assert_eq!(everything.len(), 13);
    for found in &everything {
        if !found.name().is_empty() {
            assert_eq!(ui.find(found.role(), found.name()).as_ref(), Some(found));
        }
    }

    // Values, in the parameters' own words, and as numbers in ranges.
    let cutoff = ui.get(Role::Slider, "Cutoff");
    assert_eq!(cutoff.value_text(), Some("1000 Hz"));
    assert_eq!(cutoff.numeric(), Some(1000.0));
    let range = cutoff.node.range.expect("a range");
    assert!((range.min - 20.0).abs() < 1e-3 && (range.max - 20_000.0).abs() < 1e-1);
    assert_eq!(range.step, None);
    assert_eq!(ui.get(Role::Slider, "Resonance").value_text(), Some("0.25"));
    assert_eq!(ui.get(Role::Meter, "Left").value_text(), Some("-60.0 dB"));
    assert_eq!(
        ui.get(Role::Meter, "Left").node.actions,
        manguebyte_ui::Actions::NONE
    );
    assert_eq!(
        ui.get(Role::Slider, "Mix").node.actions,
        manguebyte_ui::Actions::ADJUST
    );
}

#[test]
fn the_panel_is_accessible_in_both_built_in_themes() {
    let params = params();
    for theme in [Theme::dark(), Theme::light()] {
        let mut ui = open(&params, 1.0);
        ui.set_theme(theme);
        ui.frame();
        ui.assert_accessible();
    }
}

#[test]
fn assert_accessible_rejects_siblings_that_share_a_role_and_name() {
    let level = Arc::new(manguebyte_core::AtomicF32::new(0.0));
    let view = row((
        meter("Level", AtomicSource::new(Arc::clone(&level))),
        meter("Level", AtomicSource::new(Arc::clone(&level))),
    ));
    let mut ui = Harness::new(view, Size::new(200.0, 160.0), 1.0);
    ui.frame();
    // Neither a screen reader nor `Harness::get` can tell them apart.
    assert!(ui.find(Role::Meter, "Level").is_none());
    let checked = catch_unwind(AssertUnwindSafe(|| ui.assert_accessible()));
    let message = checked.expect_err("two sibling meters both called \"Level\" passed the check");
    let message = message.downcast_ref::<String>().expect("a message").clone();
    assert!(message.contains("\"Level\""), "{message}");
    // Named groups tell them apart.
    let view = row((
        row((meter("Level", AtomicSource::new(Arc::clone(&level))),)).named("Input"),
        row((meter("Level", AtomicSource::new(level)),)).named("Output"),
    ));
    let mut ui = Harness::new(view, Size::new(200.0, 160.0), 1.0);
    ui.frame();
    ui.assert_accessible();
}

#[test]
fn assert_accessible_rejects_text_that_cannot_be_seen() {
    let mut theme = Theme::dark();
    theme.palette.text = theme.palette.background;
    let mut ui = Harness::new(
        column((label("Invisible"), label("Also invisible"))),
        Size::new(200.0, 60.0),
        1.0,
    );
    ui.set_theme(theme);
    ui.frame();
    let checked = catch_unwind(AssertUnwindSafe(|| ui.assert_accessible()));
    assert!(
        checked.is_err(),
        "text in the background colour (contrast 1:1) passed the check"
    );
}

#[test]
fn focus_moves_in_tree_order_and_wraps_round() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let order = ["Bypass", "Cutoff", "Resonance", "Drive", "Mix"];
    for name in order.iter().chain(order.iter()) {
        ui.tab();
        assert_eq!(ui.focused().map(|f| f.node.name), Some(name.to_string()));
    }
    for name in order.iter().rev().skip(1) {
        ui.shift_tab();
        assert_eq!(ui.focused().map(|f| f.node.name), Some(name.to_string()));
    }
    // Clicking something that does not take the focus takes it away.
    let title = ui.get(Role::Label, "Filter");
    ui.click(title.center());
    assert_eq!(ui.focused(), None);
    // The focused widget draws its ring, and only it is drawn again.
    ui.frame();
    ui.tab();
    ui.frame();
    let bypass = ui.get(Role::Switch, "Bypass");
    let damage = ui.last_damage_bounds().expect("the ring was drawn");
    assert!(
        bypass.device.contains_rect(&damage),
        "{damage:?} {:?}",
        bypass.device
    );
}

#[test]
fn assistive_technology_can_do_what_a_widget_lists_and_nothing_else() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let drive = ui.get(Role::Slider, "Drive");
    let id = params.drive.param_id();
    assert!(ui.act(&drive, Action::Increment));
    assert_eq!(performed(&ui.take_calls(), id), [0.25 + 0.01]);
    assert!(ui.act(&drive, Action::SetValue(12.0)));
    assert_eq!(params.drive.value(), 12.0);
    assert!(ui.act(&drive, Action::Decrement));
    assert!(!ui.act(&drive, Action::Press), "a knob is not pressed");
    assert!(!ui.act(&drive, Action::SetValue(f64::NAN)));
    assert!(ui.act(&drive, Action::Focus));
    assert_eq!(ui.focused().map(|f| f.id), Some(drive.id));
    let bypass = ui.get(Role::Switch, "Bypass");
    assert!(ui.act(&bypass, Action::Press));
    assert!(params.bypass.value());
    let meter = ui.get(Role::Meter, "Left");
    assert!(!ui.act(&meter, Action::Focus), "a meter takes no focus");
    assert_eq!(ui.host().well_formed(), Ok(()));
}

#[test]
fn a_knob_can_be_given_another_name() {
    let params = params();
    let view = knob(ParamHandle::new(Arc::clone(&params), |p| &p.cutoff)).label("Low cut");
    let mut ui = Harness::new(view, Size::new(80.0, 100.0), 1.0);
    ui.frame();
    assert_eq!(
        ui.get(Role::Slider, "Low cut").value_text(),
        Some("1000 Hz")
    );
    assert!(ui.dump().contains("text \"Low cut\""));
}

#[test]
fn a_second_press_during_a_drag_ends_the_first_where_it_is() {
    let params = params();
    let mut ui = open(&params, 1.0);
    let resonance = ui.get(Role::Slider, "Resonance");
    let drive = ui.get(Role::Slider, "Drive");
    let at = resonance.center();
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 20.0));
    // No release came; the next press is on another knob.
    ui.press(drive.center());
    let (r, d) = (params.resonance.param_id(), params.drive.param_id());
    let calls = ui.take_calls();
    assert_eq!(
        calls[..3],
        [
            BeginGesture(r),
            Perform(r, 0.25 + 20.0 / 200.0),
            EndGesture(r)
        ]
    );
    assert_eq!(calls[3..], [BeginGesture(d)]);
    assert_eq!(ui.root().pointer_holder(), Some(drive.id));
    ui.release(drive.center());
    assert_eq!(ui.take_calls(), [EndGesture(d)]);
}

#[test]
fn a_knob_can_find_its_parameter_by_id() {
    let params = params();
    let set = Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>;
    let unknown = manguebyte_core::ParamId::from_name("nothing");
    assert!(ParamHandle::by_id(Arc::clone(&set), unknown).is_none());
    let handle = ParamHandle::by_id(Arc::clone(&set), params.drive.param_id()).expect("drive");
    assert_eq!(handle.name(), "Drive");
    assert_eq!(handle.normalized(), 0.25);
    let mut ui = Harness::with_params(knob(handle), Size::new(80.0, 100.0), 1.0, set);
    ui.frame();
    let drive = ui.get(Role::Slider, "Drive");
    ui.wheel(drive.center(), 2.0);
    assert!((params.drive.value() - 6.48).abs() < 1e-4);
}

/// A label laid out at its natural size, which snapping to device pixels
/// can make up to a pixel narrower, still shows all of its text: the title
/// of a column aligned to the start used to show "Pre…".
#[test]
fn a_label_at_its_natural_size_is_not_cut() {
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
        for text in ["Preset", "Mangue Duck", "Wide Willow", "i"] {
            let view = column((label(text).title(), label(text)))
                .padding(10.3)
                .align(manguebyte_ui::layout::Align::Start);
            let mut ui = Harness::new(view, Size::new(300.0, 100.0), scale);
            ui.frame();
            let dump = ui.dump();
            for line in dump.lines().filter(|line| line.contains("text \"")) {
                let glyphs = line.matches(',').count() + 1;
                assert_eq!(glyphs, text.chars().count(), "{text:?} at {scale}: {line}");
            }
        }
    }
}
