//! Golden images and dumps: representative scenes, drawn at 1x and 2x and
//! compared with the files in `tests/golden`.
//!
//! The dump of each scene must match exactly. Images may differ by up to
//! [`TOLERANCE`] in any channel of any pixel, which allows for the
//! rasteriser's rounding on other processors (see the constant for what
//! was measured). On a mismatch the actual image, a difference image
//! and the actual dump are written under the target directory, and the
//! failure says where.
//!
//! To regenerate the files after an intended change, run locally:
//!
//! ```text
//! MANGUEBYTE_BLESS=1 cargo test -p manguebyte-gfx --test golden
//! ```
//!
//! and review the new images before committing them. With `CI` set,
//! blessing is refused.

use std::f32::consts::PI;
use std::path::PathBuf;

use manguebyte_gfx::snapshot::{Outcome, Snapshots};
use manguebyte_gfx::{
    Align, Arc, Cap, Circle, Color, Dash, Extend, FillRule, Image, Join, Layer, LinearGradient,
    Path, Pixmap, Point, RadialGradient, Rect, Renderer, RoundedRect, Sampling, Scene, Stop,
    Stroke, TextLayout, TextStyle, Transform,
};

/// The largest difference allowed in one channel of one pixel.
///
/// Measured on x86-64 by rendering every scene here, at 1x and 2x, with
/// `tiny-skia`'s SSE2 code path and again with its plain code path (the
/// `simd` feature off): the largest difference was 0. Its aarch64 code
/// path, which the macOS runner takes, has not been measured; it converts
/// floats to integers with ties to even, which can move a value by one
/// level, so one level is allowed. A real change moves far more.
const TOLERANCE: u8 = 1;

/// A triangle wave from 0 to 1 and back, period 1: built from `floor`,
/// which is exact, so the scenes hold the same numbers on every platform.
/// (The platform's `sin` and `cos` differ in the last bit, which would show
/// in the dumps; points on circles come from `Arc::point_at`, which does
/// not.)
fn triangle(x: f32) -> f32 {
    2.0 * (x - x.floor() - 0.5).abs()
}

/// Logical size of every scene.
const WIDTH: f32 = 48.0;
const HEIGHT: f32 = 32.0;

fn background(scene: &mut Scene) {
    scene.fill(Rect::new(0.0, 0.0, WIDTH, HEIGHT), Color::hex(0x1C1F24));
}

fn shapes() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    scene.fill(Rect::new(2.0, 2.0, 8.0, 6.0), Color::hex(0xE8E8E8));
    scene.fill(Rect::new(12.25, 2.5, 7.5, 5.75), Color::hex(0xFF7A00));
    scene.fill(
        RoundedRect::new(Rect::new(22.0, 2.0, 12.0, 8.0), 3.0),
        Color::hex(0x3D8BFF),
    );
    scene.fill(
        Circle::new(Point::new(41.0, 6.0), 4.5),
        Color::hex(0x41D27A),
    );
    // An arc filled closes along its chord.
    scene.fill(
        Arc::new(Point::new(7.0, 18.0), 6.0, 0.25 * PI, 1.25 * PI),
        Color::rgba(255, 255, 255, 200),
    );
    // A five-pointed star, filled even-odd, so its middle is a hole.
    let mut star = Path::new();
    for i in 0..5 {
        let angle = -PI / 2.0 + i as f32 * 4.0 * PI / 5.0;
        let p = Arc::new(Point::new(22.0, 21.0), 7.0, 0.0, 0.0).point_at(angle);
        if i == 0 {
            star.move_to(p);
        } else {
            star.line_to(p);
        }
    }
    star.close();
    star.set_fill_rule(FillRule::EvenOdd);
    scene.fill(&star, Color::hex(0xFFD23F));
    // Quadratic and cubic curves.
    let mut curves = Path::new();
    curves
        .move_to(Point::new(31.0, 30.0))
        .quad_to(Point::new(35.0, 12.0), Point::new(39.0, 30.0))
        .cubic_to(
            Point::new(41.0, 14.0),
            Point::new(47.0, 34.0),
            Point::new(46.0, 16.0),
        )
        .close();
    scene.fill(&curves, Color::rgba(200, 80, 255, 220));
    scene
}

