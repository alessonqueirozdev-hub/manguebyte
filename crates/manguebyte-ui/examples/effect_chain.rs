//! An effect chain of pills that open floating panels, driven headless
//! and drawn into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example effect_chain
//! cargo run -p manguebyte-ui --example effect_chain -- <folder>
//! ```
//!
//! The screen Parabolic plans for its effects: the chain as a row of
//! pills, each with a light that says whether the effect is on, and a
//! keyboard under it. A pill opens its effect's panel over the screen,
//! inside the window; several stay open, each where it was put, and the
//! keyboard goes on playing under and beside them.
//!
//! The program opens three panels from their pills and the keys, drags
//! one by its title bar, plays a note on the keyboard beside them, brings
//! the panel behind to the front with a click, and closes one with
//! Escape. It writes the screens in both themes at 2x
//! (`panels-dark.png`, `panels-moved-dark.png`, `panels-front-dark.png`,
//! `panels-closed-dark.png`, and the same for `light`) into the folder given, or the system's temporary folder, and
//! prints what the application polls from the states it shares with the
//! panels: which are open and where. Nothing calls the application back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use manguebyte_gfx::{Point, Size};
use manguebyte_params::{BoolParam, FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_ui::layout::Align;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    IntoView, Key, KeySpan, KeyboardState, ParamHandle, Role, State, Theme, View, card, column,
    floating_panel, knob, label, piano_keyboard, pill, row, stack, toggle,
};

/// The screen's size in logical pixels.
const SIZE: Size = Size::new(1000.0, 600.0);

/// The scale the pictures are drawn at.
const SCALE: f32 = 2.0;

struct FxParams {
    drive: FloatParam,
    tone: FloatParam,
    rate: FloatParam,
    depth: FloatParam,
    time: FloatParam,
    feedback: FloatParam,
    size: FloatParam,
    decay: FloatParam,
    mix: FloatParam,
    ping_pong: BoolParam,
    freeze: BoolParam,
}

param_set!(FxParams {
    drive,
    tone,
    rate,
    depth,
    time,
    feedback,
    size,
    decay,
    mix,
    ping_pong,
    freeze
});

fn fx_params() -> Arc<FxParams> {
    let percent = |id: &'static str, name: &'static str, value: f32| {
        FloatParam::new(id, name, FloatRange::linear(0.0, 100.0), value)
            .with_unit("%")
            .with_precision(0)
    };
    Arc::new(FxParams {
        drive: FloatParam::new("drive", "Drive", FloatRange::linear(0.0, 24.0), 9.0)
            .with_unit("dB")
            .with_precision(1),
        tone: percent("tone", "Tone", 60.0),
        rate: FloatParam::new("rate", "Rate", FloatRange::logarithmic(0.05, 10.0), 0.8)
            .with_unit("Hz")
            .with_precision(2),
        depth: percent("depth", "Depth", 35.0),
        time: FloatParam::new("time", "Time", FloatRange::linear(1.0, 2000.0), 375.0)
            .with_unit("ms")
            .with_precision(0),
        feedback: percent("feedback", "Feedback", 40.0),
        size: percent("size", "Size", 70.0),
        decay: FloatParam::new("decay", "Decay", FloatRange::linear(0.1, 20.0), 3.2)
            .with_unit("s")
            .with_precision(1),
        mix: percent("mix", "Mix", 25.0),
        ping_pong: BoolParam::new("ping_pong", "Ping-pong", true),
        freeze: BoolParam::new("freeze", "Freeze", false),
    })
}

/// One effect of the chain: its name, whether it is on, its panel's state.
struct Effect {
    name: &'static str,
    on: bool,
    open: State<bool>,
    at: State<Point>,
}

/// The chain, as a project keeps it.
fn chain() -> Vec<Effect> {
    let effect = |name, on, at: (f32, f32)| Effect {
        name,
        on,
        open: State::new(false),
        at: State::new(Point::new(at.0, at.1)),
    };
    vec![
        effect("Drive", true, (40.0, 130.0)),
        effect("Chorus", true, (250.0, 150.0)),
        effect("Phaser", false, (300.0, 170.0)),
        effect("Delay", true, (330.0, 140.0)),
        effect("Reverb", true, (600.0, 190.0)),
        effect("Width", false, (700.0, 140.0)),
        effect("Limiter", true, (760.0, 180.0)),
    ]
}

/// What each effect's panel shows.
fn content(name: &str, params: &Arc<FxParams>) -> View {
    let k = |get: fn(&FxParams) -> &FloatParam| knob(ParamHandle::new(Arc::clone(params), get));
    let t = |get: fn(&FxParams) -> &BoolParam| toggle(ParamHandle::new(Arc::clone(params), get));
    match name {
        "Drive" => row((k(|p| &p.drive), k(|p| &p.tone))).spacing(8.0),
        "Chorus" | "Phaser" => row((k(|p| &p.rate), k(|p| &p.depth))).spacing(8.0),
        "Delay" => column((
            row((k(|p| &p.time), k(|p| &p.feedback), k(|p| &p.mix))).spacing(8.0),
            t(|p| &p.ping_pong),
        ))
        .spacing(8.0),
        "Reverb" => column((
            row((k(|p| &p.size), k(|p| &p.decay))).spacing(8.0),
            t(|p| &p.freeze),
        ))
        .spacing(8.0),
        _ => label("No settings yet").muted().into_view(),
    }
}

