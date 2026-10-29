//! Design tokens: the colours, elevations, sizes, radii, spacing, type and
//! motion an interface is drawn with, as plain data.
//!
//! A [`Theme`] is a struct of public fields. There is nothing to subclass
//! and nothing is inherited: the root holds one theme, every widget reads
//! the tokens it needs from it while it draws, and changing a token is
//! changing a field. [`Root::set_theme`](crate::Root::set_theme) switches
//! themes while the interface runs.
//!
//! Two themes are built in. [`Theme::dark`], the default, is deep and dark
//! with a warm glow: layered surfaces a step apart, hairline borders, soft
//! shadows, and one accent, the orange "mangue", that glows where a value
//! is shown. [`Theme::light`] is an editorial light theme with the same
//! structure and no glow.
//!
//! # How the colours were chosen
//!
//! The ramps are made in OKLCH, a space where equal steps of lightness
//! look equal, and stored as sRGB constants, so nothing is computed while
//! the interface runs. Each colour's OKLCH coordinates are given beside it.
//! The dark surfaces step lightness by about 0.03 on one cool hue (271°),
//! with chroma growing a little with lightness; the three text levels are
//! chosen so that each has a contrast of at least 4.5 to 1 (WCAG AA) on
//! every surface it is drawn on, which
//! [`Harness::assert_accessible`](crate::testing::Harness::assert_accessible)
//! checks. The categorical hues of zones and samples keep the colours of
//! the approved mockup and add two at the same lightness and chroma; their
//! text tints are each hue at lightness 0.88 (dark) or 0.42 (light).
//!
//! ```
//! use manguebyte_gfx::Color;
//! use manguebyte_ui::Theme;
//!
//! let mut theme = Theme::dark();
//! theme.palette.accent = Color::hex(0x3D8BFF);
//! theme.sizes.knob = 56.0;
//! assert_eq!(theme.corners.xl, 18.0);
//! assert_eq!(theme.spacing.md, 12.0);
//! assert_eq!(theme.typography.value.size, 11.0);
//! ```

use manguebyte_gfx::{Color, Shadow, TextStyle};

use crate::motion::Motion;

/// One categorical hue: how a zone, a sample or a series is coloured.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hue {
    /// The hue itself: a swatch, a waveform, a line.
    pub color: Color,
    /// Text drawn on [`fill`](Hue::fill), with a contrast of at least 4.5
    /// to 1 on it over the theme's surfaces.
    pub text: Color,
    /// A large area of the hue: a zone's body, a tint behind a row.
    pub fill: Color,
    /// The hairline around such an area.
    pub ring: Color,
}

/// The colours of an interface, in layers: four surfaces, from the
/// window up, hairlines, three levels of text, one accent, categorical
/// hues and semantic roles.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Level 0: behind everything.
    pub background: Color,
    /// Level 1: cards and panels, and the bottom of their gradient.
    pub surface: Color,
    /// Level 2: the faces of controls, pills, rows, the top of a card's
    /// gradient.
    pub raised: Color,
    /// Level 3: what floats or is pointed at: menus, tooltips, hovered
    /// pills.
    pub overlay: Color,
    /// Borders a pixel wide around cards and controls.
    pub hairline: Color,
    /// A stronger hairline: dividers, the edge of a field.
    pub hairline_strong: Color,
    /// The light caught along the top edge of a raised surface (a line a
    /// pixel high, CSS's `inset 0 1px`).
    pub highlight: Color,
    /// Text: names and values.
    pub text: Color,
    /// Text that matters less: the names of controls, headings of cards.
    pub text_secondary: Color,
    /// Text that matters least: counts, units, ranges beside a row.
    pub text_muted: Color,
    /// The one accent: values, the selection, what is on.
    pub accent: Color,
    /// The accent's bright end, for the top of gradients and glows.
    pub accent_bright: Color,
    /// The accent as a faint tint: a selected row's background.
    pub accent_soft: Color,
    /// Text and icons drawn on the accent.
    pub on_accent: Color,
    /// The ring around the widget with the keyboard focus, when it shows.
    pub focus: Color,
    /// The tracks along which values move: knob rings, slider tracks,
    /// meter wells, switches that are off.
    pub track: Color,
    /// Pointers and thumbs.
    pub thumb: Color,
    /// The light and dark ends of a control's face (a knob's dial).
    pub face_light: Color,
    /// See [`face_light`](Palette::face_light).
    pub face_dark: Color,
    /// Categorical hues for zones, samples and series, in order: violet,
    /// teal, orange, pink, blue, green.
    pub hues: [Hue; 6],
    /// Good: a level in its range, a valid value.
    pub success: Color,
    /// Close to a limit.
    pub warning: Color,
    /// Over a limit, or wrong.
    pub danger: Color,
    /// The white keys of a piano keyboard, at their foot.
    pub key_white: Color,
    /// The white keys at their top, where they go under the black ones.
    pub key_white_shade: Color,
    /// The black keys at their foot, and text drawn on white keys.
    pub key_black: Color,
    /// The black keys at their top.
    pub key_black_top: Color,
}

