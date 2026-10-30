//! One region sounding: the part of a voice that reads a sample.

use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_dsp::event::NoteRef;

use crate::controllers::Controls;
use crate::envelope::Envelope;
use crate::filter::VoiceFilter;
use crate::instrument::{
    AMPEG, CUTOFF, FILEG, FILEG_DEPTH, Instrument, MODULATES_AMPEG, MODULATES_FILEG,
    MODULATES_GAIN, MODULATES_PITCH, PITCH, Part, RESONANCE, SampleData, Sums, Zone, stages,
};
use crate::interpolate::{Interpolation, MAX_TAPS, cubic_weights};
use crate::region::OffMode;
use crate::stream::{Lease, MARGIN};

/// Seconds of the fade a voice gets when it is cut short: by a choke group
/// in `off_mode=fast`, by a polyphony limit or by voice stealing. The SFZ
/// format describes `off_mode=fast` as a 6 ms fade.
pub(crate) const FAST_FADE_SECONDS: f64 = 0.006;

/// Frames between the updates of what moves while a voice plays: its
/// filter's cutoff and resonance, and what controllers modulate. Gains and
/// the filter's settings glide in a straight line from one update to the
/// next; the pitch steps.
pub(crate) const TICK: u32 = 16;

/// One in fixed-point positions: 32 bits of fraction.
const ONE: u64 = 1 << 32;

/// Whether the key that started a voice still holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hold {
    /// The key is down.
    Key,
    /// The key is up and the sustain pedal holds the note.
    Pedal,
    /// Released, or never held (a release-triggered voice).
    Released,
}

/// Where the pitch wheel stands, as a voice is told.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Bend {
    /// As MIDI sends it, -8192 to 8191; the region's `bend_up` and
    /// `bend_down` turn it into cents.
    Wheel(i16),
    /// Already in cents.
    Cents(f64),
}

impl Bend {
    fn cents(self, zone: &Zone) -> f64 {
        match self {
            Self::Wheel(value) if value >= 0 => f64::from(value) / 8_191.0 * zone.bend_up,
            Self::Wheel(value) => f64::from(value) / -8_192.0 * zone.bend_down,
            Self::Cents(cents) => cents,
        }
    }
}

/// What a voice needs to know to start.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Start {
    pub(crate) zone: usize,
    pub(crate) note: NoteRef,
    /// The key that chose the region, after the instrument's note offset.
    pub(crate) key: u8,
    /// 1 to 127.
    pub(crate) velocity: u8,
    pub(crate) serial: u64,
    pub(crate) age: u64,
    pub(crate) bend: Bend,
    /// Started by the key coming up.
    pub(crate) release_trigger: bool,
    /// Decibels taken off, from `rt_decay`.
    pub(crate) attenuation_db: f64,
    pub(crate) sample_rate: f64,
}

/// A source of a sample's frames for one tap of the interpolator.
enum Tap {
    Data(usize),
    Tail(usize),
    Zero,
}

/// One region of an instrument being played.
#[derive(Debug, Clone)]
pub(crate) struct Player {
    pub(crate) active: bool,
    pub(crate) zone: usize,
    pub(crate) note: NoteRef,
    pub(crate) hold: Hold,
    /// The note event that started it, so that a choke group does not
    /// silence the layers started with it.
    pub(crate) serial: u64,
    /// Lower is older.
    pub(crate) age: u64,
    pub(crate) group: i64,
    pub(crate) off_by: Option<i64>,
    /// Fading out after being cut short.
    pub(crate) dying: bool,
    position: u64,
    step: u64,
    step_frames: f64,
    looping: bool,
    wrapped: bool,
    /// A sustain loop's key came up: the loop is left at the first frame
    /// that reads the same with it as without it.
    leave_loop: bool,
    /// The position went round the loop after the frame just read.
    just_wrapped: bool,
    envelope: Envelope,
    /// Level the fade under way started from.
    fade_from: f64,
    /// Frames the fade lasts, and frames of it left.
    fade_frames: u64,
    fade_left: u64,
    /// The gains of the note's velocity and of `rt_decay`.
    velocity_gain: f64,
    attenuation: f64,
    /// The region's `volume` and `amplitude`, as controllers modulate them.
    region_gain: f64,
    /// The region's pan, as controllers modulate it, -1 to 1.
    pan: f64,
    expression_volume: f64,
    expression_pan: f64,
    gain_left: f64,
    gain_right: f64,
    /// What is added to the gains per frame while they glide, and the
    /// frames of glide left.
    gain_step_left: f64,
    gain_step_right: f64,
    gain_gliding: u32,
    base_cents: f64,
    bend_cents: f64,
    expression_cents: f64,
    /// Cents controllers add.
    modulation_cents: f64,
    rate_ratio: f64,
    stereo: bool,
    /// The key that chose the region, and the velocity, 0 to 1.
    key: u8,
    velocity: f64,
    sample_rate: f64,
    /// Whether the region has a filter or modulations to update.
    ticking: bool,
    /// The next update is the first, which jumps instead of gliding.
    first_tick: bool,
    /// Frames to the next update.
    tick_left: u32,
    filtered: bool,
    filter: VoiceFilter,
    filter_envelope: Envelope,
    /// The ring a streamed sample's frames arrive in.
    lease: Lease,
    /// The frames it needed had not arrived: it holds the last values it
    /// read and fades out.
    starved: bool,
    held_left: f64,
    held_right: f64,
    /// Started by a key coming up.
    pub(crate) release_trigger: bool,
    /// Its envelopes as they were before its key came up, so that a release
    /// that crossfades from it can take that release back.
    unreleased: Option<(Envelope, Envelope)>,
    /// An equal-power crossfade under way, its length and the frames of it
    /// done.
    crossfade: Crossfade,
    crossfade_frames: u64,
    crossfade_done: u64,
}