fn screen(params: &Arc<FxParams>, effects: &[Effect], keys: &KeyboardState) -> View {
    let palette = Theme::dark().palette;
    let pills: Vec<View> = effects
        .iter()
        .map(|effect| {
            let light = if effect.on {
                palette.success
            } else {
                palette.text_muted
            };
            pill(effect.name).dot(light).opens(&effect.open).into_view()
        })
        .collect();
    let panels = effects.iter().map(|effect| {
        floating_panel(
            effect.name,
            effect.open.clone(),
            effect.at.clone(),
            content(effect.name, params),
        )
    });
    column((
        label("Effects").title(),
        row(pills).spacing(6.0),
        stack(()).flex(1.0),
        card(piano_keyboard(
            "Keyboard",
            KeySpan::new(36, 84),
            keys.clone(),
        ))
        .height(170.0)
        .align_self(Align::Stretch),
    ))
    .spacing(14.0)
    .padding(20.0)
    .align(Align::Start)
    .with_panels(panels)
}

fn save(ui: &Harness, folder: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = folder.join(name);
    let png = ui
        .pixels()
        .encode_png()
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    std::fs::write(&path, png)?;
    Ok(path)
}

/// Runs frames until nothing moves.
fn settle(ui: &mut Harness) {
    for _ in 0..200 {
        ui.advance_ms(16);
        if !ui.root().is_animating() {
            break;
        }
    }
}

fn report(look: &str, effects: &[Effect]) {
    let open: Vec<String> = effects
        .iter()
        .filter(|effect| effect.open.get())
        .map(|effect| {
            let at = effect.at.get();
            format!("{} at ({}, {})", effect.name, at.x, at.y)
        })
        .collect();
    println!("{look}: open: {}", open.join(", "));
}

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);
    std::fs::create_dir_all(&folder)?;
    let mut written = Vec::new();
    for (theme, look) in [(Theme::dark(), "dark"), (Theme::light(), "light")] {
        let params = fx_params();
        let effects = chain();
        let keys = KeyboardState::new();
        let mut ui = Harness::with_params(
            screen(&params, &effects, &keys),
            SIZE,
            SCALE,
            Arc::clone(&params) as Arc<dyn ParamSet>,
        );
        ui.set_theme(theme);
        ui.set_motion(true);
        settle(&mut ui);

        // Drive and Reverb from their pills, Delay from the keyboard: Tab
        // to its pill and Enter, which leaves the focus ring on its first
        // knob.
        ui.click(ui.get(Role::Button, "Drive").center());
        ui.click(ui.get(Role::Button, "Reverb").center());
        settle(&mut ui);
        ui.root_mut().set_focus_to(None);
        for _ in 0..4 {
            ui.tab();
        }
        ui.key(Key::Enter);
        settle(&mut ui);
        report(look, &effects);
        written.push(save(&ui, &folder, &format!("panels-{look}.png"))?);

        // The reverb dragged by its title bar down and left, over the
        // delay, and a note played on the keyboard beside the panels.
        let reverb = ui.get(Role::Region, "Reverb");
        let bar = Point::new(reverb.bounds.x + 30.0, reverb.bounds.y + 16.0);
        ui.press(bar);
        for step in 1..=8 {
            ui.move_to(Point::new(
                bar.x - 16.0 * step as f32,
                bar.y + 10.0 * step as f32,
            ));
            ui.advance_ms(16);
        }
        ui.release(Point::new(bar.x - 128.0, bar.y + 80.0));
        let keyboard = ui.get(Role::Group, "Keyboard");
        ui.press(Point::new(
            keyboard.bounds.x + 30.0,
            keyboard.bounds.bottom() - 20.0,
        ));
        settle(&mut ui);
        println!("{look}: notes held: {:?}", keys.down());
        report(look, &effects);
        written.push(save(&ui, &folder, &format!("panels-moved-{look}.png"))?);
        ui.release(Point::new(
            keyboard.bounds.x + 30.0,
            keyboard.bounds.bottom() - 20.0,
        ));

        // The delay, now behind the reverb, to the front with a click on
        // its title bar; then Escape closes it from a knob inside it.
        let delay = ui.get(Role::Region, "Delay");
        ui.click(Point::new(delay.bounds.x + 30.0, delay.bounds.y + 16.0));
        ui.move_to(Point::new(SIZE.width - 10.0, 80.0));
        settle(&mut ui);
        written.push(save(&ui, &folder, &format!("panels-front-{look}.png"))?);
        let time = ui.get(Role::Slider, "Time");
        ui.act(&time, manguebyte_ui::Action::Focus);
        ui.key(Key::Escape);
        settle(&mut ui);
        report(look, &effects);
        println!(
            "{look}: the focus is back on {:?}",
            ui.focused().map(|found| found.node.name)
        );
        written.push(save(&ui, &folder, &format!("panels-closed-{look}.png"))?);
    }
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}
