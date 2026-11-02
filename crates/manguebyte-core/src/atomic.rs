use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// An `f32` that can be read and written from several threads without locks.
///
/// This is the usual way to hand a parameter value from a UI thread to the
/// audio thread.
#[derive(Debug, Default)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    /// Creates a new atomic holding `value`.
    pub const fn new(value: f32) -> Self {
        Self(AtomicU32::new(value.to_bits()))
    }

    /// Reads the value.
    #[inline]
    pub fn load(&self, ordering: Ordering) -> f32 {
        f32::from_bits(self.0.load(ordering))
    }

    /// Writes the value.
    #[inline]
    pub fn store(&self, value: f32, ordering: Ordering) {
        self.0.store(value.to_bits(), ordering);
    }

    /// Writes `value` and returns what was there before, as one atomic step.
    #[inline]
    pub fn swap(&self, value: f32, ordering: Ordering) -> f32 {
        f32::from_bits(self.0.swap(value.to_bits(), ordering))
    }
}

/// An `f64` that can be read and written from several threads without locks.
#[derive(Debug, Default)]
pub struct AtomicF64(AtomicU64);

impl AtomicF64 {
    /// Creates a new atomic holding `value`.
    pub const fn new(value: f64) -> Self {
        Self(AtomicU64::new(value.to_bits()))
    }

    /// Reads the value.
    #[inline]
    pub fn load(&self, ordering: Ordering) -> f64 {
        f64::from_bits(self.0.load(ordering))
    }

    /// Writes the value.
    #[inline]
    pub fn store(&self, value: f64, ordering: Ordering) {
        self.0.store(value.to_bits(), ordering);
    }

    /// Writes `value` and returns what was there before, as one atomic step.
    #[inline]
    pub fn swap(&self, value: f64, ordering: Ordering) -> f64 {
        f64::from_bits(self.0.swap(value.to_bits(), ordering))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_loads() {
        let value = AtomicF32::new(0.25);
        assert_eq!(value.load(Ordering::Relaxed), 0.25);
        value.store(-3.5, Ordering::Relaxed);
        assert_eq!(value.load(Ordering::Relaxed), -3.5);
        assert_eq!(value.swap(1.0, Ordering::Relaxed), -3.5);
        assert_eq!(value.load(Ordering::Relaxed), 1.0);

        let wide = AtomicF64::new(core::f64::consts::PI);
        assert_eq!(wide.load(Ordering::Relaxed), core::f64::consts::PI);
    }
}
