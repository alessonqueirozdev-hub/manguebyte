//! Renders representative interface scenes and prints how long they take.
//!
//! Run it in release mode:
//!
//! ```text
//! cargo run -p manguebyte-gfx --example bench --release
//! cargo run -p manguebyte-gfx --example bench --release -- --quick
//! ```
//!
//! Each case is drawn whole at 1x and 2x, and some also through a damage
//! rectangle, as an interface redraws after a change. The table gives the
//! median and fastest time of a frame and the device pixels covered per
//! second. Times depend on the machine and on what else it is doing; the
//! numbers are for comparing cases and renderers on one machine.
//!
//! Text has cases of its own: laying out a 40-character label, and drawing
//! it with the glyph cache emptied before every frame (cold) and kept
//! (warm), the second being what an interface pays on every frame after
//! the first.

use std::f32::consts::PI;
use std::time::{Duration, Instant};

use manguebyte_gfx::{
    Arc, Circle, Color, Extend, LinearGradient, Path, Pixmap, Point, RadialGradient, Rect,
    Renderer, RoundedRect, Scene, Stop, Stroke, TextLayout, TextStyle,
};

/// A 40-character label, as a plugin shows under a control.
const LABEL: &str = "Low cut 120.0 Hz, slope 24 dB/oct, Q 0.7";

/// A paragraph of help text, about 600 characters.
const PARAGRAPH: &str = "The low cut removes energy below its frequency with a slope of 12, \
    24 or 48 dB per octave. Raise Q for a resonant peak just above the cut, which \
    adds weight before the roll-off; lower it for a gentle shoulder. The filter is \
    linear in phase only in the offline mode, where it adds latency of half its \
    length, reported to the host. In the real-time mode it is minimum phase and \
    adds none. Automation of the frequency is smoothed over 20 ms, so sweeps do \
    not click, and the slope can be changed while audio runs: the two settings are \
    crossfaded over one block. Values in dB are shown to one decimal place.";

/// A panel of knobs, as a plugin editor has: 8 by 3 knobs on 800 by 300.
fn knob_panel(values: &[f32]) -> (Scene, Rect) {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 800.0, 300.0), Color::hex(0x16181C));
    scene.fill(
        RoundedRect::new(Rect::new(10.0, 10.0, 780.0, 280.0), 12.0),
        Color::hex(0x23262D),
    );
    let body = [
        Stop::new(0.0, Color::hex(0x5A6070)),
        Stop::new(1.0, Color::hex(0x23262D)),
    ];
    let mut first_knob = Rect::default();
    for row in 0..3 {
        for column in 0..8 {
            let index = row * 8 + column;
            let center = Point::new(60.0 + column as f32 * 97.0, 55.0 + row as f32 * 95.0);
            let value = values[index % values.len()];
            let start = 0.75 * PI;
            let sweep = 1.5 * PI;
            scene.fill(
                Circle::new(center, 26.0),
                RadialGradient {
                    center: Point::new(center.x - 8.0, center.y - 10.0),
                    radius: 34.0,
                    stops: &body,
                    extend: Extend::Pad,
                },
            );
            scene.stroke(
                Arc::new(center, 33.0, start, sweep),
                Stroke::round(4.0),
                Color::hex(0x3A3F4A),
            );
            scene.stroke(
                Arc::new(center, 33.0, start, sweep * value),
                Stroke::round(4.0),
                Color::hex(0xFF7A00),
            );
            let angle = start + sweep * value;
            let mut pointer = Path::new();
            pointer
                .move_to(Arc::new(center, 10.0, 0.0, 0.0).point_at(angle))
                .line_to(Arc::new(center, 22.0, 0.0, 0.0).point_at(angle));
            scene.stroke(&pointer, Stroke::round(3.0), Color::WHITE);
            if index == 0 {
                first_knob = Rect::new(center.x - 36.0, center.y - 36.0, 72.0, 72.0);
            }
        }
    }
    (scene, first_knob)
}

