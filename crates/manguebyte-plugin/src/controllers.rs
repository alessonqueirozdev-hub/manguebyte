//! Which MIDI controllers a plugin listens to.
//!
//! See [`Plugin::MIDI_CONTROLLERS`](crate::Plugin::MIDI_CONTROLLERS) for
//! what the formats do with the declaration.

use core::fmt;

use manguebyte_midi::MidiMessage;

/// One MIDI message a plugin can take as a controller: a control change
/// number, channel pressure or the pitch wheel.
///
/// Each has an [`index`](Self::index) from 0 to 129: control changes keep
/// their number, channel pressure is 128 and pitch bend 129. VST 3 numbers
/// them the same way. Only those 130 can be built, so every value of this
/// type is a real controller.
///
/// ```
/// use manguebyte_plugin::MidiController;
///
/// const SUSTAIN: MidiController = MidiController::control(64).unwrap();
/// assert_eq!(SUSTAIN.control_number(), Some(64));
/// assert_eq!(MidiController::PITCH_BEND.control_number(), None);
/// // 128 is not a control change number.
/// assert_eq!(MidiController::control(128), None);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MidiController {
    /// The index, always below [`MidiController::COUNT`].
    index: u8,
}

impl MidiController {
    /// How many controllers there are: 128 control changes, channel
    /// pressure and pitch bend.
    pub const COUNT: usize = 130;

    /// Channel pressure, also called channel aftertouch. Index 128.
    pub const CHANNEL_PRESSURE: Self = Self { index: 128 };

    /// The pitch wheel. Index 129.
    pub const PITCH_BEND: Self = Self { index: 129 };

    /// Control change `number`, or `None` above 127. Numbers 120 to 127
    /// are the channel mode messages, such as all sound off (120), reset
    /// all controllers (121) and all notes off (123).
    ///
    /// In a `const`, `MidiController::control(74).unwrap()` turns a
    /// number above 127 into a compile error.
    pub const fn control(number: u8) -> Option<Self> {
        if number < 128 {
            Some(Self { index: number })
        } else {
            None
        }
    }

    /// The control change number, or `None` for channel pressure and
    /// pitch bend.
    pub const fn control_number(self) -> Option<u8> {
        if self.index < 128 {
            Some(self.index)
        } else {
            None
        }
    }

    /// The controller's index, from 0 to 129.
    ///
    /// ```
    /// use manguebyte_plugin::MidiController;
    ///
    /// assert_eq!(MidiController::control(64).unwrap().index(), 64);
    /// assert_eq!(MidiController::CHANNEL_PRESSURE.index(), 128);
    /// assert_eq!(MidiController::PITCH_BEND.index(), 129);
    /// ```
    pub const fn index(self) -> usize {
        self.index as usize
    }

    /// The controller with this [`index`](Self::index), or `None` from 130
    /// up.
    pub const fn from_index(index: usize) -> Option<Self> {
        if index < Self::COUNT {
            Some(Self { index: index as u8 })
        } else {
            None
        }
    }

    /// The controller `message` moves, and the channel it moves it on, or
    /// `None` for a message that moves no controller, such as a note.
    ///
    /// ```
    /// use manguebyte_midi::MidiMessage;
    /// use manguebyte_plugin::MidiController;
    ///
    /// let pedal = MidiMessage::ControlChange { channel: 3, controller: 64, value: 127 };
    /// assert_eq!(MidiController::of(&pedal), Some((3, MidiController::control(64).unwrap())));
    /// let note = MidiMessage::NoteOn { channel: 0, note: 60, velocity: 100 };
    /// assert_eq!(MidiController::of(&note), None);
    /// ```
    pub fn of(message: &MidiMessage) -> Option<(u8, Self)> {
        match *message {
            MidiMessage::ControlChange {
                channel,
                controller,
                ..
            } => Some((channel, Self::control(controller)?)),
            MidiMessage::ChannelPressure { channel, .. } => Some((channel, Self::CHANNEL_PRESSURE)),
            MidiMessage::PitchBend { channel, .. } => Some((channel, Self::PITCH_BEND)),
            _ => None,
        }
    }
}

impl fmt::Debug for MidiController {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.control_number() {
            Some(number) => write!(f, "MidiController::control({number})"),
            None if *self == Self::PITCH_BEND => f.write_str("MidiController::PITCH_BEND"),
            None => f.write_str("MidiController::CHANNEL_PRESSURE"),
        }
    }
}

