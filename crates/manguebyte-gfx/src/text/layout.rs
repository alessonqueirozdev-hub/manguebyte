//! Laying out one paragraph: shaping, breaking into lines, alignment, the
//! ellipsis, and the map between positions and character indices.

use std::collections::HashMap;
use std::ops::Range;

use super::font::{Coords, Face};
use super::shape::{Shaped, Shaper};
use super::{Align, FontMetrics, TextStyle};
use crate::color::Color;
use crate::geom::{Point, Rect, Size};

/// One glyph of a [`TextLayout`].
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Glyph {
    /// The glyph's number in the typeface.
    pub id: u16,
    /// Where its origin goes: the left end of its baseline, in the
    /// layout's coordinates (the layout's top-left corner is the origin).
    pub position: Point,
    /// The byte index, in the layout's text, of the first character the
    /// glyph draws. The ellipsis has the index of the first character it
    /// replaces.
    pub index: usize,
}

/// One line of a [`TextLayout`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct Line {
    /// The bytes of the text drawn on the line, trailing spaces included;
    /// line breaks and text cut off by an ellipsis are not.
    pub text: Range<usize>,
    /// The line's glyphs in [`TextLayout::glyphs`], the ellipsis included.
    pub glyphs: Range<usize>,
    /// Where the line starts, after alignment.
    pub x: f32,
    /// The line's width: the advances of its glyphs, as its text shaped on
    /// its own gives them, without trailing spaces, and with the ellipsis
    /// if the line ends with one.
    pub width: f32,
    /// The top of the line's box.
    pub top: f32,
    /// The line's baseline.
    pub baseline: f32,
}

/// Where a caret before a character goes: a vertical bar from `top` to
/// `bottom` at `x`, on line `line`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct Caret {
    /// The bar's horizontal position.
    pub x: f32,
    /// The top of the line's box.
    pub top: f32,
    /// The bottom of the line's box.
    pub bottom: f32,
    /// The line, counted from 0.
    pub line: usize,
}

/// A caret position: the byte index of a character boundary, and where it
/// is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Stop {
    index: usize,
    line: usize,
    x: f32,
}

/// A paragraph of text laid out in lines, ready to draw with
/// [`Scene::text`](crate::Scene::text).
///
/// The layout's coordinates are logical pixels with the origin at the top
/// left of its first line's box. Lines are [`line_height`] apart, and the
/// first baseline is [`baseline`] below the top. Text is set from its
/// string, a [`TextStyle`] and an optional maximum width, and the layout
/// keeps all three: change any of them by making a new layout, except the
/// colour, which [`set_color`] changes in place.
///
/// Character indices are byte indices into the text, always on character
/// boundaries, as Rust's `str` methods use them.
///
/// See the [module documentation](crate::text) for what layout does and
/// does not do.
///
/// ```
/// use manguebyte_gfx::{TextLayout, TextStyle};
///
/// use manguebyte_gfx::Point;
///
/// let style = TextStyle::new(11.0).with_max_lines(1);
/// let label = TextLayout::new("Low cut frequency", &style, Some(60.0));
/// assert_eq!(label.lines().len(), 1);
/// assert!(label.is_truncated());
/// assert!(label.size().width <= 60.0);
/// // What is left of the text; an ellipsis follows it and stands for the
/// // rest, from byte 9 on.
/// let line = &label.lines()[0];
/// assert_eq!(&label.text()[line.text.clone()], "Low cut f");
/// assert_eq!(label.glyphs().last().map(|g| g.index), Some(9));
///
/// // Positions and indices map both ways.
/// let caret = label.caret(3).expect("a boundary");
/// assert_eq!(label.index_at(Point::new(caret.x, 6.0)), 3);
/// ```
///
/// [`line_height`]: TextLayout::line_height
/// [`baseline`]: TextLayout::baseline
/// [`set_color`]: TextLayout::set_color
#[derive(Debug, Clone, PartialEq)]
pub struct TextLayout {
    text: String,
    style: TextStyle,
    max_width: Option<f32>,
    /// The size, weight and optical size used, after clamping.
    size: f32,
    weight: f32,
    optical_size: f32,
    coords: Coords,
    glyphs: Vec<Glyph>,
    lines: Vec<Line>,
    stops: Vec<Stop>,
    measured: Size,
    line_height: f32,
    baseline: f32,
    ink: Option<Rect>,
    truncated: bool,
}

