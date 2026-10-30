//! Tremolo and auto-pan: a level or a position moved by an LFO.

use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, Sample, Smoothed};

use crate::mix::MixLaw;
use crate::numeric::{glide_to, limit};
use crate::{Lfo, LfoShape, PanMode, ProcessContext, ProcessSpec, Processor, Transport};

/// Limits of the settings.
const MAX_RATE_HZ: f64 = 200.0;
const MIN_SYNC_BEATS: f64 = 1.0 / 64.0;
const MAX_SYNC_BEATS: f64 = 64.0;
const MAX_SMOOTHING_MS: f64 = 50.0;

/// How long a change of depth, width or stereo phase takes.
const GLIDE_SECONDS: f64 = 0.02;

/// The LFO behind a [`Tremolo`] or an [`AutoPan`], free-running or locked
/// to the transport.
#[derive(Debug, Clone, Copy)]
struct Sweep {
    lfo: Lfo,
    rate_hz: f64,
    /// Quarter notes per cycle, when locked to the tempo.
    sync_beats: Option<f64>,
    sample_rate: f64,
}

impl Sweep {
    fn new(shape: LfoShape, rate_hz: f64) -> Self {
        Self {
            lfo: Lfo::new(shape, rate_hz),
            rate_hz,
            sync_beats: None,
            sample_rate: 44_100.0,
        }
    }

    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        self.lfo.set_sample_rate(sample_rate);
        self.lfo.set_phase(0.0);
        self.lfo.set_frequency(self.rate_hz);
    }

    fn set_rate_hz(&mut self, hertz: f64) {
        self.rate_hz = limit(hertz, 0.0, MAX_RATE_HZ);
        if self.sync_beats.is_none() {
            self.lfo.set_frequency(self.rate_hz);
        }
    }

    fn set_sync_beats(&mut self, beats: Option<f64>) {
        self.sync_beats = beats.map(|beats| limit(beats, MIN_SYNC_BEATS, MAX_SYNC_BEATS));
        if self.sync_beats.is_none() {
            self.lfo.set_frequency(self.rate_hz);
        }
    }

    /// Locks the LFO to the transport for the block that begins `offset`
    /// frames into the one the transport describes: on the beat while the
    /// transport plays, at the tempo's rate while it is stopped, and free
    /// at the set rate when the tempo is not known or no sync is set.
    fn follow(&mut self, transport: &Transport, offset: usize) {
        let Some(beats_per_cycle) = self.sync_beats else {
            return;
        };
        let Some(per_sample) = transport.beats_per_sample(self.sample_rate) else {
            self.lfo.set_frequency(self.rate_hz);
            return;
        };
        if !per_sample.is_finite() || per_sample < 0.0 {
            return;
        }
        match transport.beats_at(offset, self.sample_rate) {
            Some(position) if transport.playing && position.is_finite() => {
                self.lfo
                    .sync_to_beats(position, beats_per_cycle, per_sample);
            }
            _ => self
                .lfo
                .set_frequency(per_sample * self.sample_rate / beats_per_cycle),
        }
    }

    fn reset(&mut self) {
        self.lfo.set_phase(0.0);
    }
}

/// The value of an LFO at `ahead` cycles past where it is, from -1 to 1. A
/// sample-and-hold has one level for the whole cycle it holds, so for it
/// every place in the cycle reads the same.
#[inline]
fn value_ahead(lfo: &Lfo, ahead: f64) -> f64 {
    match lfo.shape() {
        LfoShape::SampleAndHold => lfo.value(),
        shape => {
            let mut phase = lfo.phase() + ahead;
            if phase >= 1.0 {
                phase -= 1.0;
            }
            shape.value_at(phase)
        }
    }
}

/// One-pole coefficient for a smoothing time.
fn smoothing_coefficient(milliseconds: f64, sample_rate: f64) -> f64 {
    let samples = milliseconds * 1e-3 * sample_rate;
    if samples > 0.0 {
        (-1.0 / samples).exp()
    } else {
        0.0
    }
}

