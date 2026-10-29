//! Turning scenes into pixels, behind a private seam.
//!
//! [`Raster`] is the seam ADR 0007 asks for: the one place a rasteriser
//! plugs in. The first one is `tiny-skia`, in [`skia`], the only module that
//! names it. A replacement (our own rasteriser for identical bytes on every
//! processor, or `vello_cpu` for speed) implements the same trait and is
//! chosen by benchmark; nothing outside this module changes.

pub(crate) mod skia;

use std::fmt;

use crate::geom::{IRect, Transform};
use crate::pixmap::Pixmap;
use crate::scene::Scene;
use crate::shadow::{ShadowCache, ShadowCacheStats};
use crate::text::{GlyphCache, GlyphCacheStats};

/// Draws a scene into a pixmap.
pub(crate) trait Raster: Send {
    /// Draws `scene`, mapped by `transform` (finite) into the pixmap's
    /// pixels. With `damage`, only the pixels inside its rectangles change,
    /// and each of them comes out as a render of the whole would draw it;
    /// the rectangles are disjoint, not empty, and lie inside `target`.
    /// Glyph masks are looked up in, and added to, `glyphs`, and blurred
    /// shadow masks in `shadows`.
    fn render(
        &mut self,
        scene: &Scene,
        target: &mut Pixmap,
        transform: Transform,
        damage: Option<&[IRect]>,
        caches: Caches<'_>,
    );
}

/// The caches a renderer keeps between renders.
pub(crate) struct Caches<'a> {
    pub(crate) glyphs: &'a mut GlyphCache,
    pub(crate) shadows: &'a mut ShadowCache,
}

/// More damage rectangles than this, before or after they are made
/// disjoint, are replaced by the one rectangle that bounds them, so the
/// number of passes over a scene stays small.
const MAX_PASSES: usize = 16;

/// Draws [`Scene`]s into [`Pixmap`]s.
///
/// A renderer keeps buffers between calls (for layers, clips, and the copy
/// of the target that partial renders draw in, each the size of the
/// target), so one renderer per window draws without allocating those
/// again. Drawing is source-over onto what the pixmap already holds: clear
/// it first, or start the scene with an opaque background.
///
/// Everything is anti-aliased. Curves are replaced by straight lines no
/// farther than 0.05 device pixels from them before they are rasterised,
/// and a stroke is drawn as the filled outline of the stroke, however thin,
/// so a stroke half a pixel wide covers each pixel it crosses by about half.
///
/// Text is drawn from a cache of glyph coverage masks, one per glyph, size
/// in device pixels, weight and optical size, and position of the glyph
/// within a pixel to a quarter of a pixel each way (so a glyph is drawn at
/// most an eighth of a pixel from where the layout put it). The cache holds
/// at most a budget of bytes on the heap, 4 MiB unless
/// [`set_glyph_cache_limit`](Renderer::set_glyph_cache_limit) says
/// otherwise: the masks' pixels and the cache's own tables at the capacity
/// they have. When a new mask would pass it, the masks used least recently
/// are dropped until it fits; the tables grow only when the budget can pay
/// for them, and shrink when the room is wanted for pixels. A mask too
/// large to keep even in an empty cache is drawn without being kept and
/// dropped once its run of text is drawn. The coverage of a run of glyphs
/// is gathered first and blended once, so where glyphs overlap ("ff") a
/// translucent colour is not blended twice. Masks are used only where the
/// scale is the same both ways, at least one half, with no rotation or
/// skew, and glyphs are at most 256 device pixels per em; otherwise glyph
/// outlines are filled directly, as one path per run, which is slower but
/// exact.
///
/// ```
/// use manguebyte_gfx::{Color, IRect, Pixmap, Rect, Renderer, Scene};
///
/// let mut scene = Scene::new();
/// scene.fill(Rect::new(0.0, 0.0, 4.0, 4.0), Color::WHITE);
///
/// let mut pixels = Pixmap::new(4, 4).expect("small");
/// let mut renderer = Renderer::new();
/// // Only the top-left pixel is damaged, so only it is drawn.
/// renderer.render_damage(&scene, &mut pixels, 1.0, &[IRect::new(0, 0, 1, 1)]);
/// assert_eq!(pixels.pixel(0, 0).expect("inside").alpha(), 255);
/// assert_eq!(pixels.pixel(1, 0).expect("inside").alpha(), 0);
/// ```
///
/// Shadows ([`Scene::shadow`]) are drawn from a cache of blurred masks in
/// the same way: the mask of a shadow is drawn and blurred once, kept under
/// its shape, size, blur and scale, and blended from the cache whenever the
/// same shadow is drawn again, until the shadow changes or the budget (16
/// MiB unless [`set_shadow_cache_limit`](Renderer::set_shadow_cache_limit)
/// says otherwise) has to make room. The blur is three passes of box
/// filters each way on whole numbers, so shadows come out the same, bit
/// for bit, on every machine, and a shadow is never drawn beyond its
/// [`reach`](crate::Shadow::reach), so a partial render that covers it
/// equals a whole one.
///
/// ```
/// use manguebyte_gfx::{Color, Pixmap, Rect, Renderer, RoundedRect, Scene, Shadow};
///
/// let card = RoundedRect::new(Rect::new(10.0, 10.0, 40.0, 30.0), 8.0);
/// let mut scene = Scene::new();
/// scene.shadow(card, Shadow::new(Color::BLACK.with_alpha(0x80)).with_blur(12.0));
/// scene.fill(card, Color::hex(0x1C1F28));
/// let mut pixels = Pixmap::new(120, 100).expect("small");
/// let mut renderer = Renderer::new();
/// renderer.render(&scene, &mut pixels, 2.0);
/// renderer.render(&scene, &mut pixels, 2.0);
/// // Blurred once, drawn twice.
/// let shadows = renderer.shadow_cache();
/// assert_eq!((shadows.misses, shadows.hits), (1, 1));
/// ```
pub struct Renderer {
    raster: Box<dyn Raster>,
    glyphs: GlyphCache,
    shadows: ShadowCache,
    passes: Vec<IRect>,
    work: Vec<IRect>,
    next: Vec<IRect>,
}

