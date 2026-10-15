//! Plain conversions the backends share: units, Win32 message parameters,
//! keys and window class names.
//!
//! Nothing here calls the system, so all of it is tested on every platform,
//! including the parts only the Win32 backend uses.

#![cfg_attr(not(windows), allow(dead_code))]

use manguebyte_gfx::{Point, Size};

use crate::event::{Key, Modifiers, PhysicalPoint, PhysicalSize, WHEEL_DELTA};

/// A device position in logical pixels at `scale`.
pub(crate) fn to_logical(at: PhysicalPoint, scale: f64) -> Point {
    Point::new(
        (f64::from(at.x) / scale) as f32,
        (f64::from(at.y) / scale) as f32,
    )
}

/// A device size in logical pixels at `scale`.
pub(crate) fn logical_size(size: PhysicalSize, scale: f64) -> Size {
    Size::new(
        (f64::from(size.width) / scale) as f32,
        (f64::from(size.height) / scale) as f32,
    )
}

/// The scale for a DPI value, where 96 is 1.0; `None` for a DPI that gives
/// no usable scale.
pub(crate) fn scale_from_dpi(dpi: u32) -> Option<f64> {
    manguebyte_plugin::editor::checked_scale(f64::from(dpi) / 96.0)
}

/// Wheel units as lines.
pub(crate) fn wheel_lines(delta: i32) -> f32 {
    (f64::from(delta) / f64::from(WHEEL_DELTA)) as f32
}

/// The signed client position packed in a mouse message's `LPARAM`.
pub(crate) fn point_from_lparam(lparam: isize) -> PhysicalPoint {
    let bits = lparam as usize;
    PhysicalPoint::new(
        i32::from((bits & 0xFFFF) as u16 as i16),
        i32::from(((bits >> 16) & 0xFFFF) as u16 as i16),
    )
}

/// The unsigned size packed in a `WM_SIZE` `LPARAM`.
pub(crate) fn size_from_lparam(lparam: isize) -> PhysicalSize {
    let bits = lparam as usize;
    PhysicalSize::new((bits & 0xFFFF) as u32, ((bits >> 16) & 0xFFFF) as u32)
}

/// A position packed as a mouse message's `LPARAM`. Coordinates outside
/// the 16-bit range wrap, as they do in the system's own messages.
#[cfg_attr(not(feature = "testing"), allow(dead_code))]
pub(crate) fn lparam_from_point(at: PhysicalPoint) -> isize {
    let x = at.x as u16 as usize;
    let y = at.y as u16 as usize;
    (x | (y << 16)) as isize
}

/// The signed wheel turn in the high word of a wheel message's `WPARAM`.
pub(crate) fn wheel_from_wparam(wparam: usize) -> i32 {
    i32::from(((wparam >> 16) & 0xFFFF) as u16 as i16)
}

/// `MK_SHIFT` in a mouse message's `WPARAM`.
pub(crate) const MK_SHIFT: usize = 0x0004;
/// `MK_CONTROL` in a mouse message's `WPARAM`.
pub(crate) const MK_CONTROL: usize = 0x0008;

/// The modifiers of a mouse message: Shift and Control from its `WPARAM`,
/// Alt and the Windows key from the keyboard state.
pub(crate) fn mouse_modifiers(wparam: usize, alt: bool, meta: bool) -> Modifiers {
    Modifiers {
        shift: wparam & MK_SHIFT != 0,
        control: wparam & MK_CONTROL != 0,
        alt,
        meta,
    }
}

/// Whether a key message's `LPARAM` says the key was already down.
pub(crate) fn key_repeat(lparam: isize) -> bool {
    (lparam as usize >> 30) & 1 == 1
}

/// The key for a Win32 virtual-key code. `mapped` is what
/// `MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR)` returned for it: the unshifted
/// character in the current layout, 0 for none, with the top bit set for a
/// dead key.
pub(crate) fn key_from_vk(vk: u32, mapped: u32) -> Key {
    match vk {
        0x08 => Key::Backspace,
        0x09 => Key::Tab,
        0x0D => Key::Enter,
        0x1B => Key::Escape,
        0x20 => Key::Character(' '),
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x23 => Key::End,
        0x24 => Key::Home,
        0x25 => Key::Left,
        0x26 => Key::Up,
        0x27 => Key::Right,
        0x28 => Key::Down,
        0x2D => Key::Insert,
        0x2E => Key::Delete,
        // F1 to F24.
        0x70..=0x87 => Key::Function((vk - 0x70 + 1) as u8),
        // Modifier keys on their own.
        0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0xA0..=0xA5 => Key::Other,
        _ => {
            if mapped == 0 || mapped & 0x8000_0000 != 0 {
                return Key::Other;
            }
            match char::from_u32(mapped & 0xFFFF) {
                Some(c) if !c.is_control() => {
                    let mut lower = c.to_lowercase();
                    match (lower.next(), lower.next()) {
                        (Some(one), None) => Key::Character(one),
                        _ => Key::Character(c),
                    }
                }
                _ => Key::Other,
            }
        }
    }
}

