//! A small set of rectangles that need drawing.

use std::fmt;

use manguebyte_gfx::IRect;

/// The most rectangles a [`Damage`] keeps. Adding one more merges the two
/// whose union wastes the fewest pixels.
pub const MAX_RECTS: usize = 8;

/// Up to [`MAX_RECTS`] device-pixel rectangles that need drawing.
///
/// Adding a rectangle that another already covers changes nothing; adding
/// one that covers others replaces them. When the set is full, the pair
/// whose bounding rectangle adds the fewest pixels that neither covers is
/// merged, so the set never grows and never loses a pixel. It is a plain
/// value with no heap memory, so events can carry it.
///
/// ```
/// use manguebyte_gfx::IRect;
/// use manguebyte_window::Damage;
///
/// let mut damage = Damage::new();
/// damage.add(IRect::new(0, 0, 10, 10));
/// damage.add(IRect::new(2, 2, 3, 3)); // already covered
/// damage.add(IRect::new(40, 0, 5, 5));
/// assert_eq!(damage.rects(), [IRect::new(0, 0, 10, 10), IRect::new(40, 0, 5, 5)]);
/// assert_eq!(damage.bounds(), IRect::new(0, 0, 45, 10));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Damage {
    rects: [IRect; MAX_RECTS],
    len: u8,
}

impl Damage {
    /// No damage.
    pub const fn new() -> Self {
        Damage {
            rects: [IRect::new(0, 0, 0, 0); MAX_RECTS],
            len: 0,
        }
    }

    /// One rectangle, or nothing if it is empty.
    pub fn of(rect: IRect) -> Self {
        let mut damage = Damage::new();
        damage.add(rect);
        damage
    }

    /// The rectangles, at most [`MAX_RECTS`], none empty and none inside
    /// another.
    pub fn rects(&self) -> &[IRect] {
        &self.rects[..usize::from(self.len)]
    }

    /// Whether there is nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// How many rectangles there are.
    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    /// The smallest rectangle covering all of them; empty if there are
    /// none.
    pub fn bounds(&self) -> IRect {
        self.rects()
            .iter()
            .fold(IRect::default(), |acc, rect| acc.union(rect))
    }

    /// Forgets every rectangle.
    pub fn clear(&mut self) {
        self.len = 0;
    }

    /// The damage, leaving none behind.
    pub fn take(&mut self) -> Damage {
        std::mem::take(self)
    }

    /// Adds `rect` to what needs drawing.
    pub fn add(&mut self, rect: IRect) {
        if rect.is_empty() || self.rects().iter().any(|kept| kept.contains_rect(&rect)) {
            return;
        }
        let mut all = [IRect::default(); MAX_RECTS + 1];
        let mut count = 0;
        for kept in self.rects() {
            if !rect.contains_rect(kept) {
                all[count] = *kept;
                count += 1;
            }
        }
        all[count] = rect;
        count += 1;
        if count > MAX_RECTS {
            count = merge_cheapest(&mut all[..count]);
        }
        self.rects = [IRect::default(); MAX_RECTS];
        self.rects[..count].copy_from_slice(&all[..count]);
        // `count` is at most MAX_RECTS here.
        self.len = count as u8;
    }

    /// Adds the part of `rect` inside `bounds`.
    pub fn add_clipped(&mut self, rect: IRect, bounds: IRect) {
        if let Some(inside) = rect.intersect(&bounds) {
            self.add(inside);
        }
    }

    /// Adds every rectangle of `other`.
    pub fn add_all(&mut self, other: &Damage) {
        for rect in other.rects() {
            self.add(*rect);
        }
    }

    /// The same damage cut to `bounds`.
    pub fn clipped(&self, bounds: IRect) -> Damage {
        let mut out = Damage::new();
        for rect in self.rects() {
            out.add_clipped(*rect, bounds);
        }
        out
    }

    /// The pixels covered, counting a pixel once per rectangle that covers
    /// it.
    pub fn area(&self) -> u64 {
        self.rects()
            .iter()
            .fold(0u64, |sum, rect| sum.saturating_add(rect.area()))
    }
}

