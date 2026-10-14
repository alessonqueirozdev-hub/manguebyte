//! A conservative bounding rectangle for everything a scene draws.

use super::{Command, Scene, StoredShape, TEXT_TOLERANCE};
use crate::geom::{Rect, Transform};
use crate::path::bounds_of;

enum Frame {
    Clip(Option<Rect>),
    Transform,
    Layer(Option<Rect>),
}

/// The bounds of a shape in its own coordinates, before any stroke, or
/// `None` if it draws nothing.
pub(crate) fn shape_bounds(scene: &Scene, shape: StoredShape) -> Option<Rect> {
    let bounds = match shape {
        // An empty rectangle draws nothing, filled, stroked or as a clip.
        StoredShape::Rect(r) => (!r.is_empty()).then_some(r)?,
        StoredShape::RoundedRect(r) => (!r.rect.is_empty()).then_some(r.rect)?,
        StoredShape::Circle(c) => {
            if c.radius.is_nan() || c.radius <= 0.0 {
                return None;
            }
            Rect::new(
                c.center.x - c.radius,
                c.center.y - c.radius,
                2.0 * c.radius,
                2.0 * c.radius,
            )
        }
        StoredShape::Arc(a) => {
            if a.radius.is_nan() || a.radius <= 0.0 || a.sweep == 0.0 || !a.sweep.is_finite() {
                return None;
            }
            Rect::new(
                a.center.x - a.radius,
                a.center.y - a.radius,
                2.0 * a.radius,
                2.0 * a.radius,
            )
        }
        StoredShape::Path { points, .. } => bounds_of(&scene.points[points.range()])?,
    };
    bounds.is_finite().then_some(bounds)
}

/// Whether a rectangle has area. A line has none, so filling one adds
/// nothing, while stroking it does once the stroke's reach is added.
fn has_area(r: &Rect) -> bool {
    !r.is_empty()
}