/// A run of glyphs that stand for a run of characters, as the shaper
/// grouped them.
#[derive(Debug, Clone)]
struct Cluster {
    /// Bytes of the text.
    start: usize,
    end: usize,
    /// Glyphs, in the shaped glyphs.
    glyphs: Range<usize>,
    /// Font units.
    advance: i64,
    /// Whether every character is a space a line may break after.
    space: bool,
}

/// The text between forced breaks.
struct Hard {
    /// Its characters.
    content: Range<usize>,
    /// The break after it (empty for the last).
    brk: Range<usize>,
    /// Its clusters.
    clusters: Range<usize>,
}

/// A line before positions are worked out.
struct Draft {
    /// While breaking, clusters of the segments as shaped whole; once the
    /// line is final, the clusters it is drawn with (see [`Runs::line`]).
    clusters: Range<usize>,
    hard: usize,
    /// Whether the line is the last of its forced-break segment.
    ends_hard: bool,
    /// For the line cut with an ellipsis: the byte where the cut text
    /// starts.
    cut: Option<usize>,
}

/// The characters of `text[range]` with their byte offsets, a control
/// character given as a space (the typeface draws none of them).
fn characters(text: &str, range: Range<usize>) -> impl Iterator<Item = (usize, char)> + '_ {
    text[range.clone()]
        .char_indices()
        .map(move |(i, c)| (range.start + i, if c.is_control() { ' ' } else { c }))
}

/// Groups the glyphs `glyphs` of `shaped`, shaped from the bytes `content`
/// of `text`, into clusters appended to `out`: each cluster runs from its
/// first glyph's character to the next cluster's, the first from the start
/// of `content` and the last to its end.
fn group(
    text: &str,
    shaped: &[Shaped],
    glyphs: Range<usize>,
    content: Range<usize>,
    out: &mut Vec<Cluster>,
) {
    let first_cluster = out.len();
    let mut g = glyphs.start;
    while g < glyphs.end {
        let start = shaped[g].cluster as usize;
        let mut e = g + 1;
        while e < glyphs.end && shaped[e].cluster as usize == start {
            e += 1;
        }
        let advance = shaped[g..e].iter().map(|s| i64::from(s.advance)).sum();
        out.push(Cluster {
            start: start.clamp(content.start, content.end),
            end: content.end,
            glyphs: g..e,
            advance,
            space: false,
        });
        g = e;
    }
    if let Some(first) = out.get_mut(first_cluster) {
        first.start = content.start;
    }
    for i in first_cluster..out.len() {
        let next = out.get(i + 1).map_or(content.end, |c| c.start);
        let cluster = &mut out[i];
        cluster.end = next.clamp(cluster.start, content.end);
        cluster.space = text
            .get(cluster.start..cluster.end)
            .is_some_and(|s| !s.is_empty() && s.chars().all(is_break_space));
    }
}

/// The advances of `clusters` summed, without the spaces at the end.
fn ink_width(clusters: &[Cluster]) -> i64 {
    let ink = clusters.iter().rposition(|c| !c.space).map_or(0, |i| i + 1);
    clusters[..ink].iter().map(|c| c.advance).sum()
}

/// Shapes the bytes `range` of `text` on their own, followed, if
/// `ellipsis`, by an ellipsis standing for the text from `range.end` on,
/// and appends the glyphs to `out`.
fn shape_alone(
    shaper: &mut Shaper,
    text: &str,
    range: Range<usize>,
    ellipsis: bool,
    out: &mut Vec<Shaped>,
) {
    let first = range.start == 0;
    let last = ellipsis || range.end == text.len();
    let end = range.end;
    let chars = characters(text, range).chain(ellipsis.then_some((end, ELLIPSIS)));
    shaper.shape(chars, first, last, out);
}

/// The character that ends a line cut short.
const ELLIPSIS: char = '\u{2026}';

