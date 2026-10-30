//! Golden images and dumps of a small panel: three knobs, a slider, a
//! switch, two meters, a title and a canvas, at 1x and 2x, and in the
//! light theme.
//!
//! Each dump must match exactly; images may differ by up to [`TOLERANCE`]
//! in any channel, which `manguebyte-gfx` allows for the rasteriser's
//! rounding on other processors. To regenerate the files after an
//! intended change, run locally:
//!
//! ```text
//! MANGUEBYTE_BLESS=1 cargo test -p manguebyte-ui --test golden
//! ```
//!
//! and look at the new images before committing them. With `CI` set,
//! blessing is refused.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use manguebyte_gfx::snapshot::Snapshots;
use manguebyte_gfx::{Point, Rect, RoundedRect, Size};
use manguebyte_params::{ChoiceParam, param_set};
use manguebyte_ui::layout::Align;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    Action, DialogState, Icon, IntoView, Key, Modifiers, ParamHandle, Role, State, Theme, WaveData,
    WheelState, action_button, card, column, combo, confirm_dialog, floating_panel, knob, label,
    mod_wheel, pill, pitch_wheel, row, sidebar, tabs, text_field, toggle, wave_view,
    wavetable_view,
};

use common::{PANEL, levels, panel_view, params};

/// The same tolerance as `manguebyte-gfx`'s goldens: 0 was measured on
/// x86-64 with and without SIMD; 1 allows for aarch64's rounding.
const TOLERANCE: u8 = 1;

fn snapshots() -> Snapshots {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ui-golden-failures");
    Snapshots::new(dir, failures).with_tolerance(TOLERANCE)
}

