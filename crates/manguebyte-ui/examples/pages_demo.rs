//! Pages in one window, driven headless and drawn into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example pages_demo
//! cargo run -p manguebyte-ui --example pages_demo -- <folder>
//! ```
//!
//! Two screens. The first is a plugin generator's, as Parabolic plans
//! it: a title bar, and tabs for its Synth, Effects and Presets pages,
//! each a set of cards; the presets page deletes a preset after a
//! confirmation dialog. The second is an organ console's, as Caecilia
//! 2.0 plans it: a sidebar of divisions under section headers, each
//! division a card of stops.
//!
//! The program switches pages by clicking tabs and with the keys, opens
//! the dialog from the Delete button and answers it, and writes the
//! screens in both themes at 2x (`nav-synth-dark.png`,
//! `nav-dialog-light.png`, `nav-organ-dark.png` and so on) into the folder
//! given, or the system's temporary folder. It prints what the
//! application polled from the states it shares with the widgets: the page
//! shown and the dialog's answer. Nothing calls the application back.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use manguebyte_gfx::{Circle, Point, Rect, RoundedRect, Scene, Size, Stroke};
use manguebyte_params::{BoolParam, FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_ui::layout::{Align, Justify};
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{
    DialogResult, DialogState, Icon, IntoView, Key, ListData, ListState, Modifiers, ParamHandle,
    Role, State, Theme, View, action_button, card, column, confirm_dialog, knob, label, list_view,
    pill, row, sidebar, tabs, toggle,
};

/// Both screens' size in logical pixels.
const SIZE: Size = Size::new(1000.0, 600.0);

/// The scale the pictures are drawn at.
const SCALE: f32 = 2.0;

struct SynthParams {
    shape: FloatParam,
    detune: FloatParam,
    cutoff: FloatParam,
    resonance: FloatParam,
    drive: FloatParam,
    attack: FloatParam,
    release: FloatParam,
    size: FloatParam,
    mix: FloatParam,
    time: FloatParam,
    feedback: FloatParam,
    chorus: BoolParam,
    ping_pong: BoolParam,
}

param_set!(SynthParams {
    shape,
    detune,
    cutoff,
    resonance,
    drive,
    attack,
    release,
    size,
    mix,
    time,
    feedback,
    chorus,
    ping_pong
});

fn synth_params() -> Arc<SynthParams> {
    let percent = |id: &'static str, name: &'static str, value: f32| {
        FloatParam::new(id, name, FloatRange::linear(0.0, 100.0), value)
            .with_unit("%")
            .with_precision(0)
    };
    Arc::new(SynthParams {
        shape: percent("shape", "Shape", 35.0),
        detune: FloatParam::new("detune", "Detune", FloatRange::linear(0.0, 50.0), 12.0)
            .with_unit("ct")
            .with_precision(0),
        cutoff: FloatParam::new(
            "cutoff",
            "Cutoff",
            FloatRange::logarithmic(20.0, 20_000.0),
            1_800.0,
        )
        .with_unit("Hz")
        .with_precision(0),
        resonance: percent("resonance", "Resonance", 30.0),
        drive: FloatParam::new("drive", "Drive", FloatRange::linear(0.0, 24.0), 6.0)
            .with_unit("dB")
            .with_precision(1),
        attack: FloatParam::new("attack", "Attack", FloatRange::linear(0.0, 2000.0), 12.0)
            .with_unit("ms")
            .with_precision(0),
        release: FloatParam::new("release", "Release", FloatRange::linear(0.0, 4000.0), 640.0)
            .with_unit("ms")
            .with_precision(0),
        size: percent("size", "Size", 70.0),
        mix: percent("mix", "Mix", 25.0),
        time: FloatParam::new("time", "Time", FloatRange::linear(10.0, 2000.0), 375.0)
            .with_unit("ms")
            .with_precision(0),
        feedback: percent("feedback", "Feedback", 40.0),
        chorus: BoolParam::new("chorus", "Chorus", true),
        ping_pong: BoolParam::new("ping_pong", "Ping-pong", false),
    })
}

/// A wave: the synth's icon.
fn wave() -> Icon {
    Icon::new(|scene: &mut Scene, square: Rect, color| {
        let mut path = manguebyte_gfx::Path::new();
        let (x, y, w, h) = (square.x, square.center().y, square.width, square.height);
        path.move_to(Point::new(x + 1.0, y));
        path.cubic_to(
            Point::new(x + w * 0.25, y - h * 0.55),
            Point::new(x + w * 0.25, y + h * 0.55),
            Point::new(x + w * 0.5, y),
        );
        path.cubic_to(
            Point::new(x + w * 0.75, y - h * 0.55),
            Point::new(x + w * 0.75, y + h * 0.55),
            Point::new(x + w - 1.0, y),
        );
        scene.stroke(&path, Stroke::round(1.6), color);
    })
}

/// Three sliders: the effects' icon.
fn sliders() -> Icon {
    Icon::new(|scene: &mut Scene, square: Rect, color| {
        for (at, knob_at) in [(0.2, 0.35), (0.5, 0.7), (0.8, 0.45)] {
            let x = square.x + square.width * at;
            scene.fill(
                Rect::new(x - 0.6, square.y + 1.5, 1.2, square.height - 3.0),
                color.with_opacity(0.55),
            );
            scene.fill(
                Circle::new(Point::new(x, square.y + square.height * knob_at), 2.2),
                color,
            );
        }
    })
}

/// A folder: the presets' icon.
fn folder() -> Icon {
    Icon::new(|scene: &mut Scene, square: Rect, color| {
        let body = Rect::new(
            square.x + 1.0,
            square.y + 4.0,
            square.width - 2.0,
            square.height - 6.0,
        );
        let tab = Rect::new(square.x + 1.0, square.y + 2.5, square.width * 0.45, 3.0);
        scene.fill(RoundedRect::new(tab, 1.0), color);
        scene.stroke(RoundedRect::new(body, 2.0), Stroke::new(1.5), color);
    })
}

/// A bin: what deletes.
fn bin() -> Icon {
    Icon::new(|scene: &mut Scene, square: Rect, color| {
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

/// Organ pipes: a division's icon.
fn pipes() -> Icon {
    Icon::new(|scene: &mut Scene, square: Rect, color| {
        let width = square.width / 5.0;
        for (at, height) in [(0.0, 0.6), (2.0, 1.0), (4.0, 0.8)] {
            let tall = square.height * height;
            let bar = Rect::new(square.x + at * width, square.bottom() - tall, width, tall);
            scene.fill(RoundedRect::new(bar, width * 0.5), color);
        }
    })
}

/// A card's heading and its contents.
fn section(title: &str, contents: View) -> View {
    card(column((label(title).heading(), contents)).spacing(12.0)).into_view()
}

/// The presets the presets page lists.
struct Presets;

const PRESETS: [&str; 7] = [
    "Init",
    "Warm pad",
    "Glass bells",
    "Rubber bass",
    "Night drive",
    "Choir of saws",
    "Tape keys",
];

impl ListData for Presets {
    fn len(&self) -> usize {
        PRESETS.len()
    }

    fn name(&self, row: usize) -> String {
        PRESETS.get(row).copied().unwrap_or_default().to_owned()
    }

    fn revision(&self) -> u64 {
        0
    }

    fn hue(&self, row: usize) -> Option<u8> {
        Some(row as u8)
    }

    fn detail(&self, row: usize) -> Option<String> {
        Some(["Factory", "User"][row % 2].to_owned())
    }
}

/// The plugin generator's screen.
fn parabolic(params: &Arc<SynthParams>, page: &State<usize>, ask: &DialogState) -> View {
    let handle = |f: fn(&SynthParams) -> &FloatParam| ParamHandle::new(Arc::clone(params), f);
    let switch = |f: fn(&SynthParams) -> &BoolParam| ParamHandle::new(Arc::clone(params), f);
    let synth = row((
        section(
            "Oscillator",
            row((knob(handle(|p| &p.shape)), knob(handle(|p| &p.detune)))).spacing(14.0),
        ),
        section(
            "Filter",
            row((
                knob(handle(|p| &p.cutoff)),
                knob(handle(|p| &p.resonance)),
                knob(handle(|p| &p.drive)),
            ))
            .spacing(14.0),
        ),
        section(
            "Envelope",
            row((knob(handle(|p| &p.attack)), knob(handle(|p| &p.release)))).spacing(14.0),
        ),
    ))
    .spacing(16.0)
    .align(Align::Start)
    .padding(18.0);
    let effects = row((
        section(
            "Reverb",
            column((
                row((knob(handle(|p| &p.size)), knob(handle(|p| &p.mix)))).spacing(14.0),
                toggle(switch(|p| &p.chorus)),
            ))
            .spacing(10.0),
        ),
        section(
            "Delay",
            column((
                row((knob(handle(|p| &p.time)), knob(handle(|p| &p.feedback)))).spacing(14.0),
                toggle(switch(|p| &p.ping_pong)),
            ))
            .spacing(10.0),
        ),
    ))
    .spacing(16.0)
    .align(Align::Start)
    .padding(18.0);
    let list = ListState::new();
    list.set_selected([2]);
    let presets = row((
        card(
            column((
                label("Library").heading(),
                list_view("Presets", Presets, list).flex(1.0),
            ))
            .spacing(10.0),
        )
        .width(360.0),
        column((
            label("Glass bells").title(),
            label("Factory preset · bell-like FM over a soft pad").caption(),
            row((
                action_button("Load").primary(),
                action_button("Rename"),
                action_button("Delete").destructive().icon(bin()).opens(ask),
            ))
            .spacing(10.0),
        ))
        .spacing(10.0)
        .align(Align::Start)
        .flex(1.0),
    ))
    .spacing(18.0)
    .padding(18.0);
    let title_bar = row((
        label("Parabolic").title(),
        label("plugin generator").caption().flex(1.0),
        pill("Export plugin").primary(),
    ))
    .spacing(12.0)
    .align(Align::Center);
    column((
        title_bar,
        tabs("Sections", page.clone())
            .page("Synth", synth)
            .icon(wave())
            .page("Effects", effects)
            .icon(sliders())
            .page("Presets", presets)
            .icon(folder())
            .flex(1.0),
    ))
    .spacing(12.0)
    .padding(20.0)
    .with_dialog(
        confirm_dialog(
            ask.clone(),
            "Delete “Glass bells”?",
            "The preset is removed from the library and from every project that \
             uses it. This cannot be undone.",
        )
        .confirm_label("Delete preset")
        .destructive(),
    )
}

struct OrganParams {
    principal: BoolParam,
    flute: BoolParam,
    octave: BoolParam,
    mixture: BoolParam,
    trumpet: BoolParam,
    oboe: BoolParam,
    celeste: BoolParam,
    bourdon: BoolParam,
}

param_set!(OrganParams {
    principal,
    flute,
    octave,
    mixture,
    trumpet,
    oboe,
    celeste,
    bourdon
});

fn organ_params() -> Arc<OrganParams> {
    Arc::new(OrganParams {
        principal: BoolParam::new("principal", "Principal 8'", true),
        flute: BoolParam::new("flute", "Rohrflöte 8'", false),
        octave: BoolParam::new("octave", "Octave 4'", true),
        mixture: BoolParam::new("mixture", "Mixture IV", false),
        trumpet: BoolParam::new("trumpet", "Trumpet 8'", false),
        oboe: BoolParam::new("oboe", "Oboe 8'", true),
        celeste: BoolParam::new("celeste", "Voix céleste 8'", true),
        bourdon: BoolParam::new("bourdon", "Subbass 16'", true),
    })
}

/// The organ console's screen.
fn caecilia(params: &Arc<OrganParams>, division: &State<usize>) -> View {
    let stop = |f: fn(&OrganParams) -> &BoolParam| toggle(ParamHandle::new(Arc::clone(params), f));
    let stops = |title: &str, list: View| {
        column((label(title).title(), card(list)))
            .spacing(14.0)
            .padding(24.0)
            .align(Align::Start)
    };
    sidebar("Divisions", division.clone())
        .section("Manuals")
        .page(
            "Great",
            stops(
                "Great",
                column((
                    stop(|p| &p.principal),
                    stop(|p| &p.octave),
                    stop(|p| &p.mixture),
                    stop(|p| &p.trumpet),
                ))
                .spacing(10.0),
            ),
        )
        .icon(pipes())
        .page(
            "Swell",
            stops(
                "Swell",
                column((stop(|p| &p.oboe), stop(|p| &p.celeste), stop(|p| &p.flute))).spacing(10.0),
            ),
        )
        .icon(pipes())
        .page("Choir", stops("Choir", label("No stops drawn").into_view()))
        .icon(pipes())
        .section("Pedal")
        .page(
            "Pedal",
            stops("Pedal", column((stop(|p| &p.bourdon),)).into_view()),
        )
        .icon(pipes())
        .section("Console")
        .page(
            "Couplers",
            stops(
                "Couplers",
                row((
                    action_button("Swell to Great"),
                    action_button("Great to Pedal"),
                ))
                .spacing(10.0)
                .justify(Justify::Start),
            ),
        )
        .into_view()
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

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);
    std::fs::create_dir_all(&folder)?;
    let mut written = Vec::new();
    for (theme, look) in [(Theme::dark(), "dark"), (Theme::light(), "light")] {
        let params = synth_params();
        let page = State::new(0_usize);
        let ask = DialogState::new();
        let mut ui = Harness::with_params(
            parabolic(&params, &page, &ask),
            SIZE,
            SCALE,
            Arc::clone(&params) as Arc<dyn ParamSet>,
        );
        ui.set_theme(theme);
        ui.set_motion(true);
        settle(&mut ui);
        written.push(save(&ui, &folder, &format!("nav-synth-{look}.png"))?);

        // A click on the Effects tab; the pointer rests on Presets.
        ui.click(ui.get(Role::Tab, "Effects").center());
        ui.move_to(ui.get(Role::Tab, "Presets").center());
        settle(&mut ui);
        println!("{look}: the application polls page {}", page.get());
        written.push(save(&ui, &folder, &format!("nav-effects-{look}.png"))?);

        // Control+Tab to Presets, then Delete opens the dialog.
        ui.key_with(
            Key::Tab,
            Modifiers {
                control: true,
                ..Modifiers::NONE
            },
        );
        settle(&mut ui);
        ui.click(ui.get(Role::Button, "Delete").center());
        settle(&mut ui);
        println!(
            "{look}: page {}, dialog open: {}",
            page.get(),
            ask.is_open()
        );
        written.push(save(&ui, &folder, &format!("nav-dialog-{look}.png"))?);
        ui.key(Key::Escape);
        settle(&mut ui);
        println!(
            "{look}: the dialog answered {:?}; the focus is back on {:?}",
            ask.take_result().unwrap_or(DialogResult::Confirmed),
            ui.focused().map(|found| found.node.name)
        );

        let organ = organ_params();
        let division = State::new(1_usize);
        let mut ui = Harness::with_params(
            caecilia(&organ, &division),
            SIZE,
            SCALE,
            Arc::clone(&organ) as Arc<dyn ParamSet>,
        );
        ui.set_theme(theme);
        ui.frame();
        // The sidebar focused from the keyboard, one division down.
        ui.tab();
        ui.key(Key::Up);
        ui.key(Key::Down);
        settle(&mut ui);
        println!("{look}: the organ shows division {}", division.get());
        written.push(save(&ui, &folder, &format!("nav-organ-{look}.png"))?);
    }
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}
