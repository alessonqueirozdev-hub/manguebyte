//! The envelopes of a voice: delay, attack, hold, decay, sustain, release.

use crate::region::{EnvelopeSpec, EnvelopeStage};

/// A value for each stage, in [`EnvelopeStage`] order: seconds for the
/// times, percent for the start and sustain levels.
pub(crate) type Stages = [f64; EnvelopeStage::COUNT];

/// The level counted as silence: 90 dB below full scale.
pub(crate) const FLOOR: f64 = 3.162_277_660_168_379_4e-5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Delay,
    Attack,
    Hold,
    Decay,
    Sustain,
    Release,
    Done,
}

/// An envelope of one voice. Everything is counted in whole samples and
/// moves one sample per call, so the curve does not depend on how the
/// audio is cut into blocks.
///
/// Each stage's time is read when the stage starts, and so is each level
/// when the stage that heads for it starts (the start level with the
/// attack, the sustain level with the decay): what controllers add to
/// them ([`set_shift`](Self::set_shift)) changes the stages to come, not
/// the one under way.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Envelope {
    stage: Stage,
    level: f64,
    /// Samples spent in the current stage.
    elapsed: u64,
    /// Samples the current stage lasts.
    length: u64,
    /// The level the attack starts from.
    start: f64,
    /// The factor a decay or a release multiplies the level by per sample.
    factor: f64,
    sustain: f64,
    /// Each stage's value, velocity included.
    base: Stages,
    /// What controllers add to each stage's value.
    shift: Stages,
    sample_rate: f64,
}

/// Seconds to whole samples, never negative.
fn samples(seconds: f64, sample_rate: f64) -> u64 {
    let count = (seconds.max(0.0) * sample_rate).round();
    if count.is_finite() { count as u64 } else { 0 }
}

impl Envelope {
    pub(crate) const IDLE: Self = Self {
        stage: Stage::Done,
        level: 0.0,
        elapsed: 0,
        length: 0,
        start: 0.0,
        factor: 1.0,
        sustain: 1.0,
        base: [0.0; EnvelopeStage::COUNT],
        shift: [0.0; EnvelopeStage::COUNT],
        sample_rate: 48_000.0,
    };

    /// An envelope at the start of a note struck with `velocity` (0 to 1),
    /// with `shift` added to its stages by controllers.
    pub(crate) fn new(spec: &EnvelopeSpec, velocity: f64, sample_rate: f64, shift: Stages) -> Self {
        let base = [
            spec.delay + spec.vel2delay * velocity,
            spec.start,
            spec.attack + spec.vel2attack * velocity,
            spec.hold + spec.vel2hold * velocity,
            spec.decay + spec.vel2decay * velocity,
            spec.sustain + spec.vel2sustain * velocity,
            spec.release + spec.vel2release * velocity,
        ];
        let mut envelope = Self {
            stage: Stage::Delay,
            base,
            shift,
            sample_rate,
            ..Self::IDLE
        };
        envelope.enter(Stage::Delay);
        envelope
    }

    /// What controllers add to each stage from now on, in seconds or
    /// percent.
    pub(crate) fn set_shift(&mut self, shift: Stages) {
        self.shift = shift;
    }

    /// A stage's time, in whole samples.
    fn samples(&self, stage: EnvelopeStage) -> u64 {
        let at = stage as usize;
        samples(self.base[at] + self.shift[at], self.sample_rate)
    }

    /// A stage's level, 0 to 1.
    fn fraction(&self, stage: EnvelopeStage) -> f64 {
        let at = stage as usize;
        ((self.base[at] + self.shift[at]) / 100.0).clamp(0.0, 1.0)
    }

    /// The key was released.
    pub(crate) fn release(&mut self) {
        if matches!(self.stage, Stage::Release | Stage::Done) {
            return;
        }
        if self.level <= FLOOR || self.release_samples() == 0 {
            self.level = 0.0;
            self.enter(Stage::Done);
        } else {
            self.enter(Stage::Release);
        }
    }

