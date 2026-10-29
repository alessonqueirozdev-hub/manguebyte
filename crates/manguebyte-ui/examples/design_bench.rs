//! What the design costs: a screen of cards, shadows and glows (a sampler's
//! list, waveform, zone map, keyboard, envelope and knobs) drawn whole at
//! 1x and 2x, an idle frame, a frame of a hover transition, and a card's
//! shadow blurred cold and drawn from the cache.
//!
//! Run it in release mode:
//!
//! ```text
//! cargo run -p manguebyte-ui --example design_bench --release
//! cargo run -p manguebyte-ui --example design_bench --release -- --quick
//! cargo run -p manguebyte-ui --example design_bench --release -- --quick --widgets
//! ```
//!
//! With `--widgets` it also prints what drawing each widget's own scene
//! whole costs, the most costly first.
//!
//! Each case runs many times and prints the median and fastest time in
//! milliseconds, and the device pixels drawn. A frame is the whole of
//! `Root::frame`: polling, layout when needed, recording what changed and
//! drawing the damage. Presenting on screen is not included. Times depend
//! on the machine and on what else it is doing.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use manguebyte_gfx::{Color, IRect, Pixmap, Point, Rect, Renderer, RoundedRect, Scene, Size};
use manguebyte_params::{FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_plugin::editor::{EditorContext, HostLink};
use manguebyte_ui::layout::{Align, Justify};
use manguebyte_ui::{
    Event, IntoView, KeySpan, KeyboardState, ListData, ListState, Modifiers, ParamHandle, Root,
    Theme, View, Waveform, WaveformState, Zone, ZoneMapState, card, column, envelope, knob, label,
    list_view, piano_keyboard, pill, row, waveform_view, zone_map,
};

const SIZE: Size = Size::new(1100.0, 640.0);

struct Params {
    attack: FloatParam,
    decay: FloatParam,
    sustain: FloatParam,
    release: FloatParam,
}

param_set!(Params {
    attack,
    decay,
    sustain,
    release
});

fn params() -> Arc<Params> {
    let time = |id, name, ms| {
        FloatParam::new(id, name, FloatRange::logarithmic(1.0, 5000.0), ms)
            .with_unit("ms")
            .with_precision(0)
    };
    Arc::new(Params {
        attack: time("attack", "Attack", 4.0),
        decay: time("decay", "Decay", 54.0),
        sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 62.0)
            .with_unit("%")
            .with_precision(0),
        release: time("release", "Release", 1200.0),
    })
}

struct Samples;

impl ListData for Samples {
    fn len(&self) -> usize {
        4
    }

    fn name(&self, row: usize) -> String {
        ["Kick.wav", "Snare.wav", "Pluck.wav", "Pad.wav"][row % 4].to_owned()
    }

    fn revision(&self) -> u64 {
        0
    }

    fn hue(&self, row: usize) -> Option<u8> {
        u8::try_from(row).ok()
    }

    fn detail(&self, row: usize) -> Option<String> {
        Some(["C1", "D1", "C3\u{2013}G4", "C4\u{2013}C6"][row % 4].to_owned())
    }
}

/// The screen, and the states behind it.
struct Screen {
    root: Root,
    clock: Duration,
    _link: HostLink,
}

fn header(title: &str, meta: &str) -> View {
    row((label(title).heading().flex(1.0), label(meta).caption())).align(Align::Center)
}

