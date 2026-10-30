//! The poly synth voice: oscillators, filters, modulation and the
//! amplifier, run at twice the sample rate.

use core::f64::consts::{PI, TAU};

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_midi::MidiMessage;

use super::super::common::{HANDOVER_SECONDS, Noise, SoftClip, db, flush, key_hz, velocity_gain};
use super::filter::{Coefficients, FilterSlot};
use super::modulation::{Envelope, Floor, Lfo};
use super::osc::{Drawing, Osc, Shape, pan_gains};
use super::params::{
    ENVELOPES, FILTERS, FilterModel, FilterRouting, LFOS, MAX_UNISON, MOD_SLOTS, ModDestination,
    ModSource, OSCILLATORS, PolySynthParams, SubWave, VoiceMode, Wave,
};
use super::tables::{Table, bank};
use crate::event::{Expression, NoteRef};
use crate::numeric::{limit, step_towards};
use crate::tuning::Tuning;
use crate::{NoiseColor, ProcessSpec, Synth, Voice, fir};

/// How many times the sample rate the voice runs at.
const OVERSAMPLING: usize = 2;

/// The time constant with which the voice's continuous settings follow
/// what they are set to, so that a jump does not click.
const SMOOTHING_SECONDS: f64 = 0.005;

/// How long a change of an oscillator's shape, or of a filter's model,
/// takes to blend from the old one to the new.
const SHAPE_BLEND_SECONDS: f64 = 0.005;
const MODEL_BLEND_SECONDS: f64 = 0.01;

/// The time constant of the gains of unison voices coming and going.
const UNISON_SECONDS: f64 = 0.004;

/// The time constant of a change of level within a note, from a slurred
/// note's velocity or accent.
const GAIN_SECONDS: f64 = 0.003;

/// The shortest times the amplifier's envelope takes, so that no note
/// starts or ends with a click: 2 ms to rise or fall to the sustain, 5 ms
/// to fall silent, with curves straightened below 20 ms.
const AMP_FLOOR: Floor = Floor {
    rise: 0.002,
    release: 0.005,
    tame: 0.02,
};

/// The gain of the oscillators' mix into the filters, which leaves the
/// saturation of the ladder and the MS-20-like filter some room. It is
/// given back after them.
const MIX_GAIN: f64 = 0.5;

/// The corner of the high-pass that keeps the voice free of a DC offset.
const DC_BLOCK_HZ: f64 = 8.0;

/// The half-band filter that brings the voice down from twice the sample
/// rate: taps and stopband. It is flat to 0.418 of the output rate (20.1
/// kHz at 48 kHz), and with 100 dB of stopband from 0.58 of it.
const DECIMATOR_TAPS: usize = 79;
const DECIMATOR_DB: f64 = 100.0;
const DECIMATOR_PAIRS: usize = (DECIMATOR_TAPS + 1) / 4;
const DECIMATOR_HISTORY: usize = 128;

/// How many output samples the decimator takes to put out the last of a
/// note once its tones are silent: after them it holds nothing but zeros.
const DECIMATOR_TAIL: u32 = DECIMATOR_TAPS.div_ceil(OVERSAMPLING) as u32;

/// How many samples late, at the output rate, the voice is: two samples
/// of the oscillators' band-limiting and the decimator's 39, at twice the
/// rate.
pub const LATENCY_SAMPLES: f64 = (2 + (DECIMATOR_TAPS - 1) / 2) as f64 / OVERSAMPLING as f64;

/// Where each continuous setting a tone follows sits in its list.
mod knob {
    pub(super) const PER_OSC: usize = 8;
    pub(super) const LEVEL: usize = 0;
    pub(super) const PITCH: usize = 1;
    pub(super) const SHAPE: usize = 2;
    pub(super) const FM: usize = 3;
    pub(super) const RING: usize = 4;
    pub(super) const DETUNE: usize = 5;
    pub(super) const SPREAD: usize = 6;
    pub(super) const PAN: usize = 7;
    pub(super) const SUB: usize = 3 * PER_OSC;
    pub(super) const NOISE: usize = SUB + 1;
    pub(super) const FILTER: usize = NOISE + 1;
    pub(super) const PER_FILTER: usize = 6;
    pub(super) const CUTOFF: usize = 0;
    pub(super) const RESONANCE: usize = 1;
    pub(super) const DRIVE: usize = 2;
    pub(super) const TRACK: usize = 3;
    pub(super) const ENV: usize = 4;
    pub(super) const MORPH: usize = 5;
    pub(super) const SATURATION: usize = FILTER + 2 * PER_FILTER;
    pub(super) const PAN_ALL: usize = SATURATION + 1;
    pub(super) const OUTPUT: usize = PAN_ALL + 1;
    pub(super) const AMOUNTS: usize = OUTPUT + 1;
    pub(super) const COUNT: usize = AMOUNTS + super::MOD_SLOTS;
}

/// The values the continuous settings are heading for.
fn targets(params: &PolySynthParams) -> [f64; knob::COUNT] {
    let mut out = [0.0; knob::COUNT];
    for (index, osc) in params.oscillators.iter().enumerate() {
        let base = index * knob::PER_OSC;
        out[base + knob::LEVEL] = osc.level;
        out[base + knob::PITCH] = osc.semitones + osc.cents / 100.0;
        out[base + knob::SHAPE] = osc.shape;
        out[base + knob::FM] = osc.fm;
        out[base + knob::RING] = osc.ring;
        out[base + knob::DETUNE] = osc.detune;
        out[base + knob::SPREAD] = osc.spread;
        out[base + knob::PAN] = osc.pan;
    }
    out[knob::SUB] = params.sub.level;
    out[knob::NOISE] = params.noise.level;
    for (index, filter) in params.filters.iter().enumerate() {
        let base = knob::FILTER + index * knob::PER_FILTER;
        out[base + knob::CUTOFF] = filter.cutoff.log2();
        out[base + knob::RESONANCE] = filter.resonance;
        out[base + knob::DRIVE] = filter.drive;
        out[base + knob::TRACK] = filter.key_track;
        out[base + knob::ENV] = filter.env_amount;
        out[base + knob::MORPH] = filter.morph;
    }
    out[knob::SATURATION] = params.saturation;
    out[knob::PAN_ALL] = params.pan;
    out[knob::OUTPUT] = params.level;
    for (index, slot) in params.matrix.iter().enumerate() {
        out[knob::AMOUNTS + index] = slot.amount;
    }
    out
}

/// What the settings ask of every sample, worked out once when they are
/// set.
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// The matrix slots in use, and how many.
    slots: [usize; MOD_SLOTS],
    active: usize,
    /// Which sources the matrix or the filters read.
    reads: [bool; ModSource::ALL.len()],
    /// How each oscillator and the sub oscillator draw.
    shapes: [Shape; OSCILLATORS + 1],
}