/// A tremolo: the level moved up and down by an LFO.
///
/// The gain is `1 - depth * (1 - v) / 2`, where `v` is the LFO from -1 to
/// 1: it swings between full level and `1 - depth`. A depth of 1 swings
/// from full level to silence; the ratio of the quietest to the loudest
/// point is `1 - depth` whatever the shape.
///
/// - The LFO is an [`Lfo`] of any [`LfoShape`], at a rate in hertz, or
///   locked to the tempo with [`set_sync_beats`](Self::set_sync_beats):
///   one cycle every so many quarter notes, a cycle starting on beat zero,
///   read from the [`Transport`] of the context given to
///   [`process_with`](Processor::process_with).
/// - Channel `c` reads the LFO `c * stereo_phase` cycles ahead of the
///   first, so with a stereo phase of 0.5 the two sides of a stereo signal
///   swing in opposite directions.
/// - The gain is smoothed by a one-pole filter with a time constant of a
///   few milliseconds (2 ms unless [set](Self::set_smoothing_ms)), so the
///   edges of a square or a sawtooth do not click. Zero leaves the gain as
///   the formula gives it.
///
/// The depth and the stereo phase glide over 20 ms, and a change of rate
/// keeps the place in the cycle. All setters are real-time safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{ProcessSpec, Processor, Tremolo};
///
/// let mut tremolo = Tremolo::<f32>::new().with_rate_hz(4.0).with_depth(0.5);
/// tremolo.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 12_000, num_channels: 1 });
///
/// // A quarter of a second of a steady level: one whole cycle, down to
/// // half and back.
/// let mut block = AudioBuffer::<f32>::new(1, 12_000);
/// block.channel_mut(0).fill(1.0);
/// tremolo.process(&mut block);
/// let low = block.channel(0).iter().fold(1.0f32, |low, s| low.min(*s));
/// assert!((low - 0.5).abs() < 1e-3);
/// ```
///
/// # Limits
///
/// The rate is limited to `0.0..=200.0` Hz, the depth and the stereo phase
/// to `0.0..=1.0`, a tempo-locked cycle to between 1/64 and 64 quarter
/// notes and the smoothing to 50 ms. Smoothing turns a sine's swing down
/// a little and late: by 0.13% and 2.9 degrees at 4 Hz with 2 ms. A
/// sample-and-hold takes no notice of the stereo phase.
#[derive(Debug, Clone)]
pub struct Tremolo<S: Sample = f32> {
    sweep: Sweep,
    depth: f64,
    stereo_phase: f64,
    smoothing_ms: f64,
    smoothing: f64,
    depth_now: Smoothed<f64>,
    phase_now: Smoothed<f64>,
    /// The smoothed gain of each channel.
    gains: Vec<f64>,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for Tremolo<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> Tremolo<S> {
    /// Creates a tremolo with a sine at 5 Hz, a depth of 0.5, both sides
    /// together and 2 ms of smoothing.
    pub fn new() -> Self {
        let mut tremolo = Self {
            sweep: Sweep::new(LfoShape::Sine, 5.0),
            depth: 0.5,
            stereo_phase: 0.0,
            smoothing_ms: 2.0,
            smoothing: 0.0,
            depth_now: Smoothed::new(0.5),
            phase_now: Smoothed::new(0.0),
            gains: Vec::new(),
            marker: PhantomData,
        };
        tremolo.smoothing = smoothing_coefficient(2.0, 44_100.0);
        tremolo
    }

    /// Sets the rate in hertz, limited to `0.0..=200.0`. Used while no
    /// tempo sync is set, or while the tempo is not known.
    pub fn set_rate_hz(&mut self, hertz: f64) {
        self.sweep.set_rate_hz(hertz);
    }

    /// Locks the rate to the tempo: one cycle every `beats` quarter notes,
    /// so `Some(0.5)` is a cycle every eighth note. `None` frees it.
    pub fn set_sync_beats(&mut self, beats: Option<f64>) {
        self.sweep.set_sync_beats(beats);
    }

    /// Sets the shape of the LFO.
    pub fn set_shape(&mut self, shape: LfoShape) {
        self.sweep.lfo.set_shape(shape);
    }

    /// Sets how far the level swings, from `0.0` (not at all) to `1.0`
    /// (down to silence).
    pub fn set_depth(&mut self, depth: f64) {
        self.depth = limit(depth, 0.0, 1.0);
        glide_to(&mut self.depth_now, self.depth);
    }

    /// Sets how far ahead in the cycle each channel is of the one before,
    /// from `0.0` to `1.0` of a cycle.
    pub fn set_stereo_phase(&mut self, cycles: f64) {
        self.stereo_phase = limit(cycles, 0.0, 1.0);
        glide_to(&mut self.phase_now, self.stereo_phase);
    }

    /// Sets the time constant of the smoothing of the gain, in
    /// milliseconds, limited to `0.0..=50.0`.
    pub fn set_smoothing_ms(&mut self, milliseconds: f64) {
        self.smoothing_ms = limit(milliseconds, 0.0, MAX_SMOOTHING_MS);
        self.smoothing = smoothing_coefficient(self.smoothing_ms, self.sweep.sample_rate);
    }

    /// Builder form of [`set_rate_hz`](Self::set_rate_hz).
    #[must_use]
    pub fn with_rate_hz(mut self, hertz: f64) -> Self {
        self.set_rate_hz(hertz);
        self
    }

    /// Builder form of [`set_sync_beats`](Self::set_sync_beats).
    #[must_use]
    pub fn with_sync_beats(mut self, beats: Option<f64>) -> Self {
        self.set_sync_beats(beats);
        self
    }

    /// Builder form of [`set_shape`](Self::set_shape).
    #[must_use]
    pub fn with_shape(mut self, shape: LfoShape) -> Self {
        self.set_shape(shape);
        self
    }

    /// Builder form of [`set_depth`](Self::set_depth).
    #[must_use]
    pub fn with_depth(mut self, depth: f64) -> Self {
        self.set_depth(depth);
        self
    }

    /// Builder form of [`set_stereo_phase`](Self::set_stereo_phase).
    #[must_use]
    pub fn with_stereo_phase(mut self, cycles: f64) -> Self {
        self.set_stereo_phase(cycles);
        self
    }

    /// Builder form of [`set_smoothing_ms`](Self::set_smoothing_ms).
    #[must_use]
    pub fn with_smoothing_ms(mut self, milliseconds: f64) -> Self {
        self.set_smoothing_ms(milliseconds);
        self
    }

    /// The gain the formula gives for an LFO value of `v`, from -1 to 1,
    /// at the depth as last set.
    pub fn gain_at(&self, v: f64) -> f64 {
        1.0 - self.depth * (1.0 - limit(v, -1.0, 1.0)) * 0.5
    }
}

impl<S: Sample> Processor<S> for Tremolo<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sweep.prepare(spec.sample_rate);
        self.smoothing = smoothing_coefficient(self.smoothing_ms, spec.sample_rate);
        self.depth_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.phase_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.gains = vec![1.0; spec.num_channels];
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let smoothing = self.smoothing;
        let (mut lfo_after, mut depth_after, mut phase_after) =
            (self.sweep.lfo, self.depth_now, self.phase_now);
        for (index, (samples, held)) in buffer.iter_channels_mut().zip(&mut self.gains).enumerate()
        {
            let (mut lfo, mut depth, mut phase) = (self.sweep.lfo, self.depth_now, self.phase_now);
            let place = index as f64;
            let mut gain = *held;
            for sample in samples {
                let ahead = (place * phase.next_value()).fract();
                let v = value_ahead(&lfo, ahead);
                let target = 1.0 - depth.next_value() * (1.0 - v) * 0.5;
                gain = target + smoothing * (gain - target);
                *sample = S::from_f64(sample.to_f64() * gain);
                lfo.advance(1);
            }
            *held = gain;
            (lfo_after, depth_after, phase_after) = (lfo, depth, phase);
        }
        if self.gains.is_empty() || buffer.num_channels() == 0 {
            // No channel to process, but time still passes.
            let frames = buffer.num_frames();
            lfo_after.advance(frames);
            depth_after.skip(frames);
            phase_after.skip(frames);
        }
        (self.sweep.lfo, self.depth_now, self.phase_now) = (lfo_after, depth_after, phase_after);
    }

    /// Like [`process`](Processor::process), with the LFO locked to the
    /// context's transport when a tempo sync is set.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        self.sweep.follow(context.transport, context.offset);
        self.process(buffer);
    }

    fn reset(&mut self) {
        self.sweep.reset();
        self.depth_now.set_immediate(self.depth);
        self.phase_now.set_immediate(self.stereo_phase);
        // Each channel starts at the gain the formula gives for it.
        let start = self.sweep.lfo;
        for (index, gain) in self.gains.iter_mut().enumerate() {
            let v = value_ahead(&start, (index as f64 * self.stereo_phase).fract());
            *gain = 1.0 - self.depth * (1.0 - v) * 0.5;
        }
    }
}

