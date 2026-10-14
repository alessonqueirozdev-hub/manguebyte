//! The glyph cache: coverage masks of glyphs as the rasteriser drew them,
//! kept within a budget of bytes and dropped least recently used first.
//!
//! # What the budget counts
//!
//! Every byte the cache holds on the heap, as requested from the
//! allocator: the pixels of each mask, and its two tables at the capacity
//! they have, not only the part in use. The table of entries has room for
//! some number of masks, an [`Entry`] each, and the index has two `u32`
//! slots for each of those, so it is never more than half full; together
//! they cost [`SLOT_COST`] bytes per mask they have room for.
//!
//! A table grows (doubling) only when the budget can pay for it beside the
//! masks already held; otherwise the least recently used mask gives up its
//! entry. When a new mask does not fit, the least recently used masks are
//! dropped until it does, and if that leaves three quarters of the tables'
//! room unused, the tables are rebuilt half as large so that the room goes
//! to pixels. Lowering the budget rebuilds them just large enough for what
//! is kept, and clearing the cache gives everything back. The one thing
//! outside the budget is a mask too large to keep even in an empty cache,
//! which is handed out without being kept and dropped as soon as the run
//! of glyphs that needed it has been drawn ([`GlyphCache::release`]).

use std::mem::size_of;

use super::font::Coords;
use crate::hash::Fnv;

/// What each mask the tables have room for costs: its entry and its two
/// slots of the index.
pub(crate) const SLOT_COST: usize = size_of::<Entry>() + 2 * size_of::<u32>();

/// The budget a renderer's cache starts with: 4 MiB.
pub(crate) const DEFAULT_LIMIT: usize = 4 << 20;

/// The most masks the tables may have room for, so that the number of an
/// entry plus one fits a `u32` slot of the index.
const MAX_ROOM: usize = 1 << 28;

/// Tables with room for fewer masks than this are not rebuilt smaller.
const MIN_REBUILT: usize = 16;

/// What identifies a mask: the glyph, its size in device pixels per em,
/// the point of the variation space, and where the glyph's origin falls
/// within a device pixel, in quarters of a pixel each way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) struct GlyphKey {
    pub(crate) glyph: u16,
    /// The bits of the `f32` size, so equal sizes are equal keys.
    pub(crate) ppem: u32,
    pub(crate) coords: Coords,
    /// `x + 4 * y`, each from 0 to 3.
    pub(crate) subpixel: u8,
}

impl GlyphKey {
    /// FNV-1a of the key's fields: keys are small and not chosen by an
    /// attacker, and a fixed hash keeps the cache independent of the
    /// process's random state.
    fn hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.bytes(&self.glyph.to_le_bytes());
        h.u32(self.ppem);
        for c in self.coords.0 {
            h.bytes(&c.to_le_bytes());
        }
        h.bytes(&[self.subpixel]);
        h.finish()
    }
}

/// A glyph's coverage: `width` by `height` bytes, one per device pixel,
/// whose top-left pixel lies `left` and `top` pixels from the whole pixel
/// the glyph's origin was rounded down to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Mask {
    pub(crate) left: i32,
    pub(crate) top: i32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) data: Box<[u8]>,
}

/// How full a renderer's glyph cache is and how well it has served.
///
/// See [`Renderer::glyph_cache`](crate::Renderer::glyph_cache).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct GlyphCacheStats {
    /// Masks held.
    pub masks: usize,
    /// Bytes the cache holds on the heap: the masks' pixels and its tables
    /// at the capacity they have. Never more than the limit.
    pub bytes: usize,
    /// The budget, in the same bytes.
    pub limit: usize,
    /// Glyphs drawn from a mask already held.
    pub hits: u64,
    /// Glyphs whose mask had to be drawn first.
    pub misses: u64,
    /// Masks dropped to stay within the budget.
    pub evictions: u64,
}

/// The end of a list of entries.
const NONE: usize = usize::MAX;

/// A mask and its place in the order of use. A free entry holds an empty
/// mask, and `older` links it to the next free one.
#[derive(Debug, Default)]
struct Entry {
    key: GlyphKey,
    mask: Mask,
    /// The entry used just before this one, or `NONE`.
    older: usize,
    /// The entry used just after this one, or `NONE`.
    newer: usize,
}

