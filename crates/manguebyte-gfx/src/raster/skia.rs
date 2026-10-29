//! The `tiny-skia` rasteriser, and the PNG codec that comes with it.
//!
//! This is the only module that names `tiny_skia`.
//!
//! # Partial renders equal whole ones
//!
//! Every render draws in the target's own pixels: the same coordinates,
//! the same shader transforms and the same clipping at the target's edges.
//! A whole render draws straight into the target. A partial render copies
//! the damaged pixels into a scratch copy of the target, draws there, and
//! copies them back, so no other pixel of the target changes.
//!
//! What keeps a partial render cheap is its window: the damage rounded out
//! to a grid of [`GRID`] pixels. A command whose bounds cannot reach the
//! damage is skipped, and every outline is clamped to the window before
//! `tiny-skia` sees it, so the rasteriser walks only the window's pixels.
//!
//! Clamping leaves every pixel inside the window exactly as a whole render
//! draws it because, in every render, whole or partial, each straight edge
//! of an outline is first cut where it crosses a grid line ([`cut`]). An
//! edge inside the window is then the same edge, with the same end points,
//! in both renders. An edge outside it lies in one grid cell, so it is
//! either above or below the window, where clamping flattens it onto the
//! window's border and it covers nothing, or beside it, where clamping
//! makes it a vertical edge on the border that winds the same way over the
//! same rows, which is all a pixel inside needs from it. Rectangles, which
//! `tiny-skia` fills with a routine of its own, are clamped to the window
//! grown by a pixel instead: a clamped edge then lies on a pixel boundary
//! outside the window, and every pixel inside keeps its coverage. Gradient
//! strips for [`Scene::columns`] are computed at a column or row that does
//! not depend on the window.
//!
//! Text drawn from cached glyph masks needs no such care: a mask depends
//! only on the glyph, its size, its point of the variation space and where
//! its origin falls within a pixel, never on the window. A run's masks are
//! gathered into one coverage per pixel, in the run's order, from every
//! glyph that can touch the pixel, and blended by [`blend_solid`] pixel by
//! pixel; none of that depends on the window either.
//!
//! The argument holds for targets of at most 8191 pixels a side. Larger
//! targets are drawn by `tiny-skia` in tiles of 8191 pixels, which are not
//! on the grid, and near those tiles' edges a partial render is not
//! guaranteed to match a whole one; no test covers such targets.

use tiny_skia as sk;

use super::{Caches, Raster};
use crate::color::{Color, mul_div_255};
use crate::geom::{Circle, IRect, Point, Radii, Rect, RoundedRect, Transform};
use crate::image::Sampling;
use crate::paint::{Cap, Extend, Join, Stop, Stroke};
use crate::path::{FillRule, Verb};
use crate::pixmap::{ImageError, MAX_SIDE, Pixmap};
use crate::scene::{
    Command, MAX_DEPTH, Scene, StoredGlyph, StoredPaint, StoredShape, TEXT_TOLERANCE,
    shadow_bounds, shape_bounds,
};
use crate::shadow::{Shadow, ShadowCache, ShadowMask, blur, box_reach, box_size};
use crate::text::font::{Coords, Face};
use crate::text::{GlyphCache, GlyphKey, Mask};

/// The grid, in device pixels, that a partial render rounds its damage out
/// to, and at whose lines every outline is cut.
const GRID: u32 = 64;

/// How far, in device pixels, beyond a command's conservative bounds it may
/// still change a pixel: anti-aliasing reaches the pixels an outline
/// crosses, and bounds are mapped with other roundings than points. A
/// command farther than this from a pass's damage is skipped.
const GUARD: f32 = 2.0;

/// Strokes are computed at this many times the device scale, and at least
/// at this scale. `tiny-skia` follows curved outlines to a quarter of a
/// unit over this resolution.
const STROKE_OVERSAMPLE: f32 = 4.0;

/// The largest resolution at which a stroke's outline is computed. A
/// stroke drawn larger than this is still drawn, with its curves followed
/// less closely than a pixel.
const MAX_STROKE_RESOLUTION: f32 = 1024.0;

/// The largest device coordinate handed to the rasteriser uncut. Its edges
/// are fixed point with 16 fractional bits after 4x supersampling, so
/// coordinates must stay well within 2^13 times the largest pixmap; beyond
/// this, [`clip_contours`] cuts the shape to the target first.
const SAFE: f32 = 1_048_576.0;

/// Below this distance between a linear gradient's ends, or this radius,
/// a gradient has no direction and is drawn as one colour. `tiny-skia`
/// makes the same decision at half this distance, in `f32`, and its
/// average colour can fail on rounding; deciding first means it never has
/// to.
const DEGENERATE: f64 = 1.0 / 16_384.0;

/// Glyphs are drawn from cached masks only under a transform that scales
/// both ways by the same factor, at least this one, with no rotation or
/// skew. Masks place glyphs on a quarter of a device pixel, an eighth of a
/// pixel from where they belong at most, which at this scale is a quarter
/// of a unit of the text's coordinates: within the tolerance
/// `Scene::bounds` adds for text. Below it, and under any other transform,
/// glyph outlines are filled as paths, exactly where they belong.
const MIN_MASK_SCALE: f32 = 0.5;

/// Glyphs larger than this, in device pixels per em, are filled as paths:
/// their masks would be large, and few of them fit a screen anyway.
const MAX_MASK_PPEM: f64 = 256.0;

/// How far a glyph can reach from its origin, in ems, any way. The
/// farthest any glyph of the embedded typeface reaches, at the corners of
/// its variation space, is 2.58 em (the long arrow, to the right; measured
/// with fontTools). Glyphs farther than this from the pixels a pass draws
/// are skipped before their masks are looked up; a whole render skips
/// only glyphs that cannot touch the target, so this must hold for a
/// partial render to equal it.
const GLYPH_REACH: f64 = 4.0;

/// What a `Pop*` command undoes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Clip,
    Transform,
    Layer,
}

#[derive(Debug, Clone, Copy)]
enum Frame {
    /// A transform; the previous one is on the transform stack.
    Transform,
    /// A clip with a mask of its own.
    Clip,
    /// A layer: buffered in a surface of its own, or drawn directly.
    Layer { opacity: f32, buffered: bool },
    /// A clip or layer whose contents are not drawn.
    Skip(Kind),
}

impl Frame {
    fn kind(self) -> Kind {
        match self {
            Frame::Transform => Kind::Transform,
            Frame::Clip => Kind::Clip,
            Frame::Layer { .. } => Kind::Layer,
            Frame::Skip(kind) => kind,
        }
    }
}

/// The part of the target one pass of a partial render works on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    /// The pixels the pass rasterises: whole cells of the grid. It may
    /// reach past the target's right and bottom edges.
    area: IRect,
    /// The bounding box of the damage the pass must get right; a command
    /// that cannot reach it is skipped.
    cull: IRect,
}

/// The `tiny-skia` implementation of [`Raster`].
pub(crate) struct Skia {
    /// The buffers of buffered layers, the size of the target: entry `i`
    /// holds layer level `i + 1` (level 0 is the target itself).
    layers: Vec<Vec<u8>>,
    /// The copy of the target a partial render draws in.
    scratch: Vec<u8>,
    /// Clip masks, one per clip level, the size of the target, and for
    /// each the pixels it may hold coverage in: zero everywhere else, so
    /// that pushing a clip clears only what the last clip at that level
    /// covered, not the whole target.
    masks: Vec<sk::Mask>,
    mask_dirty: Vec<Option<IRect>>,
    /// Coverage for [`Scene::columns`], kept all zero between uses.
    columns: Option<sk::Mask>,
    /// The target size the masks are made for.
    size: (u32, u32),
    /// The walk's stacks, kept so that they are not allocated again.
    transforms: Vec<Transform>,
    frames: Vec<Frame>,
    /// Outlines are built here before they become `tiny_skia` paths: as
    /// given, flattened, then cut on the grid.
    verbs: Vec<Verb>,
    points: Vec<Point>,
    flat_verbs: Vec<Verb>,
    flat_points: Vec<Point>,
    cut_verbs: Vec<Verb>,
    cut_points: Vec<Point>,
    /// The colours of a gradient along one row or one column, for columns.
    strip: Vec<u8>,
    /// The passes of a partial render, `None` once merged into another,
    /// and the pass each damage rectangle belongs to.
    passes: Vec<Option<Window>>,
    owners: Vec<usize>,
    /// The embedded typeface, for glyph outlines.
    face: Face,
    /// The coverage of a run of glyphs drawn from masks, over its box.
    coverage: Vec<u8>,
    /// What identifies a shadow's mask in the cache, and the blur's
    /// scratch buffers.
    shadow_key: Vec<u32>,
    blur_work: Vec<u16>,
    blur_line: Vec<u16>,
    /// Drawing commands not skipped, counted for tests.
    #[cfg(test)]
    drawn: usize,
    /// For tests: panic when `drawn` reaches this.
    #[cfg(test)]
    fault: Option<usize>,
}

impl Skia {
    pub(crate) fn new() -> Self {
        Skia {
            layers: Vec::new(),
            scratch: Vec::new(),
            masks: Vec::new(),
            mask_dirty: Vec::new(),
            columns: None,
            size: (0, 0),
            transforms: Vec::new(),
            frames: Vec::new(),
            verbs: Vec::new(),
            points: Vec::new(),
            flat_verbs: Vec::new(),
            flat_points: Vec::new(),
            cut_verbs: Vec::new(),
            cut_points: Vec::new(),
            strip: Vec::new(),
            passes: Vec::new(),
            owners: Vec::new(),
            face: Face::embedded(),
            coverage: Vec::new(),
            shadow_key: Vec::new(),
            blur_work: Vec::new(),
            blur_line: Vec::new(),
            #[cfg(test)]
            drawn: 0,
            #[cfg(test)]
            fault: None,
        }
    }
}

/// Clips every contour of a flattened path (moves, lines and closes) to
/// `window` (left, top, right, bottom), closing each one, with the
/// Sutherland-Hodgman algorithm in `f64`. For a point inside the window the
/// winding number, and so the fill under either rule, is unchanged.
fn clip_contours(
    verbs: &[Verb],
    points: &[Point],
    window: (f64, f64, f64, f64),
) -> (Vec<Verb>, Vec<Point>) {
    let (x0, y0, x1, y1) = window;
    let mut out_verbs = Vec::new();
    let mut out_points = Vec::new();
    let mut contour: Vec<(f64, f64)> = Vec::new();
    let mut scratch: Vec<(f64, f64)> = Vec::new();
    let mut emit = |contour: &mut Vec<(f64, f64)>, scratch: &mut Vec<(f64, f64)>| {
        // One boundary at a time: x >= x0, x <= x1, y >= y0, y <= y1.
        for side in 0..4 {
            let inside = |p: (f64, f64)| match side {
                0 => p.0 >= x0,
                1 => p.0 <= x1,
                2 => p.1 >= y0,
                _ => p.1 <= y1,
            };
            let cross = |a: (f64, f64), b: (f64, f64)| {
                if side < 2 {
                    let x = if side == 0 { x0 } else { x1 };
                    let t = (x - a.0) / (b.0 - a.0);
                    (x, a.1 + t * (b.1 - a.1))
                } else {
                    let y = if side == 2 { y0 } else { y1 };
                    let t = (y - a.1) / (b.1 - a.1);
                    (a.0 + t * (b.0 - a.0), y)
                }
            };
            scratch.clear();
            for (i, &current) in contour.iter().enumerate() {
                let previous = contour[(i + contour.len() - 1) % contour.len()];
                match (inside(previous), inside(current)) {
                    (true, true) => scratch.push(current),
                    (true, false) => scratch.push(cross(previous, current)),
                    (false, true) => {
                        scratch.push(cross(previous, current));
                        scratch.push(current);
                    }
                    (false, false) => {}
                }
            }
            std::mem::swap(contour, scratch);
            if contour.is_empty() {
                return;
            }
        }
        if contour.len() >= 3 {
            for (i, &(x, y)) in contour.iter().enumerate() {
                out_verbs.push(if i == 0 { Verb::Move } else { Verb::Line });
                out_points.push(Point::new(x as f32, y as f32));
            }
            out_verbs.push(Verb::Close);
        }
        contour.clear();
    };
    let mut next = points.iter();
    for &verb in verbs {
        match verb {
            Verb::Move => {
                emit(&mut contour, &mut scratch);
                contour.clear();
                if let Some(p) = next.next() {
                    contour.push((f64::from(p.x), f64::from(p.y)));
                }
            }
            Verb::Line => {
                if let Some(p) = next.next() {
                    contour.push((f64::from(p.x), f64::from(p.y)));
                }
            }
            // Flattened paths have no curves; skip their points if any.
            Verb::Quad => {
                next.nth(1);
            }
            Verb::Cubic => {
                next.nth(2);
            }
            Verb::Close => {}
        }
    }
    emit(&mut contour, &mut scratch);
    (out_verbs, out_points)
}

/// How far, in device pixels, the straight lines that replace a curve may
/// stray from it: a fifth of the rasteriser's 4x4 sampling step.
const FLATNESS: f64 = 0.05;
/// The most lines one curve becomes, whatever its size.
const MAX_PIECES: f64 = 1024.0;

fn pieces(deviation: f64) -> u32 {
    if deviation.is_finite() && deviation > 0.0 {
        deviation.sqrt().ceil().clamp(1.0, MAX_PIECES) as u32
    } else {
        1
    }
}

fn length(x: f64, y: f64) -> f64 {
    (x * x + y * y).sqrt()
}

