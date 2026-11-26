//! Plain conversions and bookkeeping for the macOS backend: key codes,
//! modifier flags, wheel deltas, typed text, geometry, the view's class name
//! and the events that may go on to the host.
//!
//! Nothing here calls the system, so all of it is tested on every platform.

#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::collections::VecDeque;

use manguebyte_gfx::IRect;

use crate::dispatch::MAX_QUEUED;
use crate::event::{Key, Modifiers, PhysicalPoint, PhysicalSize, WHEEL_DELTA};

/// `NSEventModifierFlagShift`.
pub(crate) const FLAG_SHIFT: usize = 1 << 17;
/// `NSEventModifierFlagControl`.
pub(crate) const FLAG_CONTROL: usize = 1 << 18;
/// `NSEventModifierFlagOption`.
pub(crate) const FLAG_OPTION: usize = 1 << 19;
/// `NSEventModifierFlagCommand`.
pub(crate) const FLAG_COMMAND: usize = 1 << 20;
/// `NSEventModifierFlagCapsLock`.
pub(crate) const FLAG_CAPS_LOCK: usize = 1 << 16;
/// `NSEventModifierFlagFunction`.
pub(crate) const FLAG_FUNCTION: usize = 1 << 23;

/// How many points of a precise (trackpad) scroll make one line.
pub(crate) const POINTS_PER_LINE: f64 = 10.0;

/// The modifiers in an event's flags. Command is `meta` and Option is
/// `alt`.
pub(crate) fn modifiers(flags: usize) -> Modifiers {
    Modifiers {
        shift: flags & FLAG_SHIFT != 0,
        control: flags & FLAG_CONTROL != 0,
        alt: flags & FLAG_OPTION != 0,
        meta: flags & FLAG_COMMAND != 0,
    }
}

/// The flag that is set while the modifier key `code` is held, or `None`
/// for a key that is not a modifier.
pub(crate) fn modifier_flag(code: u16) -> Option<usize> {
    match code {
        54 | 55 => Some(FLAG_COMMAND),
        56 | 60 => Some(FLAG_SHIFT),
        57 => Some(FLAG_CAPS_LOCK),
        58 | 61 => Some(FLAG_OPTION),
        59 | 62 => Some(FLAG_CONTROL),
        63 => Some(FLAG_FUNCTION),
        _ => None,
    }
}

/// The key for a virtual key code. `unmodified` is the character the key
/// types in the current layout with no modifier held, if it types one.
/// Keys that type nothing are told apart by their codes, which do not
/// depend on the layout.
pub(crate) fn key(code: u16, unmodified: Option<char>) -> Key {
    match code {
        36 | 76 => Key::Enter,
        48 => Key::Tab,
        49 => Key::Character(' '),
        51 => Key::Backspace,
        53 => Key::Escape,
        114 => Key::Insert,
        115 => Key::Home,
        116 => Key::PageUp,
        117 => Key::Delete,
        119 => Key::End,
        121 => Key::PageDown,
        123 => Key::Left,
        124 => Key::Right,
        125 => Key::Down,
        126 => Key::Up,
        122 => Key::Function(1),
        120 => Key::Function(2),
        99 => Key::Function(3),
        118 => Key::Function(4),
        96 => Key::Function(5),
        97 => Key::Function(6),
        98 => Key::Function(7),
        100 => Key::Function(8),
        101 => Key::Function(9),
        109 => Key::Function(10),
        103 => Key::Function(11),
        111 => Key::Function(12),
        105 => Key::Function(13),
        107 => Key::Function(14),
        113 => Key::Function(15),
        106 => Key::Function(16),
        64 => Key::Function(17),
        79 => Key::Function(18),
        80 => Key::Function(19),
        90 => Key::Function(20),
        _ if modifier_flag(code).is_some() => Key::Other,
        _ => match unmodified {
            Some(c) if !c.is_control() && !is_function_character(c) => {
                let mut lower = c.to_lowercase();
                match (lower.next(), lower.next()) {
                    (Some(one), None) => Key::Character(one),
                    _ => Key::Character(c),
                }
            }
            _ => Key::Other,
        },
    }
}

/// Whether `c` is one of the private-use characters AppKit puts in a key
/// event for the arrows, the function keys and their like.
fn is_function_character(c: char) -> bool {
    ('\u{F700}'..='\u{F8FF}').contains(&c)
}

