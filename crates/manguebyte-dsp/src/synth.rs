use core::marker::PhantomData;

use manguebyte_core::{AudioBuffer, ParamId, Sample};
use manguebyte_midi::MidiMessage;

use crate::event::{Event, EventKind, Expression, NoteRef};
use crate::{ProcessContext, ProcessSpec, Processor, split_at_events};

/// One note's worth of sound in a [`Synth`].
///
/// A voice is told when its note starts and ends and is asked to add its
/// sound to a buffer. Everything about what it sounds like is up to the
/// implementation: usually an oscillator, a filter and an envelope.
///
/// Every method runs on the audio thread except [`prepare`](Self::prepare).
pub trait Voice<S: Sample = f32> {
    /// Gets ready to render under `spec`. May allocate.
    fn prepare(&mut self, spec: &ProcessSpec);

    /// Starts a note. `velocity` runs from 0 to 1: a [`Synth`] keeps it in
    /// that range, and passes a velocity that is not a finite number as 0.
    ///
    /// This is also called on a voice that is still sounding, when the same
    /// key is struck again or when the voice is taken over for a new note.
    fn note_on(&mut self, note: NoteRef, velocity: f32);

    /// The note was released: begin fading out. A [`Synth`] keeps
    /// `velocity` between 0 and 1, as for [`note_on`](Self::note_on).
    fn note_off(&mut self, velocity: f32);

    /// Stop at once, with no fade, and return to the state the voice had
    /// when it was new: oscillator phases, filter memory, everything.
    ///
    /// The synth calls this before every fresh note. A voice that has
    /// finished keeps being rendered until the end of the piece of the block
    /// it finished in, so without a full reset here its next note would
    /// depend on how the host happened to cut the timeline into blocks.
    fn choke(&mut self);

    /// Whether the voice is still producing sound. A voice that returns
    /// `false` is free to be given another note.
    fn is_active(&self) -> bool;

    /// Adds this voice's sound to `buffer`, on top of what is already there.
    fn render(&mut self, buffer: &mut AudioBuffer<S>);

    /// This note alone is being shaped. See [`Expression`] for the units.
    fn expression(&mut self, expression: Expression, value: f64) {
        let _ = (expression, value);
    }

    /// The pitch wheel is this many semitones from its centre.
    ///
    /// Called when the wheel moves, and also just before
    /// [`note_on`](Self::note_on) for every fresh note, so that a note
    /// struck while the wheel is held starts bent instead of in tune.
    fn pitch_bend(&mut self, semitones: f64) {
        let _ = semitones;
    }

    /// A parameter took a new value, in the parameter's own unit. Sent to
    /// every voice, sounding or not.
    fn set_param(&mut self, param: ParamId, value: f64) {
        let _ = (param, value);
    }

    /// An offset to add on top of a parameter's value for this voice. Zero
    /// ends the modulation.
    fn modulate(&mut self, param: ParamId, amount: f64) {
        let _ = (param, amount);
    }

    /// Any other MIDI message on this voice's channel, such as a controller.
    ///
    /// A controller is a position, not a moment: before
    /// [`note_on`](Self::note_on) for a fresh note the voice is told where
    /// every controller that has moved on its channel now stands.
    fn midi(&mut self, message: MidiMessage) {
        let _ = message;
    }
}

/// A controller nobody has moved yet. MIDI values stop at 127.
const UNMOVED: u8 = 0xFF;

