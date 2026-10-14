//! Points, sizes, rectangles, shapes and affine transforms.
//!
//! Lengths are logical pixels in `f32`, with `y` growing downwards. Angles
//! are radians, measured from the positive `x` axis; a positive angle turns
//! clockwise on screen, because `y` points down.

use std::ops::{Add, Mul, Neg, Sub};

use crate::trig::sin_cos;

/// A point, or a vector, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Point {
    /// Horizontal position, growing to the right.
    pub x: f32,
    /// Vertical position, growing downwards.
    pub y: f32,
}

impl Point {
    /// The origin.
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    /// A point at `(x, y)`.
    pub const fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }

    /// Whether both coordinates are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, other: Point) -> Point {
        Point::new(self.x + other.x, self.y + other.y)
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, other: Point) -> Point {
        Point::new(self.x - other.x, self.y - other.y)
    }
}

impl Mul<f32> for Point {
    type Output = Point;
    fn mul(self, factor: f32) -> Point {
        Point::new(self.x * factor, self.y * factor)
    }
}

impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

/// A width and a height in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Size {
    /// Horizontal extent.
    pub width: f32,
    /// Vertical extent.
    pub height: f32,
}

impl Size {
    /// A size of `width` by `height`.
    pub const fn new(width: f32, height: f32) -> Self {
        Size { width, height }
    }
}

/// The next `f32` above `v` (`f32::next_up`, which needs Rust 1.86).
fn next_up(v: f32) -> f32 {
    if v.is_nan() || v == f32::INFINITY {
        return v;
    }
    if v == 0.0 {
        return f32::from_bits(1);
    }
    let bits = v.to_bits();
    f32::from_bits(if v > 0.0 { bits + 1 } else { bits - 1 })
}

/// The largest `f32` that is at most `v`.
fn round_down(v: f64) -> f32 {
    let f = v as f32;
    if f64::from(f) > v { -next_up(-f) } else { f }
}

/// The smallest `f32` that is at least `v`.
fn round_up(v: f64) -> f32 {
    let f = v as f32;
    if f64::from(f) < v { next_up(f) } else { f }
}

/// `to - from`, computed in `f64` and kept within the finite range of
/// `f32`, so that the extent of two finite edges is finite.
fn span(from: f32, to: f32) -> f32 {
    (f64::from(to) - f64::from(from)).clamp(f64::from(f32::MIN), f64::from(f32::MAX)) as f32
}

/// An axis-aligned rectangle: its top-left corner and its size.
///
/// A rectangle whose width or height is zero, negative or not a number is
/// empty: it contains no point and drawing it draws nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: f32,
    /// Top edge.
    pub y: f32,
    /// Horizontal extent.
    pub width: f32,
    /// Vertical extent.
    pub height: f32,
}

impl Rect {
    /// A rectangle with its top-left corner at `(x, y)`.
    pub const fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    /// The smallest rectangle containing both points, in either order.
    pub fn from_points(a: Point, b: Point) -> Self {
        let x = a.x.min(b.x);
        let y = a.y.min(b.y);
        Rect::new(x, y, span(x, a.x.max(b.x)), span(y, a.y.max(b.y)))
    }

    /// The smallest rectangle of `f32` numbers that holds the box from
    /// `(x0, y0)` to `(x1, y1)`, given in `f64`: its left and top edges are
    /// rounded down, and its width and height chosen so that
    /// [`right`](Rect::right) and [`bottom`](Rect::bottom), computed in
    /// `f32`, are at least `x1` and `y1`. Rounding to the nearest `f32`
    /// instead can move an edge inwards by half a unit of the last place,
    /// which far from the origin is more than a pixel.
    pub(crate) fn enclosing(x0: f64, y0: f64, x1: f64, y1: f64) -> Rect {
        let (x, y) = (round_down(x0), round_down(y0));
        let (right, bottom) = (round_up(x1), round_up(y1));
        // The width rounded up makes `x + width` at least `right` before
        // rounding, and rounding to nearest cannot then fall below
        // `right`, itself an `f32`.
        Rect::new(
            x,
            y,
            round_up(f64::from(right) - f64::from(x)),
            round_up(f64::from(bottom) - f64::from(y)),
        )
    }