impl Plan {
    fn new(params: &PolySynthParams) -> Self {
        let mut plan = Self {
            slots: [0; MOD_SLOTS],
            active: 0,
            reads: [false; ModSource::ALL.len()],
            shapes: [Shape::Saw; OSCILLATORS + 1],
        };
        for (index, slot) in params.matrix.iter().enumerate() {
            if slot.is_active() {
                plan.slots[plan.active] = index;
                plan.active += 1;
                plan.reads[slot.source.index()] = true;
                plan.reads[slot.via.index()] = true;
            }
        }
        if params.filters.iter().any(|filter| filter.env_amount != 0.0) {
            plan.reads[ModSource::Env2.index()] = true;
        }
        for (shape, osc) in plan.shapes.iter_mut().zip(&params.oscillators) {
            *shape = match osc.wave {
                Wave::Sine => Shape::Sine,
                Wave::Triangle => Shape::Triangle,
                Wave::Saw => Shape::Saw,
                Wave::Pulse => Shape::Pulse,
                Wave::Table => Shape::Table(osc.table.index()),
            };
        }
        plan.shapes[OSCILLATORS] = match params.sub.wave {
            SubWave::Sine => Shape::Sine,
            SubWave::Triangle => Shape::Triangle,
            SubWave::Square => Shape::Pulse,
            SubWave::Saw => Shape::Saw,
        };
        plan
    }
}

/// The colouring of the noise at the rate the voice runs at.
#[derive(Debug, Clone, Copy, Default)]
struct NoiseShape {
    /// Five first-order sections, `b0`, `b1`, `a1`, whose poles and zeros
    /// alternate a factor of √6 apart from 15 Hz: -3 dB per octave.
    pink: [[f64; 3]; 5],
    /// The gain that gives pink noise the power of the white.
    pink_gain: f64,
    /// The leaky integrator of the brown noise, at 20 Hz, and its gain.
    brown_pole: f64,
    brown_gain: f64,
}

impl NoiseShape {
    fn new(rate: f64) -> Self {
        let mut shape = Self::default();
        let warp = |hertz: f64| 1.0 / (PI * hertz.min(0.45 * rate) / rate).tan();
        for (index, section) in shape.pink.iter_mut().enumerate() {
            let pole = 15.0 * 6f64.powi(index as i32);
            let zero = pole * 6f64.sqrt();
            let (kp, kz) = (warp(pole), warp(zero));
            // (1 + s/zero) / (1 + s/pole) through the bilinear transform.
            *section = [
                (1.0 + kz) / (1.0 + kp),
                (1.0 - kz) / (1.0 + kp),
                (1.0 - kp) / (1.0 + kp),
            ];
        }
        // The power gain of the sections, averaged over the band that is
        // left after the decimator, the lower half: white noise keeps half
        // its power there, and so will the pink.
        let points = 4_096;
        let mut power = 0.0;
        for n in 0..points {
            let omega = 0.5 * PI * (n as f64 + 0.5) / points as f64;
            let (sin, cos) = omega.sin_cos();
            let mut gain = 1.0;
            for [b0, b1, a1] in shape.pink {
                let numerator = (b0 + b1 * cos).hypot(-b1 * sin);
                let denominator = (1.0 + a1 * cos).hypot(-a1 * sin);
                gain *= numerator / denominator;
            }
            power += gain * gain;
        }
        shape.pink_gain = (points as f64 / power).sqrt();
        shape.brown_pole = (-TAU * 20.0 / rate).exp();
        // Brown noise's power is nearly all in that band.
        shape.brown_gain = (0.5 * (1.0 - shape.brown_pole * shape.brown_pole)).sqrt();
        shape
    }
}

/// The noise source of one note.
#[derive(Debug, Clone, Copy)]
struct NoiseSource {
    random: Noise,
    pink: [[f64; 2]; 5],
    brown: f64,
}

impl NoiseSource {
    fn new() -> Self {
        Self {
            random: Noise::for_note(0, 0.0, 200),
            pink: [[0.0; 2]; 5],
            brown: 0.0,
        }
    }

    #[inline]
    fn next(&mut self, color: NoiseColor, shape: &NoiseShape) -> f64 {
        let white = self.random.next();
        match color {
            NoiseColor::White => white,
            NoiseColor::Pink => {
                let mut x = white;
                for (state, [b0, b1, a1]) in self.pink.iter_mut().zip(shape.pink) {
                    let y = b0 * x + b1 * state[0] - a1 * state[1];
                    *state = [x, flush(y)];
                    x = y;
                }
                (x * shape.pink_gain).clamp(-2.0, 2.0)
            }
            NoiseColor::Brown => {
                self.brown = flush(shape.brown_pole * self.brown + shape.brown_gain * white);
                self.brown.clamp(-2.0, 2.0)
            }
        }
    }
}

/// What every tone of a voice shares while it renders.
struct Context<'a> {
    params: &'a PolySynthParams,
    targets: &'a [f64; knob::COUNT],
    plan: &'a Plan,
    bank: &'a [Table],
    noise: &'a NoiseShape,
    /// The output rate and the rate the tone runs at.
    rate: f64,
    inner: f64,
    /// Shares per output sample: of the way the settings follow, the
    /// unison gains follow, and a shape's and a model's blend go.
    follow: f64,
    unison: f64,
    shape_blend: f64,
    model_blend: f64,
    gain_follow: f64,
    /// The pole of the DC blocker, at the inner rate.
    dc_pole: f64,
    tempo: f64,
    bend: f64,
    controls: &'a Controls,
    /// The tuning of the keys.
    tuning: &'a Tuning,
}

/// The player's controllers on the voice's channel.
#[derive(Debug, Clone, Copy, Default)]
struct Controls {
    mod_wheel: f64,
    aftertouch: f64,
    key_pressure: f64,
    brightness: f64,
}

/// What a filter's coefficients were worked out for.
type CoefficientsFor = (FilterModel, Option<FilterModel>, [f64; 4]);

/// No coefficients yet, and the gains of the middle of the stereo field
/// for a place not yet asked for.
const NO_COEFFICIENTS: (Option<CoefficientsFor>, Coefficients, Coefficients) =
    (None, Coefficients::ZERO, Coefficients::ZERO);
const NO_PAN: (f64, (f64, f64)) = (f64::NAN, (1.0, 1.0));

