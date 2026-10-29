//! Soft shadows and glows: what one is, the blur that softens it, and the
//! cache of blurred masks a renderer keeps.
//!
//! # The blur
//!
//! A shadow is the coverage of its shape, blurred. The blur stands in for
//! a Gaussian with three passes of a box filter each way, as the SVG and
//! CSS filter specifications describe for `feGaussianBlur`: for a standard
//! deviation `s` in device pixels, boxes of `d = floor(s * 3 * sqrt(2 pi) /
//! 4 + 0.5)` pixels; three centred boxes when `d` is odd, and when it is
//! even two boxes of `d` (one shifted left by half a pixel, one right) and
//! one of `d + 1`. Every pass works on whole numbers (coverage times 257,
//! a running sum, and a division that rounds half up), so the result is
//! the same, bit for bit, on every processor and every build.
//!
//! # The cache
//!
//! Blurring is the expensive part, and a shadow depends only on its shape,
//! its size, its blur and the scale it is drawn at. The renderer keeps
//! each blurred mask under a key made of exactly those (the outline in
//! device pixels relative to the mask's corner, the box size and the fill
//! rule), and draws the same shadow again from the mask without blurring.
//! Masks are kept within a budget of bytes and the one used least recently
//! is dropped first.

use std::collections::HashMap;

use crate::color::Color;
use crate::geom::Point;
use crate::hash::Fnv;

/// A soft shadow or glow behind a shape, as CSS's `box-shadow` (outside
/// the shape only through what covers it) and `drop-shadow` draw it.
///
/// The shadow is the shape moved by [`offset`](Shadow::offset), grown by
/// [`spread`](Shadow::spread), filled with [`color`](Shadow::color) and
/// blurred by [`blur`](Shadow::blur). It is drawn where it is recorded, so
/// a shadow recorded before its shape lies under it. A glow is a shadow
/// with no offset in a bright colour ([`Shadow::glow`]).
///
/// Lengths are in the shape's coordinates and scale with it. The blur
/// stands in for a Gaussian whose standard deviation is half of
/// [`blur`](Shadow::blur), as in CSS, and is computed the same way on every
/// machine (see [`Renderer`](crate::Renderer)).
///
/// ```
/// use manguebyte_gfx::{Color, Rect, RoundedRect, Scene, Shadow};
///
/// let card = RoundedRect::new(Rect::new(20.0, 20.0, 120.0, 80.0), 18.0);
/// let mut scene = Scene::new();
/// scene.shadow(
///     card,
///     Shadow::new(Color::BLACK.with_alpha(0xB3))
///         .with_offset(0.0, 18.0)
///         .with_blur(40.0)
///         .with_spread(-24.0),
/// );
/// scene.fill(card, Color::hex(0x1C1F28));
/// assert!(scene.dump().starts_with(
///     "shadow rounded_rect(20 20 120 80 radii 18 18 18 18) offset 0 18 blur 40 spread -24 solid(#000000b3)\n"
/// ));
/// // The bounds hold the shadow's whole reach, below the card.
/// let bounds = scene.bounds().expect("draws");
/// assert!(bounds.bottom() > 100.0 + 18.0);
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Shadow {
    /// How far the shadow is moved from the shape.
    pub offset: Point,
    /// How soft the shadow is: twice the standard deviation of the blur,
    /// as CSS measures it. Zero, negative or not finite draws a hard
    /// shadow.
    pub blur: f32,
    /// How far the shape is grown (or, negative, shrunk) before it is
    /// blurred. Rectangles, rounded rectangles and circles shrink and grow
    /// exactly; other outlines and strokes only grow (by a round stroke of
    /// twice the spread around them), and a negative spread leaves them as
    /// they are.
    pub spread: f32,
    /// The shadow's colour; its alpha is the shadow's opacity.
    pub color: Color,
}

impl Shadow {
    /// A hard shadow of `color` right under the shape.
    pub const fn new(color: Color) -> Self {
        Shadow {
            offset: Point::new(0.0, 0.0),
            blur: 0.0,
            spread: 0.0,
            color,
        }
    }

    /// A glow: a shadow of `color` with no offset, blurred by `blur`.
    pub const fn glow(blur: f32, color: Color) -> Self {
        Shadow {
            offset: Point::new(0.0, 0.0),
            blur,
            spread: 0.0,
            color,
        }
    }

    /// The same shadow moved by `x` and `y`.
    pub const fn with_offset(self, x: f32, y: f32) -> Self {
        Shadow {
            offset: Point::new(x, y),
            ..self
        }
    }

