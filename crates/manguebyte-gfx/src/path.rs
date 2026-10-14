//! Paths: outlines made of lines and Bézier curves.

use crate::geom::{Arc, Circle, Point, Rect, RoundedRect};
use crate::trig::{sin_cos, sin_cos_f64};

/// Which parts of a path count as inside when it is filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FillRule {
    /// Inside where the outline winds around a point a non-zero number of
    /// times: overlapping parts drawn the same way stay filled.
    #[default]
    NonZero,
    /// Inside where a ray from the point crosses the outline an odd number
    /// of times: overlapping parts make holes.
    EvenOdd,
}

/// One step of a path.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// Starts a new subpath at a point.
    MoveTo(Point),
    /// A straight line to a point.
    LineTo(Point),
    /// A quadratic Bézier curve: a control point, then the end point.
    QuadTo(Point, Point),
    /// A cubic Bézier curve: two control points, then the end point.
    CubicTo(Point, Point, Point),
    /// A straight line back to where the subpath started, closing it.
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Verb {
    Move,
    Line,
    Quad,
    Cubic,
    Close,
}

impl Verb {
    pub(crate) fn points(self) -> usize {
        match self {
            Verb::Move | Verb::Line => 1,
            Verb::Quad => 2,
            Verb::Cubic => 3,
            Verb::Close => 0,
        }
    }
}

/// An outline made of lines and curves, in one or more subpaths.
///
/// Build it with [`move_to`](Path::move_to), [`line_to`](Path::line_to),
/// [`quad_to`](Path::quad_to), [`cubic_to`](Path::cubic_to) and
/// [`close`](Path::close), which return the path so calls can be chained.
/// As in HTML canvas, a line or curve with no subpath to continue starts
/// one at its first point, and drawing after [`close`](Path::close)
/// continues from where the closed subpath started.
///
/// [`clear`](Path::clear) keeps the memory, so a widget that rebuilds a
/// path every frame does not allocate once the path has reached its size.
///
/// ```
/// use manguebyte_gfx::{Path, Point, Segment};
///
/// let mut triangle = Path::new();
/// triangle
///     .move_to(Point::new(0.0, 0.0))
///     .line_to(Point::new(4.0, 0.0))
///     .line_to(Point::new(0.0, 3.0))
///     .close();
/// assert_eq!(triangle.segments().count(), 4);
/// assert_eq!(triangle.segments().last(), Some(Segment::Close));
/// ```
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Path {
    verbs: Vec<Verb>,
    points: Vec<Point>,
    fill_rule: FillRule,
    /// Where the current subpath started, if one is open or was just closed.
    start: Option<Point>,
    /// Whether the last verb leaves a current point to continue from.
    open: bool,
}

impl Path {
    /// An empty path with the non-zero fill rule.
    pub fn new() -> Self {
        Path::default()
    }

    /// An empty path with room for `segments` segments of up to three
    /// points each, so that building it does not allocate.
    pub fn with_capacity(segments: usize) -> Self {
        Path {
            verbs: Vec::with_capacity(segments),
            points: Vec::with_capacity(segments.saturating_mul(3)),
            ..Path::default()
        }
    }

    /// Removes every segment, keeping the memory and the fill rule.
    pub fn clear(&mut self) {
        self.verbs.clear();
        self.points.clear();
        self.start = None;
        self.open = false;
    }

    /// The fill rule.
    pub fn fill_rule(&self) -> FillRule {
        self.fill_rule
    }

    /// Changes the fill rule.
    pub fn set_fill_rule(&mut self, rule: FillRule) -> &mut Self {
        self.fill_rule = rule;
        self
    }

    /// Whether it has no segment.
    pub fn is_empty(&self) -> bool {
        self.verbs.is_empty()
    }

    /// Starts a new subpath at `to`. Two moves in a row keep only the
    /// second.
    pub fn move_to(&mut self, to: Point) -> &mut Self {
        if self.verbs.last() == Some(&Verb::Move) {
            if let Some(last) = self.points.last_mut() {
                *last = to;
            }
        } else {
            self.verbs.push(Verb::Move);
            self.points.push(to);
        }
        self.start = Some(to);
        self.open = true;
        self
    }

    /// Makes sure a subpath is open, starting one at `at` if needed.
    fn ensure_open(&mut self, at: Point) {
        if !self.open {
            let from = self.start.unwrap_or(at);
            self.move_to(from);
        }
    }

    /// A straight line to `to`.
    pub fn line_to(&mut self, to: Point) -> &mut Self {
        if !self.open && self.start.is_none() {
            // Nothing to draw from: the line only starts a subpath.
            return self.move_to(to);
        }
        self.ensure_open(to);
        self.verbs.push(Verb::Line);
        self.points.push(to);
        self
    }