/// The sound of one note.
#[derive(Debug, Clone)]
struct Tone {
    /// The three oscillators, then the sub oscillator.
    oscs: [Osc; OSCILLATORS + 1],
    noise: NoiseSource,
    filters: [FilterSlot; FILTERS],
    clip: [SoftClip; 2],
    /// The DC blocker of each channel: its last input and output.
    dc: [[f64; 2]; 2],
    /// Each filter's coefficients, for the model, the model it blends
    /// from, and the cutoff, resonance, drive and morph they were made for.
    coefficients: [(Option<CoefficientsFor>, Coefficients, Coefficients); FILTERS],
    /// The voice's place in the stereo field and its gains.
    pan_gains: (f64, (f64, f64)),
    envelopes: [Envelope; ENVELOPES],
    lfos: [Lfo; LFOS],
    knobs: [f64; knob::COUNT],
    settled: bool,
    /// Each LFO's rate change from the matrix, in octaves, as of the last
    /// sample.
    lfo_octaves: [f64; LFOS],
    key: f64,
    /// How far the pitch still has to glide, in semitones, and by how
    /// much a sample.
    glide: f64,
    glide_step: f64,
    velocity: f64,
    accent: bool,
    random: f64,
    /// The level velocity and accent ask for, and the level heard.
    gain: f64,
    gain_now: f64,
    /// The note's own expression.
    tuning: f64,
    volume: f64,
    pan: f64,
    pressure: f64,
    brightness: f64,
    vibrato: f64,
    /// Whether the filters run on two channels: once the oscillators are
    /// panned or spread.
    stereo: bool,
}

impl Tone {
    fn new() -> Self {
        Self {
            oscs: core::array::from_fn(|_| Osc::new()),
            noise: NoiseSource::new(),
            filters: core::array::from_fn(|_| FilterSlot::new()),
            clip: [SoftClip::default(); 2],
            dc: [[0.0; 2]; 2],
            coefficients: [NO_COEFFICIENTS; FILTERS],
            pan_gains: NO_PAN,
            envelopes: [Envelope::IDLE; ENVELOPES],
            lfos: [Lfo::new(); LFOS],
            knobs: [0.0; knob::COUNT],
            settled: false,
            lfo_octaves: [0.0; LFOS],
            key: 60.0,
            glide: 0.0,
            glide_step: 0.0,
            velocity: 0.0,
            accent: false,
            random: 0.0,
            gain: 0.0,
            gain_now: 0.0,
            tuning: 0.0,
            volume: 1.0,
            pan: 0.0,
            pressure: 0.0,
            brightness: 0.0,
            vibrato: 0.0,
            stereo: false,
        }
    }

    fn prepare(&mut self, inner: f64) {
        for filter in &mut self.filters {
            filter.prepare(inner);
        }
        self.silence();
    }

    /// Back to the state of a new tone, but for the filters' memory sizes.
    fn silence(&mut self) {
        for osc in &mut self.oscs {
            osc.silence();
        }
        self.noise = NoiseSource::new();
        for filter in &mut self.filters {
            filter.silence();
        }
        self.clip = [SoftClip::default(); 2];
        self.dc = [[0.0; 2]; 2];
        self.coefficients = [NO_COEFFICIENTS; FILTERS];
        self.pan_gains = NO_PAN;
        self.envelopes = [Envelope::IDLE; ENVELOPES];
        self.lfos = [Lfo::new(); LFOS];
        self.knobs = [0.0; knob::COUNT];
        self.settled = false;
        self.lfo_octaves = [0.0; LFOS];
        self.glide = 0.0;
        self.glide_step = 0.0;
        self.gain = 0.0;
        self.gain_now = 0.0;
        self.tuning = 0.0;
        self.volume = 1.0;
        self.pan = 0.0;
        self.pressure = 0.0;
        self.brightness = 0.0;
        self.vibrato = 0.0;
        self.stereo = false;
    }

    fn is_sounding(&self) -> bool {
        self.envelopes[0].is_active()
    }

    /// The pitch heard now, in semitones, before the oscillators' own.
    fn pitch(&self) -> f64 {
        self.key + self.glide
    }

    /// Starts a note. A fresh tone starts from rest; one that sounds
    /// carries on and strikes its envelopes again from where they are, and
    /// glides to the new key if `glide` is given (seconds).
    #[allow(clippy::too_many_arguments)]
    fn strike(
        &mut self,
        cx: &Context<'_>,
        key: u8,
        velocity: f64,
        accent: bool,
        fresh: bool,
        glide: Option<f64>,
    ) {
        let params = cx.params;
        let old_pitch = self.pitch();
        if fresh {
            self.silence();
            self.knobs = *cx.targets;
            self.settled = true;
            for (index, osc) in self.oscs.iter_mut().enumerate() {
                let mut random = Noise::for_note(key, velocity, 100 + index as u64);
                if index < OSCILLATORS {
                    let settings = &params.oscillators[index];
                    osc.arrange(
                        usize::from(settings.unison),
                        settings.detune,
                        settings.pan,
                        settings.spread,
                    );
                    osc.strike(
                        cx.plan.shapes[index],
                        settings.phase,
                        settings.random_phase,
                        &mut random,
                    );
                } else {
                    osc.arrange(1, 0.0, 0.0, 0.0);
                    osc.strike(cx.plan.shapes[index], 0.0, 0.0, &mut random);
                }
            }
            for (filter, settings) in self.filters.iter_mut().zip(&params.filters) {
                filter.set_model(settings.model, true);
            }
            self.noise.random = Noise::for_note(key, velocity, 200);
            self.random = Noise::for_note(key, velocity, 400).next();
        }
        // The key's pitch in the tuning; the glide, the bend and the
        // oscillators' own pitches are intervals on top of it.
        self.key = cx.tuning.key_pitch(key);
        self.glide = match glide {
            Some(seconds) if seconds > 0.0 && !fresh => {
                let distance = old_pitch - self.key;
                self.glide_step = distance.abs() / (seconds * cx.rate).max(1.0);
                distance
            }
            _ => 0.0,
        };
        self.take(params, velocity, accent);
        if fresh {
            self.gain_now = self.gain;
        }
        for (index, (envelope, settings)) in
            self.envelopes.iter_mut().zip(&params.envelopes).enumerate()
        {
            let floor = if index == 0 { AMP_FLOOR } else { Floor::NONE };
            envelope.strike(settings, cx.rate, floor);
        }
        for (index, (lfo, settings)) in self.lfos.iter_mut().zip(&params.lfos).enumerate() {
            lfo.strike(
                settings,
                fresh,
                Noise::for_note(key, velocity, 300 + index as u64),
                cx.rate,
            );
        }
    }

    /// Takes a note's velocity and accent.
    fn take(&mut self, params: &PolySynthParams, velocity: f64, accent: bool) {
        self.velocity = velocity;
        self.accent = accent;
        let mut gain = velocity_gain(velocity, 24.0 * params.velocity);
        if accent {
            gain *= db(params.accent_db);
        }
        self.gain = gain;
    }

    /// Slurs to `key`: the pitch glides there over `seconds` (or jumps,
    /// for none) and nothing is struck again.
    fn slur(&mut self, cx: &Context<'_>, key: u8, velocity: f64, accent: bool, seconds: f64) {
        let old_pitch = self.pitch();
        self.key = cx.tuning.key_pitch(key);
        if seconds > 0.0 {
            self.glide = old_pitch - self.key;
            self.glide_step = self.glide.abs() / (seconds * cx.rate).max(1.0);
        } else {
            self.glide = 0.0;
        }
        self.take(cx.params, velocity, accent);
    }

