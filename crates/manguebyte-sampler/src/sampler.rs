//! The sampler as a processor: voices, notes and the instrument swap.

use core::fmt;
use core::marker::PhantomData;

use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_core::{AudioBuffer, Sample};
use manguebyte_dsp::event::{Event, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Tuning};
use manguebyte_midi::MidiMessage;

use crate::controllers::{ControllerTable, MAX_CONTROLLERS};
use crate::instrument::Instrument;
use crate::interpolate::Interpolation;
use crate::player::{Bend, FAST_FADE_SECONDS, Hold, Modulation, Player, Start};
use crate::region::Trigger;

/// Most voices a [`Sampler`] accepts.
pub const MAX_POLYPHONY: usize = 4_096;

/// Most instruments a [`SamplerLoader`] can have sent and not yet
/// collected, beyond the ones the sampler holds.
pub const MAX_LOADER_CAPACITY: usize = 1_024;

/// Most notes a [`Sampler`] remembers as held, each with the velocity and
/// time its release-triggered regions need. When more are down at once,
/// the oldest is forgotten, and its key coming up starts no release
/// regions.
pub const MAX_HELD_NOTES: usize = 256;

/// Most changes [`Sampler::set_controller`] holds for one block.
pub const MAX_PENDING_CONTROLLER_CHANGES: usize = 1_024;

/// What the sampler remembers about one key of one channel.
#[derive(Debug, Clone, Copy)]
struct KeyState {
    /// How many notes on this key are down.
    down: u16,
}

const KEY_UP: KeyState = KeyState { down: 0 };

/// A note whose key is down, or whose key came up while the pedal held it:
/// what its release-triggered regions need.
#[derive(Debug, Clone, Copy)]
struct HeldNote {
    /// The note-on, for the regions its release starts.
    note: NoteRef,
    /// Its velocity, 1 to 127.
    velocity: u8,
    /// When it came, in frames since the start.
    started: u64,
    /// Note-ons counted from the start, which orders the held notes.
    order: u64,
    /// The key came up while the pedal was down: its release regions wait
    /// for the pedal.
    pending: bool,
}

#[derive(Debug, Clone, Copy)]
struct ChannelState {
    sustain: bool,
    bend: i16,
    keys: [KeyState; 128],
}

const CHANNEL_AT_REST: ChannelState = ChannelState {
    sustain: false,
    bend: 0,
    keys: [KEY_UP; 128],
};

/// A pitch ratio and a gain set from outside, as they ramp over a block:
/// from `from` at its start to `to` on its last frame, and `next` for the
/// block after.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Line {
    from: (f64, f64),
    to: (f64, f64),
    next: (f64, f64),
}

const LEVEL: Line = Line {
    from: (1.0, 1.0),
    to: (1.0, 1.0),
    next: (1.0, 1.0),
};

impl Line {
    /// The block starts: on to what was set for it.
    fn advance(&mut self) {
        self.from = self.to;
        self.to = self.next;
    }

    fn is_level(&self) -> bool {
        self.from == (1.0, 1.0) && self.to == (1.0, 1.0)
    }
}

/// A voice of the pool: what it plays and which instrument it reads.
#[derive(Debug, Clone)]
struct Voice {
    player: Player,
    /// Index into the sampler's two instrument places.
    slot: usize,
}

