//! Random scenes with hostile values: nothing panics, no pixel outside the
//! damage changes, every pixel inside it comes out as a whole render draws
//! it, and every pixel stays validly premultiplied.
//!
//! Widgets build scenes from layout results and from values the audio side
//! publishes, so a scene can carry `NaN`, infinities, huge or negative
//! sizes, degenerate transforms, gradients whose ends coincide (a widget
//! laid out with no height), text of any size, and deep nesting. The
//! renderer must draw what it can and never fail.

use manguebyte_gfx::{
    Align, Arc, Cap, Circle, Color, Dash, Extend, IRect, Image, Join, Layer, LinearGradient,
    MAX_DEPTH, OpticalSize, Paint, Path, Pixmap, Point, PremulColor, RadialGradient, Radii, Rect,
    Renderer, RoundedRect, Sampling, Scene, Stop, Stroke, TextLayout, TextStyle, Transform,
};

struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Mostly ordinary coordinates, sometimes values that break naive code.
    fn number(&mut self) -> f32 {
        const HOSTILE: [f32; 12] = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            0.0,
            -0.0,
            1e30,
            -1e30,
            f32::MAX,
            f32::MIN,
            f32::MIN_POSITIVE,
            1e-30,
            16_777_217.0,
        ];
        match self.below(10) {
            0 => HOSTILE[self.below(HOSTILE.len())],
            1 => self.unit() * 4.0 - 2.0,
            _ => self.unit() * 60.0 - 15.0,
        }
    }

    fn point(&mut self) -> Point {
        Point::new(self.number(), self.number())
    }

    fn rect(&mut self) -> Rect {
        Rect::new(self.number(), self.number(), self.number(), self.number())
    }

    fn color(&mut self) -> Color {
        Color::rgba(
            self.next() as u8,
            self.next() as u8,
            self.next() as u8,
            self.next() as u8,
        )
    }
}

fn stops(r: &mut Random) -> Vec<Stop> {
    (0..r.below(5))
        .map(|_| {
            // Mostly offsets in 0..1 with opaque colours, whose averages are
            // the ones that once came out a hair above one.
            let offset = if r.below(2) == 0 {
                r.unit()
            } else {
                r.number() / 30.0
            };
            let mut color = r.color();
            if r.below(2) == 0 {
                color.a = 255;
            }
            Stop::new(offset, color)
        })
        .collect()
}

fn with_paint<'a>(r: &mut Random, stops: &'a [Stop]) -> Paint<'a> {
    let extend = [Extend::Pad, Extend::Repeat, Extend::Reflect][r.below(3)];
    match r.below(5) {
        0 => Paint::Solid(r.color()),
        1 => Paint::Linear(LinearGradient {
            start: r.point(),
            end: r.point(),
            stops,
            extend,
        }),
        // Ends that coincide, or nearly: a gradient across a widget laid
        // out with no width or height.
        2 => {
            let start = r.point();
            let nudge = [0.0, 1.0 / 65_536.0, 1.0 / 20_000.0, 1.0 / 10_000.0][r.below(4)];
            Paint::Linear(LinearGradient {
                start,
                end: Point::new(start.x, start.y + nudge),
                stops,
                extend,
            })
        }
        3 => Paint::Radial(RadialGradient {
            center: r.point(),
            radius: r.number(),
            stops,
            extend,
        }),
        _ => Paint::Radial(RadialGradient {
            center: r.point(),
            radius: [1e-9, 1.0 / 65_536.0, 1.0 / 20_000.0][r.below(3)],
            stops,
            extend,
        }),
    }
}

fn path(r: &mut Random) -> Path {
    let mut path = Path::new();
    for _ in 0..r.below(14) {
        match r.below(6) {
            0 => {
                path.move_to(r.point());
            }
            1 | 2 => {
                path.line_to(r.point());
            }
            3 => {
                path.quad_to(r.point(), r.point());
            }
            4 => {
                path.cubic_to(r.point(), r.point(), r.point());
            }
            _ => {
                path.close();
            }
        }
    }
    if r.below(2) == 0 {
        path.set_fill_rule(manguebyte_gfx::FillRule::EvenOdd);
    }
    path
}

enum Shaped {
    Rect(Rect),
    Rounded(RoundedRect),
    Circle(Circle),
    Arc(Arc),
    Path(Path),
}

