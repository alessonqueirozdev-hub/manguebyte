//! A bank of sine partials for additive synthesis.

use manguebyte_core::vector::{Backend, Partials};

/// Partials are stored in groups of this many, so that the vector backends
/// run every one of them in registers (see
/// [`Backend::add_partials`]).
const GROUP: usize = 16;

/// One partial's settings, for [`PartialBank::set`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Partial {
    /// Frequency in hertz.
    pub freq_hz: f64,
    /// Linear gain.
    pub gain: f32,
    /// Position from `-1.0` (left) to `1.0` (right), with the equal-power
    /// law: `left = cos θ`, `right = sin θ` for `θ = (pan + 1) π / 4`.
    pub pan: f32,
}

/// A bank of sine partials, each with its own frequency, gain and pan,
/// added into a stereo pair of blocks: the sustain of an additive voice,
/// such as an organ pipe.
///
/// Each partial is a quadrature phasor `(cos, sin)` turned every frame by
/// a fixed rotation, under a gain that can ramp linearly; its output is
/// `gain * sin`, scaled by its left and right pan gains. Each frame the
/// gain moves first, then the sample is made, then the phasor turns, the
/// order of Caecilia's kernel. The rendering runs on
/// [`Backend::add_partials`], which gives each partial a lane of a vector
/// register, and is the same on every backend, bit for bit.
///
/// The settings are meant to change every few dozen frames (a control
/// block): [`set_rotation`](Self::set_rotation) and
/// [`ramp_to`](Self::ramp_to) cost a few stores, with no sine or cosine;
/// [`set_frequency`](Self::set_frequency) works one out.
///
/// # Staying on the unit circle
///
/// A rotation in `f32` moves a phasor's magnitude by up to a few parts in
/// 10^7, so the bank brings every phasor back to the circle every
/// [`NORMALISE_FRAMES`](Self::NORMALISE_FRAMES) frames rendered, counted
/// across calls, with one Newton step: `m = cos² + sin²`, both scaled by
/// `1.5 - 0.5 m`, the step Caecilia takes once a control block. A step
/// leaves an error of `m - 1` of under `(m - 1)²` plus a rounding, so the
/// error never builds up: between two steps the magnitude is within
/// `NORMALISE_FRAMES` times what one rotation moves it of 1. A rotation
/// from [`set_frequency`](Self::set_frequency) has a magnitude within
/// 6 · 10^-8 of 1 and its roundings move the phasor's by at most
/// 3.6 · 10^-7, so the magnitude stays within 3 · 10^-5 of 1 (0.0003 dB)
/// for as long as the bank runs. Ten minutes at 48 kHz, measured, stay
/// within 3 · 10^-6.
///
/// # Real time
///
/// [`new`](Self::new) allocates, once; nothing else does. Every setter and
/// [`render_add`](Self::render_add) is real-time safe.
///
/// ```
/// use manguebyte_dsp::{Partial, PartialBank};
///
/// let mut bank = PartialBank::new(16);
/// bank.set_sample_rate(48_000.0);
/// bank.set_len(2);
/// bank.set(0, Partial { freq_hz: 220.0, gain: 0.5, pan: -0.5 });
/// bank.set(1, Partial { freq_hz: 440.0, gain: 0.0, pan: 0.5 });
/// bank.ramp_to(1, 0.25, 64);
///
/// let (mut left, mut right) = (vec![0.0f32; 256], vec![0.0f32; 256]);
/// bank.render_add(&mut left, &mut right);
/// assert!(left.iter().any(|&x| x != 0.0));
/// assert!((bank.gain(1) - 0.25).abs() < 1e-6);
/// ```
#[derive(Debug, Clone)]
pub struct PartialBank {
    sample_rate: f64,
    capacity: usize,
    len: usize,
    cos: Vec<f32>,
    sin: Vec<f32>,
    gain: Vec<f32>,
    cos_step: Vec<f32>,
    sin_step: Vec<f32>,
    gain_step: Vec<f32>,
    left_gain: Vec<f32>,
    right_gain: Vec<f32>,
    /// Frames left in each partial's ramp, 0 when it is not ramping.
    ramp_left: Vec<usize>,
    /// How many partials are ramping.
    ramping: usize,
    /// No ramp ends sooner than this many frames from now (it may end
    /// later: a ramp that was cut short leaves it low until the next
    /// render, which only splits a render where it need not).
    next_ramp_end: usize,
    /// Frames rendered since the phasors were last normalised.
    since_normalised: usize,
    backend: Backend,
}

