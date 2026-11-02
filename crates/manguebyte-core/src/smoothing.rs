use crate::Sample;

/// A value that glides linearly towards its target, avoiding clicks when a
/// parameter changes.
///
/// It is `Copy`, so a processor can run a private copy per channel and then
/// [`skip`](Self::skip) the original forward by one block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoothed<S: Sample = f32> {
    current: S,
    target: S,
    step: S,
    remaining: u32,
    ramp_samples: u32,
}

impl<S: Sample> Smoothed<S> {
    /// Creates a value resting at `value`, with no ramp configured.
    ///
    /// Until [`set_ramp`](Self::set_ramp) is called, changes apply instantly.
    pub fn new(value: S) -> Self {
        Self {
            current: value,
            target: value,
            step: S::ZERO,
            remaining: 0,
            ramp_samples: 0,
        }
    }

    /// Sets how long a change takes, and snaps to the current target.
    pub fn set_ramp(&mut self, sample_rate: f64, seconds: f64) {
        self.ramp_samples = (sample_rate * seconds).max(0.0).round() as u32;
        self.set_immediate(self.target);
    }

    /// Starts gliding towards `target`.
    ///
    /// Setting the target it is already gliding to changes nothing, so a
    /// caller may set it on every block without slowing the glide down.
    pub fn set_target(&mut self, target: S) {
        if self.ramp_samples == 0 {
            self.set_immediate(target);
            return;
        }
        if target == self.target {
            return;
        }
        self.target = target;
        self.step = (target - self.current) / S::from_f64(f64::from(self.ramp_samples));
        self.remaining = self.ramp_samples;
    }

    /// Jumps straight to `value`.
    pub fn set_immediate(&mut self, value: S) {
        self.current = value;
        self.target = value;
        self.step = S::ZERO;
        self.remaining = 0;
    }

    /// Advances one sample and returns the new value.
    #[inline]
    pub fn next_value(&mut self) -> S {
        if self.remaining == 0 {
            return self.target;
        }
        self.remaining -= 1;
        if self.remaining == 0 {
            self.current = self.target;
        } else {
            self.current += self.step;
        }
        self.current
    }

    /// Advances `samples` samples, landing exactly where as many calls to
    /// [`next_value`](Self::next_value) would. Real-time safe.
    pub fn skip(&mut self, samples: usize) {
        // Adding the step once per sample, as next_value does, rounds
        // differently from adding a multiple of it, so a multiplication here
        // would make the result depend on how the samples were grouped.
        let steps = samples.min(self.remaining as usize);
        for _ in 0..steps {
            self.next_value();
        }
    }

    /// The value most recently produced.
    #[inline]
    pub fn current(&self) -> S {
        self.current
    }

    /// The value being approached.
    #[inline]
    pub fn target(&self) -> S {
        self.target
    }

    /// Whether the value is still moving.
    #[inline]
    pub fn is_smoothing(&self) -> bool {
        self.remaining > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_ramp_changes_are_instant() {
        let mut value = Smoothed::new(0.0f32);
        value.set_target(1.0);
        assert!(!value.is_smoothing());
        assert_eq!(value.next_value(), 1.0);
    }

    #[test]
    fn ramps_linearly_and_lands_exactly() {
        let mut value = Smoothed::new(0.0f64);
        value.set_ramp(4.0, 1.0);
        value.set_target(1.0);
        assert_eq!(value.next_value(), 0.25);
        assert_eq!(value.next_value(), 0.5);
        assert_eq!(value.next_value(), 0.75);
        assert_eq!(value.next_value(), 1.0);
        assert!(!value.is_smoothing());
        assert_eq!(value.next_value(), 1.0);
    }

    #[test]
    fn skip_matches_repeated_next() {
        let mut stepped = Smoothed::new(0.0f64);
        stepped.set_ramp(100.0, 1.0);
        stepped.set_target(1.0);
        let mut skipped = stepped;

        for _ in 0..37 {
            stepped.next_value();
        }
        skipped.skip(37);
        assert!((stepped.current() - skipped.current()).abs() < 1e-12);

        skipped.skip(1000);
        assert_eq!(skipped.current(), 1.0);
        assert!(!skipped.is_smoothing());
    }
}
