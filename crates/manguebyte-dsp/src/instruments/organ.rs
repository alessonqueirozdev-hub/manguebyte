//! A tonewheel organ: nine drawbars, percussion, key click and leakage.

use core::f64::consts::TAU;
use core::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use manguebyte_core::{AudioBuffer, Sample};

use super::common::{
    Handover, Noise, Tone, add_to_every_channel, db, flush, key_hz, setting, t60_factor,
};
use crate::event::{Expression, NoteRef};
use crate::tuning::Tuning;
use crate::{ProcessSpec, Processor, StateVariableFilter, SvfMode, Voice};

/// How many drawbars there are.
pub const DRAWBARS: usize = 9;

/// The drawbars' footages, from the lowest to the highest. 8' is the
/// pitch of the key; the others are its sub-octave, fifth, octave, twelfth,
/// fifteenth, seventeenth, nineteenth and twenty-second.
pub const DRAWBAR_FOOTAGES: [&str; DRAWBARS] = [
    "16'", "5 1/3'", "8'", "4'", "2 2/3'", "2'", "1 3/5'", "1 1/3'", "1'",
];

/// Each drawbar's pitch above the key, in semitones.
///
/// A tonewheel organ takes every drawbar's tone from the wheel of the
/// tempered note nearest the harmonic, so the fifths and the third are not
/// pure: the 2 2/3' is 19 semitones up (2.9966 times the frequency, not 3),
/// the 1 3/5' 28 (5.0397, not 5), the 1 1/3' 31 (5.9932, not 6). Those small
/// mistunings make the organ's slow, living beat.
const DRAWBAR_SEMITONES: [f64; DRAWBARS] = [-12.0, 7.0, 0.0, 12.0, 19.0, 24.0, 28.0, 31.0, 36.0];

/// The lowest and highest tonewheels, as key numbers: C1, 32.7 Hz, and
/// F#8, 5.92 kHz. A drawbar's tone beyond them folds back by octaves into
/// their range, as on the instrument, which also keeps every tone well
/// below half the sample rate.
const LOWEST_WHEEL: f64 = 24.0;
const HIGHEST_WHEEL: f64 = 114.0;

/// The 1' drawbar, whose key contact the percussion takes over.
const ONE_FOOT: usize = 8;

/// How far apart in time the nine contacts of a key close.
const CONTACT_SPREAD_SECONDS: f64 = 0.0015;

/// How long a contact takes to close with no click. With a click `c` it
/// takes `(1 - c)³` of that: 0.4 ms at a half, a sample from about 0.8 up.
const SMOOTH_CONTACT_SECONDS: f64 = 0.003;

/// How long the burst of contact noise of a click takes to fall 60 dB.
const CLICK_NOISE_SECONDS: f64 = 0.012;

/// How soon after the first key of a chord another must be struck to share
/// its percussion.
const CHORD_SECONDS: f64 = 0.03;

/// The percussion's decay times, to 60 dB down.
const FAST_PERCUSSION_SECONDS: f64 = 0.45;
const SLOW_PERCUSSION_SECONDS: f64 = 1.4;

/// What the leakage of neighbouring wheels comes to at a leakage of 1,
/// against a full drawbar.
const LEAKAGE_DB: f64 = -40.0;

/// How long a change of drawbars takes to be heard in full.
const DRAWBAR_GLIDE_SECONDS: f64 = 0.01;

/// Which harmonic the percussion sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PercussionHarmonic {
    /// The second harmonic, an octave up: the 4' wheel.
    Second,
    /// The third harmonic, a twelfth up: the 2 2/3' wheel.
    Third,
}

/// How fast the percussion dies away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PercussionDecay {
    /// 60 dB down in 0.45 s.
    Fast,
    /// 60 dB down in 1.4 s.
    Slow,
}

/// The percussion: a harmonic that sounds at the start of a note and dies
/// away, as on the classic console organ.
///
/// It is single-triggered: a note gets it when no other key is held, or
/// within 30 ms of such a note (a chord struck together), and a note
/// played legato into held keys gets none, until every key has been let
/// go. The voices of an organ know about each other's keys through an
/// [`OrganManual`] they share. While the percussion is on, the 1' drawbar
/// is silent, its contact taken over by the percussion, and at normal
/// volume the other drawbars are 3 dB down, as on the instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Percussion {
    /// The harmonic it sounds.
    pub harmonic: PercussionHarmonic,
    /// How fast it dies away.
    pub decay: PercussionDecay,
    /// At soft volume the percussion is 6 dB quieter and the drawbars keep
    /// their level.
    pub soft: bool,
}