/// The UTF-16 units of `text` that are typed text: control characters and
/// AppKit's function-key characters arrive as keys only.
pub(crate) fn typed_units(text: &str) -> Vec<u16> {
    let mut units = Vec::new();
    for c in text.chars() {
        if !c.is_control() && !is_function_character(c) {
            units.extend_from_slice(c.encode_utf16(&mut [0; 2]));
        }
    }
    units
}

/// Turns scrolling deltas into wheel units, keeping what is smaller than a
/// unit for the next turn, so that a slow trackpad scroll is not lost.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Wheel {
    rest_x: f64,
    rest_y: f64,
}

impl Wheel {
    /// The wheel units, across and up, of a scroll by `dx` and `dy`:
    /// points when `precise`, lines otherwise, with AppKit's signs. Above
    /// 0 is to the right and away from the person, so the horizontal sign
    /// is turned over: AppKit counts it the way the contents move.
    pub(crate) fn turn(&mut self, dx: f64, dy: f64, precise: bool) -> (i32, i32) {
        let per_line = if precise { POINTS_PER_LINE } else { 1.0 };
        let units = |rest: &mut f64, delta: f64| {
            if !delta.is_finite() {
                return 0;
            }
            let total = *rest + delta / per_line * f64::from(WHEEL_DELTA);
            // Saturates for a delta too large for the type.
            let whole = total.trunc() as i32;
            let left = total - f64::from(whole);
            *rest = if left.abs() < 1.0 { left } else { 0.0 };
            whole
        };
        (units(&mut self.rest_x, -dx), units(&mut self.rest_y, dy))
    }
}

/// The device pixel holding the point `(x, y)` of a view `height` points
/// tall, measured from its bottom-left corner as AppKit does, at `scale`
/// pixels per point.
pub(crate) fn device_point(x: f64, y: f64, height: f64, scale: f64) -> PhysicalPoint {
    // The allowance keeps a point that lies on a pixel's edge in that
    // pixel when the arithmetic lands a hair below it.
    let pixel = |v: f64| (v * scale + 1.0 / 1024.0).floor() as i32;
    PhysicalPoint::new(pixel(x), pixel(height - y))
}

/// The device size of a view of `width` by `height` points at `scale`.
pub(crate) fn device_size(width: f64, height: f64, scale: f64) -> PhysicalSize {
    // A float too large, negative or not a number saturates or becomes 0.
    let pixels = |v: f64| (v * scale).round() as u32;
    PhysicalSize::new(pixels(width), pixels(height))
}

/// Where the device rectangle `rect` lies in a view `height` points tall:
/// its left, bottom, width and height in points, from the view's
/// bottom-left corner.
pub(crate) fn view_rect(rect: IRect, height: f64, scale: f64) -> [f64; 4] {
    [
        f64::from(rect.x) / scale,
        height - (f64::from(rect.y) + f64::from(rect.height)) / scale,
        f64::from(rect.width) / scale,
        f64::from(rect.height) / scale,
    ]
}

/// The name of the view's class: it carries the address of a static of
/// this copy of the crate, which differs between two binaries loaded in
/// one process, and between two copies of the framework in one binary.
pub(crate) fn class_name(kind: &str, anchor: usize) -> String {
    let version = env!("CARGO_PKG_VERSION").replace(|c: char| !c.is_ascii_alphanumeric(), "_");
    format!("Manguebyte{kind}_{version}_{anchor:x}")
}

/// The most events kept for an answer. An event whose answer never comes
/// (one the full queue dropped) is forgotten when this many newer ones have
/// arrived.
const MAX_KEPT: usize = 2 * MAX_QUEUED;

/// A system event kept until the handler has answered everything made from
/// it.
#[derive(Debug)]
struct Kept<E> {
    number: usize,
    event: E,
    /// Answers still to come.
    waiting: usize,
    used: bool,
    /// Whether more may still be made from it.
    open: bool,
}

/// Decides which system events go on to the host: those nothing made from
/// them was used. A key on macOS makes a key event and, through the input
/// method, any number of text events; the system's event goes on once, after
/// all of them were answered, if the handler used none.
#[derive(Debug)]
pub(crate) struct PassOn<E> {
    next: usize,
    kept: VecDeque<Kept<E>>,
}

impl<E> Default for PassOn<E> {
    fn default() -> Self {
        PassOn {
            next: 0,
            kept: VecDeque::new(),
        }
    }
}

impl<E> PassOn<E> {
    /// Keeps `event`, still open, and returns its number.
    pub(crate) fn keep(&mut self, event: E) -> usize {
        if self.kept.len() >= MAX_KEPT {
            self.kept.pop_front();
        }
        self.next = self.next.wrapping_add(1);
        self.kept.push_back(Kept {
            number: self.next,
            event,
            waiting: 0,
            used: false,
            open: true,
        });
        self.next
    }