/// Appends `verbs` and `points` to the output with every curve replaced by
/// straight lines no farther than [`FLATNESS`] from it, so that every point
/// handed to the rasteriser lies on the outline.
///
/// The number of lines depends only on the curve's own points: for a
/// quadratic the distance from a chord of a piece of parameter length `h`
/// is at most `h^2 |p0 - 2 p1 + p2| / 4`, and for a cubic at most
/// `3 h^2 max(|p0 - 2 p1 + p2|, |p1 - 2 p2 + p3|) / 4`.
fn flatten(
    verbs: &[Verb],
    points: &[Point],
    out_verbs: &mut Vec<Verb>,
    out_points: &mut Vec<Point>,
) {
    let mut next = points.iter().copied();
    let mut current = Point::ZERO;
    let mut start = Point::ZERO;
    let f = |p: Point| (f64::from(p.x), f64::from(p.y));
    for &verb in verbs {
        match verb {
            Verb::Move => {
                let Some(p) = next.next() else { return };
                out_verbs.push(Verb::Move);
                out_points.push(p);
                current = p;
                start = p;
            }
            Verb::Line => {
                let Some(p) = next.next() else { return };
                out_verbs.push(Verb::Line);
                out_points.push(p);
                current = p;
            }
            Verb::Quad => {
                let (Some(c), Some(p)) = (next.next(), next.next()) else {
                    return;
                };
                let ((x0, y0), (x1, y1), (x2, y2)) = (f(current), f(c), f(p));
                let n = pieces(length(x0 - 2.0 * x1 + x2, y0 - 2.0 * y1 + y2) / (4.0 * FLATNESS));
                for i in 1..n {
                    let t = f64::from(i) / f64::from(n);
                    let u = 1.0 - t;
                    let (a, b, d) = (u * u, 2.0 * u * t, t * t);
                    out_verbs.push(Verb::Line);
                    out_points.push(Point::new(
                        (a * x0 + b * x1 + d * x2) as f32,
                        (a * y0 + b * y1 + d * y2) as f32,
                    ));
                }
                out_verbs.push(Verb::Line);
                out_points.push(p);
                current = p;
            }
            Verb::Cubic => {
                let (Some(c1), Some(c2), Some(p)) = (next.next(), next.next(), next.next()) else {
                    return;
                };
                let ((x0, y0), (x1, y1), (x2, y2), (x3, y3)) = (f(current), f(c1), f(c2), f(p));
                let bend = length(x0 - 2.0 * x1 + x2, y0 - 2.0 * y1 + y2)
                    .max(length(x1 - 2.0 * x2 + x3, y1 - 2.0 * y2 + y3));
                let n = pieces(0.75 * bend / FLATNESS);
                for i in 1..n {
                    let t = f64::from(i) / f64::from(n);
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    out_verbs.push(Verb::Line);
                    out_points.push(Point::new(
                        (a * x0 + b * x1 + c * x2 + d * x3) as f32,
                        (a * y0 + b * y1 + c * y2 + d * y3) as f32,
                    ));
                }
                out_verbs.push(Verb::Line);
                out_points.push(p);
                current = p;
            }
            Verb::Close => {
                out_verbs.push(Verb::Close);
                current = start;
            }
        }
    }
}

/// The grid lines outlines are cut at, as inclusive ranges of multiples of
/// [`GRID`]: `columns` for the vertical lines, `rows` for the horizontal.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Lines {
    columns: (f64, f64),
    rows: (f64, f64),
}

/// Appends the line from `a` to `b` as lines that each stay between two
/// neighbouring horizontal grid lines: the points where it crosses a line
/// of `rows` strictly between its ends, in order, then `b`. Each crossing
/// lies exactly on its grid line, and its `x` between those of `a` and
/// `b`.
fn cut_rows(a: Point, b: Point, rows: (f64, f64), verbs: &mut Vec<Verb>, points: &mut Vec<Point>) {
    let g = f64::from(GRID);
    let (ax, ay, bx, by) = (
        f64::from(a.x),
        f64::from(a.y),
        f64::from(b.x),
        f64::from(b.y),
    );
    let first = ((ay.min(by) / g).floor() + 1.0).max(rows.0);
    let last = ((ay.max(by) / g).ceil() - 1.0).min(rows.1);
    if first <= last {
        // At most the number of grid rows of the largest pixmap.
        let count = (last - first) as u32 + 1;
        for i in 0..count {
            let k = if by > ay {
                first + f64::from(i)
            } else {
                last - f64::from(i)
            };
            let y = k * g;
            let t = (y - ay) / (by - ay);
            let x = (ax + t * (bx - ax)).clamp(ax.min(bx), ax.max(bx));
            verbs.push(Verb::Line);
            points.push(Point::new(x as f32, y as f32));
        }
    }
    verbs.push(Verb::Line);
    points.push(b);
}

/// Appends the line from `p` to `q`, cut where it crosses the grid lines
/// of `lines` strictly between its ends, so that every piece lies in one
/// cell of the grid: first at the vertical lines (with `y` taken along
/// `p .. q`), then each piece at the horizontal ones (with `x` taken along
/// the piece). A crossing is computed from the same two points whichever
/// other lines are cut at, so the pieces inside a window of the grid do
/// not depend on whether the lines outside it are cut too.
fn cut_line(p: Point, q: Point, lines: Lines, verbs: &mut Vec<Verb>, points: &mut Vec<Point>) {
    let g = f64::from(GRID);
    let (px, py, qx, qy) = (
        f64::from(p.x),
        f64::from(p.y),
        f64::from(q.x),
        f64::from(q.y),
    );
    let first = ((px.min(qx) / g).floor() + 1.0).max(lines.columns.0);
    let last = ((px.max(qx) / g).ceil() - 1.0).min(lines.columns.1);
    let mut from = p;
    if first <= last {
        let count = (last - first) as u32 + 1;
        for i in 0..count {
            let k = if qx > px {
                first + f64::from(i)
            } else {
                last - f64::from(i)
            };
            let x = k * g;
            let t = (x - px) / (qx - px);
            let y = (py + t * (qy - py)).clamp(py.min(qy), py.max(qy));
            let to = Point::new(x as f32, y as f32);
            cut_rows(from, to, lines.rows, verbs, points);
            from = to;
        }
    }
    cut_rows(from, q, lines.rows, verbs, points);
}

/// Builds the contours of a flattened outline, closed and cut on the grid.
struct Cutter<'a> {
    lines: Lines,
    verbs: &'a mut Vec<Verb>,
    points: &'a mut Vec<Point>,
    start: Point,
    current: Point,
    /// Whether a contour has begun.
    begun: bool,
    /// Whether it has a line, so it needs closing.
    open: bool,
}

impl Cutter<'_> {
    fn move_to(&mut self, p: Point) {
        self.close();
        self.verbs.push(Verb::Move);
        self.points.push(p);
        self.start = p;
        self.current = p;
        self.begun = true;
    }

    fn line_to(&mut self, p: Point) {
        // A line after a close continues from the contour's start, as the
        // rasteriser's path builder would make it.
        if !self.begun {
            self.move_to(self.current);
        }
        cut_line(self.current, p, self.lines, self.verbs, self.points);
        self.current = p;
        self.open = true;
    }

    /// Closes the contour with an explicit, cut line back to its start:
    /// the rasteriser would close it with one uncut line.
    fn close(&mut self) {
        if self.open {
            if self.current != self.start {
                cut_line(
                    self.current,
                    self.start,
                    self.lines,
                    self.verbs,
                    self.points,
                );
            }
            self.verbs.push(Verb::Close);
        }
        self.open = false;
        self.begun = false;
        self.current = self.start;
    }
}

/// Appends the flattened outline in `verbs` and `points` with every contour
/// closed and every line cut at the grid lines of `lines`.
fn cut(
    verbs: &[Verb],
    points: &[Point],
    lines: Lines,
    out_verbs: &mut Vec<Verb>,
    out_points: &mut Vec<Point>,
) {
    let mut cutter = Cutter {
        lines,
        verbs: out_verbs,
        points: out_points,
        start: Point::ZERO,
        current: Point::ZERO,
        begun: false,
        open: false,
    };
    let mut next = points.iter().copied();
    for &verb in verbs {
        match verb {
            Verb::Move => {
                let Some(p) = next.next() else { break };
                cutter.move_to(p);
            }
            Verb::Line => {
                let Some(p) = next.next() else { break };
                cutter.line_to(p);
            }
            // Flattened outlines have no curves; skip their points if any.
            Verb::Quad => {
                next.nth(1);
            }
            Verb::Cubic => {
                next.nth(2);
            }
            Verb::Close => cutter.close(),
        }
    }
    cutter.close();
}

/// Where the colour of a column's pixel comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// One premultiplied colour.
    Solid([u8; 4]),
    /// A gradient that changes only down the area: the strip holds one
    /// colour per row of the target.
    ByRow,
    /// A gradient that changes only across: one colour per column.
    ByColumn,
    /// Anything else: a coverage mask, then the rasteriser's shader.
    Mask,
}

/// Blends one premultiplied colour, scaled by `coverage`, over a
/// premultiplied pixel (source over). Every step rounds to nearest, and the
/// result is premultiplied again: no channel can exceed its alpha or 255.
fn blend_solid(pixel: &mut [u8], color: [u8; 4], coverage: u8) {
    if coverage == 0 {
        return;
    }
    if coverage == 255 && color[3] == 255 {
        pixel.copy_from_slice(&color);
        return;
    }
    let source = if coverage == 255 {
        color
    } else {
        color.map(|c| mul_div_255(c, coverage))
    };
    let keep = 255 - source[3];
    for (d, s) in pixel.iter_mut().zip(source) {
        *d = s + mul_div_255(*d, keep);
    }
}

/// Each channel of a pixel packed in a `u32` (little-endian RGBA) times
/// `alpha` over 255, rounded as [`mul_div_255`] rounds it: two channels at
/// a time in 16-bit lanes, which never carry into each other.
fn scale_pixel(pixel: u32, alpha: u32) -> u32 {
    const LANES: u32 = 0x00FF_00FF;
    let rb = (pixel & LANES) * alpha + 0x0080_0080;
    let rb = ((rb + ((rb >> 8) & LANES)) >> 8) & LANES;
    let ag = ((pixel >> 8) & LANES) * alpha + 0x0080_0080;
    let ag = (ag + ((ag >> 8) & LANES)) & !LANES;
    rb | ag
}

/// `mask` if it is `width` by `height`; otherwise a mask of that size,
/// all zero, reusing its memory.
fn sized_mask(mask: Option<sk::Mask>, width: u32, height: u32) -> Option<sk::Mask> {
    match mask {
        Some(m) if m.width() == width && m.height() == height => Some(m),
        other => blank_mask(other, width, height),
    }
}

/// A mask of `width` by `height`, all zero, reusing `old`'s memory.
fn blank_mask(old: Option<sk::Mask>, width: u32, height: u32) -> Option<sk::Mask> {
    let size = sk::IntSize::from_wh(width, height)?;
    let mut data = old.map(sk::Mask::take).unwrap_or_default();
    data.clear();
    data.resize(width as usize * height as usize, 0);
    sk::Mask::from_vec(data, size)
}

/// Sets the bytes of `area` to zero in a buffer of `width` columns with
/// `channels` bytes per pixel. `area` lies inside the buffer.
fn clear(data: &mut [u8], width: u32, channels: usize, area: IRect) {
    let row = width as usize * channels;
    let (x0, x1) = (area.x as usize * channels, area.right() as usize * channels);
    for y in area.y as usize..area.bottom() as usize {
        data[y * row + x0..y * row + x1].fill(0);
    }
}

/// Copies the pixels of `area` from one buffer of `width` premultiplied
/// RGBA pixels per row to another of the same size.
fn copy(from: &[u8], to: &mut [u8], width: u32, area: IRect) {
    let row = width as usize * 4;
    let (x0, x1) = (area.x as usize * 4, area.right() as usize * 4);
    for y in area.y as usize..area.bottom() as usize {
        let span = y * row + x0..y * row + x1;
        to[span.clone()].copy_from_slice(&from[span]);
    }
}

/// `rect` rounded out to whole cells of the grid.
fn on_grid(rect: IRect) -> IRect {
    let g = i64::from(GRID);
    let down = |v: i64| v.div_euclid(g) * g;
    let up = |v: i64| (v + g - 1).div_euclid(g) * g;
    // Damage lies inside a pixmap of at most MAX_SIDE pixels a side, so the
    // edges fit an i32.
    IRect::from_edges(
        down(i64::from(rect.x)) as i32,
        down(i64::from(rect.y)) as i32,
        up(rect.right()) as i32,
        up(rect.bottom()) as i32,
    )
}

/// The buffer being drawn on at layer `level`: the target (or its copy)
/// at level 0, a layer's buffer above.
fn surface<'a>(
    layers: &'a mut [Vec<u8>],
    base: &'a mut [u8],
    level: usize,
) -> Option<&'a mut [u8]> {
    match level.checked_sub(1) {
        None => Some(base),
        Some(i) => layers.get_mut(i).map(Vec::as_mut_slice),
    }
}

fn sk_transform(t: Transform) -> sk::Transform {
    sk::Transform::from_row(t.a, t.b, t.c, t.d, t.e, t.f)
}