fn shape(r: &mut Random) -> Shaped {
    match r.below(5) {
        0 => Shaped::Rect(r.rect()),
        1 => Shaped::Rounded(RoundedRect::with_radii(
            r.rect(),
            Radii {
                top_left: r.number(),
                top_right: r.number(),
                bottom_right: r.number(),
                bottom_left: r.number(),
            },
        )),
        2 => Shaped::Circle(Circle::new(r.point(), r.number())),
        3 => Shaped::Arc(Arc::new(r.point(), r.number(), r.number(), r.number())),
        _ => Shaped::Path(path(r)),
    }
}

fn stroke(r: &mut Random) -> Stroke {
    let mut stroke = Stroke::new(r.number() / 4.0)
        .with_cap([Cap::Butt, Cap::Round, Cap::Square][r.below(3)])
        .with_join([Join::Miter, Join::Round, Join::Bevel][r.below(3)])
        .with_miter_limit(r.number());
    if r.below(4) == 0 {
        stroke = stroke.with_dash(Dash {
            on: r.number() / 4.0,
            off: r.number() / 4.0,
            offset: r.number(),
        });
    }
    stroke
}

fn transform(r: &mut Random) -> Transform {
    match r.below(6) {
        0 => Transform::translate(r.number(), r.number()),
        1 => Transform::scale(r.number() / 20.0, r.number() / 20.0),
        2 => Transform::rotate_about(r.number(), r.point()),
        3 => Transform::new(
            r.number() / 20.0,
            r.number() / 20.0,
            r.number() / 20.0,
            r.number() / 20.0,
            r.number(),
            r.number(),
        ),
        // Finite but extreme: products of these overflow to infinities and
        // then to `NaN` on the way to device pixels.
        4 => {
            let mut extreme = || [f32::MAX, f32::MIN, 1e30, -1e30, 1.0, -1.0, 0.0][r.below(7)];
            Transform::new(
                extreme(),
                extreme(),
                extreme(),
                extreme(),
                r.number(),
                r.number(),
            )
        }
        _ => Transform::IDENTITY,
    }
}

fn image(r: &mut Random) -> Image {
    let (w, h) = (r.below(5) as u32, r.below(5) as u32);
    let mut pixels = Pixmap::new(w, h).expect("small");
    for y in 0..h {
        for x in 0..w {
            pixels.set_pixel(x, y, r.color().premultiply());
        }
    }
    Image::new(pixels)
}

/// Text with any style: sizes, weights and widths that are not numbers or
/// are extreme, and characters the typeface lacks.
fn hostile_text(r: &mut Random) -> TextLayout {
    const PIECES: [&str; 10] = [
        "a",
        "W",
        " ",
        "\n",
        "->",
        "\u{e9}",
        "\u{1f600}",
        "1",
        "\u{301}",
        "Te",
    ];
    let text: String = (0..r.below(6))
        .map(|_| PIECES[r.below(PIECES.len())])
        .collect();
    // Sizes up to a few hundred pixels: larger ones are clamped by the
    // layout and cost time without exercising anything new.
    let size = match r.below(4) {
        0 => r.number(),
        1 => r.unit() * 300.0,
        _ => 4.0 + r.unit() * 30.0,
    };
    let style = TextStyle::new(size.min(300.0))
        .with_weight(r.number() * 30.0)
        .with_optical_size(OpticalSize::Fixed(r.number()))
        .with_tabular_figures(r.below(2) == 0)
        .with_color(r.color())
        .with_align([Align::Start, Align::Center, Align::End][r.below(3)]);
    let style = if r.below(2) == 0 {
        style.with_max_lines(r.below(3) as u32)
    } else {
        style
    };
    let width = if r.below(2) == 0 {
        None
    } else {
        Some(r.number())
    };
    TextLayout::new(&text, &style, width)
}

