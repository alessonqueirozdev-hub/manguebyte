//! An envelope editor: attack, decay, sustain and release drawn as a
//! curve whose corners are dragged.

use manguebyte_gfx::{Align, Circle, Path, Point, Rect, RoundedRect, Scene, Size, Stroke};

use super::adjust::{Adjust, Axis};
use super::{GAP, MARGIN, focus_ring, hline, line, line_height, value_style};
use crate::access::{AccessItem, AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent, Modifiers};
use crate::param::ParamHandle;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// The parts of an envelope, as indices into the editor's parameters.
const ATTACK: usize = 0;
const DECAY: usize = 1;
const SUSTAIN: usize = 2;
const RELEASE: usize = 3;
const ATTACK_CURVE: usize = 4;
const DECAY_CURVE: usize = 5;
const RELEASE_CURVE: usize = 6;

/// The order Left and Right, and assistive technology, go through the
/// parts in: as they are drawn, from left to right.
const ORDER: [usize; 7] = [
    ATTACK,
    ATTACK_CURVE,
    DECAY,
    DECAY_CURVE,
    SUSTAIN,
    RELEASE,
    RELEASE_CURVE,
];

/// Room kept around the curve for the handles, in logical pixels.
const PAD: f32 = 8.0;

/// The radius of a handle.
const HANDLE: f32 = 4.0;

/// How near a handle a press takes it, in logical pixels.
const GRAB: f32 = 9.0;

/// Something a person drags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Handle {
    /// The peak: the attack, sideways.
    Attack,
    /// The end of the decay: the decay sideways, the sustain up and down.
    Decay,
    /// The end of the sustain: the sustain, up and down.
    Sustain,
    /// The end of the release: the release, sideways.
    Release,
    /// The middle of a curved segment: its curve, up and down.
    Curve(usize),
}

impl Handle {
    /// The parameters it moves, and along which axis.
    fn parts(self) -> &'static [(usize, Axis)] {
        match self {
            Handle::Attack => &[(ATTACK, Axis::Horizontal)],
            Handle::Decay => &[(DECAY, Axis::Horizontal), (SUSTAIN, Axis::Vertical)],
            Handle::Sustain => &[(SUSTAIN, Axis::Vertical)],
            Handle::Release => &[(RELEASE, Axis::Horizontal)],
            Handle::Curve(ATTACK_CURVE) => &[(ATTACK_CURVE, Axis::Vertical)],
            Handle::Curve(DECAY_CURVE) => &[(DECAY_CURVE, Axis::Vertical)],
            Handle::Curve(_) => &[(RELEASE_CURVE, Axis::Vertical)],
        }
    }

    /// The handle a part is moved by.
    fn of(part: usize) -> Handle {
        match part {
            ATTACK => Handle::Attack,
            DECAY => Handle::Decay,
            SUSTAIN => Handle::Sustain,
            RELEASE => Handle::Release,
            curve => Handle::Curve(curve),
        }
    }
}

/// A drag of a handle.
#[derive(Debug, Clone, Copy)]
struct Drag {
    handle: Handle,
    /// Where the pointer was when the values were `starts`.
    origin: Point,
    starts: [f64; 2],
    fine: bool,
}

/// Where everything is, for one rectangle and the values shown.
#[derive(Debug, Clone, Copy)]
struct Shape {
    /// Where the curve is drawn.
    graph: Rect,
    /// The line under it where the value of the part chosen is shown and
    /// typed.
    readout: Rect,
    /// The start, the peak, the end of the decay, the end of the sustain,
    /// and the end of the release.
    points: [Point; 5],
    /// The control points of the attack, decay and release, which bend
    /// them by their curves.
    controls: [Point; 3],
}

