//! The built-in widgets.
//!
//! | Widget | Role | Shows | Changes |
//! |---|---|---|---|
//! | [`Label`] | [`Label`](crate::Role::Label) | text, fixed or from a source | nothing |
//! | [`Knob`] | [`Slider`](crate::Role::Slider) | a parameter, as an arc and as text | the parameter, by dragging up and down, the wheel, the keys and a typed value |
//! | [`Slider`] | [`Slider`](crate::Role::Slider) | a parameter, as a track and as text | the parameter, by dragging sideways, the wheel, the keys and a typed value |
//! | [`TextField`] | [`TextInput`](crate::Role::TextInput) | one line of text from a [`State`](crate::State) | the text, by typing, the clipboard and input methods |
//! | [`Toggle`] | [`Switch`](crate::Role::Switch) | a parameter that is on or off | the parameter, by clicking, Space and Enter |
//! | [`Pill`] | [`Button`](crate::Role::Button) | a label, a status dot | a count of presses the application polls, by clicking, Space and Enter |
//! | [`ActionButton`] | [`Button`](crate::Role::Button) | a label from a [`TextSource`], an optional icon, a primary, secondary or destructive look, disabled or not | a count of presses the application polls, by clicking, Space and Enter |
//! | [`ComboBox`] | [`ComboBox`](crate::Role::ComboBox) | a parameter with a few named values | the parameter, from a menu of its values, the arrows and the wheel |
//! | [`Meter`] | [`Meter`](crate::Role::Meter) | a level the audio thread publishes, as a bar and as text | nothing |
//! | [`Canvas`] | [`Image`](crate::Role::Image) | whatever a closure draws | nothing |
//! | [`ImageView`] | [`Image`](crate::Role::Image) | an image, fitted to its rectangle, sharp at any scale it has a version for | nothing |
//! | [`ListView`] | [`List`](crate::Role::List), its rows [`ListItem`](crate::Role::ListItem) | rows the application keeps, only those in sight | the selection, by clicks, the keys and typing; rows dragged out and dropped in |
//! | [`ScrollView`] | [`ScrollView`](crate::Role::ScrollView) | one child larger than itself, through a viewport, with scroll bars | the offset, by the wheel, the bars and the keys |
//! | [`EnvelopeEditor`] | [`Group`](crate::Role::Group), its parts [`Slider`](crate::Role::Slider)s | four parameters, and curves if given, as an attack, decay, sustain and release | the parameters, by dragging the curve's corners (one gesture per parameter), the wheel, the keys and typed values |
//! | [`PianoKeyboard`] | [`Group`](crate::Role::Group), its keys [`Button`](crate::Role::Button)s | keys over a [`KeySpan`], those held and those the application lights, and key ranges | notes, polled as [`NoteEvent`]s, from presses, glissandi, the computer's letters and assistive technology |
//! | [`ZoneMap`] | [`List`](crate::Role::List), its zones [`ListItem`](crate::Role::ListItem)s | zones over keys and velocities, lined up with a keyboard over the same span, and where they overlap | the zones and the selection, by dragging zones and their edges, a band, the keys, and drops that make zones |
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

mod action;
mod adjust;
mod canvas;
mod combo;
mod container;
mod dialog;
mod envelope;
mod icon;
mod image;
mod keyboard;
mod keys;
mod knob;
mod label;
mod list;
mod meter;
mod nav;
mod pill;
mod scroll_view;
mod slider;
mod text_edit;
mod text_field;
mod toggle;
mod wave_data;
mod waveform;
mod zone_map;

pub use action::{ActionButton, action_button};
pub use canvas::{Canvas, CanvasCx, canvas};
pub use combo::{ComboBox, MAX_CHOICES, combo};
pub use container::Container;
pub use dialog::{ConfirmDialog, DialogResult, DialogState, confirm_dialog};
pub use envelope::{EnvelopeEditor, envelope};
pub use icon::Icon;
pub use image::{Fit, ImageView, image};
pub use keyboard::{
    KeyRange, KeyboardState, NoteEvent, PianoKeyboard, piano_keyboard, piano_keyboard_named,
};
pub use keys::{KeySpan, is_black, note_name, note_name_portuguese};
pub use knob::{Knob, knob};
pub use label::{Label, label};
pub use list::{ListData, ListEvent, ListState, ListView, RowCx, list_view, list_view_named};
pub use meter::{Meter, meter};
pub use nav::{Pages, sidebar, tabs};
pub use pill::{Pill, pill};
pub use scroll_view::{ScrollView, ScrollViewBuilder, scroll_view};
pub use slider::{Slider, slider};
pub use text_field::{TextField, text_field};
pub use toggle::{Toggle, toggle};
pub use wave_data::{MAX_ZERO_SEARCH, Waveform};
pub use waveform::{
    MAX_PIXELS_PER_FRAME, Marker, MarkerKind, MarkerOrder, WaveformEvent, WaveformState,
    WaveformView, ZOOM_STEP, waveform_view, waveform_view_named,
};
pub use zone_map::{Zone, ZoneEvent, ZoneMap, ZoneMapState, zone_map, zone_map_named};