/// The settings of an [`OrganVoice`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OrganParams {
    /// The nine drawbars, 0 (in) to 8 (all the way out), in the order of
    /// [`DRAWBAR_FOOTAGES`]. Each step is 3 dB; fractions are allowed.
    pub drawbars: [f64; DRAWBARS],
    /// The percussion, or `None`.
    pub percussion: Option<Percussion>,
    /// How much the keys click as they are pressed and released, 0 to 1:
    /// how sharply the contacts close and how loud their noise is.
    pub click: f64,
    /// How much of the neighbouring wheels leaks into every note, 0 to 1:
    /// faint tones a semitone off, the dirt of an old instrument.
    pub leakage: f64,
    /// How much the level follows the velocity, 0 to 1. The instrument has
    /// none; at 1 the lowest velocity is 24 dB down.
    pub velocity: f64,
    /// How much louder an accented note is, in decibels, 0 to 24. An
    /// accented note also clicks harder.
    pub accent_db: f64,
    /// The output level, as a gain, 0 to 4. At the default of 0.06 a note
    /// with three drawbars all the way out peaks near -15 dBFS, so that
    /// chords of five or six notes have room.
    pub level: f64,
}

impl OrganParams {
    /// A registration from the nine digits drawbar players write, such as
    /// `"888000000"`, with no percussion, a little click and leakage, and
    /// the default level. Characters that are not digits count as 0, and
    /// 9 as 8.
    pub fn registration(digits: &str) -> Self {
        let mut drawbars = [0.0; DRAWBARS];
        for (drawbar, digit) in drawbars.iter_mut().zip(digits.chars()) {
            *drawbar = f64::from(digit.to_digit(10).unwrap_or(0).min(8));
        }
        Self {
            drawbars,
            percussion: None,
            click: 0.4,
            leakage: 0.4,
            velocity: 0.0,
            accent_db: 2.0,
            level: 0.06,
        }
    }

    /// The jazz organ: 888000000 with fast, third-harmonic percussion at
    /// normal volume.
    pub fn jazz() -> Self {
        Self {
            percussion: Some(Percussion {
                harmonic: PercussionHarmonic::Third,
                decay: PercussionDecay::Fast,
                soft: false,
            }),
            click: 0.5,
            ..Self::registration("888000000")
        }
    }

    /// A gospel organ: 888800000 with fast, second-harmonic percussion and
    /// a strong click.
    pub fn gospel() -> Self {
        Self {
            percussion: Some(Percussion {
                harmonic: PercussionHarmonic::Second,
                decay: PercussionDecay::Fast,
                soft: false,
            }),
            click: 0.6,
            ..Self::registration("888800000")
        }
    }

    /// Every drawbar out, for full organ and rock.
    pub fn full() -> Self {
        Self {
            click: 0.7,
            leakage: 0.6,
            level: 0.03,
            ..Self::registration("888888888")
        }
    }

    /// A soft, round ballad registration, 838000000, with hardly a click.
    pub fn ballad() -> Self {
        Self {
            click: 0.15,
            ..Self::registration("838000000")
        }
    }

    fn limited(self) -> Self {
        Self {
            drawbars: self.drawbars.map(|drawbar| setting(drawbar, 0.0, 8.0, 0.0)),
            percussion: self.percussion,
            click: setting(self.click, 0.0, 1.0, 0.4),
            leakage: setting(self.leakage, 0.0, 1.0, 0.4),
            velocity: setting(self.velocity, 0.0, 1.0, 0.0),
            accent_db: setting(self.accent_db, 0.0, 24.0, 2.0),
            level: setting(self.level, 0.0, 4.0, 0.06),
        }
    }

    /// Each drawbar's gain, with what the percussion takes from them.
    fn drawbar_gains(&self) -> [f64; DRAWBARS] {
        let mut gains = self.drawbars.map(|drawbar| {
            if drawbar > 0.0 {
                db(-3.0 * (8.0 - drawbar))
            } else {
                0.0
            }
        });
        if let Some(percussion) = self.percussion {
            gains[ONE_FOOT] = 0.0;
            if !percussion.soft {
                gains.iter_mut().for_each(|gain| *gain *= db(-3.0));
            }
        }
        gains
    }
}

impl Default for OrganParams {
    fn default() -> Self {
        Self::jazz()
    }
}

#[derive(Debug, Default)]
struct ManualState {
    /// How many keys are held, over every voice.
    held: AtomicU32,
    /// How many frames ago the percussion was triggered, or `u32::MAX`
    /// once a chord can no longer join it.
    since_trigger: AtomicU32,
}

/// The keyboard the voices of one organ share, so that the percussion can
/// be single-triggered: a voice must know whether other keys are held.
///
/// Give every voice of a [`Synth`](crate::Synth) a clone of the same
/// manual, or build them with [`OrganVoice::voices`]. A voice with a
/// manual of its own triggers its percussion on every note. The manual is
/// two atomics, so the voices stay `Send` and nothing locks.
#[derive(Debug, Clone)]
pub struct OrganManual(Arc<ManualState>);

impl OrganManual {
    /// A manual with no key down. Allocates.
    pub fn new() -> Self {
        Self(Arc::new(ManualState {
            held: AtomicU32::new(0),
            since_trigger: AtomicU32::new(u32::MAX),
        }))
    }

