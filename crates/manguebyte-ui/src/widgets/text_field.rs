//! A single line of text a person types.

use std::rc::Rc;

use manguebyte_gfx::{Align, Point, Rect, RoundedRect, Scene, Size, Stroke, TextStyle};

use super::text_edit::{DEFAULT_MAX_CHARS, Outcome, TextEdit};
use super::{Cached, MARGIN, line, line_height};
use crate::access::{AccessNode, Actions, Role};
use crate::event::{Button, Event, Handled};
use crate::motion::Lerp;
use crate::source::State;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// Space between a field's border and its text, across.
pub(crate) const PAD_X: f32 = 4.0;

/// Space between a field's border and its text, up and down.
pub(crate) const PAD_Y: f32 = 3.0;

/// Whether a text may be taken.
type Validate = Rc<dyn Fn(&str) -> bool>;

/// One line of text a person types, kept in a [`State`].
///
/// The field edits a copy of the state's text and puts it back into the
/// state when the edit is taken: on Enter, and when the field loses the
/// keyboard focus (a press elsewhere, Tab, the window losing the
/// keyboard). Escape throws the edit away and shows the state's text
/// again; Escape with nothing changed is left to the host. A
/// [`validate`](TextField::validate) hook can refuse a text: Enter then
/// keeps the edit open and the field's border turns to the theme's warning
/// colour, and losing the focus throws the edit away. While no edit is
/// open, the field shows whatever the state holds, read once per frame.
///
/// - The pointer: a press places the caret, Shift extends the selection,
///   a drag selects, a double click selects a word, a triple click
///   everything. Tab and assistive technology give the focus with
///   everything selected.
/// - Keys: the arrows move by character, with Control (Option on macOS)
///   by word, and with Shift they select; Home and End (and Up and Down)
///   go to the ends; Backspace and Delete remove a character, or with
///   Control a word; Control+A selects everything, Control+C, Control+X
///   and Control+V copy, cut and paste (Command on macOS; Control+Insert,
///   Shift+Delete and Shift+Insert too). Characters arrive as
///   [`Event::Text`], in the person's layout. Keys that type, move or
///   delete are kept from the host while the field has the focus, so a
///   host's shortcut on a letter or the space bar does not fire; function
///   keys, Tab and Control with keys the field does not use go on.
/// - Text a person cannot see does not go in: line breaks and tabs become
///   spaces, other control characters are dropped. At most
///   [`max_chars`](TextField::max_chars) characters (256 unless set) are
///   kept, and a [`filter`](TextField::filter) hook can refuse characters.
/// - An input method's composition is shown at the caret, underlined,
///   until it produces text (see [`Composition`](crate::Composition)).
///
/// For assistive technology it is a [`TextInput`](Role::TextInput) whose
/// value is its text and whose state is editable. The caret does not
/// blink.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Role, State, text_field};
///
/// let name = State::new(String::from("Init"));
/// let view = text_field("Preset name", name.clone()).placeholder("Untitled");
/// let mut ui = Harness::new(view, Size::new(160.0, 30.0), 1.0);
/// ui.frame();
/// let field = ui.get(Role::TextInput, "Preset name");
/// assert_eq!(field.value_text(), Some("Init"));
///
/// // A click places the caret at the end; typing goes there.
/// ui.click(field.bounds.center() + manguebyte_gfx::Point::new(70.0, 0.0));
/// ui.type_text(" bass");
/// assert_eq!(ui.get(Role::TextInput, "Preset name").value_text(), Some("Init bass"));
/// assert_eq!(name.get(), "Init", "not taken yet");
/// assert!(ui.key(Key::Enter));
/// assert_eq!(name.get(), "Init bass");
/// ```
pub fn text_field(name: impl Into<String>, text: State<String>) -> TextField {
    TextField::new(name, text)
}

/// The widget [`text_field`] makes.
pub struct TextField {
    name: String,
    state: State<String>,
    seen: Option<u64>,
    edit: TextEdit,
    placeholder: Option<String>,
    validate: Option<Validate>,
    /// The text was changed since it was last taken or thrown away.
    dirty: bool,
    /// Enter was refused by the validation.
    invalid: bool,
    /// The focus came with a press, which placed the caret itself.
    pressed: bool,
    hint: Cached<TextStyle>,
}