impl Shape {
    fn new(rect: Rect, theme: &Theme, values: [f64; 4], curves: [f64; 3]) -> Shape {
        let height = line_height(&value_style(theme));
        let readout = Rect::new(
            rect.x + MARGIN,
            rect.bottom() - MARGIN - height,
            (rect.width - 2.0 * MARGIN).max(0.0),
            height,
        );
        let graph = Rect::new(
            rect.x + PAD,
            rect.y + PAD,
            (rect.width - 2.0 * PAD).max(0.0),
            (readout.y - GAP - rect.y - 2.0 * PAD).max(0.0),
        );
        let slot = graph.width / 4.0;
        let [attack, decay, sustain, release] = values.map(|value| value.clamp(0.0, 1.0) as f32);
        let bottom = graph.bottom();
        let sustain_y = graph.y + (1.0 - sustain) * graph.height;
        let start = Point::new(graph.x, bottom);
        let peak = Point::new(graph.x + attack * slot, graph.y);
        let decayed = Point::new(peak.x + decay * slot, sustain_y);
        let held = Point::new(decayed.x + slot, sustain_y);
        let end = Point::new(held.x + release * slot, bottom);
        let control = |from: Point, to: Point, curve: f64| {
            // 0.5 is a straight line; above it the segment moves fast at
            // first, below it slowly.
            let bend = (curve.clamp(0.0, 1.0) * 2.0 - 1.0) as f32;
            let mid = Point::new((from.x + to.x) * 0.5, (from.y + to.y) * 0.5);
            let corner = Point::new(from.x, to.y);
            Point::new(
                mid.x + bend * (corner.x - mid.x),
                mid.y + bend * (corner.y - mid.y),
            )
        };
        Shape {
            graph,
            readout,
            points: [start, peak, decayed, held, end],
            controls: [
                control(start, peak, curves[0]),
                control(peak, decayed, curves[1]),
                control(held, end, curves[2]),
            ],
        }
    }

    /// Where `handle` is.
    fn handle(&self, handle: Handle) -> Point {
        let middle = |from: Point, control: Point, to: Point| {
            Point::new(
                0.25 * from.x + 0.5 * control.x + 0.25 * to.x,
                0.25 * from.y + 0.5 * control.y + 0.25 * to.y,
            )
        };
        let [start, peak, decayed, held, end] = self.points;
        match handle {
            Handle::Attack => peak,
            Handle::Decay => decayed,
            Handle::Sustain => held,
            Handle::Release => end,
            Handle::Curve(ATTACK_CURVE) => middle(start, self.controls[0], peak),
            Handle::Curve(DECAY_CURVE) => middle(peak, self.controls[1], decayed),
            Handle::Curve(_) => middle(held, self.controls[2], end),
        }
    }

    /// The width the pointer moves to take a time across its range, and
    /// the height to take a level or a curve across its range.
    fn spans(&self) -> (f32, f32) {
        (
            (self.graph.width / 4.0).max(1.0),
            self.graph.height.max(1.0),
        )
    }

    /// The part whose segment is under `x`.
    fn part_at(&self, x: f32) -> usize {
        let [_, peak, decayed, held, _] = self.points;
        if x < peak.x {
            ATTACK
        } else if x < decayed.x {
            DECAY
        } else if x < held.x {
            SUSTAIN
        } else {
            RELEASE
        }
    }

    /// The outline of the envelope, from the start to the end of the
    /// release.
    fn path(&self) -> Path {
        let [start, peak, decayed, held, end] = self.points;
        let mut path = Path::new();
        path.move_to(start)
            .quad_to(self.controls[0], peak)
            .quad_to(self.controls[1], decayed)
            .line_to(held)
            .quad_to(self.controls[2], end);
        path
    }
}

/// An editor of an ADSR envelope bound to four parameters, and to curves
/// for its attack, decay and release if the application has them.
///
/// The envelope is drawn as a curve: the attack rises to the peak, the
/// decay falls to the sustain level, the sustain holds for a fixed width
/// and the release falls to nothing. Each time takes up to a quarter of
/// the width, by its parameter's position in its range (its own scale,
/// logarithmic if the parameter's range is). A parameter given as a curve
/// bends its segment: at the middle of its range the segment is straight,
/// above it the segment moves fast at first, below it slowly.
///
/// - The handles are dragged directly: the peak sideways for the attack,
///   the end of the decay sideways for the decay and up and down for the
///   sustain, the end of the sustain up and down, the end of the release
///   sideways, and the middle of a curved segment up and down for its
///   curve. A quarter of the width, or the height, is a whole range;
///   Shift makes it [`fine_factor`](crate::Sizes::fine_factor) times as
///   far. Each drag is one gesture per parameter it moves, from press to
///   release, as a knob's is, so hosts record it as automation; Escape
///   puts the values back.
/// - A double click on a handle, or Enter, opens its value to be typed on
///   the line under the curve, read by the parameter's own parser. A
///   click with Alt, Control or Command puts its defaults back. The wheel
///   moves the segment under the pointer one step.
/// - With the keyboard focus: Left and Right choose a part, Up and Down
///   move it a step, Page Up and Page Down a tenth, Home and End to its
///   ends, Delete to its default.
///
/// To assistive technology the editor is a [`Role::Group`] and each part
/// a [`Role::Slider`] with its value in the parameter's own words, which
/// can be raised, lowered and set.
pub struct EnvelopeEditor {
    name: String,
    parts: [Option<Adjust>; 7],
    /// The part the keyboard and the line under the curve are on.
    current: usize,
    drag: Option<Drag>,
    /// A press on the value being typed holds the pointer.
    entry_pointer: bool,
}

