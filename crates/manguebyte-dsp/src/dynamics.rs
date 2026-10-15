use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, Sample};

use crate::{ProcessContext, ProcessSpec, Processor};

/// Level below which a signal is treated as silence, in decibels.
const FLOOR_DB: f64 = -120.0;

fn to_db(level: f64) -> f64 {
    if level > 1e-6 {
        20.0 * level.log10()
    } else {
        FLOOR_DB
    }
}

fn to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// One-pole smoothing coefficient that covers 63% of a step in
/// `milliseconds`. Zero or less means "instantly".
fn time_coefficient(milliseconds: f64, sample_rate: f64) -> f64 {
    if milliseconds <= 0.0 {
        0.0
    } else {
        (-1.0 / (milliseconds * 1e-3 * sample_rate)).exp()
    }
}

/// Which signal a [`Compressor`], [`NoiseGate`] or [`Expander`] measures to
/// decide its gain.
///
/// The default is the processor's own input. With
/// [`Sidechain`](Self::Sidechain) it measures the
/// [`sidechain`](ProcessContext::sidechain) of the context given to
/// [`process_with`](Processor::process_with), and falls back to its input
/// when the context has none or when it is called through
/// [`process`](Processor::process). Each processor chooses on its own, so a
/// chain can gate on its input and duck on a sidechain.
///
/// A wrapper that calls its inner processor through `process` does not pass
/// the sidechain on; [`Oversampled`](crate::Oversampled) is one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KeySource {
    /// The processor's own input.
    #[default]
    Input,
    /// The context's sidechain, when it has one.
    Sidechain,
}

/// The level that keys a dynamics processor at `frame`: the sidechain's when
/// `context` is given and has one, the input's otherwise.
#[inline]
fn key_level<S: Sample>(
    buffer: &AudioBuffer<S>,
    context: Option<&ProcessContext<'_, S>>,
    frame: usize,
) -> f64 {
    context
        .and_then(|context| context.sidechain_peak(frame))
        .unwrap_or_else(|| peak_at(buffer, frame))
}

/// A feed-forward compressor: it turns down what is louder than a threshold.
///
/// The level is measured across all channels together, so a stereo image does
/// not shift when only one side is loud. The gain computer and the smoothing
/// both work in decibels, following D. Giannoulis, M. Massberg and J. D.
/// Reiss, "Digital dynamic range compressor design: a tutorial and analysis",
/// Journal of the Audio Engineering Society, 2012.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Compressor, ProcessSpec, Processor};
///
/// let mut compressor = Compressor::<f32>::new()
///     .with_threshold_db(-20.0)
///     .with_ratio(4.0)
///     .with_attack_ms(5.0)
///     .with_release_ms(100.0);
/// compressor.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 1 });
///
/// // A full-scale signal is 20 dB over the threshold, so 4:1 leaves 5 dB of
/// // that: 15 dB of gain reduction once the attack has passed.
/// let mut block = AudioBuffer::<f32>::new(1, 4_800);
/// block.channel_mut(0).fill(1.0);
/// compressor.process(&mut block);
/// assert!((compressor.gain_reduction_db() - 15.0).abs() < 0.1);
/// ```
#[derive(Debug, Clone)]
pub struct Compressor<S: Sample = f32> {
    threshold_db: f64,
    ratio: f64,
    knee_db: f64,
    attack_ms: f64,
    release_ms: f64,
    makeup_db: f64,
    sample_rate: f64,
    attack: f64,
    release: f64,
    makeup: f64,
    key: KeySource,
    /// Smoothed gain reduction in decibels, zero or positive.
    reduction_db: f64,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for Compressor<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Compressor<S> {
    /// Creates a compressor with a threshold of -18 dB, a ratio of 4:1, a
    /// hard knee, 10 ms attack and 100 ms release.
    pub fn new() -> Self {
        let mut compressor = Self {
            threshold_db: -18.0,
            ratio: 4.0,
            knee_db: 0.0,
            attack_ms: 10.0,
            release_ms: 100.0,
            makeup_db: 0.0,
            sample_rate: 44_100.0,
            attack: 0.0,
            release: 0.0,
            makeup: 1.0,
            key: KeySource::Input,
            reduction_db: 0.0,
            marker: PhantomData,
        };
        compressor.update();
        compressor
    }

    /// Sets the level above which gain is reduced, in decibels.
    pub fn set_threshold_db(&mut self, db: f64) {
        self.threshold_db = db;
    }

    /// Sets how strongly the excess is reduced: `4.0` lets 1 dB out for every
    /// 4 dB over the threshold. Values below 1 are treated as 1; infinity
    /// makes a limiter.
    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.max(1.0);
    }

    /// Sets the width, in decibels, of the region around the threshold where
    /// compression eases in. Zero is a hard knee.
    pub fn set_knee_db(&mut self, db: f64) {
        self.knee_db = db.max(0.0);
    }

    /// Sets how quickly gain is reduced when the level rises, in milliseconds.
    pub fn set_attack_ms(&mut self, milliseconds: f64) {
        self.attack_ms = milliseconds;
        self.update();
    }

    /// Sets how quickly gain recovers when the level falls, in milliseconds.
    pub fn set_release_ms(&mut self, milliseconds: f64) {
        self.release_ms = milliseconds;
        self.update();
    }

    /// Sets the gain applied after compression, in decibels.
    pub fn set_makeup_db(&mut self, db: f64) {
        self.makeup_db = db;
        self.update();
    }

    /// Builder form of [`set_threshold_db`](Self::set_threshold_db).
    #[must_use]
    pub fn with_threshold_db(mut self, db: f64) -> Self {
        self.set_threshold_db(db);
        self
    }

    /// Builder form of [`set_ratio`](Self::set_ratio).
    #[must_use]
    pub fn with_ratio(mut self, ratio: f64) -> Self {
        self.set_ratio(ratio);
        self
    }

