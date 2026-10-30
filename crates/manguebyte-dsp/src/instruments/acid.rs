//! The acid bass: a monophonic bass synthesiser in the manner of the 303.

use core::f64::consts::{PI, TAU};

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    SILENCE, add_to_every_channel, db, flush, key_hz, setting, t60_factor, velocity_gain,
};
use crate::event::{Expression, NoteRef};
use crate::fir;
use crate::ladder::saturated;
use crate::tuning::Tuning;
use crate::{Oscillator, ProcessSpec, Voice, Waveform};

/// Where the diode ladder's feedback turns the phase by half a turn, as a
/// multiple of its stages' corner: `√(10/7)`. The resonance peaks there.
const PEAK_RATIO: f64 = 1.195_228_609_334_393_7;

/// The feedback at which the linear ladder oscillates: the size of the
/// ladder's denominator at the peak, `|ω⁴ - 15ω² + 1|` with `ω² = 10/7`.
const OSCILLATION_FEEDBACK: f64 = 18.387_755_102_040_817;

/// How far the resonant peak rises at full resonance, in decibels above
/// the passband. Without resonance the ladder is 25.3 dB down there.
const PEAK_DB: f64 = 24.0;

/// How many octaves the filter envelope opens the filter at full envelope
/// modulation, and the accent sweep at full accent.
const ENVELOPE_OCTAVES: f64 = 4.5;
const ACCENT_OCTAVES: f64 = 4.0;

/// How much further the envelope itself opens an accented note's filter
/// at full accent, as a share of its depth.
const ACCENT_ENVELOPE: f64 = 0.6;

/// The highest the envelope and the accent can open the filter, in hertz:
/// about where the original's runs out.
const HIGHEST_CUTOFF_HZ: f64 = 12_000.0;

/// How much resonance an accent adds at its height, at full accent.
const ACCENT_RESONANCE: f64 = 0.15;

/// How much louder an accented note is at full accent, in decibels.
const ACCENT_DB: f64 = 6.0;

/// The filter envelope's decay on an accented note, whatever the decay
/// setting: 60 dB down in 0.2 s.
const ACCENT_DECAY_SECONDS: f64 = 0.2;

/// The time constant with which the accent sweep follows the filter
/// envelope: long enough that it peaks some 60 ms into a note, after the
/// envelope, and that accents a sixteenth apart build on each other.
const ACCENT_SWEEP_SECONDS: f64 = 0.12;

/// The envelopes' attacks, from where they are.
const ATTACK_SECONDS: f64 = 0.003;

/// The amplifier's decay while the key is held, and its release.
const AMPLITUDE_DECAY_SECONDS: f64 = 4.0;
const RELEASE_SECONDS: f64 = 0.02;

/// How long a change of level, from velocity or accent, takes.
const GAIN_GLIDE_SECONDS: f64 = 0.003;

/// Taps of the half-band filter that brings the twice-oversampled signal
/// back down, and its stopband.
const DECIMATOR_TAPS: usize = 63;
const DECIMATOR_DB: f64 = 90.0;

/// The output's coupling high-pass, in hertz.
const OUTPUT_HIGH_PASS_HZ: f64 = 15.0;

/// The oscillator's shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum AcidWave {
    /// A sawtooth: the classic.
    #[default]
    Saw,
    /// A square: hollower.
    Square,
}

/// The settings of an [`AcidVoice`]. Out-of-range values are limited; a
/// value that is not a number takes the default of
/// [`classic`](Self::classic).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AcidParams {
    /// The oscillator's shape.
    pub wave: AcidWave,
    /// Tuning, in semitones, -24 to 24.
    pub tune: f64,
    /// Where the filter's resonance sits with its envelope closed, in
    /// hertz, 30 to 8 000.
    pub cutoff: f64,
    /// The filter's resonance, 0 to 1. The gain at the cut-off, against the
    /// bass, rises evenly in decibels from -25 dB with none to +24 dB at 1,
    /// short of oscillation; at full resonance the bass is 4 dB down.
    pub resonance: f64,
    /// How far the filter envelope opens the filter, 0 to 1: up to 4.5
    /// octaves.
    pub env_mod: f64,
    /// How long the filter envelope takes to fall 60 dB, 0.05 to 4
    /// seconds. Accented notes ignore it and fall in 0.2 s.
    pub decay: f64,
    /// How strong an accent is, 0 to 1: at 1 an accented note is 6 dB
    /// louder, its envelope opens the filter 60% further and the accent
    /// sweep up to 4 octaves more, the resonance rises, and accents close
    /// together build on each other.
    pub accent: f64,
    /// How long a slide takes, 0.005 to 1 seconds: it is 95% of the way to
    /// the new pitch in this time, whatever the interval.
    pub slide: f64,
    /// The gain into the filter's saturator, in decibels, -12 to 24.
    pub drive_db: f64,
    /// How much the level and the envelope modulation follow the
    /// velocity, 0 to 1. The original has none; at 1 the lowest velocity is
    /// 18 dB down and opens the filter half as far.
    pub velocity: f64,
    /// The output level, as a gain, 0 to 4.
    pub level: f64,
}

