//! Pointer bookkeeping: held buttons, capture, leaving, and click counts.

use crate::event::{Button, Modifiers, PhysicalPoint, PhysicalSize};

/// When two presses count as one double click.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClickSettings {
    /// The longest time between presses, in milliseconds.
    pub(crate) interval_ms: u32,
    /// How far the second press may be from the first, across.
    pub(crate) half_width: i32,
    /// How far the second press may be from the first, down.
    pub(crate) half_height: i32,
}

impl ClickSettings {
    /// Windows' defaults: 500 ms and a 4 by 4 pixel box.
    pub(crate) const DEFAULT: ClickSettings = ClickSettings {
        interval_ms: 500,
        half_width: 2,
        half_height: 2,
    };
}

/// Counts quick presses of one button in one place.
#[derive(Debug, Default)]
pub(crate) struct ClickCounter {
    last: Option<(Button, PhysicalPoint, u32)>,
    count: u32,
}

impl ClickCounter {
    /// Records a press and returns its count: 1 for a click, 2 for a
    /// double click, and so on.
    pub(crate) fn press(
        &mut self,
        button: Button,
        at: PhysicalPoint,
        time_ms: u32,
        settings: ClickSettings,
    ) -> u32 {
        let follows = self.last.is_some_and(|(last_button, last_at, last_time)| {
            last_button == button
                && time_ms.wrapping_sub(last_time) <= settings.interval_ms
                && (i64::from(at.x) - i64::from(last_at.x)).abs() <= i64::from(settings.half_width)
                && (i64::from(at.y) - i64::from(last_at.y)).abs() <= i64::from(settings.half_height)
        });
        self.count = if follows {
            self.count.saturating_add(1)
        } else {
            1
        };
        self.last = Some((button, at, time_ms));
        self.count
    }

    /// Forgets the last press, so the next one counts 1.
    pub(crate) fn reset(&mut self) {
        self.last = None;
        self.count = 0;
    }
}

/// What the window knows about the pointer.
#[derive(Debug, Default)]
pub(crate) struct PointerState {
    /// Buttons that went down in this window and have not come up.
    held: u8,
    /// Whether the window holds the pointer.
    capturing: bool,
    /// Whether the pointer is over the window, as far as the window knows.
    inside: bool,
    /// The last move reported, to drop the repeats some systems send.
    last_move: Option<(PhysicalPoint, Modifiers)>,
    clicks: ClickCounter,
}

/// What the window must do after a button went down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Down {
    /// Take the pointer: this is the first button held.
    pub(crate) capture: bool,
    /// Ask to hear when the pointer leaves: it was not known to be inside.
    pub(crate) track_leave: bool,
    /// The click count to report.
    pub(crate) clicks: u32,
}

/// What the window must do after a button came up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Up {
    /// The button was not held here: report nothing.
    Ignore,
    /// Report the up; then release the pointer if `release`, and report
    /// that the pointer left if `left`.
    Report {
        /// Every button is up: let the pointer go.
        release: bool,
        /// The drag ended outside the window.
        left: bool,
    },
}

impl PointerState {
    /// A button went down at `at`.
    pub(crate) fn down(
        &mut self,
        button: Button,
        at: PhysicalPoint,
        time_ms: u32,
        settings: ClickSettings,
    ) -> Down {
        let capture = self.held == 0 && !self.capturing;
        self.held |= button.bit();
        if capture {
            self.capturing = true;
        }
        let track_leave = !self.inside;
        self.inside = true;
        self.last_move = None;
        let clicks = self.clicks.press(button, at, time_ms, settings);
        Down {
            capture,
            track_leave,
            clicks,
        }
    }

    /// A button came up at `at`, in a window of `size`.
    pub(crate) fn up(&mut self, button: Button, at: PhysicalPoint, size: PhysicalSize) -> Up {
        if self.held & button.bit() == 0 {
            return Up::Ignore;
        }
        self.held &= !button.bit();
        self.last_move = None;
        if self.held != 0 {
            return Up::Report {
                release: false,
                left: false,
            };
        }
        let release = self.capturing;
        self.capturing = false;
        let outside = !size.bounds().contains(i64::from(at.x), i64::from(at.y));
        let left = outside && self.inside;
        if left {
            self.inside = false;
            self.clicks.reset();
        }
        Up::Report { release, left }
    }

    /// The pointer moved. Returns `None` for a repeat of the last move, or
    /// whether to ask to hear when the pointer leaves.
    pub(crate) fn moved(&mut self, at: PhysicalPoint, modifiers: Modifiers) -> Option<bool> {
        if self.last_move == Some((at, modifiers)) {
            return None;
        }
        self.last_move = Some((at, modifiers));
        let track_leave = !self.inside;
        self.inside = true;
        Some(track_leave)
    }

    /// The system says the pointer left. Returns whether to report it: not
    /// while a drag holds the pointer (its end reports it) and not twice.
    pub(crate) fn left(&mut self) -> bool {
        if self.capturing || !self.inside {
            return false;
        }
        self.inside = false;
        self.last_move = None;
        self.clicks.reset();
        true
    }

