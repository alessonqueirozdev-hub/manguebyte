use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::dynamics::{to_db, to_gain};
use crate::numeric::{glide_to, limit};
use crate::{EnvelopeFollower, LevelDetector, ProcessSpec, Processor};

/// Limits of the settings.
const MAX_GAIN_DB: f64 = 24.0;
const MIN_ATTACK_MS: f64 = 2.0;
const MAX_ATTACK_MS: f64 = 200.0;
const MIN_SUSTAIN_MS: f64 = 60.0;
const MAX_SUSTAIN_MS: f64 = 3_000.0;

/// The fixed times of the detectors, in milliseconds: how fast the
/// envelope rises and falls, and how fast the followers of the envelope
/// follow it in the direction they do not measure.
const FAST_ATTACK_MS: f64 = 0.5;
const FAST_RELEASE_MS: f64 = 50.0;

/// How far apart, in decibels, the two followers of a pair have to be for
/// the whole of a gain to be applied.
const SENSITIVITY_DB: f64 = 12.0;

/// How long a change of gain takes.
const GLIDE_SECONDS: f64 = 0.02;

/// A transient shaper: the start of each note made louder or quieter, and
/// what follows it, independently of how loud the note is.
///
/// Unlike a compressor it has no threshold: it reacts to the shape of the
/// level, not to the level itself. An [`EnvelopeFollower`] reads the peak
/// level of all channels together, rising in 0.5 ms and falling in 50 ms,
/// and two more followers follow that envelope:
///
/// - **Attack.** One rises in the attack time (20 ms unless set) and falls
///   as fast as the envelope does. At the start of a note the envelope is
///   ahead of it, and the attack gain is applied in proportion to how far,
///   all of it once the envelope is 12 dB ahead. The start of a note out of
///   silence is far more than 12 dB ahead, so its peak is changed by the
///   attack gain exactly.
/// - **Sustain.** The other rises as fast as the envelope and falls in the
///   sustain time (400 ms unless set). While a note dies away it stays
///   above the envelope, and the sustain gain is applied in proportion, all
///   of it from 12 dB apart: from about 130 ms into a note that dies away
///   by 8.7 dB every 50 ms.
///
/// The two gains add in decibels. A steady signal keeps both pairs together
/// and passes at its own level. The gains glide over 20 ms; all setters are
/// real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, TransientShaper};
///
/// let mut shaper = TransientShaper::<f32>::new().with_attack_db(6.0);
/// shaper.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 480, num_channels: 1 });
///
/// // A hit out of silence: its start comes out twice as loud.
/// let mut block = AudioBuffer::<f32>::new(1, 480);
/// for (n, sample) in block.channel_mut(0).iter_mut().enumerate() {
///     *sample = 0.25 * (n as f32 * 0.3).sin();
/// }
/// shaper.process(&mut block);
/// let peak = block.channel(0)[..48].iter().fold(0.0f32, |p, s| p.max(s.abs()));
/// assert!((peak - 0.5).abs() < 0.01);
/// ```
///
/// # Limits
///
/// The envelope follows peaks, so on a low steady tone it ripples at twice
/// the tone's frequency, and the gain wobbles a little with it: with both
/// gains at 12 dB, a 100 Hz tone comes out within a decibel of its level.
/// A note that dies away more slowly than the envelope falls gets less of
/// the sustain gain.
/// The gains are limited to ±24 dB, the attack time to 2 to 200 ms and the
/// sustain time to 60 ms to 3 s. There is no look-ahead: the first half
/// millisecond of a note is shaped as the fast follower catches up.
#[derive(Debug, Clone)]
pub struct TransientShaper<S: Sample = f32> {
    attack_db: f64,
    sustain_db: f64,
    attack_ms: f64,
    sustain_ms: f64,
    attack_now: Smoothed<f64>,
    sustain_now: Smoothed<f64>,
    /// The envelope of the input.
    envelope: EnvelopeFollower<f64>,
    /// The envelope, slow to rise.
    attack_follower: EnvelopeFollower<f64>,
    /// The envelope, slow to fall.
    sustain_follower: EnvelopeFollower<f64>,
    /// The gain applied to the last frame, in decibels, for a meter.
    gain_db: f64,
    marker: core::marker::PhantomData<S>,
}