/// Masks by key, in order of last use.
#[derive(Debug)]
pub(crate) struct GlyphCache {
    /// Entries, held or free. Its capacity is what the budget pays for.
    entries: Vec<Entry>,
    /// The index: open addressing with linear probing, twice as long as
    /// `entries`' capacity. A slot holds the number of an entry plus one,
    /// or 0 when empty.
    index: Vec<u32>,
    /// The first free entry, or `NONE`.
    free: usize,
    newest: usize,
    oldest: usize,
    /// Masks held.
    held: usize,
    /// Bytes of their pixels.
    pixels: usize,
    limit: usize,
    /// A mask too large to keep, held only until it has been drawn.
    passing: Mask,
    hits: u64,
    misses: u64,
    evictions: u64,
}

impl Default for GlyphCache {
    fn default() -> Self {
        GlyphCache::new(DEFAULT_LIMIT)
    }
}

impl GlyphCache {
    pub(crate) fn new(limit: usize) -> Self {
        GlyphCache {
            entries: Vec::new(),
            index: Vec::new(),
            free: NONE,
            newest: NONE,
            oldest: NONE,
            held: 0,
            pixels: 0,
            limit,
            passing: Mask::default(),
            hits: 0,
            misses: 0,
            evictions: 0,
        }
    }

