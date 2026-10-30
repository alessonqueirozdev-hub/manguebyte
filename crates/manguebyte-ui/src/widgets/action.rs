//! Action buttons: primary, secondary and destructive.

use std::cell::Cell;
use std::rc::Rc;

use manguebyte_gfx::{
    Align, Color, Layer, Point, Rect, RoundedRect, Scene, Shadow, Size, TextStyle,
};

use super::icon::Icon;
use super::pill::Pill;
use super::{
    Cached, draw_text, drop_shadow, focus_ring, hairline, line, line_height, text_width,
    top_highlight, vertical,
};
use crate::access::{AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::motion::Lerp;
use crate::source::{Source, State, TextSource, Watch};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// Space around the button inside its rectangle, for the focus ring.
const ROOM: f32 = 3.0;

/// The button's height.
const HEIGHT: f32 = 32.0;

/// Space between the button's ends and what it shows.
const PAD: f32 = 14.0;

/// The side of an icon, and the space after it.
const ICON: f32 = 16.0;
const ICON_GAP: f32 = 7.0;

/// How opaque a disabled button is drawn.
const DISABLED: f32 = 0.45;

/// What a button's look says about what it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The main action of a screen or a dialog.
    Primary,
    /// Any other action.
    Secondary,
    /// An action that destroys something and cannot be undone.
    Destructive,
}

/// A button that does one thing when pressed: a label from a
/// [`TextSource`], an optional [`Icon`] before it, and one of three looks
/// from the theme's tokens.
///
/// - [`primary`](ActionButton::primary): the screen's main action, the
///   accent's gradient with its glow under it, or ink on paper in a theme
///   without glow;
/// - secondary, the default: the raised surface of controls, with a
///   hairline and the light along its top;
/// - [`destructive`](ActionButton::destructive): the theme's danger
///   colour, for what deletes or cannot be undone.
///
/// A click, Space or Enter with the keyboard focus, or assistive
/// technology's press, presses it: the [`State`] given to
/// [`presses`](ActionButton::presses) counts up by one, and the
/// application polls it (ADR 0003: nothing is called back). A press that
/// is let go outside the button does nothing. Under the pointer it
/// brightens and rises a pixel, held down it settles and darkens, and with
/// the focus shown it has a ring: the root's eased hover, press and focus
/// amounts, so it moves as the other widgets do, and jumps with motion
/// off. Unlike a [`Pill`], a click gives it the keyboard focus, as buttons
/// in dialogs and forms take it.
///
/// A [`disabled`](ActionButton::disabled) button, or one whose
/// [`enabled`](ActionButton::enabled) source reads `false`, is drawn
/// faded, cannot be pressed or focused (Tab passes it), and says so to
/// assistive technology.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, State, action_button, row};
///
/// let exports = State::new(0_u64);
/// let view = row((
///     action_button("Cancel"),
///     action_button("Export").primary().presses(exports.clone()),
/// ));
/// let mut ui = Harness::new(view, Size::new(260.0, 50.0), 1.0);
/// ui.frame();
/// ui.click(ui.get(Role::Button, "Export").center());
/// assert_eq!(exports.get(), 1);
/// // A click focused it; Enter presses it again.
/// assert!(ui.get(Role::Button, "Export").is_focused());
/// ui.key(Key::Enter);
/// assert_eq!(exports.get(), 2);
/// ```
pub struct ActionButton {
    label: TextSource,
    kind: Kind,
    icon: Option<Icon>,
    presses: Option<State<u64>>,
    enabled: Option<Watch<bool>>,
    disabled: bool,
    pressed: bool,
    layout: Cached<TextStyle>,
    /// What else a press does, inside the crate: a dialog's buttons answer
    /// it, and a button can open one.
    pub(crate) hook: Option<Box<dyn Fn()>>,
    /// Set by a dialog that has just opened with this as its default
    /// button: the button takes the focus.
    pub(crate) autofocus: Option<Rc<Cell<bool>>>,
}

/// A secondary button labelled `label`, which is also its name for
/// assistive technology.
pub fn action_button(label: impl Into<TextSource>) -> ActionButton {
    ActionButton {
        label: label.into(),
        kind: Kind::Secondary,
        icon: None,
        presses: None,
        enabled: None,
        disabled: false,
        pressed: false,
        layout: Cached::new(),
        hook: None,
        autofocus: None,
    }
}