    /// How many keys are held on the manual.
    pub fn held_keys(&self) -> u32 {
        self.0.held.load(Ordering::Relaxed)
    }
}

impl Default for OrganManual {
    fn default() -> Self {
        Self::new()
    }
}

/// A sine kept as a turning pair of numbers: two multiplications and a
/// correction of the length per sample instead of a sine.
#[derive(Debug, Clone, Copy)]
struct Wheel {
    cos: f64,
    sin: f64,
    turn_cos: f64,
    turn_sin: f64,
}

impl Wheel {
    const STILL: Self = Self {
        cos: 1.0,
        sin: 0.0,
        turn_cos: 1.0,
        turn_sin: 0.0,
    };

    /// Starts the wheel at `phase` (in turns) turning at `frequency`.
    fn start(&mut self, frequency: f64, phase: f64, sample_rate: f64) {
        (self.sin, self.cos) = (TAU * phase).sin_cos();
        self.retune(frequency, sample_rate);
    }

    fn retune(&mut self, frequency: f64, sample_rate: f64) {
        (self.turn_sin, self.turn_cos) = (TAU * frequency / sample_rate).sin_cos();
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let value = self.sin;
        let cos = self.cos * self.turn_cos - self.sin * self.turn_sin;
        let sin = self.sin * self.turn_cos + self.cos * self.turn_sin;
        // Keeps the pair on the unit circle: rounding would otherwise make
        // the tone grow or fade over minutes.
        let correction = 1.5 - 0.5 * (cos * cos + sin * sin);
        self.cos = cos * correction;
        self.sin = sin * correction;
        value
    }
}

/// The wheel a drawbar of a key plays, folded into the generator's range.
fn wheel_key(key: f64, semitones: f64) -> f64 {
    let mut wheel = key + semitones;
    while wheel > HIGHEST_WHEEL {
        wheel -= 12.0;
    }
    while wheel < LOWEST_WHEEL {
        wheel += 12.0;
    }
    wheel
}

/// Where a wheel is in its turn when a key starts it: fixed for each
/// wheel, so that notes are repeatable.
fn wheel_phase(wheel: f64) -> f64 {
    Noise::for_note((wheel.round() as i64).clamp(0, 255) as u8, 0.0, 7).unit()
}

/// A key contact: it closes some time after the key goes down, moving to
/// full over a ramp, and opens the same way.
#[derive(Debug, Clone, Copy)]
struct Contact {
    wait: u32,
    value: f64,
    step: f64,
    closing: bool,
}

impl Contact {
    const OPEN: Self = Self {
        wait: 0,
        value: 0.0,
        step: 1.0,
        closing: false,
    };

    fn press(&mut self, wait: u32, step: f64) {
        self.wait = wait;
        self.step = step;
        self.closing = true;
    }

    fn release(&mut self, wait: u32) {
        self.wait = wait;
        self.closing = false;
    }

    #[inline]
    fn next(&mut self) -> f64 {
        if self.wait > 0 {
            self.wait -= 1;
        } else if self.closing {
            self.value = (self.value + self.step).min(1.0);
        } else {
            self.value = (self.value - self.step).max(0.0);
        }
        self.value
    }

    fn is_open(&self) -> bool {
        !self.closing && self.value == 0.0
    }
}

/// One organ note.
#[derive(Debug, Clone)]
struct OrganTone {
    sample_rate: f64,
    wheels: [Wheel; DRAWBARS],
    contacts: [Contact; DRAWBARS],
    /// The drawbars' gains as heard, gliding to the settings'.
    gains: [f64; DRAWBARS],
    targets: [f64; DRAWBARS],
    glide: f64,
    leaks: [Wheel; 2],
    leak_gain: f64,
    /// The percussion's level, its fall per sample, and which drawbar's
    /// wheel it sounds.
    percussion: f64,
    percussion_fall: f64,
    percussion_wheel: usize,
    percussion_gain: f64,
    /// The click's noise.
    noise: Noise,
    noise_level: f64,
    noise_fall: f64,
    noise_filter: StateVariableFilter<f64>,
    /// The note's gain from velocity and accent, and the level setting and
    /// the level heard, which glides to it as the drawbars do.
    gain: f64,
    level: f64,
    level_now: f64,
    /// The key, without bend.
    key: f64,
}

impl OrganTone {
    fn new() -> Self {
        Self {
            sample_rate: 48_000.0,
            wheels: [Wheel::STILL; DRAWBARS],
            contacts: [Contact::OPEN; DRAWBARS],
            gains: [0.0; DRAWBARS],
            targets: [0.0; DRAWBARS],
            glide: 1.0,
            leaks: [Wheel::STILL; 2],
            leak_gain: 0.0,
            percussion: 0.0,
            percussion_fall: 0.0,
            percussion_wheel: 4,
            percussion_gain: 0.0,
            noise: Noise::for_note(0, 0.0, 0),
            noise_level: 0.0,
            noise_fall: 0.0,
            noise_filter: StateVariableFilter::new(SvfMode::Bandpass, 3_000.0, 0.8),
            gain: 0.0,
            level: 0.0,
            level_now: 0.0,
            key: 60.0,
        }
    }