fn sk_color(c: Color) -> sk::Color {
    sk::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

fn spread(extend: Extend) -> sk::SpreadMode {
    match extend {
        Extend::Pad => sk::SpreadMode::Pad,
        Extend::Repeat => sk::SpreadMode::Repeat,
        Extend::Reflect => sk::SpreadMode::Reflect,
    }
}

/// Gradient stops as the rasteriser reads them: offsets clamped to
/// `0 ..= 1` and never going back (a `NaN` offset repeats the one before).
fn clean_stops(stops: &[Stop]) -> impl Iterator<Item = (f32, Color)> + '_ {
    let mut last = 0.0f32;
    stops.iter().map(move |stop| {
        if !stop.offset.is_nan() {
            last = stop.offset.clamp(0.0, 1.0).max(last);
        }
        (last, stop.color)
    })
}

fn sk_stops(stops: &[Stop]) -> Vec<sk::GradientStop> {
    clean_stops(stops)
        .map(|(offset, color)| sk::GradientStop::new(offset, sk_color(color)))
        .collect()
}

/// The average colour of a gradient along its length, straight alpha, each
/// channel on its own: the colour of a repeating gradient seen from so far
/// that a whole period fits in a point. Computed in `f64` and rounded.
fn average_color(stops: &[Stop]) -> Color {
    let channels = |c: Color| [c.r, c.g, c.b, c.a].map(f64::from);
    let mut sum = [0.0f64; 4];
    let mut previous: Option<(f64, [f64; 4])> = None;
    for (offset, color) in clean_stops(stops) {
        let (t, c) = (f64::from(offset), channels(color));
        // Before the first stop its colour holds; between two stops the
        // colour changes linearly, so it averages to their mean.
        let (weight, mean) = match previous {
            None => (t, c),
            Some((t0, c0)) => (t - t0, [0, 1, 2, 3].map(|i| 0.5 * (c0[i] + c[i]))),
        };
        for (s, m) in sum.iter_mut().zip(mean) {
            *s += weight * m;
        }
        previous = Some((t, c));
    }
    // After the last stop its colour holds to the end.
    if let Some((t, c)) = previous {
        for (s, m) in sum.iter_mut().zip(c) {
            *s += (1.0 - t) * m;
        }
    }
    let [r, g, b, a] = sum.map(|v| (v + 0.5).clamp(0.0, 255.0) as u8);
    Color::rgba(r, g, b, a)
}

/// The one colour of a gradient without a direction: its last colour when
/// padded, the average of its colours when repeated or reflected.
fn degenerate_color(stops: &[Stop], extend: Extend) -> Option<Color> {
    match extend {
        Extend::Pad => stops.last().map(|stop| stop.color),
        Extend::Repeat | Extend::Reflect => (!stops.is_empty()).then(|| average_color(stops)),
    }
}

/// What a fill, a stroke or columns paint with, once gradients that come
/// down to one colour have become that colour.
enum Ink {
    /// One colour, not fully transparent.
    Solid(Color),
    /// A gradient, in the shape's coordinates.
    Shader(sk::Shader<'static>),
}

fn solid(color: Option<Color>) -> Option<Ink> {
    color.filter(|c| c.a > 0).map(Ink::Solid)
}

/// The ink for a stored paint, or `None` if it draws nothing.
fn ink(scene: &Scene, paint: StoredPaint) -> Option<Ink> {
    match paint {
        StoredPaint::Solid(color) => solid(Some(color)),
        StoredPaint::Linear {
            start,
            end,
            stops,
            extend,
        } => {
            if !(start.is_finite() && end.is_finite()) {
                return None;
            }
            let stops = &scene.stops[stops.range()];
            let distance = length(
                f64::from(end.x) - f64::from(start.x),
                f64::from(end.y) - f64::from(start.y),
            );
            if stops.len() <= 1 || distance <= DEGENERATE {
                return solid(degenerate_color(stops, extend));
            }
            sk::LinearGradient::new(
                sk::Point::from_xy(start.x, start.y),
                sk::Point::from_xy(end.x, end.y),
                sk_stops(stops),
                spread(extend),
                sk::Transform::identity(),
            )
            .map(Ink::Shader)
        }
        StoredPaint::Radial {
            center,
            radius,
            stops,
            extend,
        } => {
            if !(center.is_finite() && radius.is_finite() && radius > 0.0) {
                return None;
            }
            let stops = &scene.stops[stops.range()];
            if stops.len() <= 1 || f64::from(radius) <= DEGENERATE {
                return solid(degenerate_color(stops, extend));
            }
            let c = sk::Point::from_xy(center.x, center.y);
            sk::RadialGradient::new(
                c,
                0.0,
                c,
                radius,
                sk_stops(stops),
                spread(extend),
                sk::Transform::identity(),
            )
            .map(Ink::Shader)
        }
    }
}

fn sk_paint(ink: Ink) -> sk::Paint<'static> {
    sk::Paint {
        shader: match ink {
            Ink::Solid(color) => sk::Shader::SolidColor(sk_color(color)),
            Ink::Shader(shader) => shader,
        },
        anti_alias: true,
        ..sk::Paint::default()
    }
}

/// A `tiny-skia` path from verbs and points, or `None` if it is empty or
/// not finite.
fn sk_path(verbs: &[Verb], points: &[Point]) -> Option<sk::Path> {
    let mut builder = sk::PathBuilder::with_capacity(verbs.len(), points.len());
    let mut next = points.iter();
    let mut take = || next.next().copied().unwrap_or(Point::ZERO);
    for &verb in verbs {
        match verb {
            Verb::Move => {
                let p = take();
                builder.move_to(p.x, p.y);
            }
            Verb::Line => {
                let p = take();
                builder.line_to(p.x, p.y);
            }
            Verb::Quad => {
                let (c, p) = (take(), take());
                builder.quad_to(c.x, c.y, p.x, p.y);
            }
            Verb::Cubic => {
                let (c1, c2, p) = (take(), take(), take());
                builder.cubic_to(c1.x, c1.y, c2.x, c2.y, p.x, p.y);
            }
            Verb::Close => builder.close(),
        }
    }
    builder.finish()
}

fn sk_fill_rule(rule: FillRule) -> sk::FillRule {
    match rule {
        FillRule::NonZero => sk::FillRule::Winding,
        FillRule::EvenOdd => sk::FillRule::EvenOdd,
    }
}

/// The walk through one scene for one pass.
struct Walk {
    /// The target's size.
    width: u32,
    height: u32,
    /// From the current coordinates to the target's device pixels.
    transform: Transform,
    /// Clip masks in use.
    clips: usize,
    /// Layer buffers in use.
    layers: usize,
    /// Clips and layers open, buffered or not (skipped ones included).
    depth: usize,
    /// How many skipped clips and layers enclose the current command.
    skipping: usize,
    /// For a partial render, the part of the target the pass works on.
    window: Option<Window>,
}

impl Walk {
    /// The pixels of the target the pass may write.
    fn pixels(&self) -> IRect {
        let all = IRect::new(0, 0, self.width, self.height);
        match self.window {
            Some(window) => window.area.intersect(&all).unwrap_or_default(),
            None => all,
        }
    }

    /// The grid lines outlines are cut at: every one through the target in
    /// a whole render, the window's own in a pass of a partial one.
    fn lines(&self) -> Lines {
        let g = i64::from(GRID);
        let ceil = |v: i64| (v + g - 1).div_euclid(g) as f64;
        match self.window {
            Some(window) => {
                let a = window.area;
                Lines {
                    columns: (i64::from(a.x).div_euclid(g) as f64, ceil(a.right())),
                    rows: (i64::from(a.y).div_euclid(g) as f64, ceil(a.bottom())),
                }
            }
            None => Lines {
                columns: (0.0, ceil(i64::from(self.width))),
                rows: (0.0, ceil(i64::from(self.height))),
            },
        }
    }

    /// Whether something drawn within `bounds` (device pixels, before the
    /// guard) can change a pixel the pass must get right. Bounds that are
    /// not finite reach everything.
    fn reaches(&self, bounds: Rect) -> bool {
        let Some(window) = self.window else {
            return true;
        };
        let edges = [bounds.x, bounds.y, bounds.right(), bounds.bottom()].map(f64::from);
        if edges.iter().any(|e| !e.is_finite()) {
            return true;
        }
        // Mapping a point and mapping bounds round differently, by a few
        // units of the last place of the largest coordinate.
        let largest = edges.iter().fold(0.0f64, |m, e| m.max(e.abs()));
        let guard = f64::from(GUARD) + largest * f64::from(f32::EPSILON) * 4.0;
        let c = window.cull;
        edges[0] - guard < c.right() as f64
            && edges[2] + guard > f64::from(c.x)
            && edges[1] - guard < c.bottom() as f64
            && edges[3] + guard > f64::from(c.y)
    }
}

impl Raster for Skia {
    fn render(
        &mut self,
        scene: &Scene,
        target: &mut Pixmap,
        transform: Transform,
        damage: Option<&[IRect]>,
        mut caches: Caches<'_>,
    ) {
        if !transform.is_finite() || target.is_empty() {
            return;
        }
        let (width, height) = (target.width(), target.height());
        if self.size != (width, height) {
            // Masks are made again at the new size when a clip needs one;
            // layer buffers are resized when a layer is pushed.
            self.masks.clear();
            self.mask_dirty.clear();
            self.columns = None;
            self.size = (width, height);
        }
        let Some(damage) = damage else {
            // A whole render draws straight into the target's own buffer,
            // which never leaves it, so a panic cannot leave it short.
            self.pass(scene, target.data_mut(), transform, None, &mut caches);
            return;
        };
        self.plan(damage);
        // Taken so that the passes can borrow `self`; a panic loses it, and
        // the next partial render makes another.
        let mut scratch = std::mem::take(&mut self.scratch);
        let len = target.data().len();
        if scratch.len() != len {
            scratch.clear();
            scratch.resize(len, 0);
        }
        for pass in 0..self.passes.len() {
            let Some(window) = self.passes[pass] else {
                continue;
            };
            for (rect, _) in damage
                .iter()
                .zip(&self.owners)
                .filter(|&(_, &owner)| owner == pass)
            {
                copy(target.data(), &mut scratch, width, *rect);
            }
            self.pass(scene, &mut scratch, transform, Some(window), &mut caches);
            let pixels = target.data_mut();
            for (rect, _) in damage
                .iter()
                .zip(&self.owners)
                .filter(|&(_, &owner)| owner == pass)
            {
                copy(&scratch, pixels, width, *rect);
            }
        }
        self.scratch = scratch;
    }
}

impl Skia {
    /// Groups disjoint damage rectangles into passes: one per rectangle,
    /// then passes whose windows overlap merged, so no window's pixels are
    /// rasterised twice.
    fn plan(&mut self, damage: &[IRect]) {
        self.passes.clear();
        self.owners.clear();
        for (i, rect) in damage.iter().enumerate() {
            self.passes.push(Some(Window {
                area: on_grid(*rect),
                cull: *rect,
            }));
            self.owners.push(i);
        }
        loop {
            let mut merged = false;
            for a in 0..self.passes.len() {
                for b in a + 1..self.passes.len() {
                    let (Some(first), Some(second)) = (self.passes[a], self.passes[b]) else {
                        continue;
                    };
                    if first.area.intersect(&second.area).is_none() {
                        continue;
                    }
                    self.passes[a] = Some(Window {
                        area: first.area.union(&second.area),
                        cull: first.cull.union(&second.cull),
                    });
                    self.passes[b] = None;
                    for owner in &mut self.owners {
                        if *owner == b {
                            *owner = a;
                        }
                    }
                    merged = true;
                }
            }
            if !merged {
                break;
            }
        }
    }

    /// Draws the whole scene once into `base`, a buffer the size of the
    /// target, within `window` if there is one.
    fn pass(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        transform: Transform,
        window: Option<Window>,
        caches: &mut Caches<'_>,
    ) {
        let mut walk = Walk {
            width: self.size.0,
            height: self.size.1,
            transform,
            clips: 0,
            layers: 0,
            depth: 0,
            skipping: 0,
            window,
        };
        self.transforms.clear();
        self.frames.clear();
        for command in &scene.commands {
            self.command(scene, base, &mut walk, *command, caches);
        }
        // Close whatever is still open, so open layers are composited.
        while let Some(frame) = self.frames.last().copied() {
            let pop = match frame.kind() {
                Kind::Clip => Command::PopClip,
                Kind::Transform => Command::PopTransform,
                Kind::Layer => Command::PopLayer,
            };
            self.command(scene, base, &mut walk, pop, caches);
        }
    }

    /// Counts a drawing command that is not skipped; in tests, panics when
    /// told to, to prove that a panic leaves the target whole.
    fn drawing(&mut self) {
        #[cfg(test)]
        {
            self.drawn += 1;
            if self.fault == Some(self.drawn) {
                panic!("fault injected at drawing command {}", self.drawn);
            }
        }
    }