fn open(scale: f32) -> Screen {
    let params = params();
    let host =
        Rc::new(RecordingEditorHost::new().with_params(Arc::clone(&params) as Arc<dyn ParamSet>));
    let (context, link) = EditorContext::attach(host);
    let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(&params), field);
    let wave = WaveformState::new();
    let rate = 48_000.0_f32;
    let data: Vec<f32> = (0..(2.48 * rate) as usize)
        .map(|i| {
            let t = i as f32 / rate;
            (std::f32::consts::TAU * 220.0 * t).sin() * (-t * 1.3).exp() * 0.6
        })
        .collect();
    wave.set_waveform(Some(Arc::new(
        Waveform::new(vec![data], f64::from(rate)).named("Pluck.wav"),
    )));
    let zones = ZoneMapState::new();
    zones.set_zones(vec![
        Zone::new(0, "Kick", 24, 29).with_hue(0),
        Zone::new(1, "Snare", 30, 35).with_hue(1),
        Zone::new(2, "Pluck", 48, 67)
            .with_velocities(64, 127)
            .with_hue(2),
        Zone::new(3, "Pluck soft", 48, 67)
            .with_velocities(0, 63)
            .with_hue(2),
        Zone::new(4, "Pad", 69, 84).with_hue(3),
    ]);
    zones.set_selected([2]);
    let keys = KeyboardState::new();
    keys.set_sounding([60, 64, 67]);
    let list = ListState::new();
    list.set_selected([2]);
    let span = KeySpan::new(24, 84);
    let view = column((
        row((
            label("Mangue Sampler").title().flex(1.0),
            pill("Undo"),
            pill("Export plugin").primary(),
        ))
        .spacing(12.0)
        .align(Align::Center),
        row((
            card(
                column((
                    header("Samples", "4"),
                    list_view("Samples", Samples, list).flex(1.0),
                ))
                .spacing(8.0),
            )
            .width(220.0),
            column((
                card(
                    column((
                        header("Pluck.wav", "2.48 s"),
                        waveform_view("Sample", wave).height(150.0),
                    ))
                    .spacing(10.0),
                ),
                row((
                    card(
                        column((
                            header("Zone map", "note \u{d7} velocity"),
                            zone_map("Zones", span, zones).flex(1.0),
                            piano_keyboard("Keyboard", span, keys).height(104.0),
                        ))
                        .spacing(10.0),
                    )
                    .flex(1.0),
                    card(
                        column((
                            header("Envelope", "ADSR"),
                            envelope(
                                "Amp envelope",
                                handle(|p| &p.attack),
                                handle(|p| &p.decay),
                                handle(|p| &p.sustain),
                                handle(|p| &p.release),
                            )
                            .height(150.0),
                            row((
                                knob(handle(|p| &p.attack)),
                                knob(handle(|p| &p.decay)),
                                knob(handle(|p| &p.sustain)),
                                knob(handle(|p| &p.release)),
                            ))
                            .justify(Justify::SpaceBetween),
                        ))
                        .spacing(10.0),
                    )
                    .width(320.0),
                ))
                .spacing(14.0)
                .flex(1.0),
            ))
            .spacing(14.0)
            .flex(1.0),
        ))
        .spacing(14.0)
        .flex(1.0),
    ))
    .spacing(18.0)
    .padding(22.0);
    Screen {
        root: Root::new(view, SIZE, scale, context),
        clock: Duration::ZERO,
        _link: link,
    }
}

impl Screen {
    /// A frame 16 ms after the last; the device pixels it drew.
    fn frame(&mut self) -> u64 {
        self.clock += Duration::from_millis(16);
        let frame = self.root.frame(self.clock);
        frame.damage.iter().map(IRect::area).sum()
    }

    fn settle(&mut self) {
        for _ in 0..100 {
            self.frame();
            if !self.root.is_animating() {
                return;
            }
        }
    }

    fn center_of(&self, name: &str) -> Point {
        self.root
            .access_tree()
            .into_iter()
            .find(|entry| entry.node.name == name && entry.item.is_none())
            .map(|entry| entry.bounds.center())
            .unwrap_or_else(|| panic!("no {name}"))
    }

    fn move_to(&mut self, at: Point) {
        self.root.event(Event::PointerMove {
            at,
            modifiers: Modifiers::NONE,
        });
    }
}

struct Row {
    name: String,
    median: f64,
    best: f64,
    pixels: u64,
}