impl Default for Renderer {
    fn default() -> Self {
        Renderer::new()
    }
}

impl fmt::Debug for Renderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Renderer").finish_non_exhaustive()
    }
}

impl Renderer {
    /// A renderer with the default rasteriser.
    pub fn new() -> Self {
        Renderer {
            raster: Box::new(skia::Skia::new()),
            glyphs: GlyphCache::default(),
            shadows: ShadowCache::default(),
            passes: Vec::new(),
            work: Vec::new(),
            next: Vec::new(),
        }
    }

    /// Draws `scene` over the whole of `target`, with one logical pixel
    /// covering `scale` device pixels each way. A scale that is not a
    /// positive finite number draws nothing.
    pub fn render(&mut self, scene: &Scene, target: &mut Pixmap, scale: f32) {
        if !(scale.is_finite() && scale > 0.0) || target.is_empty() {
            return;
        }
        self.raster.render(
            scene,
            target,
            Transform::scale(scale, scale),
            None,
            Caches {
                glyphs: &mut self.glyphs,
                shadows: &mut self.shadows,
            },
        );
    }

    /// Draws `scene` as [`render`](Renderer::render) does, but changes only
    /// the pixels inside `damage` (device pixels of `target`): every other
    /// pixel keeps its value, byte for byte, and every pixel inside comes
    /// out exactly as a render of the whole target would draw it, so a
    /// buffer kept up to date by partial renders equals a fresh render.
    /// (This holds for targets of at most 8191 pixels a side; larger ones
    /// are drawn by the rasteriser in tiles of its own, and pixels near
    /// those tiles' edges are not guaranteed to match.)
    ///
    /// Overlapping rectangles are drawn once where they overlap, so a
    /// half-transparent scene is never blended twice. More than 16
    /// rectangles, or rectangles that break into more than 16 pieces once
    /// made disjoint, are drawn as the one rectangle that bounds them.
    ///
    /// The damage is drawn in passes, one for each group of rectangles
    /// whose surroundings on a grid of 64 device pixels overlap. A pass
    /// skips every command whose bounds, two pixels wider, do not reach its
    /// damage, and rasterises the others only within its damage rounded out
    /// to the grid. So the rasterising cost of a pass follows the area of
    /// that rounded damage, not of the target; what remains is preparing
    /// the outline of each command that reaches the damage (stroking,
    /// flattening curves, cutting and clamping edges), whose cost depends
    /// on the command, not on the damage. A whole render does that work
    /// once, over the whole target.
    pub fn render_damage(
        &mut self,
        scene: &Scene,
        target: &mut Pixmap,
        scale: f32,
        damage: &[IRect],
    ) {
        if !(scale.is_finite() && scale > 0.0) || target.is_empty() {
            return;
        }
        disjoint(
            damage,
            target.bounds(),
            &mut self.passes,
            &mut self.work,
            &mut self.next,
        );
        if self.passes.is_empty() {
            return;
        }
        let transform = Transform::scale(scale, scale);
        if self.passes[..] == [target.bounds()] {
            self.raster.render(
                scene,
                target,
                transform,
                None,
                Caches {
                    glyphs: &mut self.glyphs,
                    shadows: &mut self.shadows,
                },
            );
        } else {
            self.raster.render(
                scene,
                target,
                transform,
                Some(&self.passes),
                Caches {
                    glyphs: &mut self.glyphs,
                    shadows: &mut self.shadows,
                },
            );
        }
    }