/// Which way a voice crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Crossfade {
    None,
    /// Rising as a sine from 0, a release taking over.
    In,
    /// Falling as a cosine to 0, then stopping: a sustain handing over.
    Out,
}

/// Frames compared when a release looks for the start that continues the
/// phase of the sustain it crossfades from.
pub(crate) const ALIGN_FRAMES: usize = 128;

impl Player {
    pub(crate) const IDLE: Self = Self {
        active: false,
        zone: 0,
        note: NoteRef::new(0, 0),
        hold: Hold::Released,
        serial: 0,
        age: 0,
        group: 0,
        off_by: None,
        dying: false,
        position: 0,
        step: ONE,
        step_frames: 1.0,
        looping: false,
        wrapped: false,
        leave_loop: false,
        just_wrapped: false,
        envelope: Envelope::IDLE,
        fade_from: 1.0,
        fade_frames: 1,
        fade_left: 0,
        velocity_gain: 0.0,
        attenuation: 0.0,
        region_gain: 0.0,
        pan: 0.0,
        expression_volume: 1.0,
        expression_pan: 0.0,
        gain_left: 0.0,
        gain_right: 0.0,
        gain_step_left: 0.0,
        gain_step_right: 0.0,
        gain_gliding: 0,
        base_cents: 0.0,
        bend_cents: 0.0,
        expression_cents: 0.0,
        modulation_cents: 0.0,
        rate_ratio: 1.0,
        stereo: false,
        key: 0,
        velocity: 0.0,
        sample_rate: 48_000.0,
        ticking: false,
        first_tick: false,
        tick_left: 0,
        filtered: false,
        filter: VoiceFilter::IDLE,
        filter_envelope: Envelope::IDLE,
        lease: Lease::NONE,
        starved: false,
        held_left: 0.0,
        held_right: 0.0,
        release_trigger: false,
        unreleased: None,
        crossfade: Crossfade::None,
        crossfade_frames: 1,
        crossfade_done: 0,
    };

