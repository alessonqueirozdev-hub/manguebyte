//! Pitch bend and modulation wheels, as a hardware keyboard has to the
//! left of its keys.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use manguebyte_core::AtomicF32;
use manguebyte_gfx::{Color, Extend, LinearGradient, Point, Rect, RoundedRect, Scene, Size, Stop};
use manguebyte_gfx::{Stroke, TextStyle};

use super::{
    Cached, GAP, MARGIN, device_px, draw_text, focus_ring, glow_fill, hline, line, line_height,
    name_style, snap_px, text_width, vertical,
};
use crate::access::{AccessNode, AccessRange, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::motion::{Animated, Lerp};
use crate::param::{ParamBinding, ParamHandle};
use crate::source::TextSource;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// How wide a wheel's slot is, in logical pixels.
const SLOT_WIDTH: f32 = 24.0;

/// How tall a wheel's slot is at its natural size, in logical pixels.
const SLOT_HEIGHT: f32 = 112.0;

/// The room a wheel keeps around its slot for the glow of the focus ring
/// and its centre marks.
const ROOM: f32 = 6.0;

/// How long the pitch bend wheel stays where the mouse wheel left it
/// before it springs back to the centre.
pub const BEND_RETURN_AFTER: Duration = Duration::from_millis(300);

/// One step of the arrow keys and of a notch of the mouse wheel, as a
/// fraction of the wheel's whole travel.
const STEP: f32 = 1.0 / 32.0;

/// The same with Shift held.
const FINE_STEP: f32 = 1.0 / 256.0;

/// One step of Page Up and Page Down.
const BIG_STEP: f32 = 0.25;

/// How far the value marker turns from the middle of the slot at either
/// end of the travel, in radians.
const TURN: f32 = 1.0;

/// How far round the wheel the slot shows either way from its middle, in
/// radians.
const SHOWN: f32 = 1.4;

/// The angle between two ridges of the wheel, in radians.
const RIDGE: f32 = 0.15;

/// Which wheel it is: what its values are and what it does when let go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum WheelKind {
    /// The pitch bend wheel: values from -1 to 1, 0 in the middle, where
    /// it springs back when it is let go.
    PitchBend,
    /// The modulation wheel: values from 0 to 1, staying wherever it is
    /// left.
    Modulation,
}

impl WheelKind {
    /// The lowest value.
    pub fn min(self) -> f32 {
        match self {
            WheelKind::PitchBend => -1.0,
            WheelKind::Modulation => 0.0,
        }
    }

    /// The highest value: 1 for both.
    pub fn max(self) -> f32 {
        1.0
    }

    /// Whether it springs back to the middle when let go.
    pub fn springs_back(self) -> bool {
        self == WheelKind::PitchBend
    }

    /// `value` as a position from 0 to 1 along the travel.
    fn position(self, value: f32) -> f32 {
        (value - self.min()) / (self.max() - self.min())
    }

    /// The value at `position`, from 0 to 1 along the travel.
    fn value(self, position: f32) -> f32 {
        self.min() + position.clamp(0.0, 1.0) * (self.max() - self.min())
    }

    /// `value` within the wheel's range; not a number counts as rest.
    fn clamp(self, value: f32) -> f32 {
        if value.is_nan() {
            0.0
        } else {
            value.clamp(self.min(), self.max())
        }
    }
}

struct WheelShared {
    value: AtomicF32,
    /// Counts every change, the application's and the person's.
    version: AtomicU64,
    /// Counts the person's changes.
    moved: AtomicU64,
    /// The person's changes counted at the last `take`.
    taken: AtomicU64,
    held: AtomicBool,
}

