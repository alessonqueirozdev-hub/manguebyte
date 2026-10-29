//! What knobs and sliders share: how a person changes a parameter with the
//! pointer, the wheel, the keys, a typed value and assistive technology.

use manguebyte_gfx::{Align, Point, Rect, Scene, TextStyle};

use super::text_edit::{Outcome, TextEdit};
use super::text_field::field_box;
use super::value_style;
use crate::access::{AccessNode, AccessRange, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent, Modifiers};
use crate::param::{ParamBinding, ParamHandle};
use crate::theme::Theme;
use crate::widget::EventCx;

/// The most characters a typed value may have.
const ENTRY_CHARS: usize = 64;

/// Space between the border of the box a value is typed in and its text.
const ENTRY_PAD: f32 = 3.0;

/// A value being typed.
struct Entry {
    edit: TextEdit,
    /// Enter was pressed on text that reads as no value.
    invalid: bool,
    /// The value's text when typing began.
    shown: String,
}

/// The style a value is typed in: the value's, in the text colour, from
/// the left.
fn entry_style(theme: &Theme) -> TextStyle {
    value_style(theme)
        .with_color(theme.palette.text)
        .with_align(Align::Start)
}

/// Whether a press with `modifiers` puts the default back: Alt, Control
/// or Command held.
fn resets(modifiers: Modifiers) -> bool {
    modifiers.alt || modifiers.control || modifiers.meta
}

/// Which way the pointer moves to raise the value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    /// Up.
    Vertical,
    /// To the right.
    Horizontal,
}

/// A drag in progress.
#[derive(Debug, Clone, Copy)]
struct Drag {
    /// Where the pointer was when the value was `start`.
    origin: Point,
    start: f64,
    /// Whether Shift was held, for fine changes.
    fine: bool,
}

/// A parameter and the drag or the typed value changing it.
pub(crate) struct Adjust {
    pub(crate) binding: ParamBinding,
    axis: Axis,
    drag: Option<Drag>,
    entry: Option<Entry>,
}

impl Adjust {
    pub(crate) fn new(handle: ParamHandle, axis: Axis) -> Self {
        Adjust {
            binding: ParamBinding::new(handle),
            axis,
            drag: None,
            entry: None,
        }
    }

    /// Starts typing a value, from the value shown, all of it selected.
    fn open_entry(&mut self, cx: &mut EventCx<'_>) {
        let mut edit = TextEdit::new(ENTRY_CHARS);
        edit.set_text(&self.binding.text());
        let shown = edit.text().to_owned();
        self.entry = Some(Entry {
            edit,
            invalid: false,
            shown,
        });
        cx.request_focus();
        cx.request_paint();
    }

    /// Takes the value typed, as one edit, if it reads as a value. Returns
    /// whether it did; if not, the entry stays open and shows it is wrong.
    fn commit_entry(&mut self, cx: &mut EventCx<'_>) -> bool {
        let Some(entry) = &mut self.entry else {
            return true;
        };
        cx.request_paint();
        if entry.edit.text() == entry.shown {
            // Left as it was: nothing to send. The text is the value shown
            // rounded to the parameter's precision, and reading it back
            // could move the value.
            self.entry = None;
            return true;
        }
        match self.binding.handle().parse(entry.edit.text()) {
            Some(value) => {
                self.entry = None;
                self.binding.set_once(cx.context(), value);
                true
            }
            None => {
                entry.invalid = true;
                false
            }
        }
    }

    /// Ends typing because the person went elsewhere: what was typed is
    /// taken if it reads as a value, and thrown away if not.
    fn finish_entry(&mut self, cx: &mut EventCx<'_>) {
        if !self.commit_entry(cx) {
            self.entry = None;
        }
    }