/// A polyphonic SFZ sampler.
///
/// It plays an [`Instrument`] handed over by its [`SamplerLoader`], reacting
/// to the events of each block on their exact frames: notes, the sustain
/// pedal (controller 64), the pitch wheel, per-note volume, pan and tuning
/// ([`Expression`]), note chokes, and controllers 120 (all sound off), 121
/// (reset controllers) and 123 (all notes off). A note-on with velocity 0
/// is a note-off, as in MIDI.
///
/// Controllers 0 to 119, from MIDI messages on any channel, and every
/// controller below [`MAX_CONTROLLERS`], from
/// [`set_controller`](Self::set_controller), modulate the opcodes of the
/// notes playing (`<opcode>_onccN`); see the
/// [crate documentation](crate#controllers). Controller 121 puts back
/// the initial values of MIDI's controllers that MIDI's recommended
/// practice RP-015 resets, and leaves the others, those above 127
/// included, alone.
///
/// For every note it starts each region whose keys, velocities, channels,
/// trigger, round-robin turn and random range accept it (see the
/// [crate documentation](crate) for each opcode). The output replaces what
/// was in the buffer: two channels get the left and right of each sample,
/// one channel gets their average, and channels past the second stay
/// silent.
///
/// # Voices
///
/// A voice plays one region, so a note takes as many voices as the regions
/// it starts. [`new`](Self::new) allocates `polyphony` voices, plus a
/// quarter as many again (at least two) for voices that are fading out
/// after being cut short, so that cutting never clicks. When `polyphony`
/// voices are sounding and another is needed, the oldest voice already in
/// its release fades out over 6 ms, or the oldest of all if none is; voices
/// of other notes go before the layers of the note being started. A
/// region's `polyphony` limits, the same way, the voices of its `group` if
/// a `group` opcode applies to it, and otherwise those of the regions under
/// the header where `polyphony` was written; `off_by` silences the voices
/// of another group as its `off_mode` says. If even the spare voices are
/// all in use, the quietest of the fading ones stops at once.
///
/// Release-triggered regions take the velocity and the start time of the
/// note whose key came up: each note-on is remembered (up to
/// [`MAX_HELD_NOTES`]) and a note-off ends the one with its identifier, or,
/// for notes without one, the latest on its channel and key.
///
/// # Real time
///
/// [`process_with`](Processor::process_with) and
/// [`reset`](Processor::reset) neither allocate, free, lock nor panic, and
/// the output does not depend on how the host cuts the timeline into
/// blocks: every voice moves one frame at a time and every event lands on
/// its frame. A new instrument is taken at the start of a block; the one
/// it replaces keeps playing the notes it had started until they end, and
/// is then sent back to the loader to be freed there. If a third arrives
/// while two are still sounding, the older one's notes fade out over 6 ms
/// first.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
/// use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
/// use manguebyte_sampler::{Instrument, MemorySource, Sampler};
///
/// // Off the audio thread: load an instrument and send it.
/// let instrument = Instrument::from_sfz(
///     "<region> sample=*sine ampeg_release=0.05",
///     &mut MemorySource::new(),
/// )?;
/// let (mut sampler, mut loader) = Sampler::<f32>::new(16);
/// loader.load(instrument).unwrap();
/// sampler.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 256, num_channels: 2 });
///
/// // On the audio thread: A4 starts on frame 100.
/// let mut events = EventBuffer::with_capacity(8);
/// let note = NoteRef::new(0, 69);
/// events.push(Event { offset: 100, kind: EventKind::NoteOn { note, velocity: 1.0 } }).unwrap();
/// let mut block = AudioBuffer::<f32>::new(2, 256);
/// sampler.process_with(&mut block, &ProcessContext::new(&events, &Transport::STOPPED));
/// assert!(block.channel(0)[..100].iter().all(|&s| s == 0.0));
/// assert!(block.channel(0)[100..].iter().any(|&s| s.abs() > 0.5));
/// assert_eq!(sampler.active_voices(), 1);
/// # Ok::<(), manguebyte_sampler::InstrumentError>(())
/// ```
pub struct Sampler<S: Sample = f32> {
    instruments: [Option<Box<Instrument>>; 2],
    current: usize,
    pending: Option<Box<Instrument>>,
    incoming: Consumer<Box<Instrument>>,
    retired: Producer<Box<Instrument>>,
    voices: Vec<Voice>,
    polyphony: usize,
    channels: Box<[ChannelState]>,
    /// Notes held, in no order; allocated once, never grown.
    held: Vec<HeldNote>,
    /// Note-ons counted, for [`HeldNote::order`].
    note_ons: u64,
    sample_rate: f64,
    interpolation: Interpolation,
    /// Frames rendered since the start or the last reset.
    clock: u64,
    /// Note events handled, which numbers the voices they start.
    serial: u64,
    controllers: ControllerTable,
    /// Changes [`set_controller`](Self::set_controller) asked for, in the
    /// order of their frames in the next block; allocated once, never
    /// grown.
    changes: Vec<PendingChange>,
    /// How many of `changes` the block under way has applied.
    applied: usize,
    /// Pitch and level from outside, per key and for every voice.
    keys: Box<[Line]>,
    all: Line,
    /// Frames of the block under way, and of it rendered.
    block_frames: u64,
    block_done: u64,
    /// The tuning of the keys.
    tuning: Tuning,
    marker: PhantomData<S>,
}

/// A controller change waiting for its frame.
#[derive(Debug, Clone, Copy)]
struct PendingChange {
    frame: u32,
    controller: u16,
    value: f64,
}

/// Whether controller 121 (reset all controllers) puts controller
/// `controller` back to its initial value. MIDI's recommended practice
/// RP-015 leaves alone bank select, volume, pan, the sound and effect
/// controllers and the channel mode messages.
fn reset_by_121(controller: u8) -> bool {
    !matches!(controller, 0 | 7 | 10 | 32 | 39 | 42 | 70..=79 | 91..=95 | 120..)
}

impl<S: Sample> fmt::Debug for Sampler<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sampler")
            .field("instrument", &self.instruments[self.current])
            .field("polyphony", &self.polyphony)
            .field("active_voices", &self.active_voices())
            .field("sample_rate", &self.sample_rate)
            .field("interpolation", &self.interpolation)
            .finish_non_exhaustive()
    }
}

/// The half of a [`Sampler`] that lives off the audio thread: it sends
/// instruments and frees the ones that come back.
pub struct SamplerLoader {
    outgoing: Producer<Box<Instrument>>,
    retired: Consumer<Box<Instrument>>,
    in_flight: usize,
    limit: usize,
}

impl fmt::Debug for SamplerLoader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SamplerLoader")
            .field("in_flight", &self.in_flight)
            .field("limit", &self.limit)
            .finish_non_exhaustive()
    }
}

impl<S: Sample> Sampler<S> {
    /// Creates a sampler with no instrument and `polyphony` voices (at most
    /// [`MAX_POLYPHONY`]), and the loader that feeds it. A voice plays one
    /// region, so a note that starts three layered regions takes three.
    /// Four instruments can be sent before any is collected.
    pub fn new(polyphony: usize) -> (Self, SamplerLoader) {
        Self::with_capacity(polyphony, 4)
    }

