//! The parameters of a part, by stable text identifier.
//!
//! An event names a parameter by its [`ParamId`], the 32-bit FNV-1a hash
//! of the identifier ([`ParamId::from_name`]); the JSON from
//! [`Engine::params_json`](crate::Engine::params_json) gives both.

use manguebyte_params::{
    ChoiceParam, FloatParam, FloatRange, Param, ParamId, ParamSet, Smoothing,
};

use crate::engine::{Chain, InstrumentKind};

/// The text identifiers of every parameter a part can have.
pub mod ids {
    /// Chain low-pass cutoff, hertz.
    pub const LOWPASS_CUTOFF: &str = "lowpass.cutoff";
    /// Chain low-pass resonance, 0 to 1.
    pub const LOWPASS_RESONANCE: &str = "lowpass.resonance";
    /// Chain reverb balance, 0 (dry) to 1 (wet).
    pub const REVERB_MIX: &str = "reverb.mix";
    /// Chain reverb decay time, seconds.
    pub const REVERB_DECAY: &str = "reverb.decay";
    /// Part level, decibels; the lowest value is silence.
    pub const GAIN: &str = "gain";
    /// Part balance, -1 (left) to 1 (right).
    pub const PAN: &str = "pan";

    /// Synth waveform: 0 saw, 1 square, 2 triangle, 3 sine.
    pub const SYNTH_WAVE: &str = "synth.wave";
    /// Synth detune between its two oscillators, cents.
    pub const SYNTH_DETUNE: &str = "synth.detune";
    /// Synth sub-oscillator level, 0 to 1.
    pub const SYNTH_SUB: &str = "synth.sub";
    /// Synth filter cutoff, hertz.
    pub const SYNTH_CUTOFF: &str = "synth.cutoff";
    /// Synth filter resonance, 0 to 1.
    pub const SYNTH_RESONANCE: &str = "synth.resonance";
    /// How far the filter envelope opens the synth filter, octaves.
    pub const SYNTH_ENV_AMOUNT: &str = "synth.env_amount";
    /// Synth filter envelope decay, seconds.
    pub const SYNTH_FILTER_DECAY: &str = "synth.filter_decay";
    /// Synth amplitude attack, seconds.
    pub const SYNTH_ATTACK: &str = "synth.attack";
    /// Synth amplitude decay, seconds.
    pub const SYNTH_DECAY: &str = "synth.decay";
    /// Synth sustain level, 0 to 1.
    pub const SYNTH_SUSTAIN: &str = "synth.sustain";
    /// Synth release, seconds.
    pub const SYNTH_RELEASE: &str = "synth.release";

    /// Drum kit tuning, semitones.
    pub const KIT_TUNE: &str = "kit.tune";
    /// Drum kit decay, as a factor on every drum's length.
    pub const KIT_DECAY: &str = "kit.decay";
    /// Kick level, 0 to 1.
    pub const KICK_LEVEL: &str = "kick.level";
    /// Snare level, 0 to 1.
    pub const SNARE_LEVEL: &str = "snare.level";
    /// Closed hi-hat level, 0 to 1.
    pub const CLOSED_HAT_LEVEL: &str = "closed_hat.level";
    /// Open hi-hat level, 0 to 1.
    pub const OPEN_HAT_LEVEL: &str = "open_hat.level";
    /// Ride level, 0 to 1.
    pub const RIDE_LEVEL: &str = "ride.level";
    /// Crash level, 0 to 1.
    pub const CRASH_LEVEL: &str = "crash.level";
}

pub(crate) const LOWPASS_CUTOFF: ParamId = ParamId::from_name(ids::LOWPASS_CUTOFF);
pub(crate) const LOWPASS_RESONANCE: ParamId = ParamId::from_name(ids::LOWPASS_RESONANCE);
pub(crate) const REVERB_MIX: ParamId = ParamId::from_name(ids::REVERB_MIX);
pub(crate) const REVERB_DECAY: ParamId = ParamId::from_name(ids::REVERB_DECAY);
pub(crate) const GAIN: ParamId = ParamId::from_name(ids::GAIN);
pub(crate) const PAN: ParamId = ParamId::from_name(ids::PAN);