    /// Builder form of [`set_knee_db`](Self::set_knee_db).
    #[must_use]
    pub fn with_knee_db(mut self, db: f64) -> Self {
        self.set_knee_db(db);
        self
    }

    /// Builder form of [`set_attack_ms`](Self::set_attack_ms).
    #[must_use]
    pub fn with_attack_ms(mut self, milliseconds: f64) -> Self {
        self.set_attack_ms(milliseconds);
        self
    }

    /// Builder form of [`set_release_ms`](Self::set_release_ms).
    #[must_use]
    pub fn with_release_ms(mut self, milliseconds: f64) -> Self {
        self.set_release_ms(milliseconds);
        self
    }

    /// Builder form of [`set_makeup_db`](Self::set_makeup_db).
    #[must_use]
    pub fn with_makeup_db(mut self, db: f64) -> Self {
        self.set_makeup_db(db);
        self
    }

    /// Chooses what the level is measured on: the input (the default) or the
    /// sidechain. See [`KeySource`].
    pub fn set_key_source(&mut self, key: KeySource) {
        self.key = key;
    }

    /// Builder form of [`set_key_source`](Self::set_key_source).
    #[must_use]
    pub fn with_key_source(mut self, key: KeySource) -> Self {
        self.set_key_source(key);
        self
    }

    /// What the level is measured on.
    pub fn key_source(&self) -> KeySource {
        self.key
    }

    /// How much the gain is currently being reduced, in decibels, as a
    /// positive number. For driving a meter.
    pub fn gain_reduction_db(&self) -> f64 {
        self.reduction_db
    }

    /// The gain reduction, in decibels, that a steady input at `level_db`
    /// settles at.
    pub fn static_reduction_db(&self, level_db: f64) -> f64 {
        let over = level_db - self.threshold_db;
        let slope = 1.0 - 1.0 / self.ratio;
        if 2.0 * over <= -self.knee_db {
            0.0
        } else if 2.0 * over.abs() < self.knee_db {
            let into_knee = over + self.knee_db / 2.0;
            slope * into_knee * into_knee / (2.0 * self.knee_db)
        } else {
            slope * over
        }
    }

    fn update(&mut self) {
        self.attack = time_coefficient(self.attack_ms, self.sample_rate);
        self.release = time_coefficient(self.release_ms, self.sample_rate);
        self.makeup = to_gain(self.makeup_db);
    }
}

impl<S: Sample> Processor<S> for Compressor<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.update();
        self.reduction_db = 0.0;
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.run(buffer, None);
    }

    /// Like [`process`](Processor::process). With the key source set to
    /// [`KeySource::Sidechain`] and a sidechain in the context, the level is
    /// measured on the sidechain instead of the input, from its frame
    /// [`offset`](ProcessContext::offset) on; frames the sidechain lacks
    /// count as silence.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        let key = (self.key == KeySource::Sidechain).then_some(context);
        self.run(buffer, key);
    }

    fn reset(&mut self) {
        self.reduction_db = 0.0;
    }
}

/// The largest magnitude across the channels of `source` at `frame`, or zero
/// past its end.
#[inline]
fn peak_at<S: Sample>(source: &AudioBuffer<S>, frame: usize) -> f64 {
    let mut level = 0.0f64;
    if frame < source.num_frames() {
        for channel in 0..source.num_channels() {
            level = level.max(source.channel(channel)[frame].to_f64().abs());
        }
    }
    level
}