/// An auto-pan: the position between left and right moved by an LFO.
///
/// The position is `width * v`, where `v` is the LFO from -1 to 1, so a
/// width of 1 swings from hard left to hard right. A [`MixLaw`] turns the
/// position into a gain for each side, and a [`PanMode`] says whether the
/// first two channels hold a stereo signal to balance or a mono signal (in
/// the first) to place, exactly as in a [`Panner`](crate::Panner): in
/// balance the centre leaves the signal as it was.
///
/// The LFO is the same as a [`Tremolo`]'s: any [`LfoShape`], a rate in
/// hertz or locked to the tempo, and smoothing of the position (2 ms
/// unless set). The width glides over 20 ms. All setters are real-time
/// safe.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::{AutoPan, MixLaw, PanMode, ProcessSpec, Processor};
///
/// let mut pan = AutoPan::<f32>::new()
///     .with_mode(PanMode::MonoToStereo)
///     .with_law(MixLaw::EqualPower)
///     .with_rate_hz(1.0);
/// pan.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 2 });
///
/// // A mono source sweeps across, at constant power.
/// let mut block = AudioBuffer::<f32>::new(2, 4_800);
/// block.channel_mut(0).fill(1.0);
/// pan.process(&mut block);
/// let (left, right) = (block.channel(0)[4_799], block.channel(1)[4_799]);
/// assert!(right > left);
/// assert!((left * left + right * right - 1.0).abs() < 1e-5);
/// ```
///
/// # Limits
///
/// Only the first two channels are touched; a buffer with one channel is
/// left as it is. A change of law or mode takes effect at once, without the
/// cross-fade a [`Panner`](crate::Panner) makes. The limits of the rate, the sync and the smoothing are a
/// [`Tremolo`]'s.
#[derive(Debug, Clone)]
pub struct AutoPan<S: Sample = f32> {
    sweep: Sweep,
    width: f64,
    law: MixLaw,
    mode: PanMode,
    smoothing_ms: f64,
    smoothing: f64,
    width_now: Smoothed<f64>,
    /// The smoothed position.
    position: f64,
    marker: PhantomData<S>,
}

