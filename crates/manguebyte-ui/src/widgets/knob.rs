//! A rotary control for one parameter.

use std::f32::consts::PI;

use manguebyte_gfx::{Arc, Path, Point, Rect, Scene, Size, Stroke, TextStyle};

use super::adjust::{Adjust, Axis};
use super::{
    Cached, GAP, MARGIN, draw_text, focus_ring, line, line_height, name_style, text_width,
    value_style,
};
use crate::access::{AccessNode, Action, Role};
use crate::event::{Event, Handled};
use crate::motion::Lerp;
use crate::param::ParamHandle;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// Where the arc starts: down and to the left.
const START: f32 = 0.75 * PI;

/// How far the arc runs, clockwise: three quarters of a turn.
const SWEEP: f32 = 1.5 * PI;

/// A knob: the parameter's name, a ring with an arc and a pointer for the
/// value, and the value as text in the parameter's own words.
///
/// - Dragging up raises the value and dragging down lowers it; the theme's
///   [`drag_range`](crate::Sizes::drag_range) is a whole sweep, and
///   with Shift held the same change takes
///   [`fine_factor`](crate::Sizes::fine_factor) times as far. The
///   drag is one gesture from press to release, so a click that does not
///   move sends a begin and an end with no value between; Escape during
///   it puts the starting value back.
/// - A double click, or Enter with the keyboard focus, opens the value to
///   be typed: the value's text, all selected, in a box on the value's
///   line, edited as a [`TextField`](super::TextField) is. Enter takes
///   it, read by the parameter's own parser
///   ([`ParamHandle::parse`](crate::ParamHandle::parse), with or without
///   the unit) and sent as one edit: begin, the value, end. Text that
///   reads as no value keeps the box open and marks it wrong; Escape
///   closes it and sends nothing. A press elsewhere, or the knob losing
///   the focus, takes what was typed if it reads as a value and throws it
///   away if not. While the box is open, assistive technology sees a
///   [`TextInput`](crate::Role::TextInput) with the knob's name.
/// - A click with Alt, Control or Command held puts the default back.
/// - Each notch of the wheel moves it by one step: one of the parameter's
///   steps, or a hundredth of the range.
/// - With the keyboard focus, which a press on the knob gives it: the
///   arrows move one step (a tenth of one with Shift, for continuous
///   parameters), Page Up and Page Down a tenth of the range, Home and End
///   to the ends, Delete and Backspace to the default. Each key press is
///   one edit.
///
/// While a gesture is open the knob shows the value it is sending, and
/// after an edit the value it sent until the host echoes it (see
/// [`ParamBinding`](crate::ParamBinding)). Every edit moves on from the
/// value read at that moment.
pub struct Knob {
    adjust: Adjust,
    label: Option<String>,
    name: Cached<TextStyle>,
    /// Made for the value shown, as bits, whether it is a value sent, and
    /// the style.
    value: Cached<(u64, bool, TextStyle)>,
}

/// A knob for `param`.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gfx::{Point, Size};
/// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, ParamHandle, Role, knob};
///
/// struct Params { tone: FloatParam }
/// param_set!(Params { tone });
/// let params = Arc::new(Params {
///     tone: FloatParam::new("tone", "Tone", FloatRange::linear(0.0, 100.0), 50.0)
///         .with_unit("%")
///         .with_precision(0),
/// });
/// let view = knob(ParamHandle::new(Arc::clone(&params), |p| &p.tone)).label("Colour");
/// let mut ui = Harness::with_params(view, Size::new(80.0, 100.0), 1.0, params.clone());
/// ui.frame();
/// let colour = ui.get(Role::Slider, "Colour");
/// assert_eq!(colour.value_text(), Some("50 %"));
///
/// // Twenty pixels up: a tenth of the theme's 200-pixel drag range.
/// ui.drag(colour.center(), (0.0, -20.0), 2);
/// assert!((params.tone.value() - 60.0).abs() < 1e-4);
/// // The press gave it the keyboard focus: each arrow is one step.
/// assert!(ui.key(Key::Down));
/// assert!((params.tone.value() - 59.0).abs() < 1e-4);
/// ui.frame();
/// assert_eq!(ui.get(Role::Slider, "Colour").value_text(), Some("59 %"));
/// ```
pub fn knob(param: ParamHandle) -> Knob {
    Knob::new(param)
}

