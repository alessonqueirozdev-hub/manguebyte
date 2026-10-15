//! The polyphonic synth: two detuned oscillators and a sub, a resonant
//! low-pass with its own envelope, and an amplitude envelope.

use core::f64::consts::LN_2;

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::NoteRef;
use manguebyte_dsp::{
    Oscillator, ProcessSpec, Processor, StateVariableFilter, SvfMode, Synth, Voice, Waveform,
};
use manguebyte_params::ParamId;

use crate::params::{self, SynthParams};

/// How many notes a synth part plays at once.
pub const SYNTH_VOICES: usize = 8;

/// Level below which an envelope counts as silent: -80 dB.
pub(crate) const SILENT: f64 = 1e-4;

/// The factor per sample that takes a level down by 60 dB in `seconds`.
pub(crate) fn decay_coefficient(seconds: f64, sample_rate: f64) -> f64 {
    let samples = (seconds * sample_rate).max(1.0);
    (-3.0 * core::f64::consts::LN_10 / samples).exp()
}

/// The synth's settings in plain units, as its parameters hold them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SynthSettings {
    wave: Waveform,
    detune_cents: f64,
    sub: f64,
    cutoff: f64,
    resonance: f64,
    env_amount: f64,
    filter_decay: f64,
    attack: f64,
    decay: f64,
    sustain: f64,
    release: f64,
}

fn waveform(index: f64) -> Waveform {
    match index as u32 {
        1 => Waveform::Square,
        2 => Waveform::Triangle,
        3 => Waveform::Sine,
        _ => Waveform::Saw,
    }
}

impl SynthSettings {
    pub(crate) fn from_params(params: &SynthParams) -> Self {
        let mut settings = Self {
            wave: Waveform::Saw,
            detune_cents: 0.0,
            sub: 0.0,
            cutoff: 1_000.0,
            resonance: 0.0,
            env_amount: 0.0,
            filter_decay: 0.1,
            attack: 0.01,
            decay: 0.1,
            sustain: 1.0,
            release: 0.1,
        };
        params.visit(&mut |param| {
            settings.set(param.param_id(), param.plain());
        });
        settings
    }

    /// Takes a new value; `false` if `id` is not a synth parameter.
    pub(crate) fn set(&mut self, id: ParamId, value: f64) -> bool {
        match id {
            params::SYNTH_WAVE => self.wave = waveform(value),
            params::SYNTH_DETUNE => self.detune_cents = value,
            params::SYNTH_SUB => self.sub = value,
            params::SYNTH_CUTOFF => self.cutoff = value,
            params::SYNTH_RESONANCE => self.resonance = value,
            params::SYNTH_ENV_AMOUNT => self.env_amount = value,
            params::SYNTH_FILTER_DECAY => self.filter_decay = value,
            params::SYNTH_ATTACK => self.attack = value,
            params::SYNTH_DECAY => self.decay = value,
            params::SYNTH_SUSTAIN => self.sustain = value,
            params::SYNTH_RELEASE => self.release = value,
            _ => return false,
        }
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Idle,
    Attack,
    Decay,
    Release,
}

/// Linear attack, then exponential decay to the sustain level and
/// exponential release: the shape of an analogue envelope generator.
#[derive(Debug, Clone, Copy)]
struct Envelope {
    stage: Stage,
    level: f64,
    attack_step: f64,
    decay_coefficient: f64,
    sustain: f64,
    release_coefficient: f64,
}

impl Envelope {
    fn new() -> Self {
        Self {
            stage: Stage::Idle,
            level: 0.0,
            attack_step: 1.0,
            decay_coefficient: 0.0,
            sustain: 1.0,
            release_coefficient: 0.0,
        }
    }

    fn configure(&mut self, settings: &SynthSettings, sample_rate: f64) {
        self.attack_step = 1.0 / (settings.attack * sample_rate).max(1.0);
        self.decay_coefficient = decay_coefficient(settings.decay, sample_rate);
        self.sustain = settings.sustain.clamp(0.0, 1.0);
        self.release_coefficient = decay_coefficient(settings.release, sample_rate);
    }

    fn next(&mut self) -> f64 {
        match self.stage {
            Stage::Idle => {}
            Stage::Attack => {
                self.level += self.attack_step;
                if self.level >= 1.0 {
                    self.level = 1.0;
                    self.stage = Stage::Decay;
                }
            }
            Stage::Decay => {
                self.level = self.sustain + (self.level - self.sustain) * self.decay_coefficient;
                if self.sustain < SILENT && self.level < SILENT {
                    self.silence();
                }
            }
            Stage::Release => {
                self.level *= self.release_coefficient;
                if self.level < SILENT {
                    self.silence();
                }
            }
        }
        self.level
    }