/// The text shaped segment by segment, and what breaking it into lines
/// needs: exact widths of candidate lines, and the glyphs of lines that
/// must be shaped again on their own.
///
/// A line is drawn as its own text shaped alone. Where the shaper says a
/// cut between two clusters changes nothing on either side (no kerning or
/// contextual rule reaches across it), the glyphs of the segment shaped
/// whole are exactly that, and are used as they are; a line with an end
/// where that is not so is shaped again.
struct Runs<'t> {
    text: &'t str,
    shaper: Shaper,
    /// Glyphs: the segments' first, then those of lines shaped again.
    shaped: Vec<Shaped>,
    /// Clusters, in the same order.
    clusters: Vec<Cluster>,
    /// `before[i]`: the advances of the segments' clusters `0..i` summed.
    before: Vec<i64>,
    hards: Vec<Hard>,
    /// The advance of the ellipsis shaped alone.
    ellipsis: i64,
    /// Scratch space for lines shaped to be measured.
    scratch: Vec<Shaped>,
    scratch_clusters: Vec<Cluster>,
}

impl<'t> Runs<'t> {
    /// Shapes each segment of `text` between forced breaks and groups its
    /// glyphs.
    fn new(text: &'t str, mut shaper: Shaper) -> Self {
        let segments = hard_segments(text);
        let mut shaped: Vec<Shaped> = Vec::new();
        let mut clusters: Vec<Cluster> = Vec::new();
        let mut hards: Vec<Hard> = Vec::with_capacity(segments.len());
        let last_segment = segments.len() - 1;
        for (h, (content, brk)) in segments.into_iter().enumerate() {
            let first_glyph = shaped.len();
            let chars = characters(text, content.clone());
            shaper.shape(chars, h == 0, h == last_segment, &mut shaped);
            let first_cluster = clusters.len();
            group(
                text,
                &shaped,
                first_glyph..shaped.len(),
                content.clone(),
                &mut clusters,
            );
            hards.push(Hard {
                content,
                brk,
                clusters: first_cluster..clusters.len(),
            });
        }
        let mut before = Vec::with_capacity(clusters.len() + 1);
        let mut sum = 0i64;
        before.push(sum);
        for cluster in &clusters {
            sum += cluster.advance;
            before.push(sum);
        }
        let mut scratch = Vec::new();
        shaper.shape(std::iter::once((0, ELLIPSIS)), false, true, &mut scratch);
        let ellipsis = scratch.iter().map(|s| i64::from(s.advance)).sum();
        Runs {
            text,
            shaper,
            shaped,
            clusters,
            before,
            hards,
            ellipsis,
            scratch,
            scratch_clusters: Vec::new(),
        }
    }

    /// Whether segment `h` shaped whole may be cut before its cluster `i`
    /// with both sides as they would be shaped apart: at the segment's
    /// ends, and where the shaper does not mark the cluster unsafe.
    fn safe(&self, h: usize, i: usize) -> bool {
        let hard = &self.hards[h];
        i == hard.clusters.start
            || i == hard.clusters.end
            || !self.shaped[self.clusters[i].glyphs.start].unsafe_to_break
    }

    /// The byte where cluster `i` of segment `h` starts, or the segment's
    /// end for its end.
    fn byte(&self, h: usize, i: usize) -> usize {
        let hard = &self.hards[h];
        if i < hard.clusters.end {
            self.clusters[i].start
        } else {
            hard.content.end
        }
    }

    /// The width, in font units, of the line holding clusters `start..end`
    /// of segment `h` as it is drawn: its text shaped alone, without the
    /// spaces at its end.
    fn width(&mut self, h: usize, start: usize, end: usize) -> i64 {
        if self.safe(h, start) && self.safe(h, end) {
            let mut ink = end;
            while ink > start && self.clusters[ink - 1].space {
                ink -= 1;
            }
            return self.before[ink] - self.before[start];
        }
        let range = self.byte(h, start)..self.byte(h, end);
        self.measure(range, false)
    }

