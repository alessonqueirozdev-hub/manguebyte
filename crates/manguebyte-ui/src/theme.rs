//! Design tokens: the colours, sizes, radii and type an interface is drawn
//! with, as plain data.
//!
//! A [`Theme`] is a struct of public fields. There is nothing to subclass
//! and nothing is inherited: the root holds one theme, every widget reads
//! the tokens it needs from it while it draws, and changing a token is
//! changing a field.
//!
//! ```
//! use manguebyte_gfx::Color;
//! use manguebyte_ui::Theme;
//!
//! let mut theme = Theme::dark();
//! theme.palette.accent = Color::hex(0x3D8BFF);
//! theme.sizes.knob = 56.0;
//! assert_eq!(theme.typography.value.size, 11.0);
//! ```

use manguebyte_gfx::{Color, TextStyle};

use crate::motion::Motion;

/// The colours of an interface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Behind everything.
    pub background: Color,
    /// Panels and the faces of controls.
    pub surface: Color,
    /// The tracks along which values move: knob rings, slider tracks,
    /// meter wells, switches that are off.
    pub track: Color,
    /// The part of a track that shows a value, and switches that are on.
    pub accent: Color,
    /// Pointers and thumbs.
    pub thumb: Color,
    /// Text.
    pub text: Color,
    /// Text that matters less: values, units.
    pub text_muted: Color,
    /// The ring around the widget that has the keyboard focus.
    pub focus: Color,
    /// A meter's level.
    pub meter: Color,
    /// A meter's level above its warning mark.
    pub meter_warning: Color,
    /// The white keys of a piano keyboard.
    pub key_white: Color,
    /// The black keys of a piano keyboard, and text drawn on white keys.
    pub key_black: Color,
}

/// Lengths, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sizes {
    /// Space between neighbouring widgets in the default views.
    pub spacing: f32,
    /// Space inside panels.
    pub padding: f32,
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
    /// How far the pointer moves, up or right, to take a knob or slider
    /// across its whole range.
    pub drag_range: f32,
    /// How many times further it moves for the same change while Shift is
    /// held.
    pub fine_factor: f32,
    /// The thickness of a scroll bar, and the room a scroll view keeps for
    /// one.
    pub scroll_bar: f32,
    /// How far one notch of the wheel, or an arrow key, scrolls a view.
    pub scroll_line: f32,
    /// The height of a row of a list.
    pub list_row: f32,
}

/// Corner radii, in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Corners {
    /// Small parts: tracks, meter wells.
    pub small: f32,
    /// Controls and focus rings.
    pub medium: f32,
    /// Panels.
    pub large: f32,
}

/// Text styles. Their colours are replaced by the palette's when widgets
/// draw them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Typography {
    /// Labels and the names of controls.
    pub label: TextStyle,
    /// Values shown under controls and meter readouts.
    pub value: TextStyle,
    /// Headings.
    pub title: TextStyle,
}

/// Everything an interface is drawn with.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    /// Colours.
    pub palette: Palette,
    /// Lengths.
    pub sizes: Sizes,
    /// Corner radii.
    pub corners: Corners,
    /// Text styles.
    pub typography: Typography,
    /// How long changes take and how they ease, and whether they animate.
    pub motion: Motion,
}

impl Theme {
    /// Light text on dark grey, with an orange accent.
    pub const fn dark() -> Theme {
        Theme {
            palette: Palette {
                background: Color::hex(0x16181C),
                surface: Color::hex(0x23262D),
                track: Color::hex(0x3A3F4A),
                accent: Color::hex(0xFF7A00),
                thumb: Color::hex(0xE8E8E8),
                text: Color::hex(0xE8E8E8),
                text_muted: Color::hex(0x9AA0AA),
                focus: Color::hex(0x5CA8FF),
                meter: Color::hex(0x41D27A),
                meter_warning: Color::hex(0xFF4D4D),
                key_white: Color::hex(0xDADCE0),
                key_black: Color::hex(0x101216),
            },
            sizes: Sizes {
                spacing: 8.0,
                padding: 12.0,
                knob: 44.0,
                knob_ring: 4.0,
                slider_width: 120.0,
                slider_track: 4.0,
                slider_thumb: 12.0,
                toggle_width: 32.0,
                toggle_height: 18.0,
                meter_width: 10.0,
                meter_height: 80.0,
                focus_ring: 1.5,
                drag_range: 200.0,
                fine_factor: 10.0,
                scroll_bar: 10.0,
                scroll_line: 40.0,
                list_row: 24.0,
            },
            corners: Corners {
                small: 2.0,
                medium: 4.0,
                large: 8.0,
            },
            typography: Typography {
                label: TextStyle::new(12.0).with_weight(500.0).with_max_lines(1),
                value: TextStyle::new(11.0)
                    .with_tabular_figures(true)
                    .with_max_lines(1),
                title: TextStyle::new(16.0).with_weight(600.0).with_max_lines(1),
            },
            motion: Motion::STANDARD,
        }
    }

    /// Dark text on light grey, with the same accent.
    pub const fn light() -> Theme {
        let dark = Theme::dark();
        Theme {
            palette: Palette {
                background: Color::hex(0xF2F3F5),
                surface: Color::hex(0xFFFFFF),
                track: Color::hex(0xD0D4DB),
                accent: Color::hex(0xE86A00),
                thumb: Color::hex(0x23262D),
                text: Color::hex(0x1C1F24),
                text_muted: Color::hex(0x5F6670),
                focus: Color::hex(0x1F6FEB),
                meter: Color::hex(0x1E9E52),
                meter_warning: Color::hex(0xD62828),
                key_white: Color::hex(0xFFFFFF),
                key_black: Color::hex(0x2A2D34),
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