fn record(scene: &mut Scene, r: &mut Random, depth: usize) {
    for _ in 0..1 + r.below(5) {
        let stops = stops(r);
        match r.below(10) {
            0 | 1 => {
                let paint = with_paint(r, &stops);
                match shape(r) {
                    Shaped::Rect(s) => scene.fill(s, paint),
                    Shaped::Rounded(s) => scene.fill(s, paint),
                    Shaped::Circle(s) => scene.fill(s, paint),
                    Shaped::Arc(s) => scene.fill(s, paint),
                    Shaped::Path(p) => scene.fill(&p, paint),
                }
            }
            2 | 3 => {
                let paint = with_paint(r, &stops);
                let style = stroke(r);
                match shape(r) {
                    Shaped::Rect(s) => scene.stroke(s, style, paint),
                    Shaped::Rounded(s) => scene.stroke(s, style, paint),
                    Shaped::Circle(s) => scene.stroke(s, style, paint),
                    Shaped::Arc(s) => scene.stroke(s, style, paint),
                    Shaped::Path(p) => scene.stroke(&p, style, paint),
                }
            }
            4 => {
                let sampling =
                    [Sampling::Nearest, Sampling::Bilinear, Sampling::Bicubic][r.below(3)];
                scene.image(&image(r), r.rect(), sampling);
            }
            5 => {
                let n = r.below(40);
                let low: Vec<f32> = (0..n).map(|_| r.number() / 30.0).collect();
                let high: Vec<f32> = (0..n + r.below(2)).map(|_| r.number() / 30.0).collect();
                let paint = with_paint(r, &stops);
                scene.columns(r.rect(), &low, &high, paint);
            }
            6 if depth < MAX_DEPTH + 4 => {
                let clip = shape(r);
                let mut inner = |s: &mut Scene| record(s, r, depth + 1);
                match clip {
                    Shaped::Rect(c) => scene.clipped(c, &mut inner),
                    Shaped::Rounded(c) => scene.clipped(c, &mut inner),
                    Shaped::Circle(c) => scene.clipped(c, &mut inner),
                    Shaped::Arc(c) => scene.clipped(c, &mut inner),
                    Shaped::Path(p) => scene.clipped(&p, &mut inner),
                }
            }
            7 if depth < MAX_DEPTH + 4 => {
                let by = transform(r);
                scene.transformed(by, |s| record(s, r, depth + 1));
            }
            8 if depth < MAX_DEPTH + 4 => {
                let opacity = match r.below(4) {
                    0 => r.number(),
                    1 => 1.0,
                    _ => r.unit(),
                };
                scene.layer(Layer::with_opacity(opacity), |s| record(s, r, depth + 1));
            }
            9 => {
                let layout = hostile_text(r);
                scene.text(&layout, r.point());
            }
            _ => scene.fill(r.rect(), r.color()),
        }
    }
}

fn sentinel(width: u32, height: u32, seed: u64) -> Pixmap {
    let mut r = Random(seed | 1);
    let mut p = Pixmap::new(width, height).expect("small");
    for y in 0..height {
        for x in 0..width {
            p.set_pixel(x, y, r.color().premultiply());
        }
    }
    p
}

#[test]
fn random_hostile_scenes_render_without_harm() {
    let mut r = Random(0x2545_F491_4F6C_DD1D);
    let mut renderer = Renderer::new();
    let mut deepest = 0;
    for round in 0..400u64 {
        let mut scene = Scene::new();
        record(&mut scene, &mut r, 0);

        // The text form balances its braces, and the bounds are finite.
        let dump = scene.dump();
        let opened = dump.lines().filter(|l| l.ends_with('{')).count();
        let closed = dump.lines().filter(|l| l.trim() == "}").count();
        assert_eq!(opened, closed, "{dump}");
        let depth = dump
            .lines()
            .map(|l| (l.len() - l.trim_start().len()) / 2)
            .max()
            .unwrap_or(0);
        deepest = deepest.max(depth);
        if let Some(b) = scene.bounds() {
            assert!(b.is_finite() && !b.is_empty(), "{b:?}");
        }

        let (w, h) = (1 + r.below(24) as u32, 1 + r.below(18) as u32);
        let scale = match r.below(8) {
            0 => r.number(),
            1 => 1.0,
            2 => 2.0,
            _ => 0.25 + r.unit() * 2.5,
        };
        let damage: Vec<IRect> = (0..r.below(4))
            .map(|_| {
                IRect::new(
                    r.below(40) as i32 - 10,
                    r.below(30) as i32 - 8,
                    r.below(30) as u32,
                    r.below(25) as u32,
                )
            })
            .collect();
        let before = sentinel(w, h, round);
        let mut whole = before.clone();
        renderer.render(&scene, &mut whole, scale);
        let mut part = before.clone();
        renderer.render_damage(&scene, &mut part, scale, &damage);
        check_partial(round, &scene, &before, &whole, &part, &damage);
    }
    // The nesting reached past the depth limit, so skipping was exercised.
    assert!(deepest > MAX_DEPTH, "deepest nesting {deepest}");
}