impl TextField {
    /// A field named `name` for assistive technology and tests, editing
    /// the text in `text`.
    pub fn new(name: impl Into<String>, text: State<String>) -> TextField {
        TextField {
            name: name.into(),
            state: text,
            seen: None,
            edit: TextEdit::new(DEFAULT_MAX_CHARS),
            placeholder: None,
            validate: None,
            dirty: false,
            invalid: false,
            pressed: false,
            hint: Cached::new(),
        }
    }

    /// The same field showing `text`, muted, while it is empty.
    #[must_use]
    pub fn placeholder(mut self, text: impl Into<String>) -> TextField {
        self.placeholder = Some(text.into());
        self
    }

    /// The same field taking only characters `accept` says yes to, typed
    /// or pasted; the rest are dropped.
    ///
    /// ```
    /// use manguebyte_gfx::Size;
    /// use manguebyte_ui::testing::Harness;
    /// use manguebyte_ui::{Role, State, text_field};
    ///
    /// let tempo = State::new(String::from("120"));
    /// let view = text_field("Tempo", tempo.clone())
    ///     .filter(|c| c.is_ascii_digit() || c == '.')
    ///     .validate(|text| text.parse::<f64>().is_ok_and(|bpm| (20.0..=999.0).contains(&bpm)));
    /// let mut ui = Harness::new(view, Size::new(100.0, 30.0), 1.0);
    /// ui.frame();
    /// assert!(ui.tab(), "the focus selects everything");
    /// ui.type_text("9x5,.5");
    /// assert_eq!(ui.get(Role::TextInput, "Tempo").value_text(), Some("95.5"));
    /// ```
    #[must_use]
    pub fn filter(mut self, accept: impl Fn(char) -> bool + 'static) -> TextField {
        self.edit.set_filter(Some(Rc::new(accept)));
        self
    }

    /// The same field taking a text only if `valid` says yes to it: Enter
    /// on a text it refuses keeps the edit open and shows the field as
    /// wrong.
    #[must_use]
    pub fn validate(mut self, valid: impl Fn(&str) -> bool + 'static) -> TextField {
        self.validate = Some(Rc::new(valid));
        self
    }

    /// The same field holding at most `max` characters. Text from the
    /// state that is longer is cut.
    #[must_use]
    pub fn max_chars(mut self, max: usize) -> TextField {
        self.edit.set_max_chars(max);
        self
    }

    /// Whether the text being edited may be taken.
    fn acceptable(&self) -> bool {
        self.validate
            .as_ref()
            .is_none_or(|valid| valid(self.edit.text()))
    }

    /// Puts the edit into the state, if it may be taken. Returns whether it
    /// was.
    fn commit(&mut self) -> bool {
        if !self.dirty {
            self.invalid = false;
            return true;
        }
        if !self.acceptable() {
            self.invalid = true;
            return false;
        }
        let text = self.edit.text().to_owned();
        if self.state.with(|held| *held != text) {
            self.state.set(text);
        }
        self.seen = Some(self.state.version());
        self.dirty = false;
        self.invalid = false;
        true
    }

    /// Throws the edit away and shows the state's text again.
    fn revert(&mut self) {
        self.state.with(|text| self.edit.set_text(text));
        self.seen = Some(self.state.version());
        self.dirty = false;
        self.invalid = false;
    }

    /// The edit's rectangle inside the field's, and the text's inside that.
    fn boxes(rect: Rect, style: &TextStyle) -> (Rect, Rect) {
        let outer = Rect::new(
            rect.x + MARGIN,
            rect.y + MARGIN,
            (rect.width - 2.0 * MARGIN).max(0.0),
            (rect.height - 2.0 * MARGIN).max(0.0),
        );
        let height = line_height(style).min(outer.height);
        let inner = Rect::new(
            outer.x + PAD_X,
            outer.y + (outer.height - height) * 0.5,
            (outer.width - 2.0 * PAD_X).max(0.0),
            height,
        );
        (outer, inner)
    }

    fn after_edit(&mut self, cx: &mut EventCx<'_>, before: &str) {
        if self.edit.text() != before {
            self.dirty = true;
            self.invalid = false;
        }
        cx.request_paint();
    }
}

/// The style a field's text is drawn in.
pub(crate) fn field_style(theme: &Theme) -> TextStyle {
    theme
        .typography
        .label
        .with_color(theme.palette.text)
        .with_align(Align::Start)
        .with_max_lines(1)
}

