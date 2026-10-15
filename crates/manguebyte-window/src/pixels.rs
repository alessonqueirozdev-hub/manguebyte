//! The window's back buffer and the conversion into it.

use manguebyte_gfx::{IRect, MAX_SIDE, Pixmap};

use crate::event::PhysicalSize;

/// What the window shows, in the byte order GDI reads: blue, green, red,
/// then alpha, premultiplied, rows top to bottom with no padding.
///
/// Presenting converts only the rectangles asked for; an expose copies from
/// here and never asks the handler. Pixels never presented are black.
pub(crate) struct BackBuffer {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    /// Pixels converted since the window was made, for tests and
    /// diagnostics.
    converted: u64,
}

impl BackBuffer {
    /// An empty buffer.
    pub(crate) fn empty() -> Self {
        BackBuffer {
            width: 0,
            height: 0,
            bgra: Vec::new(),
            converted: 0,
        }
    }

    /// The buffer's size. It is the window's size, unless that is larger
    /// than [`MAX_SIDE`] or memory ran out, in which case it is smaller.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn size(&self) -> PhysicalSize {
        PhysicalSize::new(self.width, self.height)
    }

    /// The pixels, blue-green-red-alpha.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    /// Pixels converted so far.
    pub(crate) fn converted(&self) -> u64 {
        self.converted
    }

    /// Makes the buffer `size`, keeping the pixels both sizes share and
    /// making the new ones black. A side above [`MAX_SIDE`] is cut to it;
    /// if memory runs out the buffer becomes empty.
    pub(crate) fn resize(&mut self, size: PhysicalSize) {
        let width = size.width.min(MAX_SIDE);
        let height = size.height.min(MAX_SIDE);
        if width == self.width && height == self.height {
            return;
        }
        // At most 16384 * 16384 * 4 bytes, which fits a usize.
        let len = width as usize * height as usize * 4;
        let mut next = Vec::new();
        if next.try_reserve_exact(len).is_err() {
            *self = BackBuffer {
                converted: self.converted,
                ..BackBuffer::empty()
            };
            return;
        }
        next.resize(len, 0);
        let keep_width = self.width.min(width) as usize * 4;
        let keep_rows = self.height.min(height) as usize;
        let old_stride = self.width as usize * 4;
        let new_stride = width as usize * 4;
        for row in 0..keep_rows {
            next[row * new_stride..row * new_stride + keep_width]
                .copy_from_slice(&self.bgra[row * old_stride..row * old_stride + keep_width]);
        }
        self.width = width;
        self.height = height;
        self.bgra = next;
    }

    /// Converts the pixels of `rect` from `source` into the buffer, at the
    /// same position. Returns the part of `rect` that lies inside both, or
    /// `None` if none does.
    pub(crate) fn convert(&mut self, source: &Pixmap, rect: IRect) -> Option<IRect> {
        let rect = rect.intersect(&source.bounds())?.intersect(&IRect::new(
            0,
            0,
            self.width,
            self.height,
        ))?;
        // Inside both buffers, so every coordinate is in range and
        // non-negative.
        let x = rect.x as usize;
        let width = rect.width as usize;
        let src_stride = source.width() as usize * 4;
        let dst_stride = self.width as usize * 4;
        for y in rect.y as usize..rect.y as usize + rect.height as usize {
            let src = &source.data()[y * src_stride + x * 4..y * src_stride + (x + width) * 4];
            let dst = &mut self.bgra[y * dst_stride + x * 4..y * dst_stride + (x + width) * 4];
            rgba_to_bgra(src, dst);
        }
        self.converted = self.converted.saturating_add(rect.area());
        Some(rect)
    }

    /// The buffer as a premultiplied RGBA pixmap.
    pub(crate) fn to_pixmap(&self) -> Pixmap {
        let mut rgba = vec![0; self.bgra.len()];
        rgba_to_bgra(&self.bgra, &mut rgba);
        Pixmap::from_premultiplied(self.width, self.height, rgba).unwrap_or_default()
    }
}

/// Swaps red and blue: premultiplied RGBA to premultiplied BGRA, or back.
/// Both slices hold whole pixels; extra bytes of the longer one are left
/// alone.
pub(crate) fn rgba_to_bgra(src: &[u8], dst: &mut [u8]) {
    for (from, to) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        to[0] = from[2];
        to[1] = from[1];
        to[2] = from[0];
        to[3] = from[3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use manguebyte_gfx::PremulColor;

    fn pattern(width: u32, height: u32) -> Pixmap {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let a = 200 + (x % 50) as u8;
                data.extend_from_slice(&[(x * 7 % 200) as u8, (y * 13 % 200) as u8, 31, a]);
            }
        }
        Pixmap::from_premultiplied(width, height, data).expect("premultiplied")
    }

    #[test]
    fn conversion_swaps_red_and_blue_and_keeps_alpha() {
        let mut out = [0u8; 8];
        rgba_to_bgra(&[1, 2, 3, 4, 10, 20, 30, 40], &mut out);
        assert_eq!(out, [3, 2, 1, 4, 30, 20, 10, 40]);
    }

    #[test]
    fn only_the_rectangle_is_converted() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(8, 6));
        let source = pattern(8, 6);
        assert_eq!(
            back.convert(&source, IRect::new(2, 1, 3, 2)),
            Some(IRect::new(2, 1, 3, 2))
        );
        assert_eq!(back.converted(), 6);
        let frame = back.to_pixmap();
        for y in 0..6 {
            for x in 0..8 {
                let inside = (2..5).contains(&x) && (1..3).contains(&y);
                let expected = if inside {
                    source.pixel(x, y).expect("inside")
                } else {
                    PremulColor::TRANSPARENT
                };
                assert_eq!(frame.pixel(x, y), Some(expected), "({x}, {y})");
            }
        }
    }

    #[test]
    fn rectangles_are_clipped_to_both_buffers() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(4, 4));
        let source = pattern(6, 2);
        assert_eq!(
            back.convert(&source, IRect::new(-3, -3, 100, 100)),
            Some(IRect::new(0, 0, 4, 2))
        );
        assert_eq!(back.convert(&source, IRect::new(4, 0, 2, 2)), None);
        assert_eq!(
            back.convert(&source, IRect::new(i32::MIN, 0, u32::MAX, 1)),
            Some(IRect::new(0, 0, 4, 1))
        );
        assert_eq!(back.converted(), 12);
    }

    #[test]
    fn resizing_keeps_the_shared_pixels() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(5, 4));
        let source = pattern(5, 4);
        back.convert(&source, source.bounds());
        back.resize(PhysicalSize::new(3, 6));
        let frame = back.to_pixmap();
        assert_eq!((frame.width(), frame.height()), (3, 6));
        for y in 0..6 {
            for x in 0..3 {
                let expected = if y < 4 {
                    source.pixel(x, y).expect("inside")
                } else {
                    PremulColor::TRANSPARENT
                };
                assert_eq!(frame.pixel(x, y), Some(expected), "({x}, {y})");
            }
        }
    }

    #[test]
    fn sides_above_the_limit_are_cut() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(MAX_SIDE + 1, 1));
        assert_eq!(back.size(), PhysicalSize::new(MAX_SIDE, 1));
    }
}