    /// Starts playing a region, with the controllers in `controls` as they
    /// are at frame `now`. Returns `false`, leaving the player idle, when
    /// the region has nothing to play.
    pub(crate) fn start<C: Controls + ?Sized>(
        &mut self,
        instrument: &Instrument,
        start: &Start,
        controls: &C,
        now: u64,
    ) -> bool {
        *self = Self::IDLE;
        let Some(zone) = instrument.zones.get(start.zone) else {
            return false;
        };
        let Some(sample) = zone.sample.and_then(|index| instrument.samples.get(index)) else {
            return false;
        };
        let velocity = f64::from(start.velocity.clamp(1, 127)) / 127.0;
        let curve = velocity * velocity;
        let velocity_gain = if zone.veltrack >= 0.0 {
            1.0 - zone.veltrack + zone.veltrack * curve
        } else {
            1.0 + zone.veltrack * curve
        };
        self.active = true;
        self.zone = start.zone;
        self.note = start.note;
        self.release_trigger = start.release_trigger;
        self.hold = if start.release_trigger {
            Hold::Released
        } else {
            Hold::Key
        };
        self.serial = start.serial;
        self.age = start.age;
        self.group = zone.group;
        self.off_by = zone.off_by;
        // A region started by a release has no key to hold its loop, so it
        // plays through once.
        self.looping = zone.lp.is_some() && !start.release_trigger;
        let sums = zone.sums(controls, now);
        self.envelope = Envelope::new(
            &zone.envelope,
            velocity,
            start.sample_rate,
            stages(&sums, AMPEG),
        );
        self.velocity_gain = velocity_gain;
        self.attenuation = 10f64.powf(-start.attenuation_db.max(0.0) / 20.0);
        self.region_gain = zone.gain;
        self.pan = zone.pan;
        self.base_cents = (f64::from(start.key) - zone.keycenter) * zone.keytrack + zone.detune;
        self.bend_cents = start.bend.cents(zone);
        self.rate_ratio = sample.rate / start.sample_rate.max(1.0);
        self.stereo = sample.stereo;
        self.key = start.key;
        self.velocity = velocity;
        self.sample_rate = start.sample_rate;
        self.ticking = zone.ticks();
        self.first_tick = true;
        self.tick_left = 0;
        if let Some(filter) = &zone.filter {
            self.filtered = true;
            self.filter = VoiceFilter::new(filter.kind);
            self.filter_envelope = Envelope::new(
                &filter.envelope,
                velocity,
                start.sample_rate,
                stages(&sums, FILEG),
            );
        }
        self.position = zone.offset << 32;
        self.wrap(zone);
        if let (true, Some(pool), Some(index)) = (
            !sample.is_resident() || sample.is_packed(),
            &instrument.pool,
            zone.sample,
        ) {
            // The frames after the stretch in memory it starts in are asked
            // for at once (and a packed sample unpacks into the slot).
            let at = self.position >> 32;
            let from = sample
                .part_at(at)
                .map_or(at.saturating_sub(MARGIN), Part::end);
            if let Some(lease) = Lease::claim(pool, index as u32, from) {
                self.lease = lease;
            }
        }
        self.modulate(zone, &sums, 0.0, 0);
        self.update_pitch();
        self.update_gain();
        true
    }

    /// Takes what controllers add, `sums`, and the filter envelope's level:
    /// at once when `glide` is 0, and otherwise gliding to it over `glide`
    /// frames.
    fn modulate(&mut self, zone: &Zone, sums: &Sums, filter_level: f64, glide: u32) {
        if zone.modulates & MODULATES_GAIN != 0 {
            self.region_gain = zone.gain_with(sums);
            self.pan = zone.pan_with(sums);
            if glide == 0 {
                self.update_gain();
            } else {
                let (left, right) = self.gains();
                let frames = f64::from(glide);
                self.gain_step_left = (left - self.gain_left) / frames;
                self.gain_step_right = (right - self.gain_right) / frames;
                self.gain_gliding = glide;
            }
        }
        if zone.modulates & MODULATES_PITCH != 0 {
            self.modulation_cents = sums[PITCH];
            self.update_pitch();
        }
        if zone.modulates & MODULATES_AMPEG != 0 {
            self.envelope.set_shift(stages(sums, AMPEG));
        }
        if let Some(filter) = &zone.filter {
            if zone.modulates & MODULATES_FILEG != 0 {
                self.filter_envelope.set_shift(stages(sums, FILEG));
            }
            let depth = filter.depth + filter.vel2depth * self.velocity + sums[FILEG_DEPTH];
            let cents = filter.keytrack * (f64::from(self.key) - filter.keycenter)
                + filter.veltrack * self.velocity
                + depth * filter_level
                + sums[CUTOFF];
            let cutoff = filter.cutoff * 2f64.powf(cents / 1_200.0);
            let resonance = filter.resonance + sums[RESONANCE];
            let tuning = self.filter.tuning(cutoff, resonance, self.sample_rate);
            self.filter.glide(tuning, glide);
        }
    }

    /// The update due on this frame, with the controllers as they are on
    /// it.
    fn tick<C: Controls + ?Sized>(
        &mut self,
        zone: &Zone,
        controls: &C,
        frame: u64,
        filter_level: f64,
    ) {
        let sums = zone.sums(controls, frame);
        let glide = if core::mem::take(&mut self.first_tick) {
            0
        } else {
            TICK
        };
        self.modulate(zone, &sums, filter_level, glide);
        self.tick_left = TICK;
    }

    /// The key came up (and the pedal, if it was down).
    pub(crate) fn release(&mut self, instrument: &Instrument) {
        self.hold = Hold::Released;
        let Some(zone) = instrument.zones.get(self.zone) else {
            return;
        };
        if zone.one_shot {
            return;
        }
        if !self.envelope.is_released() {
            self.unreleased = Some((self.envelope, self.filter_envelope));
        }
        self.envelope.release();
        self.filter_envelope.release();
        self.end_sustain(zone);
    }