    fn prepare(&mut self, sample_rate: f64) {
        self.sample_rate = sample_rate;
        Processor::<f64>::prepare(
            &mut self.noise_filter,
            &ProcessSpec {
                sample_rate,
                max_block_size: 1,
                num_channels: 1,
            },
        );
        self.glide = 1.0 - t60_factor(DRAWBAR_GLIDE_SECONDS, sample_rate);
        self.noise_fall = t60_factor(CLICK_NOISE_SECONDS, sample_rate);
        self.silence();
    }

    fn configure(&mut self, params: &OrganParams) {
        self.targets = params.drawbar_gains();
        self.leak_gain = params.leakage * db(LEAKAGE_DB);
        self.level = params.level;
        if let Some(percussion) = params.percussion {
            self.percussion_wheel = match percussion.harmonic {
                PercussionHarmonic::Second => 3,
                PercussionHarmonic::Third => 4,
            };
            self.percussion_gain = if percussion.soft { db(-6.0) } else { 1.0 };
            let seconds = match percussion.decay {
                PercussionDecay::Fast => FAST_PERCUSSION_SECONDS,
                PercussionDecay::Slow => SLOW_PERCUSSION_SECONDS,
            };
            self.percussion_fall = t60_factor(seconds, self.sample_rate);
        } else {
            self.percussion_gain = 0.0;
        }
    }

    /// Presses the key. `percussion` is the level its percussion starts
    /// at: 1 for a note that triggers it, less for one that joins a chord,
    /// 0 for none.
    fn press(
        &mut self,
        params: &OrganParams,
        key: f64,
        bend: f64,
        gain: f64,
        click: f64,
        percussion: f64,
    ) {
        let sounding = self.is_sounding();
        self.key = key;
        self.gain = gain;
        self.configure(params);
        if !sounding {
            // A key at rest: every wheel takes up its own phase, and the
            // drawbars start where they are set.
            let rate = self.sample_rate;
            for (wheel, semitones) in self.wheels.iter_mut().zip(DRAWBAR_SEMITONES) {
                let at = wheel_key(key, semitones);
                wheel.start(key_hz(at + bend), wheel_phase(at), rate);
            }
            for (leak, semitones) in self.leaks.iter_mut().zip([1.0, 13.0]) {
                let at = wheel_key(key, semitones);
                leak.start(key_hz(at + bend), wheel_phase(at + 0.5), rate);
            }
            self.gains = self.targets;
            self.level_now = self.level;
        }
        // The contacts close one after another, at moments fixed for the
        // key, sharply or smoothly as the click says.
        let rate = self.sample_rate;
        let ramp = SMOOTH_CONTACT_SECONDS * (1.0 - click).powi(3);
        let step = 1.0 / (ramp * rate).max(1.0);
        let mut timing = Noise::for_note(key.round().clamp(0.0, 127.0) as u8, 0.0, 3);
        for contact in &mut self.contacts {
            let wait = (timing.unit() * CONTACT_SPREAD_SECONDS * rate) as u32;
            contact.press(wait, step);
        }
        if percussion > 0.0 {
            self.percussion = percussion;
        }
        self.noise = Noise::for_note(key.round().clamp(0.0, 127.0) as u8, 0.0, 5);
        self.noise_level = click * click * 0.25;
    }

    fn release(&mut self) {
        let mut timing = Noise::for_note(self.key.round().clamp(0.0, 127.0) as u8, 0.0, 4);
        let rate = self.sample_rate;
        for contact in &mut self.contacts {
            let wait = (timing.unit() * CONTACT_SPREAD_SECONDS * rate) as u32;
            contact.release(wait);
        }
    }

    fn retune(&mut self, bend: f64) {
        let rate = self.sample_rate;
        for (wheel, semitones) in self.wheels.iter_mut().zip(DRAWBAR_SEMITONES) {
            wheel.retune(key_hz(wheel_key(self.key, semitones) + bend), rate);
        }
        for (leak, semitones) in self.leaks.iter_mut().zip([1.0, 13.0]) {
            leak.retune(key_hz(wheel_key(self.key, semitones) + bend), rate);
        }
    }