    /// The width of the line that keeps clusters `start..keep` of segment
    /// `h` and ends with an ellipsis, shaped together, so that the last
    /// character kept is kerned against the ellipsis.
    fn cut_width(&mut self, h: usize, start: usize, keep: usize) -> i64 {
        let range = self.byte(h, start)..self.byte(h, keep);
        self.measure(range, true)
    }

    /// Shapes `range` alone, with an ellipsis after it if asked, and gives
    /// its width without spaces at the end.
    fn measure(&mut self, range: Range<usize>, ellipsis: bool) -> i64 {
        self.scratch.clear();
        self.scratch_clusters.clear();
        shape_alone(
            &mut self.shaper,
            self.text,
            range.clone(),
            ellipsis,
            &mut self.scratch,
        );
        if ellipsis {
            return self.scratch.iter().map(|s| i64::from(s.advance)).sum();
        }
        let glyphs = 0..self.scratch.len();
        group(
            self.text,
            &self.scratch,
            glyphs,
            range,
            &mut self.scratch_clusters,
        );
        ink_width(&self.scratch_clusters)
    }

    /// Where to break a word wider than the line that starts at cluster
    /// `from` of segment `h` and runs to `end`: before the most clusters
    /// that fit, at least one, never all. The summed advances of the
    /// segment give a first guess, which shaping the line checks.
    fn longest(&mut self, h: usize, from: usize, end: usize, fits: impl Fn(i64) -> bool) -> usize {
        let mut j = from + 1;
        while j + 1 < end && fits(self.before[j + 1] - self.before[from]) {
            j += 1;
        }
        while j > from + 1 && !fits(self.width(h, from, j)) {
            j -= 1;
        }
        while j + 1 < end && fits(self.width(h, from, j + 1)) {
            j += 1;
        }
        j
    }

    /// How far the line starting at cluster `start` of segment `h` runs
    /// when it is cut with an ellipsis: the most clusters that fit beside
    /// the ellipsis, possibly running past where the line broke, with no
    /// spaces before the ellipsis. The summed advances give a first guess,
    /// which shaping the line with its ellipsis checks.
    fn keep(&mut self, h: usize, start: usize, fits: impl Fn(i64) -> bool) -> usize {
        let end = self.hards[h].clusters.end;
        let strip = |runs: &Self, mut j: usize| {
            while j > start && runs.clusters[j - 1].space {
                j -= 1;
            }
            j
        };
        let mut j = start;
        while j < end && fits(self.before[j + 1] - self.before[start] + self.ellipsis) {
            j += 1;
        }
        let mut k = strip(self, j);
        while k > start && !fits(self.cut_width(h, start, k)) {
            k = strip(self, k - 1);
        }
        loop {
            // One more character, with the spaces before it.
            let mut next = k + 1;
            while next <= end && self.clusters[next - 1].space {
                next += 1;
            }
            if next > end || !fits(self.cut_width(h, start, next)) {
                return k;
            }
            k = next;
        }
    }

    /// The clusters the line holding clusters `start..end` of segment `h`
    /// is drawn with: those of the segment if the line's ends are safe to
    /// cut at, otherwise those of its text shaped again alone, appended.
    /// A line that is `cut` is always shaped again, with an ellipsis after
    /// it, whose glyphs go to `ellipsis`.
    fn line(
        &mut self,
        h: usize,
        start: usize,
        end: usize,
        cut: bool,
        ellipsis: &mut Vec<Shaped>,
    ) -> Range<usize> {
        if !cut && self.safe(h, start) && self.safe(h, end) {
            return start..end;
        }
        let range = self.byte(h, start)..self.byte(h, end);
        let first = self.shaped.len();
        shape_alone(
            &mut self.shaper,
            self.text,
            range.clone(),
            cut,
            &mut self.shaped,
        );
        let mut stop = self.shaped.len();
        if cut {
            // The ellipsis comes last and stands for the text from the end
            // of the range on.
            stop = self.shaped[first..]
                .iter()
                .position(|s| s.cluster as usize >= range.end)
                .map_or(stop, |p| first + p);
            ellipsis.extend(self.shaped.drain(stop..));
        }
        let from = self.clusters.len();
        group(
            self.text,
            &self.shaped,
            first..stop,
            range,
            &mut self.clusters,
        );
        from..self.clusters.len()
    }
}