    /// Hands the note over to a release-triggered region: its own release,
    /// if its key has just come up, is taken back, its loop keeps going,
    /// and it falls as a cosine over `seconds`, then stops. The release
    /// rises as a sine over the same frames ([`fade_in`](Self::fade_in)),
    /// so that their powers add to one.
    pub(crate) fn hand_over(&mut self, seconds: f64, sample_rate: f64) {
        if let Some((envelope, filter_envelope)) = self.unreleased.take() {
            self.envelope = envelope;
            self.filter_envelope = filter_envelope;
            self.leave_loop = false;
        }
        self.crossfade = Crossfade::Out;
        self.crossfade_frames = (seconds.max(0.0) * sample_rate).round().clamp(1.0, 1e12) as u64;
        self.crossfade_done = 0;
    }

    /// Rises as a sine over `seconds`: the release side of
    /// [`hand_over`](Self::hand_over).
    pub(crate) fn fade_in(&mut self, seconds: f64, sample_rate: f64) {
        self.crossfade = Crossfade::In;
        self.crossfade_frames = (seconds.max(0.0) * sample_rate).round().clamp(1.0, 1e12) as u64;
        self.crossfade_done = 0;
    }

    /// The next [`ALIGN_FRAMES`] frames it reads at its pitch (each the
    /// whole frame nearest), from the one its position is in, both channels
    /// averaged, and its position's fraction in 32 bits: what a release
    /// compares itself with to start in phase.
    pub(crate) fn upcoming(&self, instrument: &Instrument) -> ([f32; ALIGN_FRAMES], u64) {
        let mut frames = [0.0; ALIGN_FRAMES];
        let fraction = self.position & (ONE - 1);
        let (Some(zone), true) = (instrument.zones.get(self.zone), self.active) else {
            return (frames, fraction);
        };
        let Some(sample) = zone.sample.and_then(|index| instrument.samples.get(index)) else {
            return (frames, fraction);
        };
        let first = (self.position >> 32) as i64;
        let step = self.step_frames;
        for (k, frame) in frames.iter_mut().enumerate() {
            let at = first + (k as f64 * step).round() as i64;
            let (left, right) = match self.map(zone, at) {
                Tap::Data(at) => self.frame(sample, at as u64).unwrap_or((0.0, 0.0)),
                Tap::Tail(at) => zone.lp.as_ref().map_or((0.0, 0.0), |lp| {
                    let left = lp.tail_left.get(at).copied().unwrap_or(0.0);
                    (left, lp.tail_right.get(at).copied().unwrap_or(left))
                }),
                Tap::Zero => (0.0, 0.0),
            };
            *frame = 0.5 * (left + right);
        }
        (frames, fraction)
    }

    /// Moves its start, within `window` frames after its offset, to where
    /// its next [`ALIGN_FRAMES`] frames best match `upcoming` (by
    /// normalised cross-correlation), and takes `fraction` as its own, so
    /// that it continues the phase of the voice those frames come from.
    /// Bounded work (`window` times [`ALIGN_FRAMES`] products, `window` at
    /// most [`MAX_RELEASE_ALIGN`](crate::region::MAX_RELEASE_ALIGN)), on
    /// the stack, for the audio thread.
    pub(crate) fn align(
        &mut self,
        instrument: &Instrument,
        (upcoming, fraction): &([f32; ALIGN_FRAMES], u64),
        window: u32,
    ) {
        const MAX: usize = crate::region::MAX_RELEASE_ALIGN as usize;
        let (Some(zone), true) = (instrument.zones.get(self.zone), self.active) else {
            return;
        };
        let Some(sample) = zone.sample.and_then(|index| instrument.samples.get(index)) else {
            return;
        };
        let offset = zone.offset;
        let step = self.step_frames;
        // The frames the comparison reaches past a start.
        let reach = ((ALIGN_FRAMES - 1) as f64 * step).round() as usize + 1;
        let room = (zone.end + 1).saturating_sub(offset) as usize;
        let window = (window as usize).min(MAX).min(room.saturating_sub(reach));
        if window == 0 {
            return;
        }
        let span = (window + reach).min(MAX + 4 * ALIGN_FRAMES);
        let mut own = [0.0f32; MAX + 4 * ALIGN_FRAMES];
        for (k, frame) in own[..span].iter_mut().enumerate() {
            let (left, right) = self.frame(sample, offset + k as u64).unwrap_or((0.0, 0.0));
            *frame = 0.5 * (left + right);
        }
        let mut best = (f64::NEG_INFINITY, 0usize);
        for start in 0..window.min(span - reach) {
            let mut product = 0.0;
            let mut energy = 1e-12;
            for (k, &value) in upcoming.iter().enumerate() {
                let at = start + (k as f64 * step).round() as usize;
                let own = f64::from(own[at]);
                product += f64::from(value) * own;
                energy += own * own;
            }
            let score = product / energy.sqrt();
            if score > best.0 {
                best = (score, start);
            }
        }
        self.position = ((offset + best.1 as u64) << 32) | fraction;
    }