    /// The same shadow with another blur.
    pub const fn with_blur(self, blur: f32) -> Self {
        Shadow { blur, ..self }
    }

    /// The same shadow with another spread.
    pub const fn with_spread(self, spread: f32) -> Self {
        Shadow { spread, ..self }
    }

    /// The same shadow in another colour.
    pub const fn with_color(self, color: Color) -> Self {
        Shadow { color, ..self }
    }

    /// The blur as it is drawn: zero when it is negative or not finite.
    pub(crate) fn drawn_blur(&self) -> f32 {
        if self.blur.is_finite() && self.blur > 0.0 {
            self.blur
        } else {
            0.0
        }
    }

    /// The spread as it is drawn: zero when it is not finite.
    pub(crate) fn drawn_spread(&self) -> f32 {
        if self.spread.is_finite() {
            self.spread
        } else {
            0.0
        }
    }

    /// How far beyond the shape, moved by the offset, the shadow may
    /// change pixels, in the shape's coordinates: the spread if it grows
    /// the shape, plus one and a half times the blur, plus one. The blur's
    /// boxes reach about 1.41 times the blur in device pixels, and two
    /// pixels more for rounding; the renderer never draws a shadow beyond
    /// this reach, so it is the reach exactly, whatever the scale.
    pub fn reach(&self) -> f32 {
        self.drawn_spread().max(0.0) + 1.5 * self.drawn_blur() + 1.0
    }
}

/// The size of the boxes that stand in for a Gaussian of standard
/// deviation `sigma` device pixels (see the module's documentation): 0 or
/// 1 for no blur.
pub(crate) fn box_size(sigma: f64) -> u32 {
    if !(sigma.is_finite() && sigma > 0.0) {
        return 0;
    }
    // 3 * sqrt(2 pi) / 4.
    const K: f64 = 1.879_971_744_356_4;
    // A mask is at most a few thousand pixels a side; larger boxes blur it
    // to almost nothing anyway.
    (sigma * K + 0.5).floor().min(4096.0) as u32
}

/// How far, in device pixels, the three boxes of size `d` spread a
/// pixel's coverage, each way.
pub(crate) fn box_reach(d: u32) -> u32 {
    if d <= 1 { 0 } else { 3 * d.div_ceil(2) }
}

/// Blurs `mask`, `width` by `height` coverage values, in place with boxes
/// of size `d` (see [`box_size`]). `work` and `line` are scratch buffers.
pub(crate) fn blur(
    mask: &mut [u8],
    width: usize,
    height: usize,
    d: u32,
    work: &mut Vec<u16>,
    line: &mut Vec<u16>,
) {
    if d <= 1 || width == 0 || height == 0 {
        return;
    }
    work.clear();
    work.extend(mask.iter().map(|&m| u16::from(m) * 257));
    // The three boxes, each as how far it reaches left (or up) and right
    // (or down) of the pixel it writes.
    let d = d as usize;
    let boxes = if d % 2 == 1 {
        let r = d / 2;
        [(r, r), (r, r), (r, r)]
    } else {
        let h = d / 2;
        [(h, h - 1), (h - 1, h), (h, h)]
    };
    line.clear();
    line.resize(width.max(height), 0);
    for (before, after) in boxes {
        for y in 0..height {
            let row = &mut work[y * width..(y + 1) * width];
            box_pass(row, 1, width, before, after, line);
        }
    }
    for (before, after) in boxes {
        for x in 0..width {
            box_pass(&mut work[x..], width, height, before, after, line);
        }
    }
    for (m, &w) in mask.iter_mut().zip(work.iter()) {
        // Back to 0..=255, rounding half up.
        *m = ((u32::from(w) + 128) / 257) as u8;
    }
}

/// One box pass along `count` values `stride` apart in `data`: each
/// becomes the mean of the values from `before` back to `after` on, with
/// zero beyond the ends, rounded half up. `line` holds at least `count`
/// values.
fn box_pass(
    data: &mut [u16],
    stride: usize,
    count: usize,
    before: usize,
    after: usize,
    line: &mut [u16],
) {
    let size = (before + after + 1) as u64;
    for (i, slot) in line[..count].iter_mut().enumerate() {
        *slot = data[i * stride];
    }
    // The sum of the window around entry 0: entries 0 ..= after.
    let mut sum: u64 = line[..count.min(after + 1)]
        .iter()
        .map(|&v| u64::from(v))
        .sum();
    for i in 0..count {
        data[i * stride] = ((sum * 2 + size) / (size * 2)) as u16;
        // Slide: add entry i + after + 1, drop entry i - before.
        if let Some(&v) = line.get(i + after + 1).filter(|_| i + after + 1 < count) {
            sum += u64::from(v);
        }
        if i >= before {
            sum -= u64::from(line[i - before]);
        }
    }
}