/// Elevations: the shadows that lift surfaces off what is under them, and
/// the accent's glow.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Elevation {
    /// Small things just above their surface: keys, thumbs, pills.
    pub sm: Shadow,
    /// Cards and panels.
    pub md: Shadow,
    /// What floats over everything: menus, tooltips.
    pub lg: Shadow,
    /// The glow around lit things: a knob's value arc, a lit key, a curve.
    /// Fully transparent in a theme without glow.
    pub glow: Shadow,
}

/// Lengths, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizes {
    /// The diameter of a knob's ring.
    pub knob: f32,
    /// The width of a knob's ring and of its value arc.
    pub knob_ring: f32,
    /// The natural width of a slider.
    pub slider_width: f32,
    /// The thickness of a slider's track.
    pub slider_track: f32,
    /// The diameter of a slider's thumb.
    pub slider_thumb: f32,
    /// The width of a switch.
    pub toggle_width: f32,
    /// The height of a switch.
    pub toggle_height: f32,
    /// The width of a meter's bar.
    pub meter_width: f32,
    /// The natural height of a meter's bar.
    pub meter_height: f32,
    /// The width of the focus ring.
    pub focus_ring: f32,
    /// How far, at most, the focus ring lies outside a control's face.
    pub focus_gap: f32,
    /// How far the pointer moves, up or right, to take a knob or slider
    /// across its whole range.
    pub drag_range: f32,
    /// How many times further it moves for the same change while Shift is
    /// held.
    pub fine_factor: f32,
    /// The room a scroll view keeps for a scroll bar; the bar itself is
    /// drawn thinner, and wider while the pointer is on it.
    pub scroll_bar: f32,
    /// How far one notch of the wheel, or an arrow key, scrolls a view.
    pub scroll_line: f32,
    /// The height of a row of a list.
    pub list_row: f32,
}

/// Corner radii, in logical pixels: a scale from small parts to cards,
/// and a radius that makes a pill of anything.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corners {
    /// Small parts: tracks, meter wells, swatches, markers' labels.
    pub sm: f32,
    /// Controls: fields, keys, list rows, zones, focus rings.
    pub md: f32,
    /// Larger controls and inner panels: menus, waveform and zone areas.
    pub lg: f32,
    /// Cards and panels.
    pub xl: f32,
    /// Pills and round ends: as large as anything is, so that the radius
    /// is cut to half the shorter side.
    pub full: f32,
}

/// The spacing scale, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Spacing {
    /// 4: inside small parts.
    pub xs: f32,
    /// 8: between the parts of a control, between rows.
    pub sm: f32,
    /// 12: inside cards, between neighbouring controls.
    pub md: f32,
    /// 16: between cards.
    pub lg: f32,
    /// 24: around a whole screen.
    pub xl: f32,
}