    /// A sustain loop stops repeating, once the reading allows it (see
    /// [`try_leave_loop`](Self::try_leave_loop)).
    fn end_sustain(&mut self, zone: &Zone) {
        if self.looping && zone.lp.as_ref().is_some_and(|lp| lp.sustain_only) {
            self.leave_loop = true;
        }
    }

    /// Let go of for good, as by an all-notes-off: released as its key
    /// coming up would release it, pedal or not, unless that would take
    /// less than `shortest` seconds or not end it at all (a one-shot, or a
    /// region started by a release, which plays through); then it fades out
    /// over `shortest` in a straight line instead. A voice already in its
    /// release is left to it.
    pub(crate) fn let_go(&mut self, instrument: &Instrument, shortest: f64, sample_rate: f64) {
        let Some(zone) = instrument.zones.get(self.zone) else {
            self.stop();
            return;
        };
        if self.envelope.is_released() {
            return;
        }
        let plays_through = zone.one_shot || self.hold == Hold::Released;
        let shortest_samples = (shortest.max(0.0) * sample_rate).round();
        if plays_through || (self.envelope.release_samples() as f64) < shortest_samples {
            self.hold = Hold::Released;
            self.fade_out(shortest, sample_rate);
        } else {
            self.release(instrument);
        }
    }

    /// Silenced by a choke group, as the region's `off_mode` says.
    pub(crate) fn off(&mut self, instrument: &Instrument, sample_rate: f64) {
        let Some(zone) = instrument.zones.get(self.zone) else {
            self.stop();
            return;
        };
        match zone.off_mode {
            OffMode::Fast => self.fade_out(FAST_FADE_SECONDS, sample_rate),
            OffMode::Time => self.fade_out(zone.off_time, sample_rate),
            OffMode::Normal => {
                self.hold = Hold::Released;
                self.envelope.release();
                self.filter_envelope.release();
                self.end_sustain(zone);
            }
        }
    }

    /// Fades out in a straight line over `seconds`, or faster if a fade is
    /// already under way that ends sooner.
    pub(crate) fn fade_out(&mut self, seconds: f64, sample_rate: f64) {
        let frames = (seconds.max(0.0) * sample_rate).round().clamp(1.0, 1e12) as u64;
        if !self.dying {
            self.fade_from = 1.0;
            self.fade_frames = frames;
            self.fade_left = frames;
        } else if frames < self.fade_left {
            self.fade_from = self.fade();
            self.fade_frames = frames;
            self.fade_left = frames;
        }
        self.dying = true;
    }

    /// The fade's level now: from where it started down to zero, in a
    /// straight line.
    fn fade(&self) -> f64 {
        if self.dying {
            self.fade_from * self.fade_left as f64 / self.fade_frames.max(1) as f64
        } else {
            1.0
        }
    }

    /// Stops at once.
    pub(crate) fn stop(&mut self) {
        *self = Self::IDLE;
    }

    /// The gain the voice has now, for choosing which to cut.
    pub(crate) fn loudness(&self) -> f64 {
        self.envelope.level() * self.fade() * self.base_gain()
    }

    /// Whether the envelope is in its release, or the voice is fading.
    pub(crate) fn is_releasing(&self) -> bool {
        self.dying || self.envelope.is_released()
    }

    pub(crate) fn set_bend(&mut self, instrument: &Instrument, bend: Bend) {
        if let Some(zone) = instrument.zones.get(self.zone) {
            self.bend_cents = bend.cents(zone);
            self.update_pitch();
        }
    }

    /// Per-note tuning, in semitones.
    pub(crate) fn set_tuning(&mut self, semitones: f64) {
        self.expression_cents = semitones * 100.0;
        self.update_pitch();
    }

    /// Per-note gain factor.
    pub(crate) fn set_volume(&mut self, volume: f64) {
        self.expression_volume = volume.max(0.0);
        self.update_gain();
    }

    /// Per-note pan, -1 to 1, added to the region's.
    pub(crate) fn set_pan(&mut self, pan: f64) {
        self.expression_pan = pan;
        self.update_gain();
    }

    fn update_pitch(&mut self) {
        let cents =
            self.base_cents + self.bend_cents + self.expression_cents + self.modulation_cents;
        let ratio = (2f64.powf(cents / 1_200.0) * self.rate_ratio).clamp(1.0 / 65_536.0, 256.0);
        let ratio = if ratio.is_finite() { ratio } else { 1.0 };
        self.step = (ratio * ONE as f64).round() as u64;
        self.step_frames = self.step as f64 / ONE as f64;
    }