/// The panel with levels on its meters, a switch turned on, a knob moved
/// and the focus on another knob, drawn in several frames.
fn panel(scale: f32, theme: Theme) -> Harness {
    let params = params();
    let levels = levels();
    let mut ui = Harness::with_params(
        panel_view(&params, &levels),
        PANEL,
        scale,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.set_theme(theme);
    ui.frame();
    levels.left.store(-12.0, Ordering::Relaxed);
    levels.right.store(-3.5, Ordering::Relaxed);
    ui.advance_ms(16);
    let bypass = ui.get(Role::Switch, "Bypass");
    ui.act(&bypass, Action::Press);
    let cutoff = ui.get(Role::Slider, "Cutoff");
    ui.drag(cutoff.center(), (0.0, -30.0), 3);
    ui.advance_ms(16);
    let drive = ui.get(Role::Slider, "Drive");
    ui.act(&drive, Action::Focus);
    ui.advance_ms(16);
    ui
}

#[test]
fn the_panel_matches_its_goldens_at_1x_and_2x() {
    let snapshots = snapshots();
    for (name, scale, theme) in [
        ("panel@1x", 1.0, Theme::dark()),
        ("panel@2x", 2.0, Theme::dark()),
        ("panel-light@1x", 1.0, Theme::light()),
    ] {
        let ui = panel(scale, theme);
        // Drawn in several partial frames, the buffer is what one whole
        // render of the final scene gives.
        assert!(
            ui.pixels() == &ui.render_fresh(),
            "{name}: partial and whole renders differ"
        );
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
}

#[test]
fn the_panel_shows_what_its_goldens_were_made_from() {
    let ui = panel(1.0, Theme::dark());
    assert_eq!(ui.get(Role::Meter, "Left").value_text(), Some("-12.0 dB"));
    assert_eq!(ui.get(Role::Meter, "Right").value_text(), Some("-3.5 dB"));
    assert_eq!(
        ui.get(Role::Switch, "Bypass").node.state.checked,
        Some(true)
    );
    assert!(ui.get(Role::Slider, "Drive").is_focused());
    // 30 pixels up from 1000 Hz is 0.15 of a logarithmic range, which
    // multiplies the frequency by 1000 to the power of 0.15.
    assert_eq!(ui.get(Role::Slider, "Cutoff").value_text(), Some("2818 Hz"));
}

/// A pitch bend wheel held half way up and a modulation wheel at 0.6 with
/// the keyboard's focus ring.
fn wheels(scale: f32, theme: Theme) -> Harness {
    let (bend, modulation) = (WheelState::new(), WheelState::new());
    let view = row((
        pitch_wheel("Pitch", bend.clone()),
        mod_wheel("Mod", modulation.clone()),
    ))
    .spacing(4.0)
    .padding(8.0);
    let mut ui = Harness::new(view, Size::new(112.0, 176.0), scale);
    ui.set_theme(theme);
    ui.frame();
    modulation.set(0.6);
    // Held: pressed and dragged up, not let go.
    let pitch = ui.get(Role::Slider, "Pitch");
    ui.press(pitch.center());
    ui.move_to(pitch.center() - manguebyte_gfx::Point::new(0.0, 30.0));
    ui.advance_ms(16);
    let wheel = ui.get(Role::Slider, "Mod");
    ui.act(&wheel, Action::Focus);
    ui.advance_ms(16);
    assert!(bend.is_held() && bend.value() > 0.4);
    ui
}

#[test]
fn the_wheels_match_their_goldens() {
    let snapshots = snapshots();
    for (name, scale, theme) in [
        ("wheels@1x", 1.0, Theme::dark()),
        ("wheels@2x", 2.0, Theme::dark()),
        ("wheels-light@1x", 1.0, Theme::light()),
        ("wheels-light@2x", 2.0, Theme::light()),
    ] {
        let ui = wheels(scale, theme);
        assert!(ui.pixels() == &ui.render_fresh(), "{name}");
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
}

/// One cycle and a wavetable of 12 frames from a sine to a saw, built with
/// `manguebyte_dsp::Wavetable`, at a position between two frames.
fn oscillators(scale: f32, theme: Theme) -> Harness {
    let frames: Vec<Vec<f32>> = (0..12)
        .map(|frame| {
            let t = frame as f32 / 11.0;
            (0..512)
                .map(|i| {
                    let phase = i as f32 / 512.0 * std::f32::consts::TAU;
                    let saw: f32 = (1..=24)
                        .map(|h| sin(phase * h as f32) / h as f32)
                        .sum::<f32>()
                        * 0.55;
                    sin(phase) + (saw - sin(phase)) * t
                })
                .collect()
        })
        .collect();
    let table = manguebyte_dsp::Wavetable::new(&frames).expect("a wavetable");
    let data = WaveData::from_wavetable(&table);
    // On a card, whose surface the stack hides what is behind with.
    let view = card(
        row((
            wave_view("Cycle", data.clone())
                .position(|| 7.5)
                .width(150.0),
            wavetable_view("Table", data, || 7.5).flex(1.0),
        ))
        .spacing(8.0),
    )
    .padding(12.0);
    let mut ui = Harness::new(view, Size::new(440.0, 180.0), scale);
    ui.set_theme(theme);
    ui.frame();
    ui
}

#[test]
fn the_oscillator_views_match_their_goldens() {
    let snapshots = snapshots();
    for (name, scale, theme) in [
        ("oscillators@1x", 1.0, Theme::dark()),
        ("oscillators@2x", 2.0, Theme::dark()),
        ("oscillators-light@1x", 1.0, Theme::light()),
        ("oscillators-light@2x", 2.0, Theme::light()),
    ] {
        let ui = oscillators(scale, theme);
        assert!(ui.pixels() == &ui.render_fresh(), "{name}");
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
}

struct Choices {
    shape: ChoiceParam,
}
param_set!(Choices { shape });

/// A small form: a title, a field being typed into with a word selected,
/// an empty field showing its placeholder, a combo box and a knob.
fn form(scale: f32, theme: Theme) -> (Harness, Arc<common::PanelParams>) {
    let params = params();
    let choices = Arc::new(Choices {
        shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square", "Noise"], 1),
    });
    let view = column((
        label("Preset").title(),
        text_field("Name", State::new(String::from("Warm pad"))),
        text_field("Notes", State::new(String::new())).placeholder("Notes"),
        row((
            combo(ParamHandle::new(Arc::clone(&choices), |p| &p.shape)),
            knob(ParamHandle::new(Arc::clone(&params), |p| &p.cutoff)),
        ))
        .spacing(8.0)
        .align(Align::Start),
    ))
    .spacing(6.0)
    .padding(10.0)
    .align(Align::Start);
    let mut ui = Harness::new(view, Size::new(220.0, 240.0), scale);
    ui.set_theme(theme);
    ui.frame();
    (ui, params)
}

#[test]
fn text_and_menus_match_their_goldens() {
    let snapshots = snapshots();
    let control_shift = Modifiers {
        control: true,
        shift: true,
        ..Modifiers::NONE
    };
    for (name, scale, theme) in [
        ("typing@1x", 1.0, Theme::dark()),
        ("typing@2x", 2.0, Theme::dark()),
        ("typing-light@1x", 1.0, Theme::light()),
    ] {
        let (mut ui, _) = form(scale, theme);
        // Tab selects everything; End, then Control+Shift+Left selects the
        // last word.
        ui.tab();
        ui.key(Key::End);
        ui.key_with(Key::Left, control_shift);
        ui.advance_ms(16);
        assert!(ui.pixels() == &ui.render_fresh(), "{name}");
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
    {
        // A value being typed into the knob, its caret after "25".
        let (mut ui, _) = form(1.0, Theme::dark());
        let cutoff = ui.get(Role::Slider, "Cutoff");
        ui.double_click(cutoff.center());
        ui.type_text("25");
        ui.advance_ms(16);
        assert_eq!(ui.get(Role::TextInput, "Cutoff").value_text(), Some("25"));
        assert!(ui.pixels() == &ui.render_fresh());
        if let Err(mismatch) = snapshots.check("entry@1x", ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
    for (name, scale, theme) in [
        ("menu@1x", 1.0, Theme::dark()),
        ("menu@2x", 2.0, Theme::dark()),
        ("menu-light@1x", 1.0, Theme::light()),
    ] {
        // The combo box open, with the item after the checked one
        // highlighted.
        let (mut ui, _) = form(scale, theme);
        let shape = ui.get(Role::ComboBox, "Shape");
        ui.click(shape.center());
        ui.key(Key::Down);
        ui.advance_ms(16);
        assert!(ui.get(Role::MenuItem, "Square").is_focused(), "{name}");
        assert!(ui.pixels() == &ui.render_fresh(), "{name}");
        if let Err(mismatch) = snapshots.check(name, ui.pixels(), &ui.dump()) {
            panic!("{mismatch}");
        }
    }
}

/// An icon of organ pipes: three upright bars of different heights.
fn pipes() -> Icon {
    Icon::new(|scene, square, color| {
        let width = square.width / 5.0;
        for (at, height) in [(0.0, 0.6), (2.0, 1.0), (4.0, 0.8)] {
            let tall = square.height * height;
            let bar = Rect::new(square.x + at * width, square.bottom() - tall, width, tall);
            scene.fill(RoundedRect::new(bar, width * 0.5), color);
        }
    })
}

/// A tab bar over its pages, the second shown, the bar focused from the
/// keyboard and the pointer over the third tab.
fn tab_bar(scale: f32, theme: Theme) -> Harness {
    let view = tabs("Sections", State::new(1))
        .page("Synth", label("Oscillators"))
        .page("Effects", label("Reverb and delay"))
        .page("Presets", label("Init"))
        .padding(8.0);
    let mut ui = Harness::new(view, Size::new(320.0, 110.0), scale);
    ui.set_theme(theme);
    ui.frame();
    assert!(ui.tab());
    let presets = ui.get(Role::Tab, "Presets");
    ui.move_to(presets.center());
    ui.advance_ms(16);
    ui
}

/// A sidebar of an organ's divisions under two headers, with icons, the
/// second division shown and the sidebar focused from the keyboard.
fn division_sidebar(scale: f32, theme: Theme) -> Harness {
    let view = sidebar("Divisions", State::new(1))
        .section("Manuals")
        .page("Great", label("Great stops"))
        .icon(pipes())
        .page("Swell", label("Swell stops"))
        .icon(pipes())
        .page("Choir", label("Choir stops"))
        .icon(pipes())
        .section("Pedal")
        .page("Pedal", label("Pedal stops"))
        .icon(pipes());
    let mut ui = Harness::new(view, Size::new(300.0, 230.0), scale);
    ui.set_theme(theme);
    ui.frame();
    assert!(ui.tab());
    ui.advance_ms(16);
    ui
}

/// An icon of a bin: a lid and a body.
fn bin() -> Icon {
    Icon::new(|scene, square, color| {
        let unit = square.width / 16.0;
        let lid = Rect::new(
            square.x + 2.0 * unit,
            square.y + 3.0 * unit,
            12.0 * unit,
            1.5 * unit,
        );
        let body = Rect::new(
            square.x + 4.0 * unit,
            square.y + 5.5 * unit,
            8.0 * unit,
            9.0 * unit,
        );
        scene.fill(RoundedRect::new(lid, 0.75 * unit), color);
        scene.fill(RoundedRect::new(body, 1.5 * unit), color);
    })
}

/// Action buttons in their three looks, one with an icon and one
/// disabled; the pointer over the primary one and the secondary one
/// focused from the keyboard.
fn button_row(scale: f32, theme: Theme) -> Harness {
    let view = column((
        row((
            action_button("Cancel"),
            action_button("Export plugin").primary(),
            action_button("Delete").destructive().icon(bin()),
        ))
        .spacing(10.0),
        row((
            action_button("Render").primary().disabled(),
            action_button("Undo").disabled(),
        ))
        .spacing(10.0),
    ))
    .spacing(10.0)
    .padding(14.0)
    .align(Align::Start);
    let mut ui = Harness::new(view, Size::new(380.0, 120.0), scale);
    ui.set_theme(theme);
    ui.frame();
    assert!(ui.tab());
    let export = ui.get(Role::Button, "Export plugin");
    ui.move_to(export.center());
    ui.advance_ms(16);
    ui
}

#[test]
fn action_buttons_match_their_goldens() {
    let snapshots = snapshots();
    for (scale, suffix) in [(1.0, "@1x"), (2.0, "@2x")] {
        for (theme, look) in [(Theme::dark(), ""), (Theme::light(), "-light")] {
            let ui = button_row(scale, theme);
            let name = format!("buttons{look}{suffix}");
            ui.assert_accessible();
            assert!(ui.get(Role::Button, "Cancel").is_focused(), "{name}");
            assert!(ui.pixels() == &ui.render_fresh(), "{name}");
            if let Err(mismatch) = snapshots.check(&name, ui.pixels(), &ui.dump()) {
                panic!("{mismatch}");
            }
        }
    }
}

/// A confirmation dialog open over a small screen, its default button
/// focused from the keyboard.
fn open_dialog(scale: f32, theme: Theme) -> Harness {
    let ask = DialogState::new();
    let view = column((
        label("Presets").title(),
        row((
            action_button("Save"),
            action_button("Delete preset").opens(&ask),
        ))
        .spacing(8.0),
    ))
    .spacing(10.0)
    .padding(16.0)
    .align(Align::Start)
    .with_dialog(
        confirm_dialog(
            ask.clone(),
            "Delete “Glass bells”?",
            "The preset is removed from the library and from every project that uses it. \
             This cannot be undone.",
        )
        .confirm_label("Delete")
        .destructive(),
    );
    let mut ui = Harness::new(view, Size::new(520.0, 300.0), scale);
    ui.set_theme(theme);
    ui.frame();
    assert!(ui.tab());
    assert!(ui.tab());
    ui.key(Key::Enter);
    // The frame that opens it gives its default button the focus, and
    // shows its ring.
    ui.advance_ms(16);
    ui
}

#[test]
fn the_dialog_matches_its_goldens() {
    let snapshots = snapshots();
    for (scale, suffix) in [(1.0, "@1x"), (2.0, "@2x")] {
        for (theme, look) in [(Theme::dark(), ""), (Theme::light(), "-light")] {
            let ui = open_dialog(scale, theme);
            let name = format!("dialog{look}{suffix}");
            ui.assert_accessible();
            assert!(ui.find(Role::Dialog, "Delete “Glass bells”?").is_some());
            assert!(ui.get(Role::Button, "Delete").is_focused(), "{name}");
            assert!(ui.pixels() == &ui.render_fresh(), "{name}");
            if let Err(mismatch) = snapshots.check(&name, ui.pixels(), &ui.dump()) {
                panic!("{mismatch}");
            }
        }
    }
}

#[test]
fn tabs_and_sidebars_match_their_goldens() {
    let snapshots = snapshots();
    for (scale, suffix) in [(1.0, "@1x"), (2.0, "@2x")] {
        for (theme, look) in [(Theme::dark(), ""), (Theme::light(), "-light")] {
            for (kind, ui) in [
                ("tabs", tab_bar(scale, theme)),
                ("sidebar", division_sidebar(scale, theme)),
            ] {
                let name = format!("{kind}{look}{suffix}");
                ui.assert_accessible();
                assert!(ui.pixels() == &ui.render_fresh(), "{name}");
                if let Err(mismatch) = snapshots.check(&name, ui.pixels(), &ui.dump()) {
                    panic!("{mismatch}");
                }
            }
        }
    }
}

/// The sine of `angle`, the same bits on every platform: `f32::sin` rounds
/// differently on Windows, Linux and macOS, which would change the data the
/// goldens are drawn from.
fn sin(angle: f32) -> f32 {
    manguebyte_gfx::sin_cos(angle).0 as f32
}

/// An effect chain of pills with two panels open over it, overlapping:
/// the delay's in front, a knob in it focused from the keyboard.
fn open_panels(scale: f32, theme: Theme) -> Harness {
    let params = params();
    let (reverb, delay) = (State::new(true), State::new(false));
    let p = |get: fn(&common::PanelParams) -> &manguebyte_params::FloatParam| {
        knob(ParamHandle::new(Arc::clone(&params), get))
    };
    let view = column((
        label("Effects").title(),
        row((
            pill("Drive"),
            pill("Reverb").opens(&reverb),
            pill("Delay").opens(&delay),
        ))
        .spacing(6.0),
    ))
    .spacing(10.0)
    .padding(16.0)
    .align(Align::Start)
    .with_panels([
        floating_panel(
            "Reverb",
            reverb.clone(),
            State::new(Point::new(36.0, 96.0)),
            row((p(|p| &p.resonance), p(|p| &p.drive))).spacing(8.0),
        ),
        floating_panel(
            "Delay",
            delay.clone(),
            State::new(Point::new(196.0, 150.0)),
            column((
                row((p(|p| &p.cutoff), p(|p| &p.mix))).spacing(8.0),
                toggle(ParamHandle::new(Arc::clone(&params), |p| &p.bypass)),
            ))
            .spacing(8.0),
        ),
    ]);
    let mut ui = Harness::with_params(
        view,
        Size::new(480.0, 350.0),
        scale,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.set_theme(theme);
    ui.frame();
    // From the keyboard: Tab to the delay's pill and Enter, which opens it
    // in front with the focus, and its ring, on its first knob.
    for _ in 0..3 {
        ui.tab();
    }
    ui.key(Key::Enter);
    ui.advance_ms(16);
    ui
}

#[test]
fn floating_panels_match_their_goldens() {
    let snapshots = snapshots();
    for (scale, suffix) in [(1.0, "@1x"), (2.0, "@2x")] {
        for (theme, look) in [(Theme::dark(), ""), (Theme::light(), "-light")] {
            let ui = open_panels(scale, theme);
            let name = format!("panels{look}{suffix}");
            ui.assert_accessible();
            assert!(ui.find(Role::Region, "Reverb").is_some(), "{name}");
            assert!(ui.get(Role::Slider, "Cutoff").is_focused(), "{name}");
            assert!(ui.pixels() == &ui.render_fresh(), "{name}");
            if let Err(mismatch) = snapshots.check(&name, ui.pixels(), &ui.dump()) {
                panic!("{mismatch}");
            }
        }
    }
}
