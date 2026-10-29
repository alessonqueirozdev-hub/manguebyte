//! Input, in the form a window layer delivers it.
//!
//! Positions are logical pixels from the top-left corner of the interface:
//! a window layer divides the device pixels it receives by the scale.
//! Keys use the editor contract's types, so a key a VST 3 host delivers
//! through the plugin interface and one the window receives itself are the
//! same event.
//!
//! ```
//! use std::sync::Arc;
//! use manguebyte_gfx::{Point, Size};
//! use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
//! use manguebyte_ui::testing::Harness;
//! use manguebyte_ui::{Event, Key, KeyEvent, Modifiers, ParamHandle, Role, knob};
//!
//! struct Params { pan: FloatParam }
//! param_set!(Params { pan });
//! let params = Arc::new(Params { pan: FloatParam::new("pan", "Pan", FloatRange::linear(-1.0, 1.0), 0.0) });
//! let view = knob(ParamHandle::new(Arc::clone(&params), |p| &p.pan));
//! let mut ui = Harness::with_params(view, Size::new(80.0, 100.0), 1.0, params.clone());
//! ui.frame();
//! let pan = ui.get(Role::Slider, "Pan");
//!
//! // One notch of the wheel away from the person raises the value a step.
//! let wheel = Event::Wheel { at: pan.center(), delta: Point::new(0.0, 1.0), modifiers: Modifiers::NONE };
//! assert_eq!(wheel.position(), Some(pan.center()));
//! assert!(ui.send(wheel));
//! assert!((params.pan.value() - 0.02).abs() < 1e-6);
//!
//! // Nothing has the keyboard focus, so a key is left to the host.
//! let end = Event::Key(KeyEvent { key: Key::End, modifiers: Modifiers::NONE, pressed: true });
//! assert!(!ui.send(end));
//! assert!(ui.tab());
//! assert!(ui.send(end));
//! assert_eq!(params.pan.value(), 1.0);
//! ```

use std::path::PathBuf;

use manguebyte_gfx::Point;

pub use manguebyte_plugin::editor::{Key, KeyEvent, Modifiers};

/// A pointer button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Button {
    /// The main button: the left one, or a tap.
    Primary,
    /// The other button: usually the right one.
    Secondary,
    /// The middle button or wheel press.
    Middle,
}

/// Something a person did, as the interface receives it.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// A button went down at `at`. `clicks` counts quick presses in the
    /// same place as the system counts them: 1 for a click, 2 for a double
    /// click.
    PointerDown {
        /// Where, in logical pixels.
        at: Point,
        /// Which button.
        button: Button,
        /// 1 for a click, 2 for a double click, and so on.
        clicks: u32,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// The pointer moved to `at`.
    PointerMove {
        /// Where, in logical pixels.
        at: Point,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// A button came up at `at`.
    PointerUp {
        /// Where, in logical pixels.
        at: Point,
        /// Which button.
        button: Button,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// The window lost the pointer in the middle of a press, as when the
    /// system takes it away: a drag ends where it is.
    PointerCancel,
    /// The pointer left the interface while no button was held: nothing is
    /// hovered any more, and a tooltip goes.
    PointerLeave,
    /// The wheel turned, or a touchpad scrolled, with the pointer at `at`.
    Wheel {
        /// Where the pointer is, in logical pixels.
        at: Point,
        /// How far, in notches (lines): `y` above 0 is away from the
        /// person, which raises a value; `x` above 0 is to the right.
        delta: Point,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// A key went down or came up.
    Key(KeyEvent),
    /// The window gained (`true`) or lost (`false`) the keyboard.
    Focus(bool),
    /// A character was typed: what the key pressed, or the composition an
    /// input method finished, produced in the person's keyboard layout,
    /// shifted, dead keys and AltGr already applied. It follows the
    /// [`Key`](Event::Key) that typed it, if there was one. Control
    /// characters never come this way: Enter, Tab, Backspace and Escape
    /// arrive as keys only.
    ///
    /// A widget that takes text takes it from here, not from
    /// [`Key::Character`], which names the key and not what it typed.
    Text(char),
    /// An input method is composing text that is not typed yet, as one
    /// does for Chinese, Japanese or Korean (see [`Composition`]). What it
    /// finally produces comes as [`Text`](Event::Text), after or before
    /// [`Composition::End`].
    Composition(Composition),
    /// A menu this widget opened with [`EventCx::open_menu`] or
    /// [`EventCx::open_menu_at`] closed: what was chosen in it, if
    /// anything. Only the widget that opened the menu receives this, once
    /// per menu.
    ///
    /// [`EventCx::open_menu`]: crate::EventCx::open_menu
    /// [`EventCx::open_menu_at`]: crate::EventCx::open_menu_at
    Menu(MenuEvent),
}

/// How a menu ended, as the widget that opened it hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MenuEvent {
    /// The item with this id was chosen, by a click, a key or assistive
    /// technology. The menu has closed.
    Chosen(usize),
    /// The menu closed with nothing chosen: Escape, Tab, a press outside
    /// it, another menu opening, or the window losing the keyboard.
    Closed,
}

/// What an input method says while it composes text (Chinese, Japanese,
/// Korean and other scripts typed through an editor of their own).
///
/// A composition starts, then gives the text it shows so far, as often as
/// it changes: [`Clear`](Composition::Clear), then one
/// [`Push`](Composition::Push) per character, then where its own caret is
/// within that text. It ends with [`End`](Composition::End), and what it
/// produced, if anything, arrives as [`Event::Text`]. A widget shows the
/// text being composed at its caret, marked, and keeps it out of its own
/// text until it arrives as typed text.
///
/// The text arrives one character at a time so that the event stays
/// `Copy`. A widget keeps at most a bounded number of characters of it.
///
/// No window backend sends these yet: on Win32 the system shows the
/// composition in a window of its own at the caret (which
/// [`Widget::caret`](crate::Widget::caret) gives) and only the text it
/// produces reaches the interface. The widgets that take text already
/// follow this protocol, and the test harness can send it.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Composition, Event, Role, State, text_field};
///
/// let text = State::new(String::new());
/// let mut ui = Harness::new(text_field("City", text.clone()), Size::new(160.0, 30.0), 1.0);
/// ui.frame();
/// assert!(ui.tab());
/// // An input method shows "とう" while the person types "tou" ...
/// ui.send(Event::Composition(Composition::Start));
/// ui.send(Event::Composition(Composition::Clear));
/// ui.send(Event::Composition(Composition::Push('と')));
/// ui.send(Event::Composition(Composition::Push('う')));
/// ui.send(Event::Composition(Composition::Caret(2)));
/// // ... which is shown, but not yet typed.
/// assert_eq!(ui.get(Role::TextInput, "City").value_text(), Some(""));
/// // The person picks "東" and the input method types it.
/// ui.send(Event::Composition(Composition::End));
/// ui.send(Event::Text('東'));
/// assert_eq!(ui.get(Role::TextInput, "City").value_text(), Some("東"));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Composition {
    /// A composition starts at the caret. Typing it replaces the selection,
    /// as typing anything does, so the selection is removed now.
    Start,
    /// The text being composed is empty again; what it now is follows.
    Clear,
    /// One more character of the text being composed.
    Push(char),
    /// Where the input method's caret is in the text being composed, in
    /// characters from its start.
    Caret(u32),
    /// The composition ended. What it produced, if anything, comes as
    /// [`Event::Text`].
    End,
}

impl Event {
    /// Where the pointer is, for pointer and wheel events.
    pub fn position(&self) -> Option<Point> {
        match *self {
            Event::PointerDown { at, .. }
            | Event::PointerMove { at, .. }
            | Event::PointerUp { at, .. }
            | Event::Wheel { at, .. } => Some(at),
            Event::PointerCancel
            | Event::Key(_)
            | Event::Focus(_)
            | Event::Text(_)
            | Event::Composition(_)
            | Event::PointerLeave
            | Event::Menu(_) => None,
        }
    }
}

/// A drag from another window or program, as a window layer reports it to
/// [`Root::file_drop`](crate::Root::file_drop).
///
/// The root runs it as a drag of its own whose
/// [`DragPayload`](crate::DragPayload) holds the [`DropData`] and that has
/// no source widget, so widgets take part exactly as in a drag inside the
/// interface, through [`Widget::drag`](crate::Widget::drag): a target asks
/// for the data with `payload.get::<DropData>()`.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum FileDrop {
    /// The drag came over the interface at `at`, in logical pixels.
    Enter {
        /// Where the pointer is.
        at: Point,
    },
    /// The drag moved to `at`.
    Over {
        /// Where the pointer is.
        at: Point,
    },
    /// The drag left the interface, or was cancelled.
    Leave,
    /// The drag was dropped at `at`.
    Drop {
        /// Where.
        at: Point,
    },
}