    fn command(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &mut Walk,
        command: Command,
        caches: &mut Caches<'_>,
    ) {
        match command {
            Command::PushTransform(by) => {
                self.frames.push(Frame::Transform);
                self.transforms.push(walk.transform);
                walk.transform = by.then(walk.transform);
            }
            Command::PopTransform => {
                if self.frames.last().map(|f| f.kind()) == Some(Kind::Transform) {
                    self.frames.pop();
                    walk.transform = self.transforms.pop().unwrap_or(Transform::IDENTITY);
                }
            }
            Command::PushClip(shape) => self.push_clip(scene, walk, shape),
            Command::PushLayer(layer) => self.push_layer(walk, layer.opacity),
            Command::PopClip | Command::PopLayer => {
                let kind = if matches!(command, Command::PopClip) {
                    Kind::Clip
                } else {
                    Kind::Layer
                };
                let Some(&frame) = self.frames.last() else {
                    return;
                };
                if frame.kind() != kind {
                    return;
                }
                self.frames.pop();
                walk.depth -= 1;
                match frame {
                    Frame::Skip(_) => walk.skipping -= 1,
                    Frame::Clip => walk.clips -= 1,
                    Frame::Layer {
                        opacity,
                        buffered: true,
                    } => {
                        self.composite(base, walk, opacity);
                        walk.layers -= 1;
                    }
                    Frame::Layer { .. } | Frame::Transform => {}
                }
            }
            _ if walk.skipping > 0 || !walk.transform.is_finite() => {}
            Command::Fill { shape, paint } => {
                let Some(local) = shape_bounds(scene, shape) else {
                    return;
                };
                if walk.reaches(walk.transform.map_rect(&local)) {
                    self.drawing();
                    self.fill(scene, base, walk, shape, paint);
                }
            }
            Command::Stroke {
                shape,
                stroke,
                paint,
            } => {
                if !(stroke.width > 0.0 && stroke.width.is_finite()) {
                    return;
                }
                let Some(local) = shape_bounds(scene, shape) else {
                    return;
                };
                if walk.reaches(walk.transform.map_rect(&local.outset(stroke.drawn_reach()))) {
                    self.drawing();
                    self.stroke(scene, base, walk, shape, &stroke, paint);
                }
            }
            Command::Shadow {
                shape,
                stroke,
                shadow,
            } => {
                let Some(local) = shadow_bounds(scene, shape, stroke, &shadow) else {
                    return;
                };
                if walk.reaches(walk.transform.map_rect(&local)) {
                    self.drawing();
                    self.shadow(
                        scene,
                        base,
                        walk,
                        (shape, stroke, shadow, local),
                        caches.shadows,
                    );
                }
            }
            Command::Image {
                image,
                to,
                sampling,
            } => {
                if walk.reaches(walk.transform.map_rect(&to)) {
                    self.drawing();
                    self.image(scene, base, walk, image, to, sampling);
                }
            }
            Command::Columns {
                area,
                values,
                paint,
            } => {
                if walk.reaches(walk.transform.map_rect(&area)) {
                    self.drawing();
                    self.columns(
                        scene,
                        base,
                        walk,
                        area,
                        &scene.values[values.range()],
                        paint,
                    );
                }
            }
            Command::Text {
                glyphs: run,
                size,
                coords,
                color,
                ink,
                ..
            } => {
                let Some(ink) = ink.filter(|r| r.is_finite()) else {
                    return;
                };
                if color.a == 0 || !(size.is_finite() && size > 0.0) {
                    return;
                }
                if walk.reaches(walk.transform.map_rect(&ink.outset(TEXT_TOLERANCE))) {
                    self.drawing();
                    let run = &scene.glyphs[run.range()];
                    self.text(base, walk, run, (size, coords, color, ink), caches.glyphs);
                }
            }
        }
    }

    /// Draws a run of glyphs of one size, point of the variation space and
    /// colour: from cached masks when the transform allows (see
    /// [`MIN_MASK_SCALE`]), as filled outlines otherwise.
    fn text(
        &mut self,
        base: &mut [u8],
        walk: &Walk,
        run: &[StoredGlyph],
        (size, coords, color, ink): (f32, Coords, Color, Rect),
        cache: &mut GlyphCache,
    ) {
        let t = walk.transform;
        let upem = f64::from(self.face.units_per_em());
        let ppem = f64::from(size) * f64::from(t.a);
        let masks = t.b == 0.0
            && t.c == 0.0
            && t.a == t.d
            && t.a >= MIN_MASK_SCALE
            && ppem <= MAX_MASK_PPEM;
        if !masks {
            // Every glyph's outline, mapped to device pixels, filled as one
            // path, so glyphs that overlap are not blended twice.
            self.verbs.clear();
            self.points.clear();
            let k = f64::from(size) / upem;
            for glyph in run {
                let (gx, gy) = (f64::from(glyph.position.x), f64::from(glyph.position.y));
                let map = |x: f32, y: f32| {
                    t.map_point(Point::new(
                        (gx + f64::from(x) * k) as f32,
                        (gy - f64::from(y) * k) as f32,
                    ))
                };
                self.face
                    .outline(glyph.id, coords, map, &mut self.verbs, &mut self.points);
            }
            if let Some(path) = self.device_path(walk) {
                self.fill_path(
                    base,
                    walk,
                    &path,
                    sk::FillRule::Winding,
                    sk_paint(Ink::Solid(color)),
                );
            }
            return;
        }
        let ppem = ppem as f32;
        let k = f64::from(ppem) / upem;
        let reach = GLYPH_REACH * f64::from(ppem) + 2.0;
        let premultiplied = color.premultiply().to_array();
        let width = walk.width as usize;
        let (a, d, e, f) = (
            f64::from(t.a),
            f64::from(t.d),
            f64::from(t.e),
            f64::from(t.f),
        );
        // The run's coverage is gathered first, over its ink box in device
        // pixels, and blended once, so that where glyphs overlap ("ff",
        // "ft") a translucent colour is not blended twice: the outline path
        // above fills every glyph as one path for the same reason. The box
        // is a pixel wider each way than the ink, since masks place glyphs
        // within an eighth of a pixel and are rounded out to whole pixels,
        // and lies within the pixels this pass draws.
        let pixels = walk.pixels();
        let area = {
            let left = (a * f64::from(ink.x) + e - 1.0).floor();
            let right = (a * f64::from(ink.right()) + e + 1.0).ceil();
            let top = (d * f64::from(ink.y) + f - 1.0).floor();
            let bottom = (d * f64::from(ink.bottom()) + f + 1.0).ceil();
            let x0 = left.max(f64::from(pixels.x));
            let x1 = right.min(pixels.right() as f64);
            let y0 = top.max(f64::from(pixels.y));
            let y1 = bottom.min(pixels.bottom() as f64);
            if !(x0 < x1 && y0 < y1) {
                return;
            }
            // Within the pixels, which lie within the target.
            IRect::from_edges(x0 as i32, y0 as i32, x1 as i32, y1 as i32)
        };
        let Skia {
            masks,
            layers,
            face,
            verbs,
            points,
            flat_verbs,
            flat_points,
            coverage,
            ..
        } = self;
        let clip = walk
            .clips
            .checked_sub(1)
            .and_then(|i| masks.get(i))
            .map(sk::Mask::data);
        let Some(surface) = surface(layers, base, walk.layers) else {
            return;
        };
        let row = area.width as usize;
        coverage.clear();
        coverage.resize(row * area.height as usize, 0);
        for glyph in run {
            let x = a * f64::from(glyph.position.x) + e;
            let y = d * f64::from(glyph.position.y) + f;
            if !(x > f64::from(area.x) - reach
                && x < area.right() as f64 + reach
                && y > f64::from(area.y) - reach
                && y < area.bottom() as f64 + reach)
            {
                // Too far from the pixels drawn to touch them (or `NaN`).
                continue;
            }
            // The origin on a quarter of a device pixel: a whole pixel and
            // a quarter from 0 to 3 each way.
            let (qx, qy) = ((x * 4.0).round(), (y * 4.0).round());
            let (ix, iy) = ((qx / 4.0).floor(), (qy / 4.0).floor());
            let (fx, fy) = ((qx - ix * 4.0) as u8, (qy - iy * 4.0) as u8);
            let key = GlyphKey {
                glyph: glyph.id,
                ppem: ppem.to_bits(),
                coords,
                subpixel: fx + 4 * fy,
            };
            let mask = cache.get_or_draw(key, || {
                glyph_mask(
                    face,
                    (glyph.id, coords),
                    k,
                    (fx, fy),
                    (verbs, points),
                    (flat_verbs, flat_points),
                )
            });
            let Some(mask) = mask else {
                continue;
            };
            // Within the reach checked above, so these are small.
            let left = ix as i64 + i64::from(mask.left);
            let top = iy as i64 + i64::from(mask.top);
            let (x0, x1) = (
                left.max(i64::from(area.x)),
                (left + i64::from(mask.width)).min(area.right()),
            );
            let (y0, y1) = (
                top.max(i64::from(area.y)),
                (top + i64::from(mask.height)).min(area.bottom()),
            );
            if x0 >= x1 || y0 >= y1 {
                continue;
            }
            let stride = mask.width as usize;
            for py in y0..y1 {
                let from = (py - top) as usize * stride;
                let to = (py - i64::from(area.y)) as usize * row;
                for px in x0..x1 {
                    let c = mask.data[from + (px - left) as usize];
                    if c == 0 {
                        continue;
                    }
                    // Two coverages joined as blending one over the other
                    // joins them: c1 + c2 - c1 * c2, which is what opaque
                    // glyphs blended one by one come to.
                    let held = &mut coverage[to + (px - i64::from(area.x)) as usize];
                    *held += mul_div_255(c, 255 - *held);
                }
            }
        }
        cache.release();
        for py in 0..area.height as usize {
            let y = area.y as usize + py;
            for (px, &c) in coverage[py * row..(py + 1) * row].iter().enumerate() {
                if c == 0 {
                    continue;
                }
                let i = y * width + area.x as usize + px;
                let c = clip.map_or(c, |clip| mul_div_255(c, clip[i]));
                blend_solid(&mut surface[i * 4..i * 4 + 4], premultiplied, c);
            }
        }
    }

    /// The outline of `shape` in the shape's own coordinates.
    fn local_outline(&mut self, scene: &Scene, shape: StoredShape) -> Option<sk::Path> {
        self.verbs.clear();
        self.points.clear();
        scene.outline(shape, &mut self.verbs, &mut self.points);
        sk_path(&self.verbs, &self.points)
    }

    /// The outline of `shape` as the rasteriser fills it, and its rule.
    fn shape_path(
        &mut self,
        scene: &Scene,
        walk: &Walk,
        shape: StoredShape,
    ) -> Option<(sk::Path, sk::FillRule)> {
        self.verbs.clear();
        self.points.clear();
        let rule = scene.outline(shape, &mut self.verbs, &mut self.points);
        let transform = walk.transform;
        for p in &mut self.points {
            *p = transform.map_point(*p);
        }
        Some((self.device_path(walk)?, sk_fill_rule(rule)))
    }

    /// The path in `verbs` and `points`, given in the target's device
    /// pixels, as the `tiny-skia` path the rasteriser fills: flattened, cut
    /// on the grid, and in a partial render clamped to the window.
    fn device_path(&mut self, walk: &Walk) -> Option<sk::Path> {
        self.flat_verbs.clear();
        self.flat_points.clear();
        flatten(
            &self.verbs,
            &self.points,
            &mut self.flat_verbs,
            &mut self.flat_points,
        );
        // A point that is not finite (given so, or overflowed on the way
        // to device pixels) leaves no outline to draw.
        if self.flat_points.iter().any(|p| !p.is_finite()) {
            return None;
        }
        if self
            .flat_points
            .iter()
            .any(|p| p.x.abs() > SAFE || p.y.abs() > SAFE)
        {
            // Too far out for the rasteriser's fixed-point edges: cut the
            // contours to a window a grid cell larger than the target
            // first, the same in every render of this target. Only edges
            // outside the target change.
            let g = f64::from(GRID);
            let window = (
                -g,
                -g,
                f64::from(walk.width) + g,
                f64::from(walk.height) + g,
            );
            let (verbs, points) = clip_contours(&self.flat_verbs, &self.flat_points, window);
            self.flat_verbs = verbs;
            self.flat_points = points;
        }
        self.cut_verbs.clear();
        self.cut_points.clear();
        cut(
            &self.flat_verbs,
            &self.flat_points,
            walk.lines(),
            &mut self.cut_verbs,
            &mut self.cut_points,
        );
        if let Some(window) = walk.window {
            let a = window.area;
            let (x0, y0) = (a.x as f32, a.y as f32);
            let (x1, y1) = (a.right() as f32, a.bottom() as f32);
            for p in &mut self.cut_points {
                *p = Point::new(p.x.clamp(x0, x1), p.y.clamp(y0, y1));
            }
        }
        sk_path(&self.cut_verbs, &self.cut_points)
    }

    /// Fills a path given in device pixels; `paint` is in the coordinates
    /// of the shape it came from.
    fn fill_path(
        &mut self,
        base: &mut [u8],
        walk: &Walk,
        path: &sk::Path,
        rule: sk::FillRule,
        mut paint: sk::Paint<'_>,
    ) {
        paint.shader.transform(sk_transform(walk.transform));
        let mask = walk.clips.checked_sub(1).and_then(|i| self.masks.get(i));
        let Some(surface) = surface(&mut self.layers, base, walk.layers) else {
            return;
        };
        if let Some(mut pixmap) = sk::PixmapMut::from_bytes(surface, walk.width, walk.height) {
            pixmap.fill_path(path, &paint, rule, sk::Transform::identity(), mask);
        }
    }