    /// Like [`new`](Self::new), allowing `capacity` instruments (1 to
    /// [`MAX_LOADER_CAPACITY`]) to be sent and not yet collected.
    pub fn with_capacity(polyphony: usize, capacity: usize) -> (Self, SamplerLoader) {
        let polyphony = polyphony.min(MAX_POLYPHONY);
        let spare = if polyphony == 0 {
            0
        } else {
            polyphony.div_ceil(4).max(2)
        };
        // The two the sampler holds and the one waiting have room of their
        // own, so neither queue can overflow.
        let limit = capacity.clamp(1, MAX_LOADER_CAPACITY) + 3;
        let (outgoing, incoming) = spsc::channel(limit);
        let (retired_producer, retired_consumer) = spsc::channel(limit);
        let voice = Voice {
            player: Player::IDLE,
            slot: 0,
        };
        (
            Self {
                instruments: [None, None],
                current: 0,
                pending: None,
                incoming,
                retired: retired_producer,
                voices: vec![voice; polyphony + spare],
                polyphony,
                channels: vec![CHANNEL_AT_REST; 16].into_boxed_slice(),
                held: Vec::with_capacity(MAX_HELD_NOTES),
                note_ons: 0,
                sample_rate: 48_000.0,
                interpolation: Interpolation::Sinc,
                clock: 0,
                serial: 0,
                controllers: ControllerTable::new(),
                changes: Vec::with_capacity(MAX_PENDING_CONTROLLER_CHANGES),
                applied: 0,
                keys: vec![LEVEL; 128].into_boxed_slice(),
                all: LEVEL,
                block_frames: 0,
                block_done: 0,
                tuning: Tuning::default(),
                marker: PhantomData,
            },
            SamplerLoader {
                outgoing,
                retired: retired_consumer,
                in_flight: 0,
                limit,
            },
        )
    }

    /// Creates a sampler already playing `instrument`.
    pub fn with_instrument(instrument: Instrument, polyphony: usize) -> (Self, SamplerLoader) {
        let (mut sampler, mut loader) = Self::new(polyphony);
        sampler.controllers.reset(&instrument.controller_defaults);
        sampler.instruments[0] = Some(Box::new(instrument));
        loader.in_flight = 1;
        (sampler, loader)
    }

    /// Sets controller `controller` to `value`, 0 to 1, from frame `frame`
    /// of the next block processed: a plugin's way of moving a parameter
    /// bound to a controller without making a MIDI message. Any controller
    /// below [`MAX_CONTROLLERS`] can be set, those above 127 included,
    /// which MIDI messages never reach. The value moves there along a
    /// straight line over [`CONTROLLER_SMOOTHING_SECONDS`], from wherever
    /// it is on that frame; a note that starts during the line starts
    /// with the value it has then.
    ///
    /// [`CONTROLLER_SMOOTHING_SECONDS`]: crate::CONTROLLER_SMOOTHING_SECONDS
    ///
    /// Several changes may be set for one block, each landing on its own
    /// frame, up to [`MAX_PENDING_CONTROLLER_CHANGES`]; past that, a
    /// change lands on the block's first frame. A frame past the block's
    /// end lands on its end. A value that is not a number is ignored, and
    /// one outside 0 to 1 is held to it. Real-time safe.
    ///
    /// ```
    /// use manguebyte_core::AudioBuffer;
    /// use manguebyte_dsp::event::NoteRef;
    /// use manguebyte_dsp::{ProcessSpec, Processor};
    /// use manguebyte_sampler::{Instrument, MemorySource, Sampler};
    ///
    /// // Controller 300, out of MIDI's reach, is the region's volume knob.
    /// let instrument = Instrument::from_sfz(
    ///     "<control> set_hdcc300=1
    ///      <region> sample=*sine amplitude=0 amplitude_oncc300=100",
    ///     &mut MemorySource::new(),
    /// )?;
    /// let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 8);
    /// sampler.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 4_800, num_channels: 1 });
    /// sampler.note_on(NoteRef::new(0, 69), 1.0);
    /// let mut block = AudioBuffer::<f32>::new(1, 4_800);
    /// sampler.process(&mut block);
    /// assert!(block.peak() > 0.99);
    ///
    /// // Turned down to a quarter halfway through the next block.
    /// sampler.set_controller(300, 0.25, 2_400);
    /// sampler.process(&mut block);
    /// assert!(block.channel(0)[..2_400].iter().any(|s| s.abs() > 0.99));
    /// assert!(block.channel(0)[3_600..].iter().all(|s| s.abs() < 0.26));
    /// assert_eq!(sampler.controller(300), 0.25);
    /// # Ok::<(), manguebyte_sampler::InstrumentError>(())
    /// ```
    pub fn set_controller(&mut self, controller: u16, value: f32, frame: u32) {
        if value.is_nan() || usize::from(controller) >= MAX_CONTROLLERS {
            return;
        }
        let change = PendingChange {
            frame,
            controller,
            value: f64::from(value).clamp(0.0, 1.0),
        };
        if self.changes.len() < self.changes.capacity() {
            // After every change already set for the same frame or an
            // earlier one.
            let at = self.changes.partition_point(|change| change.frame <= frame);
            self.changes.insert(at, change);
        } else {
            self.controllers.set(controller, change.value, self.clock);
        }
    }

    /// The value controller `controller` has, or is moving to, 0 to 1: its
    /// initial value until a MIDI message or
    /// [`set_controller`](Self::set_controller) changes it. A change set for
    /// a block not yet processed does not count until that block.
    pub fn controller(&self, controller: u16) -> f32 {
        self.controllers.target(controller) as f32
    }

    /// Applies the changes set for frames up to `frame` of the block.
    fn apply_pending(&mut self, frame: u32) {
        while let Some(change) = self.changes.get(self.applied) {
            if change.frame > frame {
                break;
            }
            self.controllers
                .set(change.controller, change.value, self.clock);
            self.applied += 1;
        }
    }

    /// Renders the block from `*position` up to `until`, cut at the frames
    /// of the controller changes due before it.
    fn render_until(&mut self, buffer: &mut AudioBuffer<S>, position: &mut usize, until: usize) {
        loop {
            self.apply_pending(*position as u32);
            let next = self
                .changes
                .get(self.applied)
                .map_or(until, |change| (change.frame as usize).min(until));
            if next > *position {
                buffer.windowed(*position..next, |piece| self.render(piece));
                *position = next;
            }
            if *position >= until {
                return;
            }
        }
    }