/// What a drag from another program carries: files, text, or both. A
/// widget finds it in the drag's payload (see [`FileDrop`]).
///
/// ```
/// use std::path::PathBuf;
/// use manguebyte_ui::DropData;
///
/// let kit = DropData::files(["kick.wav", "snare.wav"]);
/// assert_eq!(kit.files[1], PathBuf::from("snare.wav"));
/// assert!(kit.has_file_with_extension(&["WAV", "flac"]));
/// assert_eq!(kit.description(), "2 files");
/// assert!(!DropData::text("hello").has_file_with_extension(&["wav"]));
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub struct DropData {
    /// The paths of the files and folders, in the order the source gave
    /// them.
    pub files: Vec<PathBuf>,
    /// The text, if any.
    pub text: Option<String>,
}

impl DropData {
    /// A drag of `files`.
    pub fn files<P: Into<PathBuf>>(files: impl IntoIterator<Item = P>) -> DropData {
        DropData {
            files: files.into_iter().map(Into::into).collect(),
            text: None,
        }
    }

    /// A drag of `text`.
    pub fn text(text: impl Into<String>) -> DropData {
        DropData {
            files: Vec::new(),
            text: Some(text.into()),
        }
    }

    /// The same, with `text` as well.
    #[must_use]
    pub fn with_text(mut self, text: impl Into<String>) -> DropData {
        self.text = Some(text.into());
        self
    }

    /// Whether it carries no file and no text.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty() && self.text.is_none()
    }

    /// Whether one of the files has one of `extensions` (without the dot,
    /// in any case), as a widget that takes only some kinds of file asks.
    pub fn has_file_with_extension(&self, extensions: &[&str]) -> bool {
        self.files.iter().any(|file| {
            file.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| {
                    extensions
                        .iter()
                        .any(|wanted| wanted.eq_ignore_ascii_case(extension))
                })
        })
    }

    /// What it is, in a few words, as a drag's payload describes it: the
    /// file's name for one file, "3 files" for several, "Text" for text.
    pub fn description(&self) -> String {
        match self.files.as_slice() {
            [] => "Text".to_owned(),
            [one] => one.file_name().map_or_else(
                || one.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            ),
            many => format!("{} files", many.len()),
        }
    }
}

/// Whether a widget used an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handled {
    /// It did; nothing else sees the event.
    Yes,
    /// It did not; the event goes to the widget that contains it.
    No,
}

impl Handled {
    /// Whether the event was used.
    pub fn is_yes(self) -> bool {
        self == Handled::Yes
    }
}
