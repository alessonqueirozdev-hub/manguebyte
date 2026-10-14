//! Text: a style, a paragraph laid out in lines, and the glyph cache.
//!
//! # The typeface
//!
//! Text is set in the one typeface embedded in this crate: a subset of
//! Inter 4.1 by Rasmus Andersson and the Inter Project Authors, under the
//! SIL Open Font License 1.1. The font file, its licence and how it was
//! made are in the crate's `fonts/` directory, which is published with the
//! crate. The subset keeps both variation axes: weight from 100 to 900 and
//! optical size from 14 to 32.
//!
//! The font is compiled into every program that uses this crate, so such a
//! program distributes it, and the licence asks that its copyright notice
//! and licence go with every copy. The font file itself carries only a
//! short notice naming the licence and a link to it, not the licence's
//! text. A program that ships this crate must therefore ship the licence:
//! as a file beside the program, or shown in the program (an "about"
//! page), where [`TYPEFACE_LICENCE`] has the full text.
//!
//! # What layout does, and its limits
//!
//! A [`TextLayout`] is one paragraph, shaped with the font's own rules
//! (kerning, contextual alternates such as `->` becoming an arrow, and
//! tabular figures when the style asks for them), broken into lines at
//! spaces to fit an optional width, aligned to the start, centre or end,
//! and cut with an ellipsis when it needs more lines than the style allows.
//! It knows where each glyph goes, how large it is, where its baseline is,
//! and which character index lies at which position, for text entry later.
//!
//! Each line is drawn and measured as its own text shaped alone: where
//! kerning or a contextual rule reaches across a break (a word broken
//! between "A" and "V", a comma kerned against the space after it), the
//! line is shaped again on its own, and a line cut short is shaped together
//! with its ellipsis, so the last character kept is kerned against it.
//! Lines are fitted by comparing their widths, in logical pixels as
//! [`TextLayout::size`] reports them, with the maximum width.
//!
//! The limits are deliberate for the first phase of the interface:
//!
//! - **Left to right only.** There is no bidirectional reordering:
//!   right-to-left text is laid out left to right.
//! - **Latin only.** Every run is shaped as Latin script. Scripts that
//!   need complex shaping (Arabic, Indic, Thai and others) are not
//!   supported, and the subset has no glyphs for them anyway.
//! - **The glyphs the subset has.** About 360 characters: Latin for
//!   Western, Central and Southern European languages, punctuation, the
//!   symbols of values and units, arrows, and figures. Any other character
//!   is drawn as the font's missing-glyph box. There is no font fallback.
//! - **Breaks only at spaces.** A line breaks after a run of spaces
//!   (U+0020; a tab or other control character counts as a space, while a
//!   no-break space does not). A word wider than the whole line is broken
//!   between characters. There is no hyphenation and no breaking at
//!   hyphens or slashes. Line feeds, carriage returns and the Unicode line
//!   and paragraph separators force a break.
//! - **One style per paragraph.** No mixed sizes, weights or colours
//!   within a layout.
//!
//! # Example
//!
//! ```
//! use manguebyte_gfx::{Color, Pixmap, Point, Renderer, Scene, TextLayout, TextStyle};
//!
//! let style = TextStyle::new(12.0)
//!     .with_tabular_figures(true)
//!     .with_color(Color::WHITE);
//! let value = TextLayout::new("-6.0 dB", &style, None);
//! let size = value.size();
//! assert!(size.width > 30.0 && size.width < 50.0);
//!
//! let mut scene = Scene::new();
//! scene.text(&value, Point::new(2.0, 2.0));
//! let mut pixels = Pixmap::new(60, 20).expect("small");
//! Renderer::new().render(&scene, &mut pixels, 1.0);
//! // Something was drawn on the first baseline, and nothing far below it.
//! let baseline = (2.0 + value.baseline()) as u32;
//! assert!((0..60).any(|x| pixels.pixel(x, baseline - 2).is_some_and(|p| p.alpha() > 0)));
//! assert!((0..60).all(|x| pixels.pixel(x, 19).is_some_and(|p| p.alpha() == 0)));
//! ```

mod cache;
pub(crate) mod font;
mod layout;
pub(crate) mod shape;

pub use cache::GlyphCacheStats;
pub(crate) use cache::{GlyphCache, GlyphKey, Mask};
pub use layout::{Caret, Glyph, Line, TextLayout};

use crate::color::Color;

/// The licence of the embedded typeface: the full text of the SIL Open Font
/// License 1.1, with Inter's copyright line.
///
/// The licence asks that the copyright notice and the licence go with
/// every copy of the font, and every program that uses this crate holds a
/// copy. The font file names the licence and links to it (its `name`
/// table, entries 13 and 14) but does not hold its text, so a program that
/// ships this crate must ship this text too: as a file beside it (the
/// crate's `fonts/OFL.txt`, published with the crate, is the same text) or
/// shown in the program, for example on an "about" page.
///
/// ```
/// use manguebyte_gfx::TYPEFACE_LICENCE;
///
/// assert!(TYPEFACE_LICENCE.starts_with("Copyright (c) 2016 The Inter Project Authors"));
/// assert!(TYPEFACE_LICENCE.contains("PERMISSION AND CONDITIONS"));
/// ```
pub const TYPEFACE_LICENCE: &str = include_str!("../../fonts/OFL.txt");