/// What a wheel shares with the application when it is not bound to a
/// parameter: its value, which the application polls to send as MIDI
/// pitch bend or a controller (CC 1, say), and sets from MIDI it
/// receives.
///
/// Cloning shares the value. It is `Send` and `Sync`, so a plugin's
/// editor spec can keep it, and the value is an atomic: the audio thread
/// can read it with [`value`](WheelState::value), which neither waits nor
/// allocates. Nothing calls the application back.
///
/// ```
/// use manguebyte_ui::WheelState;
///
/// let bend = WheelState::new();
/// assert_eq!(bend.take(), None, "nothing moved yet");
/// // The application shows a bend it received; it is not reported back.
/// bend.set(0.5);
/// assert_eq!(bend.value(), 0.5);
/// assert_eq!(bend.take(), None);
/// assert_eq!(bend.midi_pitch_bend(), 8192 + 4096);
/// ```
#[derive(Clone)]
pub struct WheelState {
    shared: Arc<WheelShared>,
}

impl Default for WheelState {
    fn default() -> Self {
        WheelState::new()
    }
}

impl WheelState {
    /// A wheel at rest: 0.
    pub fn new() -> WheelState {
        WheelState {
            shared: Arc::new(WheelShared {
                value: AtomicF32::new(0.0),
                version: AtomicU64::new(0),
                moved: AtomicU64::new(0),
                taken: AtomicU64::new(0),
                held: AtomicBool::new(false),
            }),
        }
    }

    /// The value now: from -1 to 1 for a pitch bend wheel, 0 to 1 for a
    /// modulation wheel. Real-time safe.
    pub fn value(&self) -> f32 {
        self.shared.value.load(Ordering::Relaxed)
    }

    /// Sets the value, as the application does from MIDI it receives:
    /// the wheel shows it on its next frame, unless a person holds it.
    /// [`take`](WheelState::take) does not report it.
    pub fn set(&self, value: f32) {
        let value = if value.is_nan() {
            0.0
        } else {
            value.clamp(-1.0, 1.0)
        };
        self.shared.value.store(value, Ordering::Relaxed);
        self.shared.version.fetch_add(1, Ordering::AcqRel);
    }

    /// The value a person moved the wheel to, by any means, as the wheel
    /// sprang back included, if it changed since the last call: what the
    /// application sends.
    pub fn take(&self) -> Option<f32> {
        let moved = self.shared.moved.load(Ordering::Acquire);
        let taken = self.shared.taken.swap(moved, Ordering::AcqRel);
        (taken != moved).then(|| self.value())
    }

    /// A number that changes with every change of the value.
    pub fn version(&self) -> u64 {
        self.shared.version.load(Ordering::Acquire)
    }

    /// Whether a person holds the wheel now: dragging it, holding a key
    /// on it, or the pitch bend wheel springing back.
    pub fn is_held(&self) -> bool {
        self.shared.held.load(Ordering::Relaxed)
    }

    /// The value as a MIDI 1.0 pitch bend: 0 to 16 383, 8 192 in the
    /// middle.
    pub fn midi_pitch_bend(&self) -> u16 {
        let value = self.value().clamp(-1.0, 1.0);
        let offset = if value >= 0.0 {
            (value * 8191.0).round()
        } else {
            (value * 8192.0).round()
        };
        (8192.0 + offset) as u16
    }

    /// The value, from 0 to 1, as a MIDI 1.0 controller value: 0 to 127.
    pub fn midi_controller(&self) -> u8 {
        (self.value().clamp(0.0, 1.0) * 127.0).round() as u8
    }

    /// A person moved the wheel to `value`.
    fn store_moved(&self, value: f32) {
        self.shared.value.store(value, Ordering::Relaxed);
        self.shared.version.fetch_add(1, Ordering::AcqRel);
        self.shared.moved.fetch_add(1, Ordering::AcqRel);
    }

    fn set_held(&self, held: bool) {
        self.shared.held.store(held, Ordering::Relaxed);
    }
}

impl fmt::Debug for WheelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WheelState")
            .field("value", &self.value())
            .field("held", &self.is_held())
            .finish()
    }
}

/// What a wheel changes: a parameter, through gestures as a
/// [`knob`](crate::knob) does, or a [`WheelState`] the application polls.
pub enum WheelBinding {
    /// A parameter. A pitch bend wheel's parameter is at rest in the
    /// middle of its range.
    Param(ParamHandle),
    /// A value the application polls.
    State(WheelState),
}

