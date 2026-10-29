//! A horizontal control for one parameter.

use manguebyte_gfx::{Align, Circle, Point, Rect, RoundedRect, Scene, Size, Stroke, TextStyle};

use super::adjust::{Adjust, Axis};
use super::{
    Cached, GAP, MARGIN, draw_text, drop_shadow, focus_ring, line, line_height, name_style,
    text_width, value_style, vertical,
};
use crate::access::{AccessNode, Action, Role};
use crate::event::{Event, Handled};
use crate::motion::Lerp;
use crate::param::ParamHandle;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// Room around the thumb, beyond the margin, for its shadow, its halo
/// under the pointer and the focus ring.
const ROOM: f32 = 4.0;

/// A slider: the parameter's name and value on one line, and a track with
/// a thumb below.
///
/// Dragging to the right raises the value, and the thumb follows the
/// pointer; everything else works as for a [`Knob`](super::Knob): Shift
/// for fine changes, Escape to cancel a drag, a double click or Enter to
/// type a value (on the line of the name and value), a click with Alt,
/// Control or Command for the default, the wheel and the keys in steps.
pub struct Slider {
    adjust: Adjust,
    label: Option<String>,
    name: Cached<TextStyle>,
    /// Made for the value shown, as bits, whether it is a value sent, and
    /// the style.
    value: Cached<(u64, bool, TextStyle)>,
}

/// A slider for `param`.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gfx::Size;
/// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Action, ParamHandle, Role, slider};
///
/// struct Params { mix: FloatParam }
/// param_set!(Params { mix });
/// let params = Arc::new(Params {
///     mix: FloatParam::new("mix", "Mix", FloatRange::linear(0.0, 100.0), 100.0)
///         .with_unit("%")
///         .with_precision(0),
/// });
/// let view = slider(ParamHandle::new(Arc::clone(&params), |p| &p.mix));
/// let mut ui = Harness::with_params(view, Size::new(200.0, 40.0), 2.0, params.clone());
/// ui.frame();
/// let mix = ui.get(Role::Slider, "Mix");
/// assert_eq!(mix.value_text(), Some("100 %"));
///
/// // The thumb follows the pointer: its centre runs along the rectangle
/// // less a margin of 2, room of 4 for its shadow and half the 12-pixel
/// // thumb at each end.
/// let track = mix.bounds.width - 2.0 * (2.0 + 4.0 + 6.0);
/// ui.drag(mix.center(), (-track / 4.0, 0.0), 1);
/// assert!((params.mix.value() - 75.0).abs() < 1e-3);
///
/// // A screen reader sets it in the parameter's own unit.
/// assert!(ui.act(&mix, Action::SetValue(20.0)));
/// assert!((params.mix.value() - 20.0).abs() < 1e-4);
/// ```
pub fn slider(param: ParamHandle) -> Slider {
    Slider::new(param)
}

/// The track's ends and its vertical centre, inside `rect`.
fn track(rect: Rect, label_height: f32, thumb: f32) -> (f32, f32, f32) {
    let x0 = rect.x + MARGIN + ROOM + thumb * 0.5;
    let x1 = (rect.right() - MARGIN - ROOM - thumb * 0.5).max(x0);
    let y = rect.y + MARGIN + label_height + GAP + thumb * 0.5;
    (x0, x1, y)
}

impl Slider {
    /// A slider for `param`, named by the parameter.
    pub fn new(param: ParamHandle) -> Slider {
        Slider {
            adjust: Adjust::new(param, Axis::Horizontal),
            label: None,
            name: Cached::new(),
            value: Cached::new(),
        }
    }

    /// The same slider showing `text` as its name instead of the
    /// parameter's name.
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Slider {
        self.label = Some(text.into());
        self
    }

    fn name_text(&self) -> &str {
        name_of(self.label.as_deref(), &self.adjust)
    }
}

/// The name shown: the label if there is one, or the parameter's.
fn name_of<'a>(label: Option<&'a str>, adjust: &'a Adjust) -> &'a str {
    label.unwrap_or_else(|| adjust.binding.handle().name())
}

