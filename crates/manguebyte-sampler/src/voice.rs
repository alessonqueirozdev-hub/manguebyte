//! The sampler as a [`Voice`] for [`Synth`](manguebyte_dsp::Synth).

use std::sync::Arc;

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_dsp::event::{Expression, NoteRef};
use manguebyte_dsp::{ProcessSpec, Voice};

use crate::instrument::Instrument;
use crate::interpolate::Interpolation;
use crate::player::{Bend, FAST_FADE_SECONDS, Hold, Modulation, Player, Start};
use crate::region::Trigger;

/// Most regions one [`SamplerVoice`] plays at once, the fading ones of a
/// struck-again note and the release-triggered ones included.
pub const MAX_LAYERS: usize = 8;

/// One note of an SFZ instrument, as a [`Voice`] for
/// [`Synth`](manguebyte_dsp::Synth).
///
/// The synth decides which voice plays which note and handles the sustain
/// pedal (it releases a pedalled note when the pedal comes up, which is
/// when this voice starts the note's `trigger=release` regions). The voice
/// plays every region the note calls for, up to [`MAX_LAYERS`], sharing
/// the instrument's round robins and random numbers with the other voices.
///
/// Compared with [`Sampler`](crate::Sampler), which manages its own voices,
/// a voice sees only its own note, so:
///
/// - `off_by` silences only regions of the same note;
/// - `polyphony` is ignored (the synth's voice count is the limit);
/// - `trigger=first` regions always play and `trigger=legato` never do;
/// - the pitch wheel range is the synth's, not `bend_up` and `bend_down`;
/// - controllers stay at their initial values (`set_ccN`, `set_hdccN`):
///   a voice sees no controller messages, so `<opcode>_onccN` modulates
///   with those values only;
/// - the instrument cannot be swapped while playing: build a new synth
///   off the audio thread instead.
///
/// ```
/// use std::sync::Arc;
///
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::NoteRef;
/// use manguebyte_dsp::{ProcessSpec, Processor, Synth};
/// use manguebyte_sampler::{Instrument, MemorySource, SamplerVoice};
///
/// let instrument = Arc::new(Instrument::from_sfz("<region> sample=*sine", &mut MemorySource::new())?);
/// let mut synth = Synth::with_voices(8, || SamplerVoice::new(Arc::clone(&instrument)));
/// synth.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 64, num_channels: 1 });
/// synth.note_on(NoteRef::new(0, 60), 1.0);
/// let mut block = AudioBuffer::<f32>::new(1, 64);
/// synth.process(&mut block);
/// assert!(block.peak() > 0.5);
/// # Ok::<(), manguebyte_sampler::InstrumentError>(())
/// ```
#[derive(Debug, Clone)]
pub struct SamplerVoice {
    instrument: Arc<Instrument>,
    layers: [Player; MAX_LAYERS],
    sample_rate: f64,
    interpolation: Interpolation,
    note: NoteRef,
    velocity: u8,
    /// Frames rendered since the note started.
    held_frames: u64,
    /// Whether the note is down (or held by the pedal, which the synth
    /// handles).
    held: bool,
    /// Pitch ratio and gain from outside, where they stand, what they head
    /// for, and the frames of the way left.
    outside: (f64, f64),
    outside_to: (f64, f64),
    outside_left: u64,
    serial: u64,
    bend_cents: f64,
    volume: f64,
    pan: f64,
    tuning: f64,
}

impl SamplerVoice {
    /// A voice that plays `instrument`.
    pub fn new(instrument: Arc<Instrument>) -> Self {
        Self {
            instrument,
            layers: [const { Player::IDLE }; MAX_LAYERS],
            sample_rate: 48_000.0,
            interpolation: Interpolation::Sinc,
            note: NoteRef::new(0, 0),
            velocity: 0,
            held_frames: 0,
            held: false,
            outside: (1.0, 1.0),
            outside_to: (1.0, 1.0),
            outside_left: 0,
            serial: 0,
            bend_cents: 0.0,
            volume: 1.0,
            pan: 0.0,
            tuning: 0.0,
        }
    }

    /// Chooses how samples are read between their frames.
    pub fn set_interpolation(&mut self, interpolation: Interpolation) {
        self.interpolation = interpolation;
    }

