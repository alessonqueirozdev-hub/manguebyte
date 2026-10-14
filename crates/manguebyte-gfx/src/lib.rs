//! 2D graphics for the Manguebyte user interface.
//!
//! This crate turns what widgets draw into pixels, on the CPU, with no
//! window, display or GPU involved, so everything in it runs in a unit test
//! on any machine. It is the bottom layer of the interface described in
//! ADR 0007 and is never a dependency of anything that processes audio.
//!
//! # The pieces
//!
//! - **Geometry** ([`Point`], [`Size`], [`Rect`], [`IRect`], [`RoundedRect`],
//!   [`Circle`], [`Arc`], [`Transform`]) in logical pixels, `f32`, with `y`
//!   growing downwards. [`IRect`] is a rectangle of whole device pixels.
//! - **Colour** ([`Color`], straight alpha, and [`PremulColor`], alpha
//!   premultiplied), both sRGB-encoded with 8 bits per channel. Blending
//!   happens in sRGB space, as browsers and design tools do.
//! - **[`Pixmap`]**: an owned buffer of premultiplied RGBA8 pixels, with PNG
//!   encoding and decoding.
//! - **[`Scene`]**: a display list a widget records: fills and strokes of
//!   shapes and [`Path`]s with solid colours or gradients ([`Paint`]),
//!   [`Image`]s, text, clips, transforms, opacity layers, and
//!   [`Scene::columns`], the primitive for waveforms, spectra and meters.
//!   [`Scene::dump`] prints it as exact text for tests.
//! - **[Text](text)**: a [`TextStyle`] (size, weight, optical size,
//!   tabular figures, colour, alignment, a limit on lines) and a
//!   [`TextLayout`] of one paragraph, shaped, broken into lines and
//!   measured, drawn with [`Scene::text`].
//! - **[`Renderer`]**: draws a scene into a pixmap at a scale factor, either
//!   whole or only inside damage rectangles; a partial render changes
//!   nothing outside the damage and draws every pixel inside it exactly as
//!   a whole render would. It keeps a bounded cache of glyph masks. The
//!   rasteriser behind it is a private seam; today it is `tiny-skia`, and
//!   no type of it appears here.
//! - **[`snapshot`]**: golden-image and dump comparison for tests.
//!
//! # The typeface
//!
//! Text is set in a subset of Inter 4.1, by Rasmus Andersson and the Inter
//! Project Authors, embedded in the crate under the SIL Open Font License
//! 1.1. The font file, the licence (`fonts/OFL.txt`, also
//! [`TYPEFACE_LICENCE`]) and how the subset was made (`fonts/README.md`)
//! are published with the crate in its `fonts/` directory. Outlines,
//! metrics and variations are read with `skrifa` and text is shaped with
//! `harfrust`, each behind a private module; neither appears in a public
//! signature.
//!
//! Every program built with this crate contains the font, and the licence
//! asks that its copyright notice and licence go with every copy. The font
//! file names the licence but does not hold its text, so **a program that
//! ships this crate must ship the licence too**: as a file beside it, or
//! shown in it (an "about" page), from [`TYPEFACE_LICENCE`].
//!
//! # Example
//!
//! ```
//! use manguebyte_gfx::{Color, Pixmap, Rect, Renderer, Scene};
//!
//! let mut scene = Scene::new();
//! scene.fill(Rect::new(0.0, 0.0, 8.0, 4.0), Color::rgb(0xFF, 0x7A, 0x00));
//!
//! // The scene is drawn at 2x, so 8 by 4 logical pixels cover 16 by 8.
//! let mut pixels = Pixmap::new(16, 8).expect("a small pixmap");
//! Renderer::new().render(&scene, &mut pixels, 2.0);
//!
//! let corner = pixels.pixel(15, 7).expect("inside");
//! assert_eq!(corner.to_array(), [0xFF, 0x7A, 0x00, 0xFF]);
//! assert_eq!(scene.dump(), "fill rect(0 0 8 4) solid(#ff7a00ff)\n");
//! ```
//!
//! # Threads and hygiene
//!
//! Nothing here keeps global state or starts a thread. Apart from the test
//! helpers in [`snapshot`], which read two environment variables
//! (`MANGUEBYTE_BLESS` and `CI`) when a [`snapshot::Snapshots`] is made and
//! write files when a check fails or blessing is asked for, nothing reads a
//! setting of the process or touches the operating system: drawing,
//! recording, text layout and PNG coding work on memory only, and the
//! typeface is compiled in rather than loaded. Every type is `Send`, so a
//! scene may be rendered off the thread that recorded it, although the
//! interface itself runs on one thread. The crate has no `unsafe` code.

#![forbid(unsafe_code)]

mod color;
mod geom;
mod hash;
mod image;
mod paint;
mod path;
mod pixmap;
mod raster;
mod scene;
pub mod snapshot;
pub mod text;
mod trig;

pub use color::{Color, PremulColor, linear_to_srgb, srgb_to_linear};
pub use geom::{Arc, Circle, IRect, Point, Radii, Rect, RoundedRect, Size, Transform};
pub use image::{Image, Sampling};
pub use paint::{Cap, Dash, Extend, Join, LinearGradient, Paint, RadialGradient, Stop, Stroke};
pub use path::{FillRule, Path, Segment};
pub use pixmap::{ImageError, MAX_SIDE, Pixmap};
pub use raster::Renderer;
pub use scene::{Layer, MAX_DEPTH, Scene, Shape};
pub use text::{
    Align, Caret, FontMetrics, Glyph, GlyphCacheStats, Line, OpticalSize, TYPEFACE_LICENCE,
    TextLayout, TextStyle,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn send<T: Send>() {}
    fn sync<T: Sync>() {}

    #[test]
    fn every_type_can_be_sent_to_another_thread() {
        send::<Scene>();
        send::<Renderer>();
        send::<Pixmap>();
        send::<Image>();
        send::<Path>();
        send::<snapshot::Snapshots>();
        send::<TextLayout>();
        // Scenes, images, pixmaps and layouts can also be read from several
        // threads.
        sync::<TextLayout>();
        sync::<Scene>();
        sync::<Image>();
        sync::<Pixmap>();
    }
}