impl From<ParamHandle> for WheelBinding {
    fn from(param: ParamHandle) -> Self {
        WheelBinding::Param(param)
    }
}

impl From<WheelState> for WheelBinding {
    fn from(state: WheelState) -> Self {
        WheelBinding::State(state)
    }
}

impl fmt::Debug for WheelBinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WheelBinding::Param(param) => f.debug_tuple("Param").field(param).finish(),
            WheelBinding::State(state) => f.debug_tuple("State").field(state).finish(),
        }
    }
}

enum Target {
    Param(ParamBinding),
    /// The state, and its version as of the last value the wheel wrote or
    /// read.
    State(WheelState, u64),
}

/// A drag in progress.
#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Where the pointer was, up and down, when the value was `start`.
    origin: f32,
    start: f32,
    fine: bool,
}

/// A wheel: a ridged wheel in a recessed slot that turns up and down,
/// with its value marked in the accent and its name under it.
///
/// - Dragging up raises it, the slot's height a whole travel (ten times
///   as far with Shift held).
/// - The mouse wheel and the arrow keys (Up and Right raise it) move it a
///   thirty-second of its travel a step (a two hundred and fifty-sixth
///   with Shift); Page Up and Page Down a quarter; Home and End to its
///   ends.
/// - A double click puts it back at rest: the middle for pitch bend, the
///   parameter's default or 0 for modulation.
/// - The pitch bend wheel springs back to the middle when it is let go:
///   at the end of a drag, when the key that moved it comes up, when the
///   keyboard focus leaves it, and [`BEND_RETURN_AFTER`] after the mouse
///   wheel last turned it. It moves back on the theme's spring, and jumps
///   with motion turned off; the value it sends follows it back. The
///   modulation wheel stays where it is left.
///
/// Bound to a parameter, a drag, a held key and the spring back are one
/// gesture; a step of the mouse wheel or a key on the modulation wheel is
/// an edit of its own. Bound to a [`WheelState`], it writes the value
/// there for the application to [`take`](WheelState::take).
///
/// To assistive technology it is a [`Role::Slider`] named by its name,
/// with its value as text and as a number, that can be raised, lowered
/// and set; set this way, the pitch bend wheel stays where it is set,
/// since a screen reader cannot hold it.
pub struct Wheel {
    kind: WheelKind,
    name: TextSource,
    target: Target,
    /// Where the wheel is, and is drawn, in its own values.
    position: Animated<f32>,
    /// The value last given to the target.
    sent: f32,
    drag: Option<Drag>,
    /// The key holding the pitch bend wheel away from the middle.
    held_key: Option<Key>,
    /// When the mouse wheel last turned the pitch bend wheel, while it
    /// waits to spring back.
    scrolled: Option<Duration>,
    /// A person holds it: the target has a gesture open, or is told so.
    holding: bool,
    /// Springing back to the middle.
    returning: bool,
    label: Cached<TextStyle>,
}

/// A pitch bend wheel called `name`, changing `binding`: a
/// [`ParamHandle`] or a [`WheelState`]. It springs back to the middle
/// when let go.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, WheelState, pitch_wheel};
///
/// let bend = WheelState::new();
/// let mut ui = Harness::new(pitch_wheel("Pitch", bend.clone()), Size::new(60.0, 160.0), 1.0);
/// ui.frame();
/// let wheel = ui.get(Role::Slider, "Pitch");
/// // Dragged up: bent up, as the application reads it.
/// ui.press(wheel.center());
/// ui.move_to(wheel.center() - manguebyte_gfx::Point::new(0.0, 30.0));
/// assert!(bend.take().is_some_and(|value| value > 0.4));
/// assert!(bend.is_held());
/// // Let go, it springs back to the middle (at once, with motion off).
/// ui.release(wheel.center());
/// ui.frame();
/// assert_eq!(bend.take(), Some(0.0));
/// assert!(!bend.is_held());
/// ```
pub fn pitch_wheel(name: impl Into<TextSource>, binding: impl Into<WheelBinding>) -> Wheel {
    Wheel::new(WheelKind::PitchBend, name.into(), binding.into())
}