    fn release(&mut self, params: &PolySynthParams, rate: f64) {
        for (index, (envelope, settings)) in
            self.envelopes.iter_mut().zip(&params.envelopes).enumerate()
        {
            let floor = if index == 0 { AMP_FLOOR } else { Floor::NONE };
            envelope.release(settings, rate, floor);
        }
    }

    /// Takes the shapes and models of new settings: blended if the tone
    /// sounds, at once if not.
    fn take_settings(&mut self, params: &PolySynthParams, plan: &Plan) {
        let sounding = self.is_sounding();
        for (osc, &shape) in self.oscs.iter_mut().zip(&plan.shapes) {
            osc.set_shape(shape);
        }
        for (filter, settings) in self.filters.iter_mut().zip(&params.filters) {
            filter.set_model(settings.model, !sounding);
        }
        self.settled = false;
    }

    /// One output sample's worth: two samples at the inner rate, left and
    /// right, after the amplifier.
    #[inline]
    fn step(&mut self, cx: &Context<'_>) -> [(f64, f64); OVERSAMPLING] {
        let params = cx.params;

        // The settings follow what they are set to.
        if !self.settled {
            let mut moving = false;
            for (knob, &target) in self.knobs.iter_mut().zip(cx.targets.iter()) {
                let difference = target - *knob;
                if difference.abs() > 1e-9 {
                    *knob += difference * cx.follow;
                    moving = true;
                } else {
                    *knob = target;
                }
            }
            self.settled = !moving;
        }
        let k = &self.knobs;

        // The sources.
        let mut sources = [0.0; ModSource::ALL.len()];
        let reads = &cx.plan.reads;
        for (index, (envelope, settings)) in
            self.envelopes.iter_mut().zip(&params.envelopes).enumerate()
        {
            if index == 0 || reads[ModSource::Env1.index() + index] {
                let floor = if index == 0 { AMP_FLOOR } else { Floor::NONE };
                sources[ModSource::Env1.index() + index] = envelope.next(settings, cx.rate, floor);
            }
        }
        for (index, (lfo, settings)) in self.lfos.iter_mut().zip(&params.lfos).enumerate() {
            if reads[ModSource::Lfo1.index() + index] {
                let hertz = if settings.beats > 0.0 {
                    cx.tempo / 60.0 / settings.beats
                } else {
                    settings.rate
                };
                let hertz = hertz * limit(self.lfo_octaves[index], -12.0, 12.0).exp2();
                sources[ModSource::Lfo1.index() + index] = lfo.next(settings, hertz, cx.rate);
            }
        }
        sources[ModSource::Velocity.index()] = self.velocity;
        sources[ModSource::Accent.index()] = f64::from(u8::from(self.accent));
        sources[ModSource::Aftertouch.index()] = cx
            .controls
            .aftertouch
            .max(cx.controls.key_pressure)
            .max(self.pressure);
        sources[ModSource::ModWheel.index()] = cx.controls.mod_wheel.max(self.vibrato);
        sources[ModSource::Key.index()] = (self.key - 60.0) / 60.0;
        sources[ModSource::Random.index()] = self.random;
        sources[ModSource::Brightness.index()] = cx.controls.brightness.max(self.brightness);
        sources[ModSource::Off.index()] = 1.0;

        // The matrix.
        let mut to = [0.0; ModDestination::ALL.len()];
        for &index in &cx.plan.slots[..cx.plan.active] {
            let slot = &params.matrix[index];
            let amount = k[knob::AMOUNTS + index];
            to[slot.destination.index()] +=
                sources[slot.source.index()] * sources[slot.via.index()] * amount;
        }
        let d = |destination: ModDestination| to[destination.index()];
        self.lfo_octaves = [
            d(ModDestination::Lfo1Rate),
            d(ModDestination::Lfo2Rate),
            d(ModDestination::Lfo3Rate),
        ];

        // The pitch.
        if self.glide != 0.0 {
            self.glide = step_towards(self.glide, 0.0, self.glide_step);
        }
        let pitch = self.pitch() + cx.bend + self.tuning + d(ModDestination::Pitch);

        // The oscillators.
        const PITCH_TO: [ModDestination; 3] = [
            ModDestination::Osc1Pitch,
            ModDestination::Osc2Pitch,
            ModDestination::Osc3Pitch,
        ];
        const LEVEL_TO: [ModDestination; 3] = [
            ModDestination::Osc1Level,
            ModDestination::Osc2Level,
            ModDestination::Osc3Level,
        ];
        const SHAPE_TO: [ModDestination; 3] = [
            ModDestination::Osc1Shape,
            ModDestination::Osc2Shape,
            ModDestination::Osc3Shape,
        ];
        const FM_TO: [ModDestination; 3] = [
            ModDestination::Osc1Fm,
            ModDestination::Osc2Fm,
            ModDestination::Osc3Fm,
        ];
        let mut level = [0.0; OSCILLATORS + 1];
        let mut increment = [0.0; OSCILLATORS + 1];
        let mut shape = [0.0; OSCILLATORS];
        let mut fm = [0.0; OSCILLATORS];
        let mut ring = [0.0; OSCILLATORS];
        let detune_by = d(ModDestination::Detune);
        let mut stereo = false;
        for index in 0..OSCILLATORS {
            let base = index * knob::PER_OSC;
            let settings = &params.oscillators[index];
            level[index] = limit(k[base + knob::LEVEL] + d(LEVEL_TO[index]), 0.0, 1.0);
            shape[index] = limit(k[base + knob::SHAPE] + d(SHAPE_TO[index]), 0.0, 1.0);
            fm[index] = limit(k[base + knob::FM] + d(FM_TO[index]), 0.0, 32.0);
            ring[index] = k[base + knob::RING];
            let detune = limit(k[base + knob::DETUNE] + detune_by, 0.0, 1.0);
            let (pan, spread) = (k[base + knob::PAN], k[base + knob::SPREAD]);
            let osc = &mut self.oscs[index];
            osc.arrange(usize::from(settings.unison), detune, pan, spread);
            osc.glide(cx.unison, cx.shape_blend);
            if level[index] > 0.0 && (pan.abs() > 1e-9 || (spread > 1e-9 && settings.unison > 1)) {
                stereo = true;
            }
        }
        level[OSCILLATORS] = limit(k[knob::SUB] + d(ModDestination::SubLevel), 0.0, 1.0);
        self.oscs[OSCILLATORS].glide(cx.unison, cx.shape_blend);
        let noise_level = limit(k[knob::NOISE] + d(ModDestination::NoiseLevel), 0.0, 1.0);

        // Which oscillators have to run: those heard, those they take
        // their modulation from, and the master of a synced one.
        let mut needed = [false; OSCILLATORS + 1];
        for index in 0..=OSCILLATORS {
            needed[index] = level[index] > 0.0;
        }
        for _ in 0..2 {
            for index in 0..OSCILLATORS {
                if needed[index] {
                    let settings = &params.oscillators[index];
                    if fm[index] > 0.0 || ring[index] > 0.0 {
                        needed[settings.mod_source.index()] = true;
                    }
                    if settings.sync && index > 0 {
                        needed[0] = true;
                    }
                }
            }
        }

        // The pitches of those that run, and how they are drawn; the sub
        // oscillator follows oscillator 1's pitch.
        let mut drawings = [Drawing {
            width: 0.5,
            position: 0.0,
            level: 0,
            bank: cx.bank,
            reader: None,
        }; OSCILLATORS + 1];
        for index in 0..=OSCILLATORS {
            if !needed[index] {
                continue;
            }
            let osc_pitch = if index < OSCILLATORS {
                pitch + k[index * knob::PER_OSC + knob::PITCH] + d(PITCH_TO[index])
            } else {
                pitch + k[knob::PITCH] + d(ModDestination::Osc1Pitch)
                    - 12.0 * f64::from(params.sub.octaves)
            };
            increment[index] = key_hz(limit(osc_pitch, -60.0, 200.0)) / cx.inner;
            let drawing = &mut drawings[index];
            drawing.level = Table::level_for(increment[index]);
            if index < OSCILLATORS {
                drawing.width = 0.5 - 0.49 * shape[index];
                drawing.position = shape[index];
                if let Shape::Table(table) = cx.plan.shapes[index] {
                    let reader = cx.bank[table].reader(drawing.level, drawing.position);
                    drawing.reader = Some((table, reader));
                }
            }
        }

        // The filters' settings for this sample.
        let mut now = [Coefficients::ZERO; FILTERS];
        let mut before = [Coefficients::ZERO; FILTERS];
        let mut running = [false; FILTERS];
        const CUTOFF_TO: [ModDestination; 2] = [ModDestination::Cutoff1, ModDestination::Cutoff2];
        let filter_envelope = sources[ModSource::Env2.index()];
        let note_pitch = self.key + self.glide;
        for index in 0..FILTERS {
            let filter = &mut self.filters[index];
            filter.glide(cx.model_blend);
            let fading = filter.fading_from();
            if filter.model() == FilterModel::Off && fading.is_none() {
                continue;
            }
            running[index] = true;
            let base = knob::FILTER + index * knob::PER_FILTER;
            let octaves = k[base + knob::CUTOFF]
                + k[base + knob::TRACK] * (note_pitch - 60.0) / 12.0
                + k[base + knob::ENV] * filter_envelope
                + d(ModDestination::Cutoff)
                + d(CUTOFF_TO[index]);
            let cutoff = limit(octaves, 3.0, 16.0).exp2();
            let resonance = limit(
                k[base + knob::RESONANCE] + d(ModDestination::Resonance),
                0.0,
                1.0,
            );
            let drive = limit(k[base + knob::DRIVE] + d(ModDestination::Drive), 0.0, 48.0);
            let morph = limit(k[base + knob::MORPH] + d(ModDestination::Morph), 0.0, 1.0);
            // Worked out again only when something moved.
            let wanted = (filter.model(), fading, [cutoff, resonance, drive, morph]);
            let cache = &mut self.coefficients[index];
            if cache.0 != Some(wanted) {
                let fresh =
                    Coefficients::new(filter.model(), cutoff, resonance, drive, morph, cx.inner);
                let old = match fading {
                    Some(model) => {
                        Coefficients::new(model, cutoff, resonance, drive, morph, cx.inner)
                    }
                    None => fresh,
                };
                *cache = (Some(wanted), fresh, old);
            }
            now[index] = cache.1;
            before[index] = cache.2;
        }

        // The amplifier.
        self.gain_now += (self.gain - self.gain_now) * cx.gain_follow;
        let volume = d(ModDestination::Volume);
        let volume = if volume == 0.0 {
            1.0
        } else {
            db(limit(volume, -96.0, 24.0))
        };
        let amp = sources[ModSource::Env1.index()] / MIX_GAIN
            * self.gain_now
            * k[knob::OUTPUT]
            * volume
            * self.volume;
        let pan = k[knob::PAN_ALL] + d(ModDestination::Pan) + self.pan;
        if pan != self.pan_gains.0 {
            self.pan_gains = (pan, pan_gains(pan));
        }
        let (pan_left, pan_right) = self.pan_gains.1;
        let saturation = k[knob::SATURATION];

        if stereo && !self.stereo {
            for filter in &mut self.filters {
                filter.spread_to_stereo();
            }
            self.clip[1] = self.clip[0];
            self.dc[1] = self.dc[0];
            self.stereo = true;
        }
        let channels = if self.stereo { 2 } else { 1 };

        let zeros = [0.0; MAX_UNISON];
        let mut out = [(0.0, 0.0); OVERSAMPLING];
        for slot in &mut out {
            let mut mix = (0.0, 0.0);
            for index in 0..=OSCILLATORS {
                if !needed[index] {
                    continue;
                }
                let settings = (index < OSCILLATORS).then(|| &params.oscillators[index]);
                let drawing = &drawings[index];
                let (pm_depth, source) = match settings {
                    Some(settings) if fm[index] > 0.0 => {
                        (fm[index] / TAU, Some(settings.mod_source.index()))
                    }
                    _ => (0.0, None),
                };
                let modulator = match source {
                    Some(source) => self.oscs[source].value,
                    None => zeros,
                };
                let modulator_voices = source.map_or(1, |source| self.oscs[source].voices());
                let synced = settings.is_some_and(|settings| settings.sync) && index > 0;
                let master = synced.then(|| (self.oscs[0].wrapped, self.oscs[0].voices()));
                let (left, right) = self.oscs[index].tick(
                    increment[index],
                    drawing,
                    pm_depth,
                    &modulator,
                    modulator_voices,
                    master.as_ref().map(|(wrapped, count)| (wrapped, *count)),
                );
                let (mut left, mut right) = (left * level[index], right * level[index]);
                if index < OSCILLATORS && ring[index] > 0.0 {
                    let source = params.oscillators[index].mod_source.index();
                    let (a, b) = self.oscs[source].out;
                    let carrier = 1.0 - ring[index] + ring[index] * 0.5 * (a + b);
                    left *= carrier;
                    right *= carrier;
                }
                mix.0 += left;
                mix.1 += right;
            }
            if noise_level > 0.0 {
                let noise = noise_level * self.noise.next(params.noise.color, cx.noise);
                mix.0 += noise;
                mix.1 += noise;
            }
            let input = [mix.0 * MIX_GAIN, mix.1 * MIX_GAIN];
            let mut filtered = input;
            for channel in 0..channels {
                let advance = channel + 1 == channels;
                let x = limit(input[channel], -64.0, 64.0);
                let y = match params.routing {
                    FilterRouting::Serial => {
                        let mut y = x;
                        for index in 0..FILTERS {
                            if running[index] {
                                y = self.filters[index].process(
                                    channel,
                                    y,
                                    &now[index],
                                    &before[index],
                                    advance,
                                    true,
                                );
                            }
                        }
                        y
                    }
                    FilterRouting::Parallel => {
                        if running.iter().any(|&r| r) {
                            let mut y = 0.0;
                            for index in 0..FILTERS {
                                if running[index] {
                                    y += self.filters[index].process(
                                        channel,
                                        x,
                                        &now[index],
                                        &before[index],
                                        advance,
                                        false,
                                    );
                                }
                            }
                            y
                        } else {
                            x
                        }
                    }
                };
                let y = if saturation > 0.0 {
                    let drive = db(24.0 * saturation);
                    let wet = self.clip[channel].next(drive * y) / drive.sqrt();
                    y + (wet - y) * (8.0 * saturation).min(1.0)
                } else {
                    y
                };
                // What sync, FM and the saturators leave at 0 Hz goes.
                let [last_in, last_out] = self.dc[channel];
                let out = flush(y - last_in + cx.dc_pole * last_out);
                self.dc[channel] = [y, out];
                filtered[channel] = out;
            }
            if channels == 1 {
                filtered[1] = filtered[0];
            }
            *slot = (filtered[0] * amp * pan_left, filtered[1] * amp * pan_right);
        }
        out
    }
}

