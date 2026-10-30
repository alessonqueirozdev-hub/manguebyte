//! A pill: a small round-ended button, as a screen's title bar holds.

use manguebyte_gfx::{
    Align, Circle, Color, Point, Rect, RoundedRect, Scene, Shadow, Size, TextStyle,
};

use super::{Cached, draw_text, drop_shadow, focus_ring, glow_fill, hairline, line, line_height};
use super::{text_width, vertical};
use crate::access::{AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::motion::Lerp;
use crate::source::State;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, Widget};

/// Space around the pill inside its rectangle, for the focus ring.
const ROOM: f32 = 3.0;

/// The pill's height.
const HEIGHT: f32 = 30.0;

/// Space between the ends and the text, and between a dot and the text.
const PAD: f32 = 12.0;

/// The diameter of a status dot.
const DOT: f32 = 7.0;

/// A pill: a label on a round-ended button, with an optional status dot,
/// plain or primary.
///
/// A click, or Space or Enter with the keyboard focus, presses it: the
/// [`State`] given to [`presses`](Pill::presses) counts up by one, and the
/// application polls it (ADR 0003: nothing is called back). Like a switch,
/// a click does not take the keyboard focus. Under the pointer a plain
/// pill brightens and rises a pixel.
///
/// A primary pill is the screen's main action: the accent's gradient with
/// a glow under it, or ink on paper in a theme without glow.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Role, State, pill};
///
/// let presses = State::new(0_u64);
/// let view = pill("Export plugin").primary().presses(presses.clone());
/// let mut ui = Harness::new(view, Size::new(160.0, 40.0), 1.0);
/// ui.frame();
/// let export = ui.get(Role::Button, "Export plugin");
/// ui.click(export.center());
/// assert_eq!(presses.get(), 1);
/// ```
pub struct Pill {
    text: String,
    dot: Option<Color>,
    primary: bool,
    presses: Option<State<u64>>,
    opens: Option<State<bool>>,
    pressed: bool,
    layout: Cached<TextStyle>,
}

/// A pill showing `text`, which is also its name for assistive technology.
pub fn pill(text: impl Into<String>) -> Pill {
    Pill {
        text: text.into(),
        dot: None,
        primary: false,
        presses: None,
        opens: None,
        pressed: false,
        layout: Cached::new(),
    }
}

impl Pill {
    /// The same pill with a status dot of `color` before its text, glowing
    /// in a theme that glows.
    #[must_use]
    pub fn dot(mut self, color: Color) -> Pill {
        self.dot = Some(color);
        self
    }

    /// The same pill as the screen's main action.
    #[must_use]
    pub fn primary(mut self) -> Pill {
        self.primary = true;
        self
    }

    /// The same pill counting its presses in `count`.
    #[must_use]
    pub fn presses(mut self, count: State<u64>) -> Pill {
        self.presses = Some(count);
        self
    }

    /// The same pill setting `open` to `true` when pressed, as well as
    /// counting its presses if it does: what opens a
    /// [`floating_panel`](crate::floating_panel), or brings it to the
    /// front with the focus inside if it is open.
    #[must_use]
    pub fn opens(mut self, open: &State<bool>) -> Pill {
        self.opens = Some(open.clone());
        self
    }

    fn activate(&self) {
        if let Some(count) = &self.presses {
            count.update(|n| *n = n.wrapping_add(1));
        }
        if let Some(open) = &self.opens {
            open.set(true);
        }
    }

    fn style(&self, theme: &Theme) -> TextStyle {
        theme.typography.label.with_align(Align::Start)
    }