/// The MIDI controllers a plugin listens to, and whether their channel
/// matters to it.
///
/// A set is built in a `const`, so a plugin states it once, as
/// [`Plugin::MIDI_CONTROLLERS`](crate::Plugin::MIDI_CONTROLLERS):
///
/// ```
/// use manguebyte_plugin::{MidiController, MidiControllers};
///
/// const CUTOFF: MidiController = MidiController::control(74).unwrap();
///
/// // The musical set, plus the filter cutoff that many keyboards send.
/// const MINE: MidiControllers = MidiControllers::MUSICAL.with(CUTOFF);
/// assert_eq!(MINE.len(), 14);
/// assert!(MINE.contains(CUTOFF));
/// assert!(!MINE.is_per_channel());
///
/// // Exactly these three, kept apart per channel, as an MPE instrument
/// // wants them.
/// const MPE: MidiControllers = MidiControllers::list(&[
///     MidiController::PITCH_BEND,
///     MidiController::CHANNEL_PRESSURE,
///     CUTOFF,
/// ])
/// .per_channel();
/// assert_eq!(MPE.len(), 3);
/// assert!(MPE.is_per_channel());
/// ```
///
/// Controllers are always visited in the order of their
/// [`index`](MidiController::index), whatever order they were listed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MidiControllers {
    /// One bit per controller index.
    bits: [u64; 3],
    per_channel: bool,
}

/// Control change `number` in a `const` where the number is known to be
/// valid.
const fn cc(number: u8) -> MidiController {
    match MidiController::control(number) {
        Some(controller) => controller,
        None => panic!("not a control change number"),
    }
}

impl MidiControllers {
    /// No controllers at all.
    pub const NONE: Self = Self {
        bits: [0; 3],
        per_channel: false,
    };

    /// What an instrument played from a keyboard usually needs, and the
    /// default: pitch bend, channel pressure, modulation (1), breath (2),
    /// volume (7), pan (10), expression (11), the sustain (64), sostenuto
    /// (66) and soft (67) pedals, and the channel mode messages all sound
    /// off (120), reset all controllers (121) and all notes off (123).
    pub const MUSICAL: Self = Self::list(&[
        MidiController::PITCH_BEND,
        MidiController::CHANNEL_PRESSURE,
        cc(1),
        cc(2),
        cc(7),
        cc(10),
        cc(11),
        cc(64),
        cc(66),
        cc(67),
        cc(120),
        cc(121),
        cc(123),
    ]);

    /// All 130 controllers. With [`per_channel`](Self::per_channel), this
    /// is 2,080 parameters in VST 3.
    pub const ALL: Self = Self {
        bits: [u64::MAX, u64::MAX, 0b11],
        per_channel: false,
    };

    /// Exactly the controllers in `controllers`, for all channels together.
    /// Repeats are ignored.
    pub const fn list(controllers: &[MidiController]) -> Self {
        let mut set = Self::NONE;
        let mut index = 0;
        while index < controllers.len() {
            set = set.with(controllers[index]);
            index += 1;
        }
        set
    }

    /// This set with `controller` added.
    pub const fn with(mut self, controller: MidiController) -> Self {
        let index = controller.index();
        self.bits[index / 64] |= 1 << (index % 64);
        self
    }

    /// This set without `controller`.
    pub const fn without(mut self, controller: MidiController) -> Self {
        let index = controller.index();
        self.bits[index / 64] &= !(1 << (index % 64));
        self
    }

    /// This set, with each controller kept apart per MIDI channel.
    ///
    /// Only a plugin that treats channels differently needs this, such as
    /// an MPE instrument, where each note has a channel of its own and the
    /// pitch wheel of one channel bends one note. In VST 3 it multiplies
    /// the number of hidden parameters by 16.
    pub const fn per_channel(mut self) -> Self {
        self.per_channel = true;
        self
    }

    /// Whether the plugin asked for each controller per channel.
    pub const fn is_per_channel(&self) -> bool {
        self.per_channel
    }

    /// Whether `controller` is in the set.
    pub const fn contains(&self, controller: MidiController) -> bool {
        let index = controller.index();
        self.bits[index / 64] & (1 << (index % 64)) != 0
    }

    /// How many controllers are in the set, not counting channels.
    pub const fn len(&self) -> usize {
        (self.bits[0].count_ones() + self.bits[1].count_ones() + self.bits[2].count_ones()) as usize
    }