/// Whether a line may break after `c`.
fn is_break_space(c: char) -> bool {
    c == ' ' || c.is_control()
}

fn is_forced_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// The text split at forced breaks: each segment's characters and the
/// break that ends it. A carriage return followed by a line feed is one
/// break.
fn hard_segments(text: &str) -> Vec<(Range<usize>, Range<usize>)> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !is_forced_break(c) {
            continue;
        }
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                end = j + 1;
                chars.next();
            }
        }
        out.push((start..i, i..end));
        start = end;
    }
    out.push((start..text.len(), text.len()..text.len()));
    out
}

impl TextLayout {
    /// Lays out `text` in `style`, breaking lines to fit `max_width`
    /// logical pixels if given.
    ///
    /// A maximum width that is infinite or `NaN` is no limit; one below 0
    /// counts as 0, which puts every character on a line of its own. The
    /// text is one paragraph; see the [module documentation](crate::text)
    /// for how lines break and what is not supported.
    pub fn new(text: &str, style: &TextStyle, max_width: Option<f32>) -> TextLayout {
        let max_width = max_width.filter(|w| w.is_finite()).map(|w| w.max(0.0));
        let face = Face::embedded();
        let weight = style.used_weight();
        let optical_size = style.used_optical_size();
        let coords = face.coords(weight, optical_size);
        let mut layout = TextLayout {
            text: text.to_owned(),
            style: *style,
            max_width,
            size: 0.0,
            weight,
            optical_size,
            coords,
            glyphs: Vec::new(),
            lines: Vec::new(),
            stops: Vec::new(),
            measured: Size::default(),
            line_height: 0.0,
            baseline: 0.0,
            ink: None,
            truncated: false,
        };
        if let Some(size) = style.used_size() {
            layout.size = size;
            layout.set(&face);
        }
        layout
    }