    /// The primary pill's look in `theme`: its colours from top to bottom,
    /// its text's colour, and its shadow.
    pub(crate) fn primary_look(theme: &Theme) -> (Color, Color, Color, Shadow) {
        let palette = &theme.palette;
        if theme.elevation.glow.color.a > 0 {
            let shadow = Shadow::new(palette.accent.with_alpha(0xCC))
                .with_offset(0.0, 6.0)
                .with_blur(20.0)
                .with_spread(-6.0);
            (
                palette.accent_bright,
                palette.accent,
                palette.on_accent,
                shadow,
            )
        } else {
            let shadow = Shadow::new(Color::BLACK.with_alpha(0x80))
                .with_offset(0.0, 6.0)
                .with_blur(16.0)
                .with_spread(-8.0);
            (palette.text, palette.text, palette.raised, shadow)
        }
    }
}

impl Widget for Pill {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Button, self.text.as_str()).with_actions(Actions {
            press: true,
            focus: true,
            ..Actions::NONE
        })
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = self.style(cx.theme());
        let dot = if self.dot.is_some() { DOT + 8.0 } else { 0.0 };
        Size::new(
            2.0 * (ROOM + PAD) + dot + text_width(&self.text, &style),
            2.0 * ROOM + HEIGHT.max(line_height(&style) + 8.0),
        )
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        let shadow = if self.primary {
            Pill::primary_look(cx.theme()).3
        } else {
            cx.theme().elevation.sm
        };
        if shadow.color.a == 0 {
            return 0.0;
        }
        let offset = shadow.offset.x.abs().max(shadow.offset.y.abs());
        (shadow.reach_exact() + offset + 1.0 - ROOM).max(0.0).ceil()
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let palette = &theme.palette;
        let rect = cx.rect();
        let hover = cx.hover();
        // Up a pixel under the pointer, back down while pressed.
        let lift = hover * (1.0 - cx.press());
        let body = Rect::new(
            rect.x + ROOM,
            rect.y + ROOM - lift,
            (rect.width - 2.0 * ROOM).max(0.0),
            (rect.height - 2.0 * ROOM).max(0.0),
        );
        let radius = body.height * 0.5;
        let shape = RoundedRect::new(body, radius);
        let ink = if self.primary {
            let (top, bottom, ink, shadow) = Pill::primary_look(theme);
            drop_shadow(scene, shape, shadow);
            vertical(scene, shape, (body.y, body.bottom()), top, bottom);
            ink
        } else {
            let face = palette.raised.lerp(palette.overlay, hover);
            scene.fill(shape, face);
            hairline(scene, body, radius, cx.scale(), palette.hairline);
            palette.text_secondary.lerp(palette.text, hover)
        };
        let style = self.style(theme).with_color(ink);
        let mut x = body.x + PAD;
        let middle = body.y + body.height * 0.5;
        if let Some(color) = self.dot {
            let dot = Circle::new(Point::new(x + DOT * 0.5, middle), DOT * 0.5);
            glow_fill(scene, dot, color, theme, 1.0);
            scene.fill(dot, color);
            x += DOT + 8.0;
        }
        let room = (body.right() - PAD - x).max(0.0);
        let text = &self.text;
        let layout = self.layout.get(room, style, || line(text, &style, room));
        draw_text(
            scene,
            layout,
            Point::new(x, middle - line_height(&style) * 0.5),
        );
        focus_ring(
            scene,
            RoundedRect::new(body.outset(2.0), radius + 2.0),
            theme,
            cx.focus_ring(),
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                button: Button::Primary,
                ..
            } => {
                self.pressed = true;
                cx.capture_pointer();
                Handled::Yes
            }
            Event::PointerUp {
                at,
                button: Button::Primary,
                ..
            } => {
                if std::mem::take(&mut self.pressed) && cx.rect().contains(at) {
                    self.activate();
                }
                cx.release_pointer();
                Handled::Yes
            }
            Event::PointerCancel => {
                self.pressed = false;
                Handled::No
            }
            Event::Key(KeyEvent {
                key: Key::Character(' ') | Key::Enter,
                pressed: true,
                ..
            }) => {
                self.activate();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, _cx: &mut EventCx<'_>, action: Action) -> bool {
        if action != Action::Press {
            return false;
        }
        self.activate();
        true
    }
}