/// How many masks a renderer's shadow cache holds, how many bytes, and how
/// often a mask was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShadowCacheStats {
    /// Masks held.
    pub masks: usize,
    /// Bytes of mask pixels and keys held. Never more than the limit.
    pub bytes: usize,
    /// The budget, in the same bytes.
    pub limit: usize,
    /// Shadows drawn from a mask already held.
    pub hits: u64,
    /// Shadows whose mask had to be drawn and blurred first.
    pub misses: u64,
}

/// The budget a renderer's shadow cache starts with: 16 MiB, room for a
/// few dozen large cards at 2x.
pub(crate) const DEFAULT_LIMIT: usize = 16 << 20;

/// A blurred mask and what it was made from.
#[derive(Debug)]
pub(crate) struct ShadowMask {
    pub(crate) width: u32,
    pub(crate) data: Box<[u8]>,
}

struct Entry {
    key: Box<[u32]>,
    mask: ShadowMask,
    used: u64,
}

impl Entry {
    fn bytes(&self) -> usize {
        self.mask.data.len() + self.key.len() * 4
    }
}

/// Blurred masks, by key.
#[derive(Default)]
pub(crate) struct ShadowCache {
    entries: HashMap<u64, Entry>,
    bytes: usize,
    limit: Option<usize>,
    clock: u64,
    hits: u64,
    misses: u64,
    /// A mask too large to keep, held only while it is drawn.
    loose: Option<ShadowMask>,
}

impl ShadowCache {
    fn limit(&self) -> usize {
        self.limit.unwrap_or(DEFAULT_LIMIT)
    }

    pub(crate) fn stats(&self) -> ShadowCacheStats {
        ShadowCacheStats {
            masks: self.entries.len(),
            bytes: self.bytes,
            limit: self.limit(),
            hits: self.hits,
            misses: self.misses,
        }
    }

    pub(crate) fn set_limit(&mut self, bytes: usize) {
        self.limit = Some(bytes);
        self.evict(0);
    }

    pub(crate) fn clear(&mut self) {
        self.entries = HashMap::new();
        self.bytes = 0;
        self.loose = None;
    }