fn strokes() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    let white = Color::hex(0xF0F0F0);
    for (i, cap) in [Cap::Butt, Cap::Round, Cap::Square].into_iter().enumerate() {
        let y = 4.0 + i as f32 * 5.0;
        let mut line = Path::new();
        line.move_to(Point::new(4.0, y))
            .line_to(Point::new(16.0, y));
        scene.stroke(&line, Stroke::new(3.0).with_cap(cap), white);
    }
    for (i, join) in [Join::Miter, Join::Round, Join::Bevel]
        .into_iter()
        .enumerate()
    {
        let x = 22.0 + i as f32 * 9.0;
        let mut corner = Path::new();
        corner
            .move_to(Point::new(x, 14.0))
            .line_to(Point::new(x + 3.5, 3.0))
            .line_to(Point::new(x + 7.0, 14.0));
        scene.stroke(
            &corner,
            Stroke::new(2.5).with_join(join),
            Color::hex(0x3D8BFF),
        );
    }
    let mut dashed = Path::new();
    dashed
        .move_to(Point::new(3.0, 20.5))
        .line_to(Point::new(45.0, 20.5));
    scene.stroke(
        &dashed,
        Stroke::new(1.0).with_dash(Dash::new(3.0, 2.0)),
        Color::hex(0xFF7A00),
    );
    scene.stroke(
        RoundedRect::new(Rect::new(3.5, 23.5, 16.0, 6.0), 2.0),
        Stroke::new(0.5),
        white,
    );
    // A knob's ring: three quarters of a turn, round caps.
    scene.stroke(
        Arc::new(Point::new(34.0, 27.0), 4.0, 0.75 * PI, 1.5 * PI),
        Stroke::round(2.0),
        Color::hex(0x41D27A),
    );
    scene
}

fn gradients() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    let stops = [
        Stop::new(0.0, Color::hex(0xFF3B30)),
        Stop::new(0.5, Color::hex(0xFFD23F)),
        Stop::new(1.0, Color::hex(0x3D8BFF)),
    ];
    for (i, extend) in [Extend::Pad, Extend::Repeat, Extend::Reflect]
        .into_iter()
        .enumerate()
    {
        let y = 2.0 + i as f32 * 6.0;
        scene.fill(
            Rect::new(2.0, y, 28.0, 5.0),
            LinearGradient {
                start: Point::new(10.0, 0.0),
                end: Point::new(20.0, 0.0),
                stops: &stops,
                extend,
            },
        );
    }
    let fade = [
        Stop::new(0.0, Color::rgba(255, 255, 255, 255)),
        Stop::new(1.0, Color::rgba(255, 255, 255, 0)),
    ];
    scene.fill(
        Circle::new(Point::new(39.0, 10.0), 8.0),
        RadialGradient {
            center: Point::new(37.0, 8.0),
            radius: 9.0,
            stops: &fade,
            extend: Extend::Pad,
        },
    );
    scene.fill(
        RoundedRect::new(Rect::new(2.0, 21.0, 44.0, 9.0), 4.5),
        LinearGradient {
            start: Point::new(0.0, 21.0),
            end: Point::new(0.0, 30.0),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
    scene
}

fn groups() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    scene.clipped(
        RoundedRect::new(Rect::new(3.0, 3.0, 26.0, 26.0), 6.0),
        |s| {
            s.fill(Rect::new(0.0, 0.0, WIDTH, HEIGHT), Color::hex(0x2E3440));
            // Overlapping circles composited once, at 60 %.
            s.layer(Layer::with_opacity(0.6), |s| {
                s.fill(
                    Circle::new(Point::new(12.0, 13.0), 8.0),
                    Color::hex(0xFF7A00),
                );
                s.fill(
                    Circle::new(Point::new(20.0, 19.0), 8.0),
                    Color::hex(0x3D8BFF),
                );
            });
        },
    );
    // A square turned an eighth of a turn about its centre.
    scene.transformed(
        Transform::rotate_about(PI / 4.0, Point::new(38.0, 16.0)),
        |s| {
            s.fill(Rect::new(32.0, 10.0, 12.0, 12.0), Color::hex(0x41D27A));
            s.stroke(
                Rect::new(32.0, 10.0, 12.0, 12.0),
                Stroke::new(1.0),
                Color::WHITE,
            );
        },
    );
    scene
}

fn images() -> Scene {
    let mut checker = Pixmap::new(4, 4).expect("small");
    let colors = [
        Color::hex(0xFF3B30),
        Color::hex(0xFFD23F),
        Color::hex(0x3D8BFF),
        Color::rgba(255, 255, 255, 96),
    ];
    for y in 0..4 {
        for x in 0..4 {
            checker.set_pixel(x, y, colors[((x + y) % 4) as usize].premultiply());
        }
    }
    let image = Image::new(checker);
    let mut scene = Scene::new();
    background(&mut scene);
    for (i, sampling) in [Sampling::Nearest, Sampling::Bilinear, Sampling::Bicubic]
        .into_iter()
        .enumerate()
    {
        let x = 2.0 + i as f32 * 15.5;
        scene.image(&image, Rect::new(x, 4.0, 13.0, 13.0), sampling);
    }
    // Drawn smaller than its pixels, and turned.
    scene.transformed(Transform::rotate_about(0.3, Point::new(24.0, 26.0)), |s| {
        s.image(&image, Rect::new(20.0, 22.0, 8.0, 8.0), Sampling::Bilinear)
    });
    scene
}

