//! A synth's page built from the oscillator views and the wheels, driven
//! headless and drawn into PNG files.
//!
//! ```text
//! cargo run -p manguebyte-ui --example synth_screen
//! cargo run -p manguebyte-ui --example synth_screen -- <folder>
//! ```
//!
//! The page has a card with a wavetable of 16 frames, built with
//! `manguebyte_dsp::Wavetable` from a sine that turns into a saw and then
//! a square, stacked in depth with the frame playing lit, the cycle
//! playing beside it and a knob that moves the position; a card with a
//! second oscillator's cycle, a saw handed over as a slice of samples;
//! and a card with the pitch bend and modulation wheels left of a
//! keyboard. The pitch bend wheel is held half way up, the modulation
//! wheel set by the application as MIDI would, and a chord is lit.
//!
//! It draws the page in both themes at 1x, 1.25x and 2x
//! (`synth-dark@2x.png` and so on) into the folder given, or the system's
//! temporary folder, and prints what the application polled from the
//! wheels.

use std::f32::consts::TAU;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use manguebyte_dsp::Wavetable;
use manguebyte_gfx::{Point, Size};
use manguebyte_params::{FloatParam, FloatRange, param_set};
use manguebyte_plugin::editor::EditorContext;
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_ui::access::Role;
use manguebyte_ui::layout::Align;
use manguebyte_ui::{
    Button, Event, IntoView, KeySpan, KeyboardState, Modifiers, ParamHandle, Root, Theme, View,
    WaveData, WheelState, card, column, knob, label, mod_wheel, piano_keyboard, pitch_wheel, row,
    wave_view, wavetable_view,
};

/// The page's size in logical pixels.
const SIZE: Size = Size::new(900.0, 640.0);

/// How many frames the wavetable has.
const FRAMES: usize = 16;

struct Params {
    position: FloatParam,
    level: FloatParam,
}

param_set!(Params { position, level });

fn params() -> Arc<Params> {
    Arc::new(Params {
        position: FloatParam::new(
            "position",
            "Position",
            FloatRange::linear(0.0, (FRAMES - 1) as f32),
            9.4,
        )
        .with_precision(1),
        level: FloatParam::new("level", "Level", FloatRange::linear(-24.0, 6.0), -6.0)
            .with_unit("dB")
            .with_precision(1),
    })
}

/// A sine that turns into a saw over the first half of the frames and the
/// saw into a square over the second, each frame 1024 samples.
fn morph() -> Vec<Vec<f32>> {
    (0..FRAMES)
        .map(|frame| {
            let t = frame as f32 / (FRAMES - 1) as f32;
            (0..1024)
                .map(|i| {
                    let phase = i as f32 / 1024.0;
                    let mut saw = 0.0;
                    let mut square = 0.0;
                    for harmonic in 1..=40 {
                        let h = harmonic as f32;
                        let partial = (TAU * h * phase).sin() / h;
                        saw += partial;
                        if harmonic % 2 == 1 {
                            square += partial;
                        }
                    }
                    let sine = (TAU * phase).sin();
                    let (saw, square) = (saw * 0.55, square * 0.95);
                    if t < 0.5 {
                        sine + (saw - sine) * (t * 2.0)
                    } else {
                        saw + (square - saw) * ((t - 0.5) * 2.0)
                    }
                })
                .collect()
        })
        .collect()
}

fn header(title: &str, meta: &str) -> View {
    row((label(title).heading().flex(1.0), label(meta).caption()))
        .align(Align::Center)
        .into_view()
}