/// A modulation wheel called `name`, changing `binding`: a
/// [`ParamHandle`] or a [`WheelState`]. It stays where it is left.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, WheelState, mod_wheel};
///
/// let modulation = WheelState::new();
/// let mut ui = Harness::new(mod_wheel("Mod", modulation.clone()), Size::new(60.0, 160.0), 1.0);
/// ui.frame();
/// let wheel = ui.get(Role::Slider, "Mod");
/// assert!(ui.act(&wheel, manguebyte_ui::Action::Focus));
/// assert!(ui.key(Key::PageUp));
/// assert_eq!(modulation.take(), Some(0.25));
/// assert_eq!(modulation.midi_controller(), 32);
/// ```
pub fn mod_wheel(name: impl Into<TextSource>, binding: impl Into<WheelBinding>) -> Wheel {
    Wheel::new(WheelKind::Modulation, name.into(), binding.into())
}

impl Wheel {
    fn new(kind: WheelKind, name: TextSource, binding: WheelBinding) -> Wheel {
        let (target, value) = match binding {
            WheelBinding::Param(param) => {
                let binding = ParamBinding::new(param);
                let value = kind.value(binding.shown() as f32);
                (Target::Param(binding), value)
            }
            WheelBinding::State(state) => {
                let value = kind.clamp(state.value());
                let version = state.version();
                (Target::State(state, version), value)
            }
        };
        Wheel {
            kind,
            name,
            target,
            position: Animated::new(value),
            sent: value,
            drag: None,
            held_key: None,
            scrolled: None,
            holding: false,
            returning: false,
            label: Cached::new(),
        }
    }

    /// Which wheel it is.
    pub fn kind(&self) -> WheelKind {
        self.kind
    }

    /// The value shown now.
    pub fn value(&self) -> f32 {
        self.position.value()
    }

    /// Takes hold of the wheel, for a drag, a held key or a turn of the
    /// mouse wheel on the pitch bend wheel: a gesture opens on the
    /// parameter.
    fn hold(&mut self, cx: &mut EventCx<'_>) {
        self.returning = false;
        if self.holding {
            return;
        }
        self.holding = true;
        match &mut self.target {
            Target::Param(binding) => binding.begin(cx.context()),
            Target::State(state, _) => state.set_held(true),
        }
    }

    /// Lets go of the wheel: the gesture ends, and the application is told
    /// it is no longer held.
    fn finish(&mut self) {
        self.returning = false;
        if !self.holding {
            return;
        }
        self.holding = false;
        match &mut self.target {
            Target::Param(binding) => binding.end(),
            Target::State(state, _) => state.set_held(false),
        }
    }

    /// Gives the target the value shown, if it differs from the last one
    /// given: within the open gesture, or into the state.
    fn send(&mut self) {
        let value = self.position.value();
        if value.to_bits() == self.sent.to_bits() {
            return;
        }
        self.sent = value;
        match &mut self.target {
            Target::Param(binding) => binding.set(f64::from(self.kind.position(value))),
            Target::State(state, seen) => {
                state.store_moved(value);
                *seen = state.version();
            }
        }
    }

    /// Moves the wheel to `value` at once while it is held.
    fn move_to(&mut self, cx: &mut EventCx<'_>, value: f32) {
        self.position.jump(self.kind.clamp(value));
        self.send();
        cx.request_paint();
    }

    /// Moves the wheel to `value` as one edit of its own, not held: a key
    /// or the mouse wheel on the modulation wheel, assistive technology,
    /// a double click.
    fn set_once(&mut self, cx: &mut EventCx<'_>, value: f32) {
        let value = self.kind.clamp(value);
        self.position.jump(value);
        self.sent = value;
        match &mut self.target {
            Target::Param(binding) => {
                binding.set_once(cx.context(), f64::from(self.kind.position(value)));
            }
            Target::State(state, seen) => {
                state.store_moved(value);
                *seen = state.version();
            }
        }
        cx.request_paint();
    }