    /// Fills `rect`; returns whether anything was handed to the
    /// rasteriser. With no rotation or skew it is mapped to device pixels
    /// and drawn by `tiny-skia`'s routine for rectangles, whose coverage of
    /// each pixel depends only on where the edges fall.
    fn fill_rect(
        &mut self,
        base: &mut [u8],
        walk: &Walk,
        rect: Rect,
        mut paint: sk::Paint<'_>,
    ) -> bool {
        if rect.is_empty() || !rect.is_finite() {
            return false;
        }
        if !walk.transform.is_axis_aligned() {
            self.verbs.clear();
            self.points.clear();
            crate::path::push_rect(&mut self.verbs, &mut self.points, rect);
            let transform = walk.transform;
            for p in &mut self.points {
                *p = transform.map_point(*p);
            }
            let Some(path) = self.device_path(walk) else {
                return false;
            };
            self.fill_path(base, walk, &path, sk::FillRule::Winding, paint);
            return true;
        }
        let a = walk.transform.map_point(rect.origin());
        let b = walk
            .transform
            .map_point(Point::new(rect.right(), rect.bottom()));
        let (mut x0, mut y0, mut x1, mut y1) =
            (a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
        if !(x0.is_finite() && y0.is_finite() && x1.is_finite() && y1.is_finite()) {
            return false;
        }
        if let Some(window) = walk.window {
            // The window grown by a pixel: a clamped edge lies on a pixel
            // boundary outside the window, where it changes no coverage
            // inside, and `tiny-skia`'s special cases for rectangles one
            // pixel high or wide cannot start inside it.
            let a = window.area;
            x0 = x0.max(a.x as f32 - 1.0);
            y0 = y0.max(a.y as f32 - 1.0);
            x1 = x1.min(a.right() as f32 + 1.0);
            y1 = y1.min(a.bottom() as f32 + 1.0);
        }
        let Some(device) = sk::Rect::from_ltrb(x0, y0, x1, y1) else {
            return false;
        };
        paint.shader.transform(sk_transform(walk.transform));
        let mask = walk.clips.checked_sub(1).and_then(|i| self.masks.get(i));
        let Some(surface) = surface(&mut self.layers, base, walk.layers) else {
            return false;
        };
        if let Some(mut pixmap) = sk::PixmapMut::from_bytes(surface, walk.width, walk.height) {
            pixmap.fill_rect(device, &paint, sk::Transform::identity(), mask);
        }
        true
    }

    fn fill(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        shape: StoredShape,
        paint: StoredPaint,
    ) {
        let Some(ink) = ink(scene, paint) else {
            return;
        };
        // A gradient that changes only down the shape, with no rotation:
        // coverage first, then one colour per row (see `fill_by_rows`).
        if let (Ink::Shader(_), StoredPaint::Linear { start, end, .. }) = (&ink, paint) {
            let t = walk.transform;
            if start.x == end.x && t.b == 0.0 && t.c == 0.0 {
                self.fill_by_rows(scene, base, walk, shape, sk_paint(ink));
                return;
            }
        }
        if let StoredShape::Rect(rect) = shape {
            self.fill_rect(base, walk, rect, sk_paint(ink));
            return;
        }
        if let Some((path, rule)) = self.shape_path(scene, walk, shape) {
            self.fill_path(base, walk, &path, rule, sk_paint(ink));
        }
    }

    /// Fills `shape` with a gradient that changes only down it (in device
    /// pixels, under a transform with no rotation or skew): the shape's
    /// coverage into a mask, then each row blended in the gradient's colour
    /// at that row, from a strip of the gradient one pixel wide, as
    /// `columns` does. The rasteriser's shader then runs once per row
    /// instead of once per pixel. The strip is taken at a column that
    /// depends on the shape only, never on the pass, so a partial render
    /// draws what a whole one does.
    fn fill_by_rows(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        shape: StoredShape,
        mut paint: sk::Paint<'static>,
    ) {
        let Some((path, rule)) = self.shape_path(scene, walk, shape) else {
            return;
        };
        let w = walk.width as usize;
        let b = path.bounds();
        let pixels = walk.pixels();
        let edges = [
            f64::from(b.left()).floor() - 1.0,
            f64::from(b.top()).floor() - 1.0,
            f64::from(b.right()).ceil() + 1.0,
            f64::from(b.bottom()).ceil() + 1.0,
        ];
        let x0 = edges[0].max(f64::from(pixels.x)) as usize;
        let y0 = edges[1].max(f64::from(pixels.y)) as usize;
        let x1 = edges[2].min(pixels.right() as f64).max(x0 as f64) as usize;
        let y1 = edges[3].min(pixels.bottom() as f64).max(y0 as f64) as usize;
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        // The gradient's colour on every row down to the last drawn, at the
        // shape's first column (clamped to the target).
        let column = edges[0].clamp(0.0, w as f64) as f32;
        self.strip.clear();
        self.strip.resize(y1 * 4, 0);
        paint.shader.transform(sk_transform(
            walk.transform.then(Transform::translate(-column, 0.0)),
        ));
        let Some(mut strip) = sk::PixmapMut::from_bytes(&mut self.strip, 1, y1 as u32) else {
            return;
        };
        if let Some(whole) = sk::Rect::from_xywh(0.0, 0.0, 1.0, y1 as f32) {
            strip.fill_rect(whole, &paint, sk::Transform::identity(), None);
        }
        let Some(mut mask) = sized_mask(self.columns.take(), walk.width, walk.height) else {
            return;
        };
        mask.fill_path(&path, rule, true, sk::Transform::identity());
        let clip = walk
            .clips
            .checked_sub(1)
            .and_then(|i| self.masks.get(i))
            .map(sk::Mask::data);
        if let Some(surface) = surface(&mut self.layers, base, walk.layers) {
            let coverage = mask.data_mut();
            for y in y0..y1 {
                let p = &self.strip[y * 4..y * 4 + 4];
                let color = [p[0], p[1], p[2], p[3]];
                let row = y * w;
                for x in x0..x1 {
                    let i = row + x;
                    let mut c = std::mem::take(&mut coverage[i]);
                    if c == 0 {
                        continue;
                    }
                    if let Some(clip) = clip {
                        c = mul_div_255(c, clip[i]);
                    }
                    blend_solid(&mut surface[i * 4..i * 4 + 4], color, c);
                }
            }
        }
        // The coverage is all zero again for the next use: what the path
        // wrote lies within its bounds and this pass's pixels.
        self.columns = Some(mask);
    }

    fn stroke(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        shape: StoredShape,
        stroke: &Stroke,
        paint: StoredPaint,
    ) {
        let Some(ink) = ink(scene, paint) else {
            return;
        };
        let sk_stroke = sk::Stroke {
            width: stroke.width,
            miter_limit: stroke.drawn_miter_limit(),
            line_cap: match stroke.cap {
                Cap::Butt => sk::LineCap::Butt,
                Cap::Round => sk::LineCap::Round,
                Cap::Square => sk::LineCap::Square,
            },
            line_join: match stroke.join {
                Join::Miter => sk::LineJoin::Miter,
                Join::Round => sk::LineJoin::Round,
                Join::Bevel => sk::LineJoin::Bevel,
            },
            dash: None,
        };
        let Some(local) = self.local_outline(scene, shape) else {
            return;
        };
        // The outline of the stroke is computed in the shape's coordinates,
        // at four times the device scale and at least at four, so that its
        // approximation stays within the tolerance `Scene::bounds` allows,
        // then mapped to device pixels and filled. The resolution is
        // capped: beyond it the stroker would subdivide curves without end
        // for no visible gain.
        let resolution = (sk::PathStroker::compute_resolution_scale(&sk_transform(walk.transform))
            * STROKE_OVERSAMPLE)
            .clamp(STROKE_OVERSAMPLE, MAX_STROKE_RESOLUTION);
        let dashed;
        let local = match stroke.dash {
            None => &local,
            Some(d) => {
                // A pattern that cannot be followed draws nothing.
                let Some(dash) = sk::StrokeDash::new(vec![d.on, d.off], d.offset) else {
                    return;
                };
                let Some(path) = local.dash(&dash, resolution) else {
                    return;
                };
                dashed = path;
                &dashed
            }
        };
        let Some(outline) = local.stroke(&sk_stroke, resolution) else {
            return;
        };
        let Some(device) = self.map_sk_path(walk, &outline) else {
            return;
        };
        self.fill_path(base, walk, &device, sk::FillRule::Winding, sk_paint(ink));
    }

    /// A `tiny-skia` path in the shape's coordinates, mapped to device
    /// pixels and made ready for the rasteriser.
    fn map_sk_path(&mut self, walk: &Walk, path: &sk::Path) -> Option<sk::Path> {
        self.verbs.clear();
        self.points.clear();
        let map = |p: sk::Point| walk.transform.map_point(Point::new(p.x, p.y));
        for segment in path.segments() {
            match segment {
                sk::PathSegment::MoveTo(p) => {
                    self.verbs.push(Verb::Move);
                    self.points.push(map(p));
                }
                sk::PathSegment::LineTo(p) => {
                    self.verbs.push(Verb::Line);
                    self.points.push(map(p));
                }
                sk::PathSegment::QuadTo(c, p) => {
                    self.verbs.push(Verb::Quad);
                    self.points.extend([map(c), map(p)]);
                }
                sk::PathSegment::CubicTo(c1, c2, p) => {
                    self.verbs.push(Verb::Cubic);
                    self.points.extend([map(c1), map(c2), map(p)]);
                }
                sk::PathSegment::Close => self.verbs.push(Verb::Close),
            }
        }
        self.device_path(walk)
    }

    fn image(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        index: usize,
        to: Rect,
        sampling: Sampling,
    ) {
        let Some(image) = scene.images.get(index) else {
            return;
        };
        let pixels = image.pixels();
        if pixels.is_empty() || to.is_empty() || !to.is_finite() {
            return;
        }
        let Some(source) =
            sk::PixmapRef::from_bytes(pixels.data(), pixels.width(), pixels.height())
        else {
            return;
        };
        let quality = match sampling {
            Sampling::Nearest => sk::FilterQuality::Nearest,
            Sampling::Bilinear => sk::FilterQuality::Bilinear,
            Sampling::Bicubic => sk::FilterQuality::Bicubic,
        };
        let placement = sk::Transform::from_row(
            to.width / pixels.width() as f32,
            0.0,
            0.0,
            to.height / pixels.height() as f32,
            to.x,
            to.y,
        );
        let paint = sk::Paint {
            shader: sk::Pattern::new(source, sk::SpreadMode::Pad, quality, 1.0, placement),
            anti_alias: true,
            ..sk::Paint::default()
        };
        if self.fill_rect(base, walk, to, paint) && sampling == Sampling::Bicubic {
            self.clamp_overshoot(base, walk, to);
        }
    }

    /// Bicubic filtering can overshoot: a colour channel can come out
    /// above its alpha, which is no premultiplied colour at all. Every
    /// pixel the image may have touched is pulled back within its alpha.
    fn clamp_overshoot(&mut self, base: &mut [u8], walk: &Walk, to: Rect) {
        let t = walk.transform;
        let corners = [
            t.map_point(to.origin()),
            t.map_point(Point::new(to.right(), to.y)),
            t.map_point(Point::new(to.x, to.bottom())),
            t.map_point(Point::new(to.right(), to.bottom())),
        ];
        if corners.iter().any(|c| !c.is_finite()) {
            return;
        }
        let (mut x0, mut y0) = (f32::INFINITY, f32::INFINITY);
        let (mut x1, mut y1) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for c in corners {
            x0 = x0.min(c.x);
            y0 = y0.min(c.y);
            x1 = x1.max(c.x);
            y1 = y1.max(c.y);
        }
        let px = walk.pixels();
        let clamp =
            |v: f32, lo: i32, hi: i64| f64::from(v).clamp(f64::from(lo), hi as f64) as usize;
        let (c0, c1) = (
            clamp(x0.floor(), px.x, px.right()),
            clamp(x1.ceil(), px.x, px.right()),
        );
        let (r0, r1) = (
            clamp(y0.floor(), px.y, px.bottom()),
            clamp(y1.ceil(), px.y, px.bottom()),
        );
        if c0 >= c1 || r0 >= r1 {
            return;
        }
        let Some(surface) = surface(&mut self.layers, base, walk.layers) else {
            return;
        };
        let row = walk.width as usize * 4;
        for r in r0..r1 {
            for pixel in surface[r * row + c0 * 4..r * row + c1 * 4].chunks_exact_mut(4) {
                let alpha = pixel[3];
                for channel in &mut pixel[..3] {
                    *channel = (*channel).min(alpha);
                }
            }
        }
    }

    fn columns(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        area: Rect,
        values: &[f32],
        stored: StoredPaint,
    ) {
        let n = values.len() / 2;
        if n == 0 || area.is_empty() || !area.is_finite() {
            return;
        }
        let (low, high) = values.split_at(n);
        let Some(ink) = ink(scene, stored) else {
            return;
        };
        let ts = walk.transform;
        if !ts.is_axis_aligned() || ts.a == 0.0 || ts.d == 0.0 {
            self.columns_as_rects(base, walk, area, low, high, sk_paint(ink));
            return;
        }
        // One colour is blended here directly, and so is a gradient that
        // runs straight down or across, from a strip of its colours one
        // pixel wide: the shader then runs once per row or column instead
        // of once per pixel. Anything else goes through a coverage mask and
        // the rasteriser's shader.
        let source = match (&ink, stored) {
            (Ink::Solid(color), _) => Source::Solid(color.premultiply().to_array()),
            (Ink::Shader(_), StoredPaint::Linear { start, end, .. }) if start.x == end.x => {
                Source::ByRow
            }
            (Ink::Shader(_), StoredPaint::Linear { start, end, .. }) if start.y == end.y => {
                Source::ByColumn
            }
            _ => Source::Mask,
        };
        let mut paint = match ink {
            Ink::Solid(_) => None,
            Ink::Shader(_) => Some(sk_paint(ink)),
        };
        let (w, h) = (walk.width as usize, walk.height as usize);
        // Everything is computed in the target's device pixels, in f64, so
        // the coverage of a pixel does not depend on the pass.
        let (a, d, e, f) = (
            f64::from(ts.a),
            f64::from(ts.d),
            f64::from(ts.e),
            f64::from(ts.f),
        );
        let (ax, ay, aw, ah) = (
            f64::from(area.x),
            f64::from(area.y),
            f64::from(area.width),
            f64::from(area.height),
        );
        let (xa, xb) = (a * ax + e, a * (ax + aw) + e);
        let (x0, x1) = (xa.min(xb), xa.max(xb));
        let (ya, yb) = (d * ay + f, d * (ay + ah) + f);
        let (y0, y1) = (ya.min(yb), ya.max(yb));
        let px = walk.pixels();
        let columns = (f64::from(px.x), px.right() as f64);
        let rows = (f64::from(px.y), px.bottom() as f64);
        let c0 = x0.floor().clamp(columns.0, columns.1) as usize;
        let c1 = x1.ceil().clamp(columns.0, columns.1) as usize;
        let r0 = y0.floor().clamp(rows.0, rows.1) as usize;
        let r1 = y1.ceil().clamp(rows.0, rows.1) as usize;
        if c1 <= c0 || r1 <= r0 {
            return;
        }
        if let (Source::ByRow | Source::ByColumn, Some(paint)) = (source, paint.as_mut()) {
            // The strip's colours are those of the area's first column (for
            // a gradient down) or first row (across), clamped to the target
            // but not to the window, so they are the same in every pass.
            // The colour does not change along the other direction.
            let (sw, sh, shift) = if source == Source::ByRow {
                let column = x0.floor().clamp(0.0, w as f64) as f32;
                (1, r1 as u32, Point::new(column, 0.0))
            } else {
                let row = y0.floor().clamp(0.0, h as f64) as f32;
                (c1 as u32, 1, Point::new(0.0, row))
            };
            self.strip.clear();
            self.strip.resize(sw as usize * sh as usize * 4, 0);
            paint.shader.transform(sk_transform(
                walk.transform
                    .then(Transform::translate(-shift.x, -shift.y)),
            ));
            let Some(mut strip) = sk::PixmapMut::from_bytes(&mut self.strip, sw, sh) else {
                return;
            };
            if let Some(whole) = sk::Rect::from_xywh(0.0, 0.0, sw as f32, sh as f32) {
                strip.fill_rect(whole, paint, sk::Transform::identity(), None);
            }
        }
        let strip = &self.strip;
        let strip_color = |k: usize| -> [u8; 4] {
            let p = &strip[k * 4..k * 4 + 4];
            [p[0], p[1], p[2], p[3]]
        };
        let mut coverage = match source {
            Source::Mask => match sized_mask(self.columns.take(), walk.width, walk.height) {
                Some(mask) => Some(mask),
                None => return,
            },
            _ => None,
        };
        let clip = walk
            .clips
            .checked_sub(1)
            .and_then(|i| self.masks.get(i))
            .map(sk::Mask::data);
        let Some(surface) = surface(&mut self.layers, base, walk.layers) else {
            return;
        };
        let bottom_edge = ay + ah;
        let entries = n as f64;
        for c in c0..c1 {
            let column = c as f64;
            let left = column.max(x0);
            let right = (column + 1.0).min(x1);
            let cover_x = right - left;
            if cover_x <= 0.0 {
                continue;
            }
            // The entries this slice of the column overlaps.
            let (la, lb) = ((left - e) / a, (right - e) / a);
            let (l0, l1) = (la.min(lb), la.max(lb));
            // Multiplying before dividing keeps whole results exact, so a
            // column that spans exactly k entries gets exactly those.
            let i0 = ((l0 - ax) * entries / aw).floor().clamp(0.0, entries - 1.0) as usize;
            let i1 = ((l1 - ax) * entries / aw)
                .ceil()
                .clamp(i0 as f64 + 1.0, entries) as usize;
            let lo = low[i0..i1].iter().copied().fold(f32::INFINITY, f32::min);
            let hi = high[i0..i1]
                .iter()
                .copied()
                .fold(f32::NEG_INFINITY, f32::max);
            if hi <= lo {
                continue;
            }
            let ya = d * (bottom_edge - f64::from(hi) * ah) + f;
            let yb = d * (bottom_edge - f64::from(lo) * ah) + f;
            let (span_top, span_base) = (ya.min(yb), ya.max(yb));
            let rs = span_top.floor().max(r0 as f64) as usize;
            let re = span_base.ceil().min(r1 as f64) as usize;
            for r in rs..re {
                let row = r as f64;
                let cover_y = (row + 1.0).min(span_base) - row.max(span_top);
                if cover_y <= 0.0 {
                    continue;
                }
                let i = r * w + c;
                // The product is not negative, so the cast rounds down, and it
                // saturates at 255: the same as `floor` and `min`, which the
                // baseline x86-64 target turns into library calls.
                let mut value = (cover_x * cover_y * 255.0 + 0.5) as u8;
                if let Some(clip) = clip {
                    value = mul_div_255(value, clip[i]);
                }
                let pixel = &mut surface[i * 4..i * 4 + 4];
                match (source, coverage.as_mut()) {
                    (Source::Solid(color), _) => blend_solid(pixel, color, value),
                    (Source::ByRow, _) => blend_solid(pixel, strip_color(r), value),
                    (Source::ByColumn, _) => blend_solid(pixel, strip_color(c), value),
                    (Source::Mask, Some(mask)) => mask.data_mut()[i] = value,
                    (Source::Mask, None) => {}
                }
            }
        }
        let (Some(mut mask), Some(paint)) = (coverage, paint.as_mut()) else {
            return;
        };
        let device = sk::Rect::from_ltrb(c0 as f32, r0 as f32, c1 as f32, r1 as f32);
        paint.shader.transform(sk_transform(walk.transform));
        if let (Some(device), Some(mut pixmap)) = (
            device,
            sk::PixmapMut::from_bytes(surface, walk.width, walk.height),
        ) {
            pixmap.fill_rect(device, paint, sk::Transform::identity(), Some(&mask));
        }
        // Leave the coverage all zero for the next use.
        let data = mask.data_mut();
        for r in r0..r1 {
            data[r * w + c0..r * w + c1].fill(0);
        }
        self.columns = Some(mask);
    }

    /// Columns under a rotation or skew: one rectangle per entry.
    fn columns_as_rects(
        &mut self,
        base: &mut [u8],
        walk: &Walk,
        area: Rect,
        low: &[f32],
        high: &[f32],
        paint: sk::Paint<'static>,
    ) {
        let n = low.len();
        let step = area.width / n as f32;
        self.verbs.clear();
        self.points.clear();
        for (i, (&lo, &hi)) in low.iter().zip(high).enumerate() {
            if hi <= lo {
                continue;
            }
            let x = area.x + step * i as f32;
            let top = area.bottom() - hi * area.height;
            let bottom = area.bottom() - lo * area.height;
            crate::path::push_rect(
                &mut self.verbs,
                &mut self.points,
                Rect::new(x, top, step, bottom - top),
            );
        }
        let transform = walk.transform;
        for p in &mut self.points {
            *p = transform.map_point(*p);
        }
        if let Some(path) = self.device_path(walk) {
            self.fill_path(base, walk, &path, sk::FillRule::Winding, paint);
        }
    }

    /// Draws a shadow: the outline of the shape (or of its stroke), moved,
    /// grown, in device pixels, filled into a mask and blurred, or the
    /// mask kept from an earlier render; then the mask blended in the
    /// shadow's colour within `bounds` (in the shape's coordinates, see
    /// `shadow_bounds`). Nothing here depends on the pass's window but
    /// which pixels are blended, so a partial render draws what a whole
    /// one does.
    fn shadow(
        &mut self,
        scene: &Scene,
        base: &mut [u8],
        walk: &Walk,
        (shape, stroke, shadow, bounds): (StoredShape, Option<Stroke>, Shadow, Rect),
        cache: &mut ShadowCache,
    ) {
        let t = Transform::translate(shadow.offset.x, shadow.offset.y).then(walk.transform);
        if !t.is_finite() {
            return;
        }
        let spread = shadow.drawn_spread();
        // What is drawn into the mask: the outline, and for an outline
        // that grows by a stroke, that stroke too, unioned with it.
        let mut grow_by = 0.0f32;
        let rule = match stroke {
            None => {
                let grown = match shape {
                    StoredShape::Rect(r) => StoredShape::Rect(r.outset(spread)),
                    StoredShape::RoundedRect(r) => {
                        let radii = r.fitted_radii();
                        let grow = |v: f32| (v + spread).max(0.0);
                        StoredShape::RoundedRect(RoundedRect::with_radii(
                            r.rect.outset(spread),
                            Radii {
                                top_left: grow(radii.top_left),
                                top_right: grow(radii.top_right),
                                bottom_right: grow(radii.bottom_right),
                                bottom_left: grow(radii.bottom_left),
                            },
                        ))
                    }
                    StoredShape::Circle(c) => {
                        StoredShape::Circle(Circle::new(c.center, c.radius + spread))
                    }
                    other => {
                        grow_by = spread.max(0.0);
                        other
                    }
                };
                self.verbs.clear();
                self.points.clear();
                sk_fill_rule(scene.outline(grown, &mut self.verbs, &mut self.points))
            }
            Some(stroke) => {
                let widened = Stroke {
                    width: stroke.width + 2.0 * spread.max(0.0),
                    ..stroke
                };
                if !self.stroke_outline(scene, shape, &widened, t) {
                    return;
                }
                sk::FillRule::Winding
            }
        };
        let scale = f64::from(t.a * t.d - t.b * t.c).abs().sqrt();
        let d = box_size(f64::from(shadow.drawn_blur()) * 0.5 * scale);
        let pad = f64::from(box_reach(d) + 1);
        // The mask's top-left corner, in whole device pixels: the blur's
        // reach beyond the outline's box. The outline is drawn relative to
        // it, so that the same shadow moved by whole device pixels has the
        // same mask.
        let Some(local) = crate::path::bounds_of(&self.points) else {
            return;
        };
        let device = t.map_rect(&local);
        let (ox, oy) = (
            f64::from(device.x).floor() - pad,
            f64::from(device.y).floor() - pad,
        );
        // Beyond this a corner is not a whole `f32`.
        const WHOLE: f64 = 16_777_216.0;
        if !(ox.abs() < WHOLE && oy.abs() < WHOLE) {
            return;
        }
        let t = t.then(Transform::translate(-ox as f32, -oy as f32));
        // The outline, flattened, relative to the corner; then the grown
        // part.
        for p in &mut self.points {
            *p = t.map_point(*p);
        }
        self.flat_verbs.clear();
        self.flat_points.clear();
        flatten(
            &self.verbs,
            &self.points,
            &mut self.flat_verbs,
            &mut self.flat_points,
        );
        let mut split = (self.flat_verbs.len(), self.flat_points.len());
        if grow_by > 0.0 && self.stroke_outline(scene, shape, &Stroke::round(2.0 * grow_by), t) {
            for p in &mut self.points {
                *p = t.map_point(*p);
            }
            flatten(
                &self.verbs,
                &self.points,
                &mut self.flat_verbs,
                &mut self.flat_points,
            );
        }
        if self.flat_points.iter().any(|p| !p.is_finite()) {
            return;
        }
        let Some(outline) = crate::path::bounds_of(&self.flat_points) else {
            return;
        };
        // A shadow far larger than the target is cut to the target and the
        // blur's reach around it: what lies farther cannot reach a pixel
        // of the target. The cut depends only on the target's size and the
        // corner.
        let (w, h) = (f64::from(walk.width), f64::from(walk.height));
        let limit = (
            -ox - pad - 1.0,
            -oy - pad - 1.0,
            w - ox + pad + 1.0,
            h - oy + pad + 1.0,
        );
        let fits = f64::from(outline.x) >= limit.0
            && f64::from(outline.y) >= limit.1
            && f64::from(outline.right()) <= limit.2
            && f64::from(outline.bottom()) <= limit.3;
        if !fits {
            let (mut verbs, mut points) = clip_contours(
                &self.flat_verbs[..split.0],
                &self.flat_points[..split.1],
                limit,
            );
            let main = (verbs.len(), points.len());
            let (grown_verbs, grown_points) = clip_contours(
                &self.flat_verbs[split.0..],
                &self.flat_points[split.1..],
                limit,
            );
            verbs.extend(grown_verbs);
            points.extend(grown_points);
            self.flat_verbs = verbs;
            self.flat_points = points;
            split = main;
        }
        self.shadow_mask(
            base,
            walk,
            (rule, d, pad),
            (split, (ox, oy)),
            (shadow, bounds),
            cache,
        );
    }

    /// The outline of `shape` drawn with `stroke`, in the shape's own
    /// coordinates, into `verbs` and `points`, computed as `stroke` does
    /// for the transform `t`; false if there is none.
    fn stroke_outline(
        &mut self,
        scene: &Scene,
        shape: StoredShape,
        stroke: &Stroke,
        t: Transform,
    ) -> bool {
        if !(stroke.width > 0.0 && stroke.width.is_finite()) {
            return false;
        }
        let Some(local) = self.local_outline(scene, shape) else {
            return false;
        };
        let sk_stroke = sk::Stroke {
            width: stroke.width,
            miter_limit: stroke.drawn_miter_limit(),
            line_cap: match stroke.cap {
                Cap::Butt => sk::LineCap::Butt,
                Cap::Round => sk::LineCap::Round,
                Cap::Square => sk::LineCap::Square,
            },
            line_join: match stroke.join {
                Join::Miter => sk::LineJoin::Miter,
                Join::Round => sk::LineJoin::Round,
                Join::Bevel => sk::LineJoin::Bevel,
            },
            dash: None,
        };
        let resolution = (sk::PathStroker::compute_resolution_scale(&sk_transform(t))
            * STROKE_OVERSAMPLE)
            .clamp(STROKE_OVERSAMPLE, MAX_STROKE_RESOLUTION);
        let dashed;
        let local = match stroke.dash {
            None => &local,
            Some(d) => {
                let Some(dash) = sk::StrokeDash::new(vec![d.on, d.off], d.offset) else {
                    return false;
                };
                let Some(path) = local.dash(&dash, resolution) else {
                    return false;
                };
                dashed = path;
                &dashed
            }
        };
        let Some(outline) = local.stroke(&sk_stroke, resolution) else {
            return false;
        };
        self.verbs.clear();
        self.points.clear();
        let map = |p: sk::Point| Point::new(p.x, p.y);
        for segment in outline.segments() {
            match segment {
                sk::PathSegment::MoveTo(p) => {
                    self.verbs.push(Verb::Move);
                    self.points.push(map(p));
                }
                sk::PathSegment::LineTo(p) => {
                    self.verbs.push(Verb::Line);
                    self.points.push(map(p));
                }
                sk::PathSegment::QuadTo(c, p) => {
                    self.verbs.push(Verb::Quad);
                    self.points.extend([map(c), map(p)]);
                }
                sk::PathSegment::CubicTo(c1, c2, p) => {
                    self.verbs.push(Verb::Cubic);
                    self.points.extend([map(c1), map(c2), map(p)]);
                }
                sk::PathSegment::Close => self.verbs.push(Verb::Close),
            }
        }
        true
    }

    /// Finds, or makes and blurs, the mask of the flattened outline in
    /// `flat_verbs` and `flat_points` (device pixels relative to the
    /// mask's corner `origin`; the part from `split` on, if any, unioned
    /// with the rest by the larger coverage), and blends it within
    /// `bounds`.
    fn shadow_mask(
        &mut self,
        base: &mut [u8],
        walk: &Walk,
        (rule, d, pad): (sk::FillRule, u32, f64),
        (split, (x0, y0)): ((usize, usize), (f64, f64)),
        (shadow, bounds): (Shadow, Rect),
        cache: &mut ShadowCache,
    ) {
        let Some(outline) = crate::path::bounds_of(&self.flat_points) else {
            return;
        };
        let (mw, mh) = (
            (f64::from(outline.right()).ceil() + pad).max(0.0),
            (f64::from(outline.bottom()).ceil() + pad).max(0.0),
        );
        if !(mw >= 1.0 && mh >= 1.0 && mw <= 32_768.0 && mh <= 32_768.0) {
            return;
        }
        let (x1, y1) = (x0 + mw, y0 + mh);
        let (mw, mh) = (mw as u32, mh as u32);
        // The key: everything the mask is made from.
        let key = &mut self.shadow_key;
        key.clear();
        key.extend([
            match rule {
                sk::FillRule::Winding => 0,
                sk::FillRule::EvenOdd => 1,
            },
            d,
            mw,
            mh,
            split.0 as u32,
            split.1 as u32,
        ]);
        key.extend(self.flat_verbs.iter().map(|&v| v as u32));
        key.extend(
            self.flat_points
                .iter()
                .flat_map(|p| [p.x.to_bits(), p.y.to_bits()]),
        );
        let Skia {
            flat_verbs,
            flat_points,
            blur_work,
            blur_line,
            shadow_key,
            ..
        } = self;
        let mask = cache.get_or_make(shadow_key, || {
            let mut mask = sk::Mask::new(mw, mh)?;
            if let Some(path) = sk_path(&flat_verbs[..split.0], &flat_points[..split.1]) {
                mask.fill_path(&path, rule, true, sk::Transform::identity());
            }
            if split.0 < flat_verbs.len() {
                let mut grown = sk::Mask::new(mw, mh)?;
                if let Some(path) = sk_path(&flat_verbs[split.0..], &flat_points[split.1..]) {
                    grown.fill_path(
                        &path,
                        sk::FillRule::Winding,
                        true,
                        sk::Transform::identity(),
                    );
                }
                for (m, g) in mask.data_mut().iter_mut().zip(grown.data()) {
                    *m = (*m).max(*g);
                }
            }
            let mut data = mask.take();
            blur(&mut data, mw as usize, mh as usize, d, blur_work, blur_line);
            Some(ShadowMask {
                width: mw,
                data: data.into_boxed_slice(),
            })
        });
        let Some(mask) = mask else {
            return;
        };
        // Blended within the mask, the shadow's bounds in device pixels
        // rounded out, and the pixels this pass draws.
        let cut = walk.transform.map_rect(&bounds);
        if !cut.is_finite() {
            return;
        }
        let pixels = walk.pixels();
        let left = x0.max(f64::from(cut.x).floor()).max(f64::from(pixels.x));
        let top = y0.max(f64::from(cut.y).floor()).max(f64::from(pixels.y));
        let right = x1
            .min(f64::from(cut.right()).ceil())
            .min(pixels.right() as f64);
        let bottom = y1
            .min(f64::from(cut.bottom()).ceil())
            .min(pixels.bottom() as f64);
        if !(left < right && top < bottom) {
            return;
        }
        // Inside the target's pixels, so whole, small and not negative.
        let (left, top, right, bottom) =
            (left as usize, top as usize, right as usize, bottom as usize);
        let (mx, my) = (x0 as i64, y0 as i64);
        let color = shadow.color.premultiply().to_array();
        let width = walk.width as usize;
        let clip = walk
            .clips
            .checked_sub(1)
            .and_then(|i| self.masks.get(i))
            .map(sk::Mask::data);
        let Some(surface) = surface(&mut self.layers, base, walk.layers) else {
            return;
        };
        // The colour at each coverage, and what is kept of the pixel under
        // it, as `blend_solid` works them out, four channels at a time.
        let color = u32::from_le_bytes(color);
        let mut sources = [(0u32, 0u32); 256];
        for (c, slot) in sources.iter_mut().enumerate() {
            let source = scale_pixel(color, c as u32);
            *slot = (source, 255 - (source >> 24));
        }
        let stride = mask.width as usize;
        let (from, count) = ((left as i64 - mx) as usize, right - left);
        let blend = |pixel: &mut [u8], c: u8| {
            if c == 0 {
                return;
            }
            let (source, keep) = sources[usize::from(c)];
            let under = u32::from_le_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]);
            pixel.copy_from_slice(&(source + scale_pixel(under, keep)).to_le_bytes());
        };
        for y in top..bottom {
            let row = (y as i64 - my) as usize * stride + from;
            let coverage = &mask.data[row..row + count];
            let start = y * width + left;
            let pixels = surface[start * 4..(start + count) * 4].chunks_exact_mut(4);
            match clip {
                None => {
                    for (&c, pixel) in coverage.iter().zip(pixels) {
                        blend(pixel, c);
                    }
                }
                Some(clip) => {
                    let clip = &clip[start..start + count];
                    for ((&c, &k), pixel) in coverage.iter().zip(clip).zip(pixels) {
                        blend(pixel, mul_div_255(c, k));
                    }
                }
            }
        }
    }

    fn skip(&mut self, walk: &mut Walk, kind: Kind) {
        self.frames.push(Frame::Skip(kind));
        walk.skipping += 1;
    }

    fn push_clip(&mut self, scene: &Scene, walk: &mut Walk, shape: StoredShape) {
        walk.depth += 1;
        if walk.skipping > 0 || walk.depth > MAX_DEPTH || !walk.transform.is_finite() {
            self.skip(walk, Kind::Clip);
            return;
        }
        // A clip that draws nothing, or that cannot reach this pass's
        // damage, hides everything inside it there.
        let reaches = shape_bounds(scene, shape)
            .is_some_and(|local| walk.reaches(walk.transform.map_rect(&local)));
        if !reaches {
            self.skip(walk, Kind::Clip);
            return;
        }
        // An outline that is empty, or not finite, clips everything away.
        let Some((path, rule)) = self.shape_path(scene, walk, shape) else {
            self.skip(walk, Kind::Clip);
            return;
        };
        let level = walk.clips;
        while self.masks.len() <= level {
            match blank_mask(None, walk.width, walk.height) {
                Some(mask) => {
                    self.masks.push(mask);
                    self.mask_dirty.push(None);
                }
                None => {
                    self.skip(walk, Kind::Clip);
                    return;
                }
            }
        }
        let pixels = walk.pixels();
        let target = IRect::new(0, 0, walk.width, walk.height);
        // The pixels the outline can cover: its bounds rounded out and a
        // pixel more for anti-aliasing, within the target.
        let b = path.bounds();
        let covered = IRect::from_edges(
            (f64::from(b.left()).floor() - 1.0).max(-1.0) as i32,
            (f64::from(b.top()).floor() - 1.0).max(-1.0) as i32,
            (f64::from(b.right()).ceil() + 1.0).min(f64::from(walk.width) + 1.0) as i32,
            (f64::from(b.bottom()).ceil() + 1.0).min(f64::from(walk.height) + 1.0) as i32,
        )
        .intersect(&target);
        let (outer, inner) = self.masks.split_at_mut(level);
        let mask = &mut inner[0];
        // What the last clip at this level covered goes back to zero; the
        // rest of the mask is zero already.
        if let Some(stale) = self.mask_dirty[level].take() {
            clear(mask.data_mut(), walk.width, 1, stale);
        }
        mask.fill_path(&path, rule, true, sk::Transform::identity());
        self.mask_dirty[level] = covered;
        if let (Some(parent), Some(area)) =
            (outer.last(), covered.and_then(|c| c.intersect(&pixels)))
        {
            let (width, parent) = (walk.width as usize, parent.data());
            let data = mask.data_mut();
            let (x0, x1) = (area.x as usize, area.right() as usize);
            for y in area.y as usize..area.bottom() as usize {
                let span = y * width + x0..y * width + x1;
                for (m, p) in data[span.clone()].iter_mut().zip(&parent[span]) {
                    *m = mul_div_255(*m, *p);
                }
            }
        }
        walk.clips += 1;
        self.frames.push(Frame::Clip);
    }

    fn push_layer(&mut self, walk: &mut Walk, opacity: f32) {
        walk.depth += 1;
        if walk.skipping > 0 || walk.depth > MAX_DEPTH || opacity.is_nan() || opacity <= 0.0 {
            self.skip(walk, Kind::Layer);
            return;
        }
        if opacity >= 1.0 {
            self.frames.push(Frame::Layer {
                opacity,
                buffered: false,
            });
            return;
        }
        let level = walk.layers + 1;
        while self.layers.len() < level {
            self.layers.push(Vec::new());
        }
        let len = walk.width as usize * walk.height as usize * 4;
        let surface = &mut self.layers[level - 1];
        if surface.len() == len {
            // Only the pass's pixels are drawn and composited.
            clear(surface, walk.width, 4, walk.pixels());
        } else {
            surface.clear();
            surface.resize(len, 0);
        }
        walk.layers = level;
        self.frames.push(Frame::Layer {
            opacity,
            buffered: true,
        });
    }

    /// Draws the top layer onto the surface below it, over the pass's
    /// pixels.
    fn composite(&mut self, base: &mut [u8], walk: &Walk, opacity: f32) {
        let level = walk.layers;
        let Some(below) = level.checked_sub(1) else {
            return;
        };
        let (lower, upper) = self.layers.split_at_mut(below);
        let Some(layer) = upper.first() else {
            return;
        };
        let Some(parent) = surface(lower, base, below) else {
            return;
        };
        let (Some(layer), Some(mut parent)) = (
            sk::PixmapRef::from_bytes(layer, walk.width, walk.height),
            sk::PixmapMut::from_bytes(parent, walk.width, walk.height),
        ) else {
            return;
        };
        let pixels = walk.pixels();
        let Some(rect) = sk::Rect::from_xywh(
            pixels.x as f32,
            pixels.y as f32,
            pixels.width as f32,
            pixels.height as f32,
        ) else {
            return;
        };
        // What `draw_pixmap` does, over the pass's pixels only.
        let paint = sk::Paint {
            shader: sk::Pattern::new(
                layer,
                sk::SpreadMode::Pad,
                sk::FilterQuality::Nearest,
                opacity,
                sk::Transform::identity(),
            ),
            anti_alias: false,
            ..sk::Paint::default()
        };
        parent.fill_rect(rect, &paint, sk::Transform::identity(), None);
    }
}