impl<S: Sample> Default for TransientShaper<S> {
    fn default() -> Self {
        Self::new()
    }
}

fn follower(attack_ms: f64, release_ms: f64) -> EnvelopeFollower<f64> {
    EnvelopeFollower::new()
        .with_detector(LevelDetector::Peak)
        .with_attack_ms(attack_ms)
        .with_release_ms(release_ms)
}

impl<S: Sample> TransientShaper<S> {
    /// Creates a transient shaper that changes nothing yet: both gains at
    /// 0 dB, an attack time of 20 ms and a sustain time of 400 ms.
    pub fn new() -> Self {
        Self {
            attack_db: 0.0,
            sustain_db: 0.0,
            attack_ms: 20.0,
            sustain_ms: 400.0,
            attack_now: Smoothed::new(0.0),
            sustain_now: Smoothed::new(0.0),
            envelope: follower(FAST_ATTACK_MS, FAST_RELEASE_MS),
            attack_follower: follower(20.0, FAST_ATTACK_MS),
            sustain_follower: follower(FAST_ATTACK_MS, 400.0),
            gain_db: 0.0,
            marker: core::marker::PhantomData,
        }
    }

    /// Sets how much the start of a note is raised (positive) or lowered,
    /// in decibels, limited to ±24 dB.
    pub fn set_attack_db(&mut self, db: f64) {
        self.attack_db = if db.is_nan() {
            0.0
        } else {
            limit(db, -MAX_GAIN_DB, MAX_GAIN_DB)
        };
        glide_to(&mut self.attack_now, self.attack_db);
    }

    /// Sets how much what follows the start is raised or lowered, in
    /// decibels, limited to ±24 dB.
    pub fn set_sustain_db(&mut self, db: f64) {
        self.sustain_db = if db.is_nan() {
            0.0
        } else {
            limit(db, -MAX_GAIN_DB, MAX_GAIN_DB)
        };
        glide_to(&mut self.sustain_now, self.sustain_db);
    }

    /// Sets how long the start of a note counts as its attack: the rise
    /// time of the attack follower, in milliseconds, limited to 2 to
    /// 200 ms.
    pub fn set_attack_ms(&mut self, milliseconds: f64) {
        self.attack_ms = limit(milliseconds, MIN_ATTACK_MS, MAX_ATTACK_MS);
        self.attack_follower.set_attack_ms(self.attack_ms);
    }

    /// Sets how long a note's tail counts as its sustain: the fall time of
    /// the sustain follower, in milliseconds, limited to 60 ms to 3 s.
    pub fn set_sustain_ms(&mut self, milliseconds: f64) {
        self.sustain_ms = limit(milliseconds, MIN_SUSTAIN_MS, MAX_SUSTAIN_MS);
        self.sustain_follower.set_release_ms(self.sustain_ms);
    }

    /// Builder form of [`set_attack_db`](Self::set_attack_db).
    #[must_use]
    pub fn with_attack_db(mut self, db: f64) -> Self {
        self.set_attack_db(db);
        self
    }

    /// Builder form of [`set_sustain_db`](Self::set_sustain_db).
    #[must_use]
    pub fn with_sustain_db(mut self, db: f64) -> Self {
        self.set_sustain_db(db);
        self
    }

    /// Builder form of [`set_attack_ms`](Self::set_attack_ms).
    #[must_use]
    pub fn with_attack_ms(mut self, milliseconds: f64) -> Self {
        self.set_attack_ms(milliseconds);
        self
    }

    /// Builder form of [`set_sustain_ms`](Self::set_sustain_ms).
    #[must_use]
    pub fn with_sustain_ms(mut self, milliseconds: f64) -> Self {
        self.set_sustain_ms(milliseconds);
        self
    }

    /// The gain applied to the last frame, in decibels. For a meter.
    pub fn gain_db(&self) -> f64 {
        self.gain_db
    }