impl AcidParams {
    /// The classic acid line: a sawtooth through a resonant filter that
    /// the envelope opens and closes on every note.
    pub fn classic() -> Self {
        Self {
            wave: AcidWave::Saw,
            tune: 0.0,
            cutoff: 320.0,
            resonance: 0.72,
            env_mod: 0.6,
            decay: 0.5,
            accent: 0.7,
            slide: 0.06,
            drive_db: 6.0,
            velocity: 0.0,
            level: 0.28,
        }
    }

    /// Resonance and envelope near their tops, a short decay: the squelch.
    pub fn squelch() -> Self {
        Self {
            cutoff: 260.0,
            resonance: 0.9,
            env_mod: 0.8,
            decay: 0.3,
            accent: 0.9,
            drive_db: 9.0,
            level: 0.2,
            ..Self::classic()
        }
    }

    /// The square wave, darker and rounder.
    pub fn square() -> Self {
        Self {
            wave: AcidWave::Square,
            cutoff: 420.0,
            resonance: 0.6,
            env_mod: 0.5,
            decay: 0.7,
            level: 0.22,
            ..Self::classic()
        }
    }

    fn limited(self) -> Self {
        let default = Self::classic();
        Self {
            wave: self.wave,
            tune: setting(self.tune, -24.0, 24.0, default.tune),
            cutoff: setting(self.cutoff, 30.0, 8_000.0, default.cutoff),
            resonance: setting(self.resonance, 0.0, 1.0, default.resonance),
            env_mod: setting(self.env_mod, 0.0, 1.0, default.env_mod),
            decay: setting(self.decay, 0.05, 4.0, default.decay),
            accent: setting(self.accent, 0.0, 1.0, default.accent),
            slide: setting(self.slide, 0.005, 1.0, default.slide),
            drive_db: setting(self.drive_db, -12.0, 24.0, default.drive_db),
            velocity: setting(self.velocity, 0.0, 1.0, 0.0),
            level: setting(self.level, 0.0, 4.0, default.level),
        }
    }
}

impl Default for AcidParams {
    fn default() -> Self {
        Self::classic()
    }
}

/// How much of the bass the feedback takes away is given back after the
/// filter: the output is multiplied by `1 + MAKE_UP·k`.
const MAKE_UP: f64 = 0.6;

/// The feedback for a resonance from 0 to 1 (limited there).
///
/// With feedback `k` the ladder's gain is `1 / (1 + k)` at 0 Hz and
/// `1 / (18.39 - k)` at the peak, so the peak stands `(1 + k) / (18.39 -
/// k)` above the bass. The resonance sets that height in decibels, evenly
/// from where the ladder alone is (-25.3 dB) to [`PEAK_DB`], and this
/// solves for `k`.
fn feedback(resonance: f64) -> f64 {
    let floor = -20.0 * OSCILLATION_FEEDBACK.log10();
    let height = db(floor + resonance.clamp(0.0, 1.0) * (PEAK_DB - floor));
    ((OSCILLATION_FEEDBACK * height - 1.0) / (1.0 + height)).max(0.0)
}

/// A four-pole diode ladder, the filter of the classic bass synthesiser.
///
/// The ladder's four capacitors are each loaded by the next, so its stages
/// are not buffered as a transistor ladder's are: linearised, with every
/// stage's corner at `ωc`, it is
///
/// ```text
/// y1' = ωc (u - 2 y1 + y2)      y3' = ωc (y2 - 2 y3 + y4)
/// y2' = ωc (y1 - 2 y2 + y3)     y4' = ωc (y3 - y4)
/// ```
///
/// whose response is `1 / (s⁴ + 7s³ + 15s² + 10s + 1)` with `s` in units
/// of `ωc`. Its poles are spread from 0.12 to 3.5 times the corner, which
/// is why it rolls off more gently than the transistor ladder near the
/// cut-off and why it takes a feedback of 18.4, not 4, to oscillate. The
/// output of the fourth stage goes back to the input stage, a `tanh`, and
/// as in any ladder that feedback lowers the bass as it raises the peak;
/// the voice gives most of the bass back after the filter.
///
/// Each integrator is trapezoidal; the four-by-four system they make is
/// tridiagonal and is eliminated in closed form for each sample, and the
/// loop through the saturator is solved by the transistor ladder's own
/// Newton method ([`saturated`]), with no delay in it (V. Zavalishin, "The
/// Art of VA Filter Design", chapters 3 to 6, whose method this is).
#[derive(Debug, Clone, Copy, Default)]
struct DiodeLadder {
    state: [f64; 4],
}

