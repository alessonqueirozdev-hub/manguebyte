//! The display list a widget records.

mod bounds;
mod dump;

pub(crate) use bounds::{shadow_bounds, shape_bounds};

use std::ops::Range;

use crate::color::Color;
use crate::geom::{Arc, Circle, Point, Rect, RoundedRect, Transform};
use crate::image::{Image, Sampling};
use crate::paint::{Extend, Paint, Stop, Stroke};
use crate::path::{self, FillRule, Path, Verb};
use crate::shadow::Shadow;
use crate::text::TextLayout;
use crate::text::font::Coords;

/// How deep clips and layers may nest, counted together.
///
/// Each level needs a buffer the size of the area being drawn, so the depth
/// is bounded. Anything drawn inside a clip or layer nested deeper than
/// this is skipped; the scene still records and dumps it.
pub const MAX_DEPTH: usize = 16;

/// A shape to fill, stroke or clip to.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape<'a> {
    /// A rectangle.
    Rect(Rect),
    /// A rectangle with rounded corners.
    RoundedRect(RoundedRect),
    /// A circle.
    Circle(Circle),
    /// An arc of a circle.
    Arc(Arc),
    /// Any outline.
    Path(&'a Path),
}

impl From<Rect> for Shape<'_> {
    fn from(rect: Rect) -> Self {
        Shape::Rect(rect)
    }
}

impl From<RoundedRect> for Shape<'_> {
    fn from(rect: RoundedRect) -> Self {
        Shape::RoundedRect(rect)
    }
}

impl From<Circle> for Shape<'_> {
    fn from(circle: Circle) -> Self {
        Shape::Circle(circle)
    }
}

impl From<Arc> for Shape<'_> {
    fn from(arc: Arc) -> Self {
        Shape::Arc(arc)
    }
}

impl<'a> From<&'a Path> for Shape<'a> {
    fn from(path: &'a Path) -> Self {
        Shape::Path(path)
    }
}

/// How a group of drawing is composited onto what lies below it.
///
/// More options (blend modes, blur, shadows) may be added, so a layer is
/// made with a constructor.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Layer {
    /// The opacity of the group as a whole, from 0 to 1. At 0 (or `NaN`)
    /// nothing inside is drawn; at 1 the group is drawn directly, with no
    /// buffer of its own.
    pub opacity: f32,
}

impl Layer {
    /// A group drawn at `opacity`.
    pub const fn with_opacity(opacity: f32) -> Self {
        Layer { opacity }
    }
}

/// A range of one of the scene's arenas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Span {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

impl Span {
    fn after(start: usize, end: usize) -> Span {
        Span {
            start,
            len: end - start,
        }
    }

    pub(crate) fn range(self) -> Range<usize> {
        self.start..self.start + self.len
    }