    /// Sets the gains at once, ending any glide.
    fn update_gain(&mut self) {
        (self.gain_left, self.gain_right) = self.gains();
        self.gain_gliding = 0;
    }

    /// The gain of the region and the note, before expression and pan.
    fn base_gain(&self) -> f64 {
        self.region_gain * self.velocity_gain * self.attenuation
    }

    /// The gain of each side the settings call for.
    fn gains(&self) -> (f64, f64) {
        let gain = self.base_gain() * self.expression_volume;
        let pan = (self.pan + self.expression_pan).clamp(-1.0, 1.0);
        if self.stereo {
            // A stereo sample is balanced: one side is turned down.
            (gain * (1.0 - pan).min(1.0), gain * (1.0 + pan).min(1.0))
        } else {
            // A mono sample is placed with a constant-power law that leaves
            // each side at unity in the centre.
            let angle = (pan + 1.0) * core::f64::consts::FRAC_PI_4;
            (
                gain * core::f64::consts::SQRT_2 * angle.cos(),
                gain * core::f64::consts::SQRT_2 * angle.sin(),
            )
        }
    }

    /// Brings a position past the loop's end back into it.
    fn wrap(&mut self, zone: &Zone) {
        if !self.looping {
            return;
        }
        let Some(lp) = zone.lp.as_ref() else {
            return;
        };
        let index = self.position >> 32;
        if index > lp.end {
            let inside = lp.start + (index - lp.start) % lp.len();
            self.position = (inside << 32) | (self.position & (ONE - 1));
            self.wrapped = true;
            self.just_wrapped = true;
        }
    }

    /// Leaves a released sustain loop if the taps of this read, `count`
    /// frames from `first`, all read the same frames with the loop as
    /// without it: then the switch changes nothing in the output, even in
    /// the middle of a crossfade. A loop too short for that is left on the
    /// first read after it goes round.
    fn try_leave_loop(&mut self, zone: &Zone, sample: &SampleData, first: i64, count: usize) {
        let (low, high) = self.plain_range(zone, sample);
        let last = first + count as i64 - 1;
        let fits = high - low + 1 >= count as i64;
        if zone.lp.is_none() || (first >= low && last <= high) || (!fits && self.just_wrapped) {
            self.looping = false;
            self.leave_loop = false;
        }
    }

    /// Adds the voice's sound to `buffer`, whose first frame is frame
    /// `clock` of the controllers in `controls`. Stops the voice, on the
    /// exact frame, when its sound ends.
    pub(crate) fn render<S: Sample, C: Controls + ?Sized>(
        &mut self,
        instrument: &Instrument,
        interpolation: Interpolation,
        buffer: &mut AudioBuffer<S>,
        controls: &C,
        clock: u64,
    ) {
        if !self.active {
            return;
        }
        let frames = buffer.num_frames();
        let at = (controls, clock);
        match buffer.num_channels() {
            0 => self.run(instrument, interpolation, frames, at, |_, _, _| {}),
            1 => {
                let out = buffer.channel_mut(0);
                self.run(
                    instrument,
                    interpolation,
                    frames,
                    at,
                    |frame, left, right| {
                        if let Some(sample) = out.get_mut(frame) {
                            *sample += S::from_f64(0.5 * (left + right));
                        }
                    },
                );
            }
            _ => {
                let (out_left, out_right) = buffer.channel_pair_mut(0, 1);
                self.run(
                    instrument,
                    interpolation,
                    frames,
                    at,
                    |frame, left, right| {
                        if let Some(sample) = out_left.get_mut(frame) {
                            *sample += S::from_f64(left);
                        }
                        if let Some(sample) = out_right.get_mut(frame) {
                            *sample += S::from_f64(right);
                        }
                    },
                );
            }
        }
    }