    /// How many glyph masks the renderer holds, how many bytes the cache
    /// holds on the heap (never more than its budget), and how often masks
    /// were found.
    ///
    /// ```
    /// use manguebyte_gfx::{Pixmap, Point, Renderer, Scene, TextLayout, TextStyle};
    ///
    /// let mut scene = Scene::new();
    /// scene.text(&TextLayout::new("aaa", &TextStyle::new(12.0), None), Point::ZERO);
    /// let mut pixels = Pixmap::new(40, 20).expect("small");
    /// let mut renderer = Renderer::new();
    /// renderer.render(&scene, &mut pixels, 1.0);
    /// let cache = renderer.glyph_cache();
    /// // The three glyphs fall on different quarters of a pixel or the
    /// // same; either way none was found before it was drawn once.
    /// assert!(cache.masks >= 1 && cache.masks <= 3);
    /// assert_eq!(cache.hits + cache.misses, 3);
    /// assert!(cache.bytes <= cache.limit);
    /// ```
    pub fn glyph_cache(&self) -> GlyphCacheStats {
        self.glyphs.stats()
    }

    /// Sets the glyph cache's budget in bytes, dropping the masks used
    /// least recently until what it holds fits, in tables made no larger
    /// than they need to be. A budget of 0 keeps nothing: every glyph is
    /// drawn afresh each time.
    pub fn set_glyph_cache_limit(&mut self, bytes: usize) {
        self.glyphs.set_limit(bytes);
    }

    /// Drops every glyph mask and gives back all the memory the cache
    /// holds, keeping the budget and the counts.
    pub fn clear_glyph_cache(&mut self) {
        self.glyphs.clear();
    }

    /// How many blurred shadow masks the renderer holds, how many bytes
    /// they take (never more than the budget), and how often a shadow was
    /// drawn from one (`hits`) or had to be blurred first (`misses`).
    pub fn shadow_cache(&self) -> ShadowCacheStats {
        self.shadows.stats()
    }

    /// Sets the shadow cache's budget in bytes, dropping the masks used
    /// least recently until what it holds fits. A budget of 0 keeps
    /// nothing: every shadow is blurred afresh each time it is drawn.
    pub fn set_shadow_cache_limit(&mut self, bytes: usize) {
        self.shadows.set_limit(bytes);
    }

    /// Drops every shadow mask, keeping the budget and the counts.
    pub fn clear_shadow_cache(&mut self) {
        self.shadows.clear();
    }
}