impl DiodeLadder {
    /// Filters one sample. `g` is `tan(π fc / fs)` of the stages' corner,
    /// `k` the feedback and `drive` the gain into the saturator.
    #[inline]
    fn process(&mut self, x: f64, g: f64, k: f64, drive: f64) -> f64 {
        let [s1, s2, s3, s4] = self.state;
        // Forward elimination, keeping the input u of the first stage as
        // an unknown: y_n = a_n y_(n+1) + b_n u + c_n.
        let d1 = 1.0 + 2.0 * g;
        let (a1, b1, c1) = (g / d1, g / d1, s1 / d1);
        let d2 = 1.0 + 2.0 * g - g * a1;
        let (a2, b2, c2) = (g / d2, g * b1 / d2, (s2 + g * c1) / d2);
        let d3 = 1.0 + 2.0 * g - g * a2;
        let (a3, b3, c3) = (g / d3, g * b2 / d3, (s3 + g * c2) / d3);
        let d4 = 1.0 + g - g * a3;
        let (gain, rest) = (g * b3 / d4, (s4 + g * c3) / d4);
        // The fourth stage puts out gain·u + rest, and
        // u = tanh(drive·x - k·(gain·u + rest)).
        let u = saturated(drive * x - k * rest, k * gain);
        let y4 = gain * u + rest;
        let y3 = a3 * y4 + b3 * u + c3;
        let y2 = a2 * y3 + b2 * u + c2;
        let y1 = a1 * y2 + b1 * u + c1;
        for (memory, y) in self.state.iter_mut().zip([y1, y2, y3, y4]) {
            *memory = flush(2.0 * y - *memory);
        }
        y4
    }
}

/// An acid bass: a voice for [`Synth`](crate::Synth) in the manner of the
/// classic monophonic bass-line synthesiser, the 303.
///
/// A band-limited sawtooth or square ([`Oscillator`]) goes through a
/// four-pole diode ladder with a saturating input (see the type's source
/// for the model) and an amplifier. Both run at twice the sample rate and
/// come back down through a 63-tap half-band filter, so the harmonics the
/// saturator and the resonance make do not fold back; the voice is 16
/// samples late for it.
///
/// - **Filter envelope.** Every note opens the filter by
///   [`env_mod`](AcidParams::env_mod) and lets it close over
///   [`decay`](AcidParams::decay). [`cutoff`](AcidParams::cutoff) is where
///   the resonance peaks with the envelope closed. As on the original the
///   filter does not follow the keyboard.
/// - **Accent** ([`Synth::note_on_accented`](crate::Synth::note_on_accented)):
///   louder by up to 6 dB, a deeper envelope with a short decay whatever
///   the setting, and the accent sweep: the envelope, smoothed with a time
///   constant of 120 ms, opens the filter further and raises the
///   resonance, and because the smoothing remembers, accents on
///   consecutive notes push the filter higher each time. That memory
///   outlives the note: the voice stays active until it has gone.
/// - **Slide**: on a synth set to [legato](crate::Synth::set_legato), a
///   note struck while another is held glides to its pitch over
///   [`slide`](AcidParams::slide), whatever the interval, and neither
///   envelope starts again. This is the bass line's slide.
/// - Notes that do not slide restart both envelopes from where they are.
///   The oscillator and the filter run on from note to note, as the
///   circuits do, so a new note, on a voice taken over from another too,
///   continues the sound instead of cutting it; a voice that is silent
///   when it is choked starts its next note as new.
///
/// The output goes through a high-pass at 15 Hz, as the original's
/// coupling capacitor. Pitch bend and [`Expression::Tuning`] bend the
/// oscillator.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{AcidParams, AcidVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// let mut acid = Synth::with_voices(1, || AcidVoice::new(AcidParams::classic()));
/// acid.set_legato(true);
/// acid.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 128, num_channels: 1 });
/// acid.note_on_accented(NoteRef::new(0, 36), 1.0, true);
/// acid.note_on(NoteRef::new(0, 48), 1.0); // the first is still held: slides up an octave
/// let mut block = AudioBuffer::<f32>::new(1, 128);
/// acid.process(&mut block);
/// assert!(block.peak().is_finite());
/// ```
#[derive(Debug, Clone)]
pub struct AcidVoice {
    params: AcidParams,
    sample_rate: f64,
    oscillator: Oscillator,
    filter: DiodeLadder,
    /// The pitch now and where it slides to, as key numbers, and how much
    /// of the way it goes in a sample.
    pitch: f64,
    target: f64,
    slide: f64,
    /// The filter envelope, its attack and its fall per sample.
    envelope: f64,
    envelope_rising: bool,
    envelope_fall: f64,
    attack_step: f64,
    /// The accent sweep, whether the note is accented, and how much of the
    /// way the sweep follows the envelope in a sample.
    sweep: f64,
    accented: bool,
    sweep_follow: f64,
    /// The amplifier: level, whether it is rising, released.
    amplitude: f64,
    amplitude_rising: bool,
    released: bool,
    amplitude_fall: f64,
    release_fall: f64,
    /// The level a note asks for, and the level heard, gliding to it.
    gain: f64,
    gain_now: f64,
    gain_follow: f64,
    /// How far the velocity lets the envelope open the filter.
    envelope_depth: f64,
    /// The half-band filter and the oversampled signal it reads.
    taps: Vec<f64>,
    history: Vec<f64>,
    written: usize,
    /// The output's high-pass: its last input and output, and its pole.
    coupling: [f64; 2],
    coupling_pole: f64,
    carried: bool,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
}