fn columns() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    // A meter with a gradient that runs up its height.
    let meter = [
        Stop::new(0.0, Color::hex(0x41D27A)),
        Stop::new(0.7, Color::hex(0xFFD23F)),
        Stop::new(1.0, Color::hex(0xFF3B30)),
    ];
    scene.columns(
        Rect::new(2.0, 2.0, 6.0, 28.0),
        &[0.0, 0.0],
        &[0.83, 0.61],
        LinearGradient {
            start: Point::new(0.0, 30.0),
            end: Point::new(0.0, 2.0),
            stops: &meter,
            extend: Extend::Pad,
        },
    );
    // A spectrum: more entries than columns.
    let spectrum: Vec<f32> = (0..200)
        .map(|i| {
            let x = i as f32 / 200.0;
            (0.9 - x * 0.7) * (0.6 + 0.4 * triangle(x * 11.0))
        })
        .collect();
    scene.columns(
        Rect::new(10.0, 2.0, 36.0, 13.0),
        &[0.0; 200],
        &spectrum,
        Color::hex(0x3D8BFF),
    );
    // A waveform overview: one low and one high per entry, around the middle.
    let n = 1_000;
    let low: Vec<f32> = (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            0.5 - 1.8 * t * (1.0 - t) * (0.5 + 0.5 * triangle(t * 19.0))
        })
        .collect();
    let high: Vec<f32> = low.iter().map(|l| 1.0 - l).collect();
    scene.columns(
        Rect::new(10.0, 17.0, 36.0, 13.0),
        &low,
        &high,
        Color::hex(0xFF7A00),
    );
    scene
}

fn knob() -> Scene {
    let mut scene = Scene::new();
    background(&mut scene);
    let center = Point::new(16.0, 16.0);
    let body = [
        Stop::new(0.0, Color::hex(0x5A6070)),
        Stop::new(1.0, Color::hex(0x23262D)),
    ];
    scene.fill(
        Circle::new(center, 10.0),
        RadialGradient {
            center: Point::new(13.0, 12.0),
            radius: 13.0,
            stops: &body,
            extend: Extend::Pad,
        },
    );
    let start = 0.75 * PI;
    let sweep = 1.5 * PI;
    let value = 0.62;
    scene.stroke(
        Arc::new(center, 13.0, start, sweep),
        Stroke::round(2.0),
        Color::hex(0x3A3F4A),
    );
    scene.stroke(
        Arc::new(center, 13.0, start, sweep * value),
        Stroke::round(2.0),
        Color::hex(0xFF7A00),
    );
    let angle = start + sweep * value;
    let mut pointer = Path::new();
    pointer
        .move_to(Arc::new(center, 4.0, 0.0, 0.0).point_at(angle))
        .line_to(Arc::new(center, 8.5, 0.0, 0.0).point_at(angle));
    scene.stroke(&pointer, Stroke::round(1.5), Color::WHITE);
    // A small value bar beside it.
    scene.fill(
        RoundedRect::new(Rect::new(32.0, 13.0, 13.0, 6.0), 3.0),
        Color::hex(0x2E3440),
    );
    scene.fill(
        RoundedRect::new(Rect::new(32.0, 13.0, 13.0 * value, 6.0), 3.0),
        Color::hex(0xFF7A00),
    );
    scene
}

fn render(scene: &Scene, scale: f32) -> Pixmap {
    render_sized(scene, WIDTH, HEIGHT, scale)
}

fn render_sized(scene: &Scene, width: f32, height: f32, scale: f32) -> Pixmap {
    let mut pixels = Pixmap::new((width * scale) as u32, (height * scale) as u32).expect("small");
    Renderer::new().render(scene, &mut pixels, scale);
    pixels
}

/// The lines every text scene sets, regular then bold: kerned pairs, a
/// value in tabular figures with a minus sign, accents, and the `->`
/// ligature.
const SAMPLE: [&str; 2] = [
    "AVATAR To \u{2212}12.5 dB",
    "Caf\u{e9} na\u{ef}ve -> 440 Hz",
];