    /// A rectangle from its top-left corner and its size.
    pub const fn from_origin_size(origin: Point, size: Size) -> Self {
        Rect::new(origin.x, origin.y, size.width, size.height)
    }

    /// The right edge, `x + width`.
    pub fn right(&self) -> f32 {
        self.x + self.width
    }

    /// The bottom edge, `y + height`.
    pub fn bottom(&self) -> f32 {
        self.y + self.height
    }

    /// The top-left corner.
    pub fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    /// The width and height.
    pub fn size(&self) -> Size {
        Size::new(self.width, self.height)
    }

    /// The centre.
    pub fn center(&self) -> Point {
        Point::new(self.x + self.width * 0.5, self.y + self.height * 0.5)
    }

    /// Whether the rectangle has no area (including when a side is `NaN`).
    pub fn is_empty(&self) -> bool {
        !(self.width > 0.0 && self.height > 0.0)
    }

    /// Whether every field is finite.
    pub fn is_finite(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
    }

    /// Whether `point` is inside: left and top edges included, right and
    /// bottom edges excluded, so adjacent rectangles share no point.
    pub fn contains(&self, point: Point) -> bool {
        point.x >= self.x && point.x < self.right() && point.y >= self.y && point.y < self.bottom()
    }

    /// The overlap of two rectangles, or `None` if they do not overlap.
    pub fn intersect(&self, other: &Rect) -> Option<Rect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        let overlap = Rect::new(x0, y0, span(x0, x1), span(y0, y1));
        (!overlap.is_empty()).then_some(overlap)
    }

    /// The smallest rectangle containing both. An empty rectangle adds
    /// nothing, so the union of an empty one and `r` is `r`. The union of
    /// finite rectangles is finite: a size beyond the range of `f32` is
    /// held at its largest value.
    pub fn union(&self, other: &Rect) -> Rect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        Rect::new(x0, y0, span(x0, x1), span(y0, y1))
    }

    /// The rectangle grown by `by` on every side (shrunk if negative).
    pub fn outset(&self, by: f32) -> Rect {
        Rect::new(
            self.x - by,
            self.y - by,
            span(-by, self.width + by),
            span(-by, self.height + by),
        )
    }

    /// The rectangle moved by `by`.
    pub fn translate(&self, by: Point) -> Rect {
        Rect::new(self.x + by.x, self.y + by.y, self.width, self.height)
    }

    /// The smallest rectangle of whole pixels that covers this one once it
    /// is multiplied by `scale`: edges are rounded outwards. Coordinates
    /// beyond the range of `i32` are clamped. Empty or non-finite
    /// rectangles give an empty [`IRect`] at the origin.
    pub fn to_device(&self, scale: f32) -> IRect {
        if self.is_empty() || !self.is_finite() || !(scale.is_finite() && scale > 0.0) {
            return IRect::default();
        }
        let clamp = |v: f32| f64::from(v).clamp(f64::from(i32::MIN), f64::from(i32::MAX));
        let x0 = clamp((self.x * scale).floor());
        let y0 = clamp((self.y * scale).floor());
        let x1 = clamp((self.right() * scale).ceil());
        let y1 = clamp((self.bottom() * scale).ceil());
        IRect::from_edges(x0 as i32, y0 as i32, x1 as i32, y1 as i32)
    }
}

/// A rectangle of whole device pixels.
///
/// It covers the pixels `x .. x + width` and `y .. y + height`. Damage
/// rectangles, pixmap bounds and crops use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct IRect {
    /// Left edge, the first column covered.
    pub x: i32,
    /// Top edge, the first row covered.
    pub y: i32,
    /// Number of columns covered.
    pub width: u32,
    /// Number of rows covered.
    pub height: u32,
}

impl IRect {
    /// A rectangle with its top-left pixel at `(x, y)`.
    pub const fn new(x: i32, y: i32, width: u32, height: u32) -> Self {
        IRect {
            x,
            y,
            width,
            height,
        }
    }

    /// A rectangle from its edges; inverted edges give an empty rectangle
    /// at `(x0, y0)`.
    pub fn from_edges(x0: i32, y0: i32, x1: i32, y1: i32) -> Self {
        let width = (i64::from(x1) - i64::from(x0)).max(0) as u32;
        let height = (i64::from(y1) - i64::from(y0)).max(0) as u32;
        IRect::new(x0, y0, width, height)
    }