/// Whether a UTF-16 unit of typed text is a control character (Enter,
/// Tab, Backspace, Escape, the Control combinations), which a window
/// reports as a key and never as text.
pub(crate) fn is_control_unit(unit: u16) -> bool {
    char::from_u32(u32::from(unit)).is_some_and(char::is_control)
}

/// Joins UTF-16 units of typed text into characters.
#[derive(Debug, Default)]
pub(crate) struct Utf16Joiner {
    high: Option<u16>,
}

impl Utf16Joiner {
    /// The characters `unit` completes, in order: at most two, because a
    /// high surrogate that is not followed by a low one becomes U+FFFD
    /// before the unit that broke the pair. A lone low surrogate becomes
    /// U+FFFD too. Control characters give nothing, since they arrive as
    /// keys.
    pub(crate) fn push(&mut self, unit: u16) -> [Option<char>; 2] {
        let lost = |high: Option<u16>| high.map(|_| char::REPLACEMENT_CHARACTER);
        match unit {
            0xD800..=0xDBFF => [lost(self.high.replace(unit)), None],
            0xDC00..=0xDFFF => match self.high.take() {
                Some(high) => {
                    let code =
                        0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(unit) - 0xDC00);
                    [char::from_u32(code), None]
                }
                None => [Some(char::REPLACEMENT_CHARACTER), None],
            },
            _ => {
                let c = char::from_u32(u32::from(unit)).filter(|c| !c.is_control());
                [lost(self.high.take()), c]
            }
        }
    }
}