pub(super) fn bounds(scene: &Scene) -> Option<Rect> {
    let mut transform = Transform::IDENTITY;
    let mut transforms: Vec<Transform> = Vec::new();
    // `None` while nothing limits drawing; `Some(r)` clips to `r`, which may
    // be empty, in scene coordinates.
    let mut clip: Option<Rect> = None;
    let mut frames: Vec<Frame> = Vec::new();
    let mut total: Option<Rect> = None;

    let mut add = |local: Option<Rect>, transform: &Transform, clip: &Option<Rect>| {
        let Some(local) = local else {
            return;
        };
        if !transform.is_finite() {
            return;
        }
        let mapped = transform.map_rect(&local);
        let visible = match clip {
            None => Some(mapped),
            Some(c) => mapped.intersect(c),
        };
        if let Some(v) = visible.filter(|v| v.is_finite() && has_area(v)) {
            total = Some(total.map_or(v, |t| t.union(&v)));
        }
    };

    for command in &scene.commands {
        match *command {
            Command::Fill { shape, .. } => {
                let local = shape_bounds(scene, shape).filter(has_area);
                add(local, &transform, &clip);
            }
            Command::Stroke { shape, stroke, .. } => {
                if !(stroke.width > 0.0 && stroke.width.is_finite()) {
                    continue;
                }
                let local = shape_bounds(scene, shape).map(|b| b.outset(stroke.drawn_reach()));
                add(local, &transform, &clip);
            }
            Command::Image { to, .. } => {
                let local = Some(to).filter(|r| r.is_finite() && has_area(r));
                add(local, &transform, &clip);
            }
            Command::Columns { area, values, .. } => {
                let local = (values.len > 0 && !area.is_empty()).then_some(area);
                add(local, &transform, &clip);
            }
            Command::Text { ink, .. } => {
                let local = ink
                    .filter(|r| r.is_finite())
                    .map(|r| r.outset(TEXT_TOLERANCE));
                add(local, &transform, &clip);
            }
            Command::PushClip(shape) => {
                frames.push(Frame::Clip(clip));
                let local = shape_bounds(scene, shape).filter(has_area);
                let mapped = match local {
                    Some(l) if transform.is_finite() => transform.map_rect(&l),
                    _ => Rect::default(),
                };
                clip = Some(match clip {
                    None => mapped,
                    Some(c) => c.intersect(&mapped).unwrap_or_default(),
                });
            }
            Command::PushTransform(by) => {
                frames.push(Frame::Transform);
                transforms.push(transform);
                transform = by.then(transform);
            }
            Command::PushLayer(layer) => {
                frames.push(Frame::Layer(clip));
                // An invisible layer draws nothing: clip it away.
                if layer.opacity.is_nan() || layer.opacity <= 0.0 {
                    clip = Some(Rect::default());
                }
            }
            Command::PopClip => {
                if let Some(Frame::Clip(previous)) = frames.last() {
                    clip = *previous;
                    frames.pop();
                }
            }
            Command::PopTransform => {
                if let Some(Frame::Transform) = frames.last() {
                    transform = transforms.pop().unwrap_or(Transform::IDENTITY);
                    frames.pop();
                }
            }
            Command::PopLayer => {
                if let Some(Frame::Layer(previous)) = frames.last() {
                    clip = *previous;
                    frames.pop();
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use crate::{Arc, Circle, Color, Layer, Path, Point, Rect, Scene, Stroke, Transform};

    /// How far a stroke of width 2 with bevelled corners counts: its reach
    /// of 1, plus the tolerance of half a unit, plus 1/1024 of the reach.
    /// Every term is a power of two, so the sum is exact.
    const WIDTH_2_REACH: f32 = 1.0 + 0.5 + 1.0 / 1024.0;

    #[test]
    fn fills_strokes_and_transforms_add_up() {
        let mut scene = Scene::new();
        scene.fill(Rect::new(10.0, 10.0, 5.0, 5.0), Color::WHITE);
        scene.stroke(
            Rect::new(0.0, 0.0, 4.0, 4.0),
            Stroke::new(2.0).with_join(crate::Join::Bevel),
            Color::WHITE,
        );
        let r = WIDTH_2_REACH;
        assert_eq!(scene.bounds(), Some(Rect::new(-r, -r, 15.0 + r, 15.0 + r)));
        scene.transformed(Transform::scale(2.0, 3.0), |s| {
            s.fill(Circle::new(Point::new(10.0, 10.0), 1.0), Color::WHITE);
        });
        assert_eq!(scene.bounds(), Some(Rect::new(-r, -r, 22.0 + r, 33.0 + r)));
    }

    #[test]
    fn a_miter_limit_counts_as_it_is_drawn() {
        let mut line = Path::new();
        line.move_to(Point::new(0.0, 0.0))
            .line_to(Point::new(10.0, 0.0))
            .line_to(Point::new(0.0, 1.0));
        // The renderer draws a limit that is not finite as 4, and one below
        // 1 as bevelled corners; the bounds count the same.
        for (limit, factor) in [
            (f32::INFINITY, 4.0),
            (f32::NAN, 4.0),
            (0.5, 1.0),
            (3.0, 3.0),
        ] {
            let stroke = Stroke::new(2.0).with_miter_limit(limit);
            assert_eq!(stroke.reach(), factor, "{limit}");
            let mut scene = Scene::new();
            scene.stroke(&line, stroke, Color::WHITE);
            let bounds = scene.bounds().expect("finite");
            assert!(bounds.is_finite(), "{limit}: {bounds:?}");
        }
    }

    #[test]
    fn clips_cut_and_invisible_things_add_nothing() {
        let mut scene = Scene::new();
        scene.clipped(Rect::new(0.0, 0.0, 10.0, 10.0), |s| {
            s.fill(Rect::new(5.0, 5.0, 100.0, 100.0), Color::WHITE);
            s.layer(Layer::with_opacity(0.0), |s| {
                s.fill(Rect::new(-50.0, -50.0, 10.0, 10.0), Color::WHITE);
            });
        });
        // Empty or non-finite shapes, zero-width strokes and a zero sweep.
        scene.fill(Rect::new(0.0, 0.0, 0.0, 99.0), Color::WHITE);
        scene.fill(Rect::new(f32::NAN, 0.0, 99.0, 99.0), Color::WHITE);
        scene.stroke(
            Rect::new(0.0, 0.0, 99.0, 99.0),
            Stroke::new(0.0),
            Color::WHITE,
        );
        scene.stroke(
            Arc::new(Point::ZERO, 99.0, 1.0, 0.0),
            Stroke::new(1.0),
            Color::WHITE,
        );
        scene.fill(&Path::new(), Color::WHITE);
        assert_eq!(scene.bounds(), Some(Rect::new(5.0, 5.0, 5.0, 5.0)));
    }

    #[test]
    fn a_stroked_line_has_bounds_but_a_filled_one_does_not() {
        let mut line = Path::new();
        line.move_to(Point::new(0.0, 5.0))
            .line_to(Point::new(10.0, 5.0));
        let mut scene = Scene::new();
        scene.fill(&line, Color::WHITE);
        assert_eq!(scene.bounds(), None);
        scene.stroke(
            &line,
            Stroke::new(2.0).with_join(crate::Join::Bevel),
            Color::WHITE,
        );
        let r = WIDTH_2_REACH;
        assert_eq!(
            scene.bounds(),
            Some(Rect::new(-r, 5.0 - r, 10.0 + 2.0 * r, 2.0 * r))
        );
    }
}
