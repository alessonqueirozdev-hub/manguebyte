//! An owned buffer of premultiplied RGBA8 pixels.

use std::fmt;

use crate::color::{Color, PremulColor};
use crate::geom::IRect;
use crate::raster::skia;

/// The largest width or height a [`Pixmap`] may have, in pixels.
///
/// A square pixmap of this size takes 1 GiB. Larger buffers are refused
/// rather than attempted, so a corrupt size never asks the allocator for
/// more than that.
pub const MAX_SIDE: u32 = 16_384;

/// A rectangle of pixels in memory: premultiplied RGBA, 8 bits per channel,
/// rows from top to bottom with no padding.
///
/// The byte order is red, green, blue, alpha, and every colour channel is at
/// most the alpha of its pixel; the methods keep it that way. A pixmap may
/// be empty (zero wide or high), and drawing into an empty pixmap does
/// nothing.
///
/// ```
/// use manguebyte_gfx::{Color, IRect, Pixmap};
///
/// let mut pixels = Pixmap::new(4, 2).expect("small");
/// pixels.fill_rect(IRect::new(1, 0, 2, 2), Color::rgba(255, 0, 0, 128));
/// // Stored premultiplied: red 255 at alpha 128 is red 128.
/// assert_eq!(pixels.pixel(1, 1).expect("inside").to_array(), [128, 0, 0, 128]);
///
/// // PNG keeps every pixel exactly.
/// let png = pixels.encode_png().expect("not empty");
/// assert_eq!(Pixmap::decode_png(&png), Ok(pixels));
/// ```
#[derive(Clone, PartialEq, Eq, Default)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Vec<u8>,
}

impl fmt::Debug for Pixmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pixmap")
            .field("width", &self.width)
            .field("height", &self.height)
            .finish_non_exhaustive()
    }
}

/// Why an image could not be made, read or written.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ImageError {
    /// A side is larger than [`MAX_SIDE`], or a decoded image would take
    /// more than 64 MiB (a 4096 by 4096 RGBA image).
    TooLarge {
        /// The width asked for.
        width: u32,
        /// The height asked for.
        height: u32,
    },
    /// The data is not four bytes for each pixel.
    WrongLength {
        /// The number of bytes needed.
        expected: usize,
        /// The number of bytes given.
        actual: usize,
    },
    /// A pixel has a colour channel larger than its alpha, so it cannot be
    /// premultiplied.
    NotPremultiplied {
        /// The index of the first such pixel.
        pixel: usize,
    },
    /// An empty image cannot be written as PNG.
    Empty,
    /// The PNG data could not be read or written.
    Png(String),
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::TooLarge { width, height } => write!(
                f,
                "a {width}x{height} image is larger than {MAX_SIDE} pixels on a side                  or 64 MiB of pixels"
            ),
            ImageError::WrongLength { expected, actual } => {
                write!(f, "expected {expected} bytes of pixels, got {actual}")
            }
            ImageError::NotPremultiplied { pixel } => write!(
                f,
                "pixel {pixel} has a colour channel larger than its alpha"
            ),
            ImageError::Empty => f.write_str("an empty image cannot be encoded as PNG"),
            ImageError::Png(message) => write!(f, "PNG: {message}"),
        }
    }
}

impl std::error::Error for ImageError {}

fn byte_len(width: u32, height: u32) -> Result<usize, ImageError> {
    if width > MAX_SIDE || height > MAX_SIDE {
        return Err(ImageError::TooLarge { width, height });
    }
    // At most 16384 * 16384 * 4 = 2^30, which fits a usize on every
    // platform Rust supports with std.
    Ok(width as usize * height as usize * 4)
}

impl Pixmap {
    /// A transparent pixmap, or `None` if a side is larger than
    /// [`MAX_SIDE`].
    pub fn new(width: u32, height: u32) -> Option<Self> {
        let len = byte_len(width, height).ok()?;
        Some(Pixmap {
            width,
            height,
            data: vec![0; len],
        })
    }

    /// A pixmap that takes ownership of premultiplied RGBA bytes.
    ///
    /// # Errors
    ///
    /// [`ImageError::TooLarge`], [`ImageError::WrongLength`], or
    /// [`ImageError::NotPremultiplied`] if a channel exceeds its alpha.
    pub fn from_premultiplied(width: u32, height: u32, data: Vec<u8>) -> Result<Self, ImageError> {
        let expected = byte_len(width, height)?;
        if data.len() != expected {
            return Err(ImageError::WrongLength {
                expected,
                actual: data.len(),
            });
        }
        if let Some(pixel) = data
            .chunks_exact(4)
            .position(|p| p[0] > p[3] || p[1] > p[3] || p[2] > p[3])
        {
            return Err(ImageError::NotPremultiplied { pixel });
        }
        Ok(Pixmap {
            width,
            height,
            data,
        })
    }