fn measure(name: impl Into<String>, runs: usize, mut run: impl FnMut() -> u64) -> Row {
    let mut times = Vec::with_capacity(runs);
    let mut pixels = 0;
    for _ in 0..runs {
        let start = Instant::now();
        pixels = run();
        times.push(start.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(f64::total_cmp);
    Row {
        name: name.into(),
        median: times[times.len() / 2],
        best: times[0],
        pixels,
    }
}

fn main() {
    let quick = std::env::args().any(|arg| arg == "--quick");
    let runs = if quick { 10 } else { 60 };
    let mut rows = Vec::new();

    for scale in [1.0f32, 2.0] {
        rows.push(measure(
            format!("{scale}x: open, the first frame (blurs every shadow)"),
            runs / 3 + 1,
            || {
                let mut screen = open(scale);
                screen.frame()
            },
        ));
        let mut screen = open(scale);
        screen.settle();
        rows.push(measure(
            format!("{scale}x: whole repaint (theme set again, shadows cached)"),
            runs,
            || {
                screen.root.set_theme(Theme::dark());
                screen.frame()
            },
        ));
        let scene = screen.root.scene().clone();
        let mut whole = screen.root.pixels().clone();
        let mut renderer = Renderer::new();
        renderer.render(&scene, &mut whole, scale);
        rows.push(measure(
            format!("{scale}x: the screen's scene rendered whole, caches warm"),
            runs,
            || {
                renderer.render(&scene, &mut whole, scale);
                0
            },
        ));
        if std::env::args().any(|arg| arg == "--widgets") {
            let mut costs = Vec::new();
            for entry in screen.root.access_tree() {
                if entry.item.is_some() {
                    continue;
                }
                let Some(scene) = screen.root.widget_scene(entry.id) else {
                    continue;
                };
                let mut pixels = screen.root.pixels().clone();
                let mut renderer = Renderer::new();
                renderer.render(scene, &mut pixels, scale);
                let start = Instant::now();
                for _ in 0..5 {
                    renderer.render(scene, &mut pixels, scale);
                }
                costs.push((
                    start.elapsed().as_secs_f64() * 200.0,
                    format!("{:?} {}", entry.node.role, entry.node.name),
                ));
            }
            costs.sort_by(|a, b| b.0.total_cmp(&a.0));
            for (ms, name) in costs.iter().take(12) {
                println!("{scale}x {ms:.3} ms {name}");
            }
        }
        rows.push(measure(format!("{scale}x: idle frame"), runs * 10, || {
            screen.frame()
        }));
        // A hover transition on a knob: every frame while it eases in,
        // then out again.
        let decay = screen.center_of("Decay");
        let away = Point::new(5.0, SIZE.height - 5.0);
        let mut frames = Vec::new();
        for _ in 0..runs / 6 + 1 {
            for at in [decay, away] {
                screen.move_to(at);
                loop {
                    let start = Instant::now();
                    let pixels = screen.frame();
                    frames.push((start.elapsed().as_secs_f64() * 1e3, pixels));
                    if !screen.root.is_animating() {
                        break;
                    }
                }
            }
        }
        frames.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.push(Row {
            name: format!("{scale}x: a frame of a knob's hover transition"),
            median: frames[frames.len() / 2].0,
            best: frames[0].0,
            pixels: frames[frames.len() / 2].1,
        });
    }

    // A card's shadow at 2x: blurred every time, and from the cache.
    let card = RoundedRect::new(Rect::new(20.0, 20.0, 300.0, 200.0), 18.0);
    let mut scene = Scene::new();
    scene.shadow(card, Theme::dark().elevation.md);
    scene.fill(card, Color::hex(0x1C1F28));
    let mut pixels = Pixmap::new(680, 560).expect("small");
    let mut cold = Renderer::new();
    cold.set_shadow_cache_limit(0);
    rows.push(measure(
        "2x: a 300 x 200 card, shadow blurred",
        runs,
        || {
            cold.render(&scene, &mut pixels, 2.0);
            0
        },
    ));
    let mut warm = Renderer::new();
    warm.render(&scene, &mut pixels, 2.0);
    rows.push(measure(
        "2x: the same card, shadow from the cache",
        runs,
        || {
            warm.render(&scene, &mut pixels, 2.0);
            0
        },
    ));
    let mut bare = Scene::new();
    bare.fill(card, Color::hex(0x1C1F28));
    let mut plain = Renderer::new();
    rows.push(measure("2x: the same card with no shadow", runs, || {
        plain.render(&bare, &mut pixels, 2.0);
        0
    }));

    let stops = [
        manguebyte_gfx::Stop::new(0.0, Color::hex(0x1C1F28)),
        manguebyte_gfx::Stop::new(1.0, Color::hex(0x16181F)),
    ];
    let mut graded = Scene::new();
    graded.fill(
        card,
        manguebyte_gfx::LinearGradient {
            start: Point::new(0.0, 20.0),
            end: Point::new(0.0, 220.0),
            stops: &stops,
            extend: manguebyte_gfx::Extend::Pad,
        },
    );
    rows.push(measure(
        "2x: the same card in a gradient, no shadow",
        runs,
        || {
            plain.render(&graded, &mut pixels, 2.0);
            0
        },
    ));
    println!(
        "A sampler screen of cards, {} x {} logical; {runs} runs a case\n",
        SIZE.width, SIZE.height
    );
    println!("| case | median ms | best ms | pixels drawn |");
    println!("|---|---|---|---|");
    for row in rows {
        println!(
            "| {} | {:.3} | {:.3} | {} |",
            row.name, row.median, row.best, row.pixels
        );
    }
}