/// The half-band filter that brings two channels down from twice the
/// sample rate.
#[derive(Debug, Clone)]
struct Decimator {
    /// The taps at even distances from the newest sample, which pair up
    /// with their mirror images; every other tap but the centre is zero.
    pairs: [f64; DECIMATOR_PAIRS],
    centre: f64,
    history: [[f64; DECIMATOR_HISTORY]; 2],
    written: usize,
}

impl Decimator {
    fn new() -> Self {
        let taps = fir::lowpass(DECIMATOR_TAPS, 0.25, DECIMATOR_DB);
        let mut pairs = [0.0; DECIMATOR_PAIRS];
        for (index, pair) in pairs.iter_mut().enumerate() {
            *pair = taps[2 * index];
        }
        Self {
            pairs,
            centre: taps[(DECIMATOR_TAPS - 1) / 2],
            history: [[0.0; DECIMATOR_HISTORY]; 2],
            written: 0,
        }
    }

    fn clear(&mut self) {
        self.history = [[0.0; DECIMATOR_HISTORY]; 2];
        self.written = 0;
    }

    /// Takes two samples at the inner rate and gives one at the output's.
    #[inline]
    fn next(&mut self, pair: [(f64, f64); OVERSAMPLING]) -> (f64, f64) {
        const MASK: usize = DECIMATOR_HISTORY - 1;
        for (left, right) in pair {
            self.written = self.written.wrapping_add(1);
            self.history[0][self.written & MASK] = left;
            self.history[1][self.written & MASK] = right;
        }
        let newest = self.written;
        let mut out = [0.0; 2];
        for (channel, history) in self.history.iter().enumerate() {
            let at = |back: usize| history[newest.wrapping_sub(back) & MASK];
            let mut sum = self.centre * at((DECIMATOR_TAPS - 1) / 2);
            for (index, &tap) in self.pairs.iter().enumerate() {
                sum += tap * (at(2 * index) + at(DECIMATOR_TAPS - 1 - 2 * index));
            }
            out[channel] = flush(sum);
        }
        (out[0], out[1])
    }
}