    /// One past the last column, as `i64` so that it cannot overflow.
    pub fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.width)
    }

    /// One past the last row, as `i64` so that it cannot overflow.
    pub fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.height)
    }

    /// Whether it covers no pixel.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The number of pixels covered.
    pub fn area(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height)
    }

    /// Whether the pixel at `(x, y)` is covered.
    pub fn contains(&self, x: i64, y: i64) -> bool {
        x >= i64::from(self.x) && x < self.right() && y >= i64::from(self.y) && y < self.bottom()
    }

    /// Whether `other` lies entirely inside this rectangle. An empty
    /// rectangle lies inside every rectangle.
    pub fn contains_rect(&self, other: &IRect) -> bool {
        other.is_empty()
            || (other.x >= self.x
                && other.y >= self.y
                && other.right() <= self.right()
                && other.bottom() <= self.bottom())
    }

    /// The pixels both cover, or `None` if they share none.
    pub fn intersect(&self, other: &IRect) -> Option<IRect> {
        let x0 = self.x.max(other.x);
        let y0 = self.y.max(other.y);
        let x1 = self.right().min(other.right());
        let y1 = self.bottom().min(other.bottom());
        if x1 <= i64::from(x0) || y1 <= i64::from(y0) {
            return None;
        }
        Some(IRect::new(
            x0,
            y0,
            (x1 - i64::from(x0)) as u32,
            (y1 - i64::from(y0)) as u32,
        ))
    }

    /// The smallest rectangle covering both. An empty rectangle adds
    /// nothing.
    pub fn union(&self, other: &IRect) -> IRect {
        if self.is_empty() {
            return *other;
        }
        if other.is_empty() {
            return *self;
        }
        let x0 = self.x.min(other.x);
        let y0 = self.y.min(other.y);
        let x1 = self.right().max(other.right());
        let y1 = self.bottom().max(other.bottom());
        IRect::new(
            x0,
            y0,
            (x1 - i64::from(x0)).min(i64::from(u32::MAX)) as u32,
            (y1 - i64::from(y0)).min(i64::from(u32::MAX)) as u32,
        )
    }

    /// The same pixels as a [`Rect`].
    pub fn to_rect(&self) -> Rect {
        Rect::new(
            self.x as f32,
            self.y as f32,
            self.width as f32,
            self.height as f32,
        )
    }
}

/// The corner radii of a [`RoundedRect`], in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Radii {
    /// Radius of the top-left corner.
    pub top_left: f32,
    /// Radius of the top-right corner.
    pub top_right: f32,
    /// Radius of the bottom-right corner.
    pub bottom_right: f32,
    /// Radius of the bottom-left corner.
    pub bottom_left: f32,
}

impl Radii {
    /// The same radius at every corner.
    pub const fn all(radius: f32) -> Self {
        Radii {
            top_left: radius,
            top_right: radius,
            bottom_right: radius,
            bottom_left: radius,
        }
    }
}

/// A rectangle with circular corners.
///
/// Radii that do not fit are scaled down together, as CSS does: if the two
/// radii along a side add up to more than that side, every radius is
/// multiplied by the same factor so that they fit. Negative or non-finite
/// radii count as zero.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct RoundedRect {
    /// The rectangle the corners are cut from.
    pub rect: Rect,
    /// The corner radii.
    pub radii: Radii,
}

impl RoundedRect {
    /// A rectangle with the same radius at every corner.
    pub const fn new(rect: Rect, radius: f32) -> Self {
        RoundedRect {
            rect,
            radii: Radii::all(radius),
        }
    }

    /// A rectangle with a radius per corner.
    pub const fn with_radii(rect: Rect, radii: Radii) -> Self {
        RoundedRect { rect, radii }
    }