impl<S: Sample> Default for AutoPan<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Sample> AutoPan<S> {
    /// Creates an auto-pan with a sine at 0.5 Hz across the whole width,
    /// balancing a stereo signal by the equal-power law, with 2 ms of
    /// smoothing.
    pub fn new() -> Self {
        Self {
            sweep: Sweep::new(LfoShape::Sine, 0.5),
            width: 1.0,
            law: MixLaw::EqualPower,
            mode: PanMode::StereoBalance,
            smoothing_ms: 2.0,
            smoothing: smoothing_coefficient(2.0, 44_100.0),
            width_now: Smoothed::new(1.0),
            position: 0.0,
            marker: PhantomData,
        }
    }

    /// Sets the rate in hertz, limited to `0.0..=200.0`.
    pub fn set_rate_hz(&mut self, hertz: f64) {
        self.sweep.set_rate_hz(hertz);
    }

    /// Locks the rate to the tempo: one cycle every `beats` quarter notes.
    /// `None` frees it.
    pub fn set_sync_beats(&mut self, beats: Option<f64>) {
        self.sweep.set_sync_beats(beats);
    }

    /// Sets the shape of the LFO.
    pub fn set_shape(&mut self, shape: LfoShape) {
        self.sweep.lfo.set_shape(shape);
    }

    /// Sets how far the position swings either side of the centre, from
    /// `0.0` to `1.0`.
    pub fn set_width(&mut self, width: f64) {
        self.width = limit(width, 0.0, 1.0);
        glide_to(&mut self.width_now, self.width);
    }

    /// Sets how the position becomes two gains.
    pub fn set_law(&mut self, law: MixLaw) {
        self.law = law;
    }

    /// Sets whether a stereo signal is balanced or a mono one placed.
    pub fn set_mode(&mut self, mode: PanMode) {
        self.mode = mode;
    }

    /// Sets the time constant of the smoothing of the position, in
    /// milliseconds, limited to `0.0..=50.0`.
    pub fn set_smoothing_ms(&mut self, milliseconds: f64) {
        self.smoothing_ms = limit(milliseconds, 0.0, MAX_SMOOTHING_MS);
        self.smoothing = smoothing_coefficient(self.smoothing_ms, self.sweep.sample_rate);
    }

    /// Builder form of [`set_rate_hz`](Self::set_rate_hz).
    #[must_use]
    pub fn with_rate_hz(mut self, hertz: f64) -> Self {
        self.set_rate_hz(hertz);
        self
    }

    /// Builder form of [`set_sync_beats`](Self::set_sync_beats).
    #[must_use]
    pub fn with_sync_beats(mut self, beats: Option<f64>) -> Self {
        self.set_sync_beats(beats);
        self
    }

    /// Builder form of [`set_shape`](Self::set_shape).
    #[must_use]
    pub fn with_shape(mut self, shape: LfoShape) -> Self {
        self.set_shape(shape);
        self
    }