impl AcidVoice {
    /// A voice with these settings. Until it is prepared it is silent.
    pub fn new(params: AcidParams) -> Self {
        let params = params.limited();
        let mut voice = Self {
            params,
            sample_rate: 48_000.0,
            oscillator: Oscillator::new(Waveform::Saw, 110.0),
            filter: DiodeLadder::default(),
            pitch: 36.0,
            target: 36.0,
            slide: 1.0,
            envelope: 0.0,
            envelope_rising: false,
            envelope_fall: 0.0,
            attack_step: 1.0,
            sweep: 0.0,
            accented: false,
            sweep_follow: 1.0,
            amplitude: 0.0,
            amplitude_rising: false,
            released: false,
            amplitude_fall: 0.0,
            release_fall: 0.0,
            gain: 0.0,
            gain_now: 0.0,
            gain_follow: 1.0,
            envelope_depth: 1.0,
            taps: Vec::new(),
            history: Vec::new(),
            written: 0,
            coupling: [0.0; 2],
            coupling_pole: 0.0,
            carried: false,
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
        };
        voice.configure();
        voice
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note takes them at once.
    pub fn set_params(&mut self, params: AcidParams) {
        self.params = params.limited();
        self.configure();
    }

    /// The settings, as limited.
    pub fn params(&self) -> &AcidParams {
        &self.params
    }

    fn configure(&mut self) {
        let rate = self.sample_rate;
        let params = self.params;
        self.oscillator.set_waveform(match params.wave {
            AcidWave::Saw => Waveform::Saw,
            AcidWave::Square => Waveform::Square,
        });
        self.oscillator.set_sample_rate(2.0 * rate);
        // 95% of the way in the slide time: e^-3 is 5%.
        self.slide = 1.0 - (-3.0 / (params.slide * rate)).exp();
        self.attack_step = 1.0 / (ATTACK_SECONDS * rate).max(1.0);
        self.envelope_fall = t60_factor(self.envelope_decay(), rate);
        self.sweep_follow = 1.0 - (-1.0 / (ACCENT_SWEEP_SECONDS * rate)).exp();
        self.amplitude_fall = t60_factor(AMPLITUDE_DECAY_SECONDS, rate);
        self.release_fall = t60_factor(RELEASE_SECONDS, rate);
        self.gain_follow = 1.0 - (-3.0 / (GAIN_GLIDE_SECONDS * rate)).exp();
        self.coupling_pole = (-TAU * OUTPUT_HIGH_PASS_HZ / rate).exp();
    }

    fn envelope_decay(&self) -> f64 {
        if self.accented && self.params.accent > 0.0 {
            ACCENT_DECAY_SECONDS
        } else {
            self.params.decay
        }
    }

    fn sounding(&self) -> bool {
        self.amplitude_rising || self.amplitude > 0.0 || self.sweep > SILENCE
    }

    /// Back to the state of a new voice, but for the settings.
    fn reset(&mut self) {
        self.oscillator.reset_phase();
        self.filter = DiodeLadder::default();
        self.envelope = 0.0;
        self.envelope_rising = false;
        self.sweep = 0.0;
        self.accented = false;
        self.amplitude = 0.0;
        self.amplitude_rising = false;
        self.released = false;
        self.gain = 0.0;
        self.gain_now = 0.0;
        self.history.fill(0.0);
        self.written = 0;
        self.coupling = [0.0; 2];
        self.carried = false;
    }

    /// Takes the velocity and accent of a note: its level, the depth of
    /// its envelope, and how its filter envelope falls.
    fn take(&mut self, velocity: f32, accent: bool) {
        let params = self.params;
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        self.accented = accent;
        let mut gain = velocity_gain(velocity, 18.0 * params.velocity);
        if accent {
            gain *= db(ACCENT_DB * params.accent);
        }
        self.gain = gain;
        self.envelope_depth = 1.0 - 0.5 * params.velocity * (1.0 - velocity);
        self.envelope_fall = t60_factor(self.envelope_decay(), self.sample_rate);
    }

    /// One sample at the oversampled rate, into the decimator.
    #[inline]
    fn oversampled(&mut self, g: f64, k: f64, drive: f64) {
        let source = self.oscillator.next_sample();
        let filtered = self.filter.process(source, g, k, drive);
        let mask = self.history.len().wrapping_sub(1);
        if let Some(slot) = self.history.get_mut(self.written & mask) {
            *slot = filtered;
        }
        self.written = self.written.wrapping_add(1);
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let rate = self.sample_rate;
        let params = self.params;

        // Pitch.
        self.pitch += (self.target - self.pitch) * self.slide;
        let key = self.pitch + params.tune + self.bend + self.tuning;
        self.oscillator.set_frequency(key_hz(key));

        // Envelopes.
        if self.envelope_rising {
            self.envelope += self.attack_step;
            if self.envelope >= 1.0 {
                self.envelope = 1.0;
                self.envelope_rising = false;
            }
        } else {
            self.envelope = flush(self.envelope * self.envelope_fall);
        }
        let swept = if self.accented { self.envelope } else { 0.0 };
        self.sweep = flush(self.sweep + (swept - self.sweep) * self.sweep_follow);
        if self.amplitude_rising {
            self.amplitude += self.attack_step;
            if self.amplitude >= 1.0 {
                self.amplitude = 1.0;
                self.amplitude_rising = false;
            }
        } else {
            let fall = if self.released {
                self.release_fall
            } else {
                self.amplitude_fall
            };
            self.amplitude *= fall;
            if self.amplitude < SILENCE {
                self.amplitude = 0.0;
            }
        }
        self.gain_now += (self.gain - self.gain_now) * self.gain_follow;

        // The filter's cut-off and resonance for this sample.
        let depth = if self.accented {
            self.envelope_depth * (1.0 + ACCENT_ENVELOPE * params.accent)
        } else {
            self.envelope_depth
        };
        let octaves = params.env_mod * ENVELOPE_OCTAVES * self.envelope * depth
            + params.accent * ACCENT_OCTAVES * self.sweep;
        let cutoff =
            (params.cutoff * octaves.exp2()).clamp(20.0, HIGHEST_CUTOFF_HZ.min(0.4 * rate));
        let g = (PI * cutoff / (2.0 * rate)).tan() / PEAK_RATIO;
        let resonance = params.resonance + ACCENT_RESONANCE * params.accent * self.sweep;
        let k = feedback(resonance);
        let drive = db(params.drive_db);
        self.oversampled(g, k, drive);
        self.oversampled(g, k, drive);

        // Back down to the sample rate.
        let len = self.history.len();
        let mut filtered = 0.0;
        if len > 0 {
            let newest = self.written.wrapping_sub(1);
            for (offset, tap) in self.taps.iter().enumerate() {
                filtered += tap * self.history[newest.wrapping_sub(offset) & (len - 1)];
            }
        }
        // A saturator's level is about one; the drive is taken back out
        // above it so that the level setting means the same at any drive.
        let make_up = 1.0 + MAKE_UP * k;
        let voiced = make_up * filtered * self.amplitude * self.gain_now / drive.max(1.0).sqrt();
        let [last_in, last_out] = self.coupling;
        let out = flush(voiced - last_in + self.coupling_pole * last_out);
        self.coupling = [voiced, out];
        out * params.level
    }
}

impl<S: Sample> Voice<S> for AcidVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.taps = fir::lowpass(DECIMATOR_TAPS, 0.25, DECIMATOR_DB);
        self.history = vec![0.0; DECIMATOR_TAPS.next_power_of_two()];
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
        self.carried = false;
        self.pitch = key;
        self.target = key;
        self.take(velocity, accent);
        if !self.sounding() {
            self.gain_now = self.gain;
        }
        self.envelope_rising = true;
        self.amplitude_rising = true;
        self.released = false;
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.sounding() {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        self.target = self.key_tuning.key_pitch(note.key.min(127));
        // The gate stays open: the envelopes carry on, and only the level
        // and the accent change.
        self.take(velocity, accent);
        self.released = false;
    }

