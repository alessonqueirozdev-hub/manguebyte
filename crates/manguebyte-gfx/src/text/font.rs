//! The embedded typeface, read through `skrifa`.
//!
//! This is the only module that names `skrifa`. It gives the rest of the
//! crate what text needs from the font file in the crate's own types:
//! units per em, vertical metrics, normalised variation coordinates, glyph
//! outlines and their control boxes, all at a point of the variation
//! space, all in font units (y up).

use skrifa::instance::{LocationRef, NormalizedCoord, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use skrifa::{FontRef, GlyphId, MetadataProvider};

use crate::geom::Point;
use crate::path::Verb;

/// The embedded typeface: a subset of Inter 4.1 (see `fonts/README.md`).
pub(crate) const FONT: &[u8] = include_bytes!("../../fonts/InterVariable-Latin.ttf");

/// The number of variation axes of the embedded typeface.
pub(crate) const AXES: usize = 2;

/// A point of the typeface's variation space: one normalised coordinate
/// per axis, in the font's axis order (optical size, then weight), as the
/// bits of an `F2Dot14`. Being integers, they key caches exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct Coords(pub(crate) [i16; AXES]);

/// Vertical metrics in font units, positive upwards except `descent`,
/// which is given as a positive distance below the baseline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Vertical {
    pub(crate) ascent: f32,
    pub(crate) descent: f32,
    pub(crate) line_gap: f32,
    pub(crate) cap_height: f32,
    pub(crate) x_height: f32,
}

/// The embedded typeface, ready to read.
pub(crate) struct Face {
    font: FontRef<'static>,
}

impl Face {
    /// The embedded typeface. Reading it only checks its table directory;
    /// tables are read when used.
    pub(crate) fn embedded() -> Face {
        // The bytes are part of this crate and the tests read every table
        // used here, so this cannot fail at run time.
        let font = FontRef::new(FONT).expect("the embedded typeface has a valid table directory");
        Face { font }
    }

    /// Font units per em: 2048 for Inter.
    pub(crate) fn units_per_em(&self) -> u16 {
        self.font
            .head()
            .map(|head| head.units_per_em())
            .unwrap_or(2048)
    }

    /// The normalised coordinates for a weight and an optical size in the
    /// axes' own units, each clamped to its axis's range first.
    pub(crate) fn coords(&self, weight: f32, optical_size: f32) -> Coords {
        let location = self
            .font
            .axes()
            .location([("wght", weight), ("opsz", optical_size)]);
        let mut coords = Coords::default();
        for (slot, coord) in coords.0.iter_mut().zip(location.coords()) {
            *slot = coord.to_bits();
        }
        coords
    }

    /// The range of an axis, `(min, max)`, by tag.
    #[cfg(test)]
    pub(crate) fn axis_range(&self, tag: &str) -> Option<(f32, f32)> {
        let tag = skrifa::Tag::new_checked(tag.as_bytes()).ok()?;
        self.font
            .axes()
            .get_by_tag(tag)
            .map(|axis| (axis.min_value(), axis.max_value()))
    }

    fn location(coords: &Coords) -> [NormalizedCoord; AXES] {
        coords.0.map(NormalizedCoord::from_bits)
    }

    /// The vertical metrics at `coords`, with the font's metric variations
    /// applied.
    pub(crate) fn vertical(&self, coords: Coords) -> Vertical {
        let location = Self::location(&coords);
        let metrics = self
            .font
            .metrics(Size::unscaled(), LocationRef::new(&location));
        Vertical {
            ascent: metrics.ascent,
            descent: -metrics.descent,
            line_gap: metrics.leading,
            cap_height: metrics.cap_height.unwrap_or(metrics.ascent),
            x_height: metrics.x_height.unwrap_or(metrics.ascent),
        }
    }

    /// The glyph the font's character map gives `c`, if any.
    #[cfg(test)]
    pub(crate) fn glyph_for(&self, c: char) -> Option<u16> {
        let id = self.font.charmap().map(c)?;
        u16::try_from(id.to_u32()).ok()
    }