    /// Shapes, breaks, aligns and places the text.
    fn set(&mut self, face: &Face) {
        let text = self.text.as_str();
        let upem = f64::from(face.units_per_em());
        let k = f64::from(self.size) / upem;
        // A width in font units as `size` and `Line::width` report it in
        // logical pixels. Lines are fitted by comparing exactly this with
        // the maximum width, so text laid out again at the width it
        // measured fits it.
        let px = |units: i64| (units as f64 * k) as f32;
        let max_width = self.max_width;
        let fits = |units: i64| max_width.is_none_or(|w| px(units) <= w);
        let mut runs = Runs::new(text, Shaper::new(self.coords, self.style.tabular_figures));

        // Break each segment into lines, measuring each candidate line as
        // it will be drawn.
        let mut drafts: Vec<Draft> = Vec::new();
        for h in 0..runs.hards.len() {
            let (c0, c1) = (runs.hards[h].clusters.start, runs.hards[h].clusters.end);
            let push = |drafts: &mut Vec<Draft>, range: Range<usize>| {
                drafts.push(Draft {
                    clusters: range,
                    hard: h,
                    ends_hard: false,
                    cut: None,
                });
            };
            let mut line_start = c0;
            let mut i = c0;
            while max_width.is_some() && i < c1 {
                // A unit: characters that are not spaces, then spaces.
                let a = i;
                while i < c1 && !runs.clusters[i].space {
                    i += 1;
                }
                let ink_end = i;
                while i < c1 && runs.clusters[i].space {
                    i += 1;
                }
                let b = i;
                if a > line_start && !fits(runs.width(h, line_start, b)) {
                    push(&mut drafts, line_start..a);
                    line_start = a;
                }
                // A word wider than the line: break it between clusters,
                // at least one on each line.
                if line_start == a {
                    while ink_end - line_start > 1 && !fits(runs.width(h, line_start, b)) {
                        let j = runs.longest(h, line_start, ink_end, fits);
                        push(&mut drafts, line_start..j);
                        line_start = j;
                    }
                }
            }
            push(&mut drafts, line_start..c1);
            if let Some(last) = drafts.last_mut() {
                last.ends_hard = true;
            }
        }

        // Keep at most the lines allowed; the last one kept ends with an
        // ellipsis, holding as much of what follows as fits beside it.
        if let Some(allowed) = self.style.max_lines {
            let allowed = usize::try_from(allowed).unwrap_or(usize::MAX);
            if drafts.len() > allowed {
                self.truncated = true;
                drafts.truncate(allowed);
                if let Some(last) = drafts.last_mut() {
                    let keep = runs.keep(last.hard, last.clusters.start, fits);
                    last.clusters = last.clusters.start..keep;
                    last.cut = Some(runs.byte(last.hard, keep));
                    last.ends_hard = false;
                }
            }
        }

        // The glyphs each line is drawn with.
        let mut ellipsis: Vec<Shaped> = Vec::new();
        for draft in &mut drafts {
            let (start, end) = (draft.clusters.start, draft.clusters.end);
            let cut = draft.cut.is_some();
            draft.clusters = runs.line(draft.hard, start, end, cut, &mut ellipsis);
        }
        let ellipsis_advance: i64 = ellipsis.iter().map(|s| i64::from(s.advance)).sum();
        let Runs {
            shaped,
            clusters,
            hards,
            ..
        } = runs;

        // Vertical metrics.
        let v = face.vertical(self.coords);
        let line_height = f64::from(v.ascent + v.descent + v.line_gap) * k;
        let first_baseline = f64::from(0.5 * v.line_gap + v.ascent) * k;
        self.line_height = line_height as f32;
        self.baseline = first_baseline as f32;

        // Widths and alignment.
        let widths: Vec<i64> = drafts
            .iter()
            .map(|d| {
                ink_width(&clusters[d.clusters.clone()])
                    + if d.cut.is_some() { ellipsis_advance } else { 0 }
            })
            .collect();
        let widest = widths.iter().copied().max().unwrap_or(0);
        let room = self.max_width.map_or(widest as f64 * k, f64::from);

        // Glyphs, lines and caret stops. The ink box is gathered in `f64`
        // and rounded outwards once, so it holds every outline exactly.
        let mut bounds: HashMap<u16, Option<[f32; 4]>> = HashMap::new();
        let mut ink: Option<[f64; 4]> = None;
        let mut add_ink = |glyph: &Glyph, glyphs_bounds: &mut HashMap<u16, Option<[f32; 4]>>| {
            let b = *glyphs_bounds
                .entry(glyph.id)
                .or_insert_with(|| face.control_box(glyph.id, self.coords));
            let Some([x0, y0, x1, y1]) = b else {
                return;
            };
            let (gx, gy) = (f64::from(glyph.position.x), f64::from(glyph.position.y));
            let rect = [
                gx + f64::from(x0) * k,
                gy - f64::from(y1) * k,
                gx + f64::from(x1) * k,
                gy - f64::from(y0) * k,
            ];
            ink = Some(ink.map_or(rect, |i| {
                [
                    i[0].min(rect[0]),
                    i[1].min(rect[1]),
                    i[2].max(rect[2]),
                    i[3].max(rect[3]),
                ]
            }));
        };
        for (li, draft) in drafts.iter().enumerate() {
            let width = widths[li] as f64 * k;
            let x0 = match self.style.align {
                Align::Start => 0.0,
                Align::Center => ((room - width) * 0.5).max(0.0),
                Align::End => (room - width).max(0.0),
            };
            let top = li as f64 * line_height;
            let baseline = top + first_baseline;
            let first_glyph = self.glyphs.len();
            let mut pen = 0i64;
            let at = |pen: i64| (x0 + pen as f64 * k) as f32;
            for cluster in &clusters[draft.clusters.clone()] {
                let piece = text.get(cluster.start..cluster.end).unwrap_or("");
                let count = piece.chars().count().max(1) as f64;
                for (m, (b, _)) in piece.char_indices().enumerate() {
                    let x = x0 + (pen as f64 + cluster.advance as f64 * m as f64 / count) * k;
                    self.stops.push(Stop {
                        index: cluster.start + b,
                        line: li,
                        x: x as f32,
                    });
                }
                for s in &shaped[cluster.glyphs.clone()] {
                    let glyph = Glyph {
                        id: s.glyph,
                        position: Point::new(
                            (x0 + (pen + i64::from(s.x_offset)) as f64 * k) as f32,
                            (baseline - f64::from(s.y_offset) * k) as f32,
                        ),
                        index: cluster.start,
                    };
                    add_ink(&glyph, &mut bounds);
                    self.glyphs.push(glyph);
                    pen += i64::from(s.advance);
                }
            }
            let hard = &hards[draft.hard];
            // A line with no clusters is an empty segment, or a line cut
            // down to its ellipsis.
            let line_text = if draft.clusters.is_empty() {
                let at = draft.cut.unwrap_or(hard.content.start);
                at..at
            } else {
                clusters[draft.clusters.start].start..clusters[draft.clusters.end - 1].end
            };
            if let Some(cut) = draft.cut {
                self.stops.push(Stop {
                    index: cut,
                    line: li,
                    x: at(pen),
                });
                for s in &ellipsis {
                    let glyph = Glyph {
                        id: s.glyph,
                        position: Point::new(
                            (x0 + (pen + i64::from(s.x_offset)) as f64 * k) as f32,
                            (baseline - f64::from(s.y_offset) * k) as f32,
                        ),
                        index: cut,
                    };
                    add_ink(&glyph, &mut bounds);
                    self.glyphs.push(glyph);
                    pen += i64::from(s.advance);
                }
                let end = at(pen);
                for (b, _) in text[cut..].char_indices().skip(1) {
                    self.stops.push(Stop {
                        index: cut + b,
                        line: li,
                        x: end,
                    });
                }
                if cut < text.len() {
                    self.stops.push(Stop {
                        index: text.len(),
                        line: li,
                        x: end,
                    });
                }
            } else if draft.ends_hard {
                // The end of the segment, which for an empty segment is
                // also its start, and any boundary inside the break.
                let end = at(pen);
                self.stops.push(Stop {
                    index: hard.content.end,
                    line: li,
                    x: end,
                });
                for (b, _) in text[hard.brk.clone()].char_indices().skip(1) {
                    self.stops.push(Stop {
                        index: hard.brk.start + b,
                        line: li,
                        x: end,
                    });
                }
            }
            self.lines.push(Line {
                text: line_text,
                glyphs: first_glyph..self.glyphs.len(),
                x: x0 as f32,
                width: px(widths[li]),
                top: top as f32,
                baseline: baseline as f32,
            });
        }
        self.ink = ink.map(|[left, top, right, bottom]| Rect::enclosing(left, top, right, bottom));
        self.measured = Size::new(px(widest), (drafts.len() as f64 * line_height) as f32);
        self.stops.sort_by_key(|s| s.index);
        self.stops.dedup_by_key(|s| s.index);
    }