    /// A quadratic Bézier curve through the control point `control` to
    /// `to`.
    pub fn quad_to(&mut self, control: Point, to: Point) -> &mut Self {
        self.ensure_open(control);
        self.verbs.push(Verb::Quad);
        self.points.extend_from_slice(&[control, to]);
        self
    }

    /// A cubic Bézier curve through the control points `c1` and `c2` to
    /// `to`.
    pub fn cubic_to(&mut self, c1: Point, c2: Point, to: Point) -> &mut Self {
        self.ensure_open(c1);
        self.verbs.push(Verb::Cubic);
        self.points.extend_from_slice(&[c1, c2, to]);
        self
    }

    /// Closes the current subpath with a straight line to its start. Does
    /// nothing if no subpath is open.
    pub fn close(&mut self) -> &mut Self {
        if self.open {
            self.verbs.push(Verb::Close);
            self.open = false;
        }
        self
    }

    /// The segments, in order.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        Segments {
            verbs: &self.verbs,
            points: &self.points,
        }
    }

    /// The smallest rectangle containing every point, control points
    /// included, so it also contains the curves. `None` for an empty path.
    pub fn bounds(&self) -> Option<Rect> {
        bounds_of(&self.points)
    }

    pub(crate) fn verbs(&self) -> &[Verb] {
        &self.verbs
    }

    pub(crate) fn points(&self) -> &[Point] {
        &self.points
    }

    /// The outline of a rectangle, clockwise from its top-left corner.
    pub fn rect(rect: Rect) -> Path {
        let mut path = Path::new();
        push_rect(&mut path.verbs, &mut path.points, rect);
        path
    }

    /// The outline of a rounded rectangle, clockwise.
    pub fn rounded_rect(rect: RoundedRect) -> Path {
        let mut path = Path::new();
        push_rounded_rect(&mut path.verbs, &mut path.points, rect);
        path
    }

    /// The outline of a circle, clockwise from its rightmost point.
    pub fn circle(circle: Circle) -> Path {
        let mut path = Path::new();
        push_circle(&mut path.verbs, &mut path.points, circle);
        path
    }

    /// An arc as cubic curves, open.
    pub fn arc(arc: Arc) -> Path {
        let mut path = Path::new();
        push_arc(&mut path.verbs, &mut path.points, arc);
        path
    }
}

struct Segments<'a> {
    verbs: &'a [Verb],
    points: &'a [Point],
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        let (&verb, rest) = self.verbs.split_first()?;
        self.verbs = rest;
        let n = verb.points();
        let p = &self.points[..n];
        self.points = &self.points[n..];
        Some(match verb {
            Verb::Move => Segment::MoveTo(p[0]),
            Verb::Line => Segment::LineTo(p[0]),
            Verb::Quad => Segment::QuadTo(p[0], p[1]),
            Verb::Cubic => Segment::CubicTo(p[0], p[1], p[2]),
            Verb::Close => Segment::Close,
        })
    }
}