    /// Handles `event` while a value is being typed into `entry_box`.
    /// Returns `None` for a press or a wheel turn elsewhere, which ended
    /// the typing and goes on to be handled as usual.
    fn entry_event(
        &mut self,
        cx: &mut EventCx<'_>,
        event: &Event,
        entry_box: Rect,
    ) -> Option<Handled> {
        let style = entry_style(cx.theme());
        let entry = self.entry.as_mut()?;
        let handled = match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } if entry_box.contains(at) => {
                entry.edit.press(at, clicks, modifiers.shift, &style);
                cx.capture_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerDown { .. } | Event::Wheel { .. } => {
                self.finish_entry(cx);
                return None;
            }
            Event::PointerMove { at, .. } if cx.has_pointer() => {
                entry.edit.drag(at, &style);
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { .. } | Event::PointerCancel if cx.has_pointer() => {
                entry.edit.release();
                cx.release_pointer();
                Handled::Yes
            }
            Event::Key(key) => match entry.edit.key(key, cx.clipboard()) {
                Outcome::Ignored => Handled::No,
                Outcome::Used => {
                    entry.invalid = false;
                    cx.request_paint();
                    Handled::Yes
                }
                Outcome::Commit => {
                    self.commit_entry(cx);
                    Handled::Yes
                }
                Outcome::Cancel => {
                    self.entry = None;
                    cx.request_paint();
                    Handled::Yes
                }
            },
            Event::Text(c) => {
                entry.edit.type_char(c);
                entry.invalid = false;
                cx.request_paint();
                Handled::Yes
            }
            Event::Composition(step) => {
                entry.edit.compose(step);
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        };
        Some(handled)
    }

    /// The control lost the keyboard focus: typing ends, taking what was
    /// typed if it reads as a value.
    pub(crate) fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        if !focused && self.entry.is_some() {
            self.finish_entry(cx);
        }
    }

    /// Draws the value being typed into `entry_box`, if one is. Returns
    /// whether it did.
    pub(crate) fn paint_entry(
        &mut self,
        scene: &mut Scene,
        entry_box: Rect,
        theme: &Theme,
        focused: bool,
    ) -> bool {
        let Some(entry) = &mut self.entry else {
            return false;
        };
        let style = entry_style(theme);
        field_box(scene, entry_box, theme, entry.invalid);
        let inner = Rect::new(
            entry_box.x + ENTRY_PAD,
            entry_box.y,
            (entry_box.width - 2.0 * ENTRY_PAD).max(0.0),
            entry_box.height,
        );
        entry.edit.paint(scene, inner, &style, theme, focused);
        true
    }

    /// Whether a value is being typed.
    pub(crate) fn is_typing(&self) -> bool {
        self.entry.is_some()
    }

    /// Where the caret of the value being typed is, if one is.
    pub(crate) fn caret(&self) -> Option<Rect> {
        self.entry.as_ref()?.edit.caret_rect()
    }

    /// One step, as the arrow keys and one notch of the wheel take it: one
    /// of the parameter's steps, or a hundredth of the range.
    fn step(&self, fine: bool) -> f64 {
        match self.binding.handle().step_count() {
            Some(steps) if steps > 0 => 1.0 / f64::from(steps),
            _ if fine => 0.001,
            _ => 0.01,
        }
    }

    /// A large step, as Page Up and Page Down take it: a tenth of the
    /// range, or one step if that is larger.
    fn big_step(&self) -> f64 {
        self.step(false).max(0.1)
    }

