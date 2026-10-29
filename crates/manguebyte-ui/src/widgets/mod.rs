//! The built-in widgets.
//!
//! | Widget | Role | Shows | Changes |
//! |---|---|---|---|
//! | [`Label`] | [`Label`](crate::Role::Label) | text, fixed or from a source | nothing |
//! | [`Knob`] | [`Slider`](crate::Role::Slider) | a parameter, as an arc and as text | the parameter, by dragging up and down, the wheel, the keys and a typed value |
//! | [`Slider`] | [`Slider`](crate::Role::Slider) | a parameter, as a track and as text | the parameter, by dragging sideways, the wheel, the keys and a typed value |
//! | [`TextField`] | [`TextInput`](crate::Role::TextInput) | one line of text from a [`State`](crate::State) | the text, by typing, the clipboard and input methods |
//! | [`Toggle`] | [`Switch`](crate::Role::Switch) | a parameter that is on or off | the parameter, by clicking, Space and Enter |
//! | [`ComboBox`] | [`ComboBox`](crate::Role::ComboBox) | a parameter with a few named values | the parameter, from a menu of its values, the arrows and the wheel |
//! | [`Meter`] | [`Meter`](crate::Role::Meter) | a level the audio thread publishes, as a bar and as text | nothing |
//! | [`Canvas`] | [`Image`](crate::Role::Image) | whatever a closure draws | nothing |
//! | [`ImageView`] | [`Image`](crate::Role::Image) | an image, fitted to its rectangle, sharp at any scale it has a version for | nothing |
//! | [`ListView`] | [`List`](crate::Role::List), its rows [`ListItem`](crate::Role::ListItem) | rows the application keeps, only those in sight | the selection, by clicks, the keys and typing; rows dragged out and dropped in |
//! | [`ScrollView`] | [`ScrollView`](crate::Role::ScrollView) | one child larger than itself, through a viewport, with scroll bars | the offset, by the wheel, the bars and the keys |
//! | [`EnvelopeEditor`] | [`Group`](crate::Role::Group), its parts [`Slider`](crate::Role::Slider)s | four parameters, and curves if given, as an attack, decay, sustain and release | the parameters, by dragging the curve's corners (one gesture per parameter), the wheel, the keys and typed values |
//! | [`PianoKeyboard`] | [`Group`](crate::Role::Group), its keys [`Button`](crate::Role::Button)s | keys over a [`KeySpan`], those held and those the application lights, and key ranges | notes, polled as [`NoteEvent`]s, from presses, glissandi, the computer's letters and assistive technology |
//! | [`WaveformView`] | [`Group`](crate::Role::Group), its markers [`Slider`](crate::Role::Slider)s | a sample's channels, drawn from a [`Waveform`]'s summary, markers, a selection and a playhead | the markers (within their rules, onto zero crossings if asked), the selection and the zoom, by dragging, the wheel and the keys |
//!
//! Containers ([`row`](crate::row), [`column`](crate::column),
//! [`stack`](crate::stack), [`panel`](crate::panel)) are
//! [`Container`]s with the role [`Group`](crate::Role::Group).
//!
//! Given at least their natural size, knobs, sliders, switches and meters
//! draw inside their own rectangles, with a margin, and canvases clip what
//! they draw to theirs. Given less, a control's fixed parts (a switch's
//! pill, a slider's track, the text above and below a knob or a meter) can
//! reach past its rectangle; the [`Root`](crate::Root) clips any widget's
//! scene that does to the widget's rectangle. Either way the damage of a
//! change to one widget never reaches its neighbours: the damage tests
//! check every control at its natural size and in windows smaller than the
//! panel, at several scales. Knobs, sliders and switches show the keyboard
//! focus with a ring. Knobs and sliders show values in the parameter's own
//! words ([`Param::plain_to_text`](manguebyte_params::Param::plain_to_text)
//! and its unit), and switches give theirs to assistive technology.

mod adjust;
mod canvas;
mod combo;
mod container;
mod envelope;
mod image;
mod keyboard;
mod keys;
mod knob;
mod label;
mod list;
mod meter;
mod scroll_view;
mod slider;
mod text_edit;
mod text_field;
mod toggle;
mod wave_data;
mod waveform;

pub use canvas::{Canvas, CanvasCx, canvas};
pub use combo::{ComboBox, MAX_CHOICES, combo};
pub use container::Container;
pub use envelope::{EnvelopeEditor, envelope};
pub use image::{Fit, ImageView, image};
pub use keyboard::{KeyRange, KeyboardState, NoteEvent, PianoKeyboard, piano_keyboard};
pub use keys::{KeySpan, is_black, note_name, note_name_portuguese};
pub use knob::{Knob, knob};
pub use label::{Label, label};
pub use list::{ListData, ListEvent, ListState, ListView, RowCx, list_view};
pub use meter::{Meter, meter};
pub use scroll_view::{ScrollView, ScrollViewBuilder, scroll_view};
pub use slider::{Slider, slider};
pub use text_field::{TextField, text_field};
pub use toggle::{Toggle, toggle};
pub use wave_data::{MAX_ZERO_SEARCH, Waveform};
pub use waveform::{
    MAX_PIXELS_PER_FRAME, Marker, MarkerKind, MarkerOrder, WaveformEvent, WaveformState,
    WaveformView, ZOOM_STEP, waveform_view,
};