    /// Builder form of [`set_width`](Self::set_width).
    #[must_use]
    pub fn with_width(mut self, width: f64) -> Self {
        self.set_width(width);
        self
    }

    /// Builder form of [`set_law`](Self::set_law).
    #[must_use]
    pub fn with_law(mut self, law: MixLaw) -> Self {
        self.set_law(law);
        self
    }

    /// Builder form of [`set_mode`](Self::set_mode).
    #[must_use]
    pub fn with_mode(mut self, mode: PanMode) -> Self {
        self.set_mode(mode);
        self
    }

    /// Builder form of [`set_smoothing_ms`](Self::set_smoothing_ms).
    #[must_use]
    pub fn with_smoothing_ms(mut self, milliseconds: f64) -> Self {
        self.set_smoothing_ms(milliseconds);
        self
    }

    /// The left and right gains at a position from -1 to 1, by the law and
    /// the mode as last set.
    pub fn gains_at(&self, position: f64) -> (f64, f64) {
        let (left, right) = self.law.gains(0.5 * (limit(position, -1.0, 1.0) + 1.0));
        match self.mode {
            PanMode::MonoToStereo => (left, right),
            PanMode::StereoBalance => {
                let centre = self.law.gains(0.5);
                (left / centre.0, right / centre.1)
            }
        }
    }
}

impl<S: Sample> Processor<S> for AutoPan<S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sweep.prepare(spec.sample_rate);
        self.smoothing = smoothing_coefficient(self.smoothing_ms, spec.sample_rate);
        self.width_now.set_ramp(spec.sample_rate, GLIDE_SECONDS);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames();
        if buffer.num_channels() < 2 {
            // Nothing to pan, but time still passes.
            for _ in 0..frames {
                let target = self.width_now.next_value() * self.sweep.lfo.value();
                self.position = target + self.smoothing * (self.position - target);
                self.sweep.lfo.advance(1);
            }
            return;
        }
        let mono = self.mode == PanMode::MonoToStereo;
        let (left, right) = buffer.channel_pair_mut(0, 1);
        for (left, right) in left.iter_mut().zip(right) {
            let target = self.width_now.next_value() * self.sweep.lfo.value();
            self.position = target + self.smoothing * (self.position - target);
            self.sweep.lfo.advance(1);
            let (left_gain, right_gain) = self.gains_at(self.position);
            let source = if mono { *left } else { *right };
            *left = S::from_f64(left.to_f64() * left_gain);
            *right = S::from_f64(source.to_f64() * right_gain);
        }
    }

    /// Like [`process`](Processor::process), with the LFO locked to the
    /// context's transport when a tempo sync is set.
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        self.sweep.follow(context.transport, context.offset);
        self.process(buffer);
    }

    fn reset(&mut self) {
        self.sweep.reset();
        self.width_now.set_immediate(self.width);
        self.position = self.width * self.sweep.lfo.value();
    }
}

#[cfg(test)]
mod tests {
    use core::f64::consts::TAU;

    use super::*;
    use crate::event::EventBuffer;
    use crate::numeric::strict_buffer_peak;
    use crate::testing::{largest_jump, noise, run_in_blocks, sine};

    const RATE: f64 = 48_000.0;

    fn spec(channels: usize) -> ProcessSpec {
        ProcessSpec {
            sample_rate: RATE,
            max_block_size: 512,
            num_channels: channels,
        }
    }

    fn steady(channels: usize, frames: usize) -> AudioBuffer<f64> {
        let mut buffer = AudioBuffer::new(channels, frames);
        for channel in buffer.iter_channels_mut() {
            channel.fill(1.0);
        }
        buffer
    }

    #[test]
    fn the_gain_follows_the_formula_on_every_sample() {
        for (depth, rate) in [(0.25, 3.0), (0.5, 5.0), (1.0, 11.0)] {
            let mut tremolo = Tremolo::new()
                .with_depth(depth)
                .with_rate_hz(rate)
                .with_smoothing_ms(0.0);
            tremolo.prepare(&spec(1));
            let mut signal = steady(1, 20_000);
            run_in_blocks(&mut tremolo, &mut signal, 512);
            for (n, &gain) in signal.channel(0).iter().enumerate() {
                let v = (TAU * rate * n as f64 / RATE).sin();
                let expected = 1.0 - depth * (1.0 - v) / 2.0;
                assert!(
                    (gain - expected).abs() < 1e-9,
                    "frame {n}: {gain} against {expected}"
                );
            }
        }
    }

