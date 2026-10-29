//! A control that picks one of a parameter's values from a menu.

use manguebyte_gfx::{Align, Path, Point, Rect, Scene, Size, Stroke, TextStyle};

use super::text_field::{PAD_X, PAD_Y, field_box};
use super::{Cached, GAP, MARGIN, line, line_height, name_style, text_width};
use crate::access::{AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent, MenuEvent};
use crate::menu::{Menu, MenuItem};
use crate::param::{ParamBinding, ParamHandle};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// The most values a combo box lists. A parameter with more steps, or none,
/// lists nothing, and its combo box opens no menu: a knob or a slider suits
/// it better.
pub const MAX_CHOICES: u32 = 128;

/// The room on the right of the box for its arrow.
const ARROW: f32 = 16.0;

/// A combo box: the parameter's name, and a box showing its value, which
/// opens a menu of every value the parameter can take.
///
/// It is for parameters with a few named values: a
/// [`ChoiceParam`](manguebyte_params::ChoiceParam), a switch, an integer
/// with a short range (at most [`MAX_CHOICES`] values). Each value is
/// listed in the parameter's own words, the current one checked.
///
/// - A press, or Enter, Space, F4 or Alt+Down with the keyboard focus
///   (which a press gives it), opens the menu below the box, or above it
///   where there is no room below (see [`crate::menu`]). Choosing a value
///   sends it as one edit: begin, the value, end. Closing the menu with
///   nothing chosen sends nothing.
/// - With the focus and the menu closed, Up and Down choose the previous
///   and next value, Home and End the first and the last, and the wheel
///   steps through them, each one edit.
///
/// For assistive technology it is a [`ComboBox`](Role::ComboBox) whose
/// value is the value's text and which says whether its menu is open;
/// [`Action::Press`] opens the menu.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gfx::Size;
/// use manguebyte_params::{ChoiceParam, param_set};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{ParamHandle, Role, combo};
///
/// struct Params { shape: ChoiceParam }
/// param_set!(Params { shape });
/// let params = Arc::new(Params {
///     shape: ChoiceParam::new("shape", "Shape", &["Sine", "Saw", "Square"], 0),
/// });
/// let view = combo(ParamHandle::new(Arc::clone(&params), |p| &p.shape));
/// let mut ui = Harness::with_params(view, Size::new(160.0, 160.0), 1.0, params.clone());
/// ui.frame();
/// let shape = ui.get(Role::ComboBox, "Shape");
/// assert_eq!(shape.value_text(), Some("Sine"));
///
/// ui.click(shape.center());
/// ui.frame();
/// assert_eq!(ui.get(Role::ComboBox, "Shape").node.state.expanded, Some(true));
/// // The current value is checked, and has the focus.
/// let sine = ui.get(Role::MenuItem, "Sine");
/// assert!(sine.node.state.checked == Some(true) && sine.is_focused());
/// ui.click(ui.get(Role::MenuItem, "Square").center());
/// assert_eq!(params.shape.index(), 2);
/// ui.frame();
/// assert!(ui.find(Role::Menu, "Shape").is_none(), "the menu closed");
/// ```
pub fn combo(param: ParamHandle) -> ComboBox {
    ComboBox::new(param)
}

/// The widget [`combo`] makes.
pub struct ComboBox {
    binding: ParamBinding,
    label: Option<String>,
    /// Whether its menu is open.
    open: bool,
    name: Cached<TextStyle>,
    /// Made for the value shown, as bits, whether it is a value sent, and
    /// the style.
    value: Cached<(u64, bool, TextStyle)>,
}

impl ComboBox {
    /// A combo box for `param`, named by the parameter.
    pub fn new(param: ParamHandle) -> ComboBox {
        ComboBox {
            binding: ParamBinding::new(param),
            label: None,
            open: false,
            name: Cached::new(),
            value: Cached::new(),
        }
    }

    /// The same combo box showing `text` as its name instead of the
    /// parameter's name.
    #[must_use]
    pub fn label(mut self, text: impl Into<String>) -> ComboBox {
        self.label = Some(text.into());
        self
    }

    fn name_text(&self) -> &str {
        match &self.label {
            Some(label) => label,
            None => self.binding.handle().name(),
        }
    }

    /// How many steps the parameter has, if it is one a combo box lists.
    fn steps(&self) -> Option<u32> {
        self.binding
            .handle()
            .step_count()
            .filter(|&steps| steps > 0 && steps < MAX_CHOICES)
    }

    /// The step the value shown is on.
    fn current(&mut self, steps: u32) -> u32 {
        let value = self.binding.current();
        ((value * f64::from(steps)).round().max(0.0) as u32).min(steps)
    }

    fn open_menu(&mut self, cx: &mut EventCx<'_>) {
        let Some(steps) = self.steps() else {
            return;
        };
        let current = self.current(steps);
        let handle = self.binding.handle();
        let mut menu = Menu::new(self.name_text());
        for step in 0..=steps {
            let text = handle.text(f64::from(step) / f64::from(steps));
            menu.push(MenuItem::new(step as usize, text).checked(step == current));
        }
        self.open = true;
        cx.open_menu(menu);
        cx.request_paint();
    }