use manguebyte_gfx::{
    Color, Extend, LinearGradient, Point, Rect, RoundedRect, Scene, Shadow, Shape, Stop, Stroke,
    TextLayout, TextStyle,
};

use crate::source::TextSource;
use crate::theme::Theme;
use crate::widget::UpdateCx;

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

    /// Forgets the layout kept, because what it shows changed.
    pub(crate) fn clear(&mut self) {
        self.entry = None;
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

/// Reads a control's own name again, if it was given one. When it changed
/// the layout kept of it is forgotten, the interface is laid out again
/// (the control's width depends on its name) and the control drawn
/// again, and assistive technology is told.
pub(crate) fn poll_label(
    label: &mut Option<TextSource>,
    name: &mut Cached<TextStyle>,
    cx: &mut UpdateCx<'_>,
) {
    if label.as_mut().is_some_and(TextSource::poll) {
        name.clear();
        cx.request_layout();
        cx.request_access_update();
    }
}

/// Reads a widget's name for assistive technology again; when it changed,
/// assistive technology is told. Nothing is drawn: the widget does not
/// show its name.
pub(crate) fn poll_name(name: &mut TextSource, cx: &mut UpdateCx<'_>) {
    if name.poll() {
        cx.request_access_update();
    }
}

/// Draws the focus ring along `ring`, faded in by `amount` (the root's
/// [`focus_ring`](crate::PaintCx::focus_ring) amount): a soft line in the
/// theme's focus colour, with a faint glow of it in a theme that glows.
/// Nothing at 0. The caller keeps `ring` far enough inside its rectangle
/// for the line and its glow.
pub(crate) fn focus_ring<'s>(
    scene: &mut Scene,
    ring: impl Into<Shape<'s>>,
    theme: &Theme,
    amount: f32,
) {
    if amount.is_nan() || amount <= 0.0 {
        return;
    }
    let ring = ring.into();
    let width = theme.sizes.focus_ring.max(0.0);
    let color = theme.palette.focus.with_opacity(amount);
    glow_stroke(scene, ring, Stroke::round(width), theme, 0.5 * amount);
    scene.stroke(ring, Stroke::round(width), color);
}

/// The theme's glow, at `strength` (0 to 1) of its opacity, around the
/// outline of `shape` drawn with `stroke`. Nothing in a theme without
/// glow, or at no strength.
pub(crate) fn glow_stroke<'s>(
    scene: &mut Scene,
    shape: impl Into<Shape<'s>>,
    stroke: Stroke,
    theme: &Theme,
    strength: f32,
) {
    let glow = theme.elevation.glow;
    let color = glow.color.with_opacity(strength);
    if color.a == 0 {
        return;
    }
    scene.stroke_shadow(shape, stroke, glow.with_color(color));
}

/// The theme's glow in `color` (its opacity times `strength`) around the
/// filled `shape`. Nothing in a theme without glow.
pub(crate) fn glow_fill<'s>(
    scene: &mut Scene,
    shape: impl Into<Shape<'s>>,
    color: Color,
    theme: &Theme,
    strength: f32,
) {
    let glow = theme.elevation.glow;
    if glow.color.a == 0 {
        return;
    }
    let alpha = f32::from(glow.color.a) / 255.0 * strength;
    let color = color.with_opacity(alpha * f32::from(color.a) / 255.0);
    if color.a == 0 {
        return;
    }
    scene.shadow(shape, glow.with_color(color));
}

/// Draws a floating surface over `rect`: menus, tooltips, a drag's ghost.
/// The theme's large shadow, its overlay surface with corners of
/// `radius`, and a strong hairline.
pub(crate) fn floating(scene: &mut Scene, rect: Rect, radius: f32, theme: &Theme, scale: f32) {
    let radius = radius.min(rect.width * 0.5).min(rect.height * 0.5).max(0.0);
    let shape = RoundedRect::new(rect, radius);
    drop_shadow(scene, shape, theme.elevation.lg);
    scene.fill(shape, theme.palette.overlay);
    hairline(scene, rect, radius, scale, theme.palette.hairline_strong);
}

/// How far a floating surface's shadow reaches beyond it (see
/// [`floating`]).
pub(crate) fn floating_overflow(theme: &Theme) -> f32 {
    let shadow = theme.elevation.lg;
    if shadow.color.a == 0 {
        return 0.0;
    }
    let offset = shadow.offset.x.abs().max(shadow.offset.y.abs());
    (shadow.reach_exact() + offset).max(0.0).ceil()
}

