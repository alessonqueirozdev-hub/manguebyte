//! Motion costs the audio nothing and the interface little: an idle
//! interface draws nothing and asks for no frames, a transition redraws
//! only the widget that moves and stops asking once it settles, and with
//! motion turned off changes jump.

mod common;

use std::sync::Arc;
use std::time::Duration;

use manguebyte_gfx::{Point, Size};
use manguebyte_params::FloatParam;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{IntoView, ParamHandle, Role, Theme, knob, row};

use common::{PanelParams, params};

fn knobs(params: &Arc<PanelParams>) -> Harness {
    let handle = |f: fn(&PanelParams) -> &FloatParam| ParamHandle::new(Arc::clone(params), f);
    let view = row((
        knob(handle(|p| &p.resonance)),
        knob(handle(|p| &p.cutoff)),
        knob(handle(|p| &p.drive)),
    ))
    .spacing(8.0)
    .padding(8.0);
    let mut ui = Harness::with_params(
        view,
        Size::new(260.0, 110.0),
        2.0,
        Arc::clone(params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.set_motion(true);
    ui.frame();
    ui
}

#[test]
fn an_idle_interface_draws_nothing_and_wants_no_frames() {
    let params = params();
    let mut ui = knobs(&params);
    // A thousand frames, 16 s, with nothing happening: none draws.
    let drawn = ui.frames_rendered_during(|ui| ui.advance_ms(16_000));
    assert_eq!(drawn, 0);
    assert!(ui.frames() > 1000);
    assert!(!ui.root().is_animating());
    let frame = ui.frame();
    assert_eq!(
        (frame.damaged, frame.painted, frame.animating),
        (0, 0, false)
    );
}

#[test]
fn a_hover_transition_redraws_only_the_hovered_knob_until_it_settles() {
    let params = params();
    let mut ui = knobs(&params);
    let cutoff = ui.get(Role::Slider, "Cutoff");
    ui.move_to(cutoff.center());
    let mut moving = 0;
    let mut drawn = 0;
    let start = ui.now();
    loop {
        drawn += ui.frames_rendered_during(|ui| ui.advance_ms(16));
        for rect in ui.last_damage() {
            assert!(
                cutoff.device.contains_rect(rect),
                "{rect:?} is outside the knob {:?}",
                cutoff.device
            );
        }
        if !ui.root().is_animating() {
            break;
        }
        moving += 1;
        assert!(
            ui.now() - start < Duration::from_millis(400),
            "never settles"
        );
    }
    // The fast token is 120 ms: about seven frames of 16 ms move it, each
    // drawn.
    assert!((5..=9).contains(&moving), "{moving} frames moving");
    assert!(drawn >= 5, "{drawn} frames drawn");
    // Then nothing: no frames wanted, nothing drawn.
    assert_eq!(ui.frames_rendered_during(|ui| ui.advance_ms(500)), 0);
    assert!(!ui.root().is_animating());
    // What the frames built up equals a fresh render of the settled scene.
    assert_eq!(ui.pixels(), &ui.render_fresh());
}

#[test]
fn with_motion_off_a_change_jumps_in_one_frame() {
    let params = params();
    for (system, theme_motion) in [(false, true), (true, false)] {
        let mut ui = knobs(&params);
        ui.set_motion(system);
        let mut theme = Theme::dark();
        theme.motion.enabled = theme_motion;
        ui.set_theme(theme);
        ui.frame();
        let cutoff = ui.get(Role::Slider, "Cutoff");
        ui.move_to(cutoff.center());
        let frame = ui.frame();
        assert!(!frame.animating);
        assert!(frame.damaged > 0);
        assert_eq!(ui.root().motion().fast().duration, Duration::ZERO);
        ui.advance_ms(16);
        assert_eq!(ui.frame().damaged, 0);
    }
}

#[test]
fn frames_at_the_same_times_draw_the_same_things() {
    let params = params();
    let record = || {
        let mut ui = knobs(&params);
        let resonance = ui.get(Role::Slider, "Resonance");
        let mut dumps = Vec::new();
        ui.move_to(resonance.center());
        for _ in 0..6 {
            ui.advance_ms(13);
            dumps.push(ui.dump());
        }
        ui.move_to(Point::new(250.0, 5.0));
        for _ in 0..6 {
            ui.advance_ms(21);
            dumps.push(ui.dump());
        }
        dumps
    };
    assert_eq!(record(), record());
}

#[test]
fn the_focus_ring_eases_in_after_the_keyboard_and_not_after_a_press() {
    let params = params();
    let mut ui = knobs(&params);
    let resonance = ui.get(Role::Slider, "Resonance");
    ui.click(resonance.center());
    ui.advance_ms(300);
    assert!(ui.get(Role::Slider, "Resonance").is_focused());
    assert!(!ui.root().focus_visible());
    // The keyboard shows it, over the base duration.
    ui.tab();
    assert!(ui.root().focus_visible());
    let mut moving = 0;
    loop {
        ui.advance_ms(16);
        if !ui.root().is_animating() {
            break;
        }
        moving += 1;
    }
    assert!((9..=13).contains(&moving), "{moving} frames moving");
    // A press hides it again.
    let drive = ui.get(Role::Slider, "Drive");
    ui.press(drive.center());
    assert!(!ui.root().focus_visible());
    ui.release(drive.center());
}