    /// Lets go of the wheel: the pitch bend wheel springs back to the
    /// middle, holding its gesture until it gets there; the modulation
    /// wheel stays.
    fn let_go(&mut self, now: Duration, motion: crate::motion::Motion) {
        self.drag = None;
        self.held_key = None;
        self.scrolled = None;
        if self.kind.springs_back() && self.holding {
            self.returning = true;
            self.position.transition(0.0, now, motion.springy());
            self.follow();
        } else {
            self.finish();
        }
    }

    /// Sends where the wheel springing back has got to, and lets go once
    /// it is there.
    fn follow(&mut self) {
        self.send();
        if self.returning && !self.position.is_running() {
            self.finish();
        }
    }

    /// The value at rest: the middle for pitch bend, the parameter's
    /// default or 0 for modulation.
    fn rest(&self) -> f32 {
        match (&self.target, self.kind) {
            (_, WheelKind::PitchBend) => 0.0,
            (Target::Param(binding), kind) => {
                kind.value(binding.handle().default_normalized() as f32)
            }
            (Target::State(..), _) => 0.0,
        }
    }

    /// One step of the travel in the wheel's values.
    fn step(&self, fraction: f32) -> f32 {
        fraction * (self.kind.max() - self.kind.min())
    }

    fn key(&mut self, cx: &mut EventCx<'_>, key: Key, fine: bool) -> Handled {
        let step = self.step(if fine { FINE_STEP } else { STEP });
        let value = self.position.value();
        let wanted = match key {
            Key::Up | Key::Right => value + step,
            Key::Down | Key::Left => value - step,
            Key::PageUp => value + self.step(BIG_STEP),
            Key::PageDown => value - self.step(BIG_STEP),
            Key::Home => self.kind.min(),
            Key::End => self.kind.max(),
            _ => return Handled::No,
        };
        if self.kind.springs_back() {
            // Held while the key is down; let go when it comes up.
            self.scrolled = None;
            self.hold(cx);
            self.held_key = Some(key);
            self.move_to(cx, wanted);
        } else {
            self.set_once(cx, wanted);
        }
        Handled::Yes
    }

    /// The text of the value shown.
    fn value_text(&self) -> String {
        match &self.target {
            Target::Param(binding) => binding.text(),
            Target::State(..) => {
                let value = self.position.value();
                match self.kind {
                    WheelKind::PitchBend if value.abs() < 0.005 => "0.00".to_owned(),
                    WheelKind::PitchBend => format!("{value:+.2}"),
                    WheelKind::Modulation => format!("{:.0} %", value * 100.0),
                }
            }
        }
    }
}

/// The room around the slot: for the glow of the focus ring in a theme
/// that glows.
fn room(theme: &Theme) -> f32 {
    if theme.elevation.glow.color.a == 0 {
        ROOM * 0.5
    } else {
        ROOM
    }
}

/// Where the slot is in `rect`, its edges on device pixels at `scale`.
fn slot(rect: Rect, theme: &Theme, scale: f32) -> Rect {
    let pad = room(theme);
    let name = line_height(&name_style(theme));
    let width = SLOT_WIDTH.min(rect.width - 2.0 * (MARGIN + pad)).max(0.0);
    let left = snap_px(rect.x + (rect.width - width) * 0.5, scale);
    let top = snap_px(rect.y + MARGIN + pad, scale);
    let bottom = snap_px(rect.bottom() - MARGIN - name - GAP - pad, scale);
    let right = snap_px(left + width, scale);
    Rect::new(left, top, (right - left).max(0.0), (bottom - top).max(0.0))
}

