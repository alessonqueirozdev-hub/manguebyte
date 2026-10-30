//! Analogue drum-machine voices in the manner of the classic 808: a kit of
//! sixteen drums and the long, pitched kick played as a bass.

use core::f64::consts::TAU;
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    Handover, Noise, SILENCE, SoftClip, Tone, add_to_every_channel, db, flush, key_hz, setting,
    t60_factor, velocity_gain,
};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{Oscillator, ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice, Waveform};

/// The six square-wave oscillators the cymbal, the hi-hats and (the last
/// two) the cowbell of the classic machine share, in hertz, as given in K.
/// J. Werner, J. S. Abel and J. O. Smith, "The TR-808 cymbal: a
/// physically-informed, circuit-bendable, digital model", ICMC 2014, from
/// the instrument's service notes.
pub const METAL_HZ: [f64; 6] = [205.3, 304.4, 369.6, 522.7, 540.0, 800.0];

/// How long a drum takes to rise at the start, in seconds: the rise time of
/// the trigger pulse, which keeps the first sample from being a step.
const ONSET_SECONDS: f64 = 0.000_2;

/// How long a choked open hi-hat takes to fall silent.
const CHOKE_SECONDS: f64 = 0.008;

/// The drums of an [`AnalogKitVoice`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AnalogDrum {
    /// Bass drum: a sine that falls in pitch, with the click of the trigger,
    /// through a soft saturator.
    Kick,
    /// Snare: two tuned resonances and high-passed noise.
    Snare,
    /// Low tom: a resonance that falls a little in pitch, with a breath of
    /// noise.
    LowTom,
    /// Mid tom.
    MidTom,
    /// High tom.
    HighTom,
    /// Low conga: a tom's resonance, higher and shorter, without the noise.
    LowConga,
    /// Mid conga.
    MidConga,
    /// High conga.
    HighConga,
    /// Rimshot: two high resonances, high-passed and clipped.
    Rimshot,
    /// Hand clap: band-passed noise in a few quick bursts and a tail.
    Clap,
    /// Cowbell: two square waves through a band-pass.
    Cowbell,
    /// Claves: one short, high resonance.
    Clave,
    /// Maracas: a short burst of high-passed noise.
    Maracas,
    /// Closed hi-hat: the six square waves through a high band-pass,
    /// short. It chokes the open hi-hat.
    ClosedHat,
    /// Open hi-hat: the same, longer.
    OpenHat,
    /// Cymbal: the six square waves in two bands with their own decays.
    Cymbal,
}

impl AnalogDrum {
    /// Every drum, in the order of [`AnalogKitParams::drums`].
    pub const ALL: [Self; 16] = [
        Self::Kick,
        Self::Snare,
        Self::LowTom,
        Self::MidTom,
        Self::HighTom,
        Self::LowConga,
        Self::MidConga,
        Self::HighConga,
        Self::Rimshot,
        Self::Clap,
        Self::Cowbell,
        Self::Clave,
        Self::Maracas,
        Self::ClosedHat,
        Self::OpenHat,
        Self::Cymbal,
    ];

    /// The drum a General MIDI percussion key plays, if any: 35 and 36
    /// kick, 37 rimshot (side stick), 38 and 40 snare, 39 clap, 41 and 43
    /// low tom, 45 and 47 mid tom, 48 and 50 high tom, 42 and 44 closed
    /// hi-hat, 46 open hi-hat, 49, 51, 52, 55, 57 and 59 cymbal, 56
    /// cowbell, 62 high, 63 mid and 64 low conga, 70 maracas and 75
    /// claves.
    pub fn from_key(key: u8) -> Option<Self> {
        Some(match key {
            35 | 36 => Self::Kick,
            37 => Self::Rimshot,
            38 | 40 => Self::Snare,
            39 => Self::Clap,
            41 | 43 => Self::LowTom,
            45 | 47 => Self::MidTom,
            48 | 50 => Self::HighTom,
            42 | 44 => Self::ClosedHat,
            46 => Self::OpenHat,
            49 | 51 | 52 | 55 | 57 | 59 => Self::Cymbal,
            56 => Self::Cowbell,
            62 => Self::HighConga,
            63 => Self::MidConga,
            64 => Self::LowConga,
            70 => Self::Maracas,
            75 => Self::Clave,
            _ => return None,
        })
    }