/// A spectrum filling a 1200 by 600 window: 2048 bins as columns, over a
/// grid, coloured by a vertical gradient or by one colour.
fn spectrum(gradient: bool) -> Scene {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 1200.0, 600.0), Color::hex(0x101216));
    for i in 1..12 {
        let x = i as f32 * 100.0;
        scene.fill(Rect::new(x, 0.0, 1.0, 600.0), Color::hex(0x23262D));
    }
    for i in 1..6 {
        let y = i as f32 * 100.0;
        scene.fill(Rect::new(0.0, y, 1200.0, 1.0), Color::hex(0x23262D));
    }
    let bins = 2048;
    let high: Vec<f32> = (0..bins)
        .map(|i| {
            let x = i as f32 / bins as f32;
            let peaks = 0.25 * (x * 160.0).sin().abs() + 0.15 * (x * 31.0).cos().abs();
            ((1.0 - x).powf(1.5) * 0.7 + peaks).min(1.0)
        })
        .collect();
    let low = vec![0.0; bins];
    let stops = [
        Stop::new(0.0, Color::hex(0x3D8BFF)),
        Stop::new(0.7, Color::hex(0x41D27A)),
        Stop::new(1.0, Color::hex(0xFFD23F)),
    ];
    let area = Rect::new(0.0, 0.0, 1200.0, 600.0);
    if gradient {
        let paint = LinearGradient {
            start: Point::new(0.0, 600.0),
            end: Point::new(0.0, 0.0),
            stops: &stops,
            extend: Extend::Pad,
        };
        scene.columns(area, &low, &high, paint);
    } else {
        scene.columns(area, &low, &high, Color::hex(0x41D27A));
    }
    scene
}

/// A window-sized rectangle of one colour, opaque or not: the cheapest
/// thing to draw, as a baseline for the machine.
fn solid_fill(alpha: u8) -> Scene {
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 1200.0, 600.0),
        Color::rgba(0x3D, 0x8B, 0xFF, alpha),
    );
    scene
}

/// A window-sized rectangle filled with a gradient, to separate the cost
/// of the gradient from the cost of the columns.
fn gradient_fill() -> Scene {
    let stops = [
        Stop::new(0.0, Color::hex(0x3D8BFF)),
        Stop::new(0.7, Color::hex(0x41D27A)),
        Stop::new(1.0, Color::hex(0xFFD23F)),
    ];
    let mut scene = Scene::new();
    scene.fill(
        Rect::new(0.0, 0.0, 1200.0, 600.0),
        LinearGradient {
            start: Point::new(0.0, 600.0),
            end: Point::new(0.0, 0.0),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
    scene
}

/// 50,000 samples of a waveform across 1200 by 300.
fn samples() -> Vec<f32> {
    (0..50_000)
        .map(|i| {
            let t = i as f32 / 50_000.0;
            let envelope = (t * PI).sin();
            envelope * (0.6 * (t * 1_900.0).sin() + 0.3 * (t * 7_300.0).sin())
        })
        .collect()
}

/// The waveform drawn as one path through all 50,000 points.
fn waveform_path(samples: &[f32]) -> Scene {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 1200.0, 300.0), Color::hex(0x101216));
    let mut path = Path::with_capacity(samples.len());
    for (i, s) in samples.iter().enumerate() {
        let p = Point::new(i as f32 * 1200.0 / samples.len() as f32, 150.0 - s * 140.0);
        if i == 0 {
            path.move_to(p);
        } else {
            path.line_to(p);
        }
    }
    scene.stroke(&path, Stroke::new(1.0), Color::hex(0xFF7A00));
    scene
}

/// The same waveform as columns: each entry is one sample, and each device
/// column draws the envelope of the samples it covers.
fn waveform_columns(samples: &[f32]) -> Scene {
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 1200.0, 300.0), Color::hex(0x101216));
    let levels: Vec<f32> = samples
        .iter()
        .map(|s| 0.5 + s * 0.5 * 140.0 / 150.0)
        .collect();
    // A single sample per entry has no height; neighbours give it one.
    let low: Vec<f32> = levels.windows(2).map(|w| w[0].min(w[1])).collect();
    let high: Vec<f32> = levels.windows(2).map(|w| w[0].max(w[1])).collect();
    scene.columns(
        Rect::new(0.0, 10.0, 1200.0, 280.0),
        &low,
        &high,
        Color::hex(0xFF7A00),
    );
    scene
}