/// Where the lines of a paragraph sit across its width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Align {
    /// Against the left edge.
    #[default]
    Start,
    /// Centred.
    Center,
    /// Against the right edge.
    End,
}

/// How the typeface's optical-size axis is set.
///
/// Inter's optical size runs from 14 ("text", the default, with looser
/// spacing and sturdier details) to 32 ("display", tighter and finer).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum OpticalSize {
    /// Follow the size of the text in logical pixels, clamped to the axis,
    /// as CSS's `font-optical-sizing: auto` does.
    #[default]
    Auto,
    /// This value, clamped to the axis (from 14 to 32).
    Fixed(f32),
}

/// How a paragraph of text looks.
///
/// Built with [`TextStyle::new`] and the `with_` methods. More fields may
/// be added, so the struct cannot be built literally; its fields can be
/// read and changed.
///
/// ```
/// use manguebyte_gfx::{Align, Color, TextStyle};
///
/// let label = TextStyle::new(12.0)
///     .with_weight(600.0)
///     .with_color(Color::hex(0xE8E8E8))
///     .with_align(Align::Center)
///     .with_max_lines(1);
/// assert_eq!(label.size, 12.0);
/// assert_eq!(label.max_lines, Some(1));
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub struct TextStyle {
    /// The size of the em in logical pixels. A size that is not a positive
    /// finite number lays out nothing; sizes above 65,536 are clamped.
    pub size: f32,
    /// The weight, from 100 (thin) through 400 (regular) and 700 (bold) to
    /// 900 (black). Values outside that range are clamped; `NaN` is 400.
    pub weight: f32,
    /// The optical size.
    pub optical_size: OpticalSize,
    /// Whether figures all take the same width, so columns of numbers line
    /// up and a changing value does not shift what follows it.
    pub tabular_figures: bool,
    /// The colour of the glyphs.
    pub color: Color,
    /// Where lines sit across the width.
    pub align: Align,
    /// The most lines the paragraph may take. When it needs more, the last
    /// line that is kept ends with an ellipsis ("…"). `None` is no limit.
    pub max_lines: Option<u32>,
}

impl TextStyle {
    /// Regular (400) black text of `size` logical pixels, optical size
    /// following the size, proportional figures, aligned to the start,
    /// with no limit on lines.
    pub const fn new(size: f32) -> Self {
        TextStyle {
            size,
            weight: 400.0,
            optical_size: OpticalSize::Auto,
            tabular_figures: false,
            color: Color::BLACK,
            align: Align::Start,
            max_lines: None,
        }
    }

    /// The same style with another weight.
    pub const fn with_weight(self, weight: f32) -> Self {
        TextStyle { weight, ..self }
    }

    /// The same style with another optical size.
    pub const fn with_optical_size(self, optical_size: OpticalSize) -> Self {
        TextStyle {
            optical_size,
            ..self
        }
    }

    /// The same style with tabular figures on or off.
    pub const fn with_tabular_figures(self, tabular_figures: bool) -> Self {
        TextStyle {
            tabular_figures,
            ..self
        }
    }

    /// The same style in another colour.
    pub const fn with_color(self, color: Color) -> Self {
        TextStyle { color, ..self }
    }

    /// The same style with another alignment.
    pub const fn with_align(self, align: Align) -> Self {
        TextStyle { align, ..self }
    }

    /// The same style limited to `lines` lines.
    pub const fn with_max_lines(self, lines: u32) -> Self {
        TextStyle {
            max_lines: Some(lines),
            ..self
        }
    }

    /// The size actually used: `None` if [`size`](TextStyle::size) is not a
    /// positive finite number, and at most 65,536.
    pub(crate) fn used_size(&self) -> Option<f32> {
        (self.size.is_finite() && self.size > 0.0).then(|| self.size.min(MAX_SIZE))
    }

    /// The weight actually used, clamped to the axis.
    pub(crate) fn used_weight(&self) -> f32 {
        if self.weight.is_nan() {
            400.0
        } else {
            self.weight.clamp(100.0, 900.0)
        }
    }

    /// The optical size actually used, clamped to the axis.
    pub(crate) fn used_optical_size(&self) -> f32 {
        let wanted = match self.optical_size {
            OpticalSize::Auto => self.used_size().unwrap_or(14.0),
            OpticalSize::Fixed(value) if value.is_nan() => 14.0,
            OpticalSize::Fixed(value) => value,
        };
        wanted.clamp(14.0, 32.0)
    }