/// The pan gains of the centre, `cos(π/4)`.
const CENTRE: f32 = core::f32::consts::FRAC_1_SQRT_2;

impl PartialBank {
    /// How often the phasors are brought back to the unit circle, in frames
    /// rendered.
    pub const NORMALISE_FRAMES: usize = 64;

    /// A bank with room for `max` partials, none of them in use. Until
    /// [`set_sample_rate`](Self::set_sample_rate) is called it assumes
    /// 44.1 kHz. Allocates.
    pub fn new(max: usize) -> Self {
        let room = max.div_ceil(GROUP) * GROUP;
        Self {
            sample_rate: 44_100.0,
            capacity: max,
            len: 0,
            cos: vec![1.0; room],
            sin: vec![0.0; room],
            gain: vec![0.0; room],
            cos_step: vec![1.0; room],
            sin_step: vec![0.0; room],
            gain_step: vec![0.0; room],
            left_gain: vec![CENTRE; room],
            right_gain: vec![CENTRE; room],
            ramp_left: vec![0; room],
            ramping: 0,
            next_ramp_end: usize::MAX,
            since_normalised: 0,
            backend: Backend::detect(),
        }
    }

    /// The most partials the bank can hold.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// How many partials are in use: partials `0..len()`.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether no partial is in use.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Sets how many partials are in use. Partials that leave use are
    /// silenced and forgotten; partials that come into use start silent, at
    /// phase 0 and 0 Hz, in the centre. The cost of a render grows with the
    /// number in use, rounded up to a multiple of 16.
    ///
    /// # Panics
    ///
    /// Panics if `len` is more than [`capacity`](Self::capacity).
    #[track_caller]
    pub fn set_len(&mut self, len: usize) {
        assert!(
            len <= self.capacity,
            "{len} partials do not fit in a bank of {}",
            self.capacity
        );
        for i in len..self.len {
            self.forget(i);
        }
        self.len = len;
    }

    /// Partial `i` back to the state of one never used.
    fn forget(&mut self, i: usize) {
        self.stop_ramp(i);
        self.cos[i] = 1.0;
        self.sin[i] = 0.0;
        self.gain[i] = 0.0;
        self.cos_step[i] = 1.0;
        self.sin_step[i] = 0.0;
        self.left_gain[i] = CENTRE;
        self.right_gain[i] = CENTRE;
    }