    /// `count` more answers will come for the event `number`.
    pub(crate) fn expect(&mut self, number: usize, count: usize) {
        if let Some(index) = self.find(number) {
            self.kept[index].waiting += count;
        }
    }

    /// The event `number` was used by other means, as by an input method.
    pub(crate) fn used(&mut self, number: usize) {
        if let Some(index) = self.find(number) {
            self.kept[index].used = true;
        }
    }

    /// One answer for the event `number`. Returns the event if it is to go
    /// on to the host now.
    pub(crate) fn answer(&mut self, number: usize, used: bool) -> Option<E> {
        let index = self.find(number)?;
        let kept = &mut self.kept[index];
        kept.waiting = kept.waiting.saturating_sub(1);
        kept.used |= used;
        self.settle(index)
    }

    /// Nothing more will be made from the event `number`. Returns the event
    /// if it is to go on to the host now.
    pub(crate) fn close(&mut self, number: usize) -> Option<E> {
        let index = self.find(number)?;
        self.kept[index].open = false;
        self.settle(index)
    }

    /// Forgets every event.
    pub(crate) fn clear(&mut self) {
        self.kept.clear();
    }

    fn find(&self, number: usize) -> Option<usize> {
        self.kept.iter().position(|kept| kept.number == number)
    }

    fn settle(&mut self, index: usize) -> Option<E> {
        let kept = &self.kept[index];
        if kept.open || kept.waiting > 0 {
            return None;
        }
        let kept = self.kept.remove(index)?;
        (!kept.used).then_some(kept.event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_flag_is_its_modifier() {
        assert_eq!(modifiers(0), Modifiers::NONE);
        assert_eq!(modifiers(FLAG_CAPS_LOCK | 0xFF), Modifiers::NONE);
        let only = |shift, control, alt, meta| Modifiers {
            shift,
            control,
            alt,
            meta,
        };
        assert_eq!(modifiers(FLAG_SHIFT), only(true, false, false, false));
        assert_eq!(modifiers(FLAG_CONTROL), only(false, true, false, false));
        assert_eq!(modifiers(FLAG_OPTION), only(false, false, true, false));
        assert_eq!(modifiers(FLAG_COMMAND), only(false, false, false, true));
    }

    #[test]
    fn keys_come_from_codes_and_characters_from_the_layout() {
        assert_eq!(key(36, Some('\r')), Key::Enter);
        assert_eq!(key(76, Some('\u{3}')), Key::Enter);
        assert_eq!(key(51, Some('\u{7F}')), Key::Backspace);
        assert_eq!(key(117, Some('\u{F728}')), Key::Delete);
        assert_eq!(key(126, Some('\u{F700}')), Key::Up);
        assert_eq!(key(122, None), Key::Function(1));
        assert_eq!(key(90, None), Key::Function(20));
        assert_eq!(key(49, Some(' ')), Key::Character(' '));
        assert_eq!(key(0, Some('A')), Key::Character('a'));
        assert_eq!(key(0, Some('q')), Key::Character('q'));
        // A modifier on its own, a dead key, and a function character on a
        // code with no name here.
        assert_eq!(key(55, Some('x')), Key::Other);
        assert_eq!(key(14, None), Key::Other);
        assert_eq!(key(110, Some('\u{F735}')), Key::Other);
    }

    #[test]
    fn modifier_keys_name_their_flags() {
        assert_eq!(modifier_flag(55), Some(FLAG_COMMAND));
        assert_eq!(modifier_flag(60), Some(FLAG_SHIFT));
        assert_eq!(modifier_flag(61), Some(FLAG_OPTION));
        assert_eq!(modifier_flag(59), Some(FLAG_CONTROL));
        assert_eq!(modifier_flag(0), None);
    }

    #[test]
    fn typed_text_leaves_out_what_arrives_as_keys() {
        assert_eq!(typed_units("a\r\t\u{7F}\u{F700}b"), [0x61, 0x62]);
        assert_eq!(typed_units("\u{1F600}"), [0xD83D, 0xDE00]);
        assert_eq!(typed_units(""), [0u16; 0]);
    }

    #[test]
    fn lines_and_points_become_wheel_units_with_the_windows_signs() {
        let mut wheel = Wheel::default();
        // One line up; one line of contents moving right is a turn left.
        assert_eq!(wheel.turn(0.0, 1.0, false), (0, 120));
        assert_eq!(wheel.turn(1.0, -2.0, false), (-120, -240));
        assert_eq!(wheel.turn(-5.0, 20.0, true), (60, 240));
        // Too small for a unit on its own, but not lost.
        let mut sum = 0;
        for _ in 0..100 {
            sum += wheel.turn(0.0, 0.05, true).1;
        }
        assert!((59..=60).contains(&sum), "{sum}");
    }

    #[test]
    fn unusable_deltas_turn_nothing_and_leave_no_rest() {
        let mut wheel = Wheel::default();
        assert_eq!(wheel.turn(f64::NAN, f64::INFINITY, false), (0, 0));
        assert_eq!(wheel.turn(0.0, 1e30, false), (0, i32::MAX));
        assert_eq!(wheel.turn(0.0, 1.0, false), (0, 120));
    }

    #[test]
    fn view_points_become_device_pixels_from_the_top_left() {
        // A view 50 points tall at 2 pixels per point.
        let at = |x, y| device_point(x, y, 50.0, 2.0);
        assert_eq!(at(0.0, 50.0), PhysicalPoint::new(0, 0));
        assert_eq!(at(10.0, 45.0), PhysicalPoint::new(20, 10));
        assert_eq!(at(10.25, 44.75), PhysicalPoint::new(20, 10));
        assert_eq!(at(-1.0, 51.0), PhysicalPoint::new(-2, -2));
        // A third of a point at 1.5 pixels per point.
        let third = device_point(1.0 / 3.0 * 6.0, 0.0, 0.0, 1.5);
        assert_eq!(third, PhysicalPoint::new(3, 0));
    }

    #[test]
    fn sizes_and_rectangles_convert_both_ways() {
        assert_eq!(device_size(100.0, 50.5, 2.0), PhysicalSize::new(200, 101));
        assert_eq!(device_size(-1.0, f64::NAN, 2.0), PhysicalSize::new(0, 0));
        assert_eq!(
            view_rect(IRect::new(20, 10, 4, 30), 50.0, 2.0),
            [10.0, 30.0, 2.0, 15.0]
        );
    }

    #[test]
    fn class_names_differ_with_the_binarys_address() {
        let one = class_name("View", 0x1000);
        assert_ne!(one, class_name("View", 0x2000));
        assert_ne!(one, class_name("TestHost", 0x1000));
        assert!(one.ends_with("_1000"));
        assert!(one.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn an_event_goes_on_once_when_nothing_made_from_it_was_used() {
        let mut pass = PassOn::default();
        let key = pass.keep('k');
        // The key, answered at once, then two characters of text.
        pass.expect(key, 1);
        assert_eq!(pass.answer(key, false), None, "still open");
        pass.expect(key, 2);
        assert_eq!(pass.answer(key, false), None);
        assert_eq!(pass.answer(key, false), None);
        assert_eq!(pass.close(key), Some('k'));
        assert_eq!(pass.close(key), None, "only once");
    }

    #[test]
    fn one_use_keeps_the_event() {
        let mut pass = PassOn::default();
        let key = pass.keep('k');
        pass.expect(key, 2);
        assert_eq!(pass.answer(key, false), None);
        assert_eq!(pass.answer(key, true), None);
        assert_eq!(pass.close(key), None);
        let composed = pass.keep('c');
        pass.used(composed);
        assert_eq!(pass.close(composed), None);
    }

    #[test]
    fn answers_that_come_after_the_event_closed_still_decide() {
        let mut pass = PassOn::default();
        let first = pass.keep(1);
        pass.expect(first, 1);
        assert_eq!(pass.close(first), None);
        let second = pass.keep(2);
        pass.expect(second, 1);
        assert_eq!(pass.close(second), None);
        assert_eq!(pass.answer(second, true), None);
        assert_eq!(pass.answer(first, false), Some(1));
        // An event that made nothing goes on as it closes.
        let silent = pass.keep(3);
        assert_eq!(pass.close(silent), Some(3));
    }

    #[test]
    fn events_that_are_never_answered_are_forgotten() {
        let mut pass = PassOn::default();
        let first = pass.keep(0);
        pass.expect(first, 1);
        assert_eq!(pass.close(first), None);
        for event in 1..=MAX_KEPT {
            let number = pass.keep(event);
            pass.expect(number, 1);
        }
        assert_eq!(pass.kept.len(), MAX_KEPT);
        assert_eq!(pass.answer(first, false), None);
        pass.clear();
        assert_eq!(pass.kept.len(), 0);
    }
}