/// Checks a partial render against the target before it and a whole
/// render: outside the damage nothing changed, inside every pixel is the
/// whole render's, and every pixel of both is validly premultiplied.
/// Returns how many pixels inside the damage the scene changed.
fn check_partial(
    round: u64,
    scene: &Scene,
    before: &Pixmap,
    whole: &Pixmap,
    part: &Pixmap,
    damage: &[IRect],
) -> usize {
    let mut changed = 0;
    for y in 0..before.height() {
        for x in 0..before.width() {
            let inside = damage
                .iter()
                .any(|d| d.contains(i64::from(x), i64::from(y)));
            let expected = if inside { whole } else { before };
            changed += usize::from(inside && whole.pixel(x, y) != before.pixel(x, y));
            // `pixel` checks that the bytes are a valid premultiplied colour.
            let (p, w): (Option<PremulColor>, Option<PremulColor>) =
                (part.pixel(x, y), whole.pixel(x, y));
            assert!(
                p.is_some() && w.is_some(),
                "round {round}: ({x}, {y}) is not premultiplied
{}",
                scene.dump()
            );
            assert_eq!(
                p,
                expected.pixel(x, y),
                "round {round}: ({x}, {y}), inside the damage: {inside}; damage {damage:?}
{}",
                scene.dump()
            );
        }
    }
    changed
}

/// Ordinary drawing across a `width` by `height` area: what an editor
/// draws, with every kind of command, at random places and sizes.
fn ordinary(scene: &mut Scene, r: &mut Random, width: f32, height: f32, depth: usize) {
    let at = |r: &mut Random| Point::new(r.unit() * width, r.unit() * height);
    let stops = [
        Stop::new(0.0, Color::rgba(255, 60, 0, 230)),
        Stop::new(r.unit(), Color::rgba(0, 90, 255, 140)),
        Stop::new(1.0, Color::rgba(20, 220, 90, 255)),
    ];
    let extend = [Extend::Pad, Extend::Repeat, Extend::Reflect][r.below(3)];
    let gradient = LinearGradient {
        start: at(r),
        end: at(r),
        stops: &stops,
        extend,
    };
    for _ in 0..2 + r.below(5) {
        let center = at(r);
        let size = 2.0 + r.unit() * width * 0.4;
        match r.below(10) {
            0 => scene.fill(
                Rect::new(center.x, center.y, size, size * r.unit()),
                gradient,
            ),
            1 => scene.fill(Circle::new(center, size * 0.5), r.color()),
            2 => scene.stroke(
                Arc::new(center, size * 0.5, r.unit() * 7.0, r.unit() * 6.0),
                Stroke::round(0.5 + r.unit() * 6.0),
                gradient,
            ),
            3 => {
                let mut path = Path::new();
                path.move_to(at(r));
                for _ in 0..3 + r.below(6) {
                    match r.below(3) {
                        0 => path.line_to(at(r)),
                        1 => path.quad_to(at(r), at(r)),
                        _ => path.cubic_to(at(r), at(r), at(r)),
                    };
                }
                if r.below(2) == 0 {
                    scene.fill(&path, r.color());
                } else {
                    let stroke = Stroke::new(0.3 + r.unit() * 5.0)
                        .with_join([Join::Miter, Join::Round, Join::Bevel][r.below(3)])
                        .with_cap([Cap::Butt, Cap::Round, Cap::Square][r.below(3)]);
                    scene.stroke(&path, stroke, r.color());
                }
            }
            4 => {
                let sampling =
                    [Sampling::Nearest, Sampling::Bilinear, Sampling::Bicubic][r.below(3)];
                scene.image(
                    &image(r),
                    Rect::new(center.x, center.y, size, size * 0.7),
                    sampling,
                );
            }
            5 => {
                let n = 1 + r.below(300);
                let low: Vec<f32> = (0..n).map(|_| r.unit() * 0.5).collect();
                let high: Vec<f32> = (0..n).map(|_| 0.5 + r.unit() * 0.5).collect();
                let area = Rect::new(center.x - size, center.y, size * 2.0, size);
                if r.below(2) == 0 {
                    scene.columns(area, &low, &high, r.color());
                } else {
                    scene.columns(area, &low, &high, gradient);
                }
            }
            6 if depth < 3 => {
                let clip = RoundedRect::new(
                    Rect::new(center.x - size, center.y - size, size * 2.0, size * 1.5),
                    size * 0.3,
                );
                scene.clipped(clip, |s| ordinary(s, r, width, height, depth + 1));
            }
            7 if depth < 3 => {
                let by = Transform::rotate_about(r.unit() * 7.0, center);
                scene.transformed(by, |s| ordinary(s, r, width, height, depth + 1));
            }
            8 if depth < 3 => {
                let opacity = 0.1 + r.unit() * 0.85;
                scene.layer(Layer::with_opacity(opacity), |s| {
                    ordinary(s, r, width, height, depth + 1)
                });
            }
            9 => {
                const LABELS: [&str; 4] = ["Gain", "Low cut -> 12 dB", "AVATAR", "Wet 50 %"];
                let style = TextStyle::new(5.0 + r.unit() * 30.0)
                    .with_weight(100.0 + r.unit() * 800.0)
                    .with_color(r.color());
                let layout = TextLayout::new(LABELS[r.below(LABELS.len())], &style, None);
                scene.text(&layout, center);
            }
            _ => scene.fill(
                RoundedRect::new(Rect::new(center.x, center.y, size, size), size * 0.25),
                r.color(),
            ),
        }
    }
}