    fn note_off(&mut self, _velocity: f32) {
        self.released = true;
    }

    /// A voice still sounding is being taken over: the oscillator and the
    /// filter carry on, so the next note continues from them. Choked again,
    /// or choked when silent, the voice goes back to the state it had when
    /// new.
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
    use crate::fft::Complex;
    use crate::instruments::measure::{
        Act, RATE, accented, amplitude_at, between, centroid, cents, decibels, finite_within,
        frequency_near, largest_jump, note, off, on, peak, play, rms, synth_of,
    };
    use crate::testing::gain_at;

    fn voice(params: AcidParams) -> [AcidVoice; 1] {
        [AcidVoice::new(params)]
    }

    /// The ladder's response in theory, at `frequency` for the sample rate
    /// `rate`, through the bilinear map its integrators make:
    /// `1 / (D(s) + k)`.
    fn theory(g: f64, k: f64, frequency: f64, rate: f64) -> f64 {
        let w = (PI * frequency / rate).tan();
        let s = Complex::new(0.0, w / g);
        let s2 = s * s;
        let d = s2 * s2 + (s2 * s).scale(7.0) + s2.scale(15.0) + s.scale(10.0);
        1.0 / (d + Complex::new(1.0 + k, 0.0)).norm()
    }

    #[test]
    fn the_quiet_ladder_has_the_response_of_its_equations() {
        let rate = 96_000.0;
        let mut worst = 0.0f64;
        for cutoff in [200.0, 1_000.0, 5_000.0] {
            for resonance in [0.0, 0.5, 0.8] {
                let g = (PI * cutoff / rate).tan() / PEAK_RATIO;
                let k = feedback(resonance);
                let mut ladder = DiodeLadder::default();
                let response: Vec<f64> = (0..1 << 17)
                    .map(|n| ladder.process(if n == 0 { 1e-6 } else { 0.0 }, g, k, 1.0) / 1e-6)
                    .collect();
                let tail = &response[response.len() - 64..];
                assert!(tail.iter().all(|x| x.abs() < 1e-12), "{cutoff} {resonance}");
                for frequency in [20.0, 0.5 * cutoff, cutoff, 2.0 * cutoff, 15_000.0] {
                    let measured = gain_at(&response, frequency, rate);
                    let expected = theory(g, k, frequency, rate);
                    // Far down the slope the gain is 160 dB down, and the
                    // sum that measures it is only good to its rounding.
                    let error = (measured - expected).abs() / (expected + 1e-6);
                    worst = worst.max(error);
                    assert!(
                        error < 1e-6,
                        "{cutoff} {resonance} {frequency}: {measured} {expected}"
                    );
                }
                // At the cut-off, where the loop turns the phase by half a
                // turn, the gain stands as far above the bass as the
                // resonance says. (A gentle resonance's bump has its top a
                // little lower; a strong one's is at the cut-off.)
                let height = decibels(theory(g, k, cutoff, rate) / theory(g, k, 0.0, rate));
                let expected = -25.29 + resonance * (PEAK_DB + 25.29);
                assert!(
                    (height - expected).abs() < 0.05,
                    "{cutoff} {resonance}: {height}"
                );
            }
        }
        println!("worst {worst:e}");
        // With the voice's make-up gain the bass is 4 dB down at most.
        let g = (PI * 500.0 / rate).tan() / PEAK_RATIO;
        let k = feedback(1.0);
        let bass = decibels((1.0 + MAKE_UP * k) * theory(g, k, 0.0, rate));
        assert!((-4.5..-3.5).contains(&bass), "{bass}");
    }