    pub(crate) fn stats(&self) -> GlyphCacheStats {
        GlyphCacheStats {
            masks: self.held,
            bytes: self.bytes(),
            limit: self.limit,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }

    /// The bytes held on the heap, the passing mask aside.
    fn bytes(&self) -> usize {
        self.pixels
            + self.entries.capacity() * size_of::<Entry>()
            + self.index.capacity() * size_of::<u32>()
    }

    /// Whether `extra` more bytes would pass the budget.
    fn over(&self, extra: usize) -> bool {
        self.bytes().saturating_add(extra) > self.limit
    }

    /// Changes the budget, dropping the least recently used masks until
    /// what is held, in tables no larger than they need to be, fits.
    pub(crate) fn set_limit(&mut self, limit: usize) {
        self.limit = limit;
        if !self.over(0) {
            return;
        }
        while self.oldest != NONE
            && self
                .pixels
                .saturating_add(self.held.saturating_mul(SLOT_COST))
                > limit
        {
            self.evict_oldest();
        }
        self.rebuild(self.held);
    }

    /// Drops every mask and gives back all the memory, keeping the budget
    /// and the counts.
    pub(crate) fn clear(&mut self) {
        self.entries = Vec::new();
        self.index = Vec::new();
        self.passing = Mask::default();
        self.free = NONE;
        self.newest = NONE;
        self.oldest = NONE;
        self.held = 0;
        self.pixels = 0;
    }

    /// Drops the mask handed out without being kept, if any. The
    /// rasteriser calls this once it has drawn a run of glyphs.
    pub(crate) fn release(&mut self) {
        if !self.passing.data.is_empty() {
            self.passing = Mask::default();
        }
    }

    /// The slot of the index where `key` is, and its entry.
    fn find(&self, key: &GlyphKey) -> Option<(usize, usize)> {
        let n = self.index.len();
        if n == 0 {
            return None;
        }
        let mut i = (key.hash() % n as u64) as usize;
        // The index is at most half full, so an empty slot ends the search.
        loop {
            let held = self.index[i];
            if held == 0 {
                return None;
            }
            let entry = held as usize - 1;
            if self.entries[entry].key == *key {
                return Some((i, entry));
            }
            i = if i + 1 == n { 0 } else { i + 1 };
        }
    }

    /// Adds entry `entry`, holding `key`, to the index.
    fn index_add(&mut self, key: &GlyphKey, entry: usize) {
        let n = self.index.len();
        let mut i = (key.hash() % n as u64) as usize;
        while self.index[i] != 0 {
            i = if i + 1 == n { 0 } else { i + 1 };
        }
        // Fewer than MAX_ROOM entries, so this fits.
        self.index[i] = (entry + 1) as u32;
    }

    /// Empties slot `at` of the index, moving back the entries after it
    /// that would no longer be found past the gap.
    fn index_remove(&mut self, at: usize) {
        let n = self.index.len();
        self.index[at] = 0;
        let mut hole = at;
        let mut j = if at + 1 == n { 0 } else { at + 1 };
        while self.index[j] != 0 {
            let held = self.index[j];
            let home = (self.entries[held as usize - 1].key.hash() % n as u64) as usize;
            // It moves into the hole if the hole lies between its home slot
            // and where it is, so a search from home passes the hole.
            if (j + n - home) % n >= (j + n - hole) % n {
                self.index[hole] = held;
                self.index[j] = 0;
                hole = j;
            }
            j = if j + 1 == n { 0 } else { j + 1 };
        }
    }

    /// Makes the index anew, twice as long as the entries' capacity.
    fn reindex(&mut self) {
        // Dropped first, so that the old and the new are not held at once.
        self.index = Vec::new();
        self.index = vec![0; 2 * self.entries.capacity()];
        let mut entry = self.oldest;
        while entry != NONE {
            let key = self.entries[entry].key;
            self.index_add(&key, entry);
            entry = self.entries[entry].newer;
        }
    }

    fn unlink(&mut self, entry: usize) {
        let (older, newer) = (self.entries[entry].older, self.entries[entry].newer);
        match older {
            NONE => self.oldest = newer,
            o => self.entries[o].newer = newer,
        }
        match newer {
            NONE => self.newest = older,
            n => self.entries[n].older = older,
        }
    }

    fn link_newest(&mut self, entry: usize) {
        self.entries[entry].older = self.newest;
        self.entries[entry].newer = NONE;
        match self.newest {
            NONE => self.oldest = entry,
            n => self.entries[n].newer = entry,
        }
        self.newest = entry;
    }

    /// Drops the least recently used mask; its entry becomes free.
    fn evict_oldest(&mut self) {
        let entry = self.oldest;
        let key = self.entries[entry].key;
        if let Some((at, _)) = self.find(&key) {
            self.index_remove(at);
        }
        self.unlink(entry);
        let mask = std::mem::take(&mut self.entries[entry].mask);
        self.pixels -= mask.data.len();
        self.entries[entry].older = self.free;
        self.free = entry;
        self.held -= 1;
        self.evictions += 1;
    }

    /// Makes the tables anew with room for `room` masks (at least those
    /// held), the masks in order of use, and nothing free.
    fn rebuild(&mut self, room: usize) {
        let mut old = std::mem::take(&mut self.entries);
        self.index = Vec::new();
        let mut entries: Vec<Entry> = Vec::with_capacity(room.max(self.held));
        let mut entry = self.oldest;
        while entry != NONE {
            let next = old[entry].newer;
            let n = entries.len();
            entries.push(Entry {
                key: old[entry].key,
                mask: std::mem::take(&mut old[entry].mask),
                older: n.checked_sub(1).unwrap_or(NONE),
                newer: NONE,
            });
            if n > 0 {
                entries[n - 1].newer = n;
            }
            entry = next;
        }
        drop(old);
        self.oldest = if entries.is_empty() { NONE } else { 0 };
        self.newest = entries.len().checked_sub(1).unwrap_or(NONE);
        self.free = NONE;
        self.entries = entries;
        self.reindex();
    }

    /// An entry for a new mask of `pixels` bytes, within the budget: the
    /// least recently used masks are dropped until its pixels fit beside
    /// the tables, then it takes a free entry, a new one if the tables can
    /// grow within the budget, or the least recently used mask's. `None`
    /// if it cannot be kept at all.
    fn place(&mut self, pixels: usize) -> Option<usize> {
        // Not even an empty cache with room for one mask could keep it.
        if pixels.saturating_add(SLOT_COST) > self.limit {
            return None;
        }
        while self.over(pixels) && self.oldest != NONE {
            self.evict_oldest();
            if self.entries.capacity() >= MIN_REBUILT && self.held * 4 < self.entries.capacity() {
                // Most of the tables' room goes unused: give half of it
                // back, so the room can go to pixels.
                self.rebuild(self.held * 2);
            }
        }
        if self.over(pixels) {
            // Nothing is held, and the tables alone leave no room.
            self.rebuild(0);
        }
        if self.free == NONE && self.entries.len() == self.entries.capacity() {
            let room = self.entries.capacity();
            let affordable = self
                .limit
                .saturating_sub(self.bytes())
                .saturating_sub(pixels)
                / SLOT_COST;
            let grow = room
                .max(4)
                .min(affordable)
                .min(MAX_ROOM.saturating_sub(room));
            if grow > 0 {
                self.entries.reserve_exact(grow);
                self.reindex();
            } else if self.oldest != NONE {
                self.evict_oldest();
            } else {
                return None;
            }
            // `reserve_exact` may give more than asked: check again.
            while self.over(pixels) && self.oldest != NONE {
                self.evict_oldest();
            }
            if self.over(pixels) {
                self.rebuild(0);
                return None;
            }
        }
        let entry = if self.free != NONE {
            let entry = self.free;
            self.free = self.entries[entry].older;
            entry
        } else {
            self.entries.push(Entry::default());
            self.entries.len() - 1
        };
        Some(entry)
    }

    /// The mask for `key`: the one held, marked as just used, or a new one
    /// from `draw`. A new mask is kept if it fits the budget once the least
    /// recently used masks are dropped; one that cannot fit is returned
    /// without being kept, and dropped by the next lookup or
    /// [`release`](GlyphCache::release). `None` if `draw` gives nothing.
    pub(crate) fn get_or_draw(
        &mut self,
        key: GlyphKey,
        draw: impl FnOnce() -> Option<Mask>,
    ) -> Option<&Mask> {
        self.release();
        if let Some((_, entry)) = self.find(&key) {
            self.hits += 1;
            if self.newest != entry {
                self.unlink(entry);
                self.link_newest(entry);
            }
            return Some(&self.entries[entry].mask);
        }
        self.misses += 1;
        let mask = draw()?;
        let pixels = mask.data.len();
        let Some(entry) = self.place(pixels) else {
            self.passing = mask;
            return Some(&self.passing);
        };
        self.entries[entry] = Entry {
            key,
            mask,
            older: NONE,
            newer: NONE,
        };
        self.link_newest(entry);
        self.index_add(&key, entry);
        self.held += 1;
        self.pixels += pixels;
        Some(&self.entries[entry].mask)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(glyph: u16) -> GlyphKey {
        GlyphKey {
            glyph,
            ppem: 12f32.to_bits(),
            coords: Coords::default(),
            subpixel: 0,
        }
    }

    fn mask(bytes: usize) -> Mask {
        Mask {
            left: 0,
            top: 0,
            width: bytes as u32,
            height: 1,
            data: vec![7; bytes].into_boxed_slice(),
        }
    }

    /// The glyphs held, from least to most recently used.
    fn order(cache: &GlyphCache) -> Vec<u16> {
        let mut out = Vec::new();
        let mut entry = cache.oldest;
        while entry != NONE {
            out.push(cache.entries[entry].key.glyph);
            entry = cache.entries[entry].newer;
        }
        out
    }

    /// Checks everything the cache keeps track of against what it holds.
    fn check(cache: &GlyphCache) {
        let held = order(cache);
        assert_eq!(held.len(), cache.held);
        let mut free = 0;
        let mut entry = cache.free;
        while entry != NONE {
            assert!(cache.entries[entry].mask.data.is_empty());
            free += 1;
            entry = cache.entries[entry].older;
        }
        assert_eq!(held.len() + free, cache.entries.len());
        let pixels: usize = cache.entries.iter().map(|e| e.mask.data.len()).sum();
        assert_eq!(pixels, cache.pixels);
        assert_eq!(cache.index.len(), 2 * cache.entries.capacity());
        assert_eq!(cache.index.iter().filter(|&&s| s != 0).count(), cache.held);
        let mut entry = cache.oldest;
        while entry != NONE {
            let key = cache.entries[entry].key;
            assert_eq!(cache.find(&key).map(|f| f.1), Some(entry));
            entry = cache.entries[entry].newer;
        }
        let stats = cache.stats();
        assert_eq!(
            stats.bytes,
            pixels
                + cache.entries.capacity() * size_of::<Entry>()
                + cache.index.capacity() * size_of::<u32>()
        );
        assert!(stats.bytes <= stats.limit, "{stats:?}");
    }

    #[test]
    fn the_least_recently_used_mask_goes_first() {
        // Tables with room for four masks, and room for the pixels of
        // three masks of 36 bytes, not four.
        let mut cache = GlyphCache::new(4 * SLOT_COST + 3 * 36);
        for glyph in 1..=3 {
            assert!(cache.get_or_draw(key(glyph), || Some(mask(36))).is_some());
        }
        assert_eq!(order(&cache), [1, 2, 3]);
        assert_eq!(cache.entries.capacity(), 4);
        assert_eq!(cache.stats().bytes, cache.stats().limit);
        // Using 1 makes 2 the oldest, so adding 4 drops 2.
        let mut drawn = false;
        cache.get_or_draw(key(1), || {
            drawn = true;
            None
        });
        assert!(!drawn, "a held mask is not drawn again");
        cache.get_or_draw(key(4), || Some(mask(36)));
        assert_eq!(order(&cache), [3, 1, 4]);
        let stats = cache.stats();
        assert_eq!((stats.masks, stats.evictions), (3, 1));
        assert_eq!(stats.bytes, stats.limit);
        assert_eq!((stats.hits, stats.misses), (1, 4));
        // A larger mask drops as many as it needs.
        cache.get_or_draw(key(5), || Some(mask(72)));
        assert_eq!(order(&cache), [4, 5]);
        check(&cache);
    }

    #[test]
    fn a_mask_larger_than_the_budget_is_given_but_not_kept() {
        let mut cache = GlyphCache::new(4 * SLOT_COST + 20);
        cache.get_or_draw(key(1), || Some(mask(20)));
        let big = cache.get_or_draw(key(2), || Some(mask(500))).cloned();
        assert_eq!(big.map(|m| m.data.len()), Some(500));
        assert_eq!(order(&cache), [1]);
        assert_eq!(cache.stats().bytes, 4 * SLOT_COST + 20);
        // It is held only until released.
        assert_eq!(cache.passing.data.len(), 500);
        cache.release();
        assert!(cache.passing.data.is_empty());
        check(&cache);
        // A budget too small for even one entry keeps nothing and holds no
        // tables.
        let mut tiny = GlyphCache::new(SLOT_COST - 1);
        assert!(tiny.get_or_draw(key(1), || Some(mask(0))).is_some());
        assert_eq!(tiny.stats().masks, 0);
        assert_eq!(tiny.stats().bytes, 0);
        assert_eq!(tiny.entries.capacity() + tiny.index.capacity(), 0);
    }

    #[test]
    fn shrinking_the_budget_drops_the_oldest_and_slots_are_reused() {
        let mut cache = GlyphCache::new(1 << 20);
        for glyph in 1..=5 {
            cache.get_or_draw(key(glyph), || Some(mask(36)));
        }
        assert_eq!(cache.entries.capacity(), 8);
        // Room for two masks in tables made just big enough for them.
        cache.set_limit(2 * SLOT_COST + 2 * 36);
        assert_eq!(order(&cache), [4, 5]);
        assert_eq!(cache.entries.capacity(), 2);
        assert_eq!(cache.stats().bytes, cache.stats().limit);
        check(&cache);
        cache.get_or_draw(key(6), || Some(mask(36)));
        assert_eq!(order(&cache), [5, 6]);
        // The dropped mask's entry was reused.
        assert_eq!(cache.entries.len(), 2);
        check(&cache);
        cache.clear();
        assert_eq!(cache.stats().masks, 0);
        assert_eq!(cache.stats().bytes, 0);
        assert_eq!(cache.stats().limit, 2 * SLOT_COST + 2 * 36);
        assert_eq!(cache.entries.capacity() + cache.index.capacity(), 0);
    }

    #[test]
    fn tables_grow_only_as_the_budget_allows_and_shrink_when_mostly_empty() {
        // Many masks of no pixels (spaces) fill the tables up to the
        // budget; tables never take more than it.
        let limit = 100 * SLOT_COST;
        let mut cache = GlyphCache::new(limit);
        for glyph in 0..1000 {
            cache.get_or_draw(key(glyph), || Some(mask(0)));
            check(&cache);
        }
        assert!(cache.entries.capacity() <= 100);
        assert!(cache.stats().masks >= 64, "{:?}", cache.stats());
        // Large masks then push the small ones out, and the tables, mostly
        // unused, are made smaller, leaving the room to pixels.
        for glyph in 1000..1040 {
            cache.get_or_draw(key(glyph), || Some(mask(limit / 4)));
            check(&cache);
        }
        assert!(
            cache.entries.capacity() < MIN_REBUILT,
            "{}",
            cache.entries.capacity()
        );
        assert!(cache.stats().masks >= 2, "{:?}", cache.stats());
    }

    #[test]
    fn a_random_workload_never_passes_the_budget() {
        let mut cache = GlyphCache::new(4_000);
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for step in 0..20_000 {
            let r = next();
            match r % 101 {
                0 => cache.set_limit((r >> 8) as usize % 9_000),
                1 => cache.clear(),
                _ => {
                    // Each glyph's mask has a size of its own, up to more
                    // than some budgets.
                    let glyph = (r >> 8) as u16 % 197;
                    let size = usize::from(glyph) * 37 % 1_200;
                    let got = cache.get_or_draw(key(glyph), || Some(mask(size)));
                    assert_eq!(got.map(|m| m.data.len()), Some(size));
                }
            }
            if step % 7 == 0 {
                cache.release();
            }
            check(&cache);
        }
        assert!(cache.stats().evictions > 0);
    }
}