impl<S: Sample> Compressor<S> {
    fn run(&mut self, buffer: &mut AudioBuffer<S>, key: Option<&ProcessContext<'_, S>>) {
        let channels = buffer.num_channels();
        for frame in 0..buffer.num_frames() {
            let level = key_level(buffer, key, frame);
            let wanted = self.static_reduction_db(to_db(level));
            let coefficient = if wanted > self.reduction_db {
                self.attack
            } else {
                self.release
            };
            self.reduction_db = wanted + coefficient * (self.reduction_db - wanted);

            let gain = S::from_f64(to_gain(-self.reduction_db) * self.makeup);
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }
}

/// A noise gate: it closes when the signal falls below a threshold.
///
/// The gate opens over the attack time once the level reaches the threshold,
/// stays open for the hold time after the level drops, then closes over the
/// release time. Levels are measured across all channels together.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{NoiseGate, ProcessSpec, Processor};
///
/// let mut gate = NoiseGate::<f32>::new().with_threshold_db(-40.0);
/// gate.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 1 });
///
/// // Hiss at -60 dB is below the threshold and is silenced.
/// let mut block = AudioBuffer::<f32>::new(1, 4_800);
/// block.channel_mut(0).fill(0.001);
/// gate.process(&mut block);
/// assert_eq!(block.peak(), 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct NoiseGate<S: Sample = f32> {
    threshold_db: f64,
    attack_ms: f64,
    hold_ms: f64,
    release_ms: f64,
    floor_db: f64,
    sample_rate: f64,
    threshold: f64,
    attack: f64,
    release: f64,
    floor: f64,
    hold_samples: u32,
    hold_left: u32,
    key: KeySource,
    /// Current gain, between the floor and 1.
    gain: f64,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for NoiseGate<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> NoiseGate<S> {
    /// Creates a gate with a threshold of -50 dB, 1 ms attack, 50 ms hold and
    /// 100 ms release, closing to silence.
    pub fn new() -> Self {
        let mut gate = Self {
            threshold_db: -50.0,
            attack_ms: 1.0,
            hold_ms: 50.0,
            release_ms: 100.0,
            floor_db: f64::NEG_INFINITY,
            sample_rate: 44_100.0,
            threshold: 0.0,
            attack: 0.0,
            release: 0.0,
            floor: 0.0,
            hold_samples: 0,
            hold_left: 0,
            key: KeySource::Input,
            gain: 0.0,
            marker: PhantomData,
        };
        gate.update();
        gate
    }

    /// Sets the level below which the gate closes, in decibels.
    pub fn set_threshold_db(&mut self, db: f64) {
        self.threshold_db = db;
        self.update();
    }

    /// Sets how quickly the gate opens, in milliseconds.
    pub fn set_attack_ms(&mut self, milliseconds: f64) {
        self.attack_ms = milliseconds;
        self.update();
    }

    /// Sets how long the gate stays open after the level drops, in
    /// milliseconds.
    pub fn set_hold_ms(&mut self, milliseconds: f64) {
        self.hold_ms = milliseconds;
        self.update();
    }

    /// Sets how quickly the gate closes, in milliseconds.
    pub fn set_release_ms(&mut self, milliseconds: f64) {
        self.release_ms = milliseconds;
        self.update();
    }

    /// Sets the gain of the closed gate, in decibels. Negative infinity, the
    /// default, closes to silence; `-20.0` only turns the noise down.
    pub fn set_floor_db(&mut self, db: f64) {
        self.floor_db = db;
        self.update();
    }

    /// Builder form of [`set_threshold_db`](Self::set_threshold_db).
    #[must_use]
    pub fn with_threshold_db(mut self, db: f64) -> Self {
        self.set_threshold_db(db);
        self
    }

    /// Builder form of [`set_attack_ms`](Self::set_attack_ms).
    #[must_use]
    pub fn with_attack_ms(mut self, milliseconds: f64) -> Self {
        self.set_attack_ms(milliseconds);
        self
    }

    /// Builder form of [`set_hold_ms`](Self::set_hold_ms).
    #[must_use]
    pub fn with_hold_ms(mut self, milliseconds: f64) -> Self {
        self.set_hold_ms(milliseconds);
        self
    }

    /// Builder form of [`set_release_ms`](Self::set_release_ms).
    #[must_use]
    pub fn with_release_ms(mut self, milliseconds: f64) -> Self {
        self.set_release_ms(milliseconds);
        self
    }

    /// Builder form of [`set_floor_db`](Self::set_floor_db).
    #[must_use]
    pub fn with_floor_db(mut self, db: f64) -> Self {
        self.set_floor_db(db);
        self
    }

    /// Chooses what the gate listens to: the input (the default) or the
    /// sidechain. See [`KeySource`].
    pub fn set_key_source(&mut self, key: KeySource) {
        self.key = key;
    }

    /// Builder form of [`set_key_source`](Self::set_key_source).
    #[must_use]
    pub fn with_key_source(mut self, key: KeySource) -> Self {
        self.set_key_source(key);
        self
    }

    /// What the gate listens to.
    pub fn key_source(&self) -> KeySource {
        self.key
    }

    /// Whether the gate is currently more open than closed.
    pub fn is_open(&self) -> bool {
        self.gain > 0.5 * (1.0 + self.floor)
    }

    fn update(&mut self) {
        self.threshold = to_gain(self.threshold_db);
        self.attack = time_coefficient(self.attack_ms, self.sample_rate);
        self.release = time_coefficient(self.release_ms, self.sample_rate);
        self.floor = if self.floor_db <= FLOOR_DB {
            0.0
        } else {
            to_gain(self.floor_db).min(1.0)
        };
        self.hold_samples = (self.hold_ms.max(0.0) * 1e-3 * self.sample_rate).round() as u32;
    }
}

impl<S: Sample> Processor<S> for NoiseGate<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.update();
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.run(buffer, None);
    }

    /// Like [`process`](Processor::process). With the key source set to
    /// [`KeySource::Sidechain`] and a sidechain in the context, the gate opens
    /// and closes on the sidechain's level instead of the input's, from its
    /// frame [`offset`](ProcessContext::offset) on; frames the sidechain lacks
    /// count as silence.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        let key = (self.key == KeySource::Sidechain).then_some(context);
        self.run(buffer, key);
    }

    fn reset(&mut self) {
        self.gain = self.floor;
        self.hold_left = 0;
    }
}