/// Rasterises the coverage of a glyph at a point of the variation space,
/// at `k` device pixels per font unit, its origin `subpixel` quarters of a
/// pixel right of and below a whole pixel. A glyph with no outline (a
/// space) gets an empty mask, which is kept like any other so that it is
/// not looked for again. `outline` and `flat` are scratch buffers.
fn glyph_mask(
    face: &Face,
    (glyph, coords): (u16, Coords),
    k: f64,
    subpixel: (u8, u8),
    outline: (&mut Vec<Verb>, &mut Vec<Point>),
    flat: (&mut Vec<Verb>, &mut Vec<Point>),
) -> Option<Mask> {
    let (verbs, points) = outline;
    let (flat_verbs, flat_points) = flat;
    let (dx, dy) = (f64::from(subpixel.0) / 4.0, f64::from(subpixel.1) / 4.0);
    verbs.clear();
    points.clear();
    let map = |x: f32, y: f32| {
        Point::new(
            (f64::from(x) * k + dx) as f32,
            (dy - f64::from(y) * k) as f32,
        )
    };
    if !face.outline(glyph, coords, map, verbs, points) {
        return Some(Mask::default());
    }
    flat_verbs.clear();
    flat_points.clear();
    flatten(verbs, points, flat_verbs, flat_points);
    let Some(bounds) = crate::path::bounds_of(flat_points).filter(Rect::is_finite) else {
        return Some(Mask::default());
    };
    let left = bounds.x.floor();
    let top = bounds.y.floor();
    let width = bounds.right().ceil() - left;
    let height = bounds.bottom().ceil() - top;
    // Glyphs are at most MAX_MASK_PPEM pixels per em, so far smaller.
    if !((1.0..=4096.0).contains(&width) && (1.0..=4096.0).contains(&height)) {
        return Some(Mask::default());
    }
    for p in flat_points.iter_mut() {
        *p = Point::new(p.x - left, p.y - top);
    }
    let path = sk_path(flat_verbs, flat_points)?;
    let (width, height) = (width as u32, height as u32);
    let mut mask = sk::Mask::new(width, height)?;
    mask.fill_path(
        &path,
        sk::FillRule::Winding,
        true,
        sk::Transform::identity(),
    );
    Some(Mask {
        left: left as i32,
        top: top as i32,
        width,
        height,
        data: mask.take().into_boxed_slice(),
    })
}