    /// Appends the outline of `glyph` at `coords`, each point mapped by
    /// `map` from font units, to `verbs` and `points`. Returns `false`, and
    /// appends nothing, if the font has no such glyph or cannot draw it.
    pub(crate) fn outline(
        &self,
        glyph: u16,
        coords: Coords,
        map: impl Fn(f32, f32) -> Point,
        verbs: &mut Vec<Verb>,
        points: &mut Vec<Point>,
    ) -> bool {
        let Some(outline) = self
            .font
            .outline_glyphs()
            .get(GlyphId::new(u32::from(glyph)))
        else {
            return false;
        };
        let (verbs_before, points_before) = (verbs.len(), points.len());
        let location = Self::location(&coords);
        let settings = DrawSettings::unhinted(Size::unscaled(), LocationRef::new(&location));
        let mut pen = Pen {
            map,
            verbs: &mut *verbs,
            points: &mut *points,
        };
        if outline.draw(settings, &mut pen).is_err() {
            verbs.truncate(verbs_before);
            points.truncate(points_before);
            return false;
        }
        true
    }

    /// The box of every point of `glyph`'s outline at `coords`, control
    /// points included, as `[x_min, y_min, x_max, y_max]` in font units, or
    /// `None` for a glyph with no outline (a space).
    pub(crate) fn control_box(&self, glyph: u16, coords: Coords) -> Option<[f32; 4]> {
        let outline = self
            .font
            .outline_glyphs()
            .get(GlyphId::new(u32::from(glyph)))?;
        let location = Self::location(&coords);
        let settings = DrawSettings::unhinted(Size::unscaled(), LocationRef::new(&location));
        let mut pen = BoxPen(None);
        outline.draw(settings, &mut pen).ok()?;
        pen.0
    }
}

/// Collects an outline as verbs and points, mapping every point.
struct Pen<'a, F> {
    map: F,
    verbs: &'a mut Vec<Verb>,
    points: &'a mut Vec<Point>,
}

impl<F: Fn(f32, f32) -> Point> OutlinePen for Pen<'_, F> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::Move);
        self.points.push((self.map)(x, y));
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.verbs.push(Verb::Line);
        self.points.push((self.map)(x, y));
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.verbs.push(Verb::Quad);
        self.points.push((self.map)(cx0, cy0));
        self.points.push((self.map)(x, y));
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.verbs.push(Verb::Cubic);
        self.points.push((self.map)(cx0, cy0));
        self.points.push((self.map)(cx1, cy1));
        self.points.push((self.map)(x, y));
    }

    fn close(&mut self) {
        self.verbs.push(Verb::Close);
    }
}

/// Collects the box of every point it is given.
struct BoxPen(Option<[f32; 4]>);

impl BoxPen {
    fn add(&mut self, x: f32, y: f32) {
        let b = self.0.get_or_insert([x, y, x, y]);
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    }
}

impl OutlinePen for BoxPen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn line_to(&mut self, x: f32, y: f32) {
        self.add(x, y);
    }

    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.add(cx0, cy0);
        self.add(x, y);
    }

    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.add(cx0, cy0);
        self.add(cx1, cy1);
        self.add(x, y);
    }

    fn close(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_typeface_reads_and_has_its_two_axes() {
        let face = Face::embedded();
        assert_eq!(face.units_per_em(), 2048);
        assert_eq!(face.axis_range("wght"), Some((100.0, 900.0)));
        assert_eq!(face.axis_range("opsz"), Some((14.0, 32.0)));
        assert_eq!(face.axis_range("wdth"), None);
        // The default point is all zero; the ends of an axis are -1 and 1
        // (F2Dot14 bits -16384 and 16384).
        assert_eq!(face.coords(400.0, 14.0), Coords([0, 0]));
        assert_eq!(face.coords(900.0, 32.0), Coords([16384, 16384]));
        assert_eq!(face.coords(100.0, 14.0), Coords([0, -16384]));
        // Out of range values are clamped.
        assert_eq!(face.coords(5000.0, 2.0), Coords([0, 16384]));
    }

    #[test]
    fn outlines_and_their_boxes_agree() {
        let face = Face::embedded();
        let a = face.glyph_for('A').expect("A is mapped");
        let (mut verbs, mut points) = (Vec::new(), Vec::new());
        assert!(face.outline(a, Coords::default(), Point::new, &mut verbs, &mut points));
        assert!(!verbs.is_empty());
        let b = face
            .control_box(a, Coords::default())
            .expect("A has an outline");
        for p in &points {
            assert!(p.x >= b[0] && p.x <= b[2] && p.y >= b[1] && p.y <= b[3]);
        }
        // A space has no outline.
        let space = face.glyph_for(' ').expect("space is mapped");
        assert_eq!(face.control_box(space, Coords::default()), None);
        // A glyph id past the end draws nothing and appends nothing.
        let before = verbs.len();
        assert!(!face.outline(
            u16::MAX,
            Coords::default(),
            Point::new,
            &mut verbs,
            &mut points
        ));
        assert_eq!(verbs.len(), before);
    }
}