/// A velocity as the voices receive it: between 0 and 1, and 0 when it is
/// not a finite number. A host can send anything; a NaN or an infinity
/// multiplied into a voice's sound would stay in every filter and reverb
/// after it.
fn velocity_in_range(velocity: f32) -> f32 {
    if velocity.is_finite() {
        velocity.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Not sounding.
    Idle,
    /// The key is down.
    Held,
    /// The key is up but the sustain pedal is keeping the note going.
    Sustained,
    /// Fading out.
    Released,
}

#[derive(Debug)]
struct Slot<V> {
    voice: V,
    note: NoteRef,
    stage: Stage,
    /// Value of the synth's clock when the note started. Lower is older.
    started: u64,
}

/// A polyphonic instrument: a pool of [`Voice`]s and the logic that hands
/// notes to them.
///
/// The synth is a [`Processor`]. Given the events of a block it starts and
/// stops each note on the exact frame it belongs to, and routes everything
/// else to the right place:
///
/// - **Per-note expression and modulation** reach only the voice playing
///   that note, so one note of a chord can be bent, swelled or filtered on
///   its own.
/// - **Parameter changes** reach every voice.
/// - **The sustain pedal** (controller 64) keeps released notes sounding
///   until it is lifted, per channel.
/// - **Pitch bend** is converted to semitones using
///   [`set_pitch_bend_range`](Self::set_pitch_bend_range).
/// - **The wheel and the controllers are remembered** per channel. A voice
///   starting a note is told where they stand, so the note sounds the same
///   whichever voice it lands on and whether or not that voice was sounding
///   when the wheel moved. Controller 121 puts them back.
/// - Controllers 120 and 123 silence or release a whole channel.
///
/// When every voice is busy, a new note takes over the voice that will be
/// missed least: one that is already fading out if there is one, then one
/// held only by the pedal, then the oldest held note.
///
/// The output replaces whatever was in the buffer.
///
/// ```
/// use manguebyte_core::AudioBuffer;
/// use manguebyte_dsp::event::{Event, EventBuffer, EventKind, NoteRef};
/// use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Transport, Voice};
///
/// /// The simplest possible voice: a constant level while its key is down.
/// #[derive(Default)]
/// struct Beep { level: f32 }
///
/// impl Voice for Beep {
///     fn prepare(&mut self, _: &ProcessSpec) {}
///     fn note_on(&mut self, _: NoteRef, velocity: f32) { self.level = velocity; }
///     fn note_off(&mut self, _: f32) { self.level = 0.0; }
///     fn choke(&mut self) { self.level = 0.0; }
///     fn is_active(&self) -> bool { self.level > 0.0 }
///     fn render(&mut self, buffer: &mut AudioBuffer) {
///         for channel in buffer.iter_channels_mut() {
///             channel.iter_mut().for_each(|sample| *sample += self.level);
///         }
///     }
/// }
///
/// let mut synth = Synth::with_voices(8, Beep::default);
/// synth.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 8, num_channels: 1 });
///
/// // A note that starts on frame 5 of the block.
/// let mut events = EventBuffer::with_capacity(8);
/// let note = NoteRef::new(0, 60);
/// events.push(Event { offset: 5, kind: EventKind::NoteOn { note, velocity: 0.5 } }).unwrap();
///
/// let mut block = AudioBuffer::<f32>::new(1, 8);
/// synth.process_with(&mut block, &ProcessContext::new(&events, &Transport::STOPPED));
/// assert_eq!(block.channel(0), &[0.0, 0.0, 0.0, 0.0, 0.0, 0.5, 0.5, 0.5]);
/// ```
#[derive(Debug)]
pub struct Synth<V, S: Sample = f32> {
    slots: Vec<Slot<V>>,
    clock: u64,
    /// Whether the sustain pedal is down, per channel.
    sustain: [bool; 16],
    /// Where the pitch wheel is, per channel, as MIDI sends it.
    bend: [i16; 16],
    /// Where each controller is, per channel, or [`UNMOVED`].
    controllers: [[u8; 128]; 16],
    bend_range: f64,
    marker: PhantomData<S>,
}

impl<S: Sample, V: Voice<S>> Synth<V, S> {
    /// Creates a synth from a set of voices. Its polyphony is their number.
    pub fn new(voices: impl IntoIterator<Item = V>) -> Self {
        Self {
            slots: voices
                .into_iter()
                .map(|voice| Slot {
                    voice,
                    note: NoteRef::new(0, 0),
                    stage: Stage::Idle,
                    started: 0,
                })
                .collect(),
            clock: 0,
            sustain: [false; 16],
            bend: [0; 16],
            controllers: [[UNMOVED; 128]; 16],
            bend_range: 2.0,
            marker: PhantomData,
        }
    }

    /// Creates a synth with `count` voices made by `make`.
    pub fn with_voices(count: usize, make: impl FnMut() -> V) -> Self {
        Self::new(core::iter::repeat_with(make).take(count))
    }

    /// Sets how many semitones the pitch wheel covers in each direction.
    /// The default is 2.
    pub fn set_pitch_bend_range(&mut self, semitones: f64) {
        self.bend_range = semitones;
    }

    /// How many notes can sound at once.
    pub fn polyphony(&self) -> usize {
        self.slots.len()
    }

    /// How many voices are sounding right now.
    pub fn active_voices(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.stage != Stage::Idle && slot.voice.is_active())
            .count()
    }

    /// Every voice, sounding or not, for setting them up.
    pub fn voices_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.slots.iter_mut().map(|slot| &mut slot.voice)
    }

    /// Starts a note now. Real-time safe.
    ///
    /// The voice is given `velocity` limited to 0..=1, and 0 if it is not a
    /// finite number.
    pub fn note_on(&mut self, note: NoteRef, velocity: f32) {
        let velocity = velocity_in_range(velocity);
        self.reclaim_finished();
        self.clock += 1;

        // The same key struck again restarts its own voice, unless the two
        // notes carry different identifiers and so are meant to overlap.
        let same_note = self
            .slots
            .iter()
            .position(|slot| slot.stage != Stage::Idle && slot.note == note);
        let index = same_note.or_else(|| self.pick_voice());
        let Some(index) = index else {
            return;
        };

        let slot = &mut self.slots[index];
        if same_note.is_none() {
            // A fresh note always starts from a clean voice, whether the
            // voice was idle or is being taken over. See `Voice::choke`.
            slot.voice.choke();
            // A clean voice knows nothing of the wheel or the controllers,
            // which were last moved while it was silent or playing a note
            // on another channel.
            let channel = usize::from(note.channel & 0x0F);
            let semitones = f64::from(self.bend[channel]) / 8192.0 * self.bend_range;
            slot.voice.pitch_bend(semitones);
            for (controller, &value) in self.controllers[channel].iter().enumerate() {
                if value != UNMOVED {
                    slot.voice.midi(MidiMessage::ControlChange {
                        channel: note.channel,
                        controller: controller as u8,
                        value,
                    });
                }
            }
        }
        slot.note = note;
        slot.stage = Stage::Held;
        slot.started = self.clock;
        slot.voice.note_on(note, velocity);
    }

    /// Releases a note now. Real-time safe. The velocity is limited as for
    /// [`note_on`](Self::note_on).
    pub fn note_off(&mut self, note: NoteRef, velocity: f32) {
        let velocity = velocity_in_range(velocity);
        let pedal_down = self.sustain[usize::from(note.channel & 0x0F)];
        for slot in &mut self.slots {
            if slot.stage == Stage::Held && note.matches(&slot.note) {
                if pedal_down {
                    slot.stage = Stage::Sustained;
                } else {
                    slot.stage = Stage::Released;
                    slot.voice.note_off(velocity);
                }
            }
        }
    }

    /// Releases every note on every channel, letting them fade. Real-time
    /// safe.
    pub fn all_notes_off(&mut self) {
        self.sustain = [false; 16];
        for slot in &mut self.slots {
            if matches!(slot.stage, Stage::Held | Stage::Sustained) {
                slot.stage = Stage::Released;
                slot.voice.note_off(0.0);
            }
        }
    }

    /// Handles one event now. Real-time safe.
    pub fn handle_event(&mut self, event: &Event) {
        match event.kind {
            EventKind::NoteOn { note, velocity } => self.note_on(note, velocity),
            EventKind::NoteOff { note, velocity } => self.note_off(note, velocity),
            EventKind::NoteChoke { note } => {
                for slot in self.sounding(move |slot| note.matches(&slot.note)) {
                    slot.voice.choke();
                    slot.stage = Stage::Idle;
                }
            }
            EventKind::NoteExpression {
                note,
                expression,
                value,
            } => {
                for slot in self.sounding(move |slot| note.matches(&slot.note)) {
                    slot.voice.expression(expression, value);
                }
            }
            EventKind::ParamValue { param, value } => {
                for slot in &mut self.slots {
                    slot.voice.set_param(param, value);
                }
            }
            EventKind::ParamModulation {
                param,
                amount,
                note,
            } => match note {
                Some(note) => {
                    for slot in self.sounding(move |slot| note.matches(&slot.note)) {
                        slot.voice.modulate(param, amount);
                    }
                }
                None => {
                    for slot in &mut self.slots {
                        slot.voice.modulate(param, amount);
                    }
                }
            },
            EventKind::Midi(message) => self.handle_midi(message),
        }
    }

    fn handle_midi(&mut self, message: MidiMessage) {
        match message {
            MidiMessage::NoteOn { .. } | MidiMessage::NoteOff { .. } => {
                self.handle_event(&Event::from_midi(0, message));
            }
            MidiMessage::ControlChange {
                channel,
                controller: 64,
                value,
            } => self.set_sustain(channel, value >= 64),
            MidiMessage::ControlChange {
                channel,
                controller: 120,
                ..
            } => {
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    slot.voice.choke();
                    slot.stage = Stage::Idle;
                }
            }
            MidiMessage::ControlChange {
                channel,
                controller: 123,
                ..
            } => {
                self.sustain[usize::from(channel & 0x0F)] = false;
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    if slot.stage != Stage::Released {
                        slot.stage = Stage::Released;
                        slot.voice.note_off(0.0);
                    }
                }
            }
            MidiMessage::PitchBend { channel, value } => {
                self.bend[usize::from(channel & 0x0F)] = value;
                let semitones = f64::from(value) / 8192.0 * self.bend_range;
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    slot.voice.pitch_bend(semitones);
                }
            }
            MidiMessage::ControlChange {
                channel,
                controller,
                value,
            } => {
                let index = usize::from(channel & 0x0F);
                if controller == 121 {
                    // Reset all controllers: the wheel returns to its
                    // centre, the pedal comes up and the controllers are
                    // forgotten, except those the MIDI recommendation
                    // (RP-015) says a reset leaves alone: bank, volume,
                    // pan, and the sound and effect controllers.
                    self.bend[index] = 0;
                    for (number, held) in self.controllers[index].iter_mut().enumerate() {
                        if !matches!(number, 0 | 7 | 10 | 32 | 70..=79 | 91..=95) {
                            *held = UNMOVED;
                        }
                    }
                    self.set_sustain(channel, false);
                    for slot in self.sounding(move |slot| slot.note.channel == channel) {
                        slot.voice.pitch_bend(0.0);
                    }
                } else {
                    self.controllers[index][usize::from(controller & 0x7F)] = value.min(127);
                }
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    slot.voice.midi(message);
                }
            }
            other => {
                let channel = other.channel();
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    slot.voice.midi(other);
                }
            }
        }
    }

    fn set_sustain(&mut self, channel: u8, down: bool) {
        self.sustain[usize::from(channel & 0x0F)] = down;
        if down {
            return;
        }
        for slot in &mut self.slots {
            if slot.stage == Stage::Sustained && slot.note.channel == channel {
                slot.stage = Stage::Released;
                slot.voice.note_off(0.0);
            }
        }
    }

    /// The sounding voices that satisfy `wanted`.
    fn sounding(
        &mut self,
        wanted: impl Fn(&Slot<V>) -> bool,
    ) -> impl Iterator<Item = &mut Slot<V>> {
        self.slots
            .iter_mut()
            .filter(move |slot| slot.stage != Stage::Idle && wanted(slot))
    }

    /// Marks voices whose sound has ended as free.
    fn reclaim_finished(&mut self) {
        for slot in &mut self.slots {
            if slot.stage == Stage::Released && !slot.voice.is_active() {
                slot.stage = Stage::Idle;
            }
        }
    }

    /// Chooses the voice for a new note: the first free one, or else the one
    /// that will be missed least.
    ///
    /// A free voice is chosen by its place alone, not by when it last
    /// played: the voices are added up in their order, and which voice a
    /// note lands on decides that order, which changes the last bit of a sum
    /// of three or more. Chosen this way, once every note has ended the
    /// synth lands notes exactly as a new one would, so playing the same
    /// notes again gives the same output, bit for bit, reset or not.
    fn pick_voice(&self) -> Option<usize> {
        let cost = |stage: Stage| match stage {
            Stage::Idle => 0,
            Stage::Released => 1,
            Stage::Sustained => 2,
            Stage::Held => 3,
        };
        self.slots
            .iter()
            .enumerate()
            .min_by_key(|(_, slot)| {
                let age = if slot.stage == Stage::Idle {
                    0
                } else {
                    slot.started
                };
                (cost(slot.stage), age)
            })
            .map(|(index, _)| index)
    }
}