    /// The radii actually drawn: non-negative and scaled down to fit.
    pub fn fitted_radii(&self) -> Radii {
        let clean = |r: f32| if r.is_finite() && r > 0.0 { r } else { 0.0 };
        let r = Radii {
            top_left: clean(self.radii.top_left),
            top_right: clean(self.radii.top_right),
            bottom_right: clean(self.radii.bottom_right),
            bottom_left: clean(self.radii.bottom_left),
        };
        let width = self.rect.width.max(0.0);
        let height = self.rect.height.max(0.0);
        let mut factor = 1.0f32;
        for (side, sum) in [
            (width, r.top_left + r.top_right),
            (width, r.bottom_left + r.bottom_right),
            (height, r.top_left + r.bottom_left),
            (height, r.top_right + r.bottom_right),
        ] {
            if sum > side {
                factor = factor.min(side / sum);
            }
        }
        if factor < 1.0 {
            Radii {
                top_left: r.top_left * factor,
                top_right: r.top_right * factor,
                bottom_right: r.bottom_right * factor,
                bottom_left: r.bottom_left * factor,
            }
        } else {
            r
        }
    }
}

/// A circle.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Circle {
    /// The centre.
    pub center: Point,
    /// The radius; zero, negative or `NaN` draws nothing.
    pub radius: f32,
}

impl Circle {
    /// A circle around `center`.
    pub const fn new(center: Point, radius: f32) -> Self {
        Circle { center, radius }
    }
}

/// A circular arc: part of a circle's outline.
///
/// It starts at the angle `start` and runs through `sweep` radians;
/// a positive sweep turns clockwise on screen. A sweep larger than a full
/// turn is drawn as one full turn. Stroking an arc draws the curve;
/// filling it fills the region between the curve and its chord, as filling
/// any open path does. A pie slice is a [`Path`](crate::Path).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Arc {
    /// The centre of the circle.
    pub center: Point,
    /// The radius of the circle.
    pub radius: f32,
    /// Where the arc starts, in radians from the positive `x` axis.
    pub start: f32,
    /// How far it runs, in radians.
    pub sweep: f32,
}

impl Arc {
    /// An arc of the circle around `center`.
    pub const fn new(center: Point, radius: f32, start: f32, sweep: f32) -> Self {
        Arc {
            center,
            radius,
            start,
            sweep,
        }
    }

    /// The point at `angle` on the arc's circle.
    pub fn point_at(&self, angle: f32) -> Point {
        let (s, c) = sin_cos(angle);
        Point::new(
            self.center.x + (f64::from(self.radius) * c) as f32,
            self.center.y + (f64::from(self.radius) * s) as f32,
        )
    }
}

/// An affine transform: `x' = a·x + c·y + e`, `y' = b·x + d·y + f`.
///
/// The fields follow the order of SVG's `matrix(a b c d e f)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Transform {
    /// How `x` scales `x'`.
    pub a: f32,
    /// How `x` moves `y'`.
    pub b: f32,
    /// How `y` moves `x'`.
    pub c: f32,
    /// How `y` scales `y'`.
    pub d: f32,
    /// Horizontal translation.
    pub e: f32,
    /// Vertical translation.
    pub f: f32,
}

impl Default for Transform {
    fn default() -> Self {
        Transform::IDENTITY
    }
}