struct Timing {
    median: Duration,
    fastest: Duration,
    frames: usize,
}

/// Draws a frame until `budget` has passed (at least 3 frames, at most
/// 200), after one frame to warm the renderer's buffers.
fn measure(budget: Duration, mut frame: impl FnMut()) -> Timing {
    frame();
    let mut times = Vec::new();
    let started = Instant::now();
    while times.len() < 3 || (started.elapsed() < budget && times.len() < 200) {
        let t = Instant::now();
        frame();
        times.push(t.elapsed());
    }
    times.sort_unstable();
    Timing {
        median: times[times.len() / 2],
        fastest: times[0],
        frames: times.len(),
    }
}

fn report(name: &str, scale: f32, pixels: u64, timing: &Timing) {
    let ms = |d: Duration| d.as_secs_f64() * 1e3;
    let mpx = pixels as f64 / timing.median.as_secs_f64() / 1e6;
    println!(
        "{name:<34} {scale:>4}x {:>10} {:>9.3} {:>9.3} {:>9.1} {:>6}",
        pixels,
        ms(timing.median),
        ms(timing.fastest),
        mpx,
        timing.frames
    );
}

fn report_time(name: &str, timing: &Timing) {
    println!(
        "{name:<34} {:>5} {:>10} {:>9.3} {:>9.3} {:>9} {:>6}",
        "-",
        "-",
        timing.median.as_secs_f64() * 1e3,
        timing.fastest.as_secs_f64() * 1e3,
        "-",
        timing.frames
    );
}

