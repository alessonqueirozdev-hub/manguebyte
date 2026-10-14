//! The exact text form of a scene.

use std::fmt::Write;

use super::{Command, Scene, StoredPaint, StoredShape};
use crate::color::Color;
use crate::geom::{Point, Rect};
use crate::hash::Fnv;
use crate::image::Sampling;
use crate::paint::{Cap, Extend, Join, Stroke};
use crate::path::{FillRule, Verb};

/// Paths with more segments than this are printed as a summary.
const PATH_IN_FULL: usize = 32;
/// Column data with more entries than this is printed as a summary.
const COLUMNS_IN_FULL: usize = 8;
/// Text with more glyphs than this is printed as a summary.
const GLYPHS_IN_FULL: usize = 64;
/// Text with more characters than this is printed cut, followed by `...`.
const CHARACTERS_IN_FULL: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Clip,
    Transform,
    Layer,
}

pub(super) fn dump(scene: &Scene) -> String {
    let mut out = String::new();
    let mut open: Vec<Open> = Vec::new();
    for command in &scene.commands {
        let close = match command {
            Command::PopClip => Some(Open::Clip),
            Command::PopTransform => Some(Open::Transform),
            Command::PopLayer => Some(Open::Layer),
            _ => None,
        };
        if let Some(kind) = close {
            // An end with nothing matching open (possible only if a scene
            // was cleared inside a closure) is left out.
            if open.last() == Some(&kind) {
                open.pop();
                indent(&mut out, open.len());
                out.push_str("}\n");
            }
            continue;
        }
        indent(&mut out, open.len());
        match *command {
            Command::Fill { shape, paint } => {
                out.push_str("fill ");
                write_shape(&mut out, scene, shape);
                out.push(' ');
                write_paint(&mut out, scene, paint);
            }
            Command::Stroke {
                shape,
                stroke,
                paint,
            } => {
                out.push_str("stroke ");
                write_shape(&mut out, scene, shape);
                out.push(' ');
                write_stroke(&mut out, &stroke);
                out.push(' ');
                write_paint(&mut out, scene, paint);
            }
            Command::Image {
                image,
                to,
                sampling,
            } => {
                let image = &scene.images[image];
                let _ = write!(
                    out,
                    "image {}x{} fnv1a {:016x} ",
                    image.width(),
                    image.height(),
                    image.content_hash()
                );
                write_rect(&mut out, &to);
                out.push_str(match sampling {
                    Sampling::Nearest => " nearest",
                    Sampling::Bilinear => " bilinear",
                    Sampling::Bicubic => " bicubic",
                });
            }
            Command::Columns {
                area,
                values,
                paint,
            } => {
                out.push_str("columns ");
                write_rect(&mut out, &area);
                let values = &scene.values[values.range()];
                let (low, high) = values.split_at(values.len() / 2);
                if low.len() <= COLUMNS_IN_FULL {
                    out.push_str(" low [");
                    write_list(&mut out, low);
                    out.push_str("] high [");
                    write_list(&mut out, high);
                    out.push(']');
                } else {
                    let mut hash = Fnv::new();
                    for &v in values {
                        hash.f32(v);
                    }
                    let _ = write!(out, " {} entries fnv1a {:016x}", low.len(), hash.finish());
                }
                out.push(' ');
                write_paint(&mut out, scene, paint);
            }
            Command::PushClip(shape) => {
                out.push_str("clip ");
                write_shape(&mut out, scene, shape);
                out.push_str(" {");
                open.push(Open::Clip);
            }
            Command::PushTransform(t) => {
                out.push_str("transform matrix(");
                write_list(&mut out, &[t.a, t.b, t.c, t.d, t.e, t.f]);
                out.push_str(") {");
                open.push(Open::Transform);
            }
            Command::PushLayer(layer) => {
                out.push_str("layer opacity ");
                write_number(&mut out, layer.opacity);
                out.push_str(" {");
                open.push(Open::Layer);
            }
            Command::Text {
                text,
                glyphs,
                size,
                weight,
                optical_size,
                color,
                ..
            } => {
                let text = &scene.strings[text.range()];
                out.push_str("text \"");
                for c in text.chars().take(CHARACTERS_IN_FULL) {
                    let _ = write!(out, "{}", c.escape_debug());
                }
                out.push('"');
                if text.chars().nth(CHARACTERS_IN_FULL).is_some() {
                    out.push_str("...");
                }
                out.push_str(" size ");
                write_number(&mut out, size);
                out.push_str(" weight ");
                write_number(&mut out, weight);
                out.push_str(" opsz ");
                write_number(&mut out, optical_size);
                out.push_str(" solid(");
                write_color(&mut out, color);
                out.push_str(") [");
                let glyphs = &scene.glyphs[glyphs.range()];
                if glyphs.len() <= GLYPHS_IN_FULL {
                    for (i, glyph) in glyphs.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        let _ = write!(out, "{} ", glyph.id);
                        write_point(&mut out, glyph.position);
                    }
                } else {
                    let mut hash = Fnv::new();
                    for glyph in glyphs {
                        hash.u32(u32::from(glyph.id));
                        hash.f32(glyph.position.x);
                        hash.f32(glyph.position.y);
                    }
                    let positions: Vec<Point> = glyphs.iter().map(|g| g.position).collect();
                    let _ = write!(out, "{} glyphs ", glyphs.len());
                    if let Some(bounds) = crate::path::bounds_of(&positions) {
                        out.push_str("at ");
                        write_rect(&mut out, &bounds);
                        out.push(' ');
                    }
                    let _ = write!(out, "fnv1a {:016x}", hash.finish());
                }
                out.push(']');
            }
            Command::PopClip | Command::PopTransform | Command::PopLayer => {}
        }
        out.push('\n');
    }
    // Close what is still open, so the text always balances.
    while open.pop().is_some() {
        indent(&mut out, open.len());
        out.push_str("}\n");
    }
    out
}