    fn silence(&mut self) {
        self.stage = Stage::Idle;
        self.level = 0.0;
    }
}

/// One note of the synth.
#[derive(Debug, Clone)]
pub(crate) struct SynthVoice {
    settings: SynthSettings,
    sample_rate: f64,
    oscillators: [Oscillator; 2],
    sub: Oscillator,
    filter: StateVariableFilter<f64>,
    /// The cutoff the filter is gliding towards the setting from.
    cutoff: f64,
    cutoff_coefficient: f64,
    amplitude: Envelope,
    filter_envelope: f64,
    filter_coefficient: f64,
    key: u8,
    velocity: f64,
}

/// Scales the sum of the oscillators so that one note at full velocity
/// peaks around -9 dB.
const VOICE_LEVEL: f64 = 0.28;

fn resonance_to_q(resonance: f64) -> f64 {
    0.6 * 18f64.powf(resonance.clamp(0.0, 1.0))
}

impl SynthVoice {
    pub(crate) fn new(settings: SynthSettings) -> Self {
        let mut voice = Self {
            settings,
            sample_rate: 48_000.0,
            oscillators: [
                Oscillator::new(settings.wave, 440.0),
                Oscillator::new(settings.wave, 440.0),
            ],
            sub: Oscillator::new(Waveform::Square, 220.0),
            filter: StateVariableFilter::new(
                SvfMode::Lowpass,
                settings.cutoff,
                resonance_to_q(settings.resonance),
            ),
            cutoff: settings.cutoff,
            cutoff_coefficient: 1.0,
            amplitude: Envelope::new(),
            filter_envelope: 0.0,
            filter_coefficient: 0.0,
            key: 60,
            velocity: 0.0,
        };
        voice.configure();
        voice
    }

    fn configure(&mut self) {
        self.amplitude.configure(&self.settings, self.sample_rate);
        self.filter_coefficient = decay_coefficient(self.settings.filter_decay, self.sample_rate);
        // A one-pole glide that settles within 0.1% in 10 ms, the
        // smoothing the cutoff parameter declares.
        self.cutoff_coefficient = 1.0 - decay_coefficient(0.010, self.sample_rate);
        self.filter.set_q(resonance_to_q(self.settings.resonance));
        for oscillator in &mut self.oscillators {
            oscillator.set_waveform(self.settings.wave);
        }
        self.tune();
    }

    fn tune(&mut self) {
        let frequency = 440.0 * ((f64::from(self.key) - 69.0) / 12.0 * LN_2).exp();
        let spread = (self.settings.detune_cents / 2_400.0 * LN_2).exp();
        self.oscillators[0].set_frequency(frequency / spread);
        self.oscillators[1].set_frequency(frequency * spread);
        self.sub.set_frequency(frequency * 0.5);
    }
}

impl Voice for SynthVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        for oscillator in &mut self.oscillators {
            oscillator.set_sample_rate(spec.sample_rate);
        }
        self.sub.set_sample_rate(spec.sample_rate);
        Processor::<f64>::prepare(
            &mut self.filter,
            &ProcessSpec {
                num_channels: 1,
                ..*spec
            },
        );
        self.configure();
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        self.key = note.key.min(127);
        self.velocity = f64::from(velocity);
        self.tune();
        self.amplitude.stage = Stage::Attack;
        self.filter_envelope = 1.0;
    }

    fn note_off(&mut self, _velocity: f32) {
        if self.amplitude.stage != Stage::Idle {
            self.amplitude.stage = Stage::Release;
        }
    }

    fn choke(&mut self) {
        self.amplitude.silence();
        self.filter_envelope = 0.0;
        for oscillator in &mut self.oscillators {
            oscillator.reset_phase();
        }
        self.sub.reset_phase();
        Processor::<f64>::reset(&mut self.filter);
        self.cutoff = self.settings.cutoff;
    }

    fn is_active(&self) -> bool {
        self.amplitude.stage != Stage::Idle
    }

    fn render(&mut self, buffer: &mut AudioBuffer) {
        if !self.is_active() || buffer.num_channels() == 0 {
            return;
        }
        let nyquist_guard = self.sample_rate * 0.45;
        let tracking = (f64::from(self.key) - 60.0) / 24.0;
        let depth = self.settings.env_amount * (0.4 + 0.6 * self.velocity);
        let gain = VOICE_LEVEL * self.velocity;
        let sub = self.settings.sub;
        for sample in buffer.channel_mut(0) {
            if !self.is_active() {
                break;
            }
            self.cutoff += (self.settings.cutoff - self.cutoff) * self.cutoff_coefficient;
            let octaves = depth * self.filter_envelope + tracking;
            self.filter_envelope *= self.filter_coefficient;
            let frequency = (self.cutoff * (octaves * LN_2).exp()).clamp(20.0, nyquist_guard);
            self.filter.set_frequency(frequency);

            let tone = 0.5 * (self.oscillators[0].next_sample() + self.oscillators[1].next_sample())
                + sub * self.sub.next_sample();
            let filtered = self.filter.process_sample(0, tone);
            let level = self.amplitude.next();
            *sample += (filtered * level * gain) as f32;
        }
    }

    fn set_param(&mut self, param: ParamId, value: f64) {
        if self.settings.set(param, value) {
            self.configure();
        }
    }
}

/// A synth part's instrument.
pub(crate) type SynthInstrument = Synth<SynthVoice>;

pub(crate) fn new_synth(params: &SynthParams, spec: &ProcessSpec) -> SynthInstrument {
    let settings = SynthSettings::from_params(params);
    let mut synth = Synth::with_voices(SYNTH_VOICES, || SynthVoice::new(settings));
    synth.prepare(spec);
    synth
}