    /// The largest size in each period of a tone, to follow its level.
    fn envelope(signal: &[f64], period: usize) -> Vec<f64> {
        signal
            .chunks(period)
            .map(|chunk| chunk.iter().fold(0.0f64, |peak, s| peak.max(s.abs())))
            .collect()
    }

    #[test]
    fn the_depth_measured_on_a_tone_is_the_depth_set() {
        // A 1 kHz tone of amplitude 0.5 under a 4 Hz tremolo: its quietest
        // point over its loudest is 1 - depth. A sine has the default
        // smoothing, which takes 0.13% off its swing. The shapes with
        // corners have none, since smoothing rounds a corner off; their
        // level, taken over one period of the tone, moves by up to 0.4% of
        // full scale within it.
        for shape in [
            LfoShape::Sine,
            LfoShape::Triangle,
            LfoShape::Square,
            LfoShape::SawUp,
        ] {
            let (smoothing, tolerance) = if shape == LfoShape::Sine {
                (2.0, 3e-3)
            } else {
                (0.0, 1e-2)
            };
            for depth in [0.25, 0.5, 0.9, 1.0] {
                let mut tremolo = Tremolo::new()
                    .with_depth(depth)
                    .with_rate_hz(4.0)
                    .with_shape(shape)
                    .with_smoothing_ms(smoothing);
                tremolo.prepare(&spec(1));
                let mut tone = sine::<f64>(1, 48_000, 1_000.0, RATE, 0.5);
                run_in_blocks(&mut tremolo, &mut tone, 512);
                let levels = envelope(tone.channel(0), 48);
                let loud = levels.iter().fold(0.0f64, |a, &b| a.max(b));
                let quiet = levels.iter().fold(1.0f64, |a, &b| a.min(b));
                assert!(
                    (loud - 0.5).abs() < 2e-3,
                    "{shape:?}, depth {depth}: {loud}"
                );
                assert!(
                    (quiet / loud - (1.0 - depth)).abs() < tolerance,
                    "{shape:?}, depth {depth}: {quiet} / {loud}"
                );
            }
        }
    }

    #[test]
    fn each_channel_is_ahead_by_the_stereo_phase() {
        for offset in [0.0, 0.25, 0.5] {
            let mut tremolo = Tremolo::new()
                .with_depth(0.8)
                .with_rate_hz(2.0)
                .with_stereo_phase(offset)
                .with_smoothing_ms(0.0);
            tremolo.prepare(&spec(3));
            let mut signal = steady(3, 30_000);
            run_in_blocks(&mut tremolo, &mut signal, 333);
            for channel in 0..3 {
                let ahead = (channel as f64 * offset).fract();
                for n in (0..30_000).step_by(17) {
                    let v = (TAU * (2.0 * n as f64 / RATE + ahead)).sin();
                    let expected = 1.0 - 0.8 * (1.0 - v) / 2.0;
                    let gain = signal.channel(channel)[n];
                    assert!(
                        (gain - expected).abs() < 1e-9,
                        "offset {offset}, channel {channel}"
                    );
                }
            }
            if offset == 0.5 {
                // Opposite sides: the two gains always add up to 2 - depth.
                for n in 0..30_000 {
                    let sum = signal.channel(0)[n] + signal.channel(1)[n];
                    assert!((sum - 1.2).abs() < 1e-9);
                }
            }
        }
    }

    #[test]
    fn a_tempo_sync_puts_the_cycle_on_the_beat() {
        // 120 beats a minute, a cycle every beat: 2 Hz, starting a cycle on
        // every beat. The transport starts a quarter of a beat in.
        let render = |block: usize| {
            let mut tremolo = Tremolo::new()
                .with_depth(1.0)
                .with_rate_hz(0.1)
                .with_sync_beats(Some(1.0))
                .with_smoothing_ms(0.0);
            tremolo.prepare(&spec(1));
            let events = EventBuffer::new();
            let mut transport = Transport {
                position_beats: Some(0.25),
                ..Transport::playing_at(120.0)
            };
            let mut signal = steady(1, 24_000);
            let mut start = 0;
            while start < 24_000 {
                let end = (start + block).min(24_000);
                signal.windowed(start..end, |piece| {
                    let context = ProcessContext::new(&events, &transport);
                    tremolo.process_with(piece, &context);
                });
                transport.advance(end - start, RATE);
                start = end;
            }
            signal
        };
        for block in [512, 100, 7] {
            let signal = render(block);
            for (n, &gain) in signal.channel(0).iter().enumerate() {
                let beats = 0.25 + n as f64 * 2.0 / RATE;
                let v = (TAU * beats).sin();
                let expected = (1.0 + v) / 2.0;
                assert!(
                    (gain - expected).abs() < 1e-9,
                    "blocks of {block}, frame {n}"
                );
            }
        }
        // Stopped, it runs at the tempo's rate from where it is; without a
        // tempo, at its own rate.
        let mut tremolo = Tremolo::<f64>::new()
            .with_sync_beats(Some(1.0))
            .with_rate_hz(3.0);
        tremolo.prepare(&spec(1));
        let events = EventBuffer::new();
        let stopped = Transport {
            tempo_bpm: Some(60.0),
            ..Transport::STOPPED
        };
        let mut signal = steady(1, 64);
        tremolo.process_with(&mut signal, &ProcessContext::new(&events, &stopped));
        assert!((tremolo.sweep.lfo.phase() - 64.0 / RATE).abs() < 1e-12);
        tremolo.process_with(
            &mut signal,
            &ProcessContext::new(&events, &Transport::STOPPED),
        );
        let expected = 64.0 / RATE + 64.0 * 3.0 / RATE;
        assert!((tremolo.sweep.lfo.phase() - expected).abs() < 1e-12);
    }