/// Text styles, all in the embedded Inter at weights 400, 500 and 600.
/// Their colours are replaced by the palette's when widgets draw them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Typography {
    /// Labels, the names of controls and rows: 12, weight 500.
    pub label: TextStyle,
    /// Values shown under controls and meter readouts: 11, weight 500,
    /// tabular figures so that changing digits do not dance.
    pub value: TextStyle,
    /// Headings of screens: 16, weight 600.
    pub title: TextStyle,
    /// Headings of cards: 12, weight 500.
    pub heading: TextStyle,
    /// Small print beside things: counts, ranges, units: 11, weight 400,
    /// tabular figures.
    pub caption: TextStyle,
    /// The text of chips and markers' labels: 10, weight 600.
    pub chip: TextStyle,
}

/// Everything an interface is drawn with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Colours.
    pub palette: Palette,
    /// Shadows and glow.
    pub elevation: Elevation,
    /// Lengths of controls.
    pub sizes: Sizes,
    /// Corner radii.
    pub corners: Corners,
    /// The spacing scale.
    pub spacing: Spacing,
    /// Text styles.
    pub typography: Typography,
    /// How long changes take and how they ease, and whether they animate.
    pub motion: Motion,
}

/// A hue in the dark theme: the colour, a light tint for text, the colour
/// at 18 % for an area and at 50 % for its ring.
const fn dark_hue(color: u32, text: u32) -> Hue {
    let c = Color::hex(color);
    Hue {
        color: c,
        text: Color::hex(text),
        fill: c.with_alpha(0x2E),
        ring: c.with_alpha(0x80),
    }
}

/// A hue in the light theme: the colour, dark text, a pale tint for an
/// area and a light ring.
const fn light_hue(color: u32, text: u32, fill: u32, ring: u32) -> Hue {
    Hue {
        color: Color::hex(color),
        text: Color::hex(text),
        fill: Color::hex(fill),
        ring: Color::hex(ring),
    }
}