    /// The gain for a frame whose largest sample has the size `level`, in
    /// decibels; moves the followers on.
    #[inline]
    fn gain_for(&mut self, level: f64) -> f64 {
        let envelope = self.envelope.process_frame(&[level]);
        let behind = self.attack_follower.process_frame(&[envelope]);
        let above = self.sustain_follower.process_frame(&[envelope]);
        let envelope = to_db(envelope);
        let attack = envelope - to_db(behind);
        let sustain = to_db(above) - envelope;
        let attack_db = self.attack_now.next_value();
        let sustain_db = self.sustain_now.next_value();
        attack_db * limit(attack / SENSITIVITY_DB, 0.0, 1.0)
            + sustain_db * limit(sustain / SENSITIVITY_DB, 0.0, 1.0)
    }
}

impl<S: Sample> Processor<S> for TransientShaper<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        let mono = ProcessSpec {
            num_channels: 1,
            ..*spec
        };
        for follower in [
            &mut self.envelope,
            &mut self.attack_follower,
            &mut self.sustain_follower,
        ] {
            follower.prepare(&mono);
        }
        self.attack_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.sustain_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let channels = buffer.num_channels();
        for frame in 0..buffer.num_frames() {
            let mut level = 0.0f64;
            for channel in 0..channels {
                level = level.max(buffer.channel(channel)[frame].to_f64().abs());
            }
            self.gain_db = self.gain_for(level);
            let gain = S::from_f64(to_gain(self.gain_db));
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }

    fn reset(&mut self) {
        for follower in [
            &mut self.envelope,
            &mut self.attack_follower,
            &mut self.sustain_follower,
        ] {
            Processor::<f64>::reset(follower);
        }
        self.attack_now.set_immediate(self.attack_db);
        self.sustain_now.set_immediate(self.sustain_db);
        self.gain_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{db, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn prepared(shaper: TransientShaper<f64>, channels: usize) -> TransientShaper<f64> {
        let mut shaper = shaper;
        shaper.prepare(&ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        });
        shaper
    }

    /// Hits: a 1 kHz tone that starts at full level out of silence and
    /// dies away by 8.7 dB every 50 ms, at `level`, every half second.
    fn hits(frames: usize, level: f64) -> AudioBuffer<f64> {
        let mut buffer = AudioBuffer::new(1, frames);
        for (n, sample) in buffer.channel_mut(0).iter_mut().enumerate() {
            let t = (n % 24_000) as f64 / RATE;
            *sample = level * (-t / 0.05).exp() * (TAU * 1_000.0 * t).sin();
        }
        buffer
    }

    fn peak(signal: &[f64]) -> f64 {
        signal.iter().fold(0.0, |peak: f64, s| peak.max(s.abs()))
    }

    fn rms(signal: &[f64]) -> f64 {
        (signal.iter().map(|s| s * s).sum::<f64>() / signal.len() as f64).sqrt()
    }

    #[test]
    fn the_peak_of_an_attack_moves_by_the_attack_gain() {
        for level in [1.0, 0.1, 0.01] {
            for gain in [-12.0, -6.0, 6.0, 12.0] {
                let input = hits(48_000, level);
                let mut shaper = prepared(TransientShaper::new().with_attack_db(gain), 1);
                let mut output = input.clone();
                run_in_blocks(&mut shaper, &mut output, 512);
                // The first millisecond of the second hit, the first
                // after silence... and the first hit itself.
                for start in [0, 24_000] {
                    let before = peak(&input.channel(0)[start..start + 96]);
                    let after = peak(&output.channel(0)[start..start + 96]);
                    let change = db(after / before);
                    assert!(
                        (change - gain).abs() < 0.1,
                        "level {level}, gain {gain}: the attack moved by {change} dB"
                    );
                }
            }
        }
    }

    #[test]
    fn the_tail_moves_by_the_sustain_gain() {
        for gain in [-12.0, 6.0, 12.0] {
            let input = hits(24_000, 0.5);
            let mut shaper = prepared(TransientShaper::new().with_sustain_db(gain), 1);
            let mut output = input.clone();
            run_in_blocks(&mut shaper, &mut output, 512);
            // From 150 to 300 ms the envelope is more than 12 dB below the
            // sustain follower: the whole sustain gain.
            let range = 7_200..14_400;
            let change = db(rms(&output.channel(0)[range.clone()]) / rms(&input.channel(0)[range]));
            assert!((change - gain).abs() < 0.05, "gain {gain}: {change} dB");
            // And the very start is left alone: the attack gain is 0 dB.
            let start = db(peak(&output.channel(0)[..96]) / peak(&input.channel(0)[..96]));
            assert!(
                start.abs() < 0.1,
                "gain {gain}: the start moved by {start} dB"
            );
        }
    }

    #[test]
    fn a_steady_tone_keeps_its_level() {
        for frequency in [100.0, 1_000.0] {
            let mut shaper = prepared(
                TransientShaper::new()
                    .with_attack_db(12.0)
                    .with_sustain_db(12.0),
                1,
            );
            let mut tone = sine::<f64>(1, 96_000, frequency, RATE, 0.5);
            run_in_blocks(&mut shaper, &mut tone, 512);
            let (low, high) = tone.channel(0)[48_000..]
                .chunks((RATE / frequency) as usize)
                .map(peak)
                .fold((f64::MAX, 0.0f64), |(low, high), p| {
                    (low.min(p), high.max(p))
                });
            assert!(
                db(high / 0.5) < 1.0 && db(low / 0.5) > -1.0,
                "{frequency} Hz: from {low} to {high}"
            );
        }
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut shaper = prepared(TransientShaper::new().with_attack_db(9.0), 2);
            let mut signal = noise::<f64>(2, 6_000, 3);
            for (n, sample) in signal.channel_mut(0).iter_mut().enumerate() {
                *sample *= if (n / 1_000) % 2 == 0 { 1.0 } else { 0.05 };
            }
            let mut start = 0;
            for (end, amount) in [(2_500, -6.0), (4_000, 18.0), (6_000, 0.0)] {
                signal.windowed(start..end, |part| run_in_blocks(&mut shaper, part, block));
                shaper.set_attack_db(amount);
                shaper.set_sustain_db(-amount);
                shaper.set_attack_ms(5.0 - amount);
                shaper.set_sustain_ms(100.0 * amount);
                start = end;
            }
            signal
        };
        let reference = render(512);
        for block in [1, 7, 64, 333, 6_000] {
            assert!(render(block) == reference, "blocks of {block}");
        }
    }

    #[test]
    fn it_stays_bounded_and_survives_garbage() {
        let mut shaper = prepared(
            TransientShaper::new()
                .with_attack_db(24.0)
                .with_sustain_db(24.0),
            2,
        );
        let mut signal = noise::<f64>(2, 24_000, 1);
        run_in_blocks(&mut shaper, &mut signal, 512);
        // The two gains together are at most 48 dB.
        assert!(strict_buffer_peak(&signal) <= 251.2);
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            shaper.set_attack_db(garbage);
            shaper.set_sustain_db(garbage);
            shaper.set_attack_ms(garbage);
            shaper.set_sustain_ms(garbage);
            let mut signal = noise::<f64>(2, 4_800, 2);
            run_in_blocks(&mut shaper, &mut signal, 512);
            let peak = strict_buffer_peak(&signal);
            assert!(peak <= 252.0, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_forgets_the_level() {
        let mut shaper = prepared(TransientShaper::new().with_attack_db(6.0), 1);
        let mut expected = hits(4_000, 0.5);
        run_in_blocks(&mut shaper, &mut expected, 512);
        let mut other = noise::<f64>(1, 3_000, 5);
        run_in_blocks(&mut shaper, &mut other, 512);
        assert!(shaper.gain_db() != 0.0);
        shaper.reset();
        assert_eq!(shaper.gain_db(), 0.0);
        let mut again = hits(4_000, 0.5);
        run_in_blocks(&mut shaper, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut shaper = TransientShaper::<f32>::new().with_attack_db(6.0);
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        shaper.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 2,
        });
        shaper.process(&mut buffer);
        assert!(buffer.channel(0)[10] > 0.4);
        assert_eq!(Processor::<f32>::latency_samples(&shaper), 0);
        shaper.process(&mut AudioBuffer::<f32>::new(0, 64));
    }
}
