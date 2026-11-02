//! The window's back buffer and the conversion into it.

use manguebyte_gfx::{Color, IRect, MAX_SIDE, Pixmap};

use crate::event::{PhysicalPoint, PhysicalSize};

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

    /// Converts into `rect` of the buffer the pixels of `source`, whose
    /// top-left pixel is at `origin` in the buffer. Returns the part of
    /// `rect` that lies inside both, or `None` if none does.
    pub(crate) fn convert_at(
        &mut self,
        source: &Pixmap,
        rect: IRect,
        origin: PhysicalPoint,
    ) -> Option<IRect> {
        let placed = IRect::new(origin.x, origin.y, source.width(), source.height());
        let rect =
            rect.intersect(&placed)?
                .intersect(&IRect::new(0, 0, self.width, self.height))?;
        // Inside both, so every coordinate is in range and non-negative:
        // `rect` is at or past `origin`, and past the buffer's corner.
        let width = rect.width as usize;
        let src_x = (i64::from(rect.x) - i64::from(origin.x)) as usize;
        let src_y = (i64::from(rect.y) - i64::from(origin.y)) as usize;
        let src_stride = source.width() as usize * 4;
        let dst_stride = self.width as usize * 4;
        for row in 0..rect.height as usize {
            let from = (src_y + row) * src_stride + src_x * 4;
            let to = (rect.y as usize + row) * dst_stride + rect.x as usize * 4;
            rgba_to_bgra(
                &source.data()[from..from + width * 4],
                &mut self.bgra[to..to + width * 4],
            );
        }
        self.converted = self.converted.saturating_add(rect.area());
        Some(rect)
    }

    /// Fills `rect` with `color`. Returns the part of `rect` that lies
    /// inside the buffer, or `None` if none does.
    pub(crate) fn fill(&mut self, color: Color, rect: IRect) -> Option<IRect> {
        let rect = rect.intersect(&IRect::new(0, 0, self.width, self.height))?;
        let [r, g, b, a] = color.premultiply().to_array();
        let width = rect.width as usize;
        let stride = self.width as usize * 4;
        for row in 0..rect.height as usize {
            let to = (rect.y as usize + row) * stride + rect.x as usize * 4;
            for pixel in self.bgra[to..to + width * 4].chunks_exact_mut(4) {
                pixel.copy_from_slice(&[b, g, r, a]);
            }
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
    fn a_pixmap_shown_at_an_origin_lands_there_and_is_cut_to_the_buffer() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(8, 6));
        let source = pattern(3, 2);
        let origin = PhysicalPoint::new(4, 2);
        // A rectangle of the buffer that reaches past the pixmap.
        assert_eq!(
            back.convert_at(&source, IRect::new(5, 0, 100, 100), origin),
            Some(IRect::new(5, 2, 2, 2))
        );
        assert_eq!(back.converted(), 4);
        let frame = back.to_pixmap();
        for y in 0..6 {
            for x in 0..8 {
                let expected = if (5..7).contains(&x) && (2..4).contains(&y) {
                    source.pixel(x - 4, y - 2).expect("inside")
                } else {
                    PremulColor::TRANSPARENT
                };
                assert_eq!(frame.pixel(x, y), Some(expected), "({x}, {y})");
            }
        }
        // A pixmap reaching past the buffer's corners, and one outside it.
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(2, 2));
        let origin = PhysicalPoint::new(-1, -1);
        assert_eq!(
            back.convert_at(&source, IRect::new(0, 0, 2, 2), origin),
            Some(IRect::new(0, 0, 2, 1))
        );
        assert_eq!(
            back.to_pixmap().pixel(0, 0),
            source.pixel(1, 1),
            "the pixmap's pixel at 1, 1 is the buffer's at 0, 0"
        );
        let away = PhysicalPoint::new(i32::MAX, i32::MAX);
        assert_eq!(back.convert_at(&source, IRect::new(0, 0, 2, 2), away), None);
    }

    #[test]
    fn a_colour_fills_only_its_rectangle_in_the_buffers_byte_order() {
        let mut back = BackBuffer::empty();
        back.resize(PhysicalSize::new(4, 3));
        let color = Color::rgb(10, 20, 30);
        assert_eq!(
            back.fill(color, IRect::new(2, 1, 100, 1)),
            Some(IRect::new(2, 1, 2, 1))
        );
        assert_eq!(back.fill(color, IRect::new(9, 9, 1, 1)), None);
        assert_eq!(back.converted(), 2);
        assert_eq!(&back.bgra()[(4 + 2) * 4..(4 + 3) * 4], [30, 20, 10, 255]);
        let frame = back.to_pixmap();
        assert_eq!(frame.pixel(3, 1), Some(color.premultiply()));
        assert_eq!(frame.pixel(1, 1), Some(PremulColor::TRANSPARENT));
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