    #[test]
    fn every_note_is_in_tune() {
        let mut worst = 0.0f64;
        for params in [AcidParams::classic(), AcidParams::square()] {
            for key in [24u8, 33, 43, 55, 72] {
                // Once the envelope has closed the filter: a filter that
                // moves shifts the phase of what it passes, and so, while
                // it moves, the pitch heard.
                let signal = note(voice(params), on(key, 1.0), 1.4, 1.4);
                let expected = key_hz(f64::from(key));
                let measured = frequency_near(between(&signal, 0.8, 1.4), expected);
                worst = worst.max(cents(measured, expected).abs());
            }
        }
        println!("worst {worst:.4} cents");
        assert!(worst < 1.0);
    }

    #[test]
    fn the_envelope_closes_the_filter_in_its_decay() {
        // The envelope itself falls 60 dB in its decay time.
        for decay in [0.2, 0.5, 1.5] {
            let mut voice = AcidVoice::new(AcidParams {
                decay,
                ..AcidParams::classic()
            });
            Voice::<f64>::prepare(
                &mut voice,
                &ProcessSpec {
                    sample_rate: RATE,
                    max_block_size: 64,
                    num_channels: 1,
                },
            );
            Voice::<f64>::note_on(&mut voice, NoteRef::new(0, 36), 1.0);
            let attack = (ATTACK_SECONDS * RATE) as usize;
            for _ in 0..attack + (decay * RATE) as usize {
                voice.next();
            }
            assert!(
                (decibels(voice.envelope) + 60.0).abs() < 0.1,
                "{decay}: {}",
                voice.envelope
            );
        }
        // And the brightness follows: the filter closes as it falls, more
        // slowly with a longer decay.
        let brightness = |decay: f64, t: f64| {
            let params = AcidParams {
                decay,
                ..AcidParams::classic()
            };
            let signal = note(voice(params), on(36, 1.0), 1.0, 1.0);
            centroid(between(&signal, t, t + 0.1), 4_096)
        };
        let (short, long) = (brightness(0.3, 0.25), brightness(0.6, 0.25));
        println!(
            "centroid after 0.25 s: {short:.0} Hz with a 0.3 s decay, {long:.0} Hz with 0.6 s"
        );
        assert!(short < long && long < brightness(0.6, 0.0));
        // Released, the note is gone within its release.
        let signal = note(voice(AcidParams::classic()), on(36, 1.0), 0.3, 0.5);
        assert!(peak(between(&signal, 0.3 + 3.0 * RELEASE_SECONDS, 0.5)) < 1e-3 * peak(&signal));
    }