/// A deep polyphonic synthesiser voice: three band-limited oscillators
/// with sixteen-voice unison, a sub oscillator and noise, two filters of
/// fifteen models, four envelopes, three LFOs and a sixteen-slot
/// modulation matrix. The library of [`PRESETS`](super::PRESETS) is made
/// with it.
///
/// ```text
///  osc 1 ─┐ (sync, FM, ring between them)
///  osc 2 ─┤                 ┌─ filter 1 ─ filter 2 ─┐   (or side by side)
///  osc 3 ─┼─ mix ───────────┤                       ├─ saturator ─ amp ─ pan
///  sub   ─┤                 └───────────────────────┘     envelope 1 ┘
///  noise ─┘    envelopes, LFOs, velocity, accent, wheel, pressure → matrix
/// ```
///
/// Everything runs at twice the sample rate and comes back down through a
/// half-band filter flat to 20 kHz (at 48 kHz) with 100 dB of stopband,
/// so the voice is [`LATENCY_SAMPLES`] late.
///
/// # Oscillators
///
/// Sine, triangle, sawtooth, a pulse whose width the shape sets (and the
/// matrix modulates: PWM), and twelve [wavetables](super::TableShape)
/// built from formulas, whose position the shape sets. The classic shapes
/// are band-limited with four-point polyBLEP and polyBLAMP residuals,
/// which also smooth the jumps and bends of hard sync and of phase
/// modulation; the wavetables are mip-mapped an octave per level. The
/// tests measure what aliasing is left below 20 kHz, from the C an octave
/// above middle C to the G four octaves above it: at most -82 dB of the
/// energy of a sawtooth or a square, -78 dB of a narrow pulse, -109 dB of
/// a triangle, and -87 dB of the wavetables (-110 dB at the top); hard
/// sync leaves -97 dB between the master's harmonics.
///
/// Each oscillator plays up to [`MAX_UNISON`](super::MAX_UNISON) unison
/// voices, detuned more densely in the middle as the classic supersaw is
/// and spread across the stereo field with neighbours in pitch on
/// opposite sides, each starting at a random phase if asked. An
/// oscillator can be hard-synced to oscillator 1, and phase- or
/// ring-modulated by any of them, unison voice by unison voice. The sub
/// oscillator plays one or two octaves under oscillator 1.
///
/// # Filters
///
/// Two, in series or in parallel. Each is one of these models, every one
/// built from trapezoidal integrators with its loop solved without delay,
/// so that it can be swept at audio rate (V. Zavalishin, "The Art of VA
/// Filter Design", 2012):
///
/// - **The transistor ladder**, 24 or 12 dB low-pass, high-pass and
///   band-pass, with its `tanh` in the loop: the classic four-pole, which
///   self-oscillates at full resonance. Some of the bass its resonance
///   takes away is given back.
/// - **The state-variable filter**, linear unless driven: clean low-pass,
///   band-pass, high-pass and notch up to a Q of 20.
/// - **The SEM-like filter**: the state-variable filter with its band
///   memory softly held, so its resonance stays sweet, and its output
///   morphing continuously from low-pass through notch (or band-pass) to
///   high-pass, as the SEM's mode knob does.
/// - **The MS-20-like filter**: a two-pole Sallen–Key low- or high-pass
///   (the Korg 35 topology) with a `tanh` at the node its feedback returns
///   to, which makes it bark when driven and scream, self-limited, at full
///   resonance. Its quiet response is that of the state-variable filter
///   of Q `1/(2 - 2·resonance)`.
/// - **Combs**, with positive or negative feedback up to 0.985, read
///   between samples, damped in the loop and tuned for the damping's
///   delay, so that with key tracking they ring at the note: plucked and
///   metallic resonators, flangers in a note.
/// - **A formant filter**: three band-passes at the formants of the
///   vowels a, e, i, o and u (Peterson and Barney's), morphing between
///   them, which the cutoff moves up and down.
///
/// Each has a cutoff that follows the key and envelope 2, a resonance, a
/// drive into its saturation, and a morph where the model has one. A
/// change of model on a sounding note is blended over 10 ms.
///
/// # Modulation
///
/// Envelope 1 opens the amplifier, envelope 2 the filters by their
/// [`env_amount`](super::FilterParams::env_amount); envelopes 3 and 4 and
/// the LFOs are free. Envelopes have a curve for each segment; LFOs have
/// six shapes, a rate in hertz or in beats of the tempo
/// ([`set_tempo`](Self::set_tempo)), retrigger and fade-in. Velocity,
/// accent, aftertouch (channel, polyphonic or per-note pressure), the
/// modulation wheel, the key, a random value per note and brightness
/// (controller 74 or the per-note expression) are sources too. The
/// [matrix](super::ModSlot) adds each slot's source times its `via` times
/// its amount to a destination, every sample.
///
/// # Playing
///
/// In [poly](super::VoiceMode::Poly) mode a voice taken over for a new
/// note fades its old one out over [`HANDOVER_SECONDS`] beside the new.
/// In [mono](super::VoiceMode::Mono) and [legato](super::VoiceMode::Legato)
/// modes the voice carries on into the next note and glides to it over
/// [`glide`](super::PolySynthParams::glide), mono striking the envelopes
/// again from where they are and legato only when no key was held: use
/// [`PolySynthVoice::synth`] for a synth set up for the mode.
///
/// Continuous settings follow a change over 5 ms; a change of shape or
/// unison count is blended too. Nothing a setting can be set to makes the
/// voice put out anything but finite numbers, every source of randomness
/// is seeded by the note, and the output does not depend on how the
/// timeline is cut into blocks. Pitch bend and [`Expression::Tuning`] bend
/// every oscillator; [`Expression::Volume`] and [`Expression::Pan`] place
/// the note.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::poly::{self, PolySynthVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor};
///
/// let preset = poly::preset("Trance Supersaw").unwrap();
/// let mut synth = PolySynthVoice::synth::<f32>(&preset.params, 8);
/// synth.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [57, 60, 64] {
///     synth.note_on(NoteRef::new(0, key), 0.8);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// synth.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct PolySynthVoice {
    shared: Shared,
    now: Tone,
    old: Tone,
    /// The old tone's fade, 1 to 0, and its fall per sample.
    fade: f64,
    fade_step: f64,
    decimator: Decimator,
    /// Output samples the decimator still has to put out after the tones
    /// have fallen silent; rendered whatever the blocks, so that the end of
    /// a note does not depend on where a block starts.
    tail: u32,
    /// Whether a mono or legato voice was taken over while sounding, so
    /// that its next note carries on from it.
    carried: bool,
}

