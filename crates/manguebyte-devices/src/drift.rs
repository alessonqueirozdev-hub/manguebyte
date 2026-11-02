//! Following the drift between two devices' clocks.
//!
//! An input and an output device that are separate pieces of hardware each
//! run from their own crystal. Two crystals never agree exactly: a few tens
//! to a few hundreds of parts per million apart is normal, which at 48 kHz
//! is up to about 15 frames a second. Left alone, the audio queued between
//! the two either grows without end or runs dry.
//!
//! The duplex stream converts the captured audio with a [`Resampler`] whose
//! ratio follows the queue: when audio piles up the input is read a little
//! faster, when it thins out a little slower, so the queue stays at its
//! target and nothing is ever dropped or padded. The ratio comes from a
//! proportional-integral loop on the queue fill, the classic second-order
//! loop of a phase-locked loop (F. M. Gardner, *Phaselock Techniques*, 3rd
//! ed., Wiley 2005, chapter 2); applying one to audio clocks is the idea of
//! F. Adriaensen, "Using a DLL to filter time", Linux Audio Conference 2005.
//!
//! The fill seen by the output callback jumps by a whole input block each
//! time the input delivers one, so before the loop sees it the fill goes
//! through two one-pole low-pass stages far below the block rate. What is
//! left of that sawtooth moves the ratio by a few parts per million, which
//! is inaudible.
//!
//! [`Resampler`]: manguebyte_dsp::Resampler

use std::f64::consts::TAU;

/// The largest correction, either way: a quarter of a percent, ten times
/// the drift of ordinary hardware. Beyond it, something other than drift is
/// wrong, and the queue's own overrun and underrun handling takes over.
pub(crate) const MAX_CORRECTION: f64 = 0.0025;

/// Natural frequency of the loop, in radians per second. It settles in a
/// few seconds and follows a drift that changes as the hardware warms up.
const NATURAL_FREQUENCY: f64 = 0.5;

/// Damping of the loop: critically damped, so a step in drift is followed
/// without ringing.
const DAMPING: f64 = 1.0;

/// Corner of each of the two low-pass stages on the measured fill, in
/// hertz. Twelve times the loop's natural frequency, so it barely affects
/// the loop, and well below any block rate a device runs at.
const SMOOTHING_HZ: f64 = 1.0;

/// Turns the fill of the queue between two devices into a resampling ratio.
#[derive(Debug, Clone)]
pub(crate) struct DriftController {
    sample_rate: f64,
    /// The fill error after the first and the second low-pass stage.
    smoothed: [f64; 2],
    /// The smoothed error integrated over time, in frame-seconds.
    integral: f64,
    /// Input frames read per output frame, minus one.
    correction: f64,
    /// Whether the low-pass stages hold a measurement yet.
    started: bool,
    /// The fill the loop holds: the fill it found when it started.
    target: f64,
}

impl DriftController {
    pub(crate) fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate: sample_rate.max(1.0),
            smoothed: [0.0; 2],
            integral: 0.0,
            correction: 0.0,
            started: false,
            target: 0.0,
        }
    }

    /// Starts again from the current correction, for when the queue was
    /// refilled or trimmed and its fill jumped. The drift learnt so far is
    /// kept: the hardware has not changed. The next fill becomes the
    /// target.
    pub(crate) fn restart(&mut self) {
        self.started = false;
    }

    /// Input frames to read per output frame, minus one: positive when the
    /// input device runs fast.
    pub(crate) fn correction(&self) -> f64 {
        self.correction
    }

    /// The fill the loop holds, in input frames.
    #[cfg(test)]
    pub(crate) fn target(&self) -> f64 {
        self.target
    }

    /// Takes the queue's fill before a block of `frames` output frames, in
    /// input frames, and returns the ratio to resample that block with
    /// (output frames per input frame).
    ///
    /// The first fill after a start becomes the target, so the loop starts
    /// with no error to correct: where the input's deliveries fall against
    /// the output's when the stream starts decides the latency within one
    /// input block, and the loop keeps it there.
    ///
    /// Real-time safe.
    pub(crate) fn update(&mut self, fill: f64, frames: usize) -> f64 {
        if !fill.is_finite() {
            return self.ratio();
        }
        if !self.started {
            self.target = fill;
            self.smoothed = [0.0; 2];
            self.started = true;
        }
        let error = fill - self.target;
        let dt = frames as f64 / self.sample_rate;
        let a = 1.0 - (-TAU * SMOOTHING_HZ * dt).exp();
        self.smoothed[0] += a * (error - self.smoothed[0]);
        self.smoothed[1] += a * (self.smoothed[0] - self.smoothed[1]);
        let error = self.smoothed[1];

        // The queue fills at (input rate) - (output rate) * (1 + correction)
        // frames a second. With the correction proportional to the error
        // and to its integral, the error obeys
        // e'' + rate * kp * e' + rate * ki * e = 0, a second-order loop
        // whose natural frequency and damping set kp and ki.
        let kp = 2.0 * DAMPING * NATURAL_FREQUENCY / self.sample_rate;
        let ki = NATURAL_FREQUENCY * NATURAL_FREQUENCY / self.sample_rate;
        self.integral += error * dt;
        // Anti-windup: the integral alone never asks for more than the
        // largest correction.
        let limit = MAX_CORRECTION / ki;
        self.integral = self.integral.clamp(-limit, limit);
        self.correction = (kp * error + ki * self.integral).clamp(-MAX_CORRECTION, MAX_CORRECTION);
        self.ratio()
    }

    /// Output frames per input frame.
    pub(crate) fn ratio(&self) -> f64 {
        1.0 / (1.0 + self.correction)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the loop against a queue fed `drift` faster than it is read,
    /// in whole blocks, and returns the fill error over the last second.
    fn settle(drift: f64, seconds: f64) -> (DriftController, f64) {
        let rate = 48_000.0;
        let block = 256;
        let target = 1_000.0;
        let mut controller = DriftController::new(rate);
        let mut fill = target;
        controller.update(fill, block);
        let blocks = (seconds * rate / block as f64) as usize;
        let mut worst_late = 0.0f64;
        for index in 0..blocks {
            let ratio = controller.update(fill, block);
            fill += block as f64 * (1.0 + drift) - block as f64 / ratio;
            if index > blocks - (rate as usize / block) {
                worst_late = worst_late.max((fill - target).abs());
            }
        }
        (controller, worst_late)
    }

    #[test]
    fn the_loop_learns_the_drift_and_holds_the_fill() {
        for drift in [-500e-6, -100e-6, 0.0, 30e-6, 300e-6, 2_000e-6] {
            let (controller, error) = settle(drift, 30.0);
            assert!(
                (controller.correction() - drift).abs() < 1e-6,
                "{drift}: learnt {}",
                controller.correction()
            );
            assert!(error < 0.5, "{drift}: fill still {error} frames off");
        }
    }

    #[test]
    fn the_correction_is_bounded() {
        let (controller, _) = settle(0.05, 5.0);
        assert_eq!(controller.correction(), MAX_CORRECTION);
        let mut controller = DriftController::new(48_000.0);
        assert_eq!(controller.update(f64::NAN, 64), 1.0);
        assert_eq!(controller.update(0.0, 64), 1.0);
        assert_eq!(controller.target(), 0.0);
        assert_eq!(controller.update(1e300, 64), 1.0 / (1.0 + MAX_CORRECTION));
    }
}