impl ActionButton {
    /// The same button as the main action.
    #[must_use]
    pub fn primary(mut self) -> ActionButton {
        self.kind = Kind::Primary;
        self
    }

    /// The same button as an ordinary action: the default.
    #[must_use]
    pub fn secondary(mut self) -> ActionButton {
        self.kind = Kind::Secondary;
        self
    }

    /// The same button as an action that destroys something.
    #[must_use]
    pub fn destructive(mut self) -> ActionButton {
        self.kind = Kind::Destructive;
        self
    }

    /// The same button with `icon` before its label.
    #[must_use]
    pub fn icon(mut self, icon: Icon) -> ActionButton {
        self.icon = Some(icon);
        self
    }

    /// The same button counting its presses in `count`.
    #[must_use]
    pub fn presses(mut self, count: State<u64>) -> ActionButton {
        self.presses = Some(count);
        self
    }

    /// The same button, which cannot be used.
    #[must_use]
    pub fn disabled(mut self) -> ActionButton {
        self.enabled = None;
        self.disabled = true;
        self
    }

    /// The same button, usable while `enabled` reads `true`: read once per
    /// frame.
    #[must_use]
    pub fn enabled(mut self, enabled: impl Source<bool>) -> ActionButton {
        let mut watch = Watch::new(enabled);
        watch.poll();
        self.disabled = watch.get() == Some(&false);
        self.enabled = Some(watch);
        self
    }

    fn activate(&self) {
        if self.disabled {
            return;
        }
        if let Some(count) = &self.presses {
            count.update(|n| *n = n.wrapping_add(1));
        }
        if let Some(hook) = &self.hook {
            hook();
        }
    }

    fn style(theme: &Theme) -> TextStyle {
        theme.typography.label.with_align(Align::Start)
    }

    /// The button's colours in `theme`, `hover` and `press` of the way in:
    /// its face from top to bottom, its text's colour, its shadow.
    fn look(&self, theme: &Theme, hover: f32, press: f32) -> (Color, Color, Color, Shadow) {
        let palette = &theme.palette;
        let shade = |color: Color| color.lerp(Color::BLACK.with_alpha(color.a), 0.12 * press);
        match self.kind {
            Kind::Primary => {
                let (top, bottom, ink, shadow) = Pill::primary_look(theme);
                let lit = if theme.elevation.glow.color.a > 0 {
                    palette.accent_bright
                } else {
                    palette.text_secondary
                };
                (
                    shade(top.lerp(lit, 0.25 * hover)),
                    shade(bottom.lerp(lit, 0.35 * hover)),
                    ink,
                    shadow,
                )
            }
            Kind::Secondary => {
                let face = palette.raised.lerp(palette.overlay, hover * (1.0 - press));
                let ink = palette.text_secondary.lerp(palette.text, hover.max(press));
                (face, face, ink, theme.elevation.sm)
            }
            Kind::Destructive => {
                let lit = palette.danger.lerp(Color::WHITE, 0.12 * hover);
                let shadow = Shadow::new(palette.danger.with_alpha(0x66))
                    .with_offset(0.0, 4.0)
                    .with_blur(12.0)
                    .with_spread(-4.0);
                let shadow = if theme.elevation.glow.color.a > 0 {
                    shadow
                } else {
                    theme.elevation.sm
                };
                (shade(lit), shade(lit), palette.on_accent, shadow)
            }
        }
    }
}

impl Widget for ActionButton {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let node = AccessNode::new(Role::Button, self.label.as_str());
        if self.disabled {
            node.disabled()
        } else {
            node.with_actions(Actions::PRESS)
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let style = ActionButton::style(cx.theme());
        let icon = if self.icon.is_some() {
            ICON + ICON_GAP
        } else {
            0.0
        };
        let text = if self.label.as_str().is_empty() {
            0.0
        } else {
            text_width(self.label.as_str(), &style)
        };
        Size::new(
            2.0 * (ROOM + PAD) + icon + text,
            2.0 * ROOM + HEIGHT.max(line_height(&style) + 10.0),
        )
    }

