//! The amplitude envelope of a voice: delay, attack, hold, decay, sustain,
//! release.

use crate::region::EnvelopeSpec;

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

/// The envelope of one voice. Everything is counted in whole samples and
/// moves one sample per call, so the curve does not depend on how the
/// audio is cut into blocks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Envelope {
    stage: Stage,
    level: f64,
    /// Samples spent in the current stage.
    elapsed: u64,
    delay: u64,
    start: f64,
    attack: u64,
    hold: u64,
    decay: u64,
    decay_factor: f64,
    sustain: f64,
    release: u64,
    release_factor: f64,
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
        delay: 0,
        start: 0.0,
        attack: 0,
        hold: 0,
        decay: 0,
        decay_factor: 1.0,
        sustain: 1.0,
        release: 0,
        release_factor: 0.0,
    };

    /// An envelope at the start of a note struck with `velocity` (0 to 1).
    pub(crate) fn new(spec: &EnvelopeSpec, velocity: f64, sample_rate: f64) -> Self {
        let sustain = ((spec.sustain + spec.vel2sustain * velocity) / 100.0).clamp(0.0, 1.0);
        let decay = samples(spec.decay + spec.vel2decay * velocity, sample_rate);
        let release = samples(spec.release + spec.vel2release * velocity, sample_rate);
        // Straight lines in decibels: the decay reaches the sustain level
        // (or the floor) in `decay` samples; the release falls 90 dB in
        // `release` samples.
        let decay_factor = if decay > 0 {
            sustain.max(FLOOR).powf(1.0 / decay as f64)
        } else {
            1.0
        };
        let release_factor = if release > 0 {
            FLOOR.powf(1.0 / release as f64)
        } else {
            0.0
        };
        Self {
            stage: Stage::Delay,
            level: 0.0,
            elapsed: 0,
            delay: samples(spec.delay + spec.vel2delay * velocity, sample_rate),
            start: (spec.start / 100.0).clamp(0.0, 1.0),
            attack: samples(spec.attack + spec.vel2attack * velocity, sample_rate),
            hold: samples(spec.hold + spec.vel2hold * velocity, sample_rate),
            decay,
            decay_factor,
            sustain,
            release,
            release_factor,
        }
    }

    /// The key was released.
    pub(crate) fn release(&mut self) {
        if matches!(self.stage, Stage::Release | Stage::Done) {
            return;
        }
        self.stage = if self.level <= FLOOR || self.release == 0 {
            self.level = 0.0;
            Stage::Done
        } else {
            Stage::Release
        };
        self.elapsed = 0;
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
                    if self.elapsed < self.delay {
                        self.elapsed += 1;
                        return 0.0;
                    }
                    self.enter(Stage::Attack);
                }
                Stage::Attack => {
                    if self.elapsed < self.attack {
                        self.elapsed += 1;
                        self.level = self.start
                            + (1.0 - self.start) * self.elapsed as f64 / self.attack as f64;
                        return self.level;
                    }
                    self.level = 1.0;
                    self.enter(Stage::Hold);
                }
                Stage::Hold => {
                    if self.elapsed < self.hold {
                        self.elapsed += 1;
                        return self.level;
                    }
                    self.enter(Stage::Decay);
                }
                Stage::Decay => {
                    if self.elapsed < self.decay {
                        self.elapsed += 1;
                        self.level *= self.decay_factor;
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
                    self.level *= self.release_factor;
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

    fn enter(&mut self, stage: Stage) {
        self.stage = stage;
        self.elapsed = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(envelope: &mut Envelope, count: usize) -> Vec<f64> {
        (0..count).map(|_| envelope.next()).collect()
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
        let mut envelope = Envelope::new(&spec, 1.0, 1.0);
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
        let mut envelope = Envelope::new(&spec, 1.0, 1_000.0);
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
        let mut loud = Envelope::new(&spec, 1.0, 10.0);
        assert_eq!(loud.next(), 1.0);
        let mut soft = Envelope::new(&spec, 0.0, 10.0);
        assert!((soft.next() - 0.1).abs() < 1e-12);
        let mut half = Envelope::new(&spec, 0.5, 10.0);
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
        let mut envelope = Envelope::new(&spec, 1.0, 4.0);
        let levels = run(&mut envelope, 7);
        assert!(levels[4] <= FLOOR * 1.000_001);
        assert_eq!(levels[5], 0.0);
        assert!(envelope.is_done());
        assert!(Envelope::IDLE.is_done());
    }
}