/// An editor called `name` of the envelope `attack`, `decay`, `sustain`
/// and `release` make.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gfx::Size;
/// use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{ParamHandle, Role, envelope};
///
/// struct Params { attack: FloatParam, decay: FloatParam, sustain: FloatParam, release: FloatParam }
/// param_set!(Params { attack, decay, sustain, release });
/// let time = |id, name, ms| {
///     FloatParam::new(id, name, FloatRange::linear(0.0, 1000.0), ms).with_unit("ms").with_precision(0)
/// };
/// let params = Arc::new(Params {
///     attack: time("attack", "Attack", 100.0),
///     decay: time("decay", "Decay", 300.0),
///     sustain: FloatParam::new("sustain", "Sustain", FloatRange::linear(0.0, 100.0), 50.0)
///         .with_unit("%")
///         .with_precision(0),
///     release: time("release", "Release", 500.0),
/// });
/// let handle = |field: fn(&Params) -> &FloatParam| ParamHandle::new(Arc::clone(&params), field);
/// let view = envelope(
///     "Amp envelope",
///     handle(|p| &p.attack),
///     handle(|p| &p.decay),
///     handle(|p| &p.sustain),
///     handle(|p| &p.release),
/// );
/// let mut ui = Harness::with_params(view, Size::new(336.0, 140.0), 1.0, params.clone());
/// ui.frame();
/// // The graph is 320 wide: a time's range is 80 pixels. The peak, dragged
/// // 20 pixels right, adds a quarter of the attack's range.
/// let attack = ui.get(Role::Slider, "Attack");
/// ui.drag(attack.center(), (20.0, 0.0), 2);
/// assert!((params.attack.value() - 350.0).abs() < 1e-3);
/// ui.frame();
/// assert_eq!(ui.get(Role::Slider, "Attack").value_text(), Some("350 ms"));
/// ```
pub fn envelope(
    name: impl Into<String>,
    attack: ParamHandle,
    decay: ParamHandle,
    sustain: ParamHandle,
    release: ParamHandle,
) -> EnvelopeEditor {
    let adjust = |handle| Some(Adjust::new(handle, Axis::Vertical));
    EnvelopeEditor {
        name: name.into(),
        parts: [
            adjust(attack),
            adjust(decay),
            adjust(sustain),
            adjust(release),
            None,
            None,
            None,
        ],
        current: ATTACK,
        drag: None,
        entry_pointer: false,
    }
}

impl EnvelopeEditor {
    /// The same editor with `curve` bending the attack.
    #[must_use]
    pub fn attack_curve(mut self, curve: ParamHandle) -> EnvelopeEditor {
        self.parts[ATTACK_CURVE] = Some(Adjust::new(curve, Axis::Vertical));
        self
    }

    /// The same editor with `curve` bending the decay.
    #[must_use]
    pub fn decay_curve(mut self, curve: ParamHandle) -> EnvelopeEditor {
        self.parts[DECAY_CURVE] = Some(Adjust::new(curve, Axis::Vertical));
        self
    }

    /// The same editor with `curve` bending the release.
    #[must_use]
    pub fn release_curve(mut self, curve: ParamHandle) -> EnvelopeEditor {
        self.parts[RELEASE_CURVE] = Some(Adjust::new(curve, Axis::Vertical));
        self
    }

    fn shown(&self, part: usize) -> f64 {
        self.parts[part]
            .as_ref()
            .map_or(0.5, |adjust| adjust.binding.shown())
    }

