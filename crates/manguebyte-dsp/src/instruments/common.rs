//! What the instruments share: pitch and level conversions, noise, the
//! hand-over of a stolen voice, and a saturator that aliases little.

use manguebyte_core::{AudioBuffer, Sample};

use crate::numeric::limit;

/// The natural logarithm of 1000: a fall of 60 dB, as a factor of `e`.
const LN_1000: f64 = 6.907_755_278_982_137;

/// A level below this counts as silence: 100 dB under full scale.
pub(crate) const SILENCE: f64 = 1e-5;

/// Below this a decaying factor is put to zero, long before it could become
/// a subnormal number (ADR 0006: the flush-to-zero guard does not cover
/// offline renders or tests, so the instruments keep out of them
/// themselves).
pub(crate) const NEGLIGIBLE: f64 = 1e-20;

/// How long a sounding note that is taken over for a new one takes to fade
/// out, in seconds. The fade is heard alongside the new note, so the old one
/// stops without a click.
pub const HANDOVER_SECONDS: f64 = 0.004;

/// The frequency of a key, possibly fractional, in equal temperament with
/// key 69 at 440 Hz.
#[inline]
pub(crate) fn key_hz(key: f64) -> f64 {
    440.0 * ((key - 69.0) / 12.0).exp2()
}

/// The gain of a level in decibels.
#[inline]
pub(crate) fn db(decibels: f64) -> f64 {
    (decibels * (core::f64::consts::LN_10 / 20.0)).exp()
}

/// The factor per sample that lowers a level by 60 dB in `seconds`. Zero
/// for a time that is not positive, and 1 for an infinite one.
#[inline]
pub(crate) fn t60_factor(seconds: f64, sample_rate: f64) -> f64 {
    if seconds > 0.0 {
        (-LN_1000 / (seconds * sample_rate)).exp()
    } else {
        0.0
    }
}

/// The gain for a velocity from 0 to 1: full at 1, `range_db` down at 0,
/// evenly in decibels between.
#[inline]
pub(crate) fn velocity_gain(velocity: f64, range_db: f64) -> f64 {
    db(-range_db * (1.0 - velocity))
}

/// A setting made safe to compute with: `value` limited to `low..=high`,
/// or `default` if it is not a number.
#[inline]
pub(crate) fn setting(value: f64, low: f64, high: f64, default: f64) -> f64 {
    if value.is_nan() {
        default
    } else {
        limit(value, low, high)
    }
}

/// A value that is about to become too small to matter, put to zero.
#[inline]
pub(crate) fn flush(value: f64) -> f64 {
    if value.abs() < NEGLIGIBLE { 0.0 } else { value }
}

/// A repeatable source of noise (xorshift64).
///
/// Every instrument seeds its noise from the note it plays, so the same note
/// sounds the same every time it is played, whatever was played before and
/// however the timeline is cut into blocks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Noise(u64);