    /// The General MIDI key that plays this drum.
    pub fn key(self) -> u8 {
        match self {
            Self::Kick => 36,
            Self::Snare => 38,
            Self::LowTom => 41,
            Self::MidTom => 45,
            Self::HighTom => 48,
            Self::LowConga => 64,
            Self::MidConga => 63,
            Self::HighConga => 62,
            Self::Rimshot => 37,
            Self::Clap => 39,
            Self::Cowbell => 56,
            Self::Clave => 75,
            Self::Maracas => 70,
            Self::ClosedHat => 42,
            Self::OpenHat => 46,
            Self::Cymbal => 49,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// How one drum of an [`AnalogKitVoice`] is set, with the meaning each
/// field has for each drum.
///
/// | Drum | `tone` | `snap` |
/// |---|---|---|
/// | kick | brightness of the click | depth of the pitch drop and loudness of the click |
/// | snare | the upper resonance against the lower | the noise (the snares) |
/// | toms | how far the pitch drops | the noise |
/// | congas | how far the pitch drops | - |
/// | rimshot | the upper resonance against the lower | how hard it is clipped |
/// | clap | the band's centre, 800 to 1 600 Hz | the bursts against the tail |
/// | cowbell | the band's centre, 1.2 to 2.6 kHz | the attack against the ring |
/// | claves, maracas | - | - |
/// | hi-hats | the band's centre, 6 to 9 kHz | - |
/// | cymbal | the high band against the low | the stick's attack |
///
/// Values out of range are limited when the kit takes them; a value that
/// is not a number takes the default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrumSettings {
    /// Level, as a gain, 0 to 4.
    pub level: f64,
    /// Tuning, in semitones, -24 to 24.
    pub tune: f64,
    /// How long it rings, as a multiple of the drum's own decay time, 0.1
    /// to 10.
    pub decay: f64,
    /// See the table, 0 to 1.
    pub tone: f64,
    /// See the table, 0 to 1.
    pub snap: f64,
}

impl Default for DrumSettings {
    fn default() -> Self {
        Self {
            level: 1.0,
            tune: 0.0,
            decay: 1.0,
            tone: 0.5,
            snap: 0.5,
        }
    }
}

impl DrumSettings {
    fn limited(self) -> Self {
        let default = Self::default();
        Self {
            level: setting(self.level, 0.0, 4.0, default.level),
            tune: setting(self.tune, -24.0, 24.0, default.tune),
            decay: setting(self.decay, 0.1, 10.0, default.decay),
            tone: setting(self.tone, 0.0, 1.0, default.tone),
            snap: setting(self.snap, 0.0, 1.0, default.snap),
        }
    }
}

/// The settings of an [`AnalogKitVoice`]: one [`DrumSettings`] for each
/// drum, and what is common to them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnalogKitParams {
    /// Each drum's settings, in the order of [`AnalogDrum::ALL`].
    pub drums: [DrumSettings; 16],
    /// How much quieter the lowest velocity is than the highest, in
    /// decibels, 0 to 60. Velocity also scales each drum's snap, from half
    /// at the lowest velocity to all of it at the highest.
    pub velocity_range_db: f64,
    /// How much louder an accented drum is, in decibels, 0 to 24. An
    /// accent also adds a quarter to the snap.
    pub accent_db: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl Default for AnalogKitParams {
    fn default() -> Self {
        Self {
            drums: [DrumSettings::default(); 16],
            velocity_range_db: 24.0,
            accent_db: 4.0,
            level: 0.5,
        }
    }
}

impl AnalogKitParams {
    /// The settings of one drum.
    pub fn drum(&self, drum: AnalogDrum) -> &DrumSettings {
        &self.drums[drum.index()]
    }

    /// The settings of one drum, to change them.
    pub fn drum_mut(&mut self, drum: AnalogDrum) -> &mut DrumSettings {
        &mut self.drums[drum.index()]
    }

    fn limited(self) -> Self {
        Self {
            drums: self.drums.map(DrumSettings::limited),
            velocity_range_db: setting(self.velocity_range_db, 0.0, 60.0, 24.0),
            accent_db: setting(self.accent_db, 0.0, 24.0, 4.0),
            level: setting(self.level, 0.0, 4.0, 0.5),
        }
    }
}

/// The hi-hats of one kit: striking the closed hi-hat chokes an open one
/// that is still ringing, as one pair of cymbals can be only open or
/// closed.
///
/// Give every voice of a kit a clone of the same group, or build them with
/// [`AnalogKitVoice::voices`]. The group is one atomic counter of closed
/// strikes, so the voices stay `Send` and nothing locks.
#[derive(Debug, Clone)]
pub struct ChokeGroup(Arc<AtomicU32>);

impl ChokeGroup {
    /// A group nothing has been struck in. Allocates.
    pub fn new() -> Self {
        Self(Arc::new(AtomicU32::new(0)))
    }

    fn strikes(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for ChokeGroup {
    fn default() -> Self {
        Self::new()
    }
}

/// A resonance struck once: a sine that dies away, its pitch falling from
/// above to where it settles, as the bridged-T circuits of the classic
/// machine ring when their trigger pulse hits them.
#[derive(Debug, Clone, Copy, Default)]
struct Resonance {
    phase: f64,
    frequency: f64,
    /// How far above the frequency the pitch is, as a fraction of it, and
    /// how fast that falls.
    sweep: f64,
    sweep_fall: f64,
    level: f64,
    fall: f64,
}

impl Resonance {
    fn strike(
        &mut self,
        frequency: f64,
        sweep: f64,
        sweep_seconds: f64,
        level: f64,
        decay: f64,
        rate: f64,
    ) {
        self.phase = 0.0;
        self.frequency = frequency;
        self.sweep = sweep;
        self.sweep_fall = t60_factor(sweep_seconds, rate);
        self.level = level;
        self.fall = t60_factor(decay, rate);
    }

    #[inline]
    fn next(&mut self, rate: f64) -> f64 {
        if self.level == 0.0 {
            return 0.0;
        }
        let out = self.level * (TAU * self.phase).sin();
        self.phase += (self.frequency * (1.0 + self.sweep) / rate).min(0.5);
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        self.sweep = flush(self.sweep * self.sweep_fall);
        self.level = flush(self.level * self.fall);
        out
    }
}

/// A level that falls by 60 dB in a set time.
#[derive(Debug, Clone, Copy, Default)]
struct Decay {
    level: f64,
    fall: f64,
}

impl Decay {
    fn start(&mut self, level: f64, seconds: f64, rate: f64) {
        self.level = level;
        self.fall = t60_factor(seconds, rate);
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let level = self.level;
        self.level = flush(self.level * self.fall);
        level
    }
}

/// One drum strike.
#[derive(Debug, Clone)]
pub(crate) struct DrumTone {
    sample_rate: f64,
    drum: AnalogDrum,
    /// The rise at the start, and its step.
    onset: f64,
    onset_step: f64,
    resonances: [Resonance; 2],
    /// The six square waves.
    metal: [Oscillator; 6],
    noise: Noise,
    /// Envelopes of the noise or metal through each band, and of the
    /// clap's tail.
    envelopes: [Decay; 3],
    bands: [StateVariableFilter<f64>; 3],
    high_pass: StateVariableFilter<f64>,
    /// The kick's trigger click: samples left, its length, its level, and a
    /// one-pole low-pass that sets its brightness.
    click_left: u32,
    click_len: u32,
    click_level: f64,
    click_pole: f64,
    click_low: f64,
    clip: SoftClip,
    drive: f64,
    /// The clap's bursts: how many are left, the samples until the next,
    /// and the spacing.
    bursts: u32,
    burst_timer: u32,
    burst_spacing: u32,
    burst_level: f64,
    /// The open hi-hat's choke: the closed strikes counted when it was
    /// struck, and its fade once choked.
    strikes: u32,
    choke: f64,
    choke_step: f64,
    gain: f64,
    /// Whether anything is still sounding.
    alive: bool,
}

impl DrumTone {
    pub(crate) fn new() -> Self {
        let filter = || StateVariableFilter::new(SvfMode::Bandpass, 1_000.0, 1.0);
        Self {
            sample_rate: 48_000.0,
            drum: AnalogDrum::Kick,
            onset: 0.0,
            onset_step: 1.0,
            resonances: [Resonance::default(); 2],
            metal: METAL_HZ.map(|hertz| Oscillator::new(Waveform::Square, hertz)),
            noise: Noise::for_note(0, 0.0, 0),
            envelopes: [Decay::default(); 3],
            bands: [filter(), filter(), filter()],
            high_pass: StateVariableFilter::new(SvfMode::Highpass, 1_000.0, 0.707),
            click_left: 0,
            click_len: 1,
            click_level: 0.0,
            click_pole: 0.0,
            click_low: 0.0,
            clip: SoftClip::default(),
            drive: 1.0,
            bursts: 0,
            burst_timer: 0,
            burst_spacing: 1,
            burst_level: 0.0,
            strikes: 0,
            choke: 1.0,
            choke_step: 0.0,
            gain: 0.0,
            alive: false,
        }
    }

    pub(crate) fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        let spec = ProcessSpec {
            sample_rate,
            max_block_size: 1,
            num_channels: 1,
        };
        for filter in self
            .bands
            .iter_mut()
            .chain(core::iter::once(&mut self.high_pass))
        {
            Processor::<f64>::prepare(filter, &spec);
        }
        for oscillator in &mut self.metal {
            oscillator.set_sample_rate(sample_rate);
        }
        self.onset_step = 1.0 / (ONSET_SECONDS * sample_rate).max(1.0);
        self.choke_step = 1.0 / (CHOKE_SECONDS * sample_rate).max(1.0);
        self.silence();
    }

    fn set_band(&mut self, band: usize, mode: SvfMode, frequency: f64, q: f64) {
        let filter = &mut self.bands[band];
        filter.set_mode(mode);
        filter.set_frequency(frequency.min(0.45 * self.sample_rate));
        filter.set_q(q);
    }

    /// Strikes `drum` with its settings, a velocity from 0 to 1 and an
    /// accent. `strikes` is the choke group's count, for the open hi-hat.
    pub(crate) fn strike(
        &mut self,
        drum: AnalogDrum,
        kit: &AnalogKitParams,
        velocity: f64,
        accent: bool,
        strikes: u32,
    ) {
        self.silence();
        self.alive = true;
        self.drum = drum;
        self.strikes = strikes;
        let rate = self.sample_rate;
        let settings = kit.drums[drum.index()];
        let mut gain = settings.level * kit.level * velocity_gain(velocity, kit.velocity_range_db);
        let mut snap = settings.snap * (0.5 + 0.5 * velocity);
        if accent {
            gain *= db(kit.accent_db);
            snap = (snap + 0.25).min(1.0);
        }
        self.gain = gain;
        let tune = (settings.tune / 12.0).exp2();
        let decay = settings.decay;
        let tone = settings.tone;
        self.noise = Noise::for_note(drum.key(), 0.0, 11);
        for (oscillator, hertz) in self.metal.iter_mut().zip(METAL_HZ) {
            oscillator.set_frequency(hertz * tune);
            oscillator.reset_phase();
        }
        let [low, high] = &mut self.resonances;
        match drum {
            AnalogDrum::Kick => {
                low.strike(49.0 * tune, 2.0 * snap, 0.03, 1.0, 0.7 * decay, rate);
                self.click_len = (0.001 * rate).max(1.0) as u32;
                self.click_left = self.click_len;
                self.click_level = 0.6 * (0.3 + snap);
                let brightness = 400.0 * (4.0 * tone).exp2();
                self.click_pole = (-TAU * brightness.min(0.45 * rate) / rate).exp();
                self.drive = 1.6;
            }
            AnalogDrum::Snare => {
                low.strike(
                    185.0 * tune,
                    0.15,
                    0.04,
                    1.0 - 0.5 * tone,
                    0.16 * decay,
                    rate,
                );
                high.strike(
                    330.0 * tune,
                    0.15,
                    0.04,
                    0.3 + 0.6 * tone,
                    0.1 * decay,
                    rate,
                );
                self.envelopes[0].start(0.9 * snap, 0.22 * decay, rate);
                self.set_band(0, SvfMode::Highpass, 1_000.0, 0.707);
                self.set_band(1, SvfMode::Lowpass, 7_500.0, 0.707);
            }
            AnalogDrum::LowTom | AnalogDrum::MidTom | AnalogDrum::HighTom => {
                let (hertz, seconds) = match drum {
                    AnalogDrum::LowTom => (82.0, 0.6),
                    AnalogDrum::MidTom => (120.0, 0.5),
                    _ => (165.0, 0.42),
                };
                low.strike(
                    hertz * tune,
                    0.1 + 0.5 * tone,
                    0.1,
                    1.0,
                    seconds * decay,
                    rate,
                );
                self.envelopes[0].start(0.25 * snap, 0.05, rate);
                self.set_band(0, SvfMode::Lowpass, 2_000.0, 0.707);
            }
            AnalogDrum::LowConga | AnalogDrum::MidConga | AnalogDrum::HighConga => {
                let hertz = match drum {
                    AnalogDrum::LowConga => 170.0,
                    AnalogDrum::MidConga => 250.0,
                    _ => 370.0,
                };
                low.strike(
                    hertz * tune,
                    0.05 + 0.2 * tone,
                    0.04,
                    1.0,
                    0.25 * decay,
                    rate,
                );
            }
            AnalogDrum::Rimshot => {
                low.strike(
                    455.0 * tune,
                    0.0,
                    0.01,
                    1.0 - 0.5 * tone,
                    0.035 * decay,
                    rate,
                );
                high.strike(
                    1_667.0 * tune,
                    0.0,
                    0.01,
                    0.5 + 0.5 * tone,
                    0.025 * decay,
                    rate,
                );
                self.high_pass.set_frequency(400.0);
                self.drive = 1.5 + 3.0 * snap;
            }
            AnalogDrum::Clap => {
                let centre = 800.0 * tone.exp2();
                self.set_band(0, SvfMode::Bandpass, centre, 1.2);
                self.bursts = 3;
                self.burst_spacing = (0.01 * rate) as u32;
                self.burst_timer = 0;
                self.burst_level = 0.6 + 0.8 * snap;
                // The tail starts as the last burst does and rings on.
                self.envelopes[1].start(0.0, 0.18 * decay, rate);
                self.high_pass.set_frequency(500.0);
            }
            AnalogDrum::Cowbell => {
                let centre = 1_200.0 * (1.12 * tone).exp2();
                self.set_band(0, SvfMode::Bandpass, centre, 0.9);
                self.envelopes[0].start(0.4 + 0.6 * snap, 0.06 * decay, rate);
                self.envelopes[1].start(0.35, 0.45 * decay, rate);
            }
            AnalogDrum::Clave => {
                low.strike(2_500.0 * tune, 0.0, 0.01, 1.0, 0.05 * decay, rate);
            }
            AnalogDrum::Maracas => {
                self.envelopes[0].start(1.0, 0.06 * decay, rate);
                self.high_pass.set_frequency(6_000.0);
            }
            AnalogDrum::ClosedHat | AnalogDrum::OpenHat => {
                let centre = 6_000.0 * (0.585 * tone).exp2();
                self.set_band(0, SvfMode::Bandpass, centre, 1.2);
                let seconds = if drum == AnalogDrum::ClosedHat {
                    0.07
                } else {
                    0.5
                };
                self.envelopes[0].start(1.0, seconds * decay, rate);
                self.high_pass.set_frequency(6_000.0);
            }
            AnalogDrum::Cymbal => {
                self.set_band(0, SvfMode::Bandpass, 3_440.0, 1.6);
                self.set_band(1, SvfMode::Bandpass, 7_100.0, 1.6);
                self.set_band(2, SvfMode::Bandpass, 7_100.0, 1.0);
                self.envelopes[0].start(1.0 - 0.5 * tone, 1.4 * decay, rate);
                self.envelopes[1].start(0.4 + 0.6 * tone, 0.7 * decay, rate);
                self.envelopes[2].start(1.2 * snap, 0.08, rate);
                self.high_pass.set_frequency(2_000.0);
            }
        }
    }

    /// The six square waves, summed and scaled to about full scale.
    #[inline]
    fn metal(&mut self) -> f64 {
        self.metal
            .iter_mut()
            .map(Oscillator::next_sample)
            .sum::<f64>()
            / 6.0
    }

    #[inline]
    pub(crate) fn next(&mut self) -> f64 {
        let rate = self.sample_rate;
        let loudest: f64;
        let out = match self.drum {
            AnalogDrum::Kick => {
                let body = self.resonances[0].next(rate);
                let mut click = 0.0;
                if self.click_left > 0 {
                    let t = 1.0 - f64::from(self.click_left) / f64::from(self.click_len);
                    click = self.click_level * (core::f64::consts::PI * t).sin();
                    self.click_left -= 1;
                }
                self.click_low = flush(click + self.click_pole * (self.click_low - click));
                loudest = self.resonances[0].level.max(self.click_low.abs());
                let x = body + self.click_low;
                0.8 * self.clip.next(self.drive * x) / self.drive.tanh()
            }
            AnalogDrum::Snare => {
                let body = self.resonances[0].next(rate) + self.resonances[1].next(rate);
                let level = self.envelopes[0].next();
                let noise = self.noise.next();
                let noise = self.bands[0].process_sample(0, noise);
                let noise = self.bands[1].process_sample(0, noise);
                loudest = self.resonances[0]
                    .level
                    .max(self.resonances[1].level)
                    .max(level);
                0.55 * body + 0.8 * level * noise
            }
            AnalogDrum::LowTom | AnalogDrum::MidTom | AnalogDrum::HighTom => {
                let body = self.resonances[0].next(rate);
                let level = self.envelopes[0].next();
                let noise = self.bands[0].process_sample(0, self.noise.next());
                loudest = self.resonances[0].level.max(level);
                0.75 * (body + level * noise)
            }
            AnalogDrum::LowConga | AnalogDrum::MidConga | AnalogDrum::HighConga => {
                loudest = self.resonances[0].level;
                0.7 * self.resonances[0].next(rate)
            }
            AnalogDrum::Clave => {
                loudest = self.resonances[0].level;
                0.6 * self.resonances[0].next(rate)
            }
            AnalogDrum::Rimshot => {
                let body = self.resonances[0].next(rate) + self.resonances[1].next(rate);
                loudest = self.resonances[0].level.max(self.resonances[1].level);
                let body = self.high_pass.process_sample(0, body);
                0.6 * self.clip.next(self.drive * body)
            }
            AnalogDrum::Clap => {
                let mut level = 0.0;
                if self.bursts > 0 {
                    if self.burst_timer == 0 {
                        self.envelopes[0].start(self.burst_level, 0.012, rate);
                        self.bursts -= 1;
                        self.burst_timer = self.burst_spacing;
                        if self.bursts == 0 {
                            // The last burst sets the tail ringing, at the
                            // rate set when the clap was struck.
                            self.envelopes[1].level = 0.7;
                        }
                    }
                    self.burst_timer -= 1;
                }
                level += self.envelopes[0].next() + self.envelopes[1].next();
                let noise = self.bands[0].process_sample(0, self.noise.next());
                loudest = level.max(if self.bursts > 0 { 1.0 } else { 0.0 });
                1.6 * self.high_pass.process_sample(0, level * noise)
            }
            AnalogDrum::Cowbell => {
                let metal = self.metal[4].next_sample() + self.metal[5].next_sample();
                let level = self.envelopes[0].next() + self.envelopes[1].next();
                loudest = level;
                0.35 * level * self.bands[0].process_sample(0, metal)
            }
            AnalogDrum::Maracas => {
                let level = self.envelopes[0].next();
                loudest = level;
                0.5 * level * self.high_pass.process_sample(0, self.noise.next())
            }
            AnalogDrum::ClosedHat | AnalogDrum::OpenHat => {
                let metal = self.metal();
                let level = self.envelopes[0].next();
                loudest = level;
                let band = self.bands[0].process_sample(0, metal);
                4.5 * level * self.high_pass.process_sample(0, band)
            }
            AnalogDrum::Cymbal => {
                let metal = self.metal();
                let low = self.envelopes[0].next() * self.bands[0].process_sample(0, metal);
                let high = self.envelopes[1].next() * self.bands[1].process_sample(0, metal);
                let stick = self.envelopes[2].next() * self.bands[2].process_sample(0, metal);
                loudest = self.envelopes[0]
                    .level
                    .max(self.envelopes[1].level)
                    .max(self.envelopes[2].level);
                2.6 * self.high_pass.process_sample(0, low + high + stick)
            }
        };
        let mut out = out * self.onset * self.gain;
        self.onset = (self.onset + self.onset_step).min(1.0);
        if self.choke < 1.0 {
            out *= self.choke;
            self.choke -= self.choke_step;
            if self.choke <= 0.0 {
                self.choke = 0.0;
                self.alive = false;
            }
        }
        if loudest < SILENCE {
            self.alive = false;
        }
        out
    }

    /// Chokes an open hi-hat struck before the group's latest closed
    /// strike.
    fn check_choke(&mut self, group: &ChokeGroup) {
        if self.alive
            && self.drum == AnalogDrum::OpenHat
            && self.choke >= 1.0
            && group.strikes() != self.strikes
        {
            self.choke = 1.0 - self.choke_step;
        }
    }
}

impl Tone for DrumTone {
    fn is_sounding(&self) -> bool {
        self.alive
    }

    fn silence(&mut self) {
        self.alive = false;
        self.onset = 0.0;
        self.resonances = [Resonance::default(); 2];
        self.envelopes = [Decay::default(); 3];
        for filter in self
            .bands
            .iter_mut()
            .chain(core::iter::once(&mut self.high_pass))
        {
            Processor::<f64>::reset(filter);
        }
        self.click_left = 0;
        self.click_low = 0.0;
        self.clip.reset();
        self.bursts = 0;
        self.choke = 1.0;
    }
}

/// A drum-machine voice in the manner of the classic 808: a voice for
/// [`Synth`](crate::Synth) that plays one of sixteen analogue drums,
/// chosen by the key on the General MIDI map ([`AnalogDrum::from_key`]).
///
/// The drums are modelled on the published descriptions of that machine's
/// circuits rather than on recordings of it: bridged-T resonators struck by
/// a trigger pulse (kick, snare, toms, congas, rimshot, claves), filtered
/// noise (snare, clap, maracas), and a bank of six square-wave oscillators
/// at inharmonic frequencies ([`METAL_HZ`]) through band-passes (hi-hats,
/// cymbal, cowbell). The squares are band-limited (the crate's
/// [`Oscillator`]); the kick's and the rimshot's saturators work on the
/// antiderivative of their curve, which takes most of the aliasing out of
/// the harmonics they make. Each drum is set with a [`DrumSettings`]:
/// level, tune, decay, and a tone and a snap whose meaning the table there
/// gives.
///
/// Velocity sets the level and part of the snap; an accent is louder by
/// [`AnalogKitParams::accent_db`] and snappier. Drums are one-shots: a note
/// off changes nothing, and a key with no drum is silent. Striking the
/// closed hi-hat chokes an open one in the same [`ChokeGroup`]. A drum
/// struck again while it rings, or a voice taken over for another, fades
/// its old strike out over [`HANDOVER_SECONDS`](super::HANDOVER_SECONDS)
/// while the new one starts. The noise of every drum is seeded by the drum,
/// so a pattern sounds the same every time.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{AnalogDrum, AnalogKitParams, AnalogKitVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut kit = Synth::new(AnalogKitVoice::voices(8, AnalogKitParams::default()));
/// kit.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// kit.note_on_accented(NoteRef::new(9, AnalogDrum::Kick.key()), 1.0, true);
/// kit.note_on(NoteRef::new(9, AnalogDrum::ClosedHat.key()), 0.6);
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// kit.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct AnalogKitVoice {
    params: AnalogKitParams,
    group: ChokeGroup,
    tones: Handover<DrumTone>,
}

impl AnalogKitVoice {
    /// A voice with a choke group of its own: an open hi-hat on it is
    /// choked only by a closed one on the same voice. Use
    /// [`with_group`](Self::with_group) or [`voices`](Self::voices) for a
    /// whole kit.
    pub fn new(params: AnalogKitParams) -> Self {
        Self::with_group(params, &ChokeGroup::new())
    }

    /// A voice in the given choke group. Every voice of one kit should
    /// share it.
    pub fn with_group(params: AnalogKitParams, group: &ChokeGroup) -> Self {
        Self {
            params: params.limited(),
            group: group.clone(),
            tones: Handover::new(DrumTone::new(), DrumTone::new()),
        }
    }

    /// `count` voices sharing one new choke group, for a
    /// [`Synth`](crate::Synth). Allocates.
    pub fn voices(count: usize, params: AnalogKitParams) -> Vec<Self> {
        let group = ChokeGroup::new();
        (0..count)
            .map(|_| Self::with_group(params, &group))
            .collect()
    }

    /// Takes new settings, limited to their ranges. Real-time safe. They
    /// apply from the next strike.
    pub fn set_params(&mut self, params: AnalogKitParams) {
        self.params = params.limited();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &AnalogKitParams {
        &self.params
    }
}

impl<S: Sample> Voice<S> for AnalogKitVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.tones.prepare(spec.sample_rate);
        self.tones.now.prepare(spec.sample_rate);
        self.tones.old.prepare(spec.sample_rate);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let Some(drum) = AnalogDrum::from_key(note.key) else {
            return;
        };
        if self.tones.now.is_sounding() {
            // Struck again while it rings: the last strike fades out.
            self.tones.choke();
        }
        if drum == AnalogDrum::ClosedHat {
            self.group.0.fetch_add(1, Ordering::Relaxed);
        }
        let params = self.params;
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let strikes = self.group.strikes();
        self.tones
            .now
            .strike(drum, &params, velocity, accent, strikes);
    }

    fn note_off(&mut self, _velocity: f32) {}

    fn choke(&mut self) {
        self.tones.choke();
    }

    fn is_active(&self) -> bool {
        self.tones.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        let group = &self.group;
        self.tones.now.check_choke(group);
        self.tones.old.check_choke(group);
        if !self.tones.is_active() {
            return;
        }
        let tones = &mut self.tones;
        add_to_every_channel(buffer, || tones.next(DrumTone::next));
    }
}

/// The settings of a [`KickBassVoice`]. Out-of-range values are limited; a
/// value that is not a number takes the default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KickBassParams {
    /// Tuning, in semitones, -24 to 24.
    pub tune: f64,
    /// How long the note takes to fall 60 dB while held, 0.05 to 20
    /// seconds: the drum machine's long decay.
    pub decay: f64,
    /// How long it takes to fall 60 dB once released, 0.005 to 20.
    pub release: f64,
    /// How far above the note the pitch starts, in semitones, 0 to 48: the
    /// punch of the attack.
    pub sweep: f64,
    /// How long that drop takes to fall 60 dB (to a thousandth of its
    /// depth), 0.001 to 1 second.
    pub sweep_time: f64,
    /// How hard the saturator is driven, 0 to 1: from a clean sine to a
    /// growl full of harmonics.
    pub drive: f64,
    /// The click of the trigger at the start, 0 to 1.
    pub click: f64,
    /// How long a slur from one note to the next takes, 0 to 2 seconds: it
    /// is 95% of the way there in this time. See
    /// [`Synth::set_legato`](crate::Synth::set_legato).
    pub glide: f64,
    /// How much quieter the lowest velocity is, in decibels, 0 to 60.
    pub velocity_range_db: f64,
    /// How much louder an accented note is, in decibels, 0 to 24. An accent
    /// also clicks harder.
    pub accent_db: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl Default for KickBassParams {
    /// The classic long kick played as a bass: two and a half seconds of
    /// decay, an octave of pitch drop at the start, a little drive.
    fn default() -> Self {
        Self {
            tune: 0.0,
            decay: 2.5,
            release: 0.25,
            sweep: 12.0,
            sweep_time: 0.06,
            drive: 0.3,
            click: 0.2,
            glide: 0.08,
            velocity_range_db: 24.0,
            accent_db: 3.0,
            level: 0.45,
        }
    }
}

impl KickBassParams {
    fn limited(self) -> Self {
        let default = Self::default();
        Self {
            tune: setting(self.tune, -24.0, 24.0, default.tune),
            decay: setting(self.decay, 0.05, 20.0, default.decay),
            release: setting(self.release, 0.005, 20.0, default.release),
            sweep: setting(self.sweep, 0.0, 48.0, default.sweep),
            sweep_time: setting(self.sweep_time, 0.001, 1.0, default.sweep_time),
            drive: setting(self.drive, 0.0, 1.0, default.drive),
            click: setting(self.click, 0.0, 1.0, default.click),
            glide: setting(self.glide, 0.0, 2.0, default.glide),
            velocity_range_db: setting(self.velocity_range_db, 0.0, 60.0, 24.0),
            accent_db: setting(self.accent_db, 0.0, 24.0, default.accent_db),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }

    /// The gain into the saturator: a quarter, where `tanh` is all but a
    /// straight line, up to 8.
    fn drive_gain(&self) -> f64 {
        0.25 + 7.75 * self.drive
    }
}

/// How long the kick bass takes to rise at the start of a note, from the
/// level it is at.
const KICK_ATTACK_SECONDS: f64 = 0.001;

/// How long a change of the level a note asks for takes.
const GAIN_GLIDE_SECONDS: f64 = 0.005;

/// The long kick of the classic drum machine played as a bass, the "808":
/// a voice for [`Synth`](crate::Synth) whose key sets the pitch.
///
/// A sine starts an octave or so above the note ([`KickBassParams::sweep`])
/// and drops to it, rings for seconds, and goes through a `tanh` saturator
/// whose drive turns the sine into a growl; the saturator works on the
/// curve's antiderivative, which keeps the harmonics it makes from folding
/// back. A short pulse at the start is the trigger's click.
///
/// It is meant to be played one note at a time, as the original is: the
/// oscillator keeps turning from note to note, so a new note, even on a
/// voice taken over from another, starts from where the last one was and
/// rises to full over a millisecond instead of jumping. On a synth set to
/// [legato](crate::Synth::set_legato) a note struck while another is held
/// glides to the new pitch ([`KickBassParams::glide`]) without striking
/// again, the 808 slide of trap music. A voice that is silent when it is
/// choked starts its next note as new.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{KickBassParams, KickBassVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut bass = Synth::with_voices(1, || KickBassVoice::new(KickBassParams::default()));
/// bass.set_legato(true);
/// bass.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 1 });
/// bass.note_on(NoteRef::new(0, 33), 1.0);
/// bass.note_on(NoteRef::new(0, 36), 1.0); // slides up a minor third
/// let mut block = AudioBuffer::<f32>::new(1, 256);
/// bass.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct KickBassVoice {
    params: KickBassParams,
    sample_rate: f64,
    phase: f64,
    /// The pitch now and where it is gliding, as key numbers, and how much
    /// of the way it goes in a sample.
    pitch: f64,
    target: f64,
    glide: f64,
    /// How far above the pitch the drop still is, in semitones.
    sweep: f64,
    sweep_fall: f64,
    /// The envelope: its level, whether it is rising, and its falls.
    level: f64,
    rising: bool,
    attack_step: f64,
    decay_fall: f64,
    release_fall: f64,
    released: bool,
    /// The level the note asks for and the level heard, gliding to it.
    gain: f64,
    gain_now: f64,
    gain_glide: f64,
    click_left: u32,
    click_len: u32,
    click_level: f64,
    clip: SoftClip,
    /// Whether the last choke kept the sound going for a note to take
    /// over.
    carried: bool,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
}

impl KickBassVoice {
    /// A voice with these settings.
    pub fn new(params: KickBassParams) -> Self {
        let mut voice = Self {
            params: params.limited(),
            sample_rate: 48_000.0,
            phase: 0.0,
            pitch: 36.0,
            target: 36.0,
            glide: 1.0,
            sweep: 0.0,
            sweep_fall: 0.0,
            level: 0.0,
            rising: false,
            attack_step: 1.0,
            decay_fall: 0.0,
            release_fall: 0.0,
            released: false,
            gain: 0.0,
            gain_now: 0.0,
            gain_glide: 1.0,
            click_left: 0,
            click_len: 1,
            click_level: 0.0,
            clip: SoftClip::default(),
            carried: false,
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note takes the new decay, release and glide at once.
    pub fn set_params(&mut self, params: KickBassParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &KickBassParams {
        &self.params
    }

    fn configure(&mut self) {
        let rate = self.sample_rate;
        let params = &self.params;
        self.attack_step = 1.0 / (KICK_ATTACK_SECONDS * rate).max(1.0);
        self.decay_fall = t60_factor(params.decay, rate);
        self.release_fall = t60_factor(params.release, rate);
        self.sweep_fall = t60_factor(params.sweep_time, rate);
        // 95% of the way in the glide time: e^-3 is 5%.
        self.glide = if params.glide > 0.0 {
            1.0 - (-3.0 / (params.glide * rate)).exp()
        } else {
            1.0
        };
        self.gain_glide = 1.0 - (-3.0 / (GAIN_GLIDE_SECONDS * rate)).exp();
        self.click_len = (0.001 * rate).max(1.0) as u32;
    }

    fn sounding(&self) -> bool {
        self.rising || self.level > 0.0
    }

    /// Back to the state of a new voice, but for the settings.
    fn reset(&mut self) {
        self.phase = 0.0;
        self.sweep = 0.0;
        self.level = 0.0;
        self.rising = false;
        self.released = false;
        self.gain = 0.0;
        self.gain_now = 0.0;
        self.click_left = 0;
        self.clip.reset();
        self.carried = false;
    }

    fn note_gain(&self, velocity: f32, accent: bool) -> f64 {
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let mut gain = velocity_gain(velocity, self.params.velocity_range_db);
        if accent {
            gain *= db(self.params.accent_db);
        }
        gain
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let rate = self.sample_rate;
        self.pitch += (self.target - self.pitch) * self.glide;
        let key = self.pitch + self.sweep + self.params.tune + self.bend + self.tuning;
        self.sweep = flush(self.sweep * self.sweep_fall);
        let sine = (TAU * self.phase).sin();
        self.phase += (key_hz(key) / rate).min(0.5);
        if self.phase >= 1.0 {
            self.phase -= 1.0;
        }
        if self.rising {
            self.level += self.attack_step;
            if self.level >= 1.0 {
                self.level = 1.0;
                self.rising = false;
            }
        } else {
            let fall = if self.released {
                self.release_fall
            } else {
                self.decay_fall
            };
            self.level *= fall;
            if self.level < SILENCE {
                self.level = 0.0;
            }
        }
        let mut click = 0.0;
        if self.click_left > 0 {
            let t = 1.0 - f64::from(self.click_left) / f64::from(self.click_len);
            click = self.click_level * (core::f64::consts::PI * t).sin();
            self.click_left -= 1;
        }
        self.gain_now += (self.gain - self.gain_now) * self.gain_glide;
        let drive = self.params.drive_gain();
        let shaped = self.clip.next(drive * (sine * self.level + click)) / drive.tanh();
        shaped * self.gain_now * self.params.level
    }
}

impl<S: Sample> Voice<S> for KickBassVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.configure();
        self.reset();
    }

    fn set_tuning(&mut self, tuning: &Tuning) {
        self.key_tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let key = self.key_tuning.key_pitch(note.key.min(127));
        if !self.sounding() {
            // From silence the sine starts at the top of the drop, at the
            // start of its turn.
            self.phase = 0.0;
            self.gain_now = self.note_gain(velocity, accent);
        }
        self.carried = false;
        self.pitch = key;
        self.target = key;
        self.sweep = self.params.sweep;
        self.rising = true;
        self.released = false;
        self.gain = self.note_gain(velocity, accent);
        let click = if accent {
            (self.params.click + 0.3).min(1.0)
        } else {
            self.params.click
        };
        self.click_level = click;
        self.click_left = self.click_len;
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.sounding() {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        self.target = self.key_tuning.key_pitch(note.key.min(127));
        self.gain = self.note_gain(velocity, accent);
        self.released = false;
    }

    fn note_off(&mut self, _velocity: f32) {
        self.released = true;
    }

    /// A voice still sounding is being taken over: it keeps turning, so
    /// that the next note continues from it. Choked again, or choked when
    /// silent, it goes back to the state it had when new.
    fn choke(&mut self) {
        if self.sounding() && !self.carried {
            self.carried = true;
        } else {
            self.reset();
        }
        self.tuning = 0.0;
    }

    fn is_active(&self) -> bool {
        self.sounding()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        if !self.sounding() {
            return;
        }
        add_to_every_channel(buffer, || self.next());
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Synth;
    use crate::instruments::measure::{
        Act, RATE, accented, between, centroid, cents, decibels, finite_within, frequency_near,
        largest_jump, note, off, on, peak, play, rms, synth_of, t60_at, time_to_fall,
    };

    fn kit() -> Vec<AnalogKitVoice> {
        AnalogKitVoice::voices(4, AnalogKitParams::default())
    }

    fn strike(drum: AnalogDrum, velocity: f32, accent: bool, seconds: f64) -> Vec<f64> {
        let act = Act::On {
            key: drum.key(),
            velocity,
            accent,
        };
        note(kit(), act, seconds, seconds)
    }

    #[test]
    fn every_key_of_the_map_plays_its_drum_and_others_are_silent() {
        for drum in AnalogDrum::ALL {
            assert_eq!(AnalogDrum::from_key(drum.key()), Some(drum));
            let signal = strike(drum, 1.0, false, 0.3);
            let loudest = peak(&signal);
            println!(
                "{drum:?}: peak {:.1} dB, rms {:.1} dB, centroid {:.0} Hz",
                decibels(loudest),
                decibels(rms(between(&signal, 0.0, 0.05))),
                centroid(&signal, 8_192),
            );
            // Between -18 and -6 dB at full velocity, so a kit leaves room
            // for an accent and for drums struck together.
            assert!(
                (0.125..0.5).contains(&loudest),
                "{drum:?} peaks at {loudest}"
            );
        }
        assert_eq!(AnalogDrum::from_key(60), None);
        assert_eq!(peak(&note(kit(), on(60, 1.0), 0.1, 0.1)), 0.0);
    }

    #[test]
    fn the_kick_drops_to_its_pitch_and_rings_for_its_decay() {
        let signal = strike(AnalogDrum::Kick, 1.0, false, 1.5);
        // After the drop, the pitch is the resonance's, tuned by the
        // setting.
        let measured = frequency_near(between(&signal, 0.3, 1.2), 49.0);
        assert!(cents(measured, 49.0).abs() < 1.0, "{measured}");
        let mut params = AnalogKitParams::default();
        params.drum_mut(AnalogDrum::Kick).tune = 7.0;
        params.drum_mut(AnalogDrum::Kick).decay = 3.0;
        let act = on(AnalogDrum::Kick.key(), 1.0);
        let tuned = note(AnalogKitVoice::voices(1, params), act, 3.0, 3.0);
        let expected = 49.0 * (7.0f64 / 12.0).exp2();
        let measured = frequency_near(between(&tuned, 0.3, 1.5), expected);
        assert!(cents(measured, expected).abs() < 1.0, "{measured}");
        // The decay: 0.7 s, three times.
        let decay = t60_at(&tuned, expected, 0.3, 1.8);
        assert!((decay / 2.1 - 1.0).abs() < 0.05, "{decay}");
        // It starts above the note.
        let start = frequency_near(between(&signal, 0.0, 0.012), 70.0);
        assert!(start > 55.0, "{start}");
    }

    #[test]
    fn velocity_and_accent_change_level_and_brightness() {
        for drum in [
            AnalogDrum::Kick,
            AnalogDrum::Snare,
            AnalogDrum::ClosedHat,
            AnalogDrum::Clap,
        ] {
            let loud = strike(drum, 1.0, false, 0.3);
            let soft = strike(drum, 0.5, false, 0.3);
            let strong = strike(drum, 0.8, true, 0.3);
            let plain = strike(drum, 0.8, false, 0.3);
            let velocity_db = decibels(peak(&loud) / peak(&soft));
            let accent_db = decibels(peak(&strong) / peak(&plain));
            // Over the attack, where velocity changes the sound.
            let velocity_centroid = centroid(&loud, 2_048) / centroid(&soft, 2_048);
            println!(
                "{drum:?}: velocity {velocity_db:.1} dB ×{velocity_centroid:.2}, accent {accent_db:.1} dB"
            );
            assert!((velocity_db - 12.0).abs() < 4.0, "{drum:?}: {velocity_db}");
            assert!((accent_db - 4.0).abs() < 2.5, "{drum:?}: {accent_db}");
            if matches!(drum, AnalogDrum::Kick | AnalogDrum::Snare) {
                assert!(velocity_centroid > 1.005, "{drum:?}: {velocity_centroid}");
            }
        }
    }

    #[test]
    fn a_closed_hat_chokes_the_open_one() {
        let open = AnalogDrum::OpenHat.key();
        let closed = AnalogDrum::ClosedHat.key();
        let at = (0.15 * RATE) as usize;
        let mut synth = synth_of(kit());
        let choked = play(
            &mut synth,
            &[(0, on(open, 1.0)), (at, on(closed, 1.0))],
            2 * at,
            128,
        );
        let mut synth = synth_of(kit());
        let alone = play(&mut synth, &[(at, on(closed, 1.0))], 2 * at, 128);
        let left: Vec<f64> = choked.iter().zip(&alone).map(|(a, b)| a - b).collect();
        // The open hat stops within the choke time, without a click.
        let fade = (CHOKE_SECONDS * RATE) as usize + 2;
        assert!(peak(&left[at + fade..]) == 0.0);
        assert!(
            largest_jump(&left[at - 10..at + fade]) <= largest_jump(&left[at - 500..at]) * 1.05
        );
        // Without a shared group it rings on.
        let mut synth = synth_of([
            AnalogKitVoice::new(AnalogKitParams::default()),
            AnalogKitVoice::new(AnalogKitParams::default()),
        ]);
        let free = play(
            &mut synth,
            &[(0, on(open, 1.0)), (at, on(closed, 1.0))],
            2 * at,
            128,
        );
        assert!(peak(&free[at + 2_000..]) > 0.01);
    }

    #[test]
    fn strikes_start_and_are_taken_over_without_a_click() {
        for drum in AnalogDrum::ALL {
            let signal = strike(drum, 1.0, false, 0.3);
            // The rise, against the next 5 ms at full level. A noise burst is
            // at its loudest in the rise, so it may step a little further.
            let rise = (ONSET_SECONDS * RATE) as usize + 1;
            let onset = largest_jump(&signal[..rise]);
            let body = largest_jump(&signal[rise - 1..rise + 240]);
            assert_eq!(signal[0], 0.0, "{drum:?}");
            assert!(onset <= 1.5 * body, "{drum:?}: {onset} against {body}");
            // Struck again while ringing, on one voice: the first strike
            // fades out under the second.
            let again = (0.05 * RATE) as usize;
            let key = drum.key();
            let mut synth = synth_of(AnalogKitVoice::voices(1, AnalogKitParams::default()));
            let twice = play(
                &mut synth,
                &[(0, on(key, 1.0)), (again, on(key, 1.0))],
                3 * again,
                128,
            );
            let mut synth = synth_of(AnalogKitVoice::voices(1, AnalogKitParams::default()));
            let second = play(&mut synth, &[(again, on(key, 1.0))], 3 * again, 128);
            let residual: Vec<f64> = twice.iter().zip(&second).map(|(a, b)| a - b).collect();
            let at_strike = largest_jump(&residual[again - 10..again + 400]);
            let before = largest_jump(&residual[rise..again]);
            assert!(
                at_strike <= 1.05 * before + 1e-12,
                "{drum:?}: {at_strike} against {before}"
            );
            assert!(peak(&residual[again + 400..]) < 1e-12, "{drum:?}");
        }
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let mut script = Vec::new();
        for (step, drum) in AnalogDrum::ALL.iter().enumerate() {
            script.push((step * 1_500, accented(drum.key(), 0.8)));
            script.push((step * 1_500 + 700, on(AnalogDrum::ClosedHat.key(), 0.5)));
        }
        let run = |block| play(&mut synth_of(kit()), &script, 30_000, block);
        let reference = run(256);
        for block in [1, 7, 64, 1_000] {
            assert!(run(block) == reference, "blocks of {block}");
        }
    }

    #[test]
    fn extreme_settings_stay_finite() {
        let wild = DrumSettings {
            level: f64::INFINITY,
            tune: 1e9,
            decay: f64::NAN,
            tone: -5.0,
            snap: 1e9,
        };
        let params = AnalogKitParams {
            drums: [wild; 16],
            velocity_range_db: f64::NAN,
            accent_db: 1e9,
            level: 1e9,
        };
        let mut script = Vec::new();
        for (step, drum) in AnalogDrum::ALL.iter().enumerate() {
            script.push((step * 200, accented(drum.key(), 1.0)));
        }
        let signal = play(
            &mut synth_of(AnalogKitVoice::voices(16, params)),
            &script,
            8_000,
            64,
        );
        assert!(finite_within(&signal, 16.0 * 4.0 * 4.0 * 16.0 * 2.0));
    }

    fn bass(params: KickBassParams) -> [KickBassVoice; 1] {
        [KickBassVoice::new(params)]
    }

    #[test]
    fn the_kick_bass_is_in_tune_and_rings_for_its_decay() {
        let clean = KickBassParams {
            drive: 0.0,
            ..KickBassParams::default()
        };
        let mut worst = 0.0f64;
        for key in [24u8, 28, 33, 36, 43, 48, 55] {
            let signal = note(bass(clean), on(key, 1.0), 1.0, 1.0);
            let expected = key_hz(f64::from(key));
            let measured = frequency_near(between(&signal, 0.4, 1.0), expected);
            worst = worst.max(cents(measured, expected).abs());
        }
        println!("worst {worst:.4} cents");
        assert!(worst < 1.0);
        let signal = note(bass(clean), on(36, 1.0), 2.0, 2.0);
        let decay = t60_at(&signal, key_hz(36.0), 0.4, 1.8);
        assert!((decay / clean.decay - 1.0).abs() < 0.03, "{decay}");
        // Released, it falls in its release time.
        let signal = note(bass(clean), on(36, 1.0), 0.5, 1.0);
        let fall = time_to_fall(between(&signal, 0.5, 1.0), 40.0);
        assert!(fall < clean.release, "{fall}");
    }

    #[test]
    fn the_kick_bass_slides_and_answers_velocity_accent_and_drive() {
        let params = KickBassParams::default();
        let mut synth: Synth<KickBassVoice, f64> = synth_of(bass(params));
        synth.set_legato(true);
        let slide = (0.5 * RATE) as usize;
        let signal = play(
            &mut synth,
            &[(0, on(33, 1.0)), (slide, on(40, 1.0))],
            2 * slide,
            128,
        );
        // No new strike: the level keeps falling through the slide.
        let before = rms(&signal[slide - 2_400..slide]);
        let after = rms(&signal[slide..slide + 2_400]);
        assert!(after < before, "{after} against {before}");
        // At the new pitch once the glide is over.
        let end = slide + (params.glide * RATE) as usize;
        let measured = frequency_near(&signal[end + 2_400..2 * slide], key_hz(40.0));
        assert!(cents(measured, key_hz(40.0)).abs() < 2.0, "{measured}");
        // Half way through the glide it is between the two.
        let middle = frequency_near(&signal[slide + 960..slide + 1_920], key_hz(36.0));
        assert!(middle > key_hz(33.5) && middle < key_hz(39.5), "{middle}");

        let loud = note(bass(params), on(36, 1.0), 0.3, 0.3);
        let soft = note(bass(params), on(36, 0.5), 0.3, 0.3);
        let strong = note(bass(params), accented(36, 0.5), 0.3, 0.3);
        let quiet = decibels(rms(&loud) / rms(&soft));
        let accent = decibels(rms(&strong) / rms(&soft));
        println!("velocity {quiet:.1} dB, accent {accent:.1} dB");
        // The saturator makes loud notes less loud than they ask.
        assert!(quiet > 6.0 && quiet < 12.5, "{quiet}");
        assert!(accent > 1.5 && accent < 3.5, "{accent}");
        // Drive adds harmonics.
        let clean = note(
            bass(KickBassParams {
                drive: 0.0,
                ..params
            }),
            on(36, 1.0),
            0.3,
            0.3,
        );
        let driven = note(
            bass(KickBassParams {
                drive: 1.0,
                ..params
            }),
            on(36, 1.0),
            0.3,
            0.3,
        );
        assert!(centroid(&driven, 8_192) > 2.0 * centroid(&clean, 8_192));
    }

    #[test]
    fn the_kick_bass_starts_and_takes_over_without_a_click() {
        let params = KickBassParams {
            click: 0.0,
            ..KickBassParams::default()
        };
        let alone = note(bass(params), on(36, 1.0), 0.3, 0.6);
        // A sine's steps are its slope; the drop starts the pitch, and so
        // the slope, higher by the sweep.
        let steady = largest_jump(between(&alone, 0.01, 0.3));
        let swept = steady * (params.sweep / 12.0).exp2();
        assert!(largest_jump(between(&alone, 0.0, 0.01)) <= swept);
        assert!(largest_jump(between(&alone, 0.3, 0.6)) <= steady);
        // A second note on the same voice carries on from the first.
        let at = (0.2 * RATE) as usize;
        let mut synth = synth_of(bass(params));
        let both = play(
            &mut synth,
            &[(0, on(36, 1.0)), (at, on(31, 1.0))],
            2 * at,
            128,
        );
        assert!(largest_jump(&both[at - 10..at + 200]) <= swept);
    }

    #[test]
    fn the_kick_bass_does_not_depend_on_the_block_size_and_stays_finite() {
        let script = [
            (0, on(33, 0.9)),
            (5_000, accented(36, 0.7)),
            (9_000, off(36)),
            (9_500, Act::Bend(2.0)),
            (12_000, on(28, 1.0)),
            (20_000, off(28)),
        ];
        let run = |block| {
            let mut synth = synth_of(bass(KickBassParams::default()));
            synth.set_legato(true);
            play(&mut synth, &script, 30_000, block)
        };
        let reference = run(256);
        for block in [1, 7, 64, 1_000] {
            assert!(run(block) == reference, "blocks of {block}");
        }
        let wild = KickBassParams {
            tune: f64::NAN,
            decay: f64::INFINITY,
            release: -1.0,
            sweep: 1e9,
            sweep_time: 0.0,
            drive: 1e9,
            click: f64::INFINITY,
            glide: f64::NAN,
            velocity_range_db: -4.0,
            accent_db: 1e9,
            level: 1e9,
        };
        let signal = play(
            &mut synth_of(bass(wild)),
            &[
                (0, accented(127, 1.0)),
                (10, accented(0, 1.0)),
                (100, Act::Bend(48.0)),
            ],
            4_000,
            64,
        );
        assert!(finite_within(&signal, 4.0 * 16.0 * 1.1));
    }
}