    /// The typeface's vertical metrics for this style, in logical pixels.
    /// For a size that lays out nothing, every metric but the units per em
    /// is 0.
    ///
    /// ```
    /// use manguebyte_gfx::TextStyle;
    ///
    /// // Inter has 2048 units per em, so at 2048 pixels a unit is a pixel.
    /// let metrics = TextStyle::new(2048.0).metrics();
    /// assert_eq!(metrics.units_per_em, 2048);
    /// assert_eq!((metrics.ascent, metrics.descent), (1984.0, 494.0));
    /// assert_eq!(metrics.line_height, 2478.0);
    /// ```
    pub fn metrics(&self) -> FontMetrics {
        let face = font::Face::embedded();
        let upem = face.units_per_em();
        let Some(size) = self.used_size() else {
            return FontMetrics {
                units_per_em: upem,
                ..FontMetrics::default()
            };
        };
        let coords = face.coords(self.used_weight(), self.used_optical_size());
        let v = face.vertical(coords);
        let k = f64::from(size) / f64::from(upem);
        let px = |units: f32| (f64::from(units) * k) as f32;
        FontMetrics {
            units_per_em: upem,
            ascent: px(v.ascent),
            descent: px(v.descent),
            line_gap: px(v.line_gap),
            line_height: px(v.ascent + v.descent + v.line_gap),
            cap_height: px(v.cap_height),
            x_height: px(v.x_height),
        }
    }
}

/// The largest text size, in logical pixels.
const MAX_SIZE: f32 = 65_536.0;

/// The typeface's vertical metrics at one size and point of its variation
/// space, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub struct FontMetrics {
    /// Font units per em (2048 for Inter): metrics in the font file are
    /// given in these.
    pub units_per_em: u16,
    /// How far the typeface reaches above the baseline, for line spacing.
    pub ascent: f32,
    /// How far it reaches below the baseline, as a positive distance.
    pub descent: f32,
    /// The extra space the font asks for between lines (0 for Inter).
    pub line_gap: f32,
    /// The distance from one baseline to the next: ascent, descent and
    /// line gap together.
    pub line_height: f32,
    /// The height of flat capital letters above the baseline.
    pub cap_height: f32,
    /// The height of flat lowercase letters such as "x" above the
    /// baseline. It changes with the optical size.
    pub x_height: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_licence_is_the_ofl_with_inters_copyright() {
        assert!(TYPEFACE_LICENCE.starts_with("Copyright (c) 2016 The Inter Project Authors"));
        assert!(TYPEFACE_LICENCE.contains("SIL OPEN FONT LICENSE Version 1.1 - 26 February 2007"));
    }

    /// Whether the font file holds `text` as a name record: Windows
    /// records are UTF-16, big-endian.
    fn font_holds(text: &str) -> bool {
        let needle: Vec<u8> = text.encode_utf16().flat_map(u16::to_be_bytes).collect();
        font::FONT
            .windows(needle.len())
            .any(|w| w == needle.as_slice())
    }

    #[test]
    fn the_font_names_its_licence_but_only_the_constant_holds_its_text() {
        // From the font, read with fontTools 4.63: name ID 13 is a notice
        // naming the licence with a link (ID 14), not the licence itself.
        assert!(font_holds(
            "This Font Software is licensed under the SIL Open Font License, \
             Version 1.1. This license is available with a FAQ at: \
             http://scripts.sil.org/OFL"
        ));
        assert!(font_holds("http://scripts.sil.org/OFL"));
        // The licence's own clauses are not in the font, so a program that
        // embeds it must carry them some other way, as the documentation
        // of `TYPEFACE_LICENCE` says.
        for clause in ["PERMISSION AND CONDITIONS", "DISCLAIMER", "TERMINATION"] {
            assert!(!font_holds(clause), "{clause}");
            assert!(TYPEFACE_LICENCE.contains(clause), "{clause}");
        }
    }

    #[test]
    fn styles_clamp_what_they_use() {
        let style = TextStyle::new(f32::NAN);
        assert_eq!(style.used_size(), None);
        assert_eq!(style.metrics().line_height, 0.0);
        assert_eq!(style.metrics().units_per_em, 2048);
        assert_eq!(TextStyle::new(1e9).used_size(), Some(65_536.0));
        // Automatic optical size follows the size within 14 to 32.
        let opsz = |size: f32| TextStyle::new(size).used_optical_size();
        assert_eq!((opsz(9.0), opsz(20.0), opsz(48.0)), (14.0, 20.0, 32.0));
        let fixed = |v: f32| {
            TextStyle::new(12.0)
                .with_optical_size(OpticalSize::Fixed(v))
                .used_optical_size()
        };
        assert_eq!(
            (fixed(f32::NAN), fixed(-3.0), fixed(99.0)),
            (14.0, 14.0, 32.0)
        );
    }
}