    fn run<C: Controls + ?Sized>(
        &mut self,
        instrument: &Instrument,
        interpolation: Interpolation,
        frames: usize,
        (controls, clock): (&C, u64),
        mut emit: impl FnMut(usize, f64, f64),
    ) {
        let Some(zone) = instrument.zones.get(self.zone) else {
            self.stop();
            return;
        };
        let Some(sample) = zone.sample.and_then(|index| instrument.samples.get(index)) else {
            self.stop();
            return;
        };
        if self.lease.is_held() {
            self.lease.refresh();
            // A looping voice that has gone into a loop kept in memory
            // needs, next, the frames after that stretch: when its sustain
            // ends it plays on from there.
            let index = self.position >> 32;
            if let Some(part) = sample.part_at(index).filter(|_| self.looping) {
                if index >= part.start + MARGIN && part.end() > self.lease.start() {
                    self.lease.restart(part.end());
                }
            }
        }
        let mut weights = [0.0; MAX_TAPS];
        for frame in 0..frames {
            let index = self.position >> 32;
            if !self.looping && index > zone.end {
                self.stop();
                return;
            }
            let level = self.envelope.next();
            if self.envelope.is_done() {
                self.stop();
                return;
            }
            if self.ticking {
                let filter_level = if self.filtered {
                    self.filter_envelope.next()
                } else {
                    0.0
                };
                if self.tick_left == 0 {
                    self.tick(zone, controls, clock + frame as u64, filter_level);
                }
                self.tick_left -= 1;
                if self.gain_gliding > 0 {
                    self.gain_gliding -= 1;
                    self.gain_left += self.gain_step_left;
                    self.gain_right += self.gain_step_right;
                }
            }
            let mut gain = level;
            if self.dying {
                if self.fade_left == 0 {
                    self.stop();
                    return;
                }
                gain *= self.fade();
                self.fade_left -= 1;
            }
            if self.crossfade != Crossfade::None {
                if self.crossfade == Crossfade::Out && self.crossfade_done >= self.crossfade_frames
                {
                    self.stop();
                    return;
                }
                let angle = core::f64::consts::FRAC_PI_2 * self.crossfade_done as f64
                    / self.crossfade_frames as f64;
                gain *= if self.crossfade == Crossfade::In {
                    angle.sin()
                } else {
                    angle.cos()
                };
                self.crossfade_done += 1;
                if self.crossfade == Crossfade::In && self.crossfade_done > self.crossfade_frames {
                    self.crossfade = Crossfade::None;
                }
            }
            let fraction_bits = self.position & (ONE - 1);
            let fraction = fraction_bits as f64 / ONE as f64;
            let (first, count) = if self.step == ONE && fraction_bits == 0 {
                // At its own pitch and rate, from a whole frame, the sample
                // is read as it is.
                weights[0] = 1.0;
                (index as i64, 1)
            } else {
                match interpolation {
                    Interpolation::Sinc => {
                        let half =
                            instrument
                                .sinc
                                .weights(fraction, self.step_frames, &mut weights);
                        (index as i64 + 1 - half as i64, 2 * half)
                    }
                    Interpolation::Cubic => {
                        weights[..4].copy_from_slice(&cubic_weights(fraction));
                        (index as i64 - 1, 4)
                    }
                }
            };
            if self.leave_loop {
                self.try_leave_loop(zone, sample, first, count);
            }
            self.just_wrapped = false;
            let gathered = if self.starved {
                None
            } else {
                self.gather(zone, sample, first, &weights[..count])
            };
            let (mut left, mut right) = match gathered {
                Some(read) => {
                    (self.held_left, self.held_right) = read;
                    read
                }
                None => {
                    if !self.starved {
                        // The disk has not kept up: hold the last values
                        // and fade them out, rather than click.
                        self.starved = true;
                        if let Some(pool) = &instrument.pool {
                            pool.count_underrun();
                        }
                        self.fade_out(FAST_FADE_SECONDS, self.sample_rate);
                    }
                    (self.held_left, self.held_right)
                }
            };
            if self.filtered {
                (left, right) = self.filter.process(left, right, self.stereo);
            }
            emit(
                frame,
                left * gain * self.gain_left,
                right * gain * self.gain_right,
            );
            self.position = self.position.wrapping_add(self.step);
            self.wrap(zone);
        }
        let index = self.position >> 32;
        self.lease.release_before(index.saturating_sub(MARGIN));
        if let Some(part) = sample.part_at(index).filter(|part| part.packed.is_some()) {
            self.lease.unpack_ahead(part, (index - part.start) as usize);
        }
    }