    /// Sends step `step` as one edit.
    fn choose(&mut self, cx: &mut EventCx<'_>, step: u32) {
        if let Some(steps) = self.steps() {
            let step = step.min(steps);
            self.binding
                .set_once(cx.context(), f64::from(step) / f64::from(steps));
            cx.request_paint();
        }
    }

    /// Moves `by` steps from the value shown, as one edit.
    fn step(&mut self, cx: &mut EventCx<'_>, by: i64) {
        if let Some(steps) = self.steps() {
            let current = i64::from(self.current(steps));
            let next = (current + by).clamp(0, i64::from(steps));
            if next != current {
                self.choose(cx, next as u32);
            }
        }
    }
}

/// The box's rectangle inside the combo box's.
fn value_box(rect: Rect, theme: &Theme) -> Rect {
    let name = line_height(&name_style(theme));
    let top = rect.y + MARGIN + name + GAP;
    Rect::new(
        rect.x + MARGIN,
        top,
        (rect.width - 2.0 * MARGIN).max(0.0),
        (rect.bottom() - MARGIN - top).max(0.0),
    )
}

/// The style of the value in the box.
fn box_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .label
        .with_color(theme.palette.text)
        .with_align(Align::Start)
        .with_max_lines(1)
}

impl Widget for ComboBox {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::ComboBox, self.name_text())
            .with_value(self.binding.text())
            .with_expanded(self.open)
            .with_actions(Actions::PRESS)
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        let name = name_style(theme);
        let value = box_style(theme);
        let handle = self.binding.handle();
        let widest = match self.steps() {
            Some(steps) => (0..=steps)
                .map(|step| text_width(&handle.text(f64::from(step) / f64::from(steps)), &value))
                .fold(0.0f32, f32::max),
            None => text_width(&self.binding.text(), &value),
        };
        let width = text_width(self.name_text(), &name).max(widest + 2.0 * PAD_X + ARROW);
        Size::new(
            width + 2.0 * MARGIN,
            2.0 * MARGIN + line_height(&name) + GAP + line_height(&value) + 2.0 * PAD_Y,
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        if self.binding.poll(cx.now()) {
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let width = (rect.width - 2.0 * MARGIN).max(0.0);
        let name_style = name_style(theme);
        let label = self.label.as_deref();
        let binding = &self.binding;
        let name = self.name.get(width, name_style, || {
            line(
                label.unwrap_or_else(|| binding.handle().name()),
                &name_style,
                width,
            )
        });
        scene.text(name, Point::new(rect.x + MARGIN, rect.y + MARGIN));

        let outer = value_box(rect, theme);
        let open = if self.open { 1.0 } else { 0.0 };
        field_box(
            scene,
            outer,
            theme,
            false,
            (cx.focus_ring().max(open), cx.hover().max(open)),
        );
        let style = box_style(theme);
        let room = (outer.width - 2.0 * PAD_X - ARROW).max(0.0);
        let key = (binding.shown().to_bits(), binding.shows_sent(), style);
        let value = self
            .value
            .get(room, key, || line(&binding.text(), &style, room));
        let y = outer.y + (outer.height - line_height(&style)) * 0.5;
        scene.text(value, Point::new(outer.x + PAD_X, y));
        // A chevron pointing down.
        let x = outer.right() - ARROW * 0.5 - 2.0;
        let middle = outer.y + outer.height * 0.5;
        let mut arrow = Path::new();
        arrow
            .move_to(Point::new(x - 4.0, middle - 2.0))
            .line_to(Point::new(x, middle + 2.0))
            .line_to(Point::new(x + 4.0, middle - 2.0));
        scene.stroke(&arrow, Stroke::round(1.5), theme.palette.text_secondary);
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                button: Button::Primary,
                ..
            } => {
                cx.request_focus();
                self.open_menu(cx);
                Handled::Yes
            }
            Event::Wheel { delta, .. } => {
                let notches = delta.y + delta.x;
                if !(notches.is_finite() && notches != 0.0) {
                    return Handled::No;
                }
                // Up the wheel, up the list: to the value before.
                self.step(cx, if notches > 0.0 { -1 } else { 1 });
                Handled::Yes
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed: true,
            }) => {
                match key {
                    Key::Enter | Key::Character(' ') | Key::Function(4) => self.open_menu(cx),
                    Key::Down if modifiers.alt => self.open_menu(cx),
                    Key::Up | Key::Left => self.step(cx, -1),
                    Key::Down | Key::Right => self.step(cx, 1),
                    Key::Home => self.step(cx, -i64::from(MAX_CHOICES)),
                    Key::End => self.step(cx, i64::from(MAX_CHOICES)),
                    _ => return Handled::No,
                }
                Handled::Yes
            }
            Event::Menu(result) => {
                self.open = false;
                if let MenuEvent::Chosen(step) = result {
                    self.choose(cx, u32::try_from(step).unwrap_or(u32::MAX));
                }
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        if action != Action::Press || self.steps().is_none() {
            return false;
        }
        self.open_menu(cx);
        true
    }
}