    fn shape(&self, rect: Rect, theme: &Theme) -> Shape {
        Shape::new(
            rect,
            theme,
            [
                self.shown(ATTACK),
                self.shown(DECAY),
                self.shown(SUSTAIN),
                self.shown(RELEASE),
            ],
            [
                self.shown(ATTACK_CURVE),
                self.shown(DECAY_CURVE),
                self.shown(RELEASE_CURVE),
            ],
        )
    }

    /// The parts there are, in order.
    fn present(&self) -> impl Iterator<Item = usize> + '_ {
        ORDER.into_iter().filter(|&part| self.parts[part].is_some())
    }

    /// The handles there are.
    fn handles(&self) -> impl Iterator<Item = Handle> + '_ {
        [
            Handle::Attack,
            Handle::Decay,
            Handle::Sustain,
            Handle::Release,
        ]
        .into_iter()
        .chain(
            [ATTACK_CURVE, DECAY_CURVE, RELEASE_CURVE]
                .into_iter()
                .filter(|&part| self.parts[part].is_some())
                .map(Handle::Curve),
        )
    }

    /// The handle nearest `at`, if one is near enough. The corners come
    /// before the curves' handles.
    fn handle_at(&self, shape: &Shape, at: Point) -> Option<Handle> {
        self.handles()
            .map(|handle| {
                let point = shape.handle(handle);
                (handle, (point.x - at.x).hypot(point.y - at.y))
            })
            .filter(|&(_, distance)| distance <= GRAB)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(handle, _)| handle)
    }

    /// The part a value is being typed into, if any.
    fn typing(&self) -> Option<usize> {
        (0..7).find(|&part| self.parts[part].as_ref().is_some_and(Adjust::is_typing))
    }

    fn part_name(&self, part: usize) -> &str {
        self.parts[part]
            .as_ref()
            .map_or("", |adjust| adjust.binding.handle().name())
    }

    /// Passes `event` to `part`'s own handling: keys, the wheel, typing
    /// and the clicks that type or reset a value.
    fn pass_to(&mut self, cx: &mut EventCx<'_>, part: usize, event: &Event) -> Handled {
        let sizes = cx.theme().sizes;
        let readout = self.shape(cx.rect(), cx.theme()).readout;
        match self.parts[part].as_mut() {
            Some(adjust) => adjust.event(cx, event, sizes.drag_range, sizes.fine_factor, readout),
            None => Handled::No,
        }
    }

    fn press(
        &mut self,
        cx: &mut EventCx<'_>,
        at: Point,
        clicks: u32,
        modifiers: Modifiers,
    ) -> Handled {
        let shape = self.shape(cx.rect(), cx.theme());
        if let Some(part) = self.typing() {
            if shape.readout.contains(at) {
                self.entry_pointer = true;
                return self.pass_to(cx, part, &press_event(at, clicks, modifiers));
            }
            // A press elsewhere takes what was typed, if it reads as a value.
            if let Some(adjust) = self.parts[part].as_mut() {
                adjust.focus_changed(cx, false);
            }
        }
        cx.request_focus();
        cx.request_paint();
        let Some(handle) = self.handle_at(&shape, at) else {
            return Handled::Yes;
        };
        let parts = handle.parts();
        self.current = parts[0].0;
        if clicks >= 2 || modifiers.alt || modifiers.control || modifiers.meta {
            // A double click types the handle's first value; a click with a
            // modifier puts its values' defaults back.
            self.end_drag();
            let event = press_event(at, clicks, modifiers);
            if clicks >= 2 {
                self.pass_to(cx, parts[0].0, &event);
            } else {
                for &(part, _) in parts {
                    self.pass_to(cx, part, &event);
                }
            }
            return Handled::Yes;
        }
        let mut starts = [0.0; 2];
        for (slot, &(part, _)) in parts.iter().enumerate() {
            if let Some(adjust) = self.parts[part].as_mut() {
                starts[slot] = adjust.binding.current();
                adjust.binding.begin(cx.context());
            }
        }
        self.drag = Some(Drag {
            handle,
            origin: at,
            starts,
            fine: modifiers.shift,
        });
        cx.capture_pointer();
        Handled::Yes
    }

    fn drag_to(&mut self, cx: &mut EventCx<'_>, at: Point, modifiers: Modifiers) {
        let Some(mut drag) = self.drag else {
            return;
        };
        let parts = drag.handle.parts();
        if modifiers.shift != drag.fine {
            // Changing speed starts again from here, so nothing jumps.
            for (slot, &(part, _)) in parts.iter().enumerate() {
                drag.starts[slot] = self.shown(part);
            }
            drag.origin = at;
            drag.fine = modifiers.shift;
            self.drag = Some(drag);
        }
        let (width, height) = self.shape(cx.rect(), cx.theme()).spans();
        let fine = cx.theme().sizes.fine_factor;
        let factor = if drag.fine && fine.is_finite() {
            f64::from(fine.max(1.0))
        } else {
            1.0
        };
        for (slot, &(part, axis)) in parts.iter().enumerate() {
            let travel = match axis {
                Axis::Horizontal => f64::from((at.x - drag.origin.x) / width),
                Axis::Vertical => f64::from((drag.origin.y - at.y) / height),
            };
            if let Some(adjust) = self.parts[part].as_mut() {
                adjust.binding.set(drag.starts[slot] + travel / factor);
            }
        }
        cx.request_paint();
    }

    /// Ends the drag's gestures.
    fn end_drag(&mut self) {
        if let Some(drag) = self.drag.take() {
            for &(part, _) in drag.handle.parts() {
                if let Some(adjust) = self.parts[part].as_mut() {
                    adjust.binding.end();
                }
            }
        }
    }

    /// Ends the drag's gestures, putting the values back.
    fn cancel_drag(&mut self) -> bool {
        let Some(drag) = self.drag.take() else {
            return false;
        };
        for &(part, _) in drag.handle.parts() {
            if let Some(adjust) = self.parts[part].as_mut() {
                adjust.binding.cancel();
            }
        }
        true
    }

    fn key(&mut self, cx: &mut EventCx<'_>, event: &Event, key: KeyEvent) -> Handled {
        if key.pressed && matches!(key.key, Key::Left | Key::Right) && !key.modifiers.alt {
            let order: Vec<usize> = self.present().collect();
            let at = order
                .iter()
                .position(|&part| part == self.current)
                .unwrap_or(0);
            let next = if key.key == Key::Left {
                at.saturating_sub(1)
            } else {
                (at + 1).min(order.len() - 1)
            };
            self.current = order[next];
            cx.request_paint();
            return Handled::Yes;
        }
        self.pass_to(cx, self.current, event)
    }
}

