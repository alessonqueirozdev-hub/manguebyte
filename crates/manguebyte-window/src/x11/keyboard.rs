//! The keyboard map: what a keycode and the state sent with it mean, by the
//! core protocol's rules and the way XKB servers lay their keymap out for
//! clients of the core protocol. Plain code, with no system call.

use crate::event::{Key, Modifiers};

const SHIFT: u16 = 1;
const LOCK: u16 = 2;
const CONTROL: u16 = 4;
const MOD1: u16 = 8;
const MOD4: u16 = 64;
/// Where an XKB server says which of its layouts is in use.
const GROUP_BITS: u16 = 0x6000;

const NO_SYMBOL: u32 = 0;
const NUM_LOCK: u32 = 0xFF7F;
const MODE_SWITCH: u32 = 0xFF7E;
const LEVEL3_SHIFT: u32 = 0xFE03;
const SHIFT_L: u32 = 0xFFE1;
const SHIFT_R: u32 = 0xFFE2;
const CONTROL_L: u32 = 0xFFE3;
const CONTROL_R: u32 = 0xFFE4;
const META_L: u32 = 0xFFE7;
const META_R: u32 = 0xFFE8;
const ALT_L: u32 = 0xFFE9;
const ALT_R: u32 = 0xFFEA;
const SUPER_L: u32 = 0xFFEB;
const SUPER_R: u32 = 0xFFEC;

/// What a key press or release means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Meaning {
    /// Which key it is: what it types unshifted in the layout in use.
    pub(super) key: Key,
    /// The modifier keys held, this one included while it goes down.
    pub(super) modifiers: Modifiers,
    /// The character the press types, if it types one.
    pub(super) text: Option<char>,
}

/// The server's keyboard map and which modifier bit means what.
pub(super) struct Keyboard {
    /// The first keycode of the map.
    first: u8,
    /// Keysyms to a keycode.
    per: usize,
    keysyms: Vec<u32>,
    alt: u16,
    meta: u16,
    num_lock: u16,
    level3: u16,
    mode_switch: u16,
}

impl Keyboard {
    /// The map of `GetKeyboardMapping` from the keycode `first`, `per`
    /// keysyms to a keycode, and the modifier map of `GetModifierMapping`:
    /// eight rows of keycodes, for Shift, Lock, Control and Mod1 to Mod5.
    pub(super) fn new(first: u8, per: u8, keysyms: Vec<u32>, modifier_map: &[u8]) -> Keyboard {
        let mut keyboard = Keyboard {
            first,
            per: usize::from(per),
            keysyms,
            alt: 0,
            meta: 0,
            num_lock: 0,
            level3: 0,
            mode_switch: 0,
        };
        let row = modifier_map.len() / 8;
        // The rows of Mod1 to Mod5; the first three have fixed meanings.
        for (index, codes) in modifier_map.chunks_exact(row.max(1)).enumerate().skip(3) {
            // Eight rows at most, so the bit is within a u16.
            let bit = 1u16 << index.min(15);
            for &code in codes.iter().filter(|&&code| code != 0) {
                for column in 0..keyboard.per {
                    match keyboard.keysym(code, column) {
                        ALT_L | ALT_R => keyboard.alt |= bit,
                        SUPER_L | SUPER_R => keyboard.meta |= bit,
                        NUM_LOCK => keyboard.num_lock |= bit,
                        LEVEL3_SHIFT => keyboard.level3 |= bit,
                        MODE_SWITCH => keyboard.mode_switch |= bit,
                        _ => {}
                    }
                }
            }
        }
        if keyboard.alt == 0 {
            keyboard.alt = MOD1;
        }
        if keyboard.meta == 0 {
            keyboard.meta = MOD4;
        }
        // XKB maps put a Mode_switch on the level 3 modifier too, and say
        // which layout is in use in the group bits instead.
        keyboard.mode_switch &= !keyboard.level3;
        keyboard
    }

    /// The keysym in `column` of `code`, or `NoSymbol`.
    fn keysym(&self, code: u8, column: usize) -> u32 {
        if column >= self.per {
            return NO_SYMBOL;
        }
        usize::from(code)
            .checked_sub(usize::from(self.first))
            .and_then(|index| self.keysyms.get(index * self.per + column))
            .copied()
            .unwrap_or(NO_SYMBOL)
    }

