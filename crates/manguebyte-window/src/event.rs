//! What a window tells its handler, and the raw input a platform feeds it.

use manguebyte_gfx::{IRect, Point};

pub use manguebyte_plugin::editor::{Key, KeyEvent, Modifiers};

use crate::damage::Damage;

/// How far one notch of a mouse wheel turns, in the units Win32 reports
/// (`WHEEL_DELTA`). A window turns these units into lines: one notch is one
/// line.
pub const WHEEL_DELTA: i32 = 120;

/// A width and a height in device pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhysicalSize {
    /// Columns.
    pub width: u32,
    /// Rows.
    pub height: u32,
}

impl PhysicalSize {
    /// A size of `width` by `height` device pixels.
    pub const fn new(width: u32, height: u32) -> Self {
        PhysicalSize { width, height }
    }

    /// Whether it covers no pixel.
    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// The rectangle `0, 0, width, height`.
    pub fn bounds(&self) -> IRect {
        IRect::new(0, 0, self.width, self.height)
    }
}

/// A position in device pixels from the top-left corner of the window.
///
/// It may lie outside the window while the window holds the pointer during
/// a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PhysicalPoint {
    /// Columns to the right.
    pub x: i32,
    /// Rows down.
    pub y: i32,
}

impl PhysicalPoint {
    /// The point `(x, y)`.
    pub const fn new(x: i32, y: i32) -> Self {
        PhysicalPoint { x, y }
    }
}

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

impl Button {
    /// A bit of its own, for sets of held buttons.
    pub(crate) fn bit(self) -> u8 {
        match self {
            Button::Primary => 1,
            Button::Secondary => 2,
            Button::Middle => 4,
        }
    }
}

/// A timer of a window, named by its owner.
///
/// A window keeps at most one timer per id; setting an id again replaces
/// its period.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TimerId(pub u32);

/// Where the pointer is, in both units, and which modifiers are held.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PointerEvent {
    /// Logical pixels: the device position divided by the window's scale.
    pub position: Point,
    /// Device pixels, as the system reported them.
    pub physical: PhysicalPoint,
    /// The modifier keys held.
    pub modifiers: Modifiers,
}

/// Something a window tells its handler.
///
/// Every event arrives on the thread that owns the window (the host's main
/// thread in a plugin), one at a time.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Event {
    /// The window needs these device-pixel rectangles drawn: the handler
    /// presents them. Covered parts of the window that become visible again
    /// are not reported; the window answers those from its back buffer.
    /// A paint that had to wait for a running handler arrives after the
    /// system's own paint request has ended; presenting works the same
    /// then, and a rectangle invalidated while painting comes in a later
    /// paint.
    Paint(Damage),
    /// A button went down. The window holds the pointer until every button
    /// is up again, so the moves and the up of a drag arrive even outside
    /// it.
    PointerDown {
        /// Where.
        pointer: PointerEvent,
        /// Which button.
        button: Button,
        /// 1 for a click, 2 for a double click, and so on, counted with the
        /// system's double-click time and distance.
        clicks: u32,
    },
    /// The pointer moved.
    PointerMove(PointerEvent),
    /// A button that went down in this window came up.
    PointerUp {
        /// Where.
        pointer: PointerEvent,
        /// Which button.
        button: Button,
    },
    /// The pointer left the window while no button was held, or a drag
    /// ended outside it.
    PointerLeave,
    /// The window lost the pointer in the middle of a drag, as when the
    /// system or the host takes it away. Every button counts as up.
    PointerCancel,
    /// The wheel turned with the pointer at `pointer`.
    Wheel {
        /// Where the pointer is.
        pointer: PointerEvent,
        /// How far, in lines: one notch is one line. `y` above 0 is away
        /// from the person; `x` above 0 is to the right.
        delta: Point,
    },
    /// A key went down or came up.
    Key {
        /// The key, its modifiers and its direction.
        key: KeyEvent,
        /// True for the repeats a held key sends after its first press.
        repeat: bool,
    },
    /// The keyboard typed a character. Control characters (Enter, Tab,
    /// Backspace, Escape and the Control combinations) arrive only as keys.
    Text(char),
    /// The window gained (`true`) or lost (`false`) the keyboard.
    Focus(bool),
    /// The window's size changed, by its own [`resize`](crate::Window::resize)
    /// or by the host. Parts that became visible are reported by a
    /// [`Paint`](Event::Paint) after this.
    Resized(PhysicalSize),
    /// The window's scale changed. The whole window is damaged after this.
    ScaleChanged(f64),
    /// A timer of the window fired. A timer that was killed never fires
    /// again, even if the system had already queued its tick.
    Timer(TimerId),
    /// The window was destroyed by someone else, normally because the host
    /// destroyed its parent. Nothing arrives after this, and the window
    /// refuses every request; dropping it is still required and safe.
    Closed,
}

/// Whether a handler used an event.
///
/// It matters for keys, typed text and wheel turns that came from the
/// system: one the handler did not use goes on to the host's window, so
/// that the host's own shortcuts keep working while the editor has the
/// keyboard. On Win32 the window sends the same message to its parent.
/// This holds for an event that had to wait for a running handler too: it
/// goes on, or not, when the handler answers it. Every other event ignores
/// the answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Handled {
    /// It did.
    Yes,
    /// It did not.
    No,
}

/// Input as a platform delivers it, before the window turns it into
/// [`Event`]s: positions in device pixels, wheel turns in
/// [`WHEEL_DELTA`] units, text in UTF-16 units.
///
/// The Win32 backend makes these from window messages; a test gives them
/// to a headless window through [`Headless::input`](crate::Headless::input).
/// Either way the same code turns them into events, holds the pointer
/// during drags and counts clicks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RawInput {
    /// A button went down.
    PointerDown {
        /// Where.
        at: PhysicalPoint,
        /// Which button.
        button: Button,
        /// The modifier keys held.
        modifiers: Modifiers,
        /// The system's time of the press, in milliseconds, for counting
        /// clicks. It may wrap around.
        time_ms: u32,
    },
    /// The pointer moved.
    PointerMove {
        /// Where.
        at: PhysicalPoint,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// A button came up.
    PointerUp {
        /// Where.
        at: PhysicalPoint,
        /// Which button.
        button: Button,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// The system says the pointer left the window.
    PointerLeft,
    /// The window lost the pointer it held.
    CaptureLost,
    /// The wheel turned.
    Wheel {
        /// Where the pointer is.
        at: PhysicalPoint,
        /// How far across, in [`WHEEL_DELTA`] units; above 0 is to the
        /// right.
        delta_x: i32,
        /// How far up, in [`WHEEL_DELTA`] units; above 0 is away from the
        /// person.
        delta_y: i32,
        /// The modifier keys held.
        modifiers: Modifiers,
    },
    /// A key went down or came up.
    Key {
        /// The key.
        key: KeyEvent,
        /// Whether this is a repeat of a held key.
        repeat: bool,
    },
    /// A UTF-16 unit of typed text. Surrogate pairs come as two units.
    Utf16(u16),
    /// The window gained or lost the keyboard.
    Focus(bool),
    /// A timer tick.
    Timer(TimerId),
}