    /// The instrument it plays.
    pub fn instrument(&self) -> &Arc<Instrument> {
        &self.instrument
    }

    /// Sets the pitch ratio and gain a model outside the sampler gives this
    /// voice, as [`Sampler::modulate_key`](crate::Sampler::modulate_key)
    /// does for a key's, reached along a straight line over the next
    /// `frames` frames it renders (at once for 0), and held after them.
    /// A value that is not a number is ignored. Real-time safe.
    pub fn modulate(&mut self, pitch_ratio: f32, gain: f32, frames: usize) {
        if pitch_ratio.is_nan() || gain.is_nan() {
            return;
        }
        self.outside_to = (
            f64::from(pitch_ratio).clamp(1.0 / 65_536.0, 256.0),
            f64::from(gain).max(0.0),
        );
        self.outside_left = frames as u64;
        if frames == 0 {
            self.outside = self.outside_to;
        }
    }

    /// How many regions are sounding.
    pub fn active_layers(&self) -> usize {
        self.layers.iter().filter(|layer| layer.active).count()
    }

    fn start(&mut self, release: Option<Trigger>) {
        let instrument = &*self.instrument;
        let key = i32::from(self.note.key & 0x7F) + instrument.note_shift;
        let Ok(key) = u8::try_from(key) else {
            return;
        };
        let Some(list) = (match release {
            None => instrument.attack_by_key.get(usize::from(key)),
            Some(_) => instrument.release_by_key.get(usize::from(key)),
        }) else {
            return;
        };
        if list.is_empty() {
            return;
        }
        self.serial += 1;
        let random = instrument.next_random();
        let channel = self.note.channel & 0x0F;
        for &index in list {
            let index = index as usize;
            let Some(zone) = instrument.zones.get(index) else {
                continue;
            };
            let wanted = match (release, zone.trigger) {
                (None, Trigger::Attack | Trigger::First) => true,
                (Some(wanted), trigger) => wanted == trigger,
                _ => false,
            };
            let held = self.held_frames as f64 / self.sample_rate;
            if !wanted || !zone.accepts(channel, key, self.velocity) {
                continue;
            }
            if release.is_some() && !zone.accepts_held(held) {
                continue;
            }
            if !instrument.take_turn(index) || !zone.accepts_random(random) {
                continue;
            }
            for layer in &mut self.layers {
                if layer.active && layer.off_by == Some(zone.group) && layer.serial != self.serial {
                    layer.off(instrument, self.sample_rate);
                }
            }
            if zone.sample.is_none() {
                continue;
            }
            // The loudest sustaining layer, which a release with a
            // crossfade takes over from.
            let sustain = if release.is_some() && zone.release_crossfade > 0.0 {
                self.layers
                    .iter()
                    .enumerate()
                    .filter(|(_, layer)| layer.active && !layer.release_trigger && !layer.dying)
                    .max_by(|a, b| a.1.loudness().total_cmp(&b.1.loudness()))
                    .map(|(at, _)| at)
            } else {
                None
            };
            let start = Start {
                zone: index,
                note: self.note,
                key,
                velocity: self.velocity,
                serial: self.serial,
                age: self.serial,
                bend: Bend::Cents(self.bend_cents),
                release_trigger: release.is_some(),
                attenuation_db: if release.is_some() {
                    zone.rt_decay * held
                } else {
                    0.0
                },
                sample_rate: self.sample_rate,
            };
            let free = self
                .layers
                .iter()
                .enumerate()
                .position(|(at, layer)| !layer.active && Some(at) != sustain);
            let slot = match free {
                Some(free) => free,
                None => {
                    // All busy: the quietest layer gives way.
                    let mut chosen = 0;
                    let mut quietest = f64::INFINITY;
                    for (at, layer) in self.layers.iter().enumerate() {
                        if layer.loudness() < quietest {
                            quietest = layer.loudness();
                            chosen = at;
                        }
                    }
                    chosen
                }
            };
            let upcoming = sustain.map(|at| self.layers[at].upcoming(instrument));
            let layer = &mut self.layers[slot];
            if layer.start(instrument, &start, &*instrument.controller_defaults, 0) {
                layer.set_volume(self.volume);
                layer.set_pan(self.pan);
                layer.set_tuning(self.tuning);
                if let (Some(at), Some(upcoming)) = (sustain.filter(|&at| at != slot), upcoming) {
                    if zone.release_align > 0 {
                        layer.align(instrument, &upcoming, zone.release_align);
                    }
                    layer.fade_in(zone.release_crossfade, self.sample_rate);
                    self.layers[at].hand_over(zone.release_crossfade, self.sample_rate);
                }
            }
        }
    }
}