/// Merges the pair whose union wastes the fewest pixels, then drops any
/// rectangle the union covers. Returns the new count. Ties go to the pair
/// found first, so the result depends only on the input.
fn merge_cheapest(rects: &mut [IRect]) -> usize {
    let mut best = (0, 1);
    let mut best_cost = i128::MAX;
    for i in 0..rects.len() {
        for j in i + 1..rects.len() {
            let union = rects[i].union(&rects[j]);
            let cost = i128::from(union.area())
                - i128::from(rects[i].area())
                - i128::from(rects[j].area());
            if cost < best_cost {
                best_cost = cost;
                best = (i, j);
            }
        }
    }
    let (i, j) = best;
    let union = rects[i].union(&rects[j]);
    let mut count = 0;
    for k in 0..rects.len() {
        if k == i || k == j || union.contains_rect(&rects[k]) {
            continue;
        }
        rects[count] = rects[k];
        count += 1;
    }
    rects[count] = union;
    count + 1
}

impl fmt::Debug for Damage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.rects()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: u32, h: u32) -> IRect {
        IRect::new(x, y, w, h)
    }

    /// Every pixel of `rect` is covered by some rectangle of `damage`.
    fn covers(damage: &Damage, rect: IRect) -> bool {
        (rect.y..rect.y + rect.height as i32).all(|y| {
            (rect.x..rect.x + rect.width as i32).all(|x| {
                damage
                    .rects()
                    .iter()
                    .any(|d| d.contains(i64::from(x), i64::from(y)))
            })
        })
    }

    #[test]
    fn empty_rectangles_add_nothing() {
        let mut damage = Damage::new();
        damage.add(r(5, 5, 0, 10));
        damage.add(r(5, 5, 10, 0));
        assert!(damage.is_empty());
        assert_eq!(damage.bounds(), IRect::default());
    }

    #[test]
    fn a_covering_rectangle_replaces_the_ones_it_covers() {
        let mut damage = Damage::new();
        damage.add(r(1, 1, 2, 2));
        damage.add(r(10, 10, 2, 2));
        damage.add(r(30, 30, 2, 2));
        damage.add(r(0, 0, 20, 20));
        assert_eq!(damage.rects(), [r(30, 30, 2, 2), r(0, 0, 20, 20)]);
    }

    #[test]
    fn a_ninth_rectangle_merges_the_cheapest_pair() {
        let mut damage = Damage::new();
        // Eight 2x2 squares 100 apart, except two that are 3 apart.
        for i in 0..7 {
            damage.add(r(i * 100, 0, 2, 2));
        }
        damage.add(r(1000, 0, 2, 2));
        assert_eq!(damage.len(), 8);
        damage.add(r(1003, 0, 2, 2));
        assert_eq!(damage.len(), 8);
        // The two near squares became one 5x2 rectangle; the rest stayed.
        assert_eq!(damage.rects()[7], r(1000, 0, 5, 2));
        for i in 0..7 {
            assert_eq!(damage.rects()[i as usize], r(i * 100, 0, 2, 2));
        }
    }

    #[test]
    fn merging_never_loses_a_pixel() {
        // A fixed-seed walk of 200 random rectangles, checked after each one.
        let mut seed = 0x2545_f491_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut damage = Damage::new();
        let mut added = Vec::new();
        for _ in 0..200 {
            let rect = r(
                (next() % 60) as i32 - 5,
                (next() % 60) as i32 - 5,
                next() % 12,
                next() % 12,
            );
            damage.add(rect);
            added.push(rect);
            assert!(damage.len() <= MAX_RECTS);
            for (i, a) in damage.rects().iter().enumerate() {
                assert!(!a.is_empty());
                for (j, b) in damage.rects().iter().enumerate() {
                    assert!(i == j || !a.contains_rect(b), "{a:?} covers {b:?}");
                }
            }
            for rect in &added {
                assert!(covers(&damage, *rect), "lost {rect:?} from {damage:?}");
            }
        }
    }

    #[test]
    fn clipping_keeps_only_the_inside() {
        let mut damage = Damage::new();
        damage.add(r(-5, -5, 10, 10));
        damage.add(r(95, 10, 10, 10));
        damage.add(r(200, 200, 5, 5));
        let clipped = damage.clipped(r(0, 0, 100, 100));
        assert_eq!(clipped.rects(), [r(0, 0, 5, 5), r(95, 10, 5, 10)]);
        assert_eq!(clipped.area(), 75);
    }

    #[test]
    fn huge_rectangles_do_not_overflow() {
        let mut damage = Damage::new();
        for i in 0..20 {
            damage.add(r(i32::MIN + i, i32::MAX - 1, u32::MAX, u32::MAX));
            damage.add(r(i * 3, 0, 1, 1));
        }
        assert!(damage.len() <= MAX_RECTS);
        let _ = damage.area();
        let _ = damage.bounds();
    }
}