impl Widget for Slider {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        self.adjust.access(Role::Slider, self.name_text())
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        let name = name_style(theme);
        let value = value_style(theme);
        let handle = self.adjust.binding.handle();
        let widest_value = [
            handle.text(0.0),
            handle.text(1.0),
            self.adjust.binding.text(),
        ]
        .iter()
        .map(|text| text_width(text, &value))
        .fold(0.0f32, f32::max);
        let width = theme
            .sizes
            .slider_width
            .max(text_width(self.name_text(), &name) + widest_value + 12.0);
        // Room under the thumb for its shadow and halo.
        let height =
            line_height(&name).max(line_height(&value)) + GAP + theme.sizes.slider_thumb + ROOM;
        Size::new(width + 2.0 * MARGIN, height + 2.0 * MARGIN)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.adjust.binding.poll(cx.now()) {
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let width = (rect.width - 2.0 * MARGIN).max(0.0);
        let name_style = name_style(theme).with_align(Align::Start);
        let value_style = value_style(theme).with_align(Align::End);
        let label_height = line_height(&name_style).max(line_height(&value_style));
        let sizes = theme.sizes;
        let thumb = sizes.slider_thumb.max(0.0);
        let (x0, x1, y) = track(rect, label_height, thumb);
        let thickness = sizes.slider_track.min(thumb).max(0.0);
        let value = self.adjust.binding.shown().clamp(0.0, 1.0) as f32;
        let at = x0 + (x1 - x0) * value;
        let bar = |from: f32, to: f32| {
            RoundedRect::new(
                Rect::new(
                    from - thickness * 0.5,
                    y - thickness * 0.5,
                    to - from + thickness,
                    thickness,
                ),
                thickness * 0.5,
            )
        };
        let palette = &theme.palette;
        let hover = cx.hover();
        scene.fill(
            bar(x0, x1),
            palette.track.lerp(palette.hairline_strong, hover),
        );
        if value > 0.0 {
            let filled = bar(x0, at);
            let (top, bottom) = (y - thickness * 0.5, y + thickness * 0.5);
            vertical(
                scene,
                filled,
                (top, bottom),
                palette.accent_bright,
                palette.accent,
            );
        }
        // The thumb, lifted off the track, with a halo of the accent's tint
        // under the pointer.
        let knob = Circle::new(Point::new(at, y), thumb * 0.5);
        if hover > 0.0 {
            scene.fill(
                Circle::new(Point::new(at, y), thumb * 0.5 + 4.0 * hover),
                palette.accent_soft.with_opacity(hover),
            );
        }
        drop_shadow(scene, knob, super::thumb_shadow(theme));
        scene.fill(knob, palette.thumb);
        scene.stroke(
            Circle::new(Point::new(at, y), (thumb * 0.5 - 0.5).max(0.0)),
            Stroke::new(1.0),
            palette.hairline,
        );
        focus_ring(
            scene,
            Circle::new(Point::new(at, y), thumb * 0.5 + theme.sizes.focus_gap),
            theme,
            cx.focus_ring(),
        );

        let text_y = rect.y + MARGIN;
        let entry_box = Rect::new(rect.x + MARGIN, text_y, width, label_height);
        if !self
            .adjust
            .paint_entry(scene, entry_box, theme, cx.is_focused())
        {
            let (label, adjust) = (self.label.as_deref(), &self.adjust);
            let name = self.name.get(width, name_style, || {
                line(name_of(label, adjust), &name_style, width)
            });
            draw_text(scene, name, Point::new(rect.x + MARGIN, text_y));
            let binding = &adjust.binding;
            let key = (binding.shown().to_bits(), binding.shows_sent(), value_style);
            let value = self
                .value
                .get(width, key, || line(&binding.text(), &value_style, width));
            draw_text(scene, value, Point::new(rect.x + MARGIN, text_y));
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let theme = cx.theme();
        let thumb = theme.sizes.slider_thumb.max(0.0);
        let rect = cx.rect();
        let (x0, x1, _) = track(rect, 0.0, thumb);
        let fine = theme.sizes.fine_factor;
        let label_height = line_height(&name_style(theme)).max(line_height(&value_style(theme)));
        let entry_box = Rect::new(
            rect.x + MARGIN,
            rect.y + MARGIN,
            (rect.width - 2.0 * MARGIN).max(0.0),
            label_height,
        );
        self.adjust.event(cx, event, x1 - x0, fine, entry_box)
    }

    fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        self.adjust.act(cx, action)
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        self.adjust.focus_changed(cx, focused);
    }

    fn caret(&self, _cx: &AccessCx<'_>) -> Option<Rect> {
        self.adjust.caret()
    }
}
