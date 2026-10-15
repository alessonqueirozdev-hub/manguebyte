//! A part's effect chain: low-pass filter, reverb, then gain and pan.

use core::f64::consts::{FRAC_PI_4, SQRT_2};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::{ProcessSpec, Processor, Reverb, StateVariableFilter, SvfMode};
use manguebyte_params::{ParamId, ParamSmoother};

use crate::engine::Chain;
use crate::params::{self, ChainParams, FILTER_SMOOTHING, GAIN_FLOOR_DB, LEVEL_SMOOTHING};

/// The reverb counts as silent when its output stays below this, -120 dB,
/// while its input is silent.
const REVERB_QUIET: f32 = 1e-6;
/// How many frames the reverb has to stay quiet before it is cleared and
/// put to sleep.
pub(crate) const REVERB_SLEEP_FRAMES: u32 = 4_096;

fn resonance_to_q(resonance: f64) -> f64 {
    core::f64::consts::FRAC_1_SQRT_2 * 16f64.powf(resonance.clamp(0.0, 1.0))
}

fn gain_from_db(db: f64) -> f32 {
    if db <= f64::from(GAIN_FLOOR_DB) {
        0.0
    } else {
        10f64.powf(db / 20.0) as f32
    }
}

/// Left and right gains for `pan`: constant power, normalised so that the
/// centre leaves both sides untouched.
fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (f64::from(pan.clamp(-1.0, 1.0)) + 1.0) * FRAC_PI_4;
    ((SQRT_2 * angle.cos()) as f32, (SQRT_2 * angle.sin()) as f32)
}

/// The chain's signal processing, with its own scratch space.
pub(crate) struct ChainDsp {
    lowpass: StateVariableFilter<f64>,
    cutoff: ParamSmoother,
    resonance: ParamSmoother,
    filter_setting: (f32, f32),
    reverb: Reverb<f32>,
    reverb_mix: ParamSmoother,
    reverb_asleep: bool,
    quiet_frames: u32,
    gain: ParamSmoother,
    pan: ParamSmoother,
    pan_setting: (f32, (f32, f32)),
    /// The signal before the reverb, and the mix of each frame.
    dry: AudioBuffer<f32>,
    mixes: Vec<f32>,
}

impl ChainDsp {
    pub(crate) fn new(params: &ChainParams, spec: &ProcessSpec) -> Self {
        let rate = spec.sample_rate;
        let smoother = |smoothing, value: f32| {
            let mut smoother = ParamSmoother::new(smoothing);
            smoother.prepare(rate, value);
            smoother
        };
        let stereo = ProcessSpec {
            num_channels: 2,
            ..*spec
        };
        let cutoff = params.cutoff.value();
        let resonance = params.resonance.value();
        let mut lowpass = StateVariableFilter::new(
            SvfMode::Lowpass,
            f64::from(cutoff),
            resonance_to_q(f64::from(resonance)),
        );
        Processor::<f64>::prepare(&mut lowpass, &stereo);
        let mut reverb = Reverb::new()
            .with_mix(1.0)
            .with_damping(0.45)
            .with_size(0.6)
            .with_decay_seconds(f64::from(params.reverb_decay.value()));
        reverb.prepare(&stereo);
        let pan = params.pan.value();
        Self {
            lowpass,
            cutoff: smoother(FILTER_SMOOTHING, cutoff),
            resonance: smoother(FILTER_SMOOTHING, resonance),
            filter_setting: (cutoff, resonance),
            reverb,
            reverb_mix: smoother(LEVEL_SMOOTHING, params.reverb_mix.value()),
            reverb_asleep: true,
            quiet_frames: 0,
            gain: smoother(
                LEVEL_SMOOTHING,
                gain_from_db(f64::from(params.gain.value())),
            ),
            pan: smoother(LEVEL_SMOOTHING, pan),
            pan_setting: (pan, pan_gains(pan)),
            dry: AudioBuffer::new(2, spec.max_block_size),
            mixes: vec![0.0; spec.max_block_size],
        }
    }

    /// Takes a new value; `false` if `id` is not a chain parameter.
    pub(crate) fn set_param(&mut self, id: ParamId, value: f64) -> bool {
        match id {
            params::LOWPASS_CUTOFF => self.cutoff.set_target(value as f32),
            params::LOWPASS_RESONANCE => self.resonance.set_target(value as f32),
            params::REVERB_MIX => self.reverb_mix.set_target(value as f32),
            params::REVERB_DECAY => self.reverb.set_decay_seconds(value),
            params::GAIN => self.gain.set_target(gain_from_db(value)),
            params::PAN => self.pan.set_target(value as f32),
            _ => return false,
        }
        true
    }

    /// Whether the reverb is cleared and skipped until sound reaches it.
    pub(crate) fn reverb_asleep(&self) -> bool {
        self.reverb_asleep
    }

    /// Processes `buffer`, two channels, in place.
    pub(crate) fn process(&mut self, stages: Chain, buffer: &mut AudioBuffer<f32>) {
        if stages.lowpass {
            self.lowpass(buffer);
        }
        if stages.reverb {
            self.reverb(buffer);
        }
        self.gain_and_pan(buffer);
    }