fn indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_number(out: &mut String, value: f32) {
    // `Display` for floats prints the shortest decimal that reads back as
    // the same value, with no exponent, the same way on every platform.
    let _ = write!(out, "{value}");
}

fn write_list(out: &mut String, values: &[f32]) {
    for (i, &v) in values.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        write_number(out, v);
    }
}

fn write_point(out: &mut String, p: Point) {
    write_number(out, p.x);
    out.push(' ');
    write_number(out, p.y);
}

fn write_rect(out: &mut String, r: &Rect) {
    out.push_str("rect(");
    write_list(out, &[r.x, r.y, r.width, r.height]);
    out.push(')');
}

fn write_color(out: &mut String, c: Color) {
    let _ = write!(out, "#{:02x}{:02x}{:02x}{:02x}", c.r, c.g, c.b, c.a);
}

fn rule_name(rule: FillRule) -> &'static str {
    match rule {
        FillRule::NonZero => "nonzero",
        FillRule::EvenOdd => "evenodd",
    }
}

fn write_shape(out: &mut String, scene: &Scene, shape: StoredShape) {
    match shape {
        StoredShape::Rect(r) => write_rect(out, &r),
        StoredShape::RoundedRect(r) => {
            out.push_str("rounded_rect(");
            write_list(out, &[r.rect.x, r.rect.y, r.rect.width, r.rect.height]);
            out.push_str(" radii ");
            write_list(
                out,
                &[
                    r.radii.top_left,
                    r.radii.top_right,
                    r.radii.bottom_right,
                    r.radii.bottom_left,
                ],
            );
            out.push(')');
        }
        StoredShape::Circle(c) => {
            out.push_str("circle(");
            write_list(out, &[c.center.x, c.center.y, c.radius]);
            out.push(')');
        }
        StoredShape::Arc(a) => {
            out.push_str("arc(");
            write_list(out, &[a.center.x, a.center.y, a.radius, a.start, a.sweep]);
            out.push(')');
        }
        StoredShape::Path {
            verbs,
            points,
            rule,
        } => {
            let verbs = &scene.verbs[verbs.range()];
            let points = &scene.points[points.range()];
            let _ = write!(out, "path({}", rule_name(rule));
            if verbs.len() <= PATH_IN_FULL {
                let mut next = points.iter();
                for &verb in verbs {
                    out.push_str(match verb {
                        Verb::Move => " M",
                        Verb::Line => " L",
                        Verb::Quad => " Q",
                        Verb::Cubic => " C",
                        Verb::Close => " Z",
                    });
                    for p in next.by_ref().take(verb.points()) {
                        out.push(' ');
                        write_point(out, *p);
                    }
                }
            } else {
                let mut hash = Fnv::new();
                for &verb in verbs {
                    hash.bytes(&[verb as u8]);
                }
                for p in points {
                    hash.f32(p.x);
                    hash.f32(p.y);
                }
                let _ = write!(out, " {} segments {} points ", verbs.len(), points.len());
                if let Some(bounds) = crate::path::bounds_of(points) {
                    out.push_str("bounds ");
                    write_rect(out, &bounds);
                    out.push(' ');
                }
                let _ = write!(out, "fnv1a {:016x}", hash.finish());
            }
            out.push(')');
        }
    }
}