/// A press of the main button at `at`.
fn press_event(at: Point, clicks: u32, modifiers: Modifiers) -> Event {
    Event::PointerDown {
        at,
        button: Button::Primary,
        clicks,
        modifiers,
    }
}

impl Widget for EnvelopeEditor {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let value = [ATTACK, DECAY, SUSTAIN, RELEASE]
            .into_iter()
            .filter_map(|part| {
                let adjust = self.parts[part].as_ref()?;
                Some(format!(
                    "{} {}",
                    adjust.binding.handle().name(),
                    adjust.binding.text()
                ))
            })
            .collect::<Vec<_>>()
            .join(", ");
        AccessNode::new(Role::Group, self.name.as_str())
            .with_value(value)
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let rect = cx.rect();
        let shape = self.shape(rect, cx.theme());
        for part in self.present() {
            let Some(adjust) = self.parts[part].as_ref() else {
                continue;
            };
            let point = shape.handle(Handle::of(part));
            let around = Rect::new(point.x - GRAB, point.y - GRAB, 2.0 * GRAB, 2.0 * GRAB);
            let bounds = around.intersect(&rect).unwrap_or(around);
            let mut node = adjust.access(Role::Slider, self.part_name(part));
            node.state.focused = cx.is_focused() && self.current == part;
            items.push(AccessItem::new(part, node, bounds));
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        Size::new(
            theme.sizes.slider_width * 2.0,
            theme.sizes.meter_height + line_height(&value_style(theme)) + 2.0 * MARGIN + GAP,
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let now = cx.now();
        let mut changed = false;
        for adjust in self.parts.iter_mut().flatten() {
            changed |= adjust.binding.poll(now);
        }
        if changed {
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let scale = cx.scale();
        let shape = self.shape(rect, theme);
        let graph = shape.graph;
        scene.fill(
            RoundedRect::new(rect, theme.corners.small),
            theme.palette.background,
        );
        hline(
            scene,
            graph.y,
            (graph.x, graph.right()),
            1.0,
            scale,
            theme.palette.track,
        );
        hline(
            scene,
            graph.bottom(),
            (graph.x, graph.right()),
            1.0,
            scale,
            theme.palette.track,
        );
        let outline = shape.path();
        let mut area = outline.clone();
        area.line_to(shape.points[0]).close();
        scene.fill(&area, theme.palette.accent.with_alpha(0x40));
        scene.stroke(&outline, Stroke::round(2.0), theme.palette.accent);
        let focused = cx.is_focused();
        let active: &[(usize, Axis)] = self.drag.map_or(&[], |drag| drag.handle.parts());
        for handle in self.handles() {
            let point = shape.handle(handle);
            let parts = handle.parts();
            let is_active = active.first() == parts.first();
            let is_current = focused && parts.iter().any(|&(part, _)| part == self.current);
            let radius = if matches!(handle, Handle::Curve(_)) {
                HANDLE - 1.0
            } else {
                HANDLE
            };
            let color = if is_active {
                theme.palette.accent
            } else if matches!(handle, Handle::Curve(_)) {
                theme.palette.text_muted
            } else {
                theme.palette.thumb
            };
            scene.fill(Circle::new(point, radius), color);
            if is_current {
                scene.stroke(
                    Circle::new(point, radius + 2.0),
                    Stroke::new(1.5),
                    theme.palette.focus,
                );
            }
        }
        // The line under the curve: the part chosen and its value, or the
        // value being typed.
        let current = self.current;
        let name = self.part_name(current).to_owned();
        let readout = shape.readout;
        let typed = self.parts[current]
            .as_mut()
            .is_some_and(|adjust| adjust.paint_entry(scene, readout, theme, focused));
        if !typed {
            if let Some(adjust) = self.parts[current].as_ref() {
                let text = format!("{name} {}", adjust.binding.text());
                let style = value_style(theme).with_align(Align::Center);
                let layout = line(&text, &style, readout.width);
                scene.text(&layout, readout.origin());
            }
        }
        if focused {
            focus_ring(scene, rect, theme);
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } => self.press(cx, at, clicks, modifiers),
            Event::PointerMove { .. } | Event::PointerUp { .. } | Event::PointerCancel
                if self.entry_pointer =>
            {
                if !matches!(event, Event::PointerMove { .. }) {
                    self.entry_pointer = false;
                }
                match self.typing() {
                    Some(part) => self.pass_to(cx, part, event),
                    None => Handled::No,
                }
            }
            Event::PointerMove { at, modifiers } if self.drag.is_some() => {
                self.drag_to(cx, at, modifiers);
                Handled::Yes
            }
            Event::PointerUp { .. } if self.drag.is_some() => {
                self.end_drag();
                cx.release_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerCancel => {
                self.cancel_drag();
                Handled::No
            }
            Event::Wheel { at, .. } => {
                let shape = self.shape(cx.rect(), cx.theme());
                let part = match self.typing() {
                    Some(part) => part,
                    None => shape.part_at(at.x),
                };
                self.current = part;
                self.pass_to(cx, part, event)
            }
            Event::Key(key) => {
                if self.drag.is_some() {
                    if key.pressed && key.key == Key::Escape && self.cancel_drag() {
                        cx.release_pointer();
                        cx.request_paint();
                        return Handled::Yes;
                    }
                    return Handled::No;
                }
                match self.typing() {
                    Some(part) => self.pass_to(cx, part, event),
                    None if cx.is_focused() => self.key(cx, event, key),
                    None => Handled::No,
                }
            }
            Event::Text(_) | Event::Composition(_) => match self.typing() {
                Some(part) => self.pass_to(cx, part, event),
                None => Handled::No,
            },
            _ => Handled::No,
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        if self.parts.get(key).is_none_or(Option::is_none) {
            return false;
        }
        if action == Action::Focus {
            self.current = key;
            cx.request_paint();
            return true;
        }
        self.parts[key]
            .as_mut()
            .is_some_and(|adjust| adjust.act(cx, action))
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        for adjust in self.parts.iter_mut().flatten() {
            adjust.focus_changed(cx, focused);
        }
        cx.request_paint();
    }

    fn caret(&self, _cx: &AccessCx<'_>) -> Option<Rect> {
        let part = self.typing()?;
        self.parts[part].as_ref()?.caret()
    }
}

impl std::fmt::Debug for EnvelopeEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnvelopeEditor")
            .field("name", &self.name)
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}