    #[test]
    fn an_accent_is_louder_brighter_and_shorter() {
        let params = AcidParams::classic();
        let plain = note(voice(params), on(36, 1.0), 0.4, 0.4);
        let strong = note(voice(params), accented(36, 1.0), 0.4, 0.4);
        let louder = decibels(rms(between(&strong, 0.0, 0.1)) / rms(between(&plain, 0.0, 0.1)));
        // Brighter over its attack, where the deeper envelope opens it.
        let brighter = centroid(&strong, 2_048) / centroid(&plain, 2_048);
        println!("accent: {louder:.1} dB, {brighter:.2} times the centroid");
        assert!(louder > 3.0, "{louder}");
        assert!(brighter > 1.1, "{brighter}");
        // Its envelope falls in 0.2 s whatever the decay is set to.
        let fallen = |accent: bool| {
            let mut voice = AcidVoice::new(params);
            Voice::<f64>::prepare(
                &mut voice,
                &ProcessSpec {
                    sample_rate: RATE,
                    max_block_size: 64,
                    num_channels: 1,
                },
            );
            Voice::<f64>::note_on_accented(&mut voice, NoteRef::new(0, 36), 1.0, accent);
            let attack = (ATTACK_SECONDS * RATE) as usize;
            for _ in 0..attack + (ACCENT_DECAY_SECONDS * RATE) as usize {
                voice.next();
            }
            decibels(voice.envelope)
        };
        assert!((fallen(true) + 60.0).abs() < 0.1);
        assert!(fallen(false) > -60.0 * ACCENT_DECAY_SECONDS / params.decay - 0.1);

        // Accents in a row climb: the third of three close accents is
        // brighter than the first.
        let step = (0.12 * RATE) as usize;
        let mut synth = synth_of(voice(params));
        let script = [
            (0, accented(36, 1.0)),
            (step / 2, off(36)),
            (step, accented(36, 1.0)),
            (3 * step / 2, off(36)),
            (2 * step, accented(36, 1.0)),
        ];
        let signal = play(&mut synth, &script, 3 * step, 128);
        let first = centroid(&signal[..2_048], 2_048);
        let third = centroid(&signal[2 * step..2 * step + 2_048], 2_048);
        println!("third accent {third:.0} Hz against the first's {first:.0} Hz");
        assert!(third > 1.05 * first);
    }