impl Theme {
    /// Deep dark, with a warm glow: the default.
    pub const fn dark() -> Theme {
        let accent = Color::hex(0xFF7A3D);
        Theme {
            palette: Palette {
                // oklch(0.180 0.010 271), (0.210 0.014 271), (0.240 0.018 271),
                // (0.278 0.023 271)
                background: Color::hex(0x101116),
                surface: Color::hex(0x16181F),
                raised: Color::hex(0x1C1F28),
                overlay: Color::hex(0x242834),
                hairline: Color::rgba(0xFF, 0xFF, 0xFF, 0x0F),
                hairline_strong: Color::rgba(0xFF, 0xFF, 0xFF, 0x1A),
                highlight: Color::rgba(0xFF, 0xFF, 0xFF, 0x0A),
                // oklch(0.955 0.008 271), (0.700 0.025 270), (0.620 0.027 269)
                text: Color::hex(0xEEF0F6),
                text_secondary: Color::hex(0x989EAF),
                text_muted: Color::hex(0x7F8697),
                // oklch(0.726 0.178 43), (0.819 0.136 66)
                accent,
                accent_bright: Color::hex(0xFFB15C),
                accent_soft: accent.with_alpha(0x1F),
                on_accent: Color::hex(0x1A0D06),
                focus: accent.with_alpha(0xA6),
                track: Color::rgba(0xFF, 0xFF, 0xFF, 0x14),
                thumb: Color::hex(0xFFFFFF),
                // oklch(0.360 0.022 270), (0.220 0.016 270)
                face_light: Color::hex(0x383D49),
                face_dark: Color::hex(0x181A22),
                hues: [
                    dark_hue(0x8B7BFF, 0xD2D2FF),
                    dark_hue(0x2FD3C0, 0xA3E7DC),
                    dark_hue(0xFF7A3D, 0xFFC9B3),
                    dark_hue(0xFF5C9A, 0xFFC5D5),
                    // oklch(0.740 0.160 248), (0.740 0.160 130)
                    dark_hue(0x4AB1FF, 0xB4DCFF),
                    dark_hue(0x89BE47, 0xC8E1B1),
                ],
                // oklch(0.780 0.150 160), (0.840 0.150 80), (0.680 0.200 22)
                success: Color::hex(0x4AD496),
                warning: Color::hex(0xFDBE45),
                danger: Color::hex(0xFB575D),
                key_white: Color::hex(0xFFFFFF),
                key_white_shade: Color::hex(0xE9EBF1),
                key_black: Color::hex(0x14161C),
                key_black_top: Color::hex(0x2A2D38),
            },
            elevation: Elevation {
                sm: Shadow::new(Color::rgba(0, 0, 0, 0x66))
                    .with_offset(0.0, 2.0)
                    .with_blur(4.0),
                md: Shadow::new(Color::rgba(0, 0, 0, 0xB3))
                    .with_offset(0.0, 18.0)
                    .with_blur(40.0)
                    .with_spread(-24.0),
                lg: Shadow::new(Color::rgba(0, 0, 0, 0x99))
                    .with_offset(0.0, 16.0)
                    .with_blur(40.0)
                    .with_spread(-8.0),
                glow: Shadow::glow(8.0, accent.with_alpha(0xA6)),
            },
            sizes: Sizes {
                knob: 46.0,
                knob_ring: 3.0,
                slider_width: 120.0,
                slider_track: 4.0,
                slider_thumb: 12.0,
                toggle_width: 34.0,
                toggle_height: 20.0,
                meter_width: 8.0,
                meter_height: 80.0,
                focus_ring: 2.0,
                focus_gap: 2.0,
                drag_range: 200.0,
                fine_factor: 10.0,
                scroll_bar: 10.0,
                scroll_line: 40.0,
                list_row: 32.0,
            },
            corners: Corners {
                sm: 4.0,
                md: 10.0,
                lg: 12.0,
                xl: 18.0,
                full: 999.0,
            },
            spacing: Spacing {
                xs: 4.0,
                sm: 8.0,
                md: 12.0,
                lg: 16.0,
                xl: 24.0,
            },
            typography: Typography {
                label: TextStyle::new(12.0).with_weight(500.0).with_max_lines(1),
                value: TextStyle::new(11.0)
                    .with_weight(500.0)
                    .with_tabular_figures(true)
                    .with_max_lines(1),
                title: TextStyle::new(16.0).with_weight(600.0).with_max_lines(1),
                heading: TextStyle::new(12.0).with_weight(500.0).with_max_lines(1),
                caption: TextStyle::new(11.0)
                    .with_tabular_figures(true)
                    .with_max_lines(1),
                chip: TextStyle::new(10.0).with_weight(600.0).with_max_lines(1),
            },
            motion: Motion::STANDARD,
        }
    }