fn main() {
    let quick = std::env::args().any(|a| a == "--quick");
    let budget = Duration::from_millis(if quick { 150 } else { 1_500 });
    if cfg!(debug_assertions) {
        println!("note: this is a debug build; run with --release for meaningful numbers");
    }
    println!(
        "{:<34} {:>5} {:>10} {:>9} {:>9} {:>9} {:>6}",
        "case", "scale", "pixels", "median ms", "best ms", "Mpx/s", "frames"
    );
    let mut renderer = Renderer::new();

    let (panel, first_knob) = knob_panel(&[0.1, 0.35, 0.5, 0.62, 0.8, 0.95]);
    let samples = samples();
    let gradient_spectrum = spectrum(true);
    let solid_spectrum = spectrum(false);
    let gradient = gradient_fill();
    let opaque = solid_fill(255);
    let translucent = solid_fill(128);
    let path = waveform_path(&samples);
    let columns = waveform_columns(&samples);

    for scale in [1.0f32, 2.0] {
        // The panel, whole.
        let mut target = Pixmap::new((800.0 * scale) as u32, (300.0 * scale) as u32).expect("fits");
        let pixels = target.bounds().area();
        let t = measure(budget, || renderer.render(&panel, &mut target, scale));
        report("knob panel, full redraw", scale, pixels, &t);

        // One knob turned: re-record the panel and redraw only that knob.
        let damage = [first_knob.to_device(scale)];
        let mut value = 0.0f32;
        let t = measure(budget, || {
            value = (value + 0.01) % 1.0;
            let (scene, _) = knob_panel(&[value, 0.35, 0.5, 0.62, 0.8, 0.95]);
            renderer.render_damage(&scene, &mut target, scale, &damage);
        });
        report(
            "knob panel, one knob (record+draw)",
            scale,
            damage[0].area(),
            &t,
        );

        // The spectrum.
        let mut target =
            Pixmap::new((1200.0 * scale) as u32, (600.0 * scale) as u32).expect("fits");
        let pixels = target.bounds().area();
        let t = measure(budget, || {
            renderer.render(&gradient_spectrum, &mut target, scale)
        });
        report("spectrum, 2048 bins as columns", scale, pixels, &t);
        let t = measure(budget, || {
            renderer.render(&solid_spectrum, &mut target, scale)
        });
        report("spectrum, one colour", scale, pixels, &t);
        let t = measure(budget, || renderer.render(&gradient, &mut target, scale));
        report("window-sized gradient rectangle", scale, pixels, &t);
        let t = measure(budget, || renderer.render(&opaque, &mut target, scale));
        report("window-sized opaque rectangle", scale, pixels, &t);
        let t = measure(budget, || renderer.render(&translucent, &mut target, scale));
        report("window-sized translucent rectangle", scale, pixels, &t);

        // The waveform, as a path and as columns.
        let mut target =
            Pixmap::new((1200.0 * scale) as u32, (300.0 * scale) as u32).expect("fits");
        let pixels = target.bounds().area();
        let t = measure(budget, || renderer.render(&path, &mut target, scale));
        report("waveform, 50,000-point path", scale, pixels, &t);
        let t = measure(budget, || renderer.render(&columns, &mut target, scale));
        report("waveform, 50,000 entries as columns", scale, pixels, &t);
    }

    // Text: a label laid out, and drawn from a cold and a warm cache.
    assert_eq!(LABEL.chars().count(), 40);
    let style = TextStyle::new(12.0).with_color(Color::hex(0xE8E8E8));
    let t = measure(budget, || {
        std::hint::black_box(TextLayout::new(LABEL, &style, None));
    });
    report_time("label, 40 characters, layout", &t);
    let paragraph_style = TextStyle::new(13.0).with_color(Color::hex(0xE8E8E8));
    let t = measure(budget, || {
        std::hint::black_box(TextLayout::new(PARAGRAPH, &paragraph_style, Some(400.0)));
    });
    report_time("paragraph, 600 characters, layout", &t);
    let label = TextLayout::new(LABEL, &style, None);
    let paragraph = TextLayout::new(PARAGRAPH, &paragraph_style, Some(400.0));
    for scale in [1.0f32, 2.0] {
        let size = label.size();
        let mut scene = Scene::new();
        scene.text(&label, Point::new(4.0, 4.0));
        let mut target = Pixmap::new(
            ((size.width + 8.0) * scale) as u32,
            ((size.height + 8.0) * scale) as u32,
        )
        .expect("fits");
        let pixels = target.bounds().area();
        let mut cold = Renderer::new();
        let t = measure(budget, || {
            cold.clear_glyph_cache();
            cold.render(&scene, &mut target, scale);
        });
        report("label, draw, cold cache", scale, pixels, &t);
        let mut warm = Renderer::new();
        let t = measure(budget, || warm.render(&scene, &mut target, scale));
        report("label, draw, warm cache", scale, pixels, &t);
        let t = measure(budget, || {
            cold.clear_glyph_cache();
            let label = TextLayout::new(LABEL, &style, None);
            scene.clear();
            scene.text(&label, Point::new(4.0, 4.0));
            cold.render(&scene, &mut target, scale);
        });
        report("label, layout and draw, cold", scale, pixels, &t);
        let t = measure(budget, || {
            let label = TextLayout::new(LABEL, &style, None);
            scene.clear();
            scene.text(&label, Point::new(4.0, 4.0));
            warm.render(&scene, &mut target, scale);
        });
        report("label, layout and draw, warm", scale, pixels, &t);

        let size = paragraph.size();
        let mut scene = Scene::new();
        scene.text(&paragraph, Point::new(4.0, 4.0));
        let mut target = Pixmap::new(
            ((size.width + 8.0) * scale) as u32,
            ((size.height + 8.0) * scale) as u32,
        )
        .expect("fits");
        let pixels = target.bounds().area();
        let t = measure(budget, || {
            cold.clear_glyph_cache();
            cold.render(&scene, &mut target, scale);
        });
        report("paragraph, draw, cold cache", scale, pixels, &t);
        let t = measure(budget, || warm.render(&scene, &mut target, scale));
        report("paragraph, draw, warm cache", scale, pixels, &t);
    }
    let cache = Renderer::new();
    println!("glyph cache budget: {} bytes", cache.glyph_cache().limit);

    // Recording alone, which does not depend on the scale.
    let mut scene = Scene::new();
    let t = measure(budget, || {
        scene.clear();
        scene.append(&path);
    });
    println!(
        "{:<34} {:>5} {:>10} {:>9.3} {:>9.3}",
        "waveform path, copying the scene",
        "-",
        "-",
        t.median.as_secs_f64() * 1e3,
        t.fastest.as_secs_f64() * 1e3
    );
}