impl<S: Sample, V: Voice<S>> Processor<S> for Synth<V, S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        for slot in &mut self.slots {
            slot.voice.prepare(spec);
        }
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        buffer.clear();
        split_at_events(
            self,
            buffer,
            context.events,
            |synth, event| synth.handle_event(event),
            |synth, piece| {
                for slot in &mut synth.slots {
                    if slot.stage != Stage::Idle {
                        slot.voice.render(piece);
                    }
                }
            },
        );
        self.reclaim_finished();
    }

    /// Silences every voice, lifts the pedal, and forgets the voices' ages,
    /// so that the same notes played after a reset land on the same voices
    /// and sound exactly as they did the first time, bit for bit, which is
    /// what a host rendering a project twice expects.
    ///
    /// The pitch wheel and the controllers keep their positions: they are
    /// where the player left them, and a host that deactivates and
    /// reactivates a plugin (to change the sample rate, or to render
    /// offline) does not always send them again. Controller 121 puts them
    /// back.
    fn reset(&mut self) {
        self.clock = 0;
        self.sustain = [false; 16];
        for slot in &mut self.slots {
            slot.voice.choke();
            slot.stage = Stage::Idle;
            slot.note = NoteRef::new(0, 0);
            slot.started = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;
    use crate::event::EventBuffer;

    const CUTOFF: ParamId = ParamId::from_name("cutoff");

    /// Outputs its key number while held, half of it for two frames after
    /// release, and records everything it is told.
    #[derive(Debug, Default)]
    struct Probe {
        key: f32,
        held: bool,
        tail: usize,
        starts: usize,
        chokes: usize,
        bend: f64,
        tuning: f64,
        cutoff: f64,
        modulation: f64,
        controller: Option<(u8, u8)>,
        velocity: f32,
        release: f32,
    }

    impl Voice for Probe {
        fn prepare(&mut self, _: &ProcessSpec) {}
        fn note_on(&mut self, note: NoteRef, velocity: f32) {
            self.key = f32::from(note.key);
            self.velocity = velocity;
            self.held = true;
            self.tail = 0;
            self.starts += 1;
        }
        fn note_off(&mut self, velocity: f32) {
            self.release = velocity;
            self.held = false;
            self.tail = 2;
        }
        fn choke(&mut self) {
            self.held = false;
            self.tail = 0;
            self.chokes += 1;
            // As new, as the contract asks: what the synth tells a fresh
            // note about the controllers is all it knows of them.
            self.controller = None;
        }
        fn is_active(&self) -> bool {
            self.held || self.tail > 0
        }
        fn render(&mut self, buffer: &mut AudioBuffer) {
            for frame in 0..buffer.num_frames() {
                let level = if self.held {
                    self.key
                } else if self.tail > 0 {
                    self.tail -= 1;
                    self.key / 2.0
                } else {
                    0.0
                };
                buffer.channel_mut(0)[frame] += level;
            }
        }
        fn expression(&mut self, expression: Expression, value: f64) {
            if expression == Expression::Tuning {
                self.tuning = value;
            }
        }
        fn pitch_bend(&mut self, semitones: f64) {
            self.bend = semitones;
        }
        fn set_param(&mut self, param: ParamId, value: f64) {
            if param == CUTOFF {
                self.cutoff = value;
            }
        }
        fn modulate(&mut self, param: ParamId, amount: f64) {
            if param == CUTOFF {
                self.modulation = amount;
            }
        }
        fn midi(&mut self, message: MidiMessage) {
            if let MidiMessage::ControlChange {
                controller, value, ..
            } = message
            {
                self.controller = Some((controller, value));
            }
        }
    }

    fn synth(voices: usize) -> Synth<Probe> {
        let mut synth = Synth::with_voices(voices, Probe::default);
        synth.prepare(&ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 16,
            num_channels: 1,
        });
        synth
    }

    fn on(key: u8) -> EventKind {
        EventKind::NoteOn {
            note: NoteRef::new(0, key),
            velocity: 1.0,
        }
    }

    fn off(key: u8) -> EventKind {
        EventKind::NoteOff {
            note: NoteRef::new(0, key),
            velocity: 0.0,
        }
    }

    fn controller(controller: u8, value: u8) -> EventKind {
        EventKind::Midi(MidiMessage::ControlChange {
            channel: 0,
            controller,
            value,
        })
    }

    /// Runs one block of `frames` with the given events and returns it.
    fn run(synth: &mut Synth<Probe>, frames: usize, events: &[(u32, EventKind)]) -> Vec<f32> {
        let mut buffer_events = EventBuffer::with_capacity(32);
        for &(offset, kind) in events {
            buffer_events.push(Event { offset, kind }).unwrap();
        }
        let mut buffer = AudioBuffer::<f32>::new(1, frames);
        buffer.channel_mut(0).fill(99.0); // must be replaced, not added to
        synth.process_with(
            &mut buffer,
            &ProcessContext::new(&buffer_events, &Transport::STOPPED),
        );
        buffer.channel(0).to_vec()
    }

    /// The voices' start counts, to see which ones were used.
    fn starts(synth: &mut Synth<Probe>) -> Vec<usize> {
        synth.voices_mut().map(|voice| voice.starts).collect()
    }

    #[test]
    fn notes_start_and_stop_on_their_exact_frame() {
        let mut synth = synth(4);
        let output = run(&mut synth, 8, &[(2, on(10)), (5, off(10))]);
        //                     held on 2,3,4; two-frame tail on 5,6
        assert_eq!(output, [0.0, 0.0, 10.0, 10.0, 10.0, 5.0, 5.0, 0.0]);
        assert_eq!(synth.active_voices(), 0);
    }

    #[test]
    fn notes_sound_together_and_carry_across_blocks() {
        let mut synth = synth(4);
        let first = run(&mut synth, 4, &[(0, on(10)), (2, on(20))]);
        assert_eq!(first, [10.0, 10.0, 30.0, 30.0]);
        assert_eq!(synth.active_voices(), 2);

        let second = run(&mut synth, 4, &[(1, off(10))]);
        assert_eq!(second, [30.0, 25.0, 25.0, 20.0]);
        assert_eq!(synth.active_voices(), 1);

        // A block with no context still renders what is sounding.
        let mut buffer = AudioBuffer::<f32>::new(1, 2);
        synth.process(&mut buffer);
        assert_eq!(buffer.channel(0), &[20.0, 20.0]);
    }

    #[test]
    fn a_full_synth_takes_over_the_least_missed_voice() {
        // Released before held: the fading note goes first.
        let mut synth = synth(2);
        run(&mut synth, 2, &[(0, on(10)), (0, on(20)), (1, off(20))]);
        // 20 still has a frame of its fade left when 30 arrives. Had the
        // held note been taken instead, that frame would be heard and the
        // output would be [40, 30].
        let output = run(&mut synth, 2, &[(0, on(30))]);
        assert_eq!(output, [40.0, 40.0]);

        // All held: the oldest goes.
        let mut synth = self::synth(2);
        let output = run(&mut synth, 2, &[(0, on(10)), (0, on(20)), (1, on(30))]);
        assert_eq!(output, [30.0, 50.0]);
        let chokes: usize = synth.voices_mut().map(|voice| voice.chokes).sum();
        // One from `prepare` per voice and one before each of the three
        // fresh notes, the last of which is the take-over.
        assert_eq!(chokes, 5);
    }

    #[test]
    fn striking_a_sounding_key_again_reuses_its_voice() {
        let mut synth = synth(4);
        run(&mut synth, 4, &[(0, on(10)), (1, on(10)), (2, on(10))]);
        assert_eq!(starts(&mut synth), [3, 0, 0, 0]);
        assert_eq!(synth.active_voices(), 1);
    }

    #[test]
    fn notes_with_identifiers_overlap_on_one_key_and_end_separately() {
        let mut synth = synth(4);
        let first = NoteRef::with_id(0, 10, 1);
        let second = NoteRef::with_id(0, 10, 2);
        let start = |note| EventKind::NoteOn {
            note,
            velocity: 1.0,
        };
        let stop = |note| EventKind::NoteOff {
            note,
            velocity: 0.0,
        };
        let output = run(
            &mut synth,
            6,
            &[(0, start(first)), (0, start(second)), (2, stop(first))],
        );
        assert_eq!(output, [20.0, 20.0, 15.0, 15.0, 10.0, 10.0]);
    }

    #[test]
    fn sustain_pedal_holds_released_notes_until_it_lifts() {
        let mut synth = synth(4);
        let output = run(
            &mut synth,
            8,
            &[
                (0, on(10)),
                (1, controller(64, 127)),
                (2, off(10)),
                (5, controller(64, 0)),
            ],
        );
        assert_eq!(output, [10.0, 10.0, 10.0, 10.0, 10.0, 5.0, 5.0, 0.0]);

        // A key still held when the pedal lifts keeps sounding.
        let output = run(
            &mut synth,
            4,
            &[
                (0, on(10)),
                (0, controller(64, 127)),
                (2, controller(64, 0)),
            ],
        );
        assert_eq!(output, [10.0; 4]);
    }

    #[test]
    fn expression_and_modulation_reach_one_note_and_parameters_reach_all() {
        let mut synth = synth(3);
        let low = NoteRef::new(0, 10);
        run(
            &mut synth,
            4,
            &[
                (0, on(10)),
                (0, on(20)),
                (
                    1,
                    EventKind::NoteExpression {
                        note: low,
                        expression: Expression::Tuning,
                        value: 0.5,
                    },
                ),
                (
                    1,
                    EventKind::ParamModulation {
                        param: CUTOFF,
                        amount: 300.0,
                        note: Some(low),
                    },
                ),
                (
                    2,
                    EventKind::ParamValue {
                        param: CUTOFF,
                        value: 1_000.0,
                    },
                ),
            ],
        );
        let voices: Vec<&mut Probe> = synth.voices_mut().collect();
        assert_eq!((voices[0].tuning, voices[0].modulation), (0.5, 300.0));
        assert_eq!((voices[1].tuning, voices[1].modulation), (0.0, 0.0));
        // The parameter reaches every voice, including the idle one.
        assert!(voices.iter().all(|voice| voice.cutoff == 1_000.0));
    }

    #[test]
    fn modulation_without_a_note_reaches_every_voice() {
        let mut synth = synth(2);
        let all = EventKind::ParamModulation {
            param: CUTOFF,
            amount: -50.0,
            note: None,
        };
        run(&mut synth, 2, &[(0, all)]);
        assert!(synth.voices_mut().all(|voice| voice.modulation == -50.0));
    }

    #[test]
    fn pitch_bend_controllers_and_channel_wide_messages() {
        let mut synth = synth(4);
        synth.set_pitch_bend_range(12.0);
        let other_channel = EventKind::NoteOn {
            note: NoteRef::new(1, 40),
            velocity: 1.0,
        };
        let bend = EventKind::Midi(MidiMessage::PitchBend {
            channel: 0,
            value: 4_096,
        });
        run(
            &mut synth,
            4,
            &[
                (0, on(10)),
                (0, other_channel),
                (1, bend),
                (1, controller(1, 77)),
            ],
        );
        {
            let voices: Vec<&mut Probe> = synth.voices_mut().collect();
            assert_eq!(voices[0].bend, 6.0);
            assert_eq!(voices[0].controller, Some((1, 77)));
            // Channel 1 was not addressed.
            assert_eq!(voices[1].bend, 0.0);
            assert_eq!(voices[1].controller, None);
        }

        // All notes off on channel 0 releases; all sound off cuts.
        let output = run(&mut synth, 4, &[(1, controller(123, 0))]);
        assert_eq!(output, [50.0, 45.0, 45.0, 40.0]);
        let output = run(&mut synth, 2, &[(1, on(10)), (1, controller(120, 0))]);
        assert_eq!(output, [40.0, 40.0]);
    }

    #[test]
    fn a_note_started_later_hears_where_the_wheel_and_controllers_are() {
        let mut synth = synth(4);
        let bend = |value| EventKind::Midi(MidiMessage::PitchBend { channel: 0, value });
        // Nothing is sounding when the wheel and the mod wheel move.
        run(&mut synth, 2, &[(0, bend(-8_192)), (0, controller(1, 90))]);
        let mut expected = Vec::new();
        for key in [10, 20, 30] {
            run(&mut synth, 2, &[(0, on(key))]);
            expected.push((f32::from(key), -2.0, Some((1, 90))));
        }
        // Every voice the notes landed on was told, not just the first.
        let heard: Vec<_> = synth
            .voices_mut()
            .filter(|voice| voice.held)
            .map(|voice| (voice.key, voice.bend, voice.controller))
            .collect();
        assert_eq!(heard, expected);

        // A note on another channel has its own wheel, at rest.
        let elsewhere = EventKind::NoteOn {
            note: NoteRef::new(5, 40),
            velocity: 1.0,
        };
        run(&mut synth, 2, &[(0, elsewhere)]);
        let voice = synth.voices_mut().find(|voice| voice.key == 40.0).unwrap();
        assert_eq!((voice.bend, voice.controller), (0.0, None));
    }

    #[test]
    fn reset_all_controllers_puts_the_wheel_and_controllers_back() {
        let mut synth = synth(2);
        let bend = EventKind::Midi(MidiMessage::PitchBend {
            channel: 0,
            value: 8_191,
        });
        let moves = [(0, bend), (0, controller(1, 90)), (0, controller(7, 100))];
        run(&mut synth, 2, &moves);
        run(&mut synth, 2, &[(0, on(10)), (1, controller(121, 0))]);
        // The sounding note is straightened at once.
        assert_eq!(synth.voices_mut().next().unwrap().bend, 0.0);

        // The next note starts in tune and is told of the volume, which a
        // reset leaves alone, but not of the mod wheel.
        run(&mut synth, 2, &[(0, on(20))]);
        let voice = synth.voices_mut().find(|voice| voice.key == 20.0).unwrap();
        assert_eq!((voice.bend, voice.controller), (0.0, Some((7, 100))));
    }

    #[test]
    fn choke_reset_and_all_notes_off() {
        let mut synth = synth(4);
        let choke = EventKind::NoteChoke {
            note: NoteRef::new(0, 10),
        };
        let output = run(&mut synth, 4, &[(0, on(10)), (0, on(20)), (2, choke)]);
        assert_eq!(output, [30.0, 30.0, 20.0, 20.0]);

        synth.all_notes_off();
        assert_eq!(run(&mut synth, 4, &[]), [10.0, 10.0, 0.0, 0.0]);

        synth.note_on(NoteRef::new(0, 50), 1.0);
        synth.reset();
        assert_eq!(run(&mut synth, 2, &[]), [0.0, 0.0]);
        assert_eq!(synth.polyphony(), 4);
    }

    #[test]
    fn a_free_voice_is_chosen_by_its_place_not_by_its_age() {
        let mut synth = synth(4);
        // Three notes on the first three voices, released, and their tails
        // over.
        run(&mut synth, 2, &[(0, on(10)), (0, on(20)), (0, on(30))]);
        run(&mut synth, 4, &[(0, off(10)), (0, off(20)), (0, off(30))]);
        assert_eq!(synth.active_voices(), 0);
        // The next note goes where it would in a new synth, the first voice,
        // not to the voice that has gone longest without a note, the fourth.
        run(&mut synth, 2, &[(0, on(40)), (0, on(50))]);
        assert_eq!(starts(&mut synth), [2, 2, 1, 0]);
        // Stealing still takes the oldest note: with all four held, a fifth
        // replaces 40, the oldest.
        run(&mut synth, 2, &[(0, on(60)), (0, on(70))]);
        let keys: Vec<f32> = synth.voices_mut().map(|voice| voice.key).collect();
        assert_eq!(keys, [40.0, 50.0, 60.0, 70.0]);
        run(&mut synth, 2, &[(0, on(80))]);
        let keys: Vec<f32> = synth.voices_mut().map(|voice| voice.key).collect();
        assert_eq!(keys, [80.0, 50.0, 60.0, 70.0]);
    }

    #[test]
    fn a_reset_forgets_the_notes_and_their_ages_but_keeps_the_controllers() {
        let bend = EventKind::Midi(MidiMessage::PitchBend {
            channel: 0,
            value: 4_096,
        });
        let wheels = [(0, bend), (0, controller(1, 90))];
        let mut used = synth(4);
        run(&mut used, 4, &wheels);
        run(
            &mut used,
            4,
            &[
                (0, controller(64, 127)),
                (1, on(10)),
                (1, on(20)),
                (1, on(30)),
            ],
        );
        run(&mut used, 4, &[(0, on(40)), (2, off(10))]);
        used.reset();

        // A new synth whose wheel and mod wheel were moved the same way, and
        // that never played a note.
        let mut new = synth(4);
        run(&mut new, 4, &wheels);
        let notes = [(0, on(50)), (1, on(60)), (3, off(50))];
        let heard = run(&mut used, 8, &notes);
        assert_eq!(heard, run(&mut new, 8, &notes));
        // The notes land on the first two voices, as in the new synth; the
        // pedal is up, so the released note fades; and the wheel (half way
        // up, one semitone) and the mod wheel are where they were left.
        for synth in [&mut used, &mut new] {
            let state: Vec<_> = synth
                .voices_mut()
                .map(|voice| (voice.key, voice.held, voice.bend, voice.controller))
                .collect();
            assert_eq!(
                state[..2],
                [
                    (50.0, false, 1.0, Some((1, 90))),
                    (60.0, true, 1.0, Some((1, 90)))
                ]
            );
        }
    }

    #[test]
    fn velocities_reach_the_voices_in_range() {
        for (sent, received) in [
            (0.5, 0.5_f32),
            (1.0, 1.0),
            (0.0, 0.0),
            (1.5, 1.0),
            (f32::MAX, 1.0),
            (-0.25, 0.0),
            (f32::INFINITY, 0.0),
            (f32::NEG_INFINITY, 0.0),
            (f32::NAN, 0.0),
        ] {
            let mut synth = synth(1);
            let note = NoteRef::new(0, 10);
            run(
                &mut synth,
                2,
                &[
                    (
                        0,
                        EventKind::NoteOn {
                            note,
                            velocity: sent,
                        },
                    ),
                    (
                        1,
                        EventKind::NoteOff {
                            note,
                            velocity: sent,
                        },
                    ),
                ],
            );
            let voice = synth.voices_mut().next().unwrap();
            assert_eq!(
                (voice.velocity.to_bits(), voice.release.to_bits()),
                (received.to_bits(), received.to_bits()),
                "{sent}"
            );
        }
    }

    #[test]
    fn a_synth_with_no_voices_stays_silent() {
        let mut synth = synth(0);
        assert_eq!(run(&mut synth, 2, &[(0, on(10))]), [0.0, 0.0]);
    }
}