/// The page.
fn page(params: &Arc<Params>, table: &WaveData, wheels: &(WheelState, WheelState)) -> View {
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(params), field);
    let position = handle(|p| &p.position);
    let at = position.clone();
    let read = move || at.plain(at.normalized()) as f32;
    let at = position.clone();
    let read_too = move || at.plain(at.normalized()) as f32;
    let saw: Vec<f32> = (0..600).map(|i| 1.0 - i as f32 / 300.0).collect();
    let oscillator_a = card(
        column((
            header("Oscillator A", "wavetable \u{b7} 16 frames"),
            wavetable_view("Wavetable A", table.clone(), read).flex(1.0),
            row((
                wave_view("Cycle A", table.clone())
                    .position(read_too)
                    .width(150.0)
                    .height(72.0),
                knob(position),
                knob(handle(|p| &p.level)),
            ))
            .spacing(12.0)
            .align(Align::Center),
        ))
        .spacing(10.0),
    )
    .flex(1.0);
    let oscillator_b = card(
        column((
            header("Oscillator B", "saw \u{b7} 600 samples"),
            wave_view("Cycle B", WaveData::from_cycle(&saw)).flex(1.0),
        ))
        .spacing(10.0),
    )
    .width(260.0);
    let keys = card(
        column((
            header("Performance", "bend \u{b1}2 st \u{b7} mod CC 1"),
            row((
                pitch_wheel("Pitch", wheels.0.clone()),
                mod_wheel("Mod", wheels.1.clone()),
                piano_keyboard("Keyboard", KeySpan::new(36, 84), keyboard()).flex(1.0),
            ))
            .spacing(6.0)
            .align(Align::Stretch),
        ))
        .spacing(10.0),
    )
    .height(200.0);
    column((
        row((column((
            label("Mangue Synth").title(),
            label("Init \u{b7} 48 kHz").caption(),
        ))
        .flex(1.0),)),
        row((oscillator_a, oscillator_b)).spacing(14.0).flex(1.0),
        keys,
    ))
    .spacing(14.0)
    .padding(22.0)
    .into_view()
}

fn keyboard() -> KeyboardState {
    let state = KeyboardState::new();
    state.set_sounding([60, 64, 67]);
    state
}

fn main() -> std::io::Result<()> {
    let folder = std::env::args_os()
        .nth(1)
        .map_or_else(std::env::temp_dir, PathBuf::from);
    let table =
        Wavetable::new(&morph()).map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    let mut written = Vec::new();
    for (theme, theme_name) in [(Theme::dark(), "dark"), (Theme::light(), "light")] {
        for (scale, suffix) in [(1.0, "@1x"), (1.25, "@1.25x"), (2.0, "@2x")] {
            let params = params();
            let data = WaveData::from_wavetable(&table);
            let wheels = (WheelState::new(), WheelState::new());
            let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
            let (context, _link) = EditorContext::attach(host);
            let mut root =
                Root::new(page(&params, &data, &wheels), SIZE, scale, context).with_theme(theme);
            let mut clock = Duration::ZERO;
            let mut frame = |root: &mut Root| {
                clock += Duration::from_millis(16);
                root.frame(clock);
            };
            frame(&mut root);
            // The application sets the modulation from MIDI it received;
            // a person holds the pitch bend wheel half way up.
            wheels.1.set(0.62);
            let pitch = root
                .access_tree()
                .into_iter()
                .find(|entry| entry.node.role == Role::Slider && entry.node.name == "Pitch")
                .map(|entry| entry.bounds)
                .expect("the pitch bend wheel");
            let at = pitch.center();
            root.event(Event::PointerDown {
                at,
                button: Button::Primary,
                clicks: 1,
                modifiers: Modifiers::NONE,
            });
            root.event(Event::PointerMove {
                at: Point::new(at.x, at.y - 30.0),
                modifiers: Modifiers::NONE,
            });
            for _ in 0..30 {
                frame(&mut root);
            }
            if scale == 1.0 && theme_name == "dark" {
                println!(
                    "polled: pitch bend {:?} (MIDI {}), modulation {:.2} (CC {}), held {}",
                    wheels.0.take(),
                    wheels.0.midi_pitch_bend(),
                    wheels.1.value(),
                    wheels.1.midi_controller(),
                    wheels.0.is_held(),
                );
            }
            written.push(save(
                &root,
                &folder,
                &format!("synth-{theme_name}{suffix}.png"),
            )?);
        }
    }
    for path in written {
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn save(root: &Root, folder: &Path, name: &str) -> std::io::Result<PathBuf> {
    let path = folder.join(name);
    let png = root
        .pixels()
        .encode_png()
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    std::fs::write(&path, png)?;
    Ok(path)
}