    fn shifted(self, by: usize) -> Span {
        Span {
            start: self.start + by,
            len: self.len,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StoredShape {
    Rect(Rect),
    RoundedRect(RoundedRect),
    Circle(Circle),
    Arc(Arc),
    Path {
        verbs: Span,
        points: Span,
        rule: FillRule,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StoredPaint {
    Solid(Color),
    Linear {
        start: Point,
        end: Point,
        stops: Span,
        extend: Extend,
    },
    Radial {
        center: Point,
        radius: f32,
        stops: Span,
        extend: Extend,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Command {
    Fill {
        shape: StoredShape,
        paint: StoredPaint,
    },
    Stroke {
        shape: StoredShape,
        stroke: Stroke,
        paint: StoredPaint,
    },
    Image {
        image: usize,
        to: Rect,
        sampling: Sampling,
    },
    /// `values` holds the lows, then the highs, one of each per column.
    Columns {
        area: Rect,
        values: Span,
        paint: StoredPaint,
    },
    /// A soft shadow of the shape filled, or of its outline drawn with
    /// `stroke`.
    Shadow {
        shape: StoredShape,
        stroke: Option<Stroke>,
        shadow: Shadow,
    },
    PushClip(StoredShape),
    PopClip,
    PushTransform(Transform),
    PopTransform,
    PushLayer(Layer),
    PopLayer,
    /// Shaped glyphs of the embedded typeface: `text` in the string arena
    /// (for the dump), `glyphs` in the glyph arena, positioned in the
    /// scene's coordinates.
    Text {
        text: Span,
        glyphs: Span,
        size: f32,
        weight: f32,
        optical_size: f32,
        coords: Coords,
        color: Color,
        /// A box holding the glyphs' outlines, control points included, at
        /// the positions in `glyphs` (see [`Scene::text`]).
        ink: Option<Rect>,
    },
}

/// A glyph as a scene stores it: its number and its origin on the
/// baseline, in the scene's coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StoredGlyph {
    pub(crate) id: u16,
    pub(crate) position: Point,
}

/// How far beyond the box of its glyphs' outlines text may change pixels,
/// in the text's own units: glyphs drawn from cached masks are placed to a
/// quarter of a device pixel, which moves them by at most an eighth of a
/// pixel, and masks are used only at scales of a half or more (see the
/// rasteriser), so the move is at most a quarter of a unit.
pub(crate) const TEXT_TOLERANCE: f32 = 0.5;

/// A display list: what a widget draws, recorded in order.
///
/// A scene holds commands, not pixels. A [`Renderer`](crate::Renderer)
/// turns it into pixels at any scale, as often as needed, and
/// [`Scene::dump`] turns it into text that tests compare exactly.
///
/// Coordinates are logical pixels. [`clipped`](Scene::clipped),
/// [`transformed`](Scene::transformed) and [`layer`](Scene::layer) take a
/// closure that records what they apply to, so every clip, transform and
/// layer is closed where it was opened.
///
/// Recording copies paths, gradient stops and column values into the
/// scene's own memory, which [`clear`](Scene::clear) keeps: re-recording a
/// scene of the same size does not allocate.
///
/// ```
/// use manguebyte_gfx::{Circle, Color, Layer, Point, Rect, Scene, Stroke};
///
/// let mut scene = Scene::new();
/// scene.fill(Rect::new(0.0, 0.0, 40.0, 40.0), Color::hex(0x202020));
/// scene.layer(Layer::with_opacity(0.5), |scene| {
///     scene.stroke(
///         Circle::new(Point::new(20.0, 20.0), 12.0),
///         Stroke::new(2.0),
///         Color::WHITE,
///     );
/// });
/// assert_eq!(
///     scene.dump(),
///     "fill rect(0 0 40 40) solid(#202020ff)\n\
///      layer opacity 0.5 {\n\
///      \x20 stroke circle(20 20 12) width 2 cap butt join miter 4 solid(#ffffffff)\n\
///      }\n"
/// );
/// assert_eq!(scene.bounds(), Some(Rect::new(0.0, 0.0, 40.0, 40.0)));
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Scene {
    pub(crate) commands: Vec<Command>,
    pub(crate) verbs: Vec<Verb>,
    pub(crate) points: Vec<Point>,
    pub(crate) stops: Vec<Stop>,
    pub(crate) values: Vec<f32>,
    pub(crate) images: Vec<Image>,
    pub(crate) glyphs: Vec<StoredGlyph>,
    pub(crate) strings: String,
}

impl Scene {
    /// An empty scene.
    pub fn new() -> Self {
        Scene::default()
    }

    /// Removes every command, keeping the memory.
    pub fn clear(&mut self) {
        self.commands.clear();
        self.verbs.clear();
        self.points.clear();
        self.stops.clear();
        self.values.clear();
        self.images.clear();
        self.glyphs.clear();
        self.strings.clear();
    }

    /// The number of commands recorded, counting the start and the end of
    /// each clip, transform and layer as one each.
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Whether nothing has been recorded.
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    fn store_shape(&mut self, shape: Shape<'_>) -> StoredShape {
        match shape {
            Shape::Rect(r) => StoredShape::Rect(r),
            Shape::RoundedRect(r) => StoredShape::RoundedRect(r),
            Shape::Circle(c) => StoredShape::Circle(c),
            Shape::Arc(a) => StoredShape::Arc(a),
            Shape::Path(p) => {
                let verbs = self.verbs.len();
                let points = self.points.len();
                self.verbs.extend_from_slice(p.verbs());
                self.points.extend_from_slice(p.points());
                StoredShape::Path {
                    verbs: Span::after(verbs, self.verbs.len()),
                    points: Span::after(points, self.points.len()),
                    rule: p.fill_rule(),
                }
            }
        }
    }

    fn store_stops(&mut self, stops: &[Stop]) -> Span {
        let start = self.stops.len();
        self.stops.extend_from_slice(stops);
        Span::after(start, self.stops.len())
    }

    fn store_paint(&mut self, paint: Paint<'_>) -> StoredPaint {
        match paint {
            Paint::Solid(color) => StoredPaint::Solid(color),
            Paint::Linear(g) => StoredPaint::Linear {
                start: g.start,
                end: g.end,
                stops: self.store_stops(g.stops),
                extend: g.extend,
            },
            Paint::Radial(g) => StoredPaint::Radial {
                center: g.center,
                radius: g.radius,
                stops: self.store_stops(g.stops),
                extend: g.extend,
            },
        }
    }

    /// Fills `shape` with `paint`.
    ///
    /// Open subpaths are closed with a straight line for filling.
    pub fn fill<'s, 'p>(&mut self, shape: impl Into<Shape<'s>>, paint: impl Into<Paint<'p>>) {
        let shape = self.store_shape(shape.into());
        let paint = self.store_paint(paint.into());
        self.commands.push(Command::Fill { shape, paint });
    }

    /// Draws the outline of `shape` with `stroke`, coloured by `paint`.
    pub fn stroke<'s, 'p>(
        &mut self,
        shape: impl Into<Shape<'s>>,
        stroke: Stroke,
        paint: impl Into<Paint<'p>>,
    ) {
        let shape = self.store_shape(shape.into());
        let paint = self.store_paint(paint.into());
        self.commands.push(Command::Stroke {
            shape,
            stroke,
            paint,
        });
    }

    /// Draws a soft shadow of `shape`, filled: the shape moved, grown and
    /// blurred as `shadow` says (see [`Shadow`]). Record it before the
    /// shape to put it under the shape.
    ///
    /// The renderer blurs the shadow's mask once and keeps it, so drawing
    /// the same shadow again costs only blending it (see
    /// [`Renderer::shadow_cache`](crate::Renderer::shadow_cache)).
    pub fn shadow<'s>(&mut self, shape: impl Into<Shape<'s>>, shadow: Shadow) {
        let shape = self.store_shape(shape.into());
        self.commands.push(Command::Shadow {
            shape,
            stroke: None,
            shadow,
        });
    }

    /// Draws a soft shadow of the outline of `shape` drawn with `stroke`:
    /// the glow of a knob's arc or of a curve.
    ///
    /// ```
    /// use manguebyte_gfx::{Arc, Color, Point, Scene, Shadow, Stroke};
    ///
    /// let arc = Arc::new(Point::new(26.0, 26.0), 20.0, 2.5, 2.0);
    /// let accent = Color::hex(0xFF7A3D);
    /// let mut scene = Scene::new();
    /// scene.stroke_shadow(arc, Stroke::round(3.0), Shadow::glow(8.0, accent.with_alpha(0x99)));
    /// scene.stroke(arc, Stroke::round(3.0), accent);
    /// assert!(scene.dump().starts_with("shadow arc(26 26 20 2.5 2) width 3 cap round"));
    /// ```
    pub fn stroke_shadow<'s>(
        &mut self,
        shape: impl Into<Shape<'s>>,
        stroke: Stroke,
        shadow: Shadow,
    ) {
        let shape = self.store_shape(shape.into());
        self.commands.push(Command::Shadow {
            shape,
            stroke: Some(stroke),
            shadow,
        });
    }

    /// Draws `image` stretched to fill `to`, sampled with `sampling`.
    pub fn image(&mut self, image: &Image, to: Rect, sampling: Sampling) {
        let index = self.images.len();
        self.images.push(image.clone());
        self.commands.push(Command::Image {
            image: index,
            to,
            sampling,
        });
    }

    /// Draws one vertical span per column of `area`: the primitive for
    /// waveforms, spectra and meters.
    ///
    /// `low[i]` and `high[i]` are the bottom and top of entry `i`, as
    /// fractions of the area's height measured from its bottom edge: 0 is
    /// the bottom, 1 the top. Values are clamped to `0 ..= 1`, `NaN` counts
    /// as 0, and a low above its high is swapped with it. If the slices
    /// differ in length, the extra values are ignored.
    ///
    /// The entries are spread evenly across the area's width. Each device
    /// pixel column draws one span: from the lowest low to the highest high
    /// of the entries it overlaps. With more entries than columns, a column
    /// therefore shows the envelope of its entries, which is what a
    /// waveform overview needs; with fewer, neighbouring columns repeat an
    /// entry. The span's ends and the area's left and right edges are
    /// anti-aliased by exact coverage, and the cost is proportional to the
    /// area drawn plus the number of entries, never to the number of edges
    /// a path through the same points would have.
    ///
    /// A column whose low equals its high draws nothing. Under a rotation
    /// or a skew there are no device columns to follow, and each entry is
    /// drawn as its own rectangle instead.
    pub fn columns<'p>(
        &mut self,
        area: Rect,
        low: &[f32],
        high: &[f32],
        paint: impl Into<Paint<'p>>,
    ) {
        let count = low.len().min(high.len());
        let start = self.values.len();
        let clean = |v: f32| if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
        self.values.extend(
            low[..count]
                .iter()
                .zip(&high[..count])
                .map(|(&l, &h)| clean(l).min(clean(h))),
        );
        self.values.extend(
            low[..count]
                .iter()
                .zip(&high[..count])
                .map(|(&l, &h)| clean(l).max(clean(h))),
        );
        let values = Span::after(start, self.values.len());
        let paint = self.store_paint(paint.into());
        self.commands.push(Command::Columns {
            area,
            values,
            paint,
        });
    }

    /// Draws the glyphs of `layout` with the layout's top-left corner at
    /// `origin`, in the colour of its style.
    ///
    /// The scene copies the glyphs and the text (which appears in the
    /// dump), so the layout can be dropped or changed afterwards, and
    /// re-recording into a cleared scene does not allocate once its memory
    /// has grown to fit.
    ///
    /// ```
    /// use manguebyte_gfx::{Point, Scene, TextLayout, TextStyle};
    ///
    /// let label = TextLayout::new("Mix", &TextStyle::new(12.0), None);
    /// let mut scene = Scene::new();
    /// scene.text(&label, Point::new(4.0, 2.0));
    /// assert!(scene.dump().starts_with("text \"Mix\" size 12 weight 400 opsz 14 solid(#000000ff) ["));
    /// assert!(scene.bounds().is_some());
    /// ```
    pub fn text(&mut self, layout: &TextLayout, origin: Point) {
        let start = self.strings.len();
        self.strings.push_str(layout.text());
        let text = Span::after(start, self.strings.len());
        let start = self.glyphs.len();
        // Each position is rounded to `f32` once moved, which far from the
        // origin moves a glyph by up to half a unit of the last place. The
        // ink box grows by the most any glyph moved, and is rounded
        // outwards, so it still holds every outline as it is drawn.
        let (mut moved_x, mut moved_y) = (0.0f64, 0.0f64);
        for glyph in layout.glyphs() {
            let position = glyph.position + origin;
            let exact = |at: f32, by: f32| f64::from(at) + f64::from(by);
            moved_x =
                moved_x.max((f64::from(position.x) - exact(glyph.position.x, origin.x)).abs());
            moved_y =
                moved_y.max((f64::from(position.y) - exact(glyph.position.y, origin.y)).abs());
            self.glyphs.push(StoredGlyph {
                id: glyph.id,
                position,
            });
        }
        let glyphs = Span::after(start, self.glyphs.len());
        let ink = layout.ink_bounds().map(|ink| {
            let (x, y) = (f64::from(origin.x), f64::from(origin.y));
            Rect::enclosing(
                f64::from(ink.x) + x - moved_x,
                f64::from(ink.y) + y - moved_y,
                f64::from(ink.right()) + x + moved_x,
                f64::from(ink.bottom()) + y + moved_y,
            )
        });
        self.commands.push(Command::Text {
            text,
            glyphs,
            size: layout.used_size(),
            weight: layout.used_weight(),
            optical_size: layout.used_optical_size(),
            coords: layout.coords(),
            color: layout.style().color,
            ink,
        });
    }

    /// Records `draw` clipped to `shape`: only the parts inside the shape
    /// are drawn. The clip's edge is anti-aliased.
    pub fn clipped<'s>(&mut self, shape: impl Into<Shape<'s>>, draw: impl FnOnce(&mut Scene)) {
        let shape = self.store_shape(shape.into());
        self.commands.push(Command::PushClip(shape));
        draw(self);
        self.commands.push(Command::PopClip);
    }

    /// Records `draw` with its coordinates mapped by `by`, then by whatever
    /// transform is already in effect.
    pub fn transformed(&mut self, by: Transform, draw: impl FnOnce(&mut Scene)) {
        self.commands.push(Command::PushTransform(by));
        draw(self);
        self.commands.push(Command::PopTransform);
    }

    /// Records `draw` as a group composited as a whole, so overlapping
    /// parts of a half-transparent group do not show through each other.
    pub fn layer(&mut self, layer: Layer, draw: impl FnOnce(&mut Scene)) {
        self.commands.push(Command::PushLayer(layer));
        draw(self);
        self.commands.push(Command::PopLayer);
    }

    /// Records everything in `other` after what is already here, as if it
    /// had been recorded into this scene.
    pub fn append(&mut self, other: &Scene) {
        let verbs = self.verbs.len();
        let points = self.points.len();
        let stops = self.stops.len();
        let values = self.values.len();
        let images = self.images.len();
        let glyphs = self.glyphs.len();
        let strings = self.strings.len();
        self.glyphs.extend_from_slice(&other.glyphs);
        self.strings.push_str(&other.strings);
        self.verbs.extend_from_slice(&other.verbs);
        self.points.extend_from_slice(&other.points);
        self.stops.extend_from_slice(&other.stops);
        self.values.extend_from_slice(&other.values);
        self.images.extend_from_slice(&other.images);
        let shape = |s: StoredShape| match s {
            StoredShape::Path {
                verbs: v,
                points: p,
                rule,
            } => StoredShape::Path {
                verbs: v.shifted(verbs),
                points: p.shifted(points),
                rule,
            },
            other => other,
        };
        let paint = |p: StoredPaint| match p {
            StoredPaint::Solid(c) => StoredPaint::Solid(c),
            StoredPaint::Linear {
                start,
                end,
                stops: s,
                extend,
            } => StoredPaint::Linear {
                start,
                end,
                stops: s.shifted(stops),
                extend,
            },
            StoredPaint::Radial {
                center,
                radius,
                stops: s,
                extend,
            } => StoredPaint::Radial {
                center,
                radius,
                stops: s.shifted(stops),
                extend,
            },
        };
        self.commands
            .extend(other.commands.iter().map(|&command| match command {
                Command::Fill { shape: s, paint: p } => Command::Fill {
                    shape: shape(s),
                    paint: paint(p),
                },
                Command::Stroke {
                    shape: s,
                    stroke,
                    paint: p,
                } => Command::Stroke {
                    shape: shape(s),
                    stroke,
                    paint: paint(p),
                },
                Command::Image {
                    image,
                    to,
                    sampling,
                } => Command::Image {
                    image: image + images,
                    to,
                    sampling,
                },
                Command::Columns {
                    area,
                    values: v,
                    paint: p,
                } => Command::Columns {
                    area,
                    values: v.shifted(values),
                    paint: paint(p),
                },
                Command::PushClip(s) => Command::PushClip(shape(s)),
                Command::Shadow {
                    shape: s,
                    stroke,
                    shadow,
                } => Command::Shadow {
                    shape: shape(s),
                    stroke,
                    shadow,
                },
                Command::Text {
                    text,
                    glyphs: g,
                    size,
                    weight,
                    optical_size,
                    coords,
                    color,
                    ink,
                } => Command::Text {
                    text: text.shifted(strings),
                    glyphs: g.shifted(glyphs),
                    size,
                    weight,
                    optical_size,
                    coords,
                    color,
                    ink,
                },
                other => other,
            }));
    }

    /// The scene as text, one command per line, exact and the same on every
    /// platform.
    ///
    /// Numbers are printed in the shortest form that reads back as the same
    /// `f32`, and colours as `#rrggbbaa`. The contents of clips, transforms
    /// and layers are indented inside braces. Paths of more than 32
    /// segments, column data of more than 8 entries and images are printed
    /// as their size and a 64-bit FNV-1a hash of their contents, so the
    /// dump stays short and still changes whenever they do.
    ///
    /// Text is printed with its string (escaped, and cut after 64
    /// characters), the size, weight and optical size used, its colour and
    /// every glyph as its number and its position, `[id x y, ...]`; more
    /// than 64 glyphs are printed as their count, their box of positions
    /// and a hash.
    pub fn dump(&self) -> String {
        dump::dump(self)
    }

    /// A rectangle, in the scene's coordinates, that contains everything
    /// the scene draws, or `None` if it draws nothing.
    ///
    /// It is conservative: strokes count their full
    /// [`reach`](Stroke::reach) (mitred corners and square caps included)
    /// plus half a unit of the shape's coordinates and 1/1024 of the reach,
    /// more than the rasteriser's approximation of round caps, round joins
    /// and curved outlines ever adds; curves count their control points,
    /// arcs their whole circle, and clips cut it down to their own bounds.
    /// Text counts the box of its glyphs' outlines, control points
    /// included, at the positions recorded (the box is rounded outwards,
    /// and grows by as much as rounding a position to `f32` moved a glyph,
    /// which matters far from the origin), plus half a unit, more than
    /// placing glyphs on a quarter of a device pixel ever moves them.
    /// Every pixel the scene changes, at any scale, lies inside these bounds
    /// mapped to device pixels and rounded out, so damage built from them
    /// leaves nothing stale. Commands with coordinates that are not finite
    /// draw nothing and are left out.
    pub fn bounds(&self) -> Option<Rect> {
        bounds::bounds(self)
    }

    /// Appends the outline of `shape` to `verbs` and `points`, and returns
    /// the rule to fill it with.
    pub(crate) fn outline(
        &self,
        shape: StoredShape,
        verbs: &mut Vec<Verb>,
        points: &mut Vec<Point>,
    ) -> FillRule {
        match shape {
            // An empty rectangle draws nothing, filled, stroked or as a clip.
            StoredShape::Rect(r) => {
                if !r.is_empty() {
                    path::push_rect(verbs, points, r);
                }
            }
            StoredShape::RoundedRect(r) => {
                if !r.rect.is_empty() {
                    path::push_rounded_rect(verbs, points, r);
                }
            }
            StoredShape::Circle(c) => {
                if c.radius > 0.0 {
                    path::push_circle(verbs, points, c);
                }
            }
            StoredShape::Arc(a) => path::push_arc(verbs, points, a),
            StoredShape::Path {
                verbs: v,
                points: p,
                rule,
            } => {
                verbs.extend_from_slice(&self.verbs[v.range()]);
                points.extend_from_slice(&self.points[p.range()]);
                return rule;
            }
        }
        FillRule::NonZero
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appending_equals_recording_in_one_scene() {
        let mut triangle = Path::new();
        triangle
            .move_to(Point::new(0.0, 0.0))
            .line_to(Point::new(4.0, 0.0))
            .line_to(Point::new(0.0, 4.0))
            .close();
        let stops = [Stop::new(0.0, Color::BLACK), Stop::new(1.0, Color::WHITE)];
        let gradient = crate::LinearGradient {
            start: Point::ZERO,
            end: Point::new(4.0, 0.0),
            stops: &stops,
            extend: Extend::Pad,
        };
        let image = Image::new(crate::Pixmap::new(1, 1).expect("small"));
        let label = TextLayout::new("Gain", &crate::TextStyle::new(10.0), None);
        let record = |scene: &mut Scene| {
            scene.fill(&triangle, gradient);
            scene.clipped(&triangle, |s| {
                s.columns(
                    Rect::new(0.0, 0.0, 4.0, 4.0),
                    &[0.0, 0.1],
                    &[1.0, 0.5],
                    gradient,
                );
                s.image(&image, Rect::new(0.0, 0.0, 1.0, 1.0), Sampling::Nearest);
                s.text(&label, Point::new(1.0, 2.0));
            });
        };
        let mut whole = Scene::new();
        record(&mut whole);
        record(&mut whole);
        let mut first = Scene::new();
        record(&mut first);
        let mut joined = first.clone();
        joined.append(&first);
        assert_eq!(joined, whole);
        assert_eq!(joined.dump(), whole.dump());
    }

    #[test]
    fn column_values_are_cleaned_when_recorded() {
        let mut scene = Scene::new();
        scene.columns(
            Rect::new(0.0, 0.0, 1.0, 1.0),
            &[0.5, f32::NAN, -1.0, 0.25],
            &[0.25, 2.0, 0.5],
            Color::WHITE,
        );
        // Three entries: lows then highs, swapped and clamped.
        assert_eq!(scene.values, vec![0.25, 0.0, 0.0, 0.5, 1.0, 0.5]);
    }

    #[test]
    fn clearing_keeps_nothing_but_memory() {
        let mut scene = Scene::new();
        scene.fill(Rect::new(0.0, 0.0, 1.0, 1.0), Color::WHITE);
        scene.clear();
        assert!(scene.is_empty());
        assert_eq!(scene.dump(), "");
        assert_eq!(scene.bounds(), None);
    }
}