/// Encodes premultiplied RGBA bytes as an RGBA PNG.
pub(crate) fn encode_png(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ImageError> {
    let pixmap = sk::PixmapRef::from_bytes(data, width, height).ok_or(ImageError::Empty)?;
    pixmap
        .encode_png()
        .map_err(|error| ImageError::Png(error.to_string()))
}

/// The most bytes a decoded image may take: 64 MiB, a 4096 by 4096 RGBA
/// image.
pub(crate) const MAX_DECODED_BYTES: u64 = 64 << 20;

/// The width and height a PNG's header announces, if it has one.
fn png_header_size(bytes: &[u8]) -> Option<(u32, u32)> {
    const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.get(..8)? != SIGNATURE || bytes.get(12..16)? != *b"IHDR" {
        return None;
    }
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    Some((word(16)?, word(20)?))
}

/// Decodes a PNG into premultiplied RGBA bytes, its width and its height.
pub(crate) fn decode_png(bytes: &[u8]) -> Result<(Vec<u8>, u32, u32), ImageError> {
    // Refuse sizes beyond our limits before the decoder allocates anything:
    // the decoder sizes its output from the header alone, and its own
    // limit applies only to the compressed data it buffers. Its output is
    // at most four bytes a pixel, after which grey and RGB are expanded to
    // RGBA in a buffer of the same size.
    if let Some((width, height)) = png_header_size(bytes) {
        let too_large = width > MAX_SIDE
            || height > MAX_SIDE
            || u64::from(width) * u64::from(height) * 4 > MAX_DECODED_BYTES;
        if too_large {
            return Err(ImageError::TooLarge { width, height });
        }
    }
    let pixmap =
        sk::Pixmap::decode_png(bytes).map_err(|error| ImageError::Png(error.to_string()))?;
    let (width, height) = (pixmap.width(), pixmap.height());
    Ok((pixmap.take(), width, height))
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::{Circle, LinearGradient, Path};

    impl Skia {
        /// `render` with caches of its own.
        fn draw(
            &mut self,
            scene: &Scene,
            target: &mut Pixmap,
            transform: Transform,
            damage: Option<&[IRect]>,
        ) {
            let caches = Caches {
                glyphs: &mut GlyphCache::default(),
                shadows: &mut ShadowCache::default(),
            };
            self.render(scene, target, transform, damage, caches);
        }
    }

    fn busy() -> Scene {
        let mut scene = Scene::new();
        scene.fill(Rect::new(0.0, 0.0, 30.0, 20.0), Color::rgb(20, 30, 40));
        scene.fill(
            Circle::new(Point::new(10.0, 10.0), 6.5),
            Color::rgba(200, 100, 0, 180),
        );
        scene.layer(crate::Layer::with_opacity(0.5), |s| {
            s.fill(Rect::new(12.5, 3.25, 9.0, 9.0), Color::WHITE);
        });
        scene
    }

    #[test]
    fn a_panic_during_a_whole_render_leaves_the_target_whole_and_the_renderer_working() {
        let scene = busy();
        let mut clean = Pixmap::new(30, 20).expect("small");
        Skia::new().draw(&scene, &mut clean, Transform::IDENTITY, None);
        for fault in 1..=3 {
            let mut raster = Skia::new();
            raster.fault = Some(fault);
            let mut target = Pixmap::new(30, 20).expect("small");
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                raster.draw(&scene, &mut target, Transform::IDENTITY, None);
            }));
            assert!(outcome.is_err(), "fault {fault} was not reached");
            // The buffer never left the target, so it keeps its length and
            // stays valid to read and to paint over.
            assert_eq!(target.data().len(), 30 * 20 * 4);
            assert!(target.pixel(29, 19).is_some());
            target.fill(Color::rgb(1, 2, 3));
            // The same renderer draws correctly afterwards, whole and in part.
            raster.fault = None;
            let mut again = Pixmap::new(30, 20).expect("small");
            raster.draw(&scene, &mut again, Transform::IDENTITY, None);
            assert_eq!(again, clean, "fault {fault}");
            let mut part = Pixmap::new(30, 20).expect("small");
            raster.draw(
                &scene,
                &mut part,
                Transform::IDENTITY,
                Some(&[IRect::new(3, 4, 20, 9)]),
            );
            assert_eq!(
                part.crop(IRect::new(3, 4, 20, 9)),
                clean.crop(IRect::new(3, 4, 20, 9))
            );
        }
    }

    #[test]
    fn a_panic_during_a_partial_render_leaves_the_target_whole() {
        let scene = busy();
        let mut raster = Skia::new();
        raster.fault = Some(2);
        let mut target = Pixmap::new(30, 20).expect("small");
        target.fill(Color::rgb(9, 9, 9));
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            raster.draw(
                &scene,
                &mut target,
                Transform::IDENTITY,
                Some(&[IRect::new(0, 0, 8, 8)]),
            );
        }));
        assert!(outcome.is_err());
        // The damage is copied back only after a pass completes.
        let mut untouched = Pixmap::new(30, 20).expect("small");
        untouched.fill(Color::rgb(9, 9, 9));
        assert_eq!(target, untouched);
    }

    #[test]
    fn commands_far_from_the_damage_are_skipped_before_any_work() {
        let mut wave = Path::new();
        for i in 0..2_000 {
            let x = i as f32 * 0.1;
            wave.line_to(Point::new(x, 50.0 + (i % 37) as f32));
        }
        let mut scene = Scene::new();
        scene.stroke(&wave, Stroke::new(1.0), Color::WHITE);
        scene.fill(Rect::new(390.0, 0.0, 10.0, 10.0), Color::WHITE);
        let mut raster = Skia::new();
        let mut target = Pixmap::new(400, 100).expect("small");
        // Damage near x 395: only the small rectangle is drawn.
        let damage: Vec<IRect> = (0..16).map(|i| IRect::new(380 + i, 2 * i, 1, 1)).collect();
        raster.draw(&scene, &mut target, Transform::IDENTITY, Some(&damage));
        assert_eq!(raster.drawn, 1);
        // Those sixteen pixels lie in two cells of the grid, side by side:
        // two passes, and the rectangle reaches only the second.
        assert_eq!(raster.passes.iter().flatten().count(), 2);
        // A whole render draws both.
        raster.drawn = 0;
        raster.draw(&scene, &mut target, Transform::IDENTITY, None);
        assert_eq!(raster.drawn, 2);
    }

    #[test]
    fn passes_whose_windows_overlap_are_merged() {
        let mut raster = Skia::new();
        raster.plan(&[
            IRect::new(0, 0, 10, 10),
            IRect::new(60, 60, 10, 10),
            IRect::new(200, 0, 10, 10),
        ]);
        // The first two windows share the cell (64, 64): one pass of two
        // cells by two; the third is on its own.
        let passes: Vec<Window> = raster.passes.iter().flatten().copied().collect();
        assert_eq!(
            passes,
            [
                Window {
                    area: IRect::new(0, 0, 128, 128),
                    cull: IRect::new(0, 0, 70, 70),
                },
                Window {
                    area: IRect::new(192, 0, 64, 64),
                    cull: IRect::new(200, 0, 10, 10),
                },
            ]
        );
        assert_eq!(raster.owners, [0, 0, 2]);
    }

    /// The points of a cut outline, as (x, y) pairs, closing included.
    fn cut_points(points: &[Point], lines: Lines) -> Vec<(f32, f32)> {
        let mut verbs = vec![Verb::Move];
        verbs.extend(std::iter::repeat_n(Verb::Line, points.len() - 1));
        let (mut out_verbs, mut out_points) = (Vec::new(), Vec::new());
        cut(&verbs, points, lines, &mut out_verbs, &mut out_points);
        assert_eq!(out_verbs.first(), Some(&Verb::Move));
        assert_eq!(out_verbs.last(), Some(&Verb::Close));
        out_points.iter().map(|p| (p.x, p.y)).collect()
    }

    #[test]
    fn lines_are_cut_where_they_cross_the_grid_and_closed() {
        let all = Lines {
            columns: (0.0, 4.0),
            rows: (0.0, 4.0),
        };
        // From (10, 10) to (138, 74): crosses x = 64 at y = 37 and x = 128
        // at y = 69, and y = 64 at x = 118 (between the two).
        let points = cut_points(&[Point::new(10.0, 10.0), Point::new(138.0, 74.0)], all);
        assert_eq!(
            points,
            [
                (10.0, 10.0),
                (64.0, 37.0),
                (118.0, 64.0),
                (128.0, 69.0),
                (138.0, 74.0),
                // Closed back to the start, cut the same way.
                (128.0, 69.0),
                (118.0, 64.0),
                (64.0, 37.0),
                (10.0, 10.0),
            ]
        );
        // Only the lines given are cut at.
        let window = Lines {
            columns: (2.0, 3.0),
            rows: (2.0, 3.0),
        };
        let points = cut_points(&[Point::new(10.0, 10.0), Point::new(138.0, 74.0)], window);
        assert_eq!(
            points,
            [
                (10.0, 10.0),
                (128.0, 69.0),
                (138.0, 74.0),
                (128.0, 69.0),
                (10.0, 10.0),
            ]
        );
    }

    #[test]
    fn every_cut_piece_lies_in_one_cell_and_on_the_original_line() {
        let all = Lines {
            columns: (0.0, 64.0),
            rows: (0.0, 64.0),
        };
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 40) as f32 / (1u64 << 24) as f32 * 600.0 - 50.0
        };
        for _ in 0..2_000 {
            let (p, q) = (Point::new(next(), next()), Point::new(next(), next()));
            let points = cut_points(&[p, q], all);
            for pair in points.windows(2) {
                let ((ax, ay), (bx, by)) = (pair[0], pair[1]);
                let cell = |v: f32, w: f32| ((v.min(w) / 64.0).floor(), (v.max(w) / 64.0).ceil());
                let (x0, x1) = cell(ax, bx);
                let (y0, y1) = cell(ay, by);
                assert!(x1 - x0 <= 1.0 && y1 - y0 <= 1.0, "{pair:?} spans cells");
            }
            for &(x, y) in &points {
                // Distance from the line through p and q.
                let (dx, dy) = (f64::from(q.x - p.x), f64::from(q.y - p.y));
                let cross = (f64::from(x - p.x) * dy - f64::from(y - p.y) * dx).abs();
                assert!(
                    cross <= 1e-3 * (dx * dx + dy * dy).sqrt().max(1.0),
                    "{x}, {y} off {p:?}-{q:?}"
                );
            }
        }
    }

    #[test]
    fn packed_scaling_rounds_as_each_channel_alone() {
        for &pixel in &[0u32, 0xFFFF_FFFF, 0x80FF_4012, 0x0102_03FE, 0xDEAD_BEEF] {
            for alpha in 0..=255u32 {
                let bytes = pixel.to_le_bytes();
                let one_by_one = bytes.map(|b| mul_div_255(b, alpha as u8));
                assert_eq!(scale_pixel(pixel, alpha).to_le_bytes(), one_by_one);
            }
        }
    }

    #[test]
    fn degenerate_gradients_become_one_colour() {
        let stops = [
            Stop::new(0.0, Color::rgba(0, 0, 0, 0)),
            Stop::new(0.5, Color::rgba(255, 255, 255, 255)),
        ];
        // 0 to 0.5: from transparent to white, mean half; 0.5 to 1: white.
        assert_eq!(average_color(&stops), Color::rgba(191, 191, 191, 191));
        // A stop at 0.25 holds before it; offsets out of order are raised.
        let stops = [
            Stop::new(0.25, Color::rgb(100, 0, 0)),
            Stop::new(0.1, Color::rgb(200, 0, 0)),
        ];
        // 100 to 0.25, then 100..200 over nothing, then 200 to the end.
        assert_eq!(average_color(&stops), Color::rgb(175, 0, 0));
        let mut scene = Scene::new();
        let paint = |extend| LinearGradient {
            start: Point::new(3.0, 3.0),
            end: Point::new(3.0, 3.0 + 1.0 / 32_768.0),
            stops: &stops,
            extend,
        };
        scene.fill(Rect::new(0.0, 0.0, 1.0, 1.0), paint(Extend::Pad));
        scene.fill(Rect::new(1.0, 0.0, 1.0, 1.0), paint(Extend::Repeat));
        let mut target = Pixmap::new(2, 1).expect("small");
        Skia::new().draw(&scene, &mut target, Transform::IDENTITY, None);
        assert_eq!(
            target.pixel(0, 0).map(|p| p.to_array()),
            Some([200, 0, 0, 255])
        );
        assert_eq!(
            target.pixel(1, 0).map(|p| p.to_array()),
            Some([175, 0, 0, 255])
        );
    }
}