    #[test]
    fn smoothing_takes_the_click_out_of_a_square() {
        let jump = |smoothing: f64| {
            let mut tremolo = Tremolo::new()
                .with_depth(1.0)
                .with_rate_hz(8.0)
                .with_shape(LfoShape::Square)
                .with_smoothing_ms(smoothing);
            tremolo.prepare(&spec(1));
            let mut signal = steady(1, 24_000);
            run_in_blocks(&mut tremolo, &mut signal, 512);
            largest_jump(signal.channel(0))
        };
        assert_eq!(jump(0.0), 1.0);
        // A one-pole of 2 ms moves a step of 1 by 1 - exp(-1/96) at most.
        let smoothed = jump(2.0);
        let most = 1.0 - (-1.0f64 / 96.0).exp();
        assert!(
            smoothed <= most + 1e-12 && smoothed > 0.99 * most,
            "{smoothed}"
        );
    }

    #[test]
    fn the_auto_pan_follows_its_law_and_keeps_the_power() {
        // Mono to stereo by the straight-line law: the right gain is
        // (1 + position) / 2, which gives the position back.
        let mut pan = AutoPan::new()
            .with_mode(PanMode::MonoToStereo)
            .with_law(MixLaw::Linear)
            .with_width(0.6)
            .with_rate_hz(3.0)
            .with_smoothing_ms(0.0);
        pan.prepare(&spec(2));
        let mut signal = steady(2, 20_000);
        signal.channel_mut(1).fill(0.0);
        run_in_blocks(&mut pan, &mut signal, 512);
        for n in 0..20_000 {
            let position = 2.0 * signal.channel(1)[n] - 1.0;
            let expected = 0.6 * (TAU * 3.0 * n as f64 / RATE).sin();
            assert!((position - expected).abs() < 1e-9, "frame {n}");
            assert!((signal.channel(0)[n] + signal.channel(1)[n] - 1.0).abs() < 1e-12);
        }

        // Balancing a stereo signal by the equal-power law keeps the power
        // of the two sides, and reaches each side in turn.
        let mut pan = AutoPan::new()
            .with_rate_hz(2.0)
            .with_shape(LfoShape::Triangle)
            .with_smoothing_ms(0.0);
        pan.prepare(&spec(2));
        let mut signal = steady(2, 24_000);
        run_in_blocks(&mut pan, &mut signal, 100);
        let (left, right) = (signal.channel(0), signal.channel(1));
        for n in 0..24_000 {
            assert!((left[n] * left[n] + right[n] * right[n] - 2.0).abs() < 1e-9);
        }
        // The triangle is at the right on frame 0 and at the left half a
        // cycle later.
        assert!(left[0] == 0.0 && (right[0] - 2f64.sqrt()).abs() < 1e-12);
        assert!(right[12_000].abs() < 1e-12 && (left[12_000] - 2f64.sqrt()).abs() < 1e-12);

        // One channel is left as it is.
        let mut mono = steady(1, 64);
        pan.process(&mut mono);
        assert_eq!(mono.channel(0), &[1.0; 64]);
    }