    /// The window lost the pointer. Returns whether a drag was cancelled.
    pub(crate) fn capture_lost(&mut self) -> bool {
        if !self.capturing {
            return false;
        }
        self.capturing = false;
        self.held = 0;
        self.last_move = None;
        self.clicks.reset();
        true
    }

    /// Whether the window holds the pointer.
    pub(crate) fn capturing(&self) -> bool {
        self.capturing
    }

    /// Forgets everything, as when the window closes.
    pub(crate) fn reset(&mut self) {
        *self = PointerState::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AT: PhysicalPoint = PhysicalPoint::new(10, 10);
    const SIZE: PhysicalSize = PhysicalSize::new(100, 50);

    #[test]
    fn quick_presses_in_one_place_count_up() {
        let mut counter = ClickCounter::default();
        let s = ClickSettings::DEFAULT;
        assert_eq!(counter.press(Button::Primary, AT, 1000, s), 1);
        assert_eq!(
            counter.press(Button::Primary, PhysicalPoint::new(12, 8), 1500, s),
            2
        );
        assert_eq!(counter.press(Button::Primary, AT, 1600, s), 3);
        // Too late.
        assert_eq!(counter.press(Button::Primary, AT, 2101, s), 1);
        // Too far.
        assert_eq!(
            counter.press(Button::Primary, PhysicalPoint::new(13, 10), 2200, s),
            1
        );
        // Another button.
        assert_eq!(
            counter.press(Button::Secondary, PhysicalPoint::new(13, 10), 2300, s),
            1
        );
    }

    #[test]
    fn click_time_may_wrap_around() {
        let mut counter = ClickCounter::default();
        let s = ClickSettings::DEFAULT;
        assert_eq!(counter.press(Button::Primary, AT, u32::MAX - 100, s), 1);
        assert_eq!(counter.press(Button::Primary, AT, 200, s), 2);
    }

    #[test]
    fn extreme_positions_do_not_overflow() {
        let mut counter = ClickCounter::default();
        let s = ClickSettings::DEFAULT;
        counter.press(
            Button::Primary,
            PhysicalPoint::new(i32::MIN, i32::MIN),
            0,
            s,
        );
        assert_eq!(
            counter.press(
                Button::Primary,
                PhysicalPoint::new(i32::MAX, i32::MAX),
                1,
                s
            ),
            1
        );
    }

    #[test]
    fn the_first_button_takes_the_pointer_and_the_last_lets_it_go() {
        let mut state = PointerState::default();
        let s = ClickSettings::DEFAULT;
        let first = state.down(Button::Primary, AT, 0, s);
        assert!(first.capture && first.track_leave);
        let second = state.down(Button::Secondary, AT, 10, s);
        assert!(!second.capture && !second.track_leave);
        assert_eq!(
            state.up(Button::Primary, AT, SIZE),
            Up::Report {
                release: false,
                left: false
            }
        );
        assert!(state.capturing());
        assert_eq!(
            state.up(Button::Secondary, AT, SIZE),
            Up::Report {
                release: true,
                left: false
            }
        );
        assert!(!state.capturing());
    }

    #[test]
    fn an_up_without_its_down_is_ignored() {
        let mut state = PointerState::default();
        assert_eq!(state.up(Button::Primary, AT, SIZE), Up::Ignore);
        state.down(Button::Primary, AT, 0, ClickSettings::DEFAULT);
        assert_eq!(state.up(Button::Middle, AT, SIZE), Up::Ignore);
    }

    #[test]
    fn a_drag_that_ends_outside_reports_leaving_once() {
        let mut state = PointerState::default();
        state.down(Button::Primary, AT, 0, ClickSettings::DEFAULT);
        // The system's leave notice during the drag is held back.
        assert!(!state.left());
        assert_eq!(
            state.up(Button::Primary, PhysicalPoint::new(-3, 20), SIZE),
            Up::Report {
                release: true,
                left: true
            }
        );
        assert!(!state.left());
    }

    #[test]
    fn losing_the_pointer_cancels_a_drag_once() {
        let mut state = PointerState::default();
        assert!(!state.capture_lost());
        state.down(Button::Primary, AT, 0, ClickSettings::DEFAULT);
        assert!(state.capture_lost());
        assert!(!state.capture_lost());
        assert_eq!(state.up(Button::Primary, AT, SIZE), Up::Ignore);
    }

    #[test]
    fn repeated_moves_are_dropped() {
        let mut state = PointerState::default();
        assert_eq!(state.moved(AT, Modifiers::NONE), Some(true));
        assert_eq!(state.moved(AT, Modifiers::NONE), None);
        let shift = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        assert_eq!(state.moved(AT, shift), Some(false));
        assert!(state.left());
        assert_eq!(state.moved(AT, shift), Some(true));
    }
}