impl Knob {
    /// A knob for `param`, named by the parameter.
    pub fn new(param: ParamHandle) -> Knob {
        Knob {
            adjust: Adjust::new(param, Axis::Vertical),
            label: None,
            name: Cached::new(),
            value: Cached::new(),
        }
    }

    /// The same knob showing `text` as its name, for people and for
    /// assistive technology, instead of the parameter's name.
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> Knob {
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

impl Widget for Knob {
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
            .knob
            .max(text_width(self.name_text(), &name))
            .max(widest_value + 4.0);
        Size::new(
            width + 2.0 * MARGIN,
            2.0 * MARGIN + line_height(&name) + GAP + theme.sizes.knob + GAP + line_height(&value),
        )
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
        let name_style = name_style(theme);
        let value_style = value_style(theme);
        let name_height = line_height(&name_style);
        let value_height = line_height(&value_style);

        // The dial, between the name and the value.
        let top = rect.y + MARGIN + name_height + GAP;
        let bottom = rect.bottom() - MARGIN - value_height - GAP;
        let room = (bottom - top).max(0.0);
        let diameter = theme.sizes.knob.min(room).min(width);
        let ring = theme.sizes.knob_ring.min(diameter * 0.5).max(0.0);
        let radius = (diameter - ring) * 0.5;
        let center = Point::new(rect.x + rect.width * 0.5, top + room * 0.5);
        let value = self.adjust.binding.shown().clamp(0.0, 1.0) as f32;
        if radius > 0.0 {
            // The track brightens a little as the pointer comes over.
            let track = theme
                .palette
                .track
                .lerp(theme.palette.text_secondary, 0.35 * cx.hover());
            scene.stroke(
                Arc::new(center, radius, START, SWEEP),
                Stroke::round(ring),
                track,
            );
            if value > 0.0 {
                scene.stroke(
                    Arc::new(center, radius, START, SWEEP * value),
                    Stroke::round(ring),
                    theme.palette.accent,
                );
            }
            let angle = START + SWEEP * value;
            let from = Arc::new(center, radius * 0.25, 0.0, 0.0).point_at(angle);
            let to = Arc::new(center, (radius - ring).max(0.0), 0.0, 0.0).point_at(angle);
            let mut pointer = Path::new();
            pointer.move_to(from).line_to(to);
            scene.stroke(&pointer, Stroke::round(2.0), theme.palette.thumb);
        }

        let (label, adjust) = (self.label.as_deref(), &self.adjust);
        let name = self.name.get(width, name_style, || {
            line(name_of(label, adjust), &name_style, width)
        });
        draw_text(scene, name, Point::new(rect.x + MARGIN, rect.y + MARGIN));
        let entry_box = value_line(rect, theme);
        if !self
            .adjust
            .paint_entry(scene, entry_box, theme, cx.is_focused())
        {
            let binding = &self.adjust.binding;
            let key = (binding.shown().to_bits(), binding.shows_sent(), value_style);
            let value = self
                .value
                .get(width, key, || line(&binding.text(), &value_style, width));
            draw_text(scene, value, entry_box.origin());
        }
        if cx.is_focused() {
            focus_ring(scene, rect, theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let sizes = cx.theme().sizes;
        let entry_box = value_line(cx.rect(), cx.theme());
        self.adjust
            .event(cx, event, sizes.drag_range, sizes.fine_factor, entry_box)
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

/// The line under the dial where the value is shown, and typed.
fn value_line(rect: Rect, theme: &Theme) -> Rect {
    let height = line_height(&value_style(theme));
    Rect::new(
        rect.x + MARGIN,
        rect.bottom() - MARGIN - height,
        (rect.width - 2.0 * MARGIN).max(0.0),
        height,
    )
}