    fn overflow(&self, cx: &MeasureCx<'_>) -> f32 {
        let shadow = self.look(cx.theme(), 1.0, 0.0).3;
        if shadow.color.a == 0 {
            return 0.0;
        }
        let offset = shadow.offset.x.abs().max(shadow.offset.y.abs());
        (shadow.reach_exact() + offset + 1.0 - ROOM).max(0.0).ceil()
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self
            .autofocus
            .as_ref()
            .is_some_and(|flag| flag.replace(false))
        {
            cx.request_focus();
        }
        if self.label.poll() {
            self.layout.clear();
            cx.request_layout();
            cx.request_access_update();
        }
        if let Some(enabled) = &mut self.enabled {
            if enabled.poll() {
                let disabled = enabled.get() == Some(&false);
                if disabled != self.disabled {
                    self.disabled = disabled;
                    self.pressed = false;
                    cx.request_paint();
                    cx.request_access_update();
                }
            }
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let scale = cx.scale();
        let (hover, press) = if self.disabled {
            (0.0, 0.0)
        } else {
            (cx.hover(), cx.press())
        };
        let (top, bottom, ink, shadow) = self.look(theme, hover, press);
        // Up a pixel under the pointer, back down while pressed.
        let lift = hover * (1.0 - press);
        let body = Rect::new(
            rect.x + ROOM,
            rect.y + ROOM - lift,
            (rect.width - 2.0 * ROOM).max(0.0),
            (rect.height - 2.0 * ROOM).max(0.0),
        );
        let radius = theme.corners.md.min(body.height * 0.5).max(0.0);
        let shape = RoundedRect::new(body, radius);
        let style = ActionButton::style(theme).with_color(ink);
        let draw = |scene: &mut Scene, layout: &manguebyte_gfx::TextLayout| {
            drop_shadow(scene, shape, shadow);
            vertical(scene, shape, (body.y, body.bottom()), top, bottom);
            match self.kind {
                Kind::Secondary => {
                    hairline(scene, body, radius, scale, theme.palette.hairline);
                    top_highlight(scene, body, radius, scale, theme.palette.highlight);
                }
                Kind::Primary | Kind::Destructive => {
                    top_highlight(scene, body, radius, scale, theme.palette.highlight);
                }
            }
            let icon = if self.icon.is_some() {
                ICON + ICON_GAP
            } else {
                0.0
            };
            let content = icon + layout.size().width;
            // Centred, as far as there is room.
            let mut x = body.x + ((body.width - content) * 0.5).max(PAD.min(body.width * 0.5));
            let middle = body.y + body.height * 0.5;
            if let Some(icon) = &self.icon {
                icon.draw(scene, Rect::new(x, middle - ICON * 0.5, ICON, ICON), ink);
                x += ICON + ICON_GAP;
            }
            draw_text(
                scene,
                layout,
                Point::new(x, middle - line_height(&style) * 0.5),
            );
        };
        let icon = if self.icon.is_some() {
            ICON + ICON_GAP
        } else {
            0.0
        };
        let room = (body.width - 2.0 * PAD - icon).max(0.0);
        let text = &self.label;
        let layout = self
            .layout
            .get(room, style, || line(text.as_str(), &style, room));
        if self.disabled {
            scene.layer(Layer::with_opacity(DISABLED), |scene| draw(scene, layout));
        } else {
            draw(scene, layout);
        }
        focus_ring(
            scene,
            RoundedRect::new(body.outset(2.0), radius + 2.0),
            theme,
            cx.focus_ring(),
        );
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        if self.disabled {
            return Handled::No;
        }
        match *event {
            Event::PointerDown {
                button: Button::Primary,
                ..
            } => {
                self.pressed = true;
                cx.capture_pointer();
                cx.request_focus();
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
                modifiers,
                pressed,
            }) if !modifiers.control && !modifiers.alt && !modifiers.meta => {
                if pressed {
                    self.activate();
                }
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, _cx: &mut EventCx<'_>, action: Action) -> bool {
        if action != Action::Press || self.disabled {
            return false;
        }
        self.activate();
        true
    }
}