    /// Sets the sample rate in hertz, which [`set_frequency`](Self::set_frequency)
    /// works from. Rotations already set keep their angle per frame, so
    /// set the frequencies again after a change.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        if sample_rate.is_finite() && sample_rate > 0.0 {
            self.sample_rate = sample_rate;
        }
    }

    /// The sample rate in hertz.
    pub fn sample_rate(&self) -> f64 {
        self.sample_rate
    }

    /// The backend the bank renders on, [`Backend::detect`] unless set.
    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Renders on `backend` from now on. The output is the same on every
    /// backend; only the speed differs. A backend the processor lacks runs
    /// the scalar code.
    pub fn set_backend(&mut self, backend: Backend) {
        self.backend = backend;
    }

    #[track_caller]
    fn check(&self, i: usize) {
        assert!(
            i < self.len,
            "partial {i} is not in use: the bank uses {}",
            self.len
        );
    }

    /// Sets partial `i`'s frequency, gain (at once, ending a ramp) and pan.
    ///
    /// # Panics
    ///
    /// Panics if `i` is not below [`len`](Self::len), as every method that
    /// takes a partial's index does.
    #[track_caller]
    pub fn set(&mut self, i: usize, partial: Partial) {
        self.set_frequency(i, partial.freq_hz);
        self.set_gain(i, partial.gain);
        self.set_pan(i, partial.pan);
    }

    /// Sets partial `i`'s frequency in hertz: a rotation of
    /// `2π · freq_hz / sample_rate` radians a frame, worked out in `f64`
    /// and rounded to `f32` once. The phase carries on. A frequency at or
    /// above Nyquist aliases, so fade such a partial out; one that is not
    /// finite counts as 0 Hz.
    #[track_caller]
    pub fn set_frequency(&mut self, i: usize, freq_hz: f64) {
        self.check(i);
        let w = core::f64::consts::TAU * freq_hz / self.sample_rate;
        let w = if w.is_finite() { w } else { 0.0 };
        self.cos_step[i] = w.cos() as f32;
        self.sin_step[i] = w.sin() as f32;
    }

    /// Sets partial `i`'s rotation each frame directly: `(cos w, sin w)`
    /// for `w` radians a frame. Costs two stores, for a caller that already
    /// has the increments; a pair whose magnitude is not 1 makes the
    /// partial grow or fade until the next normalisation.
    #[track_caller]
    pub fn set_rotation(&mut self, i: usize, cos_step: f32, sin_step: f32) {
        self.check(i);
        self.cos_step[i] = cos_step;
        self.sin_step[i] = sin_step;
    }

    /// Partial `i`'s rotation each frame, `(cos w, sin w)`.
    #[track_caller]
    pub fn rotation(&self, i: usize) -> (f32, f32) {
        self.check(i);
        (self.cos_step[i], self.sin_step[i])
    }

    /// Sets partial `i`'s gain at once, ending its ramp if it had one.
    #[track_caller]
    pub fn set_gain(&mut self, i: usize, gain: f32) {
        self.check(i);
        self.stop_ramp(i);
        self.gain[i] = gain;
    }

    /// Ramps partial `i`'s gain linearly to `target` over the next `frames`
    /// frames rendered, replacing any ramp it had; with `frames` 0 the gain
    /// is set at once.
    ///
    /// The step is `(target - gain) * (1 / frames)` and is added before
    /// each sample, so the first sample of the ramp already moves and the
    /// last reaches the target, as in Caecilia. The gain is the sum of the
    /// steps, not set to `target` at the end, so it can miss the target by
    /// a few roundings (a relative `frames · 2^-24` at worst); a bank that
    /// ramps every control block, as Caecilia's does, then takes the next
    /// step from where the gain really is.
    #[track_caller]
    pub fn ramp_to(&mut self, i: usize, target: f32, frames: usize) {
        self.check(i);
        if frames == 0 {
            self.set_gain(i, target);
            return;
        }
        if self.ramping == 0 {
            self.next_ramp_end = frames;
        }
        if self.ramp_left[i] == 0 {
            self.ramping += 1;
        }
        self.ramp_left[i] = frames;
        self.gain_step[i] = (target - self.gain[i]) * (1.0 / frames as f32);
        self.next_ramp_end = self.next_ramp_end.min(frames);
    }

    fn stop_ramp(&mut self, i: usize) {
        if self.ramp_left[i] != 0 {
            self.ramp_left[i] = 0;
            self.ramping -= 1;
        }
        self.gain_step[i] = 0.0;
    }

    /// Partial `i`'s gain now.
    #[track_caller]
    pub fn gain(&self, i: usize) -> f32 {
        self.check(i);
        self.gain[i]
    }

    /// Whether partial `i`'s gain is ramping.
    #[track_caller]
    pub fn is_ramping(&self, i: usize) -> bool {
        self.check(i);
        self.ramp_left[i] != 0
    }

    /// Sets partial `i`'s position from `-1.0` (left) to `1.0` (right),
    /// with the equal-power law of [`Partial::pan`]. A NaN is the centre.
    #[track_caller]
    pub fn set_pan(&mut self, i: usize, pan: f32) {
        let pan = if pan.is_nan() {
            0.0
        } else {
            f64::from(pan.clamp(-1.0, 1.0))
        };
        let theta = (pan + 1.0) * core::f64::consts::FRAC_PI_4;
        self.set_pan_gains(i, theta.cos() as f32, theta.sin() as f32);
    }

    /// Sets the gains partial `i`'s output is sent to the left and right
    /// with, directly.
    #[track_caller]
    pub fn set_pan_gains(&mut self, i: usize, left: f32, right: f32) {
        self.check(i);
        self.left_gain[i] = left;
        self.right_gain[i] = right;
    }

    /// Partial `i`'s left and right gains.
    #[track_caller]
    pub fn pan_gains(&self, i: usize) -> (f32, f32) {
        self.check(i);
        (self.left_gain[i], self.right_gain[i])
    }

    /// Sets partial `i`'s phase, in turns: its next sample is
    /// `gain · sin(2π · turns)` (after the gain's step).
    #[track_caller]
    pub fn set_phase(&mut self, i: usize, turns: f64) {
        let angle = core::f64::consts::TAU * turns.rem_euclid(1.0);
        let angle = if angle.is_finite() { angle } else { 0.0 };
        self.set_phasor(i, angle.cos() as f32, angle.sin() as f32);
    }

    /// Sets partial `i`'s phasor directly, a point that should be on the
    /// unit circle.
    #[track_caller]
    pub fn set_phasor(&mut self, i: usize, cos: f32, sin: f32) {
        self.check(i);
        self.cos[i] = cos;
        self.sin[i] = sin;
    }

    /// Partial `i`'s phasor now, `(cos, sin)`.
    #[track_caller]
    pub fn phasor(&self, i: usize) -> (f32, f32) {
        self.check(i);
        (self.cos[i], self.sin[i])
    }

    /// Renders the partials in use for `left.len()` frames and adds them to
    /// `left` and `right`.
    ///
    /// The render is split where a ramp ends and where the phasors are
    /// normalised, and nowhere else; how the caller splits its blocks does
    /// not change the output, except that a ramp or a setting given
    /// between two calls starts on a frame of the caller's choosing.
    ///
    /// # Panics
    ///
    /// Panics if `left` and `right` differ in length.
    #[track_caller]
    pub fn render_add(&mut self, left: &mut [f32], right: &mut [f32]) {
        assert!(
            left.len() == right.len(),
            "left and right differ in length: {} and {}",
            left.len(),
            right.len()
        );
        let frames = left.len();
        let used = self.len.div_ceil(GROUP) * GROUP;
        let mut at = 0;
        while at < frames {
            if self.since_normalised == 0 {
                normalise(&mut self.cos[..used], &mut self.sin[..used]);
            }
            let mut n = (frames - at).min(Self::NORMALISE_FRAMES - self.since_normalised);
            if self.ramping > 0 {
                n = n.min(self.next_ramp_end);
            }
            self.backend.add_partials(
                &mut left[at..at + n],
                &mut right[at..at + n],
                &mut Partials {
                    cos: &mut self.cos[..used],
                    sin: &mut self.sin[..used],
                    gain: &mut self.gain[..used],
                    cos_step: &self.cos_step[..used],
                    sin_step: &self.sin_step[..used],
                    gain_step: &self.gain_step[..used],
                    left_gain: &self.left_gain[..used],
                    right_gain: &self.right_gain[..used],
                },
            );
            if self.ramping > 0 {
                self.advance_ramps(n);
            }
            self.since_normalised = (self.since_normalised + n) % Self::NORMALISE_FRAMES;
            at += n;
        }
    }

    /// Counts `frames` off every ramp, ending those that are done.
    fn advance_ramps(&mut self, frames: usize) {
        let mut next = usize::MAX;
        for i in 0..self.len {
            let left = self.ramp_left[i];
            if left == 0 {
                continue;
            }
            if left <= frames {
                self.ramp_left[i] = 0;
                self.gain_step[i] = 0.0;
                self.ramping -= 1;
            } else {
                self.ramp_left[i] = left - frames;
                next = next.min(left - frames);
            }
        }
        self.next_ramp_end = next;
    }
}