pub(crate) const SYNTH_WAVE: ParamId = ParamId::from_name(ids::SYNTH_WAVE);
pub(crate) const SYNTH_DETUNE: ParamId = ParamId::from_name(ids::SYNTH_DETUNE);
pub(crate) const SYNTH_SUB: ParamId = ParamId::from_name(ids::SYNTH_SUB);
pub(crate) const SYNTH_CUTOFF: ParamId = ParamId::from_name(ids::SYNTH_CUTOFF);
pub(crate) const SYNTH_RESONANCE: ParamId = ParamId::from_name(ids::SYNTH_RESONANCE);
pub(crate) const SYNTH_ENV_AMOUNT: ParamId = ParamId::from_name(ids::SYNTH_ENV_AMOUNT);
pub(crate) const SYNTH_FILTER_DECAY: ParamId = ParamId::from_name(ids::SYNTH_FILTER_DECAY);
pub(crate) const SYNTH_ATTACK: ParamId = ParamId::from_name(ids::SYNTH_ATTACK);
pub(crate) const SYNTH_DECAY: ParamId = ParamId::from_name(ids::SYNTH_DECAY);
pub(crate) const SYNTH_SUSTAIN: ParamId = ParamId::from_name(ids::SYNTH_SUSTAIN);
pub(crate) const SYNTH_RELEASE: ParamId = ParamId::from_name(ids::SYNTH_RELEASE);

pub(crate) const KIT_TUNE: ParamId = ParamId::from_name(ids::KIT_TUNE);
pub(crate) const KIT_DECAY: ParamId = ParamId::from_name(ids::KIT_DECAY);
/// The level parameters of the drums, in [`Drum`](crate::Drum) order.
pub(crate) const DRUM_LEVELS: [ParamId; 6] = [
    ParamId::from_name(ids::KICK_LEVEL),
    ParamId::from_name(ids::SNARE_LEVEL),
    ParamId::from_name(ids::CLOSED_HAT_LEVEL),
    ParamId::from_name(ids::OPEN_HAT_LEVEL),
    ParamId::from_name(ids::RIDE_LEVEL),
    ParamId::from_name(ids::CRASH_LEVEL),
];

/// Names of the synth waveforms, in the order of their values.
pub(crate) const WAVE_NAMES: [&str; 4] = ["Saw", "Square", "Triangle", "Sine"];

fn seconds(id: &str, name: &str, min: f32, max: f32, default: f32) -> FloatParam {
    FloatParam::new(id, name, FloatRange::skewed(min, max, 0.35), default)
        .with_unit("s")
        .with_precision(3)
}

fn unit(id: &str, name: &str, default: f32) -> FloatParam {
    FloatParam::new(id, name, FloatRange::linear(0.0, 1.0), default)
}

pub(crate) struct SynthParams {
    pub(crate) wave: ChoiceParam,
    pub(crate) detune: FloatParam,
    pub(crate) sub: FloatParam,
    pub(crate) cutoff: FloatParam,
    pub(crate) resonance: FloatParam,
    pub(crate) env_amount: FloatParam,
    pub(crate) filter_decay: FloatParam,
    pub(crate) attack: FloatParam,
    pub(crate) decay: FloatParam,
    pub(crate) sustain: FloatParam,
    pub(crate) release: FloatParam,
}

impl SynthParams {
    pub(crate) fn new() -> Self {
        Self {
            wave: ChoiceParam::new(ids::SYNTH_WAVE, "Wave", &WAVE_NAMES, 0),
            detune: FloatParam::new(
                ids::SYNTH_DETUNE,
                "Detune",
                FloatRange::linear(0.0, 50.0),
                9.0,
            )
            .with_unit("ct")
            .with_precision(1),
            sub: unit(ids::SYNTH_SUB, "Sub", 0.35),
            cutoff: FloatParam::new(
                ids::SYNTH_CUTOFF,
                "Cutoff",
                FloatRange::logarithmic(40.0, 16_000.0),
                900.0,
            )
            .with_unit("Hz")
            .with_precision(0)
            .with_smoothing(Smoothing::Exponential(10.0)),
            resonance: unit(ids::SYNTH_RESONANCE, "Resonance", 0.25),
            env_amount: FloatParam::new(
                ids::SYNTH_ENV_AMOUNT,
                "Env amount",
                FloatRange::linear(0.0, 6.0),
                2.5,
            )
            .with_unit("oct")
            .with_precision(2),
            filter_decay: seconds(ids::SYNTH_FILTER_DECAY, "Filter decay", 0.01, 4.0, 0.35),
            attack: seconds(ids::SYNTH_ATTACK, "Attack", 0.001, 4.0, 0.003),
            decay: seconds(ids::SYNTH_DECAY, "Decay", 0.01, 8.0, 0.6),
            sustain: unit(ids::SYNTH_SUSTAIN, "Sustain", 0.5),
            release: seconds(ids::SYNTH_RELEASE, "Release", 0.005, 8.0, 0.25),
        }
    }

    pub(crate) fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param)) {
        visitor(&self.wave);
        visitor(&self.detune);
        visitor(&self.sub);
        visitor(&self.cutoff);
        visitor(&self.resonance);
        visitor(&self.env_amount);
        visitor(&self.filter_decay);
        visitor(&self.attack);
        visitor(&self.decay);
        visitor(&self.sustain);
        visitor(&self.release);
    }
}