    /// Whether the set has no controllers.
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where `controller` is among the controllers of the set, counting in
    /// the order of their index, or `None` if it is not in the set.
    ///
    /// ```
    /// use manguebyte_plugin::{MidiController, MidiControllers};
    ///
    /// let set = MidiControllers::MUSICAL;
    /// let cc = |number| MidiController::control(number).unwrap();
    /// assert_eq!(set.position(cc(1)), Some(0));
    /// assert_eq!(set.position(cc(64)), Some(5));
    /// assert_eq!(set.position(MidiController::PITCH_BEND), Some(12));
    /// assert_eq!(set.position(cc(74)), None);
    /// ```
    pub fn position(&self, controller: MidiController) -> Option<usize> {
        if !self.contains(controller) {
            return None;
        }
        let index = controller.index();
        let (word, bit) = (index / 64, index % 64);
        let before: u32 = self.bits[..word].iter().map(|bits| bits.count_ones()).sum();
        let below = self.bits[word] & ((1u64 << bit) - 1);
        Some((before + below.count_ones()) as usize)
    }

    /// The controller at `position` in the order of [`iter`](Self::iter),
    /// or `None` past the end. The inverse of [`position`](Self::position).
    pub fn get(&self, position: usize) -> Option<MidiController> {
        let mut left = position;
        for (word, &bits) in self.bits.iter().enumerate() {
            let count = bits.count_ones() as usize;
            if left >= count {
                left -= count;
                continue;
            }
            let mut rest = bits;
            for _ in 0..left {
                // Drop the lowest set bit.
                rest &= rest - 1;
            }
            return MidiController::from_index(word * 64 + rest.trailing_zeros() as usize);
        }
        None
    }

    /// The controllers of the set, in the order of their index.
    pub fn iter(&self) -> impl Iterator<Item = MidiController> + use<> {
        let set = *self;
        (0..MidiController::COUNT)
            .filter_map(MidiController::from_index)
            .filter(move |&controller| set.contains(controller))
    }

