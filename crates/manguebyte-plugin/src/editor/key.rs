//! Keys that a plugin format delivers to the editor itself.

/// A key going down or coming up, as a format that delivers keys through
/// its own interface reports it.
///
/// VST 3 hosts often keep keyboard focus and pass keys on through
/// `IPlugView::onKeyDown` and `onKeyUp`; the wrapper turns those into
/// `KeyEvent`s for [`OpenEditor::key`](super::OpenEditor::key). Under CLAP
/// the editor's own window receives keys, and this type is not used.
///
/// ```
/// use manguebyte_plugin::editor::{Key, KeyEvent, Modifiers};
///
/// let undo = KeyEvent {
///     key: Key::Character('z'),
///     modifiers: Modifiers { control: true, ..Modifiers::NONE },
///     pressed: true,
/// };
/// assert!(undo.modifiers.any());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    /// Which key.
    pub key: Key,
    /// The modifier keys held at the time.
    pub modifiers: Modifiers,
    /// True when the key went down, false when it came up.
    pub pressed: bool,
}

/// A key on the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Key {
    /// A key that types a character, such as `'a'`, `'7'` or `' '`.
    Character(char),
    /// Backspace.
    Backspace,
    /// Tab.
    Tab,
    /// Return or Enter.
    Enter,
    /// Escape.
    Escape,
    /// Delete, which removes forwards.
    Delete,
    /// Insert.
    Insert,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
    /// Arrow left.
    Left,
    /// Arrow right.
    Right,
    /// Arrow up.
    Up,
    /// Arrow down.
    Down,
    /// A function key, numbered from 1 (F1).
    Function(u8),
    /// A key with no meaning here, such as a media key.
    Other,
}

/// The modifier keys held while a key is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Modifiers {
    /// Shift.
    pub shift: bool,
    /// Control.
    pub control: bool,
    /// Alt, or Option on macOS.
    pub alt: bool,
    /// The Windows key, or Command on macOS.
    pub meta: bool,
}

impl Modifiers {
    /// No modifier held.
    pub const NONE: Self = Self {
        shift: false,
        control: false,
        alt: false,
        meta: false,
    };

    /// Whether any modifier is held.
    pub fn any(self) -> bool {
        self != Self::NONE
    }
}