    /// A pixmap from straight-alpha RGBA bytes, which are premultiplied.
    ///
    /// # Errors
    ///
    /// [`ImageError::TooLarge`] or [`ImageError::WrongLength`].
    pub fn from_straight(width: u32, height: u32, rgba: &[u8]) -> Result<Self, ImageError> {
        let expected = byte_len(width, height)?;
        if rgba.len() != expected {
            return Err(ImageError::WrongLength {
                expected,
                actual: rgba.len(),
            });
        }
        let mut data = Vec::with_capacity(expected);
        for p in rgba.chunks_exact(4) {
            data.extend_from_slice(&Color::rgba(p[0], p[1], p[2], p[3]).premultiply().to_array());
        }
        Ok(Pixmap {
            width,
            height,
            data,
        })
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// The rectangle `0, 0, width, height`.
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width, self.height)
    }

    /// Whether it has no pixel.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The premultiplied RGBA bytes, row after row.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Gives the bytes away.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }

    /// The bytes, for the rasteriser, which keeps them premultiplied.
    pub(crate) fn data_mut(&mut self) -> &mut [u8] {
        &mut self.data
    }

    fn offset(&self, x: u32, y: u32) -> Option<usize> {
        (x < self.width && y < self.height)
            .then(|| (y as usize * self.width as usize + x as usize) * 4)
    }

    /// The pixel at `(x, y)`, or `None` outside the pixmap.
    pub fn pixel(&self, x: u32, y: u32) -> Option<PremulColor> {
        let i = self.offset(x, y)?;
        let p = &self.data[i..i + 4];
        PremulColor::new(p[0], p[1], p[2], p[3])
    }

    /// Replaces the pixel at `(x, y)`; returns `false`, changing nothing,
    /// outside the pixmap.
    pub fn set_pixel(&mut self, x: u32, y: u32, color: PremulColor) -> bool {
        match self.offset(x, y) {
            Some(i) => {
                self.data[i..i + 4].copy_from_slice(&color.to_array());
                true
            }
            None => false,
        }
    }

    /// Replaces every pixel with `color`.
    pub fn fill(&mut self, color: Color) {
        let bytes = color.premultiply().to_array();
        for pixel in self.data.chunks_exact_mut(4) {
            pixel.copy_from_slice(&bytes);
        }
    }

    /// Replaces the pixels of `rect` that lie inside the pixmap with
    /// `color`. Nothing is blended.
    pub fn fill_rect(&mut self, rect: IRect, color: Color) {
        let Some(rect) = rect.intersect(&self.bounds()) else {
            return;
        };
        let bytes = color.premultiply().to_array();
        let row_len = self.width as usize * 4;
        for y in rect.y as usize..rect.bottom() as usize {
            let start = y * row_len + rect.x as usize * 4;
            let row = &mut self.data[start..start + rect.width as usize * 4];
            for pixel in row.chunks_exact_mut(4) {
                pixel.copy_from_slice(&bytes);
            }
        }
    }

    /// A copy of the pixels of `rect` that lie inside the pixmap, or `None`
    /// if none do.
    pub fn crop(&self, rect: IRect) -> Option<Pixmap> {
        let rect = rect.intersect(&self.bounds())?;
        let mut out = Pixmap::new(rect.width, rect.height)?;
        out.copy_from(self, rect, 0, 0);
        Some(out)
    }

    /// Copies the pixels of `from` in `source` so that its top-left pixel
    /// lands at `(to_x, to_y)` here, replacing what was there. Pixels that
    /// fall outside either pixmap are skipped.
    pub fn copy_from(&mut self, source: &Pixmap, from: IRect, to_x: i32, to_y: i32) {
        let Some(clipped) = from.intersect(&source.bounds()) else {
            return;
        };
        // Where the clipped source lands, then clip that to this pixmap.
        let dx = i64::from(to_x) - i64::from(from.x);
        let dy = i64::from(to_y) - i64::from(from.y);
        let landing_x = i64::from(clipped.x) + dx;
        let landing_y = i64::from(clipped.y) + dy;
        let x0 = landing_x.max(0);
        let y0 = landing_y.max(0);
        let x1 = (landing_x + i64::from(clipped.width)).min(i64::from(self.width));
        let y1 = (landing_y + i64::from(clipped.height)).min(i64::from(self.height));
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let row_bytes = (x1 - x0) as usize * 4;
        let src_row_len = source.width as usize * 4;
        let dst_row_len = self.width as usize * 4;
        for y in y0..y1 {
            let src_y = (y - dy) as usize;
            let src_x = (x0 - dx) as usize;
            let src_start = src_y * src_row_len + src_x * 4;
            let dst_start = y as usize * dst_row_len + x0 as usize * 4;
            self.data[dst_start..dst_start + row_bytes]
                .copy_from_slice(&source.data[src_start..src_start + row_bytes]);
        }
    }

    /// Encodes the pixels as an 8-bit RGBA PNG, with alpha straightened.
    ///
    /// Decoding the result with [`Pixmap::decode_png`] gives back exactly
    /// these pixels.
    ///
    /// # Errors
    ///
    /// [`ImageError::Empty`] for an empty pixmap, or [`ImageError::Png`].
    pub fn encode_png(&self) -> Result<Vec<u8>, ImageError> {
        if self.is_empty() {
            return Err(ImageError::Empty);
        }
        skia::encode_png(&self.data, self.width, self.height)
    }

    /// Decodes a PNG of any colour type and bit depth into premultiplied
    /// pixels. Sixteen-bit channels are reduced to eight, palettes and grey
    /// are expanded, and only the first frame of an animated PNG is read.
    ///
    /// Hostile input is safe: malformed data is an error, and an image whose
    /// pixels would need more than 64 MiB, or that is wider or taller than
    /// [`MAX_SIDE`], is refused from its header, before the decoder
    /// allocates anything; no buffer the decoder makes is larger than
    /// 64 MiB.
    ///
    /// # Errors
    ///
    /// [`ImageError::Png`] or [`ImageError::TooLarge`].
    pub fn decode_png(bytes: &[u8]) -> Result<Pixmap, ImageError> {
        let (data, width, height) = skia::decode_png(bytes)?;
        Pixmap::from_premultiplied(width, height, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(width: u32, height: u32) -> Pixmap {
        let mut p = Pixmap::new(width, height).expect("small");
        for y in 0..height {
            for x in 0..width {
                let v = (y * width + x) as u8;
                p.set_pixel(x, y, PremulColor::new(v / 2, 0, 0, v).expect("valid"));
            }
        }
        p
    }

    #[test]
    fn sizes_are_checked() {
        assert!(Pixmap::new(MAX_SIDE, 1).is_some());
        assert!(Pixmap::new(MAX_SIDE + 1, 1).is_none());
        assert!(Pixmap::new(0, 7).expect("empty is allowed").is_empty());
        assert_eq!(
            Pixmap::from_premultiplied(2, 1, vec![0; 7]),
            Err(ImageError::WrongLength {
                expected: 8,
                actual: 7
            })
        );
        assert_eq!(
            Pixmap::from_premultiplied(2, 1, vec![0, 0, 0, 0, 9, 0, 0, 8]),
            Err(ImageError::NotPremultiplied { pixel: 1 })
        );
    }

    #[test]
    fn straight_pixels_are_premultiplied_on_the_way_in() {
        let p = Pixmap::from_straight(1, 1, &[255, 128, 0, 128]).expect("valid");
        assert_eq!(p.data(), &[128, 64, 0, 128]);
    }

    #[test]
    fn filling_a_rectangle_is_clipped_and_exact() {
        let mut p = Pixmap::new(4, 3).expect("small");
        p.fill_rect(IRect::new(2, 1, 10, 10), Color::rgb(1, 2, 3));
        let filled: Vec<(u32, u32)> = (0..3)
            .flat_map(|y| (0..4).map(move |x| (x, y)))
            .filter(|&(x, y)| p.pixel(x, y).expect("inside").alpha() == 255)
            .collect();
        assert_eq!(filled, vec![(2, 1), (3, 1), (2, 2), (3, 2)]);
    }

    #[test]
    fn copies_are_clipped_on_both_sides() {
        let source = pattern(4, 4);
        let mut target = Pixmap::new(3, 3).expect("small");
        // From (1, 1) of the source, landing at (-1, 2): only the source
        // pixels (2, 1), (3, 1) reach the target at (0, 2), (1, 2).
        target.copy_from(&source, IRect::new(1, 1, 3, 3), -1, 2);
        assert_eq!(target.pixel(0, 2), source.pixel(2, 1));
        assert_eq!(target.pixel(1, 2), source.pixel(3, 1));
        assert_eq!(target.pixel(2, 2), Some(PremulColor::TRANSPARENT));
        assert_eq!(target.pixel(0, 1), Some(PremulColor::TRANSPARENT));
        let crop = source.crop(IRect::new(3, 3, 5, 5)).expect("one pixel");
        assert_eq!((crop.width(), crop.height()), (1, 1));
        assert_eq!(crop.pixel(0, 0), source.pixel(3, 3));
        assert!(source.crop(IRect::new(4, 0, 1, 1)).is_none());
    }
}