/// Text at `size` pixels: each sample line regular (400) then bold (700),
/// light on the dark background, on a canvas sized to fit.
fn text_at(size: f32) -> (Scene, f32, f32) {
    let width = (size * 13.0).ceil();
    let height = (size * 5.25).ceil();
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, width, height), Color::hex(0x1C1F24));
    let mut y = size * 0.25;
    for (weight, color) in [(400.0, Color::hex(0xE8E8E8)), (700.0, Color::hex(0xFF7A00))] {
        for line in SAMPLE {
            let style = TextStyle::new(size)
                .with_weight(weight)
                .with_tabular_figures(true)
                .with_color(color);
            let layout = TextLayout::new(line, &style, None);
            scene.text(&layout, Point::new(size * 0.25, y));
            y += layout.line_height();
        }
    }
    (scene, width, height)
}

/// Text where masks cannot be used or must be cut: turned a little, under
/// a clip with round corners, in a half-transparent layer, centred and cut
/// with an ellipsis.
fn text_turned() -> (Scene, f32, f32) {
    let (width, height) = (96.0, 48.0);
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, width, height), Color::hex(0x1C1F24));
    let label = TextLayout::new(
        "Low cut frequency",
        &TextStyle::new(11.0)
            .with_color(Color::WHITE)
            .with_align(Align::Center)
            .with_max_lines(1),
        Some(60.0),
    );
    scene.text(&label, Point::new(18.0, 2.0));
    scene.transformed(Transform::rotate_about(-0.2, Point::new(48.0, 30.0)), |s| {
        let turned = TextLayout::new(
            "Resonance",
            &TextStyle::new(14.0)
                .with_weight(600.0)
                .with_color(Color::hex(0x41D27A)),
            None,
        );
        s.text(&turned, Point::new(14.0, 18.0));
    });
    scene.clipped(
        RoundedRect::new(Rect::new(52.0, 30.0, 40.0, 14.0), 5.0),
        |s| {
            s.layer(Layer::with_opacity(0.6), |s| {
                let big = TextLayout::new(
                    "WIDE",
                    &TextStyle::new(20.0)
                        .with_weight(900.0)
                        .with_color(Color::hex(0x3D8BFF)),
                    None,
                );
                s.text(&big, Point::new(48.0, 24.0));
            });
        },
    );
    (scene, width, height)
}

#[test]
fn scenes_match_their_golden_images_and_dumps() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden-failures");
    let snapshots = Snapshots::new(dir, failures).with_tolerance(TOLERANCE);
    type Build = fn() -> Scene;
    let cases: [(&str, Build); 7] = [
        ("shapes", shapes),
        ("strokes", strokes),
        ("gradients", gradients),
        ("groups", groups),
        ("images", images),
        ("columns", columns),
        ("knob", knob),
    ];
    let mut problems = Vec::new();
    let mut worst = 0;
    for (name, build) in cases {
        let scene = build();
        let mut results = vec![snapshots.check_dump(name, &scene.dump())];
        for scale in [1u32, 2] {
            let image = render(&scene, scale as f32);
            results.push(snapshots.check_image(&format!("{name}@{scale}x"), &image));
        }
        for result in results {
            match result {
                Ok(Outcome::Matched(difference)) => worst = worst.max(difference.max_channel),
                Ok(Outcome::Blessed) => {}
                Err(mismatch) => problems.push(mismatch.to_string()),
            }
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    assert!(worst <= TOLERANCE);
}

#[test]
fn text_matches_its_golden_images_and_dumps() {
    // Text at 12 and 24 pixels, regular and bold, drawn at 1x and 2x,
    // and text under a rotation, a clip and a layer.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let failures = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("golden-failures");
    let snapshots = Snapshots::new(dir, failures).with_tolerance(TOLERANCE);
    let cases = [
        ("text-12", text_at(12.0)),
        ("text-24", text_at(24.0)),
        ("text-turned", text_turned()),
    ];
    let mut problems = Vec::new();
    let mut worst = 0;
    for (name, (scene, width, height)) in cases {
        let mut results = vec![snapshots.check_dump(name, &scene.dump())];
        for scale in [1u32, 2] {
            let image = render_sized(&scene, width, height, scale as f32);
            results.push(snapshots.check_image(&format!("{name}@{scale}x"), &image));
        }
        for result in results {
            match result {
                Ok(Outcome::Matched(difference)) => worst = worst.max(difference.max_channel),
                Ok(Outcome::Blessed) => {}
                Err(mismatch) => problems.push(mismatch.to_string()),
            }
        }
    }
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
    assert!(worst <= TOLERANCE);
}