    /// Whether the plugin wants `message`: `false` only for a message that
    /// moves a controller outside the set. Notes and program changes are
    /// always wanted.
    ///
    /// The VST 3 wrapper can only deliver controllers in the set. Formats
    /// that carry MIDI as it is, such as CLAP, deliver everything; a plugin
    /// can use this to ignore the rest in the same way everywhere.
    ///
    /// ```
    /// use manguebyte_midi::MidiMessage;
    /// use manguebyte_plugin::MidiControllers;
    ///
    /// let cutoff = MidiMessage::ControlChange { channel: 0, controller: 74, value: 9 };
    /// let pedal = MidiMessage::ControlChange { channel: 0, controller: 64, value: 127 };
    /// assert!(!MidiControllers::MUSICAL.wants(&cutoff));
    /// assert!(MidiControllers::MUSICAL.wants(&pedal));
    /// ```
    pub fn wants(&self, message: &MidiMessage) -> bool {
        match MidiController::of(message) {
            Some((_, controller)) => self.contains(controller),
            None => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MidiController, MidiControllers};
    use manguebyte_midi::MidiMessage;

    fn cc(number: u8) -> MidiController {
        MidiController::control(number).unwrap()
    }

    /// Every set worth checking: the named ones, single controllers at the
    /// edges of each word, and pseudo-random ones.
    fn sets() -> Vec<MidiControllers> {
        let mut sets = vec![
            MidiControllers::NONE,
            MidiControllers::MUSICAL,
            MidiControllers::ALL,
            MidiControllers::ALL.per_channel(),
        ];
        for index in [0, 1, 63, 64, 65, 127, 128, 129] {
            let controller = MidiController::from_index(index).unwrap();
            sets.push(MidiControllers::NONE.with(controller));
            sets.push(MidiControllers::ALL.without(controller));
        }
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        // Miri interprets every step, so it gets fewer.
        let count = if cfg!(miri) { 12 } else { 200 };
        for _ in 0..count {
            let mut set = MidiControllers::NONE;
            for index in 0..MidiController::COUNT {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state % 3 == 0 {
                    set = set.with(MidiController::from_index(index).unwrap());
                }
            }
            sets.push(set);
        }
        sets
    }

    #[test]
    fn indices_go_both_ways_and_stop_at_130() {
        for index in 0..MidiController::COUNT {
            let controller = MidiController::from_index(index).unwrap();
            assert_eq!(controller.index(), index);
        }
        assert_eq!(MidiController::from_index(130), None);
        assert_eq!(MidiController::from_index(usize::MAX), None);
        assert_eq!(MidiController::CHANNEL_PRESSURE.index(), 128);
        assert_eq!(MidiController::PITCH_BEND.index(), 129);
    }

    #[test]
    fn only_numbers_up_to_127_are_control_changes() {
        for number in 0..=127u8 {
            let controller = cc(number);
            assert_eq!(controller.index(), usize::from(number));
            assert_eq!(controller.control_number(), Some(number));
        }
        for number in 128..=255u8 {
            assert_eq!(MidiController::control(number), None, "{number}");
        }
        assert_eq!(MidiController::CHANNEL_PRESSURE.control_number(), None);
        assert_eq!(MidiController::PITCH_BEND.control_number(), None);
        // The three kinds are told apart, and print as they are written.
        assert_eq!(format!("{:?}", cc(74)), "MidiController::control(74)");
        assert_eq!(
            format!("{:?}", MidiController::PITCH_BEND),
            "MidiController::PITCH_BEND"
        );
        assert_eq!(
            format!("{:?}", MidiController::CHANNEL_PRESSURE),
            "MidiController::CHANNEL_PRESSURE"
        );
        // The order is the order of the index.
        assert!(cc(127) < MidiController::CHANNEL_PRESSURE);
        assert!(MidiController::CHANNEL_PRESSURE < MidiController::PITCH_BEND);
    }

    #[test]
    fn the_musical_set_is_the_documented_one() {
        let numbers: Vec<usize> = MidiControllers::MUSICAL
            .iter()
            .map(MidiController::index)
            .collect();
        assert_eq!(
            numbers,
            [1, 2, 7, 10, 11, 64, 66, 67, 120, 121, 123, 128, 129]
        );
        assert!(!MidiControllers::MUSICAL.is_per_channel());
        assert_eq!(MidiControllers::ALL.len(), 130);
        assert_eq!(MidiControllers::NONE.len(), 0);
        assert!(MidiControllers::NONE.is_empty());
    }

    #[test]
    fn position_get_and_iter_agree_for_every_set() {
        for set in sets() {
            let listed: Vec<MidiController> = set.iter().collect();
            assert_eq!(listed.len(), set.len());
            for (position, &controller) in listed.iter().enumerate() {
                assert_eq!(set.get(position), Some(controller));
                assert_eq!(set.position(controller), Some(position));
            }
            assert_eq!(set.get(set.len()), None);
            assert_eq!(set.get(usize::MAX), None);
            for index in 0..MidiController::COUNT {
                let controller = MidiController::from_index(index).unwrap();
                assert_eq!(set.contains(controller), listed.contains(&controller));
                if !set.contains(controller) {
                    assert_eq!(set.position(controller), None);
                }
                // Adding and removing touch that controller alone.
                let added = set.with(controller);
                let removed = set.without(controller);
                assert!(added.contains(controller) && !removed.contains(controller));
                assert_eq!(added.without(controller), removed);
                assert_eq!(added.is_per_channel(), set.is_per_channel());
            }
        }
    }

    #[test]
    fn a_list_ignores_order_and_repeats() {
        let a = MidiControllers::list(&[MidiController::PITCH_BEND, cc(7), cc(7)]);
        let b = MidiControllers::list(&[cc(7), MidiController::PITCH_BEND]);
        assert_eq!(a, b);
        assert_eq!(a.len(), 2);
        assert_eq!(a.per_channel().len(), 2);
        assert_ne!(a, a.per_channel());
    }

    #[test]
    fn messages_are_wanted_unless_they_move_a_controller_outside_the_set() {
        let set = MidiControllers::list(&[cc(64)]);
        let message = |controller| MidiMessage::ControlChange {
            channel: 9,
            controller,
            value: 1,
        };
        assert_eq!(MidiController::of(&message(64)), Some((9, cc(64))));
        assert!(set.wants(&message(64)));
        assert!(!set.wants(&message(65)));
        assert!(!set.wants(&MidiMessage::PitchBend {
            channel: 0,
            value: 0
        }));
        assert!(!set.wants(&MidiMessage::ChannelPressure {
            channel: 0,
            pressure: 3
        }));
        // Not a valid controller number, so not a controller.
        assert_eq!(MidiController::of(&message(200)), None);
        for message in [
            MidiMessage::NoteOn {
                channel: 0,
                note: 1,
                velocity: 2,
            },
            MidiMessage::NoteOff {
                channel: 0,
                note: 1,
                velocity: 2,
            },
            MidiMessage::PolyPressure {
                channel: 0,
                note: 1,
                pressure: 2,
            },
            MidiMessage::ProgramChange {
                channel: 0,
                program: 4,
            },
            message(200),
        ] {
            assert!(MidiControllers::NONE.wants(&message), "{message:?}");
        }
    }
}