pub(crate) struct KitParams {
    pub(crate) tune: FloatParam,
    pub(crate) decay: FloatParam,
    pub(crate) levels: [FloatParam; 6],
}

impl KitParams {
    pub(crate) fn new() -> Self {
        Self {
            tune: FloatParam::new(ids::KIT_TUNE, "Tune", FloatRange::linear(-12.0, 12.0), 0.0)
                .with_unit("st")
                .with_precision(1),
            decay: FloatParam::new(
                ids::KIT_DECAY,
                "Decay",
                FloatRange::logarithmic(0.25, 4.0),
                1.0,
            )
            .with_unit("x")
            .with_precision(2),
            levels: [
                unit(ids::KICK_LEVEL, "Kick", 0.8),
                unit(ids::SNARE_LEVEL, "Snare", 0.8),
                unit(ids::CLOSED_HAT_LEVEL, "Closed hat", 0.8),
                unit(ids::OPEN_HAT_LEVEL, "Open hat", 0.8),
                unit(ids::RIDE_LEVEL, "Ride", 0.8),
                unit(ids::CRASH_LEVEL, "Crash", 0.8),
            ],
        }
    }

    pub(crate) fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param)) {
        visitor(&self.tune);
        visitor(&self.decay);
        for level in &self.levels {
            visitor(level);
        }
    }
}

pub(crate) struct ChainParams {
    pub(crate) cutoff: FloatParam,
    pub(crate) resonance: FloatParam,
    pub(crate) reverb_mix: FloatParam,
    pub(crate) reverb_decay: FloatParam,
    pub(crate) gain: FloatParam,
    pub(crate) pan: FloatParam,
}

/// The smoothing every chain parameter that moves audio uses: the hint in
/// its description and what the chain does.
pub(crate) const FILTER_SMOOTHING: Smoothing = Smoothing::Exponential(20.0);
pub(crate) const LEVEL_SMOOTHING: Smoothing = Smoothing::Linear(20.0);

/// The lowest gain, which the chain treats as silence.
pub(crate) const GAIN_FLOOR_DB: f32 = -60.0;

impl ChainParams {
    pub(crate) fn new() -> Self {
        Self {
            cutoff: FloatParam::new(
                ids::LOWPASS_CUTOFF,
                "Low-pass cutoff",
                FloatRange::logarithmic(20.0, 20_000.0),
                20_000.0,
            )
            .with_unit("Hz")
            .with_precision(0)
            .with_smoothing(FILTER_SMOOTHING),
            resonance: unit(ids::LOWPASS_RESONANCE, "Low-pass resonance", 0.0)
                .with_smoothing(FILTER_SMOOTHING),
            reverb_mix: unit(ids::REVERB_MIX, "Reverb mix", 0.15).with_smoothing(LEVEL_SMOOTHING),
            reverb_decay: seconds(ids::REVERB_DECAY, "Reverb decay", 0.2, 10.0, 1.6),
            gain: FloatParam::new(
                ids::GAIN,
                "Gain",
                FloatRange::linear(GAIN_FLOOR_DB, 12.0),
                0.0,
            )
            .with_unit("dB")
            .with_precision(1)
            .with_smoothing(LEVEL_SMOOTHING),
            pan: FloatParam::new(ids::PAN, "Pan", FloatRange::linear(-1.0, 1.0), 0.0)
                .with_precision(2)
                .with_smoothing(LEVEL_SMOOTHING),
        }
    }
}

pub(crate) enum InstrumentParams {
    Synth(SynthParams),
    Drums(KitParams),
}

/// Every parameter of one part: its instrument's, then those of the chain
/// stages it has, then gain and pan.
pub(crate) struct PartParams {
    pub(crate) instrument: InstrumentParams,
    pub(crate) chain: ChainParams,
    pub(crate) stages: Chain,
}

impl PartParams {
    pub(crate) fn new(kind: InstrumentKind, stages: Chain) -> Self {
        Self {
            instrument: match kind {
                InstrumentKind::Synth => InstrumentParams::Synth(SynthParams::new()),
                InstrumentKind::Drums => InstrumentParams::Drums(KitParams::new()),
            },
            chain: ChainParams::new(),
            stages,
        }
    }
}

impl ParamSet for PartParams {
    fn visit<'a>(&'a self, visitor: &mut dyn FnMut(&'a dyn Param)) {
        match &self.instrument {
            InstrumentParams::Synth(synth) => synth.visit(visitor),
            InstrumentParams::Drums(kit) => kit.visit(visitor),
        }
        if self.stages.lowpass {
            visitor(&self.chain.cutoff);
            visitor(&self.chain.resonance);
        }
        if self.stages.reverb {
            visitor(&self.chain.reverb_mix);
            visitor(&self.chain.reverb_decay);
        }
        visitor(&self.chain.gain);
        visitor(&self.chain.pan);
    }
}