    /// Drops the masks used least recently until `room` more bytes fit.
    fn evict(&mut self, room: usize) {
        while self.bytes + room > self.limit() && !self.entries.is_empty() {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(&hash, _)| hash);
            if let Some(entry) = oldest.and_then(|hash| self.entries.remove(&hash)) {
                self.bytes -= entry.bytes();
            }
        }
    }

    /// The mask for `key`, made by `make` if none is held. A mask that
    /// would not fit even in an empty cache is made and handed out without
    /// being kept.
    pub(crate) fn get_or_make(
        &mut self,
        key: &[u32],
        make: impl FnOnce() -> Option<ShadowMask>,
    ) -> Option<&ShadowMask> {
        let mut hash = Fnv::new();
        for &word in key {
            hash.u32(word);
        }
        let hash = hash.finish();
        self.clock += 1;
        let found = self
            .entries
            .get(&hash)
            .is_some_and(|entry| *entry.key == *key);
        if found {
            self.hits += 1;
            let entry = self.entries.get_mut(&hash)?;
            entry.used = self.clock;
            return Some(&entry.mask);
        }
        self.misses += 1;
        self.loose = None;
        let mask = make()?;
        let entry = Entry {
            key: key.into(),
            mask,
            used: self.clock,
        };
        let bytes = entry.bytes();
        if bytes > self.limit() {
            self.loose = Some(entry.mask);
            return self.loose.as_ref();
        }
        if let Some(old) = self.entries.remove(&hash) {
            // Another key with the same hash: replaced.
            self.bytes -= old.bytes();
        }
        self.evict(bytes);
        self.bytes += bytes;
        Some(&self.entries.entry(hash).or_insert(entry).mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A blur with floating-point boxes, for comparison.
    fn reference(mask: &[u8], width: usize, height: usize, d: u32) -> Vec<f64> {
        let mut data: Vec<f64> = mask.iter().map(|&m| f64::from(m)).collect();
        let d = d as usize;
        let boxes = if d % 2 == 1 {
            [(d / 2, d / 2); 3]
        } else {
            [(d / 2, d / 2 - 1), (d / 2 - 1, d / 2), (d / 2, d / 2)]
        };
        let get = |data: &[f64], i: isize, n: usize, at: &dyn Fn(usize) -> usize| {
            if i < 0 || i as usize >= n {
                0.0
            } else {
                data[at(i as usize)]
            }
        };
        for (before, after) in boxes {
            let old = data.clone();
            for y in 0..height {
                for x in 0..width {
                    let mut sum = 0.0;
                    for k in -(before as isize)..=after as isize {
                        sum += get(&old, x as isize + k, width, &|i| y * width + i);
                    }
                    data[y * width + x] = sum / (before + after + 1) as f64;
                }
            }
        }
        for (before, after) in boxes {
            let old = data.clone();
            for x in 0..width {
                for y in 0..height {
                    let mut sum = 0.0;
                    for k in -(before as isize)..=after as isize {
                        sum += get(&old, y as isize + k, height, &|i| i * width + x);
                    }
                    data[y * width + x] = sum / (before + after + 1) as f64;
                }
            }
        }
        data
    }

    #[test]
    fn the_blur_follows_floating_point_boxes_to_within_one_level() {
        let (width, height) = (40, 30);
        let mut mask = vec![0u8; width * height];
        for y in 10..20 {
            for x in 12..28 {
                mask[y * width + x] = 255;
            }
        }
        mask[3 * width + 3] = 200;
        for d in [2, 3, 4, 7, 10] {
            let mut blurred = mask.clone();
            blur(
                &mut blurred,
                width,
                height,
                d,
                &mut Vec::new(),
                &mut Vec::new(),
            );
            let exact = reference(&mask, width, height, d);
            for (i, (&b, &e)) in blurred.iter().zip(&exact).enumerate() {
                assert!(
                    (f64::from(b) - e).abs() <= 1.0,
                    "d {d}, pixel {i}: {b} against {e}"
                );
            }
        }
    }

    #[test]
    fn the_blur_keeps_the_total_and_reaches_no_farther_than_its_boxes() {
        let (width, height) = (61, 61);
        let mut mask = vec![0u8; width * height];
        mask[30 * width + 30] = 255;
        for d in [3u32, 4, 9] {
            let mut blurred = mask.clone();
            blur(
                &mut blurred,
                width,
                height,
                d,
                &mut Vec::new(),
                &mut Vec::new(),
            );
            let reach = box_reach(d) as usize;
            for y in 0..height {
                for x in 0..width {
                    if x.abs_diff(30) > reach || y.abs_diff(30) > reach {
                        assert_eq!(blurred[y * width + x], 0, "d {d} at {x}, {y}");
                    }
                }
            }
            // Symmetric for odd boxes.
            if d % 2 == 1 {
                for y in 0..height {
                    for x in 0..width {
                        assert_eq!(
                            blurred[y * width + x],
                            blurred[(height - 1 - y) * width + (width - 1 - x)]
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn box_sizes_follow_the_filter_specification() {
        assert_eq!(box_size(0.0), 0);
        assert_eq!(box_size(f64::NAN), 0);
        assert_eq!(box_size(1.0), 2);
        assert_eq!(box_size(2.0), 4);
        assert_eq!(box_size(20.0), 38);
        assert_eq!(box_reach(0), 0);
        assert_eq!(box_reach(1), 0);
        assert_eq!(box_reach(4), 6);
        assert_eq!(box_reach(5), 9);
    }

    #[test]
    fn the_cache_keeps_within_its_budget_and_counts_hits() {
        let mut cache = ShadowCache::default();
        cache.set_limit(1000);
        let make = |side: u32| {
            move || {
                Some(ShadowMask {
                    width: side,
                    data: vec![1; (side * side) as usize].into(),
                })
            }
        };
        assert!(cache.get_or_make(&[1], make(20)).is_some());
        assert!(cache.get_or_make(&[1], make(20)).is_some());
        assert_eq!((cache.stats().hits, cache.stats().misses), (1, 1));
        assert!(cache.get_or_make(&[2], make(20)).is_some());
        // 400 + 4 bytes each: the third pushes out the oldest.
        assert!(cache.get_or_make(&[3], make(20)).is_some());
        let stats = cache.stats();
        assert_eq!(stats.masks, 2);
        assert!(stats.bytes <= stats.limit);
        // Too large to keep: handed out, not kept.
        assert!(cache.get_or_make(&[4], make(40)).is_some());
        assert_eq!(cache.stats().masks, 2);
        cache.clear();
        assert_eq!(cache.stats().bytes, 0);
    }
}