impl Widget for Wheel {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let (value, range) = match &self.target {
            Target::Param(binding) => {
                let handle = binding.handle();
                (
                    binding.plain(),
                    AccessRange::new(handle.plain(0.0), handle.plain(1.0), None),
                )
            }
            Target::State(..) => (
                f64::from(self.position.value()),
                AccessRange::new(f64::from(self.kind.min()), f64::from(self.kind.max()), None),
            ),
        };
        AccessNode::new(Role::Slider, self.name.as_str())
            .with_value(self.value_text())
            .with_numeric(value, range)
            .with_actions(Actions::ADJUST)
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        let style = name_style(theme);
        let pad = room(theme);
        let width = (SLOT_WIDTH + 2.0 * pad).max(text_width(self.name.as_str(), &style));
        Size::new(
            width + 2.0 * MARGIN,
            2.0 * (MARGIN + pad) + SLOT_HEIGHT + GAP + line_height(&style),
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.name.poll() {
            // The name is drawn, and the width may follow it.
            self.label.clear();
            cx.request_layout();
            cx.request_access_update();
        }
        let free = !self.holding && self.drag.is_none();
        match &mut self.target {
            Target::Param(binding) => {
                // Host automation, or an echo of what was sent.
                if binding.poll(cx.now()) && free {
                    let value = self.kind.value(binding.shown() as f32);
                    if value.to_bits() != self.position.value().to_bits() {
                        self.position.jump(value);
                        self.sent = value;
                        cx.request_paint();
                    }
                }
            }
            Target::State(state, seen) => {
                let version = state.version();
                if version != *seen {
                    *seen = version;
                    if free {
                        let value = self.kind.clamp(state.value());
                        self.position.jump(value);
                        self.sent = value;
                        cx.request_paint();
                    }
                }
            }
        }
        if let Some(at) = self.scrolled {
            if cx.now().saturating_sub(at) >= BEND_RETURN_AFTER {
                self.let_go(cx.now(), cx.motion());
                cx.request_paint();
            } else {
                cx.request_frame();
            }
        }
        if self.returning {
            cx.animate(&mut self.position);
            self.follow();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let rect = cx.rect();
        let scale = cx.scale();
        let slot = slot(rect, theme, scale);
        let radius = theme.corners.md.min(slot.width * 0.5).max(0.0);
        let shape = RoundedRect::new(slot, radius);
        if slot.height > 0.0 && slot.width > 0.0 {
            // The recess, darker than what is around it and darkest at
            // the top, where its rim shades it, with a lit lip below.
            let back = palette.background;
            let dark = u32::from(back.r) + u32::from(back.g) + u32::from(back.b) < 384;
            let (top, bottom) = if dark { (0.45, 0.12) } else { (0.10, 0.03) };
            vertical(
                scene,
                shape,
                (slot.y, slot.bottom()),
                back.lerp(Color::BLACK, top),
                back.lerp(Color::BLACK, bottom),
            );
            super::hairline(scene, slot, radius, scale, palette.hairline_strong);
            hline(
                scene,
                slot.bottom(),
                (slot.x + radius, slot.right() - radius),
                1.0,
                scale,
                palette.highlight,
            );
            self.wheel(cx, scene, (slot, radius), dark);
            if self.kind == WheelKind::PitchBend {
                // Marks either side of the middle, where it rests.
                let middle = slot.y + slot.height * 0.5;
                let mark = (room(theme) - 1.0).max(1.0);
                for (from, to) in [
                    (slot.x - mark - 1.0, slot.x - 1.0),
                    (slot.right() + 1.0, slot.right() + 1.0 + mark),
                ] {
                    hline(scene, middle, (from, to), 1.0, scale, palette.text_muted);
                }
            }
            focus_ring(
                scene,
                RoundedRect::new(
                    slot.outset(theme.sizes.focus_gap),
                    radius + theme.sizes.focus_gap,
                ),
                theme,
                cx.focus_ring(),
            );
        }
        let style = name_style(theme);
        let width = (rect.width - 2.0 * MARGIN).max(0.0);
        let name = self.name.as_str();
        let layout = self.label.get(width, style, || line(name, &style, width));
        let y = rect.bottom() - MARGIN - line_height(&style);
        draw_text(scene, layout, Point::new(rect.x + MARGIN, y));
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } => {
                cx.request_focus();
                if clicks >= 2 {
                    // A double click puts it back at rest.
                    self.let_go(cx.now(), cx.motion());
                    self.finish();
                    cx.release_pointer();
                    let rest = self.rest();
                    self.set_once(cx, rest);
                    return Handled::Yes;
                }
                self.scrolled = None;
                self.hold(cx);
                self.drag = Some(Drag {
                    origin: at.y,
                    start: self.position.value(),
                    fine: modifiers.shift,
                });
                cx.capture_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerMove { at, modifiers } => {
                let Some(drag) = &mut self.drag else {
                    return Handled::No;
                };
                if modifiers.shift != drag.fine {
                    // Changing speed starts again from here.
                    *drag = Drag {
                        origin: at.y,
                        start: self.position.value(),
                        fine: modifiers.shift,
                    };
                }
                let drag = *drag;
                let travel = slot(cx.rect(), cx.theme(), cx.scale()).height.max(1.0)
                    * if drag.fine {
                        cx.theme().sizes.fine_factor.max(1.0)
                    } else {
                        1.0
                    };
                let moved = (drag.origin - at.y) / travel;
                if moved.is_finite() {
                    let value = drag.start + self.step(moved);
                    self.move_to(cx, value);
                }
                Handled::Yes
            }
            Event::PointerUp { .. } | Event::PointerCancel => {
                if self.drag.is_none() {
                    return Handled::No;
                }
                cx.release_pointer();
                self.let_go(cx.now(), cx.motion());
                cx.request_paint();
                Handled::Yes
            }
            Event::Wheel { delta, .. } => {
                let notches = delta.y;
                if !(notches.is_finite() && notches != 0.0) {
                    return Handled::No;
                }
                let value = self.position.value() + self.step(STEP) * notches;
                if self.drag.is_some() || self.held_key.is_some() {
                    self.move_to(cx, value);
                } else if self.kind.springs_back() {
                    self.hold(cx);
                    self.scrolled = Some(cx.now());
                    self.move_to(cx, value);
                } else {
                    self.set_once(cx, value);
                }
                Handled::Yes
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed,
            }) => {
                if !pressed {
                    if self.held_key == Some(key) {
                        self.let_go(cx.now(), cx.motion());
                        cx.request_paint();
                        return Handled::Yes;
                    }
                    return Handled::No;
                }
                if let Some(drag) = self.drag {
                    if key != Key::Escape {
                        return Handled::No;
                    }
                    // Escape puts back where the drag began, and lets go.
                    self.move_to(cx, drag.start);
                    cx.release_pointer();
                    self.let_go(cx.now(), cx.motion());
                    return Handled::Yes;
                }
                if !cx.is_focused() || modifiers.alt || modifiers.control || modifiers.meta {
                    return Handled::No;
                }
                self.key(cx, key, modifiers.shift)
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        let value = self.position.value();
        let wanted = match action {
            Action::Increment => value + self.step(STEP),
            Action::Decrement => value - self.step(STEP),
            Action::SetValue(plain) if plain.is_finite() => match &self.target {
                Target::Param(binding) => self
                    .kind
                    .value(binding.handle().to_normalized(plain) as f32),
                Target::State(..) => plain as f32,
            },
            _ => return false,
        };
        self.let_go(cx.now(), cx.motion());
        self.finish();
        self.set_once(cx, wanted);
        true
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        if !focused && self.held_key.is_some() {
            self.let_go(cx.now(), cx.motion());
            cx.request_paint();
        }
        cx.request_paint();
    }
}

impl Wheel {
    /// Draws the wheel seen through `slot`: a cylinder, light in the
    /// middle and dark where it turns away, with ridges that move as it
    /// turns, the stretch from rest to the value tinted, and the value
    /// marked by a glowing bar in the accent.
    /// `dark` says whether the theme is a dark one, where the ridges catch
    /// less light and cast deeper shade.
    fn wheel(&self, cx: &PaintCx<'_>, scene: &mut Scene, (slot, radius): (Rect, f32), dark: bool) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let scale = cx.scale();
        let inset = 3.0;
        let face = Rect::new(
            slot.x + inset,
            slot.y + inset,
            (slot.width - 2.0 * inset).max(0.0),
            (slot.height - 2.0 * inset).max(0.0),
        );
        if face.height <= 0.0 || face.width <= 0.0 {
            return;
        }
        let shape = RoundedRect::new(face, (radius - inset).max(0.0));
        let (edge, ridge_light, ridge_shade) = if dark {
            (0.25, 0.10, 0.45)
        } else {
            (0.10, 0.85, 0.10)
        };
        let light = palette
            .face_light
            .lerp(palette.text_secondary, 0.12 * cx.hover());
        let stops = [
            Stop::new(0.0, palette.face_dark.lerp(Color::BLACK, edge)),
            Stop::new(0.42, light),
            Stop::new(1.0, palette.face_dark.lerp(Color::BLACK, edge)),
        ];
        scene.fill(
            shape,
            LinearGradient {
                start: Point::new(face.x, face.y),
                end: Point::new(face.x, face.bottom()),
                stops: &stops,
                extend: Extend::Pad,
            },
        );
        let middle = face.y + face.height * 0.5;
        // The radius of the wheel, so that the slot shows `SHOWN` either
        // way.
        let wheel_radius = face.height * 0.5 / SHOWN.sin();
        let y_of = |angle: f32| middle - wheel_radius * angle.sin();
        let value = self.position.value();
        let turned = (self.kind.position(value) - 0.5) * 2.0 * TURN;
        let marker = y_of(turned);
        let rest = match self.kind {
            WheelKind::PitchBend => middle,
            WheelKind::Modulation => y_of(-TURN),
        };
        let accent = palette.accent;
        let px = device_px(1.0, scale);
        scene.clipped(shape, |scene| {
            // The ridges, turning with the wheel.
            let first = ((-SHOWN - turned) / RIDGE).floor() as i32;
            let last = ((SHOWN - turned) / RIDGE).ceil() as i32;
            for ridge in first..=last {
                let angle = turned + ridge as f32 * RIDGE;
                if angle.abs() >= SHOWN {
                    continue;
                }
                let facing = angle.cos();
                let y = y_of(angle);
                let lit = Color::WHITE.with_opacity(ridge_light * facing);
                let shade = Color::BLACK.with_opacity(ridge_shade * facing);
                hline(scene, y, (face.x, face.right()), 1.0, scale, lit);
                hline(scene, y + px, (face.x, face.right()), 1.0, scale, shade);
            }
            // From rest to the value, tinted.
            let (top, bottom) = (marker.min(rest), marker.max(rest));
            if bottom - top > 0.0 {
                // Brightest at the value, fading towards rest.
                let (near, far) = (accent.with_opacity(0.34), accent.with_opacity(0.06));
                let (from, to) = if marker < rest {
                    (near, far)
                } else {
                    (far, near)
                };
                vertical(
                    scene,
                    Rect::new(face.x, top, face.width, bottom - top),
                    (top, bottom),
                    from,
                    to,
                );
            }
            // The value: a bar across the wheel, thinner as it turns away,
            // glowing.
            let thick = (4.0 * turned.cos()).max(1.5);
            let bar = Rect::new(face.x, marker - thick * 0.5, face.width, thick);
            glow_fill(scene, bar, accent, theme, 1.0);
            vertical(
                scene,
                bar,
                (bar.y, bar.bottom()),
                palette.accent_bright,
                accent,
            );
        });
        // The wheel's rim against the slot.
        scene.stroke(
            RoundedRect::new(face, (radius - inset).max(0.0)),
            Stroke::new(px),
            Color::BLACK.with_opacity(0.25),
        );
    }
}

impl fmt::Debug for Wheel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wheel")
            .field("kind", &self.kind)
            .field("name", &self.name.as_str())
            .field("value", &self.position.value())
            .field("holding", &self.holding)
            .finish_non_exhaustive()
    }
}