    /// Sets the pitch ratio and gain a model outside the sampler gives the
    /// voices of `key` (the note's key, 0 to 127, on any channel, before
    /// `note_offset`): the wind of an organ's model, which moves every
    /// pipe's pitch and level a little. The next block processed moves
    /// them along a straight line from where they stand to these values on
    /// its last frame, every frame of the way, and they hold after it. The
    /// ratio multiplies the step the voice reads its sample at (1 leaves it
    /// alone, and 2 is an octave up), through the same interpolator, so a
    /// few cents never alias; it is held to what a voice can read. The gain
    /// multiplies the voice's level. They multiply with those of
    /// [`modulate_all`](Self::modulate_all). A value that is not a number
    /// is ignored. Real-time safe.
    ///
    /// The line is laid over the host's block, so the same values set
    /// before blocks cut differently give a slightly different output; a
    /// model that runs once a block wants exactly that.
    pub fn modulate_key(&mut self, key: u8, pitch_ratio: f32, gain: f32) {
        if let Some(line) = self.keys.get_mut(usize::from(key & 0x7F)) {
            set_line(line, pitch_ratio, gain);
        }
    }

    /// Sets the pitch ratio and gain of every voice, as
    /// [`modulate_key`](Self::modulate_key) does for one key's.
    pub fn modulate_all(&mut self, pitch_ratio: f32, gain: f32) {
        set_line(&mut self.all, pitch_ratio, gain);
    }

    /// Chooses how samples are read between their frames. The default is
    /// [`Interpolation::Sinc`]. Takes effect from the next frame.
    pub fn set_interpolation(&mut self, interpolation: Interpolation) {
        self.interpolation = interpolation;
    }

    /// How samples are read between their frames.
    pub fn interpolation(&self) -> Interpolation {
        self.interpolation
    }

    /// Plays from now on in `tuning` instead of 12-tone equal temperament
    /// with A4 at 440 Hz. Real-time safe: the tuning is copied, which
    /// allocates nothing (see [`manguebyte_dsp::tuning`]).
    ///
    /// A key's pitch in the tuning takes the place of the key number in the
    /// region's pitch, `(pitch - pitch_keycenter) * pitch_keytrack / 100`
    /// semitones, so a sample sounds at its own pitch on its keycenter only
    /// where the tuning gives that key its equal-tempered pitch. The
    /// instrument's `note_offset` and `octave_offset` move the pitch in
    /// semitones on top, and the pitch wheel and `tune` and `transpose`
    /// stay intervals in cents and semitones. Regions are still chosen by
    /// key, and the filter still follows the key number. Notes already
    /// sounding keep their pitch; a note on a key the tuning leaves
    /// unmapped does not start.
    pub fn set_tuning(&mut self, tuning: &Tuning) {
        self.tuning.clone_from(tuning);
    }

    /// The tuning last given to [`set_tuning`](Self::set_tuning).
    pub fn tuning(&self) -> &Tuning {
        &self.tuning
    }

    /// The instrument new notes play, if any has arrived.
    pub fn instrument(&self) -> Option<&Instrument> {
        self.instruments[self.current].as_deref()
    }

    /// How many voices (regions, not notes) can sound at once, not counting
    /// those fading out after being cut short.
    pub fn polyphony(&self) -> usize {
        self.polyphony
    }

    /// How many voices are sounding, fading ones included. A note that
    /// starts several regions uses a voice for each.
    pub fn active_voices(&self) -> usize {
        self.voices
            .iter()
            .filter(|voice| voice.player.active)
            .count()
    }

    /// Starts a note now. A velocity of 0 or less, or one that is not a
    /// number, releases it instead. A note on a key the
    /// [tuning](Self::set_tuning) leaves unmapped is ignored. Real-time
    /// safe.
    pub fn note_on(&mut self, note: NoteRef, velocity: f32) {
        if velocity.is_nan() || velocity <= 0.0 {
            self.note_off(note, 0.0);
            return;
        }
        if !self.tuning.is_mapped(note.key & 0x7F) {
            return;
        }
        let channel = usize::from(note.channel & 0x0F);
        let velocity = (f64::from(velocity) * 127.0).round().clamp(1.0, 127.0) as u8;
        let held_before: u32 = self.channels[channel]
            .keys
            .iter()
            .map(|key| u32::from(key.down))
            .sum();
        let key_state = &mut self.channels[channel].keys[usize::from(note.key & 0x7F)];
        key_state.down = key_state.down.saturating_add(1);
        self.remember(note, velocity);
        self.start_regions_for(note, velocity, None, held_before);
    }

    /// Adds a note to the held ones, forgetting the oldest if there is no
    /// room. Never allocates: the list was given its capacity up front.
    fn remember(&mut self, note: NoteRef, velocity: u8) {
        self.note_ons += 1;
        let entry = HeldNote {
            note,
            velocity,
            started: self.clock,
            order: self.note_ons,
            pending: false,
        };
        if self.held.len() < self.held.capacity() {
            self.held.push(entry);
        } else if let Some(oldest) = self.held.iter_mut().min_by_key(|held| held.order) {
            *oldest = entry;
        }
    }