    /// How many samples the release takes to fall 90 dB, if it starts now.
    pub(crate) fn release_samples(&self) -> u64 {
        self.samples(EnvelopeStage::Release)
    }

    /// Whether the envelope has reached silence for good.
    pub(crate) fn is_done(&self) -> bool {
        self.stage == Stage::Done
    }

    /// The level produced last.
    pub(crate) fn level(&self) -> f64 {
        self.level
    }

    /// Whether the envelope is releasing or done.
    pub(crate) fn is_released(&self) -> bool {
        matches!(self.stage, Stage::Release | Stage::Done)
    }

    /// The level for this sample, then a step forward.
    #[inline]
    pub(crate) fn next(&mut self) -> f64 {
        loop {
            match self.stage {
                Stage::Delay => {
                    if self.elapsed < self.length {
                        self.elapsed += 1;
                        return 0.0;
                    }
                    self.enter(Stage::Attack);
                }
                Stage::Attack => {
                    if self.elapsed < self.length {
                        self.elapsed += 1;
                        self.level = self.start
                            + (1.0 - self.start) * self.elapsed as f64 / self.length as f64;
                        return self.level;
                    }
                    self.level = 1.0;
                    self.enter(Stage::Hold);
                }
                Stage::Hold => {
                    if self.elapsed < self.length {
                        self.elapsed += 1;
                        return self.level;
                    }
                    self.enter(Stage::Decay);
                }
                Stage::Decay => {
                    if self.elapsed < self.length {
                        self.elapsed += 1;
                        self.level *= self.factor;
                        return self.level;
                    }
                    self.level = self.sustain;
                    self.enter(Stage::Sustain);
                }
                Stage::Sustain => {
                    if self.sustain <= FLOOR {
                        self.level = 0.0;
                        self.enter(Stage::Done);
                        return 0.0;
                    }
                    return self.level;
                }
                Stage::Release => {
                    self.level *= self.factor;
                    if self.level <= FLOOR {
                        self.level = 0.0;
                        self.enter(Stage::Done);
                    }
                    return self.level;
                }
                Stage::Done => return 0.0,
            }
        }
    }