    #[inline]
    fn next(&mut self) -> f64 {
        let mut sum = 0.0;
        let mut wheel_values = [0.0; DRAWBARS];
        let mut contact_sum = 0.0;
        let drawbars = self
            .wheels
            .iter_mut()
            .zip(&mut self.contacts)
            .zip(self.gains.iter_mut().zip(&self.targets))
            .zip(&mut wheel_values);
        for (((wheel, contact), (gain, target)), heard) in drawbars {
            let value = wheel.next();
            *heard = value;
            let contact = contact.next();
            contact_sum += contact;
            *gain += (target - *gain) * self.glide;
            sum += *gain * contact * value;
        }
        if self.percussion > 0.0 {
            sum += self.percussion
                * self.percussion_gain
                * self.contacts[ONE_FOOT].value
                * wheel_values[self.percussion_wheel];
            self.percussion = flush(self.percussion * self.percussion_fall);
        }
        let leak = self.leaks[0].next() + self.leaks[1].next();
        sum += self.leak_gain * leak * contact_sum / DRAWBARS as f64;
        if self.noise_level > 0.0 {
            let noise = self.noise.next();
            sum += self.noise_level * self.noise_filter.process_sample(0, noise);
            self.noise_level = flush(self.noise_level * self.noise_fall);
        }
        self.level_now += (self.level - self.level_now) * self.glide;
        sum * self.gain * self.level_now
    }
}

impl Tone for OrganTone {
    fn is_sounding(&self) -> bool {
        self.contacts.iter().any(|contact| !contact.is_open()) || self.noise_level > 0.0
    }

    fn silence(&mut self) {
        self.contacts = [Contact::OPEN; DRAWBARS];
        self.percussion = 0.0;
        self.noise_level = 0.0;
        Processor::<f64>::reset(&mut self.noise_filter);
    }
}

/// A tonewheel organ: a voice for [`Synth`](crate::Synth) with nine
/// drawbars, single-triggered percussion, key click and leakage.
///
/// Each drawbar adds a sine at its footage ([`DRAWBAR_FOOTAGES`]), taken,
/// as on the instrument, from the tempered note nearest the harmonic, and
/// folded back by octaves at both ends of the tonewheel generator's range,
/// C1 to F#8. Every tone is a sine below 6 kHz, so the organ cannot alias.
///
/// - **Key click.** A key's nine contacts close at slightly different
///   moments, within 1.5 ms, fixed for each key. With no click each closes
///   over 3 ms; with full click at once, so the tones start with the step
///   of a wheel caught mid-turn, and a burst of contact noise sounds with
///   them. Releasing the key opens the contacts the same way.
/// - **Percussion** ([`Percussion`]): the second or third harmonic, fast or
///   slow, single-triggered across the voices that share an
///   [`OrganManual`].
/// - **Leakage**: two wheels a semitone above the 8' and 4' sound faintly
///   with every key, up to 40 dB below a full drawbar.
/// - **Velocity** changes nothing by default, as on the instrument; see
///   [`OrganParams::velocity`]. **An accent** is louder and clicks harder.
///
/// Drawbars moved while a note sounds glide over 10 ms, so registration
/// changes do not click. A voice taken over for a new note hands its old
/// note to a fade over [`HANDOVER_SECONDS`](super::HANDOVER_SECONDS). Pitch
/// bend and [`Expression::Tuning`] retune every wheel of the note. For the
/// rotating speaker the organ is usually heard through, put a
/// [`RotarySpeaker`](super::RotarySpeaker) after the synth.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::instruments::{OrganParams, OrganVoice};
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
///
/// // Sixteen voices sharing one manual, so the percussion is single-triggered.
/// let mut organ = Synth::new(OrganVoice::voices(16, OrganParams::jazz()));
/// organ.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
/// for key in [48, 55, 64, 67] {
///     organ.note_on(NoteRef::new(0, key), 1.0);
/// }
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// organ.process(&mut block);
/// assert!(block.peak() > 0.0);
/// ```
#[derive(Debug, Clone)]
pub struct OrganVoice {
    params: OrganParams,
    manual: OrganManual,
    tones: Handover<OrganTone>,
    /// Whether this voice's key is down, as counted on the manual.
    held: bool,
    /// Whether this voice triggered the percussion that a chord can still
    /// join, and how many frames ago.
    owner: bool,
    age: u32,
    chord_frames: u32,
    sample_rate: f64,
    bend: f64,
    tuning: f64,
    /// The tuning of the keys, from `Synth::set_tuning`.
    key_tuning: Tuning,
}

impl OrganVoice {
    /// A voice with a manual of its own: its percussion triggers on every
    /// note. Use [`with_manual`](Self::with_manual) or
    /// [`voices`](Self::voices) for single-triggered percussion.
    pub fn new(params: OrganParams) -> Self {
        Self::with_manual(params, &OrganManual::new())
    }

    /// A voice on the given manual. Every voice of one organ should share
    /// it.
    pub fn with_manual(params: OrganParams, manual: &OrganManual) -> Self {
        let mut voice = Self {
            params: params.limited(),
            manual: manual.clone(),
            tones: Handover::new(OrganTone::new(), OrganTone::new()),
            held: false,
            owner: false,
            age: 0,
            chord_frames: 0,
            sample_rate: 48_000.0,
            bend: 0.0,
            tuning: 0.0,
            key_tuning: Tuning::default(),
        };
        let params = voice.params;
        voice.tones.now.configure(&params);
        voice.tones.old.configure(&params);
        voice
    }