/// What both tones of a voice read.
#[derive(Debug, Clone)]
struct Shared {
    params: PolySynthParams,
    targets: [f64; knob::COUNT],
    plan: Plan,
    noise: NoiseShape,
    rate: f64,
    tempo: f64,
    bend: f64,
    controls: Controls,
    /// The key of the note, for polyphonic pressure.
    key: u8,
    /// The tuning of the keys, from `Synth::set_tuning`.
    tuning: Tuning,
}

impl Shared {
    fn context<'a>(&'a self, bank: &'a [Table]) -> Context<'a> {
        let rate = self.rate;
        let share = |seconds: f64| 1.0 - (-1.0 / (seconds * rate)).exp();
        Context {
            params: &self.params,
            targets: &self.targets,
            plan: &self.plan,
            bank,
            noise: &self.noise,
            rate,
            inner: rate * OVERSAMPLING as f64,
            follow: share(SMOOTHING_SECONDS),
            unison: share(UNISON_SECONDS),
            shape_blend: 1.0 / (SHAPE_BLEND_SECONDS * rate),
            model_blend: 1.0 / (MODEL_BLEND_SECONDS * rate),
            gain_follow: share(GAIN_SECONDS),
            dc_pole: (-TAU * DC_BLOCK_HZ / (rate * OVERSAMPLING as f64)).exp(),
            tempo: self.tempo,
            bend: self.bend,
            controls: &self.controls,
            tuning: &self.tuning,
        }
    }
}

impl PolySynthVoice {
    /// A voice with these settings. The first voice made builds the
    /// wavetables, which takes a moment; make voices off the audio thread.
    pub fn new(params: PolySynthParams) -> Self {
        // Built here, so that nothing on the audio thread ever builds them.
        let _ = bank();
        let params = params.limited();
        let rate = 48_000.0;
        Self {
            shared: Shared {
                targets: targets(&params),
                plan: Plan::new(&params),
                params,
                noise: NoiseShape::new(rate * OVERSAMPLING as f64),
                rate,
                tempo: 120.0,
                bend: 0.0,
                controls: Controls::default(),
                key: 60,
                tuning: Tuning::default(),
            },
            now: Tone::new(),
            old: Tone::new(),
            fade: 0.0,
            fade_step: 1.0,
            decimator: Decimator::new(),
            tail: 0,
            carried: false,
        }
    }

    /// A [`Synth`] of voices with these settings, set up for their
    /// [mode](super::VoiceMode): `polyphony` voices in poly mode, one voice
    /// in mono and legato modes, and legato playing on in legato mode.
    pub fn synth<S: Sample>(params: &PolySynthParams, polyphony: usize) -> Synth<Self, S> {
        let count = match params.mode {
            VoiceMode::Poly => polyphony.max(1),
            VoiceMode::Mono | VoiceMode::Legato => 1,
        };
        let mut synth = Synth::with_voices(count, || Self::new(*params));
        synth.set_legato(params.mode == VoiceMode::Legato);
        synth
    }

    /// Takes new settings, limited to their ranges. Real-time safe. A
    /// sounding note follows the continuous ones over a few milliseconds
    /// and blends to a new shape or filter model.
    pub fn set_params(&mut self, params: PolySynthParams) {
        let shared = &mut self.shared;
        shared.params = params.limited();
        shared.targets = targets(&shared.params);
        shared.plan = Plan::new(&shared.params);
        self.now.take_settings(&shared.params, &shared.plan);
        self.old.take_settings(&shared.params, &shared.plan);
    }