/// The name of a window class: unique to this binary (through its module
/// handle) and to this copy of the crate (through the address of one of
/// its statics), so two plugins, or two versions of the framework in one
/// plugin, never share a class. NUL-terminated UTF-16.
pub(crate) fn class_name(kind: &str, module: usize, anchor: usize) -> Vec<u16> {
    format!(
        "Manguebyte.{kind}.{}.{module:x}.{anchor:x}",
        env!("CARGO_PKG_VERSION")
    )
    .encode_utf16()
    .chain(std::iter::once(0))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_positions_divide_by_the_scale() {
        assert_eq!(
            to_logical(PhysicalPoint::new(15, 30), 1.5),
            Point::new(10.0, 20.0)
        );
        assert_eq!(
            to_logical(PhysicalPoint::new(-3, 7), 1.5),
            Point::new(-2.0, (7.0f64 / 1.5) as f32)
        );
        assert_eq!(
            to_logical(PhysicalPoint::new(7, 9), 1.0),
            Point::new(7.0, 9.0)
        );
        assert_eq!(
            logical_size(PhysicalSize::new(300, 150), 1.5),
            Size::new(200.0, 100.0)
        );
    }

    #[test]
    fn dpi_gives_the_scale_or_nothing() {
        assert_eq!(scale_from_dpi(96), Some(1.0));
        assert_eq!(scale_from_dpi(144), Some(1.5));
        assert_eq!(scale_from_dpi(192), Some(2.0));
        assert_eq!(scale_from_dpi(0), None);
        assert_eq!(scale_from_dpi(u32::MAX), None);
    }

    #[test]
    fn wheel_units_become_lines() {
        assert_eq!(wheel_lines(120), 1.0);
        assert_eq!(wheel_lines(-240), -2.0);
        assert_eq!(wheel_lines(60), 0.5);
        assert_eq!(wheel_lines(15), 0.125);
        assert_eq!(wheel_from_wparam(0xFF88_0000), -120);
        assert_eq!(wheel_from_wparam(0x0078_0008), 120);
    }

    #[test]
    fn message_positions_are_signed_sixteen_bit() {
        for at in [
            PhysicalPoint::new(0, 0),
            PhysicalPoint::new(15, 30),
            PhysicalPoint::new(-1, -32768),
            PhysicalPoint::new(32767, -5),
        ] {
            assert_eq!(point_from_lparam(lparam_from_point(at)), at);
        }
        assert_eq!(point_from_lparam(0xFFFF_FFFF), PhysicalPoint::new(-1, -1));
        assert_eq!(point_from_lparam(-1), PhysicalPoint::new(-1, -1));
        assert_eq!(size_from_lparam(0x0032_0064), PhysicalSize::new(100, 50));
        assert_eq!(size_from_lparam(-1), PhysicalSize::new(65535, 65535));
    }

    #[test]
    fn mouse_modifiers_come_from_the_flags_and_the_keyboard() {
        assert_eq!(mouse_modifiers(0, false, false), Modifiers::NONE);
        let m = mouse_modifiers(MK_SHIFT | MK_CONTROL | 0x1, true, false);
        assert_eq!(
            m,
            Modifiers {
                shift: true,
                control: true,
                alt: true,
                meta: false
            }
        );
        assert!(key_repeat(0x4000_0001));
        assert!(!key_repeat(0x0000_0001));
    }

    #[test]
    fn keys_map_by_virtual_code_then_by_layout() {
        assert_eq!(key_from_vk(0x25, 0), Key::Left);
        assert_eq!(key_from_vk(0x0D, 0x0D), Key::Enter);
        assert_eq!(key_from_vk(0x70, 0), Key::Function(1));
        assert_eq!(key_from_vk(0x87, 0), Key::Function(24));
        assert_eq!(key_from_vk(0x20, 0x20), Key::Character(' '));
        assert_eq!(key_from_vk(0x41, 0x41), Key::Character('a'));
        assert_eq!(key_from_vk(0x31, 0x31), Key::Character('1'));
        // A Cyrillic layout maps the same key to another letter.
        assert_eq!(key_from_vk(0x41, 0x0424), Key::Character('\u{0444}'));
        // A dead key, no mapping, a modifier, a control character.
        assert_eq!(key_from_vk(0xDE, 0x8000_0027), Key::Other);
        assert_eq!(key_from_vk(0xFF, 0), Key::Other);
        assert_eq!(key_from_vk(0x10, 0), Key::Other);
        assert_eq!(key_from_vk(0x99, 0x7F), Key::Other);
        // Every code maps to something without panicking.
        for vk in 0..=0x1FF {
            for mapped in [0, 0x41, 0xD800, 0x8000_0041, 0xFFFF_FFFF] {
                let _ = key_from_vk(vk, mapped);
            }
        }
    }

    #[test]
    fn text_joins_surrogate_pairs() {
        let mut joiner = Utf16Joiner::default();
        assert_eq!(joiner.push(u16::from(b'a')), [None, Some('a')]);
        // U+1F600 is D83D DE00.
        assert_eq!(joiner.push(0xD83D), [None, None]);
        assert_eq!(joiner.push(0xDE00), [Some('\u{1F600}'), None]);
        // A broken pair says so, then delivers what broke it.
        assert_eq!(joiner.push(0xD83D), [None, None]);
        assert_eq!(joiner.push(u16::from(b'b')), [Some('\u{FFFD}'), Some('b')]);
        assert_eq!(joiner.push(0xDE00), [Some('\u{FFFD}'), None]);
        assert_eq!(joiner.push(0xD800), [None, None]);
        assert_eq!(joiner.push(0xD801), [Some('\u{FFFD}'), None]);
        // Control characters are keys, not text.
        let mut joiner = Utf16Joiner::default();
        for control in [0x08, 0x09, 0x0D, 0x1B, 0x7F, 0x01] {
            assert_eq!(joiner.push(control), [None, None]);
        }
        assert_eq!(joiner.push(0xE9), [None, Some('\u{e9}')]);
    }

    #[test]
    fn control_units_are_the_ones_text_leaves_out() {
        for unit in [0x00, 0x08, 0x09, 0x0D, 0x1B, 0x7F, 0x85] {
            assert!(is_control_unit(unit), "{unit:#x}");
        }
        for unit in [0x20, 0x41, 0xE9, 0xD83D, 0xDE00, 0xFFFD] {
            assert!(!is_control_unit(unit), "{unit:#x}");
        }
        // The same rule as the joiner's.
        for unit in 0..=0xFFFFu16 {
            let mut joiner = Utf16Joiner::default();
            if is_control_unit(unit) {
                assert_eq!(joiner.push(unit), [None, None], "{unit:#x}");
            }
        }
    }

    #[test]
    fn class_names_differ_by_module_and_by_copy_of_the_crate() {
        let name = class_name("Window", 0x7ff6_0000, 0x7ff6_1234);
        let text = String::from_utf16(&name[..name.len() - 1]).expect("utf-16");
        assert_eq!(
            text,
            format!(
                "Manguebyte.Window.{}.7ff60000.7ff61234",
                env!("CARGO_PKG_VERSION")
            )
        );
        assert_eq!(name.last(), Some(&0));
        assert_eq!(name.iter().filter(|&&unit| unit == 0).count(), 1);
        assert_ne!(name, class_name("Window", 0x1000_0000, 0x7ff6_1234));
        assert_ne!(name, class_name("Window", 0x7ff6_0000, 0x7ff6_1238));
        assert_ne!(name, class_name("TopLevel", 0x7ff6_0000, 0x7ff6_1234));
        let longest = class_name("TopLevel", usize::MAX, usize::MAX);
        assert!(longest.len() <= 256, "Win32 class names stop at 256 units");
    }
}