    /// `count` voices sharing one new manual, for a [`Synth`](crate::Synth).
    /// Allocates.
    pub fn voices(count: usize, params: OrganParams) -> Vec<Self> {
        let manual = OrganManual::new();
        (0..count)
            .map(|_| Self::with_manual(params, &manual))
            .collect()
    }

    /// Takes new settings, limited to their ranges. Real-time safe. The
    /// drawbars of a sounding note glide to their new positions.
    pub fn set_params(&mut self, params: OrganParams) {
        self.params = params.limited();
        let params = self.params;
        self.tones.now.configure(&params);
        self.tones.old.configure(&params);
    }

    /// The settings, as limited.
    pub fn params(&self) -> &OrganParams {
        &self.params
    }

    /// The manual this voice plays on.
    pub fn manual(&self) -> &OrganManual {
        &self.manual
    }

    fn let_go(&mut self) {
        if self.held {
            self.held = false;
            self.manual.0.held.fetch_sub(1, Ordering::Relaxed);
        }
        if self.owner {
            // A chord can join the percussion only while its first key is
            // still down.
            self.owner = false;
            self.manual
                .0
                .since_trigger
                .store(u32::MAX, Ordering::Relaxed);
        }
    }
}

impl<S: Sample> Voice<S> for OrganVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        self.sample_rate = spec.sample_rate;
        self.chord_frames = (CHORD_SECONDS * spec.sample_rate) as u32;
        self.tones.prepare(spec.sample_rate);
        let params = self.params;
        for tone in [&mut self.tones.now, &mut self.tones.old] {
            tone.prepare(spec.sample_rate);
            tone.configure(&params);
        }
    }

    fn set_tuning(&mut self, tuning: &Tuning) {
        self.key_tuning.clone_from(tuning);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        Voice::<S>::note_on_accented(self, note, velocity, false);
    }

    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let params = self.params;
        let state = &self.manual.0;
        if !self.held {
            self.held = true;
            state.held.fetch_add(1, Ordering::Relaxed);
        }
        let others = state.held.load(Ordering::Relaxed).saturating_sub(1);
        let mut percussion = 0.0;
        if params.percussion.is_some() {
            let since = state.since_trigger.load(Ordering::Relaxed);
            if others == 0 {
                // The first key down: it triggers the percussion.
                percussion = 1.0;
                self.owner = true;
                self.age = 0;
                state.since_trigger.store(0, Ordering::Relaxed);
            } else if since <= self.chord_frames {
                // Part of the chord the first key began: its percussion,
                // already a little way into its fall.
                let fall = self.tones.now.percussion_fall;
                percussion = fall.powi(since as i32);
            }
        }
        let velocity = f64::from(velocity).clamp(0.0, 1.0);
        let mut gain = db(-24.0 * params.velocity * (1.0 - velocity));
        let mut click = params.click;
        if accent {
            gain *= db(params.accent_db);
            click = (click + 0.3).min(1.0);
        }
        let key = self.key_tuning.key_pitch(note.key.min(127));
        let bend = self.bend + self.tuning;
        self.tones
            .now
            .press(&params, key, bend, gain, click, percussion);
    }

    fn note_off(&mut self, _velocity: f32) {
        self.let_go();
        self.tones.now.release();
    }

    fn choke(&mut self) {
        self.let_go();
        self.tones.choke();
        self.tuning = 0.0;
    }

    fn is_active(&self) -> bool {
        self.tones.is_active()
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        let frames = buffer.num_frames() as u32;
        if self.owner {
            self.age = self.age.saturating_add(frames);
            let age = if self.age > self.chord_frames {
                u32::MAX
            } else {
                self.age
            };
            self.manual.0.since_trigger.store(age, Ordering::Relaxed);
        }
        if !self.tones.is_active() {
            return;
        }
        let tones = &mut self.tones;
        add_to_every_channel(buffer, || tones.next(OrganTone::next));
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        if expression == Expression::Tuning && value.is_finite() {
            self.tuning = value.clamp(-48.0, 48.0);
            self.tones.now.retune(self.bend + self.tuning);
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        if semitones.is_finite() {
            self.bend = semitones.clamp(-48.0, 48.0);
            self.tones.now.retune(self.bend + self.tuning);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instruments::measure::{
        Act, RATE, accented, amplitude_at, between, cents, decibels, finite_within, frequency_near,
        largest_jump, note, off, on, peak, play, rms, synth_of, t60_at,
    };

    fn voice(params: OrganParams) -> [OrganVoice; 1] {
        [OrganVoice::new(params)]
    }

    #[test]
    fn every_drawbar_is_in_tune_across_the_keyboard() {
        // The 8' alone is the key's own pitch; the others are the tempered
        // intervals above it, folded back at the top.
        let mut worst = 0.0f64;
        for (drawbar, semitones) in DRAWBAR_SEMITONES.iter().enumerate() {
            let mut params = OrganParams::registration("000000000");
            params.drawbars[drawbar] = 8.0;
            params.leakage = 0.0;
            params.click = 0.0;
            for key in [36u8, 48, 60, 72, 84, 96] {
                let signal = note(voice(params), on(key, 1.0), 0.5, 0.5);
                let expected = key_hz(wheel_key(f64::from(key), *semitones));
                let measured = frequency_near(between(&signal, 0.05, 0.45), expected);
                let error = cents(measured, expected);
                worst = worst.max(error.abs());
                assert!(
                    error.abs() < 0.1,
                    "{} key {key}: {error} cents",
                    DRAWBAR_FOOTAGES[drawbar]
                );
            }
        }
        println!("worst {worst:.5} cents");
        // The fold back keeps the top of the keyboard under 6 kHz.
        assert_eq!(wheel_key(96.0, 36.0), 108.0);
        assert_eq!(wheel_key(24.0, -12.0), 24.0);
        assert!(key_hz(HIGHEST_WHEEL) < 6_000.0);
    }

    #[test]
    fn drawbars_are_three_decibels_a_step() {
        let level = |position: f64| {
            let mut params = OrganParams::registration("000000000");
            params.drawbars[2] = position;
            params.leakage = 0.0;
            let signal = note(voice(params), on(60, 1.0), 0.3, 0.3);
            decibels(amplitude_at(between(&signal, 0.05, 0.25), key_hz(60.0)))
        };
        let full = level(8.0);
        for position in [7.0, 4.0, 1.0] {
            let difference = full - level(position);
            assert!(
                (difference - 3.0 * (8.0 - position)).abs() < 0.05,
                "{position}: {difference}"
            );
        }
    }

    #[test]
    fn percussion_decays_and_is_single_triggered() {
        let params = OrganParams {
            leakage: 0.0,
            click: 0.0,
            ..OrganParams::jazz()
        };
        let third = |signal: &[f64], from: f64, to: f64, key: u8| {
            amplitude_at(between(signal, from, to), key_hz(f64::from(key) + 19.0))
        };
        // Alone, the third harmonic rises above the drawbars' and falls in
        // the percussion's time.
        let signal = note(OrganVoice::voices(1, params), on(60, 1.0), 1.0, 1.0);
        let measured = t60_at(&signal, key_hz(79.0), 0.05, 0.4);
        println!("percussion falls 60 dB in {measured:.3} s");
        assert!(
            (measured / FAST_PERCUSSION_SECONDS - 1.0).abs() < 0.05,
            "{measured}"
        );
        assert!(
            third(&signal, 0.8, 0.95, 60) < 1e-3,
            "no drawbar plays the third"
        );

        // A chord struck within 30 ms shares it; a key added later does not,
        // until every key is up.
        let mut synth = synth_of(OrganVoice::voices(4, params));
        let frame = |seconds: f64| (seconds * RATE) as usize;
        let script = [
            (0, on(60, 1.0)),
            (frame(0.01), on(64, 1.0)),
            (frame(0.3), on(67, 1.0)),
            (frame(0.6), off(60)),
            (frame(0.6), off(64)),
            (frame(0.6), off(67)),
            (frame(0.7), on(72, 1.0)),
        ];
        let signal = play(&mut synth, &script, frame(0.9), 128);
        let chord = third(&signal, 0.015, 0.08, 64);
        let late = third(&signal, 0.31, 0.38, 67);
        let after = third(&signal, 0.71, 0.78, 72);
        println!("percussion: chord {chord:.4}, late {late:.5}, after {after:.4}");
        assert!(chord > 0.3 * params.level && after > 0.3 * params.level);
        assert!(late < 0.01 * params.level, "{late}");
        assert_eq!(synth.voices_mut().next().unwrap().manual().held_keys(), 1);

        // Voices on manuals of their own trigger on every note.
        let mut synth = synth_of([OrganVoice::new(params), OrganVoice::new(params)]);
        let signal = play(
            &mut synth,
            &[(0, on(60, 1.0)), (frame(0.3), on(67, 1.0))],
            frame(0.4),
            128,
        );
        assert!(third(&signal, 0.31, 0.38, 67) > 0.3 * params.level);
    }

    #[test]
    fn velocity_and_accent() {
        let params = OrganParams {
            velocity: 1.0,
            ..OrganParams::ballad()
        };
        let loud = note(voice(params), on(60, 1.0), 0.3, 0.3);
        let soft = note(voice(params), on(60, 0.5), 0.3, 0.3);
        let difference = decibels(rms(&loud) / rms(&soft));
        assert!((difference - 12.0).abs() < 0.1, "{difference}");
        // By default the organ ignores velocity, as the instrument does.
        let plain = OrganParams::ballad();
        let a = note(voice(plain), on(60, 1.0), 0.3, 0.3);
        let b = note(voice(plain), on(60, 0.2), 0.3, 0.3);
        assert_eq!(a, b);
        // An accent is louder by its setting, and clicks harder.
        let strong = note(voice(plain), accented(60, 1.0), 0.3, 0.3);
        let accent = decibels(rms(between(&strong, 0.05, 0.3)) / rms(between(&a, 0.05, 0.3)));
        assert!((accent - plain.accent_db).abs() < 0.1, "{accent}");
        assert!(largest_jump(between(&strong, 0.0, 0.01)) > largest_jump(between(&a, 0.0, 0.01)));
    }

    #[test]
    fn without_click_notes_start_and_end_smoothly() {
        let params = OrganParams {
            click: 0.0,
            ..OrganParams::full()
        };
        let signal = note(voice(params), on(60, 1.0), 0.3, 0.4);
        let steady = largest_jump(between(&signal, 0.05, 0.3));
        let attack = largest_jump(between(&signal, 0.0, 0.05));
        let release = largest_jump(between(&signal, 0.3, 0.4));
        assert!(
            attack <= steady && release <= steady,
            "{attack} {release} {steady}"
        );
        assert!(
            peak(between(&signal, 0.31, 0.4)) == 0.0,
            "the note has ended"
        );

        // With full click, the start has edges the steady sound has not:
        // heard best on the low drawbars, whose own sound is smooth.
        let low = |click| OrganParams {
            click,
            ..OrganParams::registration("888000000")
        };
        let smooth = note(voice(low(0.0)), on(60, 1.0), 0.3, 0.4);
        let clicky = note(voice(low(1.0)), on(60, 1.0), 0.3, 0.4);
        let low_steady = largest_jump(between(&smooth, 0.05, 0.3));
        assert!(largest_jump(between(&smooth, 0.0, 0.05)) <= low_steady);
        let edges = largest_jump(between(&clicky, 0.0, 0.01));
        println!("click: {edges:.4} against {low_steady:.4}");
        assert!(edges > 4.0 * low_steady);

        // A voice taken over fades its old note out.
        let steal = (0.2 * RATE) as usize;
        let mut synth = synth_of(voice(params));
        let both = play(
            &mut synth,
            &[(0, on(60, 1.0)), (steal, on(48, 1.0))],
            steal * 2,
            256,
        );
        let mut fresh = synth_of(voice(params));
        let second = play(&mut fresh, &[(steal, on(48, 1.0))], steal * 2, 256);
        let residual: Vec<f64> = both.iter().zip(&second).map(|(a, b)| a - b).collect();
        let at_steal = largest_jump(&residual[steal - 64..steal + 512]);
        assert!(at_steal <= 1.05 * steady, "{at_steal} against {steady}");
        assert!(peak(&residual[steal + 400..]) < 1e-12);

        // Moving the drawbars while a note sounds glides.
        let mut synth = synth_of(voice(params));
        let mut signal = play(&mut synth, &[(0, on(60, 1.0))], steal, 256);
        for voice in synth.voices_mut() {
            voice.set_params(OrganParams {
                click: 0.0,
                ..OrganParams::registration("008000000")
            });
        }
        signal.extend(play(&mut synth, &[], steal, 256));
        assert!(largest_jump(&signal[steal - 64..steal + 960]) <= 1.05 * steady);
    }

    #[test]
    fn the_output_does_not_depend_on_the_block_size() {
        let script = [
            (0, on(48, 0.9)),
            (300, accented(55, 0.7)),
            (7_000, on(64, 0.5)),
            (9_001, off(48)),
            (9_500, Act::Bend(0.5)),
            (12_000, on(48, 1.0)),
            (20_000, off(55)),
        ];
        for params in [OrganParams::jazz(), OrganParams::full()] {
            let run = |block| {
                play(
                    &mut synth_of(OrganVoice::voices(3, params)),
                    &script,
                    30_000,
                    block,
                )
            };
            let reference = run(256);
            for block in [1, 7, 64, 1_000] {
                assert!(run(block) == reference, "blocks of {block}");
            }
        }
    }

    #[test]
    fn extreme_settings_and_notes_stay_finite() {
        let wild = OrganParams {
            drawbars: [f64::INFINITY; DRAWBARS],
            percussion: Some(Percussion {
                harmonic: PercussionHarmonic::Second,
                decay: PercussionDecay::Slow,
                soft: false,
            }),
            click: f64::NAN,
            leakage: 1e9,
            velocity: -3.0,
            accent_db: 1e9,
            level: 1e9,
        };
        let mut synth = synth_of(OrganVoice::voices(2, wild));
        let script = [
            (0, accented(0, 1.0)),
            (10, accented(127, 1.0)),
            (20, Act::Bend(48.0)),
            (500, accented(127, 1.0)),
            (900, Act::Bend(-48.0)),
            (2_000, off(0)),
        ];
        let signal = play(&mut synth, &script, 6_000, 100);
        // Two notes, each of nine drawbars, the percussion, the leaks and
        // the click, at level 4 and with the accent's 24 dB.
        assert!(finite_within(&signal, 2.0 * 12.0 * 16.0 * 4.0));
    }
}