impl<S: Sample> NoiseGate<S> {
    fn run(&mut self, buffer: &mut AudioBuffer<S>, key: Option<&ProcessContext<'_, S>>) {
        let channels = buffer.num_channels();
        for frame in 0..buffer.num_frames() {
            let level = key_level(buffer, key, frame);

            let (target, coefficient) = if level >= self.threshold {
                self.hold_left = self.hold_samples;
                (1.0, self.attack)
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
                (1.0, self.attack)
            } else {
                (self.floor, self.release)
            };
            self.gain = target + coefficient * (self.gain - target);
            // An exponential approach never arrives; finish the last step so
            // a closed gate is truly silent.
            if (self.gain - target).abs() < 1e-5 {
                self.gain = target;
            }

            let gain = S::from_f64(self.gain);
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }
}

/// The deepest reduction an [`Expander`] applies, in decibels. Far below
/// what any sample format resolves.
const MAX_RANGE_DB: f64 = 240.0;

/// A smoothing time with not-a-number read as "instantly".
fn sane_ms(milliseconds: f64) -> f64 {
    if milliseconds.is_nan() {
        0.0
    } else {
        milliseconds
    }
}

/// A downward expander: it turns down what is quieter than a threshold, and
/// the quieter, the more.
///
/// Below the threshold every decibel of input becomes `ratio` decibels of
/// output, so with a ratio of 2 a signal 10 dB under the threshold comes out
/// 20 dB under it. The reduction never exceeds the range. A soft knee eases
/// the curve in around the threshold. Above the threshold nothing changes.
///
/// It is the mirror image of the [`Compressor`] and is built the same way: the
/// level is the peak across all channels, measured on every frame, and the
/// gain computer and smoothing work in decibels (Giannoulis, Massberg and
/// Reiss, 2012). Attack is how quickly it opens when the level rises,
/// release how quickly it closes when the level falls. With the key source
/// set to [`KeySource::Sidechain`], the level is measured on the sidechain of
/// the [`ProcessContext`] instead.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{Expander, ProcessSpec, Processor};
///
/// let mut expander = Expander::<f32>::new()
///     .with_threshold_db(-40.0)
///     .with_ratio(2.0)
///     .with_range_db(30.0);
/// expander.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 48_000, num_channels: 1 });
///
/// // A steady -50 dB is 10 dB under the threshold: it is turned down by
/// // another 10 dB once the expander has settled.
/// let mut block = AudioBuffer::<f32>::new(1, 48_000);
/// block.channel_mut(0).fill(10f32.powf(-50.0 / 20.0));
/// expander.process(&mut block);
/// assert!((expander.gain_reduction_db() - 10.0).abs() < 0.01);
/// ```
#[derive(Debug, Clone)]
pub struct Expander<S: Sample = f32> {
    threshold_db: f64,
    ratio: f64,
    knee_db: f64,
    attack_ms: f64,
    release_ms: f64,
    range_db: f64,
    sample_rate: f64,
    attack: f64,
    release: f64,
    key: KeySource,
    /// Smoothed gain reduction in decibels, zero or positive.
    reduction_db: f64,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for Expander<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Expander<S> {
    /// Creates an expander with a threshold of -40 dB, a ratio of 2:1, a hard
    /// knee, 1 ms attack, 100 ms release and a range of 40 dB.
    pub fn new() -> Self {
        let mut expander = Self {
            threshold_db: -40.0,
            ratio: 2.0,
            knee_db: 0.0,
            attack_ms: 1.0,
            release_ms: 100.0,
            range_db: 40.0,
            sample_rate: 44_100.0,
            attack: 0.0,
            release: 0.0,
            key: KeySource::Input,
            reduction_db: 0.0,
            marker: PhantomData,
        };
        expander.update();
        expander
    }

    /// Sets the level below which gain is reduced, in decibels.
    pub fn set_threshold_db(&mut self, db: f64) {
        self.threshold_db = db;
    }

    /// Sets how steeply the level falls below the threshold: `2.0` makes
    /// every decibel under it two. Values below 1 (and not-a-number) are
    /// treated as 1, which does nothing; infinity makes a gate.
    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.max(1.0);
    }

    /// Sets the width, in decibels, of the region around the threshold where
    /// the expansion eases in. Zero is a hard knee.
    pub fn set_knee_db(&mut self, db: f64) {
        self.knee_db = db.max(0.0);
    }

    /// Sets how quickly the expander opens when the level rises, in
    /// milliseconds.
    pub fn set_attack_ms(&mut self, milliseconds: f64) {
        self.attack_ms = sane_ms(milliseconds);
        self.update();
    }

    /// Sets how quickly the expander closes when the level falls, in
    /// milliseconds.
    pub fn set_release_ms(&mut self, milliseconds: f64) {
        self.release_ms = sane_ms(milliseconds);
        self.update();
    }

    /// Sets the most the gain is ever reduced by, in decibels. Infinity is
    /// taken as 240 dB; negative values and not-a-number as zero.
    pub fn set_range_db(&mut self, db: f64) {
        self.range_db = if db.is_nan() {
            0.0
        } else {
            db.clamp(0.0, MAX_RANGE_DB)
        };
    }

    /// Builder form of [`set_threshold_db`](Self::set_threshold_db).
    #[must_use]
    pub fn with_threshold_db(mut self, db: f64) -> Self {
        self.set_threshold_db(db);
        self
    }

    /// Builder form of [`set_ratio`](Self::set_ratio).
    #[must_use]
    pub fn with_ratio(mut self, ratio: f64) -> Self {
        self.set_ratio(ratio);
        self
    }

    /// Builder form of [`set_knee_db`](Self::set_knee_db).
    #[must_use]
    pub fn with_knee_db(mut self, db: f64) -> Self {
        self.set_knee_db(db);
        self
    }

    /// Builder form of [`set_attack_ms`](Self::set_attack_ms).
    #[must_use]
    pub fn with_attack_ms(mut self, milliseconds: f64) -> Self {
        self.set_attack_ms(milliseconds);
        self
    }

    /// Builder form of [`set_release_ms`](Self::set_release_ms).
    #[must_use]
    pub fn with_release_ms(mut self, milliseconds: f64) -> Self {
        self.set_release_ms(milliseconds);
        self
    }

    /// Builder form of [`set_range_db`](Self::set_range_db).
    #[must_use]
    pub fn with_range_db(mut self, db: f64) -> Self {
        self.set_range_db(db);
        self
    }

    /// Chooses what the level is measured on: the input (the default) or the
    /// sidechain. See [`KeySource`].
    pub fn set_key_source(&mut self, key: KeySource) {
        self.key = key;
    }

    /// Builder form of [`set_key_source`](Self::set_key_source).
    #[must_use]
    pub fn with_key_source(mut self, key: KeySource) -> Self {
        self.set_key_source(key);
        self
    }

    /// What the level is measured on.
    pub fn key_source(&self) -> KeySource {
        self.key
    }

    /// How much the gain is currently being reduced, in decibels, as a
    /// positive number. For driving a meter.
    pub fn gain_reduction_db(&self) -> f64 {
        self.reduction_db
    }

    /// The gain reduction, in decibels, that a steady input at `level_db`
    /// settles at.
    pub fn static_reduction_db(&self, level_db: f64) -> f64 {
        let under = self.threshold_db - level_db;
        let slope = self.ratio - 1.0;
        let reduction = if 2.0 * under <= -self.knee_db {
            0.0
        } else if 2.0 * under.abs() < self.knee_db {
            let into_knee = under + self.knee_db / 2.0;
            slope * into_knee * into_knee / (2.0 * self.knee_db)
        } else {
            slope * under
        };
        // `min` also turns a not-a-number (an unknown threshold) into the
        // full range, which is the safe side.
        reduction.min(self.range_db)
    }

    fn update(&mut self) {
        self.attack = time_coefficient(self.attack_ms, self.sample_rate);
        self.release = time_coefficient(self.release_ms, self.sample_rate);
    }