    /// Releases a note now. Real-time safe.
    pub fn note_off(&mut self, note: NoteRef, velocity: f32) {
        let _ = velocity;
        let channel = usize::from(note.channel & 0x0F);
        let pedal = self.channels[channel].sustain;
        for voice in &mut self.voices {
            let player = &mut voice.player;
            if player.active && player.hold == Hold::Key && note.matches(&player.note) {
                if pedal {
                    player.hold = Hold::Pedal;
                } else if let Some(instrument) = self.instruments[voice.slot].as_deref() {
                    player.release(instrument);
                }
            }
        }
        // The held note this note-off ends: the one with its identifier,
        // or, without one, the latest on its channel and key.
        let found = self
            .held
            .iter()
            .enumerate()
            .filter(|(_, held)| !held.pending && note.matches(&held.note))
            .max_by_key(|(_, held)| held.order)
            .map(|(index, held)| (index, *held));
        // The key that comes up is the held note's, which an identifier
        // may name on a key of its own.
        let ended = found.map_or(note, |(_, held)| held.note);
        let key_state = &mut self.channels[usize::from(ended.channel & 0x0F)].keys
            [usize::from(ended.key & 0x7F)];
        if key_state.down == 0 {
            return;
        }
        key_state.down -= 1;
        // A note forgotten for want of room starts no release regions.
        let Some((index, held)) = found else {
            return;
        };
        self.start_release_regions(&held, Trigger::ReleaseKey);
        if pedal {
            self.held[index].pending = true;
        } else {
            self.held.swap_remove(index);
            self.start_release_regions(&held, Trigger::Release);
        }
    }

    /// Lets go of every note on every channel, as all-notes-off would with
    /// the pedals up, but never in less than `shortest` seconds: a voice is
    /// released as its key coming up would release it, unless that would
    /// take less than `shortest` or would not end it at all (a one-shot, or
    /// a region started by a release, which plays through), and then it
    /// fades out over `shortest` in a straight line instead. A voice already
    /// in its release, or fading faster, is left to it. Afterwards every
    /// key and pedal is up, and no release-triggered region has started.
    /// Real-time safe.
    pub fn release_all(&mut self, shortest: f64) {
        let rate = self.sample_rate;
        for voice in &mut self.voices {
            if !voice.player.active {
                continue;
            }
            match self.instruments[voice.slot].as_deref() {
                Some(instrument) => voice.player.let_go(instrument, shortest, rate),
                None => voice.player.stop(),
            }
        }
        self.forget_keys();
    }

    /// Fades every voice out over `seconds` in a straight line, or faster
    /// if it was already fading faster; at 0 every voice falls silent after
    /// one more frame. Afterwards every key and pedal is up, and no
    /// release-triggered region has started. Real-time safe.
    pub fn fade_out_all(&mut self, seconds: f64) {
        let rate = self.sample_rate;
        for voice in &mut self.voices {
            if voice.player.active {
                voice.player.fade_out(seconds, rate);
            }
        }
        self.forget_keys();
    }

    /// Every key and pedal up, with no note held.
    fn forget_keys(&mut self) {
        for channel in self.channels.iter_mut() {
            channel.sustain = false;
            channel.keys = [KEY_UP; 128];
        }
        self.held.clear();
    }

    /// Handles one event now. Real-time safe.
    pub fn handle_event(&mut self, event: &Event) {
        match event.kind {
            EventKind::NoteOn { note, velocity } => self.note_on(note, velocity),
            EventKind::NoteOff { note, velocity } => self.note_off(note, velocity),
            EventKind::NoteChoke { note } => {
                let rate = self.sample_rate;
                for voice in &mut self.voices {
                    if voice.player.active && note.matches(&voice.player.note) {
                        voice.player.fade_out(FAST_FADE_SECONDS, rate);
                    }
                }
            }
            EventKind::NoteExpression {
                note,
                expression,
                value,
            } => {
                for voice in &mut self.voices {
                    let player = &mut voice.player;
                    if player.active && note.matches(&player.note) {
                        match expression {
                            Expression::Volume => player.set_volume(value),
                            Expression::Pan => player.set_pan(value),
                            Expression::Tuning => player.set_tuning(value),
                            _ => {}
                        }
                    }
                }
            }
            EventKind::Midi(message) => self.handle_midi(message),
            EventKind::ParamValue { .. }
            | EventKind::NoteParamValue { .. }
            | EventKind::ParamModulation { .. } => {}
        }
    }