    /// Light and editorial, with no glow: the same structure on warm paper.
    pub const fn light() -> Theme {
        let dark = Theme::dark();
        let accent = Color::hex(0xFF5A1F);
        let ink = Color::hex(0x16150F);
        Theme {
            palette: Palette {
                // oklch(0.964 0.005 95), (0.985 0.004 95), white,
                // (0.945 0.006 95)
                background: Color::hex(0xF4F3EF),
                surface: Color::hex(0xFBFAF7),
                raised: Color::hex(0xFFFFFF),
                overlay: Color::hex(0xEEEDE8),
                hairline: Color::rgba(0x14, 0x12, 0x0C, 0x14),
                hairline_strong: Color::rgba(0x14, 0x12, 0x0C, 0x24),
                highlight: Color::rgba(0xFF, 0xFF, 0xFF, 0xB3),
                // oklch(0.195 0.012 100), (0.440 0.014 97), (0.497 0.015 93)
                text: ink,
                text_secondary: Color::hex(0x55534A),
                text_muted: Color::hex(0x656259),
                // oklch(0.682 0.211 38)
                accent,
                accent_bright: Color::hex(0xFF7A45),
                accent_soft: accent.with_alpha(0x14),
                on_accent: Color::hex(0xFFFFFF),
                focus: accent.with_alpha(0x99),
                track: Color::rgba(0x14, 0x12, 0x0C, 0x14),
                thumb: ink,
                face_light: Color::hex(0xFFFFFF),
                face_dark: Color::hex(0xF1F0EB),
                hues: [
                    light_hue(0x6B5CFF, 0x423E96, 0xEDEFFF, 0xC0C5FE),
                    light_hue(0x0E9F8E, 0x006352, 0xDFF6F1, 0x8EDCCE),
                    light_hue(0xFF5A1F, 0x882600, 0xFFEBE4, 0xF9B9A5),
                    light_hue(0xE3367A, 0x861F46, 0xFFEAEF, 0xF8B5C6),
                    // oklch(0.600 0.170 250), (0.600 0.170 135)
                    light_hue(0x0083E0, 0x004E95, 0xE4F2FF, 0xA3CFFD),
                    light_hue(0x4F950F, 0x265C00, 0xE9F4E4, 0xB4D7A2),
                ],
                // oklch(0.580 0.130 160), (0.700 0.150 70), (0.580 0.210 27)
                success: Color::hex(0x06915F),
                warning: Color::hex(0xD98B09),
                danger: Color::hex(0xDB2C2B),
                key_white: Color::hex(0xFFFFFF),
                key_white_shade: Color::hex(0xFFFFFF),
                key_black: ink,
                key_black_top: Color::hex(0x2B2A25),
            },
            elevation: Elevation {
                sm: Shadow::new(Color::rgba(0x14, 0x12, 0x0C, 0x1A))
                    .with_offset(0.0, 1.0)
                    .with_blur(2.0),
                md: Shadow::new(Color::rgba(0x14, 0x12, 0x0C, 0x40))
                    .with_offset(0.0, 12.0)
                    .with_blur(32.0)
                    .with_spread(-20.0),
                lg: Shadow::new(Color::rgba(0x14, 0x12, 0x0C, 0x33))
                    .with_offset(0.0, 12.0)
                    .with_blur(32.0)
                    .with_spread(-8.0),
                glow: Shadow::glow(0.0, Color::TRANSPARENT),
            },
            ..dark
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Theme::dark()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OKLCH lightness of an sRGB colour (Björn Ottosson's matrices).
    fn lightness(c: Color) -> f64 {
        let linear = |v: u8| {
            let v = f64::from(v) / 255.0;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        let (r, g, b) = (linear(c.r), linear(c.g), linear(c.b));
        let l = (0.412_221_470_8 * r + 0.536_332_536_3 * g + 0.051_445_992_9 * b).cbrt();
        let m = (0.211_903_498_2 * r + 0.680_699_545_1 * g + 0.107_396_956_6 * b).cbrt();
        let s = (0.088_302_461_9 * r + 0.281_718_837_6 * g + 0.629_978_700_5 * b).cbrt();
        0.210_454_255_3 * l + 0.793_617_785 * m - 0.004_072_046_8 * s
    }

    #[test]
    fn the_dark_surfaces_step_up_evenly_in_perceived_lightness() {
        let p = Theme::dark().palette;
        let levels = [p.background, p.surface, p.raised, p.overlay].map(lightness);
        for pair in levels.windows(2) {
            let step = pair[1] - pair[0];
            assert!((0.025..0.045).contains(&step), "{levels:?}");
        }
        // Text levels step down from the text to the muted text.
        let text = [p.text, p.text_secondary, p.text_muted].map(lightness);
        assert!(text[0] > text[1] && text[1] > text[2], "{text:?}");
    }

    #[test]
    fn the_light_theme_has_no_glow_and_the_dark_one_does() {
        assert_eq!(Theme::light().elevation.glow.color.a, 0);
        assert!(Theme::dark().elevation.glow.color.a > 0);
        // Everything but colours and elevations is shared.
        let (dark, light) = (Theme::dark(), Theme::light());
        assert_eq!(dark.sizes, light.sizes);
        assert_eq!(dark.corners, light.corners);
        assert_eq!(dark.spacing, light.spacing);
        assert_eq!(dark.typography, light.typography);
    }

    #[test]
    fn the_categorical_hues_are_told_apart() {
        for theme in [Theme::dark(), Theme::light()] {
            let hues = theme.palette.hues;
            for (i, a) in hues.iter().enumerate() {
                for b in &hues[i + 1..] {
                    assert_ne!(a.color, b.color);
                }
            }
        }
    }
}