    fn run(&mut self, buffer: &mut AudioBuffer<S>, key: Option<&ProcessContext<'_, S>>) {
        let channels = buffer.num_channels();
        for frame in 0..buffer.num_frames() {
            let level = key_level(buffer, key, frame);
            let wanted = self.static_reduction_db(to_db(level));
            let coefficient = if wanted < self.reduction_db {
                self.attack
            } else {
                self.release
            };
            self.reduction_db = wanted + coefficient * (self.reduction_db - wanted);

            let gain = S::from_f64(to_gain(-self.reduction_db));
            for channel in 0..channels {
                buffer.channel_mut(channel)[frame] *= gain;
            }
        }
    }
}

impl<S: Sample> Processor<S> for Expander<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.update();
        self.reduction_db = 0.0;
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.run(buffer, None);
    }

    /// Like [`process`](Processor::process). With the key source set to
    /// [`KeySource::Sidechain`] and a sidechain in the context, the level is
    /// measured on the sidechain instead of the input, from its frame
    /// [`offset`](ProcessContext::offset) on; frames the sidechain lacks
    /// count as silence.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        let key = (self.key == KeySource::Sidechain).then_some(context);
        self.run(buffer, key);
    }

    fn reset(&mut self) {
        self.reduction_db = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: ProcessSpec = ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 48_000,
        num_channels: 1,
    };

    fn steady(level: f64, frames: usize) -> AudioBuffer<f64> {
        let mut buffer = AudioBuffer::new(1, frames);
        buffer.channel_mut(0).fill(level);
        buffer
    }

    #[test]
    fn leaves_quiet_signals_alone() {
        let mut compressor = Compressor::<f64>::new().with_threshold_db(-12.0);
        compressor.prepare(&SPEC);
        let mut buffer = steady(to_gain(-20.0), 4_800);
        compressor.process(&mut buffer);
        assert!((buffer.channel(0)[4_799] - to_gain(-20.0)).abs() < 1e-12);
        assert_eq!(compressor.gain_reduction_db(), 0.0);
    }

    #[test]
    fn steady_state_follows_the_ratio() {
        for (ratio, expected_out_db) in [(2.0, -15.0), (4.0, -17.5), (f64::INFINITY, -20.0)] {
            let mut compressor = Compressor::<f64>::new()
                .with_threshold_db(-20.0)
                .with_ratio(ratio)
                .with_attack_ms(1.0);
            compressor.prepare(&SPEC);
            let mut buffer = steady(to_gain(-10.0), 9_600);
            compressor.process(&mut buffer);
            let out_db = to_db(buffer.channel(0)[9_599]);
            assert!(
                (out_db - expected_out_db).abs() < 0.01,
                "ratio {ratio}: {out_db} dB"
            );
        }
    }

    #[test]
    fn soft_knee_is_continuous_and_eases_in() {
        let compressor = Compressor::<f64>::new()
            .with_threshold_db(-20.0)
            .with_ratio(4.0)
            .with_knee_db(12.0);
        // Below the knee: nothing. Above it: the straight line.
        assert_eq!(compressor.static_reduction_db(-26.0), 0.0);
        assert!((compressor.static_reduction_db(-14.0) - 0.75 * 6.0).abs() < 1e-12);
        // At the threshold a hard knee does nothing; a soft one already acts.
        let at_threshold = compressor.static_reduction_db(-20.0);
        assert!(at_threshold > 0.0 && at_threshold < 0.75 * 6.0);
        // No jumps anywhere along the curve.
        let mut previous = 0.0;
        for step in 0..=400 {
            let level = -30.0 + 0.05 * f64::from(step);
            let reduction = compressor.static_reduction_db(level);
            assert!(reduction >= previous && reduction - previous < 0.05);
            previous = reduction;
        }
    }

    #[test]
    fn attack_and_release_take_their_stated_time() {
        let mut compressor = Compressor::<f64>::new()
            .with_threshold_db(-20.0)
            .with_ratio(f64::INFINITY)
            .with_attack_ms(10.0)
            .with_release_ms(100.0);
        compressor.prepare(&SPEC);

        // 20 dB over the threshold; after one attack time, 63% of the way.
        let mut loud = steady(1.0, 480);
        compressor.process(&mut loud);
        let after_attack = compressor.gain_reduction_db();
        assert!((after_attack - 20.0 * (1.0 - (-1.0f64).exp())).abs() < 0.1);

        let mut settle = steady(1.0, 48_000);
        compressor.process(&mut settle);
        assert!((compressor.gain_reduction_db() - 20.0).abs() < 1e-6);

        // Silence: after one release time, 37% of the reduction is left.
        let mut quiet = steady(0.0, 4_800);
        compressor.process(&mut quiet);
        assert!((compressor.gain_reduction_db() - 20.0 * (-1.0f64).exp()).abs() < 0.1);
    }

    #[test]
    fn channels_are_linked_and_makeup_is_applied() {
        let mut compressor = Compressor::<f64>::new()
            .with_threshold_db(-20.0)
            .with_ratio(f64::INFINITY)
            .with_attack_ms(0.0)
            .with_makeup_db(6.0);
        compressor.prepare(&ProcessSpec {
            num_channels: 2,
            ..SPEC
        });
        let mut buffer = AudioBuffer::<f64>::new(2, 64);
        buffer.channel_mut(0).fill(1.0);
        buffer.channel_mut(1).fill(0.01);
        compressor.process(&mut buffer);
        // Both sides are turned down by the same 20 dB, then up by 6.
        assert!((to_db(buffer.channel(0)[63]) + 14.0).abs() < 1e-6);
        assert!((to_db(buffer.channel(1)[63]) + 54.0).abs() < 1e-6);
    }

    #[test]
    fn gate_opens_holds_and_closes() {
        let mut gate = NoiseGate::<f64>::new()
            .with_threshold_db(-30.0)
            .with_attack_ms(1.0)
            .with_hold_ms(20.0)
            .with_release_ms(10.0);
        gate.prepare(&SPEC);
        assert!(!gate.is_open());

        let mut loud = steady(0.5, 960);
        gate.process(&mut loud);
        assert!(gate.is_open());
        assert!((loud.channel(0)[959] - 0.5).abs() < 1e-6);

        // Just inside the hold time, it is still fully open.
        let mut quiet = steady(0.001, 900);
        gate.process(&mut quiet);
        assert!(gate.is_open());
        assert!((quiet.channel(0)[899] - 0.001).abs() < 1e-9);

        // Well after hold and release, it is silent.
        let mut tail = steady(0.001, 9_600);
        gate.process(&mut tail);
        assert!(!gate.is_open());
        assert_eq!(tail.channel(0)[9_599], 0.0);
    }

    #[test]
    fn gate_floor_attenuates_instead_of_silencing() {
        let mut gate = NoiseGate::<f64>::new()
            .with_threshold_db(-30.0)
            .with_floor_db(-20.0);
        gate.prepare(&SPEC);
        let mut buffer = steady(0.001, 4_800);
        gate.process(&mut buffer);
        assert!((buffer.channel(0)[4_799] - 0.0001).abs() < 1e-9);
    }

    #[test]
    fn expander_static_curve_follows_ratio_and_range() {
        let expander = Expander::<f64>::new()
            .with_threshold_db(-40.0)
            .with_ratio(2.0)
            .with_range_db(20.0);
        for (level, expected) in [
            (0.0, 0.0),
            (-30.0, 0.0),
            (-40.0, 0.0),
            (-50.0, 10.0),
            (-55.0, 15.0),
            (-70.0, 20.0),
            (-200.0, 20.0),
        ] {
            assert_eq!(expander.static_reduction_db(level), expected, "{level} dB");
        }
        let gate_like = Expander::<f64>::new()
            .with_ratio(f64::INFINITY)
            .with_range_db(f64::INFINITY);
        assert_eq!(gate_like.static_reduction_db(-40.0), 0.0);
        assert_eq!(gate_like.static_reduction_db(-40.001), MAX_RANGE_DB);
    }

    #[test]
    fn expander_soft_knee_is_continuous_and_eases_in() {
        let expander = Expander::<f64>::new()
            .with_threshold_db(-40.0)
            .with_ratio(3.0)
            .with_knee_db(10.0)
            .with_range_db(100.0);
        assert_eq!(expander.static_reduction_db(-35.0), 0.0);
        assert!((expander.static_reduction_db(-45.0) - 2.0 * 5.0).abs() < 1e-12);
        let at_threshold = expander.static_reduction_db(-40.0);
        assert!(at_threshold > 0.0 && at_threshold < 10.0, "{at_threshold}");
        // Falling level, rising reduction, and no jumps.
        let mut previous = 0.0;
        for step in 0..=400 {
            let level = -30.0 - 0.05 * f64::from(step);
            let reduction = expander.static_reduction_db(level);
            assert!(
                reduction >= previous && reduction - previous < 0.11,
                "{level}"
            );
            previous = reduction;
        }
    }

    #[test]
    fn expander_settles_at_the_ratio() {
        let mut expander = Expander::<f64>::new()
            .with_threshold_db(-40.0)
            .with_ratio(2.0);
        expander.prepare(&SPEC);
        let mut buffer = steady(to_gain(-50.0), 48_000);
        expander.process(&mut buffer);
        let out_db = to_db(buffer.channel(0)[47_999]);
        assert!((out_db + 60.0).abs() < 0.01, "{out_db} dB");
        // Above the threshold the signal is untouched once it has opened.
        let mut loud = steady(to_gain(-20.0), 4_800);
        expander.process(&mut loud);
        assert!((to_db(loud.channel(0)[4_799]) + 20.0).abs() < 1e-6);
    }

    #[test]
    fn expander_attack_and_release_take_their_stated_time() {
        let mut expander = Expander::<f64>::new()
            .with_threshold_db(-40.0)
            .with_ratio(f64::INFINITY)
            .with_range_db(40.0)
            .with_attack_ms(10.0)
            .with_release_ms(100.0);
        expander.prepare(&SPEC);

        // Silence: after one release time, 63% of the way to the range.
        let mut quiet = steady(0.0, 4_800);
        expander.process(&mut quiet);
        let after_release = expander.gain_reduction_db();
        assert!((after_release - 40.0 * (1.0 - (-1.0f64).exp())).abs() < 0.01);
        let mut settle = steady(0.0, 48_000);
        expander.process(&mut settle);
        assert!((expander.gain_reduction_db() - 40.0).abs() < 1e-3);

        // A loud signal: after one attack time, 37% of the range is left.
        let mut loud = steady(0.5, 480);
        expander.process(&mut loud);
        assert!((expander.gain_reduction_db() - 40.0 * (-1.0f64).exp()).abs() < 0.01);
    }

    #[test]
    fn expander_never_boosts_and_survives_hostile_settings() {
        use crate::testing::Random;
        const VALUES: [f64; 9] = [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            0.0,
            -1.0,
            1e300,
            -1e300,
            3.0,
            -60.0,
        ];
        let mut random = Random::new(3);
        let mut pick = || VALUES[(random.unit() * VALUES.len() as f64) as usize];
        for _ in 0..300 {
            let mut expander = Expander::<f64>::new()
                .with_threshold_db(pick())
                .with_ratio(pick())
                .with_knee_db(pick())
                .with_attack_ms(pick())
                .with_release_ms(pick())
                .with_range_db(pick());
            expander.prepare(&SPEC);
            let mut buffer = crate::testing::noise::<f64>(2, 256, 7);
            let level = pick();
            buffer.apply_gain(if level.is_nan() {
                1.0
            } else {
                level.clamp(-1e3, 1e3)
            });
            let scaled = buffer.clone();
            expander.process(&mut buffer);
            for channel in 0..2 {
                for (out, inp) in buffer.channel(channel).iter().zip(scaled.channel(channel)) {
                    assert!(out.is_finite(), "{expander:?}");
                    assert!(out.abs() <= inp.abs() + 1e-12, "{expander:?}");
                }
            }
            assert!(expander.gain_reduction_db().is_finite());
        }
    }

    #[test]
    fn a_sidechain_drives_the_compressor_the_gate_and_the_expander() {
        use crate::Transport;
        use crate::event::EventBuffer;
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        let key_loud = steady(1.0, 4_800);
        let key_silent = steady(0.0, 4_800);
        let loud = ProcessContext::new(&events, &transport).with_sidechain(&key_loud);
        let silent = ProcessContext::new(&events, &transport).with_sidechain(&key_silent);

        // A quiet input ducks under a loud key: 20 dB over at 4:1 is 15 dB.
        let mut compressor = Compressor::<f64>::new()
            .with_threshold_db(-20.0)
            .with_ratio(4.0)
            .with_attack_ms(0.0)
            .with_key_source(KeySource::Sidechain);
        compressor.prepare(&SPEC);
        let mut buffer = steady(0.01, 4_800);
        compressor.process_with(&mut buffer, &loud);
        assert!((to_db(buffer.channel(0)[4_799]) + 55.0).abs() < 1e-9);

        // A loud input is shut by a silent key, and let through by a loud one.
        let mut gate = NoiseGate::<f64>::new()
            .with_threshold_db(-30.0)
            .with_key_source(KeySource::Sidechain);
        gate.prepare(&SPEC);
        let mut buffer = steady(0.5, 4_800);
        gate.process_with(&mut buffer, &silent);
        assert_eq!(buffer.channel(0)[4_799], 0.0);
        let mut buffer = steady(0.5, 4_800);
        gate.process_with(&mut buffer, &loud);
        assert!((buffer.channel(0)[4_799] - 0.5).abs() < 1e-9);

        let mut expander = Expander::<f64>::new()
            .with_threshold_db(-30.0)
            .with_range_db(24.0)
            .with_release_ms(1.0)
            .with_key_source(KeySource::Sidechain);
        expander.prepare(&SPEC);
        let mut buffer = steady(0.5, 4_800);
        expander.process_with(&mut buffer, &silent);
        assert!((to_db(buffer.channel(0)[4_799]) - (to_db(0.5) - 24.0)).abs() < 1e-6);
    }

    #[test]
    fn a_short_or_empty_sidechain_counts_as_silence() {
        use crate::Transport;
        use crate::event::EventBuffer;
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        let short_key = steady(1.0, 10);
        let no_channels = AudioBuffer::<f64>::new(0, 20);
        let mut gate = NoiseGate::<f64>::new()
            .with_threshold_db(-30.0)
            .with_attack_ms(0.0)
            .with_hold_ms(0.0)
            .with_release_ms(0.0)
            .with_key_source(KeySource::Sidechain);
        gate.prepare(&SPEC);
        let mut buffer = steady(0.5, 20);
        gate.process_with(
            &mut buffer,
            &ProcessContext::new(&events, &transport).with_sidechain(&short_key),
        );
        let mut expected = [0.0; 20];
        expected[..10].fill(0.5);
        assert_eq!(buffer.channel(0), &expected);

        let mut buffer = steady(0.5, 20);
        gate.process_with(
            &mut buffer,
            &ProcessContext::new(&events, &transport).with_sidechain(&no_channels),
        );
        assert_eq!(buffer.channel(0), &[0.0; 20]);
    }

    fn bits(buffer: &AudioBuffer<f64>) -> Vec<u64> {
        buffer
            .iter_channels()
            .flat_map(|channel| channel.iter().map(|s| s.to_bits()))
            .collect()
    }

    /// Runs copies of a processor over the same noise: one through
    /// `process`, one through `process_with` with an empty context and one
    /// through `process_with` with `context`. All three must agree.
    fn same_with_and_without_context<P: Processor<f64>>(
        make: impl Fn() -> P,
        context: &ProcessContext<'_, f64>,
    ) {
        let spec = ProcessSpec {
            num_channels: 2,
            ..SPEC
        };
        let input = crate::testing::noise::<f64>(2, 9_600, 11);
        let (mut a, mut b, mut c) = (make(), make(), make());
        a.prepare(&spec);
        b.prepare(&spec);
        c.prepare(&spec);
        let (mut x, mut y, mut z) = (input.clone(), input.clone(), input.clone());
        a.process(&mut x);
        b.process_with(&mut y, &ProcessContext::empty());
        c.process_with(&mut z, context);
        assert_eq!(bits(&x), bits(&y));
        assert_eq!(bits(&x), bits(&z));
        assert_ne!(bits(&x), bits(&input));
    }

    #[test]
    fn without_a_sidechain_process_with_is_process_bit_for_bit() {
        // Keyed on the sidechain, but given none: the input keys.
        let empty = ProcessContext::empty();
        same_with_and_without_context(
            || {
                Compressor::<f64>::new()
                    .with_threshold_db(-12.0)
                    .with_knee_db(6.0)
                    .with_makeup_db(3.0)
                    .with_key_source(KeySource::Sidechain)
            },
            &empty,
        );
        same_with_and_without_context(
            || {
                NoiseGate::<f64>::new()
                    .with_threshold_db(-3.0)
                    .with_key_source(KeySource::Sidechain)
            },
            &empty,
        );
        same_with_and_without_context(
            || {
                Expander::<f64>::new()
                    .with_threshold_db(-3.0)
                    .with_key_source(KeySource::Sidechain)
            },
            &empty,
        );
    }

    #[test]
    fn keyed_on_the_input_a_connected_sidechain_changes_nothing() {
        // A plugin with a sidechain port always passes one, silent when the
        // host leaves it unconnected. By default it must not key anything.
        use crate::Transport;
        use crate::event::EventBuffer;
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        let silent_key = AudioBuffer::<f64>::new(2, 9_600);
        let loud_key = steady(1.0, 9_600);
        for key in [&silent_key, &loud_key] {
            let context = ProcessContext::new(&events, &transport).with_sidechain(key);
            same_with_and_without_context(
                || {
                    Compressor::<f64>::new()
                        .with_threshold_db(-12.0)
                        .with_makeup_db(3.0)
                },
                &context,
            );
            same_with_and_without_context(
                || NoiseGate::<f64>::new().with_threshold_db(-3.0),
                &context,
            );
            same_with_and_without_context(
                || Expander::<f64>::new().with_threshold_db(-3.0),
                &context,
            );
        }
        assert_eq!(Compressor::<f64>::new().key_source(), KeySource::Input);
        assert_eq!(NoiseGate::<f64>::new().key_source(), KeySource::Input);
        assert_eq!(Expander::<f64>::new().key_source(), KeySource::Input);
    }

    #[test]
    fn in_one_chain_the_gate_listens_to_its_input_and_the_compressor_to_the_key() {
        use crate::Transport;
        use crate::event::EventBuffer;
        let events = EventBuffer::new();
        let transport = Transport::STOPPED;
        // The key is loud for the first half only.
        let mut key = AudioBuffer::<f64>::new(1, 8);
        key.channel_mut(0)[..4].fill(1.0);
        let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
        let mut chain = (
            NoiseGate::<f64>::new()
                .with_threshold_db(-20.0)
                .with_attack_ms(0.0)
                .with_hold_ms(0.0)
                .with_release_ms(0.0),
            Compressor::<f64>::new()
                .with_threshold_db(-20.0)
                .with_ratio(f64::INFINITY)
                .with_attack_ms(0.0)
                .with_release_ms(0.0)
                .with_key_source(KeySource::Sidechain),
        );
        chain.prepare(&SPEC);
        let mut block = steady(0.5, 8);
        chain.process_with(&mut block, &context);
        // The input (-6 dB) holds the gate open; the key ducks the first
        // half to the threshold: 0.5 * 0.1.
        let ducked = 0.5 * to_gain(-20.0);
        for frame in 0..4 {
            assert!((block.channel(0)[frame] - ducked).abs() < 1e-12, "{frame}");
        }
        assert_eq!(&block.channel(0)[4..], &[0.5; 4]);
    }

    fn note_at(offset: u32) -> crate::event::Event {
        crate::event::Event {
            offset,
            kind: crate::event::EventKind::NoteOn {
                note: crate::event::NoteRef::new(0, 60),
                velocity: 1.0,
            },
        }
    }

    #[test]
    fn a_sidechain_stays_aligned_when_the_block_is_split_at_events() {
        use crate::Transport;
        use crate::event::EventBuffer;
        let new_gate = || {
            let mut gate = NoiseGate::<f32>::new()
                .with_threshold_db(-20.0)
                .with_attack_ms(0.0)
                .with_hold_ms(0.0)
                .with_release_ms(0.0)
                .with_key_source(KeySource::Sidechain);
            gate.prepare(&SPEC);
            gate
        };
        let mut key = AudioBuffer::<f32>::new(1, 8);
        key.channel_mut(0)[4..].fill(1.0);
        let transport = Transport::STOPPED;
        for offsets in [&[4][..], &[1, 4, 6], &[0, 7, 7, 30]] {
            let mut events = EventBuffer::with_capacity(4);
            for &offset in offsets {
                events.push(note_at(offset)).unwrap();
            }
            let context = ProcessContext::new(&events, &transport).with_sidechain(&key);

            // Whole block: closed for four frames, open for four.
            let mut gate = new_gate();
            let mut whole = AudioBuffer::<f32>::new(1, 8);
            whole.channel_mut(0).fill(1.0);
            gate.process_with(&mut whole, &context);
            assert_eq!(whole.channel(0), &[0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0]);

            // Split at the events, as a plugin applying them on their frame.
            let mut gate = new_gate();
            let mut split = AudioBuffer::<f32>::new(1, 8);
            split.channel_mut(0).fill(1.0);
            crate::split_at_events_with(
                &mut gate,
                &mut split,
                &context,
                |_, _| {},
                |gate, piece, piece_context| gate.process_with(piece, piece_context),
            );
            assert_eq!(split.channel(0), whole.channel(0), "events at {offsets:?}");
        }
    }

    #[test]
    fn a_sidechained_compressor_ducks_on_the_right_frames_in_split_blocks() {
        use crate::Transport;
        use crate::event::EventBuffer;
        let mut compressor = Compressor::<f32>::new()
            .with_threshold_db(-20.0)
            .with_ratio(f64::INFINITY)
            .with_attack_ms(0.0)
            .with_release_ms(0.0)
            .with_key_source(KeySource::Sidechain);
        compressor.prepare(&SPEC);
        let mut key = AudioBuffer::<f32>::new(1, 8);
        key.channel_mut(0)[..4].fill(1.0);
        let transport = Transport::STOPPED;
        let mut events = EventBuffer::with_capacity(4);
        events.push(note_at(4)).unwrap();
        let context = ProcessContext::new(&events, &transport).with_sidechain(&key);
        let mut block = AudioBuffer::<f32>::new(1, 8);
        block.channel_mut(0).fill(0.5);
        crate::split_at_events_with(
            &mut compressor,
            &mut block,
            &context,
            |_, _| {},
            |compressor, piece, piece_context| compressor.process_with(piece, piece_context),
        );
        // Ducked to the threshold while the key is loud, untouched after.
        for frame in 0..4 {
            assert!(
                (block.channel(0)[frame] - 0.05).abs() < 1e-6,
                "frame {frame}"
            );
        }
        for frame in 4..8 {
            assert_eq!(block.channel(0)[frame], 0.5, "frame {frame} was ducked");
        }
    }
}