    fn handle_midi(&mut self, message: MidiMessage) {
        match message {
            MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. } => {
                self.handle_event(&Event::from_midi(0, message));
            }
            MidiMessage::PitchBend { channel, value } => self.set_bend(channel, value),
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => {
                let index = usize::from(channel & 0x0F);
                if controller < 120 {
                    // MIDI's own controllers, shared by every channel.
                    let value = f64::from(value.min(127)) / 127.0;
                    self.controllers
                        .set(u16::from(controller), value, self.clock);
                }
                match controller {
                    64 => self.set_sustain(channel, value >= 64),
                    120 => {
                        let rate = self.sample_rate;
                        for voice in &mut self.voices {
                            if voice.player.active
                                && voice.player.note.channel & 0x0F == channel & 0x0F
                            {
                                voice.player.fade_out(FAST_FADE_SECONDS, rate);
                            }
                        }
                    }
                    121 => {
                        self.set_bend(channel, 0);
                        self.set_sustain(channel, false);
                        let clock = self.clock;
                        let defaults: &[f64] = self.instruments[self.current]
                            .as_deref()
                            .map_or(&[], |instrument| &instrument.controller_defaults);
                        for number in 0..120u8 {
                            if reset_by_121(number) {
                                self.controllers.restore(u16::from(number), defaults, clock);
                            }
                        }
                    }
                    123 => {
                        for voice in &mut self.voices {
                            let player = &mut voice.player;
                            if player.active
                                && player.hold != Hold::Released
                                && player.note.channel & 0x0F == channel & 0x0F
                            {
                                if let Some(instrument) = self.instruments[voice.slot].as_deref() {
                                    player.release(instrument);
                                }
                            }
                        }
                        for key in self.channels[index].keys.iter_mut() {
                            *key = KEY_UP;
                        }
                        self.held
                            .retain(|held| held.note.channel & 0x0F != channel & 0x0F);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn set_bend(&mut self, channel: u8, value: i16) {
        let channel = channel & 0x0F;
        self.channels[usize::from(channel)].bend = value;
        for voice in &mut self.voices {
            if voice.player.active && voice.player.note.channel & 0x0F == channel {
                if let Some(instrument) = self.instruments[voice.slot].as_deref() {
                    voice.player.set_bend(instrument, Bend::Wheel(value));
                }
            }
        }
    }

    fn set_sustain(&mut self, channel: u8, down: bool) {
        let index = usize::from(channel & 0x0F);
        let was_down = self.channels[index].sustain;
        self.channels[index].sustain = down;
        if down || !was_down {
            return;
        }
        for voice in &mut self.voices {
            let player = &mut voice.player;
            if player.active
                && player.hold == Hold::Pedal
                && player.note.channel & 0x0F == channel & 0x0F
            {
                if let Some(instrument) = self.instruments[voice.slot].as_deref() {
                    player.release(instrument);
                }
            }
        }
        // The notes the pedal held, by key and then in the order they
        // came.
        while let Some(at) = self
            .held
            .iter()
            .enumerate()
            .filter(|(_, held)| held.pending && held.note.channel & 0x0F == channel & 0x0F)
            .min_by_key(|(_, held)| (held.note.key, held.order))
            .map(|(at, _)| at)
        {
            let held = self.held.swap_remove(at);
            self.start_release_regions(&held, Trigger::Release);
        }
    }

    fn start_release_regions(&mut self, note: &HeldNote, trigger: Trigger) {
        let held = self.clock.saturating_sub(note.started) as f64 / self.sample_rate;
        self.start_regions_for(note.note, note.velocity, Some((trigger, held)), 0);
    }

    /// Starts the regions a note event calls for: on a key going down when
    /// `release` is `None`, or on it coming up with the trigger that
    /// applies and the seconds the note was held.
    fn start_regions_for(
        &mut self,
        note: NoteRef,
        velocity: u8,
        release: Option<(Trigger, f64)>,
        held_before: u32,
    ) {
        let slot = self.current;
        let Some(instrument) = self.instruments[slot].as_deref() else {
            return;
        };
        let key = i32::from(note.key & 0x7F) + instrument.note_shift;
        let Ok(key) = u8::try_from(key) else {
            return;
        };
        if key > 127 {
            return;
        }
        let list = match release {
            None => &instrument.attack_by_key[usize::from(key)],
            Some(_) => &instrument.release_by_key[usize::from(key)],
        };
        if list.is_empty() {
            return;
        }
        self.serial += 1;
        let serial = self.serial;
        let random = instrument.next_random();
        let channel = note.channel & 0x0F;
        let bend = self.channels[usize::from(channel)].bend;
        let pitch = self.tuning.key_pitch(note.key & 0x7F) + f64::from(instrument.note_shift);
        for &index in list {
            let index = index as usize;
            let Some(zone) = instrument.zones.get(index) else {
                continue;
            };
            let wanted = match (release, zone.trigger) {
                (None, Trigger::Attack) => true,
                (None, Trigger::First) => held_before == 0,
                (None, Trigger::Legato) => held_before > 0,
                (Some((wanted, _)), trigger) => wanted == trigger,
                _ => false,
            };
            if !wanted || !zone.accepts(channel, key, velocity) {
                continue;
            }
            if release.is_some_and(|(_, held)| !zone.accepts_held(held)) {
                continue;
            }
            if !instrument.take_turn(index) || !zone.accepts_random(random) {
                continue;
            }
            // The note's loudest sustaining voice, which a release with a
            // crossfade takes over from.
            let sustain = if release.is_some() && zone.release_crossfade > 0.0 {
                self.voices
                    .iter()
                    .enumerate()
                    .filter(|(_, voice)| {
                        voice.player.active
                            && voice.slot == slot
                            && !voice.player.release_trigger
                            && !voice.player.dying
                            && note.matches(&voice.player.note)
                    })
                    .max_by(|a, b| a.1.player.loudness().total_cmp(&b.1.player.loudness()))
                    .map(|(at, _)| at)
            } else {
                None
            };
            let attenuation_db = release.map_or(0.0, |(_, held)| zone.rt_decay * held);
            let start = Start {
                zone: index,
                note,
                key,
                pitch,
                velocity,
                serial,
                age: serial,
                bend: Bend::Wheel(bend),
                release_trigger: release.is_some(),
                attenuation_db,
                sample_rate: self.sample_rate,
            };
            let controls = (&self.controllers, self.clock);
            let started = start_voice(
                &mut self.voices,
                self.polyphony,
                instrument,
                slot,
                &start,
                controls,
            );
            if let (Some(sustain), Some(started)) = (sustain, started) {
                if sustain != started {
                    let rate = self.sample_rate;
                    let upcoming = self.voices[sustain].player.upcoming(instrument);
                    let release = &mut self.voices[started].player;
                    if zone.release_align > 0 {
                        release.align(instrument, &upcoming, zone.release_align);
                    }
                    release.fade_in(zone.release_crossfade, rate);
                    self.voices[sustain]
                        .player
                        .hand_over(zone.release_crossfade, rate);
                }
            }
        }
    }

    /// Takes the newest instrument that has arrived, if the place it goes
    /// to is free.
    ///
    /// Also sends back the instrument before the current one once its last
    /// note has ended.
    fn take_arrivals(&mut self) {
        while let Some(instrument) = self.incoming.pop() {
            if let Some(skipped) = self.pending.replace(instrument) {
                self.retire(skipped);
            }
        }
        let other = 1 - self.current;
        let waiting = self.pending.is_some();
        let mut busy = false;
        for voice in &mut self.voices {
            if voice.player.active && voice.slot == other {
                busy = true;
                if waiting {
                    voice.player.fade_out(FAST_FADE_SECONDS, self.sample_rate);
                }
            }
        }
        if busy {
            return;
        }
        if let Some(old) = self.instruments[other].take() {
            self.retire(old);
        }
        if let Some(pending) = self.pending.take() {
            // Controllers no one has set take the new instrument's initial
            // values: at once if nothing sounds, and smoothly otherwise.
            let at_once = !self.voices.iter().any(|voice| voice.player.active);
            self.controllers
                .adopt(&pending.controller_defaults, self.clock, at_once);
            self.instruments[other] = Some(pending);
            self.current = other;
        }
    }

    fn retire(&mut self, instrument: Box<Instrument>) {
        if let Err(instrument) = self.retired.push(instrument) {
            // The loader never has more instruments out than this queue
            // holds, so this cannot happen. Should it, freeing here could
            // stall the audio, so the memory is abandoned instead.
            core::mem::forget(instrument);
        }
    }

    fn render(&mut self, piece: &mut AudioBuffer<S>) {
        let len = self.block_frames.max(1);
        let all = self.all;
        let offset = self.block_done;
        for voice in &mut self.voices {
            if !voice.player.active {
                continue;
            }
            let key = &self.keys[usize::from(voice.player.note.key & 0x7F)];
            let modulation = if key.is_level() && all.is_level() {
                Modulation::NONE
            } else {
                let step = |line: &Line| {
                    (
                        (line.to.0 - line.from.0) / len as f64,
                        (line.to.1 - line.from.1) / len as f64,
                    )
                };
                let (key_step, all_step) = (step(key), step(&all));
                Modulation {
                    active: true,
                    ratio: [key.from.0, all.from.0],
                    ratio_step: [key_step.0, all_step.0],
                    gain: [key.from.1, all.from.1],
                    gain_step: [key_step.1, all_step.1],
                    offset,
                    len,
                }
            };
            match self.instruments[voice.slot].as_deref() {
                Some(instrument) => voice.player.render(
                    instrument,
                    self.interpolation,
                    piece,
                    &self.controllers,
                    self.clock,
                    &modulation,
                ),
                None => voice.player.stop(),
            }
        }
        self.clock = self.clock.wrapping_add(piece.num_frames() as u64);
        self.block_done += piece.num_frames() as u64;
    }
}

/// Sets what a line heads for in the next block.
fn set_line(line: &mut Line, pitch_ratio: f32, gain: f32) {
    if pitch_ratio.is_nan() || gain.is_nan() {
        return;
    }
    let ratio = f64::from(pitch_ratio).clamp(1.0 / 65_536.0, 256.0);
    line.next = (ratio, f64::from(gain).max(0.0));
}

/// Finds a voice for a region and starts it, after silencing what its
/// choke group and polyphony call for. Returns the voice, if it started.
fn start_voice(
    voices: &mut [Voice],
    polyphony: usize,
    instrument: &Instrument,
    slot: usize,
    start: &Start,
    (controls, now): (&ControllerTable, u64),
) -> Option<usize> {
    let zone = instrument.zones.get(start.zone)?;
    if voices.is_empty() {
        return None;
    }
    // Choke groups: this region's group silences the voices that name it
    // in `off_by`, except those started by the same event.
    for voice in voices.iter_mut() {
        let player = &mut voice.player;
        if player.active
            && voice.slot == slot
            && player.off_by == Some(zone.group)
            && player.serial != start.serial
        {
            player.off(instrument, start.sample_rate);
        }
    }
    // A region that never sounds still chokes, as `end=-1` is for.
    zone.sample?;
    // The region's own polyphony, over the voices it covers: those of its
    // group if it names one, and otherwise those of the regions under the
    // header that set the limit. Voices of other notes go first.
    if let Some(limit) = zone.polyphony {
        let scope = zone.polyphony_scope;
        loop {
            let in_scope = |voice: &Voice| {
                voice.player.active
                    && !voice.player.dying
                    && voice.slot == slot
                    && instrument
                        .zones
                        .get(voice.player.zone)
                        .is_some_and(|other| other.polyphony_scope == scope)
            };
            let count = voices.iter().filter(|voice| in_scope(voice)).count();
            if count < limit as usize {
                break;
            }
            let Some(oldest) = voices
                .iter_mut()
                .filter(|voice| in_scope(voice))
                .min_by_key(|voice| (voice.player.serial == start.serial, voice.player.age))
            else {
                break;
            };
            oldest.player.fade_out(FAST_FADE_SECONDS, start.sample_rate);
        }
    }
    // The sampler's polyphony: the voice that will be missed least fades,
    // a voice of another note before a layer of this one.
    let alive = voices
        .iter()
        .filter(|voice| voice.player.active && !voice.player.dying)
        .count();
    if alive >= polyphony {
        let victim = voices
            .iter_mut()
            .filter(|voice| voice.player.active && !voice.player.dying)
            .min_by_key(|voice| {
                (
                    voice.player.serial == start.serial,
                    !voice.player.is_releasing(),
                    voice.player.age,
                )
            });
        if let Some(victim) = victim {
            victim.player.fade_out(FAST_FADE_SECONDS, start.sample_rate);
        }
    }
    let free = match voices.iter().position(|voice| !voice.player.active) {
        Some(free) => free,
        None => {
            // Every voice, spares included, is busy: the quietest of those
            // fading out (they all are, once the polyphony is reached)
            // stops at once.
            let mut chosen = 0;
            let mut quietest = f64::INFINITY;
            for (index, voice) in voices.iter().enumerate() {
                let loudness = if voice.player.dying {
                    voice.player.loudness()
                } else {
                    f64::INFINITY
                };
                if loudness < quietest {
                    quietest = loudness;
                    chosen = index;
                }
            }
            chosen
        }
    };
    let voice = voices.get_mut(free)?;
    voice.slot = slot;
    voice
        .player
        .start(instrument, start, controls, now)
        .then_some(free)
}

impl<S: Sample> Processor<S> for Sampler<S> {
    /// Takes the sample rate and silences every voice.
    fn prepare(&mut self, spec: &ProcessSpec) {
        if spec.sample_rate.is_finite() && spec.sample_rate > 0.0 {
            self.sample_rate = spec.sample_rate;
        }
        self.controllers.set_sample_rate(self.sample_rate);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    /// Renders a block, each event and each change set by
    /// [`set_controller`](Sampler::set_controller) on its frame (a change
    /// before an event on the same frame).
    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        self.take_arrivals();
        buffer.clear();
        let frames = buffer.num_frames();
        for line in self.keys.iter_mut() {
            line.advance();
        }
        self.all.advance();
        self.block_frames = frames as u64;
        self.block_done = 0;
        let mut position = 0;
        for event in context.events {
            let at = (event.offset as usize).min(frames);
            self.render_until(buffer, &mut position, at);
            self.apply_pending(at as u32);
            self.handle_event(event);
        }
        self.render_until(buffer, &mut position, frames);
        // What is set for the block's end or later lands there.
        self.apply_pending(u32::MAX);
        self.changes.clear();
        self.applied = 0;
    }

    /// Silences every voice at once and returns to the state of a sampler
    /// that has just received its instrument: pedals up, wheels centred,
    /// no key down, every controller at the instrument's initial value
    /// (or, at once, at a value set for a block not yet processed), and the
    /// instrument's round robins and random numbers started again. Playing
    /// the same notes after a reset gives the same output, bit for bit.
    fn reset(&mut self) {
        for voice in &mut self.voices {
            voice.player.stop();
            voice.slot = 0;
        }
        for channel in self.channels.iter_mut() {
            *channel = CHANNEL_AT_REST;
        }
        self.held.clear();
        self.note_ons = 0;
        self.clock = 0;
        self.serial = 0;
        // Nothing sounds now, so a waiting instrument can come in, and the
        // one before the current one can go.
        let other = 1 - self.current;
        if let Some(old) = self.instruments[other].take() {
            self.retire(old);
        }
        if let Some(pending) = self.pending.take() {
            self.instruments[other] = Some(pending);
            self.current = other;
        }
        if let Some(instrument) = self.instruments[self.current].as_deref() {
            instrument.restart();
        }
        let defaults: &[f64] = self.instruments[self.current]
            .as_deref()
            .map_or(&[], |instrument| &instrument.controller_defaults);
        self.controllers.reset(defaults);
        for change in &self.changes {
            self.controllers.jump(change.controller, change.value);
        }
        // What was set from outside holds, at once.
        for line in self.keys.iter_mut().chain(core::iter::once(&mut self.all)) {
            line.from = line.next;
            line.to = line.next;
        }
        self.changes.clear();
        self.applied = 0;
    }
}

impl SamplerLoader {
    /// Sends `instrument` to replace the one playing. Its round robins and
    /// random numbers start again.
    ///
    /// # Errors
    ///
    /// Hands the instrument back, boxed, if too many have been sent and not
    /// collected; call [`collect`](Self::collect) after the audio thread has
    /// run, and try again.
    pub fn load(&mut self, instrument: Instrument) -> Result<(), Box<Instrument>> {
        self.collect();
        let instrument = Box::new(instrument);
        if self.in_flight >= self.limit {
            return Err(instrument);
        }
        instrument.restart();
        match self.outgoing.push(instrument) {
            Ok(()) => {
                self.in_flight += 1;
                Ok(())
            }
            Err(instrument) => Err(instrument),
        }
    }

    /// Sends an instrument that is boxed already, as [`load`](Self::load)
    /// does, without collecting. Never allocates or frees, so a loader kept
    /// on the audio thread, beside its sampler, can take an instrument made
    /// and boxed on another thread; see [`take_retired`](Self::take_retired)
    /// for the other way.
    ///
    /// # Errors
    ///
    /// Hands the instrument back if too many have been sent and not
    /// collected or taken back.
    pub fn load_boxed(&mut self, instrument: Box<Instrument>) -> Result<(), Box<Instrument>> {
        if self.in_flight >= self.limit {
            return Err(instrument);
        }
        instrument.restart();
        match self.outgoing.push(instrument) {
            Ok(()) => {
                self.in_flight += 1;
                Ok(())
            }
            Err(instrument) => Err(instrument),
        }
    }

    /// Takes back one instrument the audio thread has finished with,
    /// without freeing it, for a caller that frees it on another thread.
    /// Never allocates or frees. [`collect`](Self::collect) frees them all
    /// instead.
    pub fn take_retired(&mut self) -> Option<Box<Instrument>> {
        let instrument = self.retired.pop()?;
        self.in_flight = self.in_flight.saturating_sub(1);
        Some(instrument)
    }

    /// Frees the instruments the audio thread has finished with, and
    /// returns how many there were.
    pub fn collect(&mut self) -> usize {
        let mut count = 0;
        while let Some(instrument) = self.retired.pop() {
            drop(instrument);
            count += 1;
        }
        self.in_flight = self.in_flight.saturating_sub(count);
        count
    }

    /// Instruments sent and not yet collected, including the one playing.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }
}