    fn lowpass(&mut self, buffer: &mut AudioBuffer<f32>) {
        let (left, right) = buffer.channel_pair_mut(0, 1);
        for (left, right) in left.iter_mut().zip(right) {
            let setting = (self.cutoff.next_value(), self.resonance.next_value());
            if setting != self.filter_setting {
                if setting.0 != self.filter_setting.0 {
                    self.lowpass.set_frequency(f64::from(setting.0));
                }
                if setting.1 != self.filter_setting.1 {
                    self.lowpass.set_q(resonance_to_q(f64::from(setting.1)));
                }
                self.filter_setting = setting;
            }
            *left = self.lowpass.process_sample(0, f64::from(*left)) as f32;
            *right = self.lowpass.process_sample(1, f64::from(*right)) as f32;
        }
    }

    /// The reverb, mixed in by `reverb.mix`.
    ///
    /// While the mix is 0 the reverb hears nothing. Once its input has been
    /// silent and its output below [`REVERB_QUIET`] for
    /// [`REVERB_SLEEP_FRAMES`] frames in a row, it is cleared and skipped
    /// until sound reaches it again. A cleared reverb fed silence gives
    /// exact zeros, so skipping it changes nothing; and the frame it falls
    /// asleep on is worked out per frame, so the output does not depend on
    /// how the timeline is cut into blocks.
    fn reverb(&mut self, buffer: &mut AudioBuffer<f32>) {
        let frames = buffer.num_frames();
        self.dry.set_num_frames(frames);
        self.dry.copy_from(buffer);
        for mix in &mut self.mixes[..frames] {
            *mix = self.reverb_mix.next_value();
        }
        // The reverb's input: the dry signal while the mix is above 0.
        {
            let (left, right) = buffer.channel_pair_mut(0, 1);
            for ((left, right), &mix) in left.iter_mut().zip(right).zip(&self.mixes[..frames]) {
                if mix <= 0.0 {
                    *left = 0.0;
                    *right = 0.0;
                }
            }
        }

        let mut start = 0;
        while start < frames {
            if self.reverb_asleep {
                // Asleep, silence stays silence: find where sound starts.
                let woken = (start..frames)
                    .find(|&frame| buffer.channel(0)[frame] != 0.0 || buffer.channel(1)[frame] != 0.0);
                let Some(woken) = woken else {
                    break;
                };
                self.reverb_asleep = false;
                self.quiet_frames = 0;
                start = woken;
            }
            // Run up to the first frame on which it could fall asleep, so
            // that it falls asleep on the same frame however the frames
            // arrive.
            let room = (REVERB_SLEEP_FRAMES - self.quiet_frames) as usize;
            let end = frames.min(start + room);
            // The reverb overwrites its input, so find the last frame with
            // sound in it first: the quiet run cannot start before it.
            let last_loud = (start..end)
                .rev()
                .find(|&frame| buffer.channel(0)[frame] != 0.0 || buffer.channel(1)[frame] != 0.0);
            buffer.windowed(start..end, |piece| self.reverb.process(piece));
            let quiet_from = last_loud.map_or(start, |frame| frame + 1);
            let run = (quiet_from..end)
                .rev()
                .take_while(|&frame| {
                    buffer.channel(0)[frame].abs() < REVERB_QUIET
                        && buffer.channel(1)[frame].abs() < REVERB_QUIET
                })
                .count();
            self.quiet_frames = if last_loud.is_none() && run == end - start {
                self.quiet_frames + run as u32
            } else {
                run as u32
            };
            if self.quiet_frames >= REVERB_SLEEP_FRAMES {
                self.reverb.reset();
                self.reverb_asleep = true;
                self.quiet_frames = 0;
            }
            start = end;
        }

        // `buffer` now holds the wet signal.
        let (left, right) = buffer.channel_pair_mut(0, 1);
        let dry = self.dry.iter_channels().zip([left, right]);
        for (dry, wet) in dry {
            for ((wet, &dry), &mix) in wet.iter_mut().zip(dry).zip(&self.mixes[..frames]) {
                *wet = dry * (1.0 - mix) + *wet * mix;
            }
        }
    }

    /// Clears the low-pass filter's memory, for when it joins the chain.
    pub(crate) fn clear_lowpass(&mut self) {
        Processor::<f64>::reset(&mut self.lowpass);
    }

    /// Clears the reverb's tail, for when it joins the chain.
    pub(crate) fn clear_reverb(&mut self) {
        self.reverb.reset();
        self.reverb_asleep = true;
        self.quiet_frames = 0;
    }

    fn gain_and_pan(&mut self, buffer: &mut AudioBuffer<f32>) {
        let (left, right) = buffer.channel_pair_mut(0, 1);
        for (left, right) in left.iter_mut().zip(right) {
            let gain = self.gain.next_value();
            let pan = self.pan.next_value();
            if pan != self.pan_setting.0 {
                self.pan_setting = (pan, pan_gains(pan));
            }
            let (left_gain, right_gain) = self.pan_setting.1;
            *left *= gain * left_gain;
            *right *= gain * right_gain;
        }
    }
}
