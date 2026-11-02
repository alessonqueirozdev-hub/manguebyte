//! Controller values, as voices read them: each one moves to a new value
//! along a short straight line, so that a jump never clicks.

/// How many controllers a [`Sampler`](crate::Sampler) keeps: 0 to 127 are
/// MIDI's, and the numbers above them are SFZ 2's extended controllers,
/// which no MIDI controller message reaches. See the
/// [crate documentation](crate#controllers).
pub const MAX_CONTROLLERS: usize = 512;

/// Seconds a controller takes to reach a new value, along a straight line.
pub const CONTROLLER_SMOOTHING_SECONDS: f64 = 0.01;

/// What a voice reads controllers from.
pub(crate) trait Controls {
    /// Controller `controller`'s value at frame `frame` of the sampler's
    /// clock, 0 to 1.
    fn value(&self, controller: u16, frame: u64) -> f64;
}

/// Fixed values, one per controller: an instrument's initial values.
impl Controls for [f64] {
    fn value(&self, controller: u16, _frame: u64) -> f64 {
        self.get(usize::from(controller)).copied().unwrap_or(0.0)
    }
}

/// One controller: the straight line it is on.
#[derive(Debug, Clone, Copy)]
struct Ramp {
    from: f64,
    to: f64,
    /// The frame the line starts on.
    start: u64,
    /// Set by a MIDI message or by the plugin since the last reset, so a
    /// new instrument's initial value does not replace it.
    touched: bool,
}

const AT_REST: Ramp = Ramp {
    from: 0.0,
    to: 0.0,
    start: 0,
    touched: false,
};

/// The controllers of a sampler, each read at any frame in closed form,
/// so the value a voice sees does not depend on how the timeline is cut
/// into blocks.
#[derive(Debug, Clone)]
pub(crate) struct ControllerTable {
    ramps: Box<[Ramp]>,
    /// Frames a ramp lasts, at least 1.
    frames: u64,
}

impl ControllerTable {
    pub(crate) fn new() -> Self {
        Self {
            ramps: vec![AT_REST; MAX_CONTROLLERS].into_boxed_slice(),
            frames: 480,
        }
    }

    /// Takes the sample rate, for the length of a ramp.
    pub(crate) fn set_sample_rate(&mut self, sample_rate: f64) {
        let frames = (CONTROLLER_SMOOTHING_SECONDS * sample_rate).round();
        self.frames = if frames.is_finite() && frames >= 1.0 {
            frames as u64
        } else {
            1
        };
    }

    /// Every controller at its initial value at once, none touched.
    pub(crate) fn reset(&mut self, initial: &[f64]) {
        for (number, ramp) in self.ramps.iter_mut().enumerate() {
            let value = initial.value(number as u16, 0);
            *ramp = Ramp {
                from: value,
                to: value,
                start: 0,
                touched: false,
            };
        }
    }

    /// Controllers no one has set take `initial`'s values: at once on
    /// `now` if `at_once`, and otherwise along a ramp from there.
    pub(crate) fn adopt(&mut self, initial: &[f64], now: u64, at_once: bool) {
        for number in 0..self.ramps.len() {
            if self.ramps[number].touched {
                continue;
            }
            let value = initial.value(number as u16, now);
            if at_once {
                self.ramps[number] = Ramp {
                    from: value,
                    to: value,
                    start: now,
                    touched: false,
                };
            } else {
                self.move_to(number, value, now);
            }
        }
    }

    /// Controller `controller` heads for `value` (0 to 1) from frame
    /// `now`, from wherever it is then. Marks it as set.
    pub(crate) fn set(&mut self, controller: u16, value: f64, now: u64) {
        let number = usize::from(controller);
        if number < self.ramps.len() && value.is_finite() {
            self.move_to(number, value.clamp(0.0, 1.0), now);
            self.ramps[number].touched = true;
        }
    }

    /// Controller `controller` is at `value` (0 to 1) from the start of the
    /// clock, with no ramp. Marks it as set.
    pub(crate) fn jump(&mut self, controller: u16, value: f64) {
        if let Some(ramp) = self.ramps.get_mut(usize::from(controller)) {
            if value.is_finite() {
                let value = value.clamp(0.0, 1.0);
                *ramp = Ramp {
                    from: value,
                    to: value,
                    start: 0,
                    touched: true,
                };
            }
        }
    }

    /// Controller `controller` heads back to its initial value from frame
    /// `now`, and counts as not set.
    pub(crate) fn restore(&mut self, controller: u16, initial: &[f64], now: u64) {
        let number = usize::from(controller);
        if number < self.ramps.len() {
            self.move_to(number, initial.value(controller, now), now);
            self.ramps[number].touched = false;
        }
    }

    fn move_to(&mut self, number: usize, value: f64, now: u64) {
        let from = self.at(number, now);
        let ramp = &mut self.ramps[number];
        ramp.from = from;
        ramp.to = value;
        ramp.start = now;
    }

    /// The value controller `controller` is heading for.
    pub(crate) fn target(&self, controller: u16) -> f64 {
        self.ramps
            .get(usize::from(controller))
            .map_or(0.0, |ramp| ramp.to)
    }

    fn at(&self, number: usize, frame: u64) -> f64 {
        let Some(ramp) = self.ramps.get(number) else {
            return 0.0;
        };
        let elapsed = frame.saturating_sub(ramp.start);
        if elapsed >= self.frames {
            ramp.to
        } else {
            ramp.from + (ramp.to - ramp.from) * elapsed as f64 / self.frames as f64
        }
    }
}

impl Controls for ControllerTable {
    fn value(&self, controller: u16, frame: u64) -> f64 {
        self.at(usize::from(controller), frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_jump_becomes_a_straight_line_from_where_it_was() {
        let mut table = ControllerTable::new();
        table.set_sample_rate(1_000.0);
        table.reset(&[]);
        table.set(3, 1.0, 100);
        assert_eq!(table.value(3, 100), 0.0);
        assert_eq!(table.value(3, 105), 0.5);
        assert_eq!(table.value(3, 110), 1.0);
        // Turned back halfway down: up again from where it was then.
        table.set(3, 0.0, 200);
        assert_eq!(table.value(3, 205), 0.5);
        table.set(3, 1.0, 205);
        assert_eq!(table.value(3, 205), 0.5);
        assert_eq!(table.value(3, 210), 0.75);
        assert_eq!(table.target(3), 1.0);
        assert_eq!(table.value(3, 215), 1.0);
    }

    #[test]
    fn values_are_held_to_the_unit_range_and_numbers_to_the_table() {
        let mut table = ControllerTable::new();
        table.set(1, 7.0, 0);
        assert_eq!(table.target(1), 1.0);
        table.set(1, f64::NAN, 0);
        assert_eq!(table.target(1), 1.0);
        table.set(MAX_CONTROLLERS as u16, 1.0, 0);
        assert_eq!(table.value(MAX_CONTROLLERS as u16, 0), 0.0);
    }

    #[test]
    fn a_new_instrument_leaves_controllers_that_were_set_alone() {
        let mut table = ControllerTable::new();
        table.reset(&[]);
        table.set(2, 0.25, 0);
        let mut initial = vec![0.0; MAX_CONTROLLERS];
        initial[2] = 0.75;
        initial[5] = 0.5;
        table.adopt(&initial, 1_000, true);
        assert_eq!((table.target(2), table.target(5)), (0.25, 0.5));
        table.restore(2, &initial, 2_000);
        assert_eq!(table.target(2), 0.75);
        table.reset(&initial);
        assert_eq!(table.value(2, 0), 0.75);
    }
}