    #[test]
    fn overlapping_notes_slide_without_striking_again() {
        // A slow slide, to see it on its way.
        let params = AcidParams {
            slide: 0.2,
            ..AcidParams::classic()
        };
        let mut synth: Synth<AcidVoice, f64> = synth_of(voice(params));
        synth.set_legato(true);
        let at = (0.3 * RATE) as usize;
        let script = [(0, on(36, 1.0)), (at, on(43, 1.0)), (at + 100, off(36))];
        let signal = play(&mut synth, &script, 3 * at, 128);
        // Where the slide starts the level does not rise again, as it does
        // when the note is struck.
        let rise =
            |signal: &[f64]| decibels(rms(&signal[at..at + 2_400]) / rms(&signal[at - 2_400..at]));
        let mut synth: Synth<AcidVoice, f64> = synth_of(voice(params));
        let struck = play(&mut synth, &script, 3 * at, 128);
        println!(
            "level after the slide {:.1} dB, after a strike {:.1} dB",
            rise(&signal),
            rise(&struck)
        );
        assert!(rise(&signal) < 0.5 && rise(&struck) > rise(&signal) + 3.0);
        // From 20 to 60 ms into the slide it is on its way, and twice the
        // slide time on at the new note.
        let middle = frequency_near(&signal[at + 960..at + 2_880], key_hz(39.0));
        assert!(middle > key_hz(37.0) && middle < key_hz(41.0), "{middle}");
        let end = at + (2.0 * params.slide * RATE) as usize;
        let measured = frequency_near(&signal[end..], key_hz(43.0));
        assert!(cents(measured, key_hz(43.0)).abs() < 1.0, "{measured}");

        // Struck, the filter opens again; slid, it does not.
        let bright = |signal: &[f64]| centroid(&signal[at + 480..at + 4_576], 4_096);
        assert!(bright(&struck) > 1.2 * bright(&signal));
    }

    #[test]
    fn notes_start_end_and_follow_each_other_without_a_click() {
        let params = AcidParams::classic();
        let alone = note(voice(params), on(36, 1.0), 0.3, 0.5);
        // The attack, against the brightest moments just after it.
        let attack = largest_jump(between(&alone, 0.0, ATTACK_SECONDS));
        let after = largest_jump(between(&alone, ATTACK_SECONDS, 0.02));
        assert!(attack <= after, "{attack} against {after}");
        // The release, against the end of the held note.
        let steady = largest_jump(between(&alone, 0.2, 0.3));
        assert!(largest_jump(between(&alone, 0.3, 0.5)) <= steady);
        // A new note on the one voice, without legato: no jump either.
        let at = (0.2 * RATE) as usize;
        let mut synth = synth_of(voice(params));
        let both = play(
            &mut synth,
            &[(0, on(36, 1.0)), (at, on(31, 1.0))],
            2 * at,
            128,
        );
        let next = largest_jump(&both[at..at + 4_800]);
        assert!(largest_jump(&both[at - 10..at + 200]) <= next.max(steady));
    }

    #[test]
    fn velocity_is_ignored_by_default_and_followed_when_asked() {
        let params = AcidParams::classic();
        let a = note(voice(params), on(36, 1.0), 0.3, 0.3);
        let b = note(voice(params), on(36, 0.3), 0.3, 0.3);
        assert_eq!(a, b);
        let sensitive = AcidParams {
            velocity: 1.0,
            ..params
        };
        let loud = note(voice(sensitive), on(36, 1.0), 0.3, 0.3);
        let soft = note(voice(sensitive), on(36, 0.5), 0.3, 0.3);
        let difference = decibels(rms(&loud) / rms(&soft));
        let brightness = centroid(&loud, 8_192) / centroid(&soft, 8_192);
        println!("velocity: {difference:.1} dB, {brightness:.2} times the centroid");
        assert!(difference > 6.0 && brightness > 1.05);
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size_and_stays_finite() {
        let script = [
            (0, accented(36, 0.9)),
            (3_000, on(48, 0.7)),
            (5_000, off(36)),
            (6_000, off(48)),
            (9_000, accented(31, 1.0)),
            (9_500, Act::Bend(1.0)),
            (12_000, on(43, 1.0)),
            (20_000, off(43)),
        ];
        for params in [
            AcidParams::classic(),
            AcidParams::squelch(),
            AcidParams::square(),
        ] {
            let run = |block| {
                let mut synth = synth_of(voice(params));
                synth.set_legato(true);
                play(&mut synth, &script, 30_000, block)
            };
            let reference = run(256);
            for block in [1, 7, 64, 1_000] {
                assert!(run(block) == reference, "blocks of {block}");
            }
        }
        let wild = AcidParams {
            wave: AcidWave::Square,
            tune: f64::NAN,
            cutoff: f64::INFINITY,
            resonance: 1e9,
            env_mod: 1e9,
            decay: -1.0,
            accent: f64::INFINITY,
            slide: 0.0,
            drive_db: 1e9,
            velocity: f64::NAN,
            level: 1e9,
        };
        let signal = play(
            &mut synth_of(voice(wild)),
            &[
                (0, accented(127, 1.0)),
                (10, accented(0, 1.0)),
                (100, Act::Bend(48.0)),
            ],
            8_000,
            64,
        );
        assert!(finite_within(&signal, 4.0 * 4.0 * 2.0));
        assert!(amplitude_at(&signal, 1_000.0).is_finite());
    }
}