    /// The text laid out.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The style it was laid out in.
    pub fn style(&self) -> &TextStyle {
        &self.style
    }

    /// The maximum width it was laid out for: `None` if there was none,
    /// or if the width given was infinite or `NaN`.
    pub fn max_width(&self) -> Option<f32> {
        self.max_width
    }

    /// Changes the colour the text is drawn in, keeping everything else.
    pub fn set_color(&mut self, color: Color) {
        self.style.color = color;
    }

    /// The width of the widest line, without trailing spaces, and the
    /// height of all lines.
    ///
    /// Lines are fitted to a maximum width by comparing their widths with
    /// it exactly as this method and [`Line::width`] report them, so a
    /// label measured on one line and then given exactly its measured
    /// width stays on one line, with no ellipsis.
    ///
    /// Aligned lines may start to the right of 0 within the maximum width;
    /// see [`Line::x`].
    pub fn size(&self) -> Size {
        self.measured
    }

    /// The distance between one baseline and the next.
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// The distance from the top of the layout to its first baseline.
    pub fn baseline(&self) -> f32 {
        self.baseline
    }

    /// The typeface's vertical metrics at this layout's size and weight.
    pub fn metrics(&self) -> FontMetrics {
        self.style.metrics()
    }

    /// The lines, from the top.
    pub fn lines(&self) -> &[Line] {
        &self.lines
    }

