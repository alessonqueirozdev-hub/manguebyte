//! Images a scene can draw.

use std::fmt;
use std::sync::Arc;

use crate::hash::Fnv;
use crate::pixmap::{ImageError, Pixmap};

/// How an image is sampled when it is drawn at another size or position
/// than its own pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Sampling {
    /// The nearest pixel: hard edges, for pixel art and exact copies.
    Nearest,
    /// A weighted mix of the four nearest pixels.
    #[default]
    Bilinear,
    /// A smoother mix of the sixteen nearest pixels, for enlarging.
    Bicubic,
}

/// An image a scene can draw: pixels that do not change, shared cheaply.
///
/// Cloning an `Image` shares its pixels instead of copying them, so a widget
/// can keep one and hand it to a scene on every frame. The content hash is
/// computed once, when the image is made, and names the image in a scene's
/// dump.
#[derive(Clone)]
pub struct Image {
    pixels: Arc<Pixmap>,
    hash: u64,
}

impl Image {
    /// An image of these pixels.
    pub fn new(pixels: Pixmap) -> Self {
        let mut hash = Fnv::new();
        hash.u32(pixels.width());
        hash.u32(pixels.height());
        hash.bytes(pixels.data());
        Image {
            pixels: Arc::new(pixels),
            hash: hash.finish(),
        }
    }

    /// An image decoded from PNG data.
    ///
    /// # Errors
    ///
    /// As for [`Pixmap::decode_png`].
    pub fn decode_png(bytes: &[u8]) -> Result<Self, ImageError> {
        Pixmap::decode_png(bytes).map(Image::new)
    }

    /// The pixels.
    pub fn pixels(&self) -> &Pixmap {
        &self.pixels
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.pixels.width()
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.pixels.height()
    }

    /// A 64-bit FNV-1a hash of the size and the pixels: equal images have
    /// equal hashes on every platform.
    pub fn content_hash(&self) -> u64 {
        self.hash
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Image")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("hash", &format_args!("{:016x}", self.hash))
            .finish()
    }
}

impl PartialEq for Image {
    fn eq(&self, other: &Image) -> bool {
        Arc::ptr_eq(&self.pixels, &other.pixels)
            || (self.hash == other.hash && self.pixels == other.pixels)
    }
}