/// The shadow under a small thumb (a switch's, a slider's): the theme's
/// small elevation at half its offset and blur, so that it stays inside
/// the control.
pub(crate) fn thumb_shadow(theme: &Theme) -> Shadow {
    let sm = theme.elevation.sm;
    sm.with_offset(sm.offset.x * 0.5, sm.offset.y * 0.5)
        .with_blur(sm.blur * 0.5)
}

/// Draws `shadow` behind `shape` unless it is fully transparent.
pub(crate) fn drop_shadow<'s>(scene: &mut Scene, shape: impl Into<Shape<'s>>, shadow: Shadow) {
    if shadow.color.a > 0 {
        scene.shadow(shape, shadow);
    }
}

/// Fills `rect` with a gradient from `top` to `bottom`.
pub(crate) fn vertical<'s>(
    scene: &mut Scene,
    shape: impl Into<Shape<'s>>,
    (y0, y1): (f32, f32),
    top: Color,
    bottom: Color,
) {
    if top == bottom {
        scene.fill(shape, top);
        return;
    }
    let stops = [Stop::new(0.0, top), Stop::new(1.0, bottom)];
    scene.fill(
        shape,
        LinearGradient {
            start: Point::new(0.0, y0),
            end: Point::new(0.0, y1),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
}

/// A hairline a device pixel wide just inside `rect`, with corners of
/// `radius`: the border of a card or a control.
pub(crate) fn hairline(scene: &mut Scene, rect: Rect, radius: f32, scale: f32, color: Color) {
    let px = device_px(1.0, scale);
    let edge = Rect::new(
        rect.x + px * 0.5,
        rect.y + px * 0.5,
        (rect.width - px).max(0.0),
        (rect.height - px).max(0.0),
    );
    scene.stroke(
        RoundedRect::new(edge, (radius - px * 0.5).max(0.0)),
        Stroke::new(px),
        color,
    );
}

/// The light caught along the top of a raised surface: a line a device
/// pixel high inside the top edge of `rect`, cut by its corners of
/// `radius` (CSS's `inset 0 1px 0`).
pub(crate) fn top_highlight(scene: &mut Scene, rect: Rect, radius: f32, scale: f32, color: Color) {
    if color.a == 0 {
        return;
    }
    // A thin band along the top edge, as round as the corners: the
    // rectangle's top less the same rectangle a pixel lower, drawn as one
    // outline filled even-odd (no clip, which costs a mask).
    let px = device_px(1.0, scale);
    let r = radius.min(rect.width * 0.5).min(rect.height * 0.5).max(0.0);
    let band = Rect::new(rect.x, rect.y, rect.width, (r + px).min(rect.height));
    let mut path = manguebyte_gfx::Path::new();
    top_rounded(&mut path, band, r);
    top_rounded(
        &mut path,
        Rect::new(band.x, band.y + px, band.width, (band.height - px).max(0.0)),
        r,
    );
    path.set_fill_rule(manguebyte_gfx::FillRule::EvenOdd);
    scene.fill(&path, color);
}

/// Adds to `path` the outline of `rect` with its top corners rounded by
/// `r` (each corner a cubic close to a quarter circle) and its bottom
/// corners square.
fn top_rounded(path: &mut manguebyte_gfx::Path, rect: Rect, r: f32) {
    // How far a cubic's controls lie along the tangents for a quarter
    // circle.
    const K: f32 = 0.552_284_8;
    let (x0, y0, x1, y1) = (rect.x, rect.y, rect.right(), rect.bottom());
    let r = r.min(rect.height).min(rect.width * 0.5).max(0.0);
    path.move_to(Point::new(x0, y1))
        .line_to(Point::new(x0, y0 + r))
        .cubic_to(
            Point::new(x0, y0 + r - K * r),
            Point::new(x0 + r - K * r, y0),
            Point::new(x0 + r, y0),
        )
        .line_to(Point::new(x1 - r, y0))
        .cubic_to(
            Point::new(x1 - r + K * r, y0),
            Point::new(x1, y0 + r - K * r),
            Point::new(x1, y0 + r),
        )
        .line_to(Point::new(x1, y1))
        .close();
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

/// The style of a control's name, in the theme's secondary text colour.
pub(crate) fn name_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .label
        .with_color(theme.palette.text_secondary)
        .with_align(manguebyte_gfx::Align::Center)
}

/// The style of a control's value, in the theme's text colour.
pub(crate) fn value_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .value
        .with_color(theme.palette.text)
        .with_align(manguebyte_gfx::Align::Center)
}