    /// Handles `event`. `span` is how far, in logical pixels, the pointer
    /// moves to cross the whole range; `fine_factor` how many times
    /// further it moves while Shift is held; `entry_box` where a value is
    /// typed.
    ///
    /// Every edit that moves on from the value shown takes it from
    /// [`ParamBinding::current`], read at that moment: host automation may
    /// have moved the parameter since the last frame, and a value sent a
    /// moment ago may not have been echoed yet.
    pub(crate) fn event(
        &mut self,
        cx: &mut EventCx<'_>,
        event: &Event,
        span: f32,
        fine_factor: f32,
        entry_box: Rect,
    ) -> Handled {
        if let Some(handled) = self.entry_event(cx, event, entry_box) {
            return handled;
        }
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } => {
                if resets(modifiers) {
                    // Alt, Control or Command and a click put the default
                    // back, as one edit.
                    self.drag = None;
                    self.binding.end();
                    let default = self.binding.handle().default_normalized();
                    self.binding.set_once(cx.context(), default);
                    cx.release_pointer();
                } else if clicks >= 2 {
                    // A double click opens the value to be typed. The first
                    // click was a gesture of its own, already ended.
                    self.drag = None;
                    self.binding.end();
                    cx.release_pointer();
                    self.open_entry(cx);
                } else {
                    let start = self.binding.current();
                    self.drag = Some(Drag {
                        origin: at,
                        start,
                        fine: modifiers.shift,
                    });
                    self.binding.begin(cx.context());
                    cx.capture_pointer();
                }
                // A control pressed takes the keyboard focus, so the arrow
                // keys go on from where the pointer left it.
                cx.request_focus();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerMove { at, modifiers } => {
                let Some(drag) = &mut self.drag else {
                    return Handled::No;
                };
                if modifiers.shift != drag.fine {
                    // Changing speed starts again from here, so the value
                    // does not jump.
                    *drag = Drag {
                        origin: at,
                        start: self.binding.shown(),
                        fine: modifiers.shift,
                    };
                }
                let travel = match self.axis {
                    Axis::Vertical => drag.origin.y - at.y,
                    Axis::Horizontal => at.x - drag.origin.x,
                };
                let factor = if drag.fine && fine_factor.is_finite() {
                    fine_factor.max(1.0)
                } else {
                    1.0
                };
                let span = f64::from(span.max(1.0)) * f64::from(factor);
                let value = drag.start + f64::from(travel) / span;
                // A position that is not a number gives a value that is
                // not one, which the gesture ignores.
                self.binding.set(value);
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { .. } | Event::PointerCancel => {
                if self.drag.take().is_none() {
                    return Handled::No;
                }
                self.binding.end();
                cx.release_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::Wheel { delta, .. } => {
                let notches = match self.axis {
                    Axis::Vertical => delta.y,
                    Axis::Horizontal => delta.y + delta.x,
                };
                if !(notches.is_finite() && notches != 0.0) {
                    return Handled::No;
                }
                let value = self.binding.current() + self.step(false) * f64::from(notches);
                if self.drag.is_some() {
                    self.binding.set(value);
                } else {
                    self.binding.set_once(cx.context(), value);
                }
                cx.request_paint();
                Handled::Yes
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed: true,
            }) => {
                if self.drag.is_some() {
                    if key != Key::Escape {
                        return Handled::No;
                    }
                    // Escape during a drag puts back the value it began at.
                    self.drag = None;
                    self.binding.cancel();
                    cx.release_pointer();
                    cx.request_paint();
                    return Handled::Yes;
                }
                if key == Key::Enter {
                    // Enter opens the value to be typed, as a double click
                    // does.
                    self.open_entry(cx);
                    return Handled::Yes;
                }
                let step = self.step(modifiers.shift);
                let value = match key {
                    Key::Up | Key::Right => self.binding.current() + step,
                    Key::Down | Key::Left => self.binding.current() - step,
                    Key::PageUp => self.binding.current() + self.big_step(),
                    Key::PageDown => self.binding.current() - self.big_step(),
                    Key::Home => 0.0,
                    Key::End => 1.0,
                    Key::Delete | Key::Backspace => self.binding.handle().default_normalized(),
                    _ => return Handled::No,
                };
                self.binding.set_once(cx.context(), value);
                cx.request_paint();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    /// Carries out an action from assistive technology.
    pub(crate) fn act(&mut self, cx: &mut EventCx<'_>, action: Action) -> bool {
        let value = match action {
            Action::Increment => self.binding.current() + self.step(false),
            Action::Decrement => self.binding.current() - self.step(false),
            Action::SetValue(plain) if plain.is_finite() => {
                self.binding.handle().to_normalized(plain)
            }
            Action::SetValue(_) | Action::Focus | Action::Press => return false,
        };
        self.binding.set_once(cx.context(), value);
        cx.request_paint();
        true
    }

    /// The description of a control with this value: while a value is
    /// typed into it, a text input with the text typed so far.
    pub(crate) fn access(&self, role: Role, name: &str) -> AccessNode {
        if let Some(entry) = &self.entry {
            return AccessNode::new(Role::TextInput, name)
                .with_value(entry.edit.text())
                .with_actions(Actions {
                    focus: true,
                    ..Actions::NONE
                })
                .editable();
        }
        let handle = self.binding.handle();
        let (min, max) = (handle.plain(0.0), handle.plain(1.0));
        let step = handle
            .step_count()
            .filter(|&steps| steps > 0)
            .map(|steps| (max - min) / f64::from(steps));
        AccessNode::new(role, name)
            .with_value(self.binding.text())
            .with_numeric(self.binding.plain(), AccessRange::new(min, max, step))
            .with_actions(Actions::ADJUST)
    }
}