    /// Starts a stage, reading its time and the level it heads for.
    fn enter(&mut self, stage: Stage) {
        self.stage = stage;
        self.elapsed = 0;
        self.length = match stage {
            Stage::Delay => self.samples(EnvelopeStage::Delay),
            Stage::Attack => {
                self.start = self.fraction(EnvelopeStage::Start);
                self.samples(EnvelopeStage::Attack)
            }
            Stage::Hold => self.samples(EnvelopeStage::Hold),
            Stage::Decay => {
                // A straight line in decibels to the sustain level (or the
                // floor) in the decay's time.
                self.sustain = self.fraction(EnvelopeStage::Sustain);
                let length = self.samples(EnvelopeStage::Decay);
                self.factor = if length > 0 {
                    self.sustain.max(FLOOR).powf(1.0 / length as f64)
                } else {
                    1.0
                };
                length
            }
            Stage::Release => {
                // A straight line in decibels, 90 dB down in the release's
                // time.
                let length = self.release_samples();
                self.factor = if length > 0 {
                    FLOOR.powf(1.0 / length as f64)
                } else {
                    0.0
                };
                length
            }
            Stage::Sustain | Stage::Done => 0,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_SHIFT: Stages = [0.0; EnvelopeStage::COUNT];

    fn run(envelope: &mut Envelope, count: usize) -> Vec<f64> {
        (0..count).map(|_| envelope.next()).collect()
    }

    #[test]
    fn shifts_are_read_when_their_stage_starts() {
        let spec = EnvelopeSpec {
            attack: 2.0,
            hold: 1.0,
            sustain: 100.0,
            release: 2.0,
            ..EnvelopeSpec::default()
        };
        let mut shift = NO_SHIFT;
        shift[EnvelopeStage::Attack as usize] = 2.0;
        let mut envelope = Envelope::new(&spec, 1.0, 1.0, shift);
        assert_eq!(envelope.next(), 0.25, "four samples of attack");
        // Changed during the attack: the attack keeps its length, the hold
        // takes the new one.
        shift[EnvelopeStage::Attack as usize] = 0.0;
        shift[EnvelopeStage::Hold as usize] = 2.0;
        shift[EnvelopeStage::Sustain as usize] = -50.0;
        envelope.set_shift(shift);
        assert_eq!(run(&mut envelope, 3), [0.5, 0.75, 1.0]);
        assert_eq!(
            run(&mut envelope, 3),
            [1.0, 1.0, 1.0],
            "three samples of hold"
        );
        assert_eq!(envelope.next(), 0.5, "the sustain level read at the decay");
        shift[EnvelopeStage::Release as usize] = -2.0;
        envelope.set_shift(shift);
        assert_eq!(envelope.release_samples(), 0);
        envelope.release();
        assert!(envelope.is_done());
    }

    #[test]
    fn stages_last_their_whole_samples() {
        let spec = EnvelopeSpec {
            delay: 2.0,
            start: 50.0,
            attack: 2.0,
            hold: 1.0,
            decay: 2.0,
            sustain: 25.0,
            release: 0.0,
            ..EnvelopeSpec::default()
        };
        // One sample per second makes the counts readable.
        let mut envelope = Envelope::new(&spec, 1.0, 1.0, NO_SHIFT);
        let levels = run(&mut envelope, 8);
        assert_eq!(&levels[..5], &[0.0, 0.0, 0.75, 1.0, 1.0]);
        assert!((levels[5] - 0.5).abs() < 1e-12); // sqrt(0.25)
        assert!((levels[6] - 0.25).abs() < 1e-12);
        assert_eq!(levels[7], 0.25);
        envelope.release();
        assert!(envelope.is_done());
        assert_eq!(envelope.next(), 0.0);
    }

    #[test]
    fn release_falls_ninety_decibels_in_its_time() {
        let spec = EnvelopeSpec {
            release: 1.0,
            ..EnvelopeSpec::default()
        };
        let mut envelope = Envelope::new(&spec, 1.0, 1_000.0, NO_SHIFT);
        assert_eq!(envelope.next(), 1.0);
        envelope.release();
        assert!(envelope.is_released());
        let levels = run(&mut envelope, 1_001);
        assert!((levels[499] - FLOOR.sqrt()).abs() < 1e-9, "{}", levels[499]);
        assert!(levels[998] > 0.0);
        assert_eq!(levels[1_000], 0.0);
        assert!(envelope.is_done());
    }

    #[test]
    fn velocity_moves_the_stages() {
        let spec = EnvelopeSpec {
            attack: 1.0,
            vel2attack: -1.0,
            sustain: 50.0,
            vel2sustain: 50.0,
            ..EnvelopeSpec::default()
        };
        let mut loud = Envelope::new(&spec, 1.0, 10.0, NO_SHIFT);
        assert_eq!(loud.next(), 1.0);
        let mut soft = Envelope::new(&spec, 0.0, 10.0, NO_SHIFT);
        assert!((soft.next() - 0.1).abs() < 1e-12);
        let mut half = Envelope::new(&spec, 0.5, 10.0, NO_SHIFT);
        let levels = run(&mut half, 10);
        assert_eq!(levels[9], 0.75);
    }

    #[test]
    fn zero_sustain_ends_the_note_after_the_decay() {
        let spec = EnvelopeSpec {
            decay: 1.0,
            sustain: 0.0,
            ..EnvelopeSpec::default()
        };
        let mut envelope = Envelope::new(&spec, 1.0, 4.0, NO_SHIFT);
        let levels = run(&mut envelope, 7);
        assert!(levels[4] <= FLOOR * 1.000_001);
        assert_eq!(levels[5], 0.0);
        assert!(envelope.is_done());
        assert!(Envelope::IDLE.is_done());
    }
}