    /// Every glyph, line by line, left to right.
    pub fn glyphs(&self) -> &[Glyph] {
        &self.glyphs
    }

    /// Whether the text needed more lines than the style allows, so that
    /// some was cut and an ellipsis drawn.
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// A rectangle containing every point of every glyph's outline,
    /// control points included, in the layout's coordinates, or `None` if
    /// nothing is drawn (no glyphs, or only spaces).
    pub fn ink_bounds(&self) -> Option<Rect> {
        self.ink
    }

    /// Where the caret goes before the character at byte `index` (or at the
    /// end, for the text's length), or `None` if `index` is not a character
    /// boundary of the text, or there are no lines.
    ///
    /// Inside a glyph that stands for several characters (a ligature such
    /// as `->`) the glyph's width is shared evenly between them. An index
    /// where a line breaks after spaces belongs to the start of the next
    /// line. Indices cut off by the ellipsis go after the ellipsis, except
    /// the first, which goes before it.
    pub fn caret(&self, index: usize) -> Option<Caret> {
        if !self.text.is_char_boundary(index) {
            return None;
        }
        let at = self.stops.partition_point(|s| s.index <= index);
        let stop = self.stops.get(at.checked_sub(1)?)?;
        let line = self.lines.get(stop.line)?;
        Some(Caret {
            x: stop.x,
            top: line.top,
            bottom: line.top + self.line_height,
            line: stop.line,
        })
    }

    /// The byte index of the character boundary nearest to `point`: on the
    /// line `point` is level with (the first or last line above or below
    /// them), the boundary whose caret is closest across, the first of
    /// equally close ones. A coordinate that is `NaN` counts as 0. 0 if
    /// there are no lines.
    pub fn index_at(&self, point: Point) -> usize {
        if self.lines.is_empty() {
            return 0;
        }
        let number = |v: f32| if v.is_nan() { 0.0 } else { f64::from(v) };
        let (x, y) = (number(point.x), number(point.y));
        let row = if self.line_height > 0.0 {
            (y / f64::from(self.line_height)).floor()
        } else {
            0.0
        };
        let line = row.clamp(0.0, (self.lines.len() - 1) as f64) as usize;
        let mut best: Option<(f64, usize)> = None;
        for stop in self.stops.iter().filter(|s| s.line == line) {
            let distance = (f64::from(stop.x) - x).abs();
            if best.is_none_or(|(d, _)| distance < d) {
                best = Some((distance, stop.index));
            }
        }
        best.map_or(0, |(_, index)| index)
    }

    pub(crate) fn used_size(&self) -> f32 {
        self.size
    }

    pub(crate) fn used_weight(&self) -> f32 {
        self.weight
    }

    pub(crate) fn used_optical_size(&self) -> f32 {
        self.optical_size
    }

    pub(crate) fn coords(&self) -> Coords {
        self.coords
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_breaks_split_the_text_and_crlf_is_one() {
        let segments = hard_segments("a\r\nb\nc\rd\u{2028}");
        let content: Vec<Range<usize>> = segments.iter().map(|s| s.0.clone()).collect();
        let breaks: Vec<Range<usize>> = segments.iter().map(|s| s.1.clone()).collect();
        assert_eq!(content, [0..1, 3..4, 5..6, 7..8, 11..11]);
        assert_eq!(breaks, [1..3, 4..5, 6..7, 8..11, 11..11]);
    }

    #[test]
    fn every_boundary_has_one_stop_in_order() {
        let style = TextStyle::new(10.0).with_max_lines(2);
        for text in [
            "",
            "a",
            "Hello world, this is long",
            "a\n\nb ",
            "\u{e9}t\u{e9} -> \r\nx",
            "  lead",
        ] {
            for width in [None, Some(0.0), Some(25.0), Some(1000.0)] {
                let layout = TextLayout::new(text, &style, width);
                let indices: Vec<usize> = layout.stops.iter().map(|s| s.index).collect();
                let mut expected: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
                expected.push(text.len());
                // Every boundary up to the end has a stop, unless lines
                // were dropped, when the cut ones go with the ellipsis.
                assert_eq!(indices, expected, "{text:?} at {width:?}");
            }
        }
    }
}