    /// The weighted sum of the frames from `first` on, through the loop
    /// when the voice is looping. `None` when a frame it needs is streamed
    /// and has not arrived.
    #[inline]
    fn gather(
        &self,
        zone: &Zone,
        sample: &SampleData,
        first: i64,
        weights: &[f64],
    ) -> Option<(f64, f64)> {
        let count = weights.len();
        let (low, high) = self.plain_range(zone, sample);
        if first >= low && first + count as i64 - 1 <= high {
            let start = first as u64;
            if let Some((left, right)) = sample.slice(start, count) {
                let left_sum = dot(left, weights);
                let right_sum = right.map_or(left_sum, |right| dot(right, weights));
                return Some((left_sum, right_sum));
            }
            // Across two stretches, or from the ring: gathered first, so
            // that the sums are made in the same order as above.
            let mut left = [0.0f32; MAX_TAPS];
            let mut right = [0.0f32; MAX_TAPS];
            for offset in 0..count {
                (left[offset], right[offset]) = self.frame(sample, start + offset as u64)?;
            }
            let left_sum = dot(&left[..count], weights);
            let right_sum = if sample.stereo {
                dot(&right[..count], weights)
            } else {
                left_sum
            };
            return Some((left_sum, right_sum));
        }
        let lp = zone.lp.as_ref();
        let mut left_sum = 0.0;
        let mut right_sum = 0.0;
        for (offset, &weight) in weights.iter().enumerate() {
            let (left, right) = match self.map(zone, first + offset as i64) {
                Tap::Data(at) => self.frame(sample, at as u64)?,
                Tap::Tail(at) => {
                    let Some(lp) = lp else { continue };
                    let left = lp.tail_left.get(at).copied().unwrap_or(0.0);
                    let right = if sample.stereo {
                        lp.tail_right.get(at).copied().unwrap_or(0.0)
                    } else {
                        left
                    };
                    (left, right)
                }
                Tap::Zero => continue,
            };
            left_sum += weight * f64::from(left);
            right_sum += weight * f64::from(right);
        }
        Some((left_sum, right_sum))
    }

    /// Frame `at` of the sample, left and right (the left twice for a mono
    /// sample), from memory or from the ring; silence past its end. `None`
    /// when it is streamed and has not arrived.
    #[inline]
    fn frame(&self, sample: &SampleData, at: u64) -> Option<(f32, f32)> {
        crate::store::frame(sample, &self.lease, at)
    }

    /// The frames that read straight from the sample data, needing neither
    /// the loop nor the crossfade nor silence for frames outside `offset`
    /// to `end`.
    fn plain_range(&self, zone: &Zone, sample: &SampleData) -> (i64, i64) {
        let last = (sample.frames() as i64 - 1).min(zone.end as i64);
        match zone.lp.as_ref() {
            Some(lp) if self.looping => {
                let low = if self.wrapped { lp.start } else { zone.offset };
                (low as i64, lp.end as i64 - lp.crossfade() as i64)
            }
            _ => (self.first_frame(zone) as i64, last),
        }
    }

    /// The first frame a voice that is not looping hears: `offset`, or the
    /// loop's start if that is earlier and the voice has been round the
    /// loop (a sustain loop that starts before `offset` and was left on
    /// release plays on from inside it).
    fn first_frame(&self, zone: &Zone) -> u64 {
        match zone.lp.as_ref() {
            Some(lp) if self.wrapped => lp.start.min(zone.offset),
            _ => zone.offset,
        }
    }

    /// Where the tap at `frame` reads. Frames before `offset` (before the
    /// loop first goes round, see [`first_frame`](Self::first_frame)) and
    /// after `end` are silence, so that a region cut from a longer file
    /// never hears its neighbours.
    fn map(&self, zone: &Zone, frame: i64) -> Tap {
        if let (true, Some(lp)) = (self.looping, zone.lp.as_ref()) {
            let start = lp.start as i64;
            let end = lp.end as i64;
            let len = lp.len() as i64;
            let mut at = frame;
            if at > end {
                at = start + (at - start) % len;
            } else if self.wrapped && at < start {
                at = end - (start - 1 - at) % len;
            }
            let crossfade = lp.crossfade() as i64;
            if crossfade > 0 && at > end - crossfade {
                return Tap::Tail((at - (end - crossfade + 1)) as usize);
            }
            return if at >= 0 && (self.wrapped || at >= zone.offset as i64) {
                Tap::Data(at as usize)
            } else {
                Tap::Zero
            };
        }
        if frame >= self.first_frame(zone) as i64 && frame <= zone.end as i64 {
            Tap::Data(frame as usize)
        } else {
            Tap::Zero
        }
    }
}

#[inline]
fn dot(frames: &[f32], weights: &[f64]) -> f64 {
    // Four partial sums, so that the additions do not wait on one another.
    let mut sums = [0.0; 4];
    let mut frame_chunks = frames.chunks_exact(4);
    let mut weight_chunks = weights.chunks_exact(4);
    for (frame, weight) in (&mut frame_chunks).zip(&mut weight_chunks) {
        for lane in 0..4 {
            sums[lane] += f64::from(frame[lane]) * weight[lane];
        }
    }
    let mut total = (sums[0] + sums[1]) + (sums[2] + sums[3]);
    for (&frame, &weight) in frame_chunks
        .remainder()
        .iter()
        .zip(weight_chunks.remainder())
    {
        total += f64::from(frame) * weight;
    }
    total
}