#[test]
fn random_scenes_repaint_in_part_exactly_as_whole_on_large_targets() {
    // Targets of several cells of the renderer's grid, scaled so that the
    // scenes' coordinates cross them, and damage anywhere inside: the
    // windows of partial renders then cut and clamp outlines that the
    // whole render draws uncut.
    let mut r = Random(0x9E37_79B9_7F4A_7C15);
    let mut renderer = Renderer::new();
    let mut drawn = 0;
    for round in 0..100u64 {
        let (w, h) = (90 + r.below(120) as u32, 70 + r.below(100) as u32);
        let scale = [1.0, 1.25, 1.5, 2.0, 3.0][r.below(5)];
        let mut scene = Scene::new();
        ordinary(&mut scene, &mut r, w as f32 / scale, h as f32 / scale, 0);
        if r.below(3) == 0 {
            record(&mut scene, &mut r, 0);
        }
        let damage: Vec<IRect> = (0..1 + r.below(5))
            .map(|_| {
                IRect::new(
                    r.below(w as usize) as i32 - 4,
                    r.below(h as usize) as i32 - 4,
                    1 + r.below(70) as u32,
                    1 + r.below(50) as u32,
                )
            })
            .collect();
        let before = sentinel(w, h, round);
        let mut whole = before.clone();
        renderer.render(&scene, &mut whole, scale);
        let mut part = before.clone();
        renderer.render_damage(&scene, &mut part, scale, &damage);
        let changed = check_partial(round, &scene, &before, &whole, &part, &damage);
        drawn += usize::from(changed > 0);
    }
    // Most scenes drew inside their damage, so the comparison had pixels.
    assert!(
        drawn > 80,
        "only {drawn} of 100 scenes drew inside the damage"
    );
}

#[test]
fn images_under_extreme_finite_transforms_render_without_harm() {
    // Coefficients that are finite but whose products overflow: an image's
    // corners then map to infinities or `NaN` while the transform itself
    // is accepted. Every sampling, whole and partial renders.
    let mut r = Random(0xD1B5_4A32_D192_ED03);
    let mut renderer = Renderer::new();
    let extremes = [f32::MAX, f32::MIN, 1e30, -1e30, 1e19, 1.0, -1.0, 0.0];
    for round in 0..600u64 {
        let mut pick = || extremes[r.below(extremes.len())];
        let by = Transform::new(pick(), pick(), pick(), pick(), r.number(), r.number());
        let sampling = [Sampling::Nearest, Sampling::Bilinear, Sampling::Bicubic][r.below(3)];
        let picture = image(&mut r);
        let to = r.rect();
        let mut scene = Scene::new();
        scene.transformed(by, |s| s.image(&picture, to, sampling));
        let damage = [IRect::new(
            r.below(16) as i32,
            r.below(16) as i32,
            1 + r.below(16) as u32,
            1 + r.below(16) as u32,
        )];
        let before = sentinel(16, 16, round);
        let mut whole = before.clone();
        renderer.render(&scene, &mut whole, 1.0);
        let mut part = before.clone();
        renderer.render_damage(&scene, &mut part, 1.0, &damage);
        check_partial(round, &scene, &before, &whole, &part, &damage);
    }
}