impl Noise {
    /// A sequence for one note: the key, the velocity and something that
    /// tells the instrument's sources apart.
    pub(crate) fn for_note(key: u8, velocity: f64, source: u64) -> Self {
        let mut state = u64::from(key)
            ^ velocity.to_bits().rotate_left(17)
            ^ source.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // splitmix64, so that close seeds give unrelated sequences.
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        state = (state ^ (state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        state = (state ^ (state >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        state ^= state >> 31;
        Self(state | 1)
    }

    /// A number from -1 up to 1.
    #[inline]
    pub(crate) fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }

    /// A number from 0 up to 1.
    #[inline]
    pub(crate) fn unit(&mut self) -> f64 {
        0.5 * (self.next() + 1.0)
    }
}

/// Adds the samples `next` produces to every channel of `buffer`, one frame
/// at a time.
#[inline]
pub(crate) fn add_to_every_channel<S: Sample>(
    buffer: &mut AudioBuffer<S>,
    mut next: impl FnMut() -> f64,
) {
    let channels = buffer.num_channels();
    for frame in 0..buffer.num_frames() {
        let value = next();
        for channel in 0..channels {
            buffer.channel_mut(channel)[frame] += S::from_f64(value);
        }
    }
}

/// The sound of one note, which a polyphonic voice keeps two of so that a
/// note can fade out while the next one starts.
pub(crate) trait Tone {
    /// Whether the note is still making sound.
    fn is_sounding(&self) -> bool;
    /// Back to the state the tone had when it was new.
    fn silence(&mut self);
}

/// A voice's current note, and the one it was playing before it was taken
/// over, fading out over [`HANDOVER_SECONDS`].
///
/// [`Synth`](crate::Synth) chokes a voice before every fresh note. A voice
/// that is still sounding is being taken over: its note moves aside and
/// fades while the new note starts from a clean tone. A voice that is
/// silent goes back to the state it had when new, fading note and all, as
/// the contract of [`Voice::choke`](crate::Voice::choke) asks. A choke the
/// synth ends at once (a note choke, all sound off, a reset) stops the
/// rendering of the voice, so the fade is not heard there, and the next
/// fresh note finds the voice silent and clears it.
#[derive(Debug, Clone)]
pub(crate) struct Handover<T> {
    pub(crate) now: T,
    pub(crate) old: T,
    fade: f64,
    step: f64,
}

impl<T: Tone> Handover<T> {
    pub(crate) fn new(now: T, old: T) -> Self {
        Self {
            now,
            old,
            fade: 0.0,
            step: 1.0,
        }
    }

    /// Takes the sample rate the fade runs at, and stops any fade. The
    /// tones are prepared and silenced by their owner.
    pub(crate) fn prepare(&mut self, sample_rate: f64) {
        self.step = 1.0 / (HANDOVER_SECONDS * sample_rate).max(1.0);
        self.fade = 0.0;
    }

    /// What a choke means; see the type's documentation.
    pub(crate) fn choke(&mut self) {
        if self.now.is_sounding() {
            core::mem::swap(&mut self.now, &mut self.old);
            self.fade = 1.0;
        } else {
            self.old.silence();
            self.fade = 0.0;
        }
        self.now.silence();
    }

    /// Forgets the fading note, unheard: for a voice a budget suspends.
    pub(crate) fn drop_fading(&mut self) {
        self.old.silence();
        self.fade = 0.0;
    }

    /// Whether either note is making sound.
    pub(crate) fn is_active(&self) -> bool {
        self.now.is_sounding() || self.fade > 0.0
    }

    /// One sample: the note's own, and the fading note's.
    #[inline]
    pub(crate) fn next(&mut self, mut sample: impl FnMut(&mut T) -> f64) -> f64 {
        let mut value = 0.0;
        if self.now.is_sounding() {
            value += sample(&mut self.now);
        }
        if self.fade > 0.0 {
            value += sample(&mut self.old) * self.fade;
            self.fade -= self.step;
            if self.fade <= 0.0 {
                self.fade = 0.0;
                self.old.silence();
            }
        }
        value
    }
}

/// `ln(cosh(x))`, the antiderivative of `tanh`, without overflow.
#[inline]
fn log_cosh(x: f64) -> f64 {
    let x = x.abs();
    x + (-2.0 * x).exp().ln_1p() - core::f64::consts::LN_2
}

/// A `tanh` saturator with first-order antiderivative anti-aliasing.
///
/// Instead of the curve at each sample, it puts out the curve's average
/// between the last input and this one, `(F(x[n]) - F(x[n-1])) / (x[n] -
/// x[n-1])` with `F` the curve's antiderivative. That is the curve applied
/// to the straight line joining the two samples, filtered by a box one
/// sample long, which takes most of the aliasing out of the harmonics the
/// curve makes, for the price of half a sample of delay. The method is
/// J. D. Parker, V. Zavalishin and E. Le Bivic, "Reducing the aliasing of
/// nonlinear waveshaping using continuous-time convolution", DAFx 2016.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SoftClip {
    last: f64,
    last_integral: f64,
}

impl SoftClip {
    #[inline]
    pub(crate) fn next(&mut self, x: f64) -> f64 {
        let x = setting(x, -1e6, 1e6, 0.0);
        let integral = log_cosh(x);
        let difference = x - self.last;
        let y = if difference.abs() < 1e-5 * (1.0 + x.abs()) {
            // The two inputs are too close for the quotient, whose rounding
            // error grows as they approach: the curve at their middle is
            // the same to second order.
            (0.5 * (x + self.last)).tanh()
        } else {
            (integral - self.last_integral) / difference
        };
        self.last = x;
        self.last_integral = integral;
        // The average of a curve between -1 and 1 lies there too; this only
        // removes rounding.
        y.clamp(-1.0, 1.0)
    }

    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert!((key_hz(69.0) - 440.0).abs() < 1e-12);
        assert!((key_hz(57.0) - 220.0).abs() < 1e-12);
        assert!((db(-6.020_599_913_279_624) - 0.5).abs() < 1e-12);
        // 60 dB in a second at 1 kHz: a thousandth after 1000 samples.
        let factor = t60_factor(1.0, 1_000.0);
        assert!((factor.powi(1_000) - 1e-3).abs() < 1e-12);
        assert_eq!(t60_factor(0.0, 1_000.0), 0.0);
        assert_eq!(t60_factor(f64::NAN, 1_000.0), 0.0);
        assert_eq!(velocity_gain(1.0, 30.0), 1.0);
        assert!((velocity_gain(0.0, 20.0) - 0.1).abs() < 1e-12);
        assert_eq!(setting(f64::NAN, 0.0, 1.0, 0.5), 0.5);
        assert_eq!(setting(7.0, 0.0, 1.0, 0.5), 1.0);
        assert_eq!(setting(f64::NEG_INFINITY, 0.0, 1.0, 0.5), 0.0);
        assert_eq!(flush(1e-30), 0.0);
        assert_eq!(flush(1e-3), 1e-3);
    }

    #[test]
    fn noise_is_repeatable_spread_and_in_range() {
        let mut a = Noise::for_note(60, 0.5, 1);
        let mut b = Noise::for_note(60, 0.5, 1);
        let mut c = Noise::for_note(61, 0.5, 1);
        let (mut sum, mut squares) = (0.0, 0.0);
        let mut differs = false;
        for _ in 0..10_000 {
            let x = a.next();
            assert_eq!(x, b.next());
            differs |= x != c.next();
            assert!((-1.0..1.0).contains(&x));
            sum += x;
            squares += x * x;
        }
        assert!(differs);
        assert!((0.0..1.0).contains(&a.unit()));
        assert!((sum / 10_000.0).abs() < 0.03);
        // Uniform noise has a mean square of a third.
        assert!((squares / 10_000.0 - 1.0 / 3.0).abs() < 0.02);
    }

    #[test]
    fn the_soft_clip_follows_tanh_and_aliases_less() {
        // A slow signal comes out as tanh of itself, half a sample late.
        let mut clip = SoftClip::default();
        let mut worst = 0.0f64;
        for n in 0..1_000 {
            let t = f64::from(n) / 1_000.0;
            let x = 3.0 * (core::f64::consts::TAU * t).sin();
            let y = clip.next(x);
            if n > 0 {
                let middle = 3.0 * (core::f64::consts::TAU * (t - 0.0005)).sin();
                worst = worst.max((y - middle.tanh()).abs());
            }
        }
        assert!(worst < 1e-4, "{worst}");
        // Huge and broken inputs stay in range.
        for x in [1e300, -1e300, f64::NAN, f64::INFINITY, 0.0, 1e-9] {
            let y = clip.next(x);
            assert!(y.is_finite() && y.abs() <= 1.0, "{x} gave {y}");
        }
        clip.reset();
        assert_eq!(clip.next(0.0), 0.0);
    }
}