/// Puts into `out` rectangles that cover exactly the pixels of `damage`
/// inside `bounds`, with no two overlapping.
fn disjoint(
    damage: &[IRect],
    bounds: IRect,
    out: &mut Vec<IRect>,
    work: &mut Vec<IRect>,
    next: &mut Vec<IRect>,
) {
    out.clear();
    let clipped = damage.iter().filter_map(|r| r.intersect(&bounds));
    if damage.len() > MAX_PASSES {
        if let Some(union) = clipped.reduce(|a, b| a.union(&b)) {
            out.push(union);
        }
        return;
    }
    for rect in clipped {
        work.clear();
        work.push(rect);
        for accepted in out.iter() {
            next.clear();
            for piece in work.drain(..) {
                subtract(piece, accepted, next);
            }
            std::mem::swap(work, next);
            if work.is_empty() {
                break;
            }
        }
        out.append(work);
    }
    if out.len() > MAX_PASSES {
        if let Some(union) = out.iter().copied().reduce(|a, b| a.union(&b)) {
            out.clear();
            out.push(union);
        }
    }
}

/// Pushes the parts of `a` outside `b`: at most four rectangles.
fn subtract(a: IRect, b: &IRect, out: &mut Vec<IRect>) {
    let Some(overlap) = a.intersect(b) else {
        out.push(a);
        return;
    };
    let (ax0, ay0, ax1, ay1) = (i64::from(a.x), i64::from(a.y), a.right(), a.bottom());
    let (ox0, oy0, ox1, oy1) = (
        i64::from(overlap.x),
        i64::from(overlap.y),
        overlap.right(),
        overlap.bottom(),
    );
    // Every edge lies inside `bounds`, a pixmap of at most MAX_SIDE pixels,
    // so the conversions back to i32 are exact.
    let mut push = |x0: i64, y0: i64, x1: i64, y1: i64| {
        if x1 > x0 && y1 > y0 {
            out.push(IRect::from_edges(
                x0 as i32, y0 as i32, x1 as i32, y1 as i32,
            ));
        }
    };
    push(ax0, ay0, ax1, oy0);
    push(ax0, oy1, ax1, ay1);
    push(ax0, oy0, ox0, oy1);
    push(ox1, oy0, ax1, oy1);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(damage: &[IRect]) -> Vec<IRect> {
        let mut out = Vec::new();
        disjoint(
            damage,
            IRect::new(0, 0, 100, 100),
            &mut out,
            &mut Vec::new(),
            &mut Vec::new(),
        );
        out
    }

    fn covered(rects: &[IRect]) -> Vec<(i64, i64)> {
        let mut pixels: Vec<(i64, i64)> = (0..100)
            .flat_map(|y| (0..100).map(move |x| (x, y)))
            .filter(|&(x, y)| rects.iter().any(|r| r.contains(x, y)))
            .collect();
        pixels.sort_unstable();
        pixels
    }

    #[test]
    fn overlapping_damage_becomes_disjoint_and_covers_the_same_pixels() {
        let damage = [
            IRect::new(10, 10, 30, 30),
            IRect::new(20, 20, 30, 30),
            IRect::new(15, 0, 5, 100),
            IRect::new(90, 90, 50, 50),
            IRect::new(-5, -5, 3, 3),
        ];
        let out = run(&damage);
        for (i, a) in out.iter().enumerate() {
            for b in &out[i + 1..] {
                assert_eq!(a.intersect(b), None, "{a:?} overlaps {b:?}");
            }
        }
        let clipped: Vec<IRect> = damage
            .iter()
            .filter_map(|r| r.intersect(&IRect::new(0, 0, 100, 100)))
            .collect();
        assert_eq!(covered(&out), covered(&clipped));
        let area: u64 = out.iter().map(IRect::area).sum();
        assert_eq!(area as usize, covered(&clipped).len());
    }

    #[test]
    fn a_rectangle_inside_another_adds_nothing() {
        assert_eq!(
            run(&[IRect::new(0, 0, 50, 50), IRect::new(10, 10, 5, 5)]),
            vec![IRect::new(0, 0, 50, 50)]
        );
    }

    #[test]
    fn too_many_rectangles_become_their_bounds() {
        let many: Vec<IRect> = (0..17).map(|i| IRect::new(i * 5, i * 5, 2, 2)).collect();
        assert_eq!(run(&many), vec![IRect::new(0, 0, 82, 82)]);
    }
}