pub(crate) fn bounds_of(points: &[Point]) -> Option<Rect> {
    let (first, rest) = points.split_first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for p in rest {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

/// Four times the tangent of a quarter of a right angle, divided by three:
/// the control-point distance, in radii, of a cubic Bézier curve that
/// follows a quarter circle (error under 0.03 % of the radius).
const KAPPA: f32 = 0.552_284_8;

pub(crate) fn push_rect(verbs: &mut Vec<Verb>, points: &mut Vec<Point>, r: Rect) {
    verbs.extend_from_slice(&[Verb::Move, Verb::Line, Verb::Line, Verb::Line, Verb::Close]);
    points.extend_from_slice(&[
        Point::new(r.x, r.y),
        Point::new(r.right(), r.y),
        Point::new(r.right(), r.bottom()),
        Point::new(r.x, r.bottom()),
    ]);
}

pub(crate) fn push_rounded_rect(verbs: &mut Vec<Verb>, points: &mut Vec<Point>, rr: RoundedRect) {
    let r = rr.rect;
    let radii = rr.fitted_radii();
    let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
    let k = KAPPA;
    let corner = |verbs: &mut Vec<Verb>, points: &mut Vec<Point>, c1, c2, to| {
        verbs.push(Verb::Cubic);
        points.extend_from_slice(&[c1, c2, to]);
    };
    verbs.push(Verb::Move);
    points.push(Point::new(x0 + radii.top_left, y0));
    verbs.push(Verb::Line);
    points.push(Point::new(x1 - radii.top_right, y0));
    let t = radii.top_right;
    if t > 0.0 {
        corner(
            verbs,
            points,
            Point::new(x1 - t + t * k, y0),
            Point::new(x1, y0 + t - t * k),
            Point::new(x1, y0 + t),
        );
    }
    verbs.push(Verb::Line);
    points.push(Point::new(x1, y1 - radii.bottom_right));
    let t = radii.bottom_right;
    if t > 0.0 {
        corner(
            verbs,
            points,
            Point::new(x1, y1 - t + t * k),
            Point::new(x1 - t + t * k, y1),
            Point::new(x1 - t, y1),
        );
    }
    verbs.push(Verb::Line);
    points.push(Point::new(x0 + radii.bottom_left, y1));
    let t = radii.bottom_left;
    if t > 0.0 {
        corner(
            verbs,
            points,
            Point::new(x0 + t - t * k, y1),
            Point::new(x0, y1 - t + t * k),
            Point::new(x0, y1 - t),
        );
    }
    verbs.push(Verb::Line);
    points.push(Point::new(x0, y0 + radii.top_left));
    let t = radii.top_left;
    if t > 0.0 {
        corner(
            verbs,
            points,
            Point::new(x0, y0 + t - t * k),
            Point::new(x0 + t - t * k, y0),
            Point::new(x0 + t, y0),
        );
    }
    verbs.push(Verb::Close);
}

pub(crate) fn push_circle(verbs: &mut Vec<Verb>, points: &mut Vec<Point>, c: Circle) {
    let (x, y, r) = (c.center.x, c.center.y, c.radius);
    let k = r * KAPPA;
    verbs.extend_from_slice(&[
        Verb::Move,
        Verb::Cubic,
        Verb::Cubic,
        Verb::Cubic,
        Verb::Cubic,
        Verb::Close,
    ]);
    points.extend_from_slice(&[
        Point::new(x + r, y),
        Point::new(x + r, y + k),
        Point::new(x + k, y + r),
        Point::new(x, y + r),
        Point::new(x - k, y + r),
        Point::new(x - r, y + k),
        Point::new(x - r, y),
        Point::new(x - r, y - k),
        Point::new(x - k, y - r),
        Point::new(x, y - r),
        Point::new(x + k, y - r),
        Point::new(x + r, y - k),
        Point::new(x + r, y),
    ]);
}

/// Appends an arc as at most four cubic curves, one per quarter turn or
/// less. Nothing is appended for a zero sweep, a radius that is not
/// positive, or non-finite values.
pub(crate) fn push_arc(verbs: &mut Vec<Verb>, points: &mut Vec<Point>, arc: Arc) {
    let tau = std::f64::consts::TAU;
    let radius = f64::from(arc.radius);
    let start = f64::from(arc.start);
    let sweep = f64::from(arc.sweep).clamp(-tau, tau);
    if !(radius > 0.0 && radius.is_finite() && start.is_finite() && sweep.is_finite())
        || sweep == 0.0
    {
        return;
    }
    // A quarter turn given in `f32` is a hair over the `f64` quarter turn;
    // the small allowance keeps it one piece.
    let pieces = (sweep.abs() / std::f64::consts::FRAC_PI_2 - 1e-5)
        .ceil()
        .max(1.0);
    let step = sweep / pieces;
    // Distance of the control points along the tangent, in radii.
    let (s, c) = sin_cos_f64(step / 4.0);
    let k = 4.0 / 3.0 * (s / c);
    let (cx, cy) = (f64::from(arc.center.x), f64::from(arc.center.y));
    let at = |sin: f64, cos: f64| ((cx + radius * cos) as f32, (cy + radius * sin) as f32);
    // The start is reduced exactly once; each piece's end is the start
    // turned by a whole number of steps, at most one turn, by the angle
    // sum formulas, so a start of any size gives the same arc.
    let (start_sin, start_cos) = sin_cos(arc.start);
    let (mut sin0, mut cos0) = (start_sin, start_cos);
    let (x, y) = at(sin0, cos0);
    verbs.push(Verb::Move);
    points.push(Point::new(x, y));
    for i in 1..=pieces as u32 {
        let (turn_sin, turn_cos) = sin_cos_f64(step * f64::from(i));
        let sin1 = start_sin * turn_cos + start_cos * turn_sin;
        let cos1 = start_cos * turn_cos - start_sin * turn_sin;
        // The tangent at angle a points along (-sin a, cos a).
        let c1 = (
            (cx + radius * (cos0 - k * sin0)) as f32,
            (cy + radius * (sin0 + k * cos0)) as f32,
        );
        let c2 = (
            (cx + radius * (cos1 + k * sin1)) as f32,
            (cy + radius * (sin1 - k * cos1)) as f32,
        );
        let end = at(sin1, cos1);
        verbs.push(Verb::Cubic);
        points.extend_from_slice(&[
            Point::new(c1.0, c1.1),
            Point::new(c2.0, c2.1),
            Point::new(end.0, end.1),
        ]);
        (sin0, cos0) = (sin1, cos1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_without_a_subpath_start_one_as_canvas_does() {
        let mut path = Path::new();
        path.line_to(Point::new(1.0, 1.0))
            .line_to(Point::new(2.0, 1.0));
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec![
                Segment::MoveTo(Point::new(1.0, 1.0)),
                Segment::LineTo(Point::new(2.0, 1.0)),
            ]
        );
    }

    #[test]
    fn drawing_after_close_continues_from_the_subpath_start() {
        let mut path = Path::new();
        path.move_to(Point::new(5.0, 5.0))
            .line_to(Point::new(6.0, 5.0))
            .close()
            .line_to(Point::new(5.0, 9.0));
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec![
                Segment::MoveTo(Point::new(5.0, 5.0)),
                Segment::LineTo(Point::new(6.0, 5.0)),
                Segment::Close,
                Segment::MoveTo(Point::new(5.0, 5.0)),
                Segment::LineTo(Point::new(5.0, 9.0)),
            ]
        );
    }

    #[test]
    fn repeated_moves_and_closes_collapse() {
        let mut path = Path::new();
        path.move_to(Point::new(1.0, 1.0))
            .move_to(Point::new(2.0, 2.0))
            .close()
            .close();
        assert_eq!(
            path.segments().collect::<Vec<_>>(),
            vec![Segment::MoveTo(Point::new(2.0, 2.0)), Segment::Close]
        );
    }

    #[test]
    fn a_curve_without_a_subpath_starts_at_its_first_control_point() {
        let mut path = Path::new();
        path.quad_to(Point::new(1.0, 0.0), Point::new(2.0, 2.0));
        assert_eq!(
            path.segments().next(),
            Some(Segment::MoveTo(Point::new(1.0, 0.0)))
        );
    }

    #[test]
    fn a_quarter_arc_is_one_cubic_on_the_circle() {
        let path = Path::arc(Arc::new(
            Point::new(0.0, 0.0),
            10.0,
            0.0,
            std::f32::consts::FRAC_PI_2,
        ));
        let segments: Vec<_> = path.segments().collect();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], Segment::MoveTo(Point::new(10.0, 0.0)));
        let Segment::CubicTo(c1, c2, end) = segments[1] else {
            panic!("expected a cubic, got {:?}", segments[1]);
        };
        // A quarter turn clockwise on screen ends straight below the centre.
        assert!(end.x.abs() < 1e-5 && end.y == 10.0, "{end:?}");
        // The control points sit at KAPPA radii along the tangents.
        assert_eq!(c1.x, 10.0);
        assert!((c1.y - 10.0 * KAPPA).abs() < 1e-5, "{c1:?}");
        assert!((c2.x - 10.0 * KAPPA).abs() < 1e-5, "{c2:?}");
        assert_eq!(c2.y, 10.0);
    }

    #[test]
    fn arcs_split_into_quarters_and_clamp_to_one_turn() {
        let count = |sweep: f32| {
            Path::arc(Arc::new(Point::ZERO, 1.0, 0.3, sweep))
                .segments()
                .count()
        };
        assert_eq!(count(0.1), 2);
        assert_eq!(count(std::f32::consts::PI), 3);
        assert_eq!(count(-std::f32::consts::PI * 1.5), 4);
        assert_eq!(count(100.0), 5);
        assert_eq!(count(0.0), 0);
        assert_eq!(count(f32::NAN), 0);
    }

    #[test]
    fn shapes_become_closed_clockwise_outlines() {
        let rect = Path::rect(Rect::new(1.0, 2.0, 3.0, 4.0));
        assert_eq!(
            rect.segments().collect::<Vec<_>>(),
            vec![
                Segment::MoveTo(Point::new(1.0, 2.0)),
                Segment::LineTo(Point::new(4.0, 2.0)),
                Segment::LineTo(Point::new(4.0, 6.0)),
                Segment::LineTo(Point::new(1.0, 6.0)),
                Segment::Close,
            ]
        );
        assert_eq!(
            Path::circle(Circle::new(Point::new(5.0, 5.0), 2.0)).bounds(),
            Some(Rect::new(3.0, 3.0, 4.0, 4.0))
        );
        let rounded = Path::rounded_rect(RoundedRect::new(Rect::new(0.0, 0.0, 10.0, 10.0), 2.0));
        assert_eq!(rounded.bounds(), Some(Rect::new(0.0, 0.0, 10.0, 10.0)));
        assert_eq!(rounded.segments().count(), 10);
        let square = Path::rounded_rect(RoundedRect::new(Rect::new(0.0, 0.0, 10.0, 10.0), 0.0));
        assert_eq!(square.segments().count(), 6);
    }
}