    #[test]
    fn output_does_not_depend_on_block_size() {
        let render = |block: usize| {
            let mut tremolo = Tremolo::new()
                .with_shape(LfoShape::SampleAndHold)
                .with_rate_hz(9.0)
                .with_stereo_phase(0.3);
            let mut pan = AutoPan::new()
                .with_shape(LfoShape::SawDown)
                .with_rate_hz(7.0);
            tremolo.prepare(&spec(2));
            pan.prepare(&spec(2));
            let mut signal = noise::<f64>(2, 6_000, 3);
            let mut start = 0;
            for (end, depth) in [(1_500, 0.9), (4_000, 0.2), (6_000, 0.6)] {
                signal.windowed(start..end, |part| {
                    run_in_blocks(&mut tremolo, part, block);
                    run_in_blocks(&mut pan, part, block);
                });
                tremolo.set_depth(depth);
                tremolo.set_stereo_phase(depth);
                tremolo.set_shape(LfoShape::Sine);
                pan.set_width(depth);
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
    fn a_change_of_depth_or_width_does_not_click() {
        let mut tremolo = Tremolo::new().with_rate_hz(1.0);
        let mut pan = AutoPan::new().with_rate_hz(1.0);
        tremolo.prepare(&spec(2));
        pan.prepare(&spec(2));
        let mut signal = sine::<f64>(2, 48_000, 220.0, RATE, 0.5);
        for (index, amount) in [1.0, 0.0, 0.7, 0.1].iter().enumerate() {
            signal.windowed(index * 12_000..(index + 1) * 12_000, |part| {
                run_in_blocks(&mut tremolo, part, 512);
                run_in_blocks(&mut pan, part, 512);
            });
            tremolo.set_depth(*amount);
            tremolo.set_stereo_phase(*amount);
            pan.set_width(*amount);
        }
        // The tone moves by at most 0.0144 per sample, raised by the
        // balance's 3 dB at the sides.
        for channel in signal.iter_channels() {
            let jump = largest_jump(channel);
            assert!(jump < 0.0144 * 1.5, "a jump of {jump}");
        }
    }

    #[test]
    fn they_stay_bounded_and_survive_garbage() {
        let mut tremolo = Tremolo::new();
        let mut pan = AutoPan::new();
        tremolo.prepare(&spec(2));
        pan.prepare(&spec(2));
        for garbage in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1e300] {
            tremolo.set_rate_hz(garbage);
            tremolo.set_depth(garbage);
            tremolo.set_stereo_phase(garbage);
            tremolo.set_smoothing_ms(garbage);
            tremolo.set_sync_beats(Some(garbage));
            pan.set_rate_hz(garbage);
            pan.set_width(garbage);
            pan.set_smoothing_ms(garbage);
            pan.set_sync_beats(Some(garbage));
            let events = EventBuffer::new();
            let transport = Transport {
                tempo_bpm: Some(garbage),
                position_beats: Some(garbage),
                ..Transport::playing_at(120.0)
            };
            let context = ProcessContext::new(&events, &transport);
            let mut signal = noise::<f64>(2, 4_800, 2);
            tremolo.process_with(&mut signal, &context);
            pan.process_with(&mut signal, &context);
            // Gains of at most 1 and the balance's sqrt(2).
            let peak = strict_buffer_peak(&signal);
            assert!(peak <= 2f64.sqrt() + 1e-9, "after {garbage}: {peak}");
        }
    }

    #[test]
    fn reset_starts_the_cycle_again() {
        let mut tremolo = Tremolo::new().with_rate_hz(3.0);
        tremolo.prepare(&spec(2));
        let mut expected = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut tremolo, &mut expected, 512);
        let mut other = noise::<f64>(2, 2_000, 5);
        run_in_blocks(&mut tremolo, &mut other, 512);
        tremolo.reset();
        let mut again = noise::<f64>(2, 2_000, 1);
        run_in_blocks(&mut tremolo, &mut again, 512);
        assert!(again == expected);
    }

    #[test]
    fn single_precision_and_unprepared_use_work() {
        let mut tremolo = Tremolo::<f32>::new();
        let mut buffer = AudioBuffer::<f32>::new(2, 64);
        buffer.channel_mut(0).fill(0.25);
        // Not prepared: no channels to follow, so nothing is touched.
        tremolo.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[0.25; 64]);
        tremolo.prepare(&ProcessSpec {
            sample_rate: 44_100.0,
            max_block_size: 64,
            num_channels: 1,
        });
        tremolo.process(&mut buffer);
        assert!(buffer.channel(0)[63] < 0.25);
        assert_eq!(buffer.channel(1), &[0.0; 64]);
        tremolo.process(&mut AudioBuffer::<f32>::new(0, 64));
        let mut pan = AutoPan::<f32>::new();
        pan.process(&mut buffer);
        assert_eq!(Processor::<f32>::latency_samples(&pan), 0);
    }
}