/// One Newton step towards magnitude 1 for each phasor, as Caecilia takes
/// it: `m = x² + y²`, then `x` and `y` scaled by `1.5 - 0.5 m`.
fn normalise(cos: &mut [f32], sin: &mut [f32]) {
    for (x, y) in cos.iter_mut().zip(sin.iter_mut()) {
        let magnitude = *x * *x + *y * *y;
        let scale = 1.5 - 0.5 * magnitude;
        *x *= scale;
        *y *= scale;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ramp_reaches_its_target_and_stops() {
        let mut bank = PartialBank::new(3);
        bank.set_len(3);
        bank.set_gain(0, 1.0);
        bank.ramp_to(0, 0.0, 10);
        bank.ramp_to(1, 0.5, 100);
        assert!(bank.is_ramping(0) && bank.is_ramping(1));
        let (mut l, mut r) = (vec![0.0; 37], vec![0.0; 37]);
        bank.render_add(&mut l, &mut r);
        assert!(!bank.is_ramping(0));
        assert!(bank.gain(0).abs() < 1e-6, "{}", bank.gain(0));
        assert!(bank.is_ramping(1));
        assert!((bank.gain(1) - 0.5 * 37.0 / 100.0).abs() < 1e-6);
        let (mut l, mut r) = (vec![0.0; 200], vec![0.0; 200]);
        bank.render_add(&mut l, &mut r);
        assert!(!bank.is_ramping(1));
        assert!((bank.gain(1) - 0.5).abs() < 1e-6);
        assert_eq!(bank.ramping, 0);
    }

    #[test]
    fn partials_leaving_use_are_silenced() {
        let mut bank = PartialBank::new(20);
        bank.set_sample_rate(48_000.0);
        bank.set_len(20);
        for i in 0..20 {
            bank.set(
                i,
                Partial {
                    freq_hz: 100.0 * (i + 1) as f64,
                    gain: 0.1,
                    pan: 0.0,
                },
            );
            bank.ramp_to(i, 0.2, 1_000);
        }
        bank.set_len(0);
        assert_eq!(bank.ramping, 0);
        let (mut l, mut r) = (vec![0.0; 100], vec![0.0; 100]);
        bank.render_add(&mut l, &mut r);
        assert!(l.iter().chain(&r).all(|&x| x == 0.0));
        bank.set_len(1);
        assert_eq!(bank.gain(0), 0.0);
        assert_eq!(bank.phasor(0), (1.0, 0.0));
    }

    #[test]
    fn equal_power_pan() {
        let mut bank = PartialBank::new(1);
        bank.set_len(1);
        for pan in [-1.0f32, -0.3, 0.0, 0.7, 1.0, 5.0, f32::NAN] {
            bank.set_pan(0, pan);
            let (l, r) = bank.pan_gains(0);
            assert!((l * l + r * r - 1.0).abs() < 1e-6, "{pan}");
        }
        bank.set_pan(0, -1.0);
        assert_eq!(bank.pan_gains(0), (1.0, 0.0));
        bank.set_pan(0, 0.0);
        let (l, r) = bank.pan_gains(0);
        assert_eq!(l, r);
    }

    #[test]
    #[should_panic(expected = "not in use")]
    fn an_index_past_the_length_is_refused() {
        let mut bank = PartialBank::new(32);
        bank.set_len(4);
        bank.set_gain(4, 1.0);
    }
}