impl Transform {
    /// The transform that changes nothing.
    pub const IDENTITY: Transform = Transform {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    /// A transform from its six coefficients.
    pub const fn new(a: f32, b: f32, c: f32, d: f32, e: f32, f: f32) -> Self {
        Transform { a, b, c, d, e, f }
    }

    /// A move by `(dx, dy)`.
    pub const fn translate(dx: f32, dy: f32) -> Self {
        Transform::new(1.0, 0.0, 0.0, 1.0, dx, dy)
    }

    /// A scale by `sx` horizontally and `sy` vertically, about the origin.
    pub const fn scale(sx: f32, sy: f32) -> Self {
        Transform::new(sx, 0.0, 0.0, sy, 0.0, 0.0)
    }

    /// A rotation by `angle` radians about the origin, clockwise on screen.
    ///
    /// The sine and cosine are computed the same way on every platform, so
    /// the coefficients, and therefore a scene's dump, are identical
    /// everywhere. The angle is reduced to a quarter turn exactly, so any
    /// finite angle, however large, gives a rotation; an angle that is not
    /// finite gives `NaN` coefficients, and a scene drawn under them draws
    /// nothing.
    pub fn rotate(angle: f32) -> Self {
        let (s, c) = sin_cos(angle);
        let (s, c) = (s as f32, c as f32);
        Transform::new(c, s, -s, c, 0.0, 0.0)
    }

    /// A rotation by `angle` radians about `center`.
    pub fn rotate_about(angle: f32, center: Point) -> Self {
        Transform::translate(-center.x, -center.y)
            .then(Transform::rotate(angle))
            .then(Transform::translate(center.x, center.y))
    }

    /// This transform followed by `next`: mapping a point through the
    /// result equals mapping it through `self`, then through `next`.
    pub fn then(self, next: Transform) -> Transform {
        let n = next;
        let s = self;
        Transform {
            a: n.a * s.a + n.c * s.b,
            b: n.b * s.a + n.d * s.b,
            c: n.a * s.c + n.c * s.d,
            d: n.b * s.c + n.d * s.d,
            e: n.a * s.e + n.c * s.f + n.e,
            f: n.b * s.e + n.d * s.f + n.f,
        }
    }

    /// The transform that undoes this one, or `None` if it collapses the
    /// plane onto a line or a point, or is not finite.
    pub fn invert(&self) -> Option<Transform> {
        let det = f64::from(self.a) * f64::from(self.d) - f64::from(self.b) * f64::from(self.c);
        if det == 0.0 || !det.is_finite() || !self.is_finite() {
            return None;
        }
        let (a, b, c, d, e, f) = (
            f64::from(self.a),
            f64::from(self.b),
            f64::from(self.c),
            f64::from(self.d),
            f64::from(self.e),
            f64::from(self.f),
        );
        let inverse = Transform {
            a: (d / det) as f32,
            b: (-b / det) as f32,
            c: (-c / det) as f32,
            d: (a / det) as f32,
            e: ((c * f - d * e) / det) as f32,
            f: ((b * e - a * f) / det) as f32,
        };
        inverse.is_finite().then_some(inverse)
    }

    /// Where `point` goes.
    pub fn map_point(&self, point: Point) -> Point {
        Point::new(
            self.a * point.x + self.c * point.y + self.e,
            self.b * point.x + self.d * point.y + self.f,
        )
    }

    /// The smallest axis-aligned rectangle containing the transformed
    /// corners of `rect`.
    pub fn map_rect(&self, rect: &Rect) -> Rect {
        let corners = [
            self.map_point(Point::new(rect.x, rect.y)),
            self.map_point(Point::new(rect.right(), rect.y)),
            self.map_point(Point::new(rect.x, rect.bottom())),
            self.map_point(Point::new(rect.right(), rect.bottom())),
        ];
        let mut x0 = corners[0].x;
        let mut y0 = corners[0].y;
        let mut x1 = x0;
        let mut y1 = y0;
        for corner in &corners[1..] {
            x0 = x0.min(corner.x);
            y0 = y0.min(corner.y);
            x1 = x1.max(corner.x);
            y1 = y1.max(corner.y);
        }
        Rect::new(x0, y0, span(x0, x1), span(y0, y1))
    }

    /// Whether it keeps horizontal lines horizontal and vertical lines
    /// vertical: no rotation other than by half turns, and no skew.
    pub fn is_axis_aligned(&self) -> bool {
        self.b == 0.0 && self.c == 0.0
    }

    /// Whether every coefficient is finite.
    pub fn is_finite(&self) -> bool {
        self.a.is_finite()
            && self.b.is_finite()
            && self.c.is_finite()
            && self.d.is_finite()
            && self.e.is_finite()
            && self.f.is_finite()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enclosing_rectangle_never_rounds_inwards() {
        // Exact values stay exact.
        assert_eq!(
            Rect::enclosing(1.0, 2.0, 4.0, 8.0),
            Rect::new(1.0, 2.0, 3.0, 6.0)
        );
        // Near 10^7 an f32 has no fraction: 10000003.6 rounds to nearest
        // up, but an enclosing edge goes down, and the far edge up.
        let r = Rect::enclosing(10_000_003.6, -0.1, 10_000_083.4, 0.1);
        assert_eq!((r.x, r.right()), (10_000_003.0, 10_000_084.0));
        assert!(f64::from(r.y) <= -0.1 && f64::from(r.bottom()) >= 0.1);
        // Edges of different signs and sizes, and the ends of the range.
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        for _ in 0..10_000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let a = (seed >> 11) as f64 / (1u64 << 53) as f64;
            let scale = [1.0, 1e3, 1e7, 3e9][(seed % 4) as usize];
            let x0 = (a - 0.6) * scale;
            let x1 = x0 + a * scale * 0.01 + 0.3;
            let r = Rect::enclosing(x0, x0, x1, x1);
            assert!(
                f64::from(r.x) <= x0 && f64::from(r.right()) >= x1,
                "{x0} {x1}: {r:?}"
            );
            assert!(
                f64::from(r.y) <= x0 && f64::from(r.bottom()) >= x1,
                "{x0} {x1}: {r:?}"
            );
        }
        // Beyond the range of f32 the box is not finite, and the scene's
        // bounds leave out what is not finite.
        assert!(!Rect::enclosing(-1e300, 0.0, 1e300, 1.0).is_finite());
        assert_eq!(Rect::enclosing(0.0, 0.0, 1e39, 1.0).right(), f32::INFINITY);
        assert_eq!(next_up(-0.0), f32::from_bits(1));
        assert_eq!(next_up(f32::MAX), f32::INFINITY);
        assert_eq!(next_up(-f32::INFINITY), f32::MIN);
    }

    #[test]
    fn rectangles_intersect_and_unite_exactly() {
        let a = Rect::new(0.0, 0.0, 10.0, 10.0);
        let b = Rect::new(5.0, 2.5, 10.0, 5.0);
        assert_eq!(a.intersect(&b), Some(Rect::new(5.0, 2.5, 5.0, 5.0)));
        assert_eq!(a.union(&b), Rect::new(0.0, 0.0, 15.0, 10.0));
        assert_eq!(a.intersect(&Rect::new(10.0, 0.0, 1.0, 1.0)), None);
        assert_eq!(Rect::default().union(&b), b);
        assert!(Rect::new(0.0, 0.0, f32::NAN, 1.0).is_empty());
        assert!(a.contains(Point::new(0.0, 0.0)));
        assert!(!a.contains(Point::new(10.0, 5.0)));
    }

    #[test]
    fn extents_of_finite_rectangles_stay_finite() {
        let a = Rect::new(-3e38, -3e38, 1.0, 1.0);
        let b = Rect::new(3e38, 3e38, 1.0, 1.0);
        let u = a.union(&b);
        assert!(u.is_finite(), "{u:?}");
        assert_eq!(u.width, f32::MAX);
        assert!(Rect::from_points(a.origin(), b.origin()).is_finite());
        assert!(Rect::new(0.0, 0.0, 1.0, 1.0).outset(f32::MAX).is_finite());
        assert_eq!(
            Rect::new(0.0, 0.0, 4.0, 2.0).outset(1.0),
            Rect::new(-1.0, -1.0, 6.0, 4.0)
        );
    }

    #[test]
    fn device_rectangles_round_outwards() {
        let r = Rect::new(0.25, 1.5, 2.5, 1.0);
        assert_eq!(r.to_device(1.0), IRect::new(0, 1, 3, 2));
        assert_eq!(r.to_device(2.0), IRect::new(0, 3, 6, 2));
        assert_eq!(
            Rect::new(-1e30, 0.0, 2e30, 1.0).to_device(1.0),
            IRect::new(i32::MIN, 0, u32::MAX, 1)
        );
        assert_eq!(
            Rect::new(0.0, 0.0, 1.0, 1.0).to_device(f32::NAN),
            IRect::default()
        );
    }

    #[test]
    fn integer_rectangles_never_overflow() {
        let far = IRect::new(i32::MAX - 1, 0, u32::MAX, 1);
        assert_eq!(far.right(), i64::from(i32::MAX) - 1 + i64::from(u32::MAX));
        let near = IRect::new(i32::MIN, 0, 10, 10);
        assert_eq!(far.union(&near).width, u32::MAX);
        assert_eq!(far.intersect(&near), None);
        assert_eq!(
            IRect::new(0, 0, 4, 4).intersect(&IRect::new(2, 2, 4, 4)),
            Some(IRect::new(2, 2, 2, 2))
        );
        assert_eq!(IRect::from_edges(5, 5, 1, 1), IRect::new(5, 5, 0, 0));
    }

    #[test]
    fn transforms_compose_in_the_order_they_are_written() {
        let move_then_scale = Transform::translate(1.0, 2.0).then(Transform::scale(10.0, 100.0));
        assert_eq!(
            move_then_scale.map_point(Point::new(1.0, 1.0)),
            Point::new(20.0, 300.0)
        );
        let scale_then_move = Transform::scale(10.0, 100.0).then(Transform::translate(1.0, 2.0));
        assert_eq!(
            scale_then_move.map_point(Point::new(1.0, 1.0)),
            Point::new(11.0, 102.0)
        );
        assert_eq!(
            move_then_scale,
            Transform::new(10.0, 0.0, 0.0, 100.0, 10.0, 200.0)
        );
    }

    #[test]
    fn a_quarter_turn_is_exact_and_clockwise_on_screen() {
        let turn = Transform::rotate(std::f32::consts::FRAC_PI_2);
        // The x axis goes to the y axis, which points down: clockwise.
        let p = turn.map_point(Point::new(1.0, 0.0));
        assert_eq!(p.y, 1.0);
        assert!(p.x.abs() < 1e-7);
        let about = Transform::rotate_about(std::f32::consts::PI, Point::new(5.0, 5.0));
        let q = about.map_point(Point::new(6.0, 5.0));
        assert!((q.x - 4.0).abs() < 1e-5 && (q.y - 5.0).abs() < 1e-5);
    }

    #[test]
    fn a_rotation_by_any_finite_angle_is_a_rotation() {
        // Reduced with one f64 multiple of a quarter turn, an angle of 1e18
        // once gave a cosine of 2.4e20: a "rotation" that scaled the scene.
        // The reduction is exact now, so every finite angle gives sine and
        // cosine on the unit circle, and 1e18 the true ones.
        let t = Transform::rotate(1e18);
        assert_eq!((t.a, t.b), (0.976_229_85, -0.216_737_88));
        for angle in [1e7, 16_777_217.0, 1e16, 1e17, -3e20, 1e30, f32::MAX] {
            let t = Transform::rotate(angle);
            assert_eq!((t.a, t.b), (t.d, -t.c), "{angle:e}");
            let length = f64::from(t.a).hypot(f64::from(t.b));
            assert!((length - 1.0).abs() < 1e-7, "{angle:e}: {t:?}");
        }
        let nan = Transform::rotate(f32::INFINITY);
        assert!(nan.a.is_nan() && nan.b.is_nan());
    }

    #[test]
    fn inverting_undoes_and_degenerate_transforms_have_no_inverse() {
        let t = Transform::new(2.0, 0.0, 0.0, 4.0, 3.0, -8.0);
        let inverse = t.invert().expect("invertible");
        assert_eq!(inverse, Transform::new(0.5, 0.0, 0.0, 0.25, -1.5, 2.0));
        assert_eq!(t.then(inverse), Transform::IDENTITY);
        assert_eq!(Transform::scale(0.0, 1.0).invert(), None);
        assert_eq!(Transform::translate(f32::NAN, 0.0).invert(), None);
    }

    #[test]
    fn mapped_rectangles_bound_the_corners() {
        let r = Rect::new(0.0, 0.0, 2.0, 1.0);
        assert_eq!(
            Transform::scale(-1.0, 2.0).map_rect(&r),
            Rect::new(-2.0, 0.0, 2.0, 2.0)
        );
    }

    #[test]
    fn radii_that_do_not_fit_shrink_together() {
        let rr = RoundedRect::with_radii(
            Rect::new(0.0, 0.0, 10.0, 40.0),
            Radii {
                top_left: 10.0,
                top_right: 10.0,
                bottom_right: -3.0,
                bottom_left: f32::NAN,
            },
        );
        assert_eq!(
            rr.fitted_radii(),
            Radii {
                top_left: 5.0,
                top_right: 5.0,
                bottom_right: 0.0,
                bottom_left: 0.0,
            }
        );
        assert_eq!(
            RoundedRect::new(Rect::new(0.0, 0.0, 10.0, 10.0), 3.0).fitted_radii(),
            Radii::all(3.0)
        );
    }
}