use manguebyte_gfx::{Color, Point, Rect, RoundedRect, Scene, Stroke, TextLayout, TextStyle};

use crate::theme::Theme;

/// Space every control keeps free inside its rectangle, in logical pixels:
/// room for the focus ring, and for the rasteriser's tolerance, so that
/// nothing a control draws reaches past its rectangle.
pub(crate) const MARGIN: f32 = 2.0;

/// Space between the parts of a control (its name, its dial, its value).
pub(crate) const GAP: f32 = 2.0;

/// The distance between baselines of `style`.
pub(crate) fn line_height(style: &TextStyle) -> f32 {
    style.metrics().line_height
}

/// The width to ask for `text` on one line in `style`: its width rounded
/// up, and one more pixel, because layout snaps a widget's edges to the
/// nearest device pixels and may take up to a device pixel from it, which
/// would cut the text with an ellipsis.
pub(crate) fn text_width(text: &str, style: &TextStyle) -> f32 {
    TextLayout::new(text, style, None).size().width.ceil() + 1.0
}

/// A layout of `text` in `style`, one line within `width`.
pub(crate) fn line(text: &str, style: &TextStyle, width: f32) -> TextLayout {
    TextLayout::new(text, &style.with_max_lines(1), Some(width.max(0.0)))
}

/// A layout kept for one width and one key, such as the value it shows,
/// and made again when either changes.
pub(crate) struct Cached<K> {
    entry: Option<(f32, K, TextLayout)>,
}

impl<K: PartialEq> Cached<K> {
    pub(crate) fn new() -> Self {
        Cached { entry: None }
    }

    /// The layout for `width` and `key`, made by `make` if the one kept was
    /// made for something else.
    pub(crate) fn get(
        &mut self,
        width: f32,
        key: K,
        make: impl FnOnce() -> TextLayout,
    ) -> &TextLayout {
        let stale = self
            .entry
            .as_ref()
            .is_none_or(|(w, k, _)| *w != width || *k != key);
        if stale {
            self.entry = None;
        }
        let (_, _, layout) = self.entry.get_or_insert_with(|| (width, key, make()));
        layout
    }
}

/// Draws the focus ring just inside `rect`.
pub(crate) fn focus_ring(scene: &mut Scene, rect: Rect, theme: &Theme) {
    let width = theme.sizes.focus_ring.max(0.0);
    // Half the width, and a pixel for the rasteriser's tolerance, inside
    // the edge: the ring never reaches past the widget's rectangle.
    let inset = width * 0.5 + 1.0;
    let ring = Rect::new(
        rect.x + inset,
        rect.y + inset,
        (rect.width - 2.0 * inset).max(0.0),
        (rect.height - 2.0 * inset).max(0.0),
    );
    scene.stroke(
        RoundedRect::new(ring, theme.corners.medium),
        Stroke::round(width),
        theme.palette.focus,
    );
}

/// `x`, in logical pixels, moved to the nearest edge between device
/// pixels at `scale`, so that what starts there starts sharp.
pub(crate) fn snap_px(x: f32, scale: f32) -> f32 {
    if scale > 0.0 && scale.is_finite() && x.is_finite() {
        (x * scale).round() / scale
    } else {
        x
    }
}

/// `count` whole device pixels at `scale`, in logical pixels: a line that
/// is that many pixels wide at every scale.
pub(crate) fn device_px(count: f32, scale: f32) -> f32 {
    if scale > 0.0 && scale.is_finite() {
        count / scale
    } else {
        count
    }
}

/// A vertical line `pixels` device pixels wide from `top` to `bottom`,
/// starting at the device pixel edge nearest `x`.
pub(crate) fn vline(
    scene: &mut Scene,
    x: f32,
    (top, bottom): (f32, f32),
    pixels: f32,
    scale: f32,
    color: Color,
) {
    let x = snap_px(x, scale);
    scene.fill(
        Rect::new(x, top, device_px(pixels, scale), (bottom - top).max(0.0)),
        color,
    );
}

/// A horizontal line `pixels` device pixels tall from `left` to `right`,
/// starting at the device pixel edge nearest `y`.
pub(crate) fn hline(
    scene: &mut Scene,
    y: f32,
    (left, right): (f32, f32),
    pixels: f32,
    scale: f32,
    color: Color,
) {
    let y = snap_px(y, scale);
    scene.fill(
        Rect::new(left, y, (right - left).max(0.0), device_px(pixels, scale)),
        color,
    );
}

/// Draws `layout` at `at`.
pub(crate) fn draw_text(scene: &mut Scene, layout: &TextLayout, at: Point) {
    scene.text(layout, at);
}

/// The style of a control's name, in the theme's text colour.
pub(crate) fn name_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .label
        .with_color(theme.palette.text)
        .with_align(manguebyte_gfx::Align::Center)
}

/// The style of a control's value, in the theme's muted text colour.
pub(crate) fn value_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .value
        .with_color(theme.palette.text_muted)
        .with_align(manguebyte_gfx::Align::Center)
}

/// The colour of a switch's track or a knob's arc, by state.
pub(crate) fn on_off(theme: &Theme, on: bool) -> Color {
    if on {
        theme.palette.accent
    } else {
        theme.palette.track
    }
}