    /// The modifiers a pointer or key event's `state` holds.
    pub(super) fn modifiers(&self, state: u16) -> Modifiers {
        Modifiers {
            shift: state & SHIFT != 0,
            control: state & CONTROL != 0,
            alt: state & self.alt != 0,
            meta: state & self.meta != 0,
        }
    }

    /// What the key `code` means going down (`pressed`) or up, with the
    /// modifiers and layout of `state`.
    pub(super) fn meaning(&self, code: u8, state: u16, pressed: bool) -> Meaning {
        let switched = state & (GROUP_BITS | self.mode_switch) != 0;
        let mut group = usize::from(switched);
        if self.keysym(code, group * 2) == NO_SYMBOL && self.keysym(code, group * 2 + 1) == 0 {
            group = 0;
        }
        let plain = self.keysym(code, group * 2);
        let shifted = self.keysym(code, group * 2 + 1);
        let shift = state & SHIFT != 0;

        let keypad = is_keypad(plain) || is_keypad(shifted);
        // On the keypad Num Lock picks the digits, and Shift undoes it.
        let digits = keypad && is_keypad(shifted) && (state & self.num_lock != 0) != shift;
        let identity = if keypad {
            if digits { shifted } else { plain }
        } else {
            plain
        };

        let mut modifiers = self.modifiers(state);
        // The state is the one before the event: a modifier key that goes
        // down is held, and one that comes up is not.
        match plain {
            SHIFT_L | SHIFT_R => modifiers.shift = pressed,
            CONTROL_L | CONTROL_R => modifiers.control = pressed,
            ALT_L | ALT_R | META_L | META_R => modifiers.alt = pressed,
            SUPER_L | SUPER_R => modifiers.meta = pressed,
            _ => {}
        }

        // A shortcut types nothing.
        let shortcut = state & (CONTROL | self.alt | self.meta) != 0;
        let text = (pressed && !shortcut)
            .then(|| {
                if keypad {
                    return character(identity);
                }
                // Level 3 (AltGr) where the key has it.
                if state & self.level3 != 0 {
                    let third = self.keysym(code, 4 + group * 2 + usize::from(shift));
                    if third != NO_SYMBOL {
                        return character(third);
                    }
                }
                typed(plain, shifted, shift, state & LOCK != 0)
            })
            .flatten()
            .filter(|c| !c.is_control());

        Meaning {
            key: key(identity),
            modifiers,
            text,
        }
    }
}

/// The character of a key whose two keysyms are `plain` and `shifted`.
fn typed(plain: u32, shifted: u32, shift: bool, lock: bool) -> Option<char> {
    let first = character(plain);
    if shifted == NO_SYMBOL {
        // One keysym stands for both cases of a letter.
        let c = first?;
        return Some(if shift != lock { upper(c) } else { c });
    }
    let second = character(shifted);
    // Caps Lock turns letters over, and nothing else.
    let letter = matches!((first, second), (Some(a), Some(b)) if a != b && upper(a) == b);
    if shift != (lock && letter) {
        second
    } else {
        first
    }
}

/// The upper case of `c`, where it is a single character.
fn upper(c: char) -> char {
    single(c.to_uppercase()).unwrap_or(c)
}

fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
    match (chars.next(), chars.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Whether `keysym` is one of the numeric keypad's.
fn is_keypad(keysym: u32) -> bool {
    (0xFF80..=0xFFBD).contains(&keysym)
}

/// The character a keysym stands for: the Latin-1 keysyms, the Unicode
/// ones, the euro sign and the keypad's characters. The older keysyms of
/// other alphabets give none.
fn character(keysym: u32) -> Option<char> {
    match keysym {
        0x20..=0x7E | 0xA0..=0xFF => char::from_u32(keysym),
        0x0100_0100..=0x0110_FFFF => char::from_u32(keysym - 0x0100_0000),
        0x20AC => Some('\u{20AC}'),
        // The keypad's space, its operators and digits, and its equals
        // sign are ASCII above 0xFF80.
        0xFF80 | 0xFFAA..=0xFFB9 | 0xFFBD => char::from_u32(keysym - 0xFF80),
        _ => None,
    }
}

/// The key a keysym names.
fn key(keysym: u32) -> Key {
    match keysym {
        0xFF08 => Key::Backspace,
        // Tab, Shift+Tab (`ISO_Left_Tab`) and the keypad's.
        0xFF09 | 0xFE20 | 0xFF89 => Key::Tab,
        0xFF0D | 0xFF8D => Key::Enter,
        0xFF1B => Key::Escape,
        0xFF50 | 0xFF95 => Key::Home,
        0xFF51 | 0xFF96 => Key::Left,
        0xFF52 | 0xFF97 => Key::Up,
        0xFF53 | 0xFF98 => Key::Right,
        0xFF54 | 0xFF99 => Key::Down,
        0xFF55 | 0xFF9A => Key::PageUp,
        0xFF56 | 0xFF9B => Key::PageDown,
        0xFF57 | 0xFF9C => Key::End,
        0xFF63 | 0xFF9E => Key::Insert,
        0xFFFF | 0xFF9F => Key::Delete,
        // The keypad's F1 to F4.
        0xFF91..=0xFF94 => Key::Function((keysym - 0xFF91 + 1) as u8),
        // F1 to F24.
        0xFFBE..=0xFFD5 => Key::Function((keysym - 0xFFBE + 1) as u8),
        _ => match character(keysym) {
            Some(c) if !c.is_control() => Key::Character(single(c.to_lowercase()).unwrap_or(c)),
            _ => Key::Other,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NUM: u16 = 16;
    const ALTGR: u16 = 128;

    /// A small map laid out as an XKB server lays one out: columns for two
    /// layouts of two levels each, then levels 3 and 4 of both.
    fn keyboard() -> Keyboard {
        let rows: &[[u32; 6]] = &[
            // 10: a letter, with a Cyrillic second layout and AltGr.
            [0x61, 0x41, 0x0100_0444, 0x0100_0424, 0xE6, 0xC6],
            // 11: a digit with a symbol.
            [0x32, 0x40, 0x32, 0x22, 0xB2, 0],
            // 12 to 18: the modifiers.
            [SHIFT_L, 0, 0, 0, 0, 0],
            [CONTROL_L, 0, 0, 0, 0, 0],
            [ALT_L, META_L, 0, 0, 0, 0],
            [NUM_LOCK, 0, 0, 0, 0, 0],
            [SUPER_L, 0, 0, 0, 0, 0],
            [LEVEL3_SHIFT, 0, 0, 0, 0, 0],
            [MODE_SWITCH, 0, 0, 0, 0, 0],
            // 19: the keypad's 7.
            [0xFF95, 0xFFB7, 0, 0, 0, 0],
            // 20: Return. 21: a letter with one keysym only.
            [0xFF0D, 0, 0, 0, 0, 0],
            [0x7A, 0, 0, 0, 0, 0],
            // 22: a dead key. 23: an old Cyrillic keysym.
            [0xFE51, 0, 0, 0, 0, 0],
            [0x06C6, 0x06E6, 0, 0, 0, 0],
        ];
        let keysyms = rows.iter().flatten().copied().collect();
        let modifiers = [
            [12, 0],
            [0, 0],
            [13, 0],
            [14, 0],
            [15, 0],
            [0, 0],
            [16, 0],
            [17, 18],
        ]
        .concat();
        Keyboard::new(10, 6, keysyms, &modifiers)
    }

    fn text(code: u8, state: u16) -> Option<char> {
        keyboard().meaning(code, state, true).text
    }

    fn key_of(code: u8, state: u16) -> Key {
        keyboard().meaning(code, state, true).key
    }

    #[test]
    fn shift_and_caps_lock_pick_the_level() {
        assert_eq!(text(10, 0), Some('a'));
        assert_eq!(text(10, SHIFT), Some('A'));
        assert_eq!(text(10, LOCK), Some('A'));
        assert_eq!(text(10, LOCK | SHIFT), Some('a'));
        assert_eq!(text(11, 0), Some('2'));
        assert_eq!(text(11, SHIFT), Some('@'));
        // Caps Lock leaves what is no letter alone.
        assert_eq!(text(11, LOCK), Some('2'));
        assert_eq!(text(11, LOCK | SHIFT), Some('@'));
        // One keysym for both cases.
        assert_eq!(text(21, 0), Some('z'));
        assert_eq!(text(21, SHIFT), Some('Z'));
        assert_eq!(text(21, LOCK), Some('Z'));
        // The key is the unshifted character, whatever is held.
        assert_eq!(key_of(11, SHIFT), Key::Character('2'));
        assert_eq!(key_of(10, SHIFT | LOCK), Key::Character('a'));
    }

    #[test]
    fn the_second_layout_and_level_three_are_read() {
        let second = 0x2000;
        assert_eq!(text(10, second), Some('\u{444}'));
        assert_eq!(text(10, second | SHIFT), Some('\u{424}'));
        assert_eq!(key_of(10, second), Key::Character('\u{444}'));
        assert_eq!(text(10, ALTGR), Some('\u{e6}'));
        assert_eq!(text(10, ALTGR | SHIFT), Some('\u{c6}'));
        // A key without a third level types what it types without AltGr,
        // and one without a second layout stays in the first.
        assert_eq!(text(21, ALTGR), Some('z'));
        assert_eq!(text(21, second), Some('z'));
        // The Mode_switch on the level 3 modifier does not switch layouts.
        assert_eq!(key_of(10, ALTGR), Key::Character('a'));
    }

    #[test]
    fn shortcuts_releases_and_keys_without_characters_type_nothing() {
        assert_eq!(text(10, CONTROL), None);
        assert_eq!(text(10, MOD1), None);
        assert_eq!(text(10, MOD4), None);
        assert_eq!(keyboard().meaning(10, 0, false).text, None);
        assert_eq!(text(20, 0), None);
        assert_eq!(key_of(20, 0), Key::Enter);
        // Dead keys and the old keysyms of other alphabets.
        assert_eq!((key_of(22, 0), text(22, 0)), (Key::Other, None));
        assert_eq!((key_of(23, 0), text(23, 0)), (Key::Other, None));
        // Keycodes outside the map.
        for code in [0, 9, 24, 255] {
            assert_eq!((key_of(code, 0), text(code, 0)), (Key::Other, None));
        }
    }

    #[test]
    fn num_lock_picks_the_keypads_digits_and_shift_undoes_it() {
        assert_eq!((key_of(19, 0), text(19, 0)), (Key::Home, None));
        assert_eq!(
            (key_of(19, NUM), text(19, NUM)),
            (Key::Character('7'), Some('7'))
        );
        assert_eq!(key_of(19, NUM | SHIFT), Key::Home);
        assert_eq!(key_of(19, SHIFT), Key::Character('7'));
    }

    #[test]
    fn modifiers_come_from_the_state_and_the_key_itself() {
        let keyboard = keyboard();
        assert_eq!(keyboard.modifiers(0), Modifiers::NONE);
        assert_eq!(
            keyboard.modifiers(SHIFT | CONTROL | MOD1 | MOD4),
            Modifiers {
                shift: true,
                control: true,
                alt: true,
                meta: true
            }
        );
        // Num Lock and AltGr are none of the four.
        assert_eq!(keyboard.modifiers(NUM | ALTGR | LOCK), Modifiers::NONE);
        assert!(keyboard.meaning(12, 0, true).modifiers.shift);
        assert!(!keyboard.meaning(12, SHIFT, false).modifiers.shift);
        assert!(keyboard.meaning(13, 0, true).modifiers.control);
        assert!(keyboard.meaning(14, 0, true).modifiers.alt);
        assert!(keyboard.meaning(16, 0, true).modifiers.meta);
        assert_eq!(keyboard.meaning(12, 0, true).key, Key::Other);
    }

    #[test]
    fn named_keys_and_characters_of_keysyms() {
        for (keysym, expected) in [
            (0xFF08, Key::Backspace),
            (0xFE20, Key::Tab),
            (0xFF8D, Key::Enter),
            (0xFF1B, Key::Escape),
            (0xFF55, Key::PageUp),
            (0xFFFF, Key::Delete),
            (0xFFBE, Key::Function(1)),
            (0xFFD5, Key::Function(24)),
            (0xFF91, Key::Function(1)),
            (0x20, Key::Character(' ')),
            (0x41, Key::Character('a')),
            (0xC9, Key::Character('\u{e9}')),
            (0x20AC, Key::Character('\u{20ac}')),
            (0xFFAB, Key::Character('+')),
            (0xFFD6, Key::Other),
            (0, Key::Other),
        ] {
            assert_eq!(key(keysym), expected, "{keysym:#x}");
        }
        assert_eq!(character(0x0101_F600), Some('\u{1F600}'));
        // A surrogate is no character.
        assert_eq!(character(0x0100_D800), None);
    }

    #[test]
    fn an_empty_or_short_map_means_nothing() {
        let empty = Keyboard::new(8, 0, Vec::new(), &[]);
        let meaning = empty.meaning(38, SHIFT, true);
        assert_eq!((meaning.key, meaning.text), (Key::Other, None));
        let short = Keyboard::new(8, 4, vec![0x61], &[1, 2, 3]);
        assert_eq!(short.meaning(8, 0, true).text, Some('a'));
        assert_eq!(short.meaning(200, 0, true).text, None);
    }
}