/// Draws a field's box: the background inside a border, in the warning
/// colour if `invalid`.
///
/// `active`, from 0 to 1, turns the border to the focus colour and
/// thickens it: 1 while text is being typed, the focus ring's amount
/// otherwise; `hover` lightens the face.
pub(crate) fn field_box(
    scene: &mut Scene,
    outer: Rect,
    theme: &Theme,
    invalid: bool,
    (active, hover): (f32, f32),
) {
    let palette = &theme.palette;
    let radius = theme.corners.md.min(outer.height * 0.5);
    let face = palette.raised.lerp(palette.overlay, hover.clamp(0.0, 1.0));
    scene.fill(RoundedRect::new(outer, radius), face);
    let active = active.clamp(0.0, 1.0);
    let border = if invalid {
        palette.danger
    } else {
        palette.hairline_strong.lerp(palette.focus, active)
    };
    let width = 1.0 + 0.5 * active;
    let edge = Rect::new(
        outer.x + width * 0.5,
        outer.y + width * 0.5,
        (outer.width - width).max(0.0),
        (outer.height - width).max(0.0),
    );
    scene.stroke(
        RoundedRect::new(edge, (radius - width * 0.5).max(0.0)),
        Stroke::new(width),
        border,
    );
}

impl Widget for TextField {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::TextInput, self.name.as_str())
            .with_value(self.edit.text())
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
            .editable()
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        let style = field_style(theme);
        Size::new(
            theme.sizes.slider_width + 2.0 * MARGIN,
            line_height(&style) + 2.0 * (PAD_Y + MARGIN),
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.state.version();
        if self.seen != Some(version) && !self.dirty {
            self.seen = Some(version);
            self.state.with(|text| self.edit.set_text(text));
            self.edit.caret_to_end();
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let style = field_style(theme);
        let (outer, inner) = TextField::boxes(rect, &style);
        let typing = if cx.is_focused() { 1.0 } else { 0.0 };
        field_box(
            scene,
            outer,
            theme,
            self.invalid,
            (typing, cx.hover() * 0.5),
        );
        if self.edit.text().is_empty() && !self.edit.is_composing() {
            if let Some(placeholder) = &self.placeholder {
                let muted = style.with_color(theme.palette.text_secondary);
                let hint = self.hint.get(inner.width, muted, || {
                    line(placeholder, &muted, inner.width)
                });
                scene.text(hint, Point::new(inner.x, inner.y));
            }
        }
        self.edit
            .paint(scene, inner, &style, theme, cx.is_focused());
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        let style = field_style(cx.theme());
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                clicks,
                modifiers,
            } => {
                if !cx.is_focused() {
                    self.pressed = true;
                }
                self.edit.press(at, clicks, modifiers.shift, &style);
                cx.request_focus();
                cx.capture_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerMove { at, .. } if cx.has_pointer() => {
                self.edit.drag(at, &style);
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { .. } | Event::PointerCancel if cx.has_pointer() => {
                self.edit.release();
                cx.release_pointer();
                Handled::Yes
            }
            Event::Key(key) if cx.is_focused() => {
                let before = self.edit.text().to_owned();
                match self.edit.key(key, cx.clipboard()) {
                    Outcome::Ignored => return Handled::No,
                    Outcome::Used => self.after_edit(cx, &before),
                    Outcome::Commit => {
                        self.commit();
                        cx.request_paint();
                    }
                    Outcome::Cancel => {
                        if !self.dirty {
                            // Nothing to throw away: Escape is the host's.
                            return Handled::No;
                        }
                        self.revert();
                        cx.request_paint();
                    }
                }
                Handled::Yes
            }
            Event::Text(c) if cx.is_focused() => {
                let before = self.edit.text().to_owned();
                self.edit.type_char(c);
                self.after_edit(cx, &before);
                Handled::Yes
            }
            Event::Composition(step) if cx.is_focused() => {
                let before = self.edit.text().to_owned();
                self.edit.compose(step);
                self.after_edit(cx, &before);
                Handled::Yes
            }
            _ => Handled::No,
        }
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        if focused {
            if !std::mem::take(&mut self.pressed) {
                self.edit.select_all();
            }
        } else {
            self.pressed = false;
            if !self.commit() {
                self.revert();
            }
            self.edit.release();
        }
        cx.request_paint();
    }

    fn caret(&self, _cx: &AccessCx<'_>) -> Option<Rect> {
        self.edit.caret_rect()
    }
}