    /// The settings, as limited.
    pub fn params(&self) -> &PolySynthParams {
        &self.shared.params
    }

    /// Sets the tempo in quarter notes per minute, which LFOs locked to it
    /// follow. 120 until set; limited to 20 to 999. Real-time safe.
    pub fn set_tempo(&mut self, bpm: f64) {
        if bpm.is_finite() {
            self.shared.tempo = bpm.clamp(20.0, 999.0);
        }
    }

    /// Takes the key of a new note; the pressure on another key does not
    /// carry over to it.
    fn take_key(&mut self, key: u8) {
        if key != self.shared.key {
            self.shared.controls.key_pressure = 0.0;
        }
        self.shared.key = key;
    }

    /// Back to the state of a new voice, but for the settings and the
    /// player's controllers on the channel. The pressure on the key, which
    /// belongs to the note, goes.
    fn reset(&mut self) {
        self.now.silence();
        self.old.silence();
        self.fade = 0.0;
        self.decimator.clear();
        self.tail = 0;
        self.carried = false;
        self.shared.controls.key_pressure = 0.0;
    }
}

/// The next output sample of a voice's two tones, left and right.
#[inline]
fn next_sample(
    now: &mut Tone,
    old: &mut Tone,
    fade: &mut f64,
    fade_step: f64,
    decimator: &mut Decimator,
    cx: &Context<'_>,
) -> (f64, f64) {
    let mut pair = if now.is_sounding() {
        now.step(cx)
    } else {
        [(0.0, 0.0); OVERSAMPLING]
    };
    if *fade > 0.0 {
        let gone = old.step(cx);
        for (sample, (left, right)) in pair.iter_mut().zip(gone) {
            sample.0 += left * *fade;
            sample.1 += right * *fade;
        }
        *fade -= fade_step;
        if *fade <= 0.0 {
            *fade = 0.0;
            old.silence();
        }
    }
    decimator.next(pair)
}

impl Default for PolySynthVoice {
    fn default() -> Self {
        Self::new(PolySynthParams::INIT)
    }
}

impl<S: Sample> Voice<S> for PolySynthVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        let rate = if spec.sample_rate.is_finite() && spec.sample_rate > 0.0 {
            spec.sample_rate
        } else {
            48_000.0
        };
        self.shared.rate = rate;
        let inner = rate * OVERSAMPLING as f64;
        self.shared.noise = NoiseShape::new(inner);
        self.now.prepare(inner);
        self.old.prepare(inner);
        self.fade_step = 1.0 / (HANDOVER_SECONDS * rate).max(1.0);
        self.reset();
    }

    fn set_tuning(&mut self, tuning: &Tuning) {
        self.shared.tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let key = note.key.min(127);
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let carried = core::mem::take(&mut self.carried);
        let shared = &self.shared;
        let glide =
            (carried && shared.params.mode == VoiceMode::Mono).then_some(shared.params.glide);
        let fresh = !self.now.is_sounding();
        let cx = shared.context(bank());
        self.now.strike(&cx, key, velocity, accent, fresh, glide);
        self.take_key(key);
    }

    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.now.is_sounding() {
            Voice::<S>::note_on_accented(self, note, velocity, accent);
            return;
        }
        let key = note.key.min(127);
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let cx = self.shared.context(bank());
        self.now
            .slur(&cx, key, velocity, accent, self.shared.params.glide);
        self.take_key(key);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.now.release(&self.shared.params, self.shared.rate);
    }

    fn choke(&mut self) {
        let sounding = self.now.is_sounding();
        match self.shared.params.mode {
            VoiceMode::Poly => {
                if sounding {
                    core::mem::swap(&mut self.now, &mut self.old);
                    self.fade = 1.0;
                    self.now.silence();
                } else {
                    self.reset();
                }
            }
            VoiceMode::Mono | VoiceMode::Legato => {
                if sounding && !self.carried {
                    self.carried = true;
                } else {
                    self.reset();
                }
            }
        }
        self.now.tuning = 0.0;
    }

    fn is_active(&self) -> bool {
        self.now.is_sounding() || self.fade > 0.0 || self.tail > 0
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        if !Voice::<S>::is_active(self) {
            return;
        }
        let channels = buffer.num_channels();
        let cx = self.shared.context(bank());
        for frame in 0..buffer.num_frames() {
            let (left, right) = next_sample(
                &mut self.now,
                &mut self.old,
                &mut self.fade,
                self.fade_step,
                &mut self.decimator,
                &cx,
            );
            if self.now.is_sounding() || self.fade > 0.0 {
                self.tail = DECIMATOR_TAIL;
            } else {
                self.tail = self.tail.saturating_sub(1);
            }
            match channels {
                0 => {}
                1 => buffer.channel_mut(0)[frame] += S::from_f64(0.5 * (left + right)),
                _ => {
                    buffer.channel_mut(0)[frame] += S::from_f64(left);
                    buffer.channel_mut(1)[frame] += S::from_f64(right);
                    for channel in 2..channels {
                        buffer.channel_mut(channel)[frame] += S::from_f64(0.5 * (left + right));
                    }
                }
            }
        }
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if !value.is_finite() {
            return;
        }
        let tone = &mut self.now;
        match expression {
            Expression::Volume => tone.volume = value.clamp(0.0, 4.0),
            Expression::Pan => tone.pan = value.clamp(-1.0, 1.0),
            Expression::Tuning => tone.tuning = value.clamp(-48.0, 48.0),
            Expression::Vibrato => tone.vibrato = value.clamp(0.0, 1.0),
            Expression::Brightness => tone.brightness = value.clamp(0.0, 1.0),
            Expression::Pressure => tone.pressure = value.clamp(0.0, 1.0),
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.shared.bend = semitones.clamp(-48.0, 48.0);
        }
    }

    fn midi(&mut self, message: MidiMessage) {
        let unit = |value: u8| f64::from(value.min(127)) / 127.0;
        let key = self.shared.key;
        let controls = &mut self.shared.controls;
        match message {
            MidiMessage::ControlChange {
                controller: 1,
                value,
                ..
            } => controls.mod_wheel = unit(value),
            MidiMessage::ControlChange {
                controller: 74,
                value,
                ..
            } => controls.brightness = unit(value),
            MidiMessage::ControlChange {
                controller: 121, ..
            } => *controls = Controls::default(),
            MidiMessage::ChannelPressure { pressure, .. } => {
                controls.aftertouch = unit(pressure);
            }
            MidiMessage::PolyPressure { note, pressure, .. } if note == key => {
                controls.key_pressure = unit(pressure);
            }
            _ => {}
        }
    }
}