fn write_stroke(out: &mut String, stroke: &Stroke) {
    out.push_str("width ");
    write_number(out, stroke.width);
    out.push_str(match stroke.cap {
        Cap::Butt => " cap butt",
        Cap::Round => " cap round",
        Cap::Square => " cap square",
    });
    match stroke.join {
        Join::Miter => {
            out.push_str(" join miter ");
            write_number(out, stroke.miter_limit);
        }
        Join::Round => out.push_str(" join round"),
        Join::Bevel => out.push_str(" join bevel"),
    }
    if let Some(dash) = stroke.dash {
        out.push_str(" dash ");
        write_list(out, &[dash.on, dash.off, dash.offset]);
    }
}

fn extend_name(extend: Extend) -> &'static str {
    match extend {
        Extend::Pad => "pad",
        Extend::Repeat => "repeat",
        Extend::Reflect => "reflect",
    }
}

fn write_paint(out: &mut String, scene: &Scene, paint: StoredPaint) {
    let write_stops = |out: &mut String, stops: super::Span| {
        out.push('[');
        for (i, stop) in scene.stops[stops.range()].iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            write_number(out, stop.offset);
            out.push(' ');
            write_color(out, stop.color);
        }
        out.push(']');
    };
    match paint {
        StoredPaint::Solid(color) => {
            out.push_str("solid(");
            write_color(out, color);
            out.push(')');
        }
        StoredPaint::Linear {
            start,
            end,
            stops,
            extend,
        } => {
            out.push_str("linear(");
            write_point(out, start);
            out.push_str(", ");
            write_point(out, end);
            let _ = write!(out, ", {} ", extend_name(extend));
            write_stops(out, stops);
            out.push(')');
        }
        StoredPaint::Radial {
            center,
            radius,
            stops,
            extend,
        } => {
            out.push_str("radial(");
            write_point(out, center);
            out.push_str(", ");
            write_number(out, radius);
            let _ = write!(out, ", {} ", extend_name(extend));
            write_stops(out, stops);
            out.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        Arc, Cap, Circle, Color, Dash, Extend, FillRule, Image, Join, Layer, LinearGradient, Path,
        Pixmap, Point, RadialGradient, Rect, RoundedRect, Sampling, Scene, Stop, Stroke, Transform,
    };

    #[test]
    fn every_command_has_an_exact_line() {
        let stops = [
            Stop::new(0.0, Color::hex(0x102030)),
            Stop::new(0.5, Color::rgba(255, 0, 0, 128)),
        ];
        let mut path = Path::new();
        path.move_to(Point::new(0.5, -1.0))
            .line_to(Point::new(2.0, 0.0))
            .quad_to(Point::new(3.0, 1.0), Point::new(4.0, 0.0))
            .cubic_to(
                Point::new(5.0, 1.0),
                Point::new(6.0, 2.0),
                Point::new(7.0, 3.0),
            )
            .close();
        path.set_fill_rule(FillRule::EvenOdd);
        let image = Image::new(Pixmap::new(2, 3).expect("small"));
        let mut scene = Scene::new();
        scene.fill(Rect::new(1.0, 2.0, 3.0, 4.0), Color::hex(0xFF7A00));
        scene.fill(
            RoundedRect::new(Rect::new(0.0, 0.0, 10.0, 5.0), 2.5),
            LinearGradient {
                start: Point::new(0.0, 0.0),
                end: Point::new(10.0, 0.0),
                stops: &stops,
                extend: Extend::Reflect,
            },
        );
        scene.stroke(
            Arc::new(Point::new(5.0, 5.0), 4.0, 2.25, -1.5),
            Stroke::round(3.0).with_dash(Dash::new(1.0, 2.0)),
            RadialGradient {
                center: Point::new(5.0, 5.0),
                radius: 4.0,
                stops: &stops,
                extend: Extend::Pad,
            },
        );
        scene.clipped(Circle::new(Point::new(1.0, 1.0), 0.1), |s| {
            s.transformed(Transform::translate(10.0, -20.0), |s| {
                s.layer(Layer::with_opacity(0.25), |s| {
                    s.fill(&path, Color::BLACK);
                    s.stroke(
                        &path,
                        Stroke::new(1.0)
                            .with_cap(Cap::Square)
                            .with_join(Join::Bevel),
                        Color::WHITE,
                    );
                });
            });
        });
        scene.image(&image, Rect::new(0.0, 0.0, 4.0, 6.0), Sampling::Nearest);
        scene.columns(
            Rect::new(0.0, 0.0, 4.0, 10.0),
            &[0.0, 0.0],
            &[1.0, 0.05],
            Color::hex(0x00FF00),
        );
        let expected = "\
fill rect(1 2 3 4) solid(#ff7a00ff)
fill rounded_rect(0 0 10 5 radii 2.5 2.5 2.5 2.5) linear(0 0, 10 0, reflect [0 #102030ff, 0.5 #ff000080])
stroke arc(5 5 4 2.25 -1.5) width 3 cap round join round dash 1 2 0 radial(5 5, 4, pad [0 #102030ff, 0.5 #ff000080])
clip circle(1 1 0.1) {
  transform matrix(1 0 0 1 10 -20) {
    layer opacity 0.25 {
      fill path(evenodd M 0.5 -1 L 2 0 Q 3 1 4 0 C 5 1 6 2 7 3 Z) solid(#000000ff)
      stroke path(evenodd M 0.5 -1 L 2 0 Q 3 1 4 0 C 5 1 6 2 7 3 Z) width 1 cap square join bevel solid(#ffffffff)
    }
  }
}
image 2x3 fnv1a "
            .to_owned()
            + &format!("{:016x}", image.content_hash())
            + " rect(0 0 4 6) nearest
columns rect(0 0 4 10) low [0 0] high [1 0.05] solid(#00ff00ff)
";
        assert_eq!(scene.dump(), expected);
    }

    #[test]
    fn large_data_is_summarised_by_size_and_hash() {
        let mut path = Path::new();
        path.move_to(Point::ZERO);
        for i in 1..=40 {
            path.line_to(Point::new(i as f32, (i % 2) as f32));
        }
        let mut scene = Scene::new();
        scene.fill(&path, Color::WHITE);
        let values: Vec<f32> = (0..9).map(|i| i as f32 / 8.0).collect();
        scene.columns(
            Rect::new(0.0, 0.0, 9.0, 1.0),
            &values,
            &values,
            Color::WHITE,
        );
        let dump = scene.dump();
        let lines: Vec<&str> = dump.lines().collect();
        assert!(
            lines[0].starts_with(
                "fill path(nonzero 41 segments 41 points bounds rect(0 0 40 1) fnv1a "
            ),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with("columns rect(0 0 9 1) 9 entries fnv1a "),
            "{}",
            lines[1]
        );

        // Moving one point, with the same count and bounds, changes the hash
        // and nothing else.
        let mut moved = Path::new();
        moved.move_to(Point::ZERO);
        for i in 1..=40 {
            let y = if i == 17 { 0.5 } else { (i % 2) as f32 };
            moved.line_to(Point::new(i as f32, y));
        }
        let mut other = Scene::new();
        other.fill(&moved, Color::WHITE);
        let other = other.dump();
        let other_line = other.lines().next().expect("a line");
        assert_ne!(lines[0], other_line);
        let prefix = |line: &str| line.split(" fnv1a ").next().map(str::to_owned);
        assert_eq!(prefix(lines[0]), prefix(other_line));
    }

    #[test]
    fn non_finite_numbers_are_printed_as_such() {
        let mut scene = Scene::new();
        scene.fill(
            Rect::new(f32::NAN, f32::INFINITY, -0.0, 1e-3),
            Color::TRANSPARENT,
        );
        assert_eq!(
            scene.dump(),
            "fill rect(NaN inf -0 0.001) solid(#00000000)\n"
        );
    }
}