impl<S: Sample> Voice<S> for SamplerVoice {
    fn prepare(&mut self, spec: &ProcessSpec) {
        if spec.sample_rate.is_finite() && spec.sample_rate > 0.0 {
            self.sample_rate = spec.sample_rate;
        }
        Voice::<S>::choke(self);
    }

    fn note_on(&mut self, note: NoteRef, velocity: f32) {
        // The same key struck again: what still sounds fades out.
        for layer in &mut self.layers {
            if layer.active {
                layer.fade_out(FAST_FADE_SECONDS, self.sample_rate);
            }
        }
        self.note = note;
        self.velocity = (f64::from(velocity) * 127.0).round().clamp(1.0, 127.0) as u8;
        self.held_frames = 0;
        self.held = true;
        self.start(None);
    }

    fn note_off(&mut self, _velocity: f32) {
        for layer in &mut self.layers {
            if layer.active && layer.hold == Hold::Key {
                layer.release(&self.instrument);
            }
        }
        // A note's release regions play once, even if it had no region
        // of its own to hold.
        if core::mem::take(&mut self.held) {
            self.start(Some(Trigger::ReleaseKey));
            self.start(Some(Trigger::Release));
        }
    }

    fn choke(&mut self) {
        for layer in &mut self.layers {
            layer.stop();
        }
        self.note = NoteRef::new(0, 0);
        self.velocity = 0;
        self.held_frames = 0;
        self.held = false;
        self.serial = 0;
        self.bend_cents = 0.0;
        self.volume = 1.0;
        self.pan = 0.0;
        self.tuning = 0.0;
    }

    fn is_active(&self) -> bool {
        self.layers.iter().any(|layer| layer.active)
    }

    fn render(&mut self, buffer: &mut AudioBuffer<S>) {
        let defaults = &*self.instrument.controller_defaults;
        let frames = buffer.num_frames() as u64;
        let modulation = if self.outside == (1.0, 1.0) && self.outside_to == (1.0, 1.0) {
            Modulation::NONE
        } else {
            let left = self.outside_left.max(1) as f64;
            Modulation {
                active: true,
                ratio: [self.outside.0, 1.0],
                ratio_step: [(self.outside_to.0 - self.outside.0) / left, 0.0],
                gain: [self.outside.1, 1.0],
                gain_step: [(self.outside_to.1 - self.outside.1) / left, 0.0],
                offset: 0,
                len: self.outside_left,
            }
        };
        for layer in &mut self.layers {
            layer.render(
                &self.instrument,
                self.interpolation,
                buffer,
                defaults,
                0,
                &modulation,
            );
        }
        // Along the line by this many frames.
        if self.outside_left > 0 {
            let done = frames.min(self.outside_left);
            let part = done as f64 / self.outside_left as f64;
            self.outside.0 += (self.outside_to.0 - self.outside.0) * part;
            self.outside.1 += (self.outside_to.1 - self.outside.1) * part;
            self.outside_left -= done;
        }
        if self.outside_left == 0 {
            self.outside = self.outside_to;
        }
        self.held_frames = self.held_frames.wrapping_add(frames);
    }

    fn expression(&mut self, expression: Expression, value: f64) {
        for layer in &mut self.layers {
            match expression {
                Expression::Volume => layer.set_volume(value),
                Expression::Pan => layer.set_pan(value),
                Expression::Tuning => layer.set_tuning(value),
                _ => {}
            }
        }
        match expression {
            Expression::Volume => self.volume = value,
            Expression::Pan => self.pan = value,
            Expression::Tuning => self.tuning = value,
            _ => {}
        }
    }

    fn pitch_bend(&mut self, semitones: f64) {
        self.bend_cents = semitones * 100.0;
        for layer in &mut self.layers {
            if layer.active {
                layer.set_bend(&self.instrument, Bend::Cents(self.bend_cents));
            }
        }
    }
}
