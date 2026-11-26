use manguebyte_core::governor::{Allowance, Clock, Governor};
use manguebyte_core::{AudioBuffer, ParamId, Sample};
use manguebyte_midi::MidiMessage;

use crate::event::{Event, EventKind, Expression, NoteRef};
use crate::tuning::Tuning;
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

    /// Starts a note, accented or not.
    ///
    /// An accent is a second dimension of how hard a note is played, beside
    /// its velocity: the accent of a drum machine or a step sequencer, or a
    /// note Aratu writes in capitals. What it changes is up to the voice;
    /// usually the note is louder and brighter. MIDI has no message for it,
    /// so it arrives only through [`Synth::note_on_accented`]; every other
    /// way of starting a note calls this with `accent` false.
    ///
    /// The default ignores the accent and calls
    /// [`note_on`](Self::note_on), so a voice that has no use for accents
    /// need not know they exist. A voice that does implement this should
    /// make `note_on(note, velocity)` the same as
    /// `note_on_accented(note, velocity, false)`.
    fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        let _ = accent;
        self.note_on(note, velocity);
    }

    /// A note takes this voice over while the voice's note is still held:
    /// a slur. Only a [`Synth`] set to [legato](Synth::set_legato) calls
    /// this, when a note starts on a channel where a key is down.
    ///
    /// The voice should move to the new pitch without striking the note
    /// again: a monophonic synthesiser glides there and keeps its
    /// envelopes going, as a slide on a bass-line sequencer does. The
    /// default strikes the note again, as when a sounding key is struck
    /// again: it calls [`note_on_accented`](Self::note_on_accented),
    /// without a [`choke`](Self::choke) first.
    fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        self.note_on_accented(note, velocity, accent);
    }

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

    /// This voice's note alone takes `value` for a parameter, in the
    /// parameter's own unit ([`EventKind::NoteParamValue`], from a host's
    /// per-note automation). Ignored unless overridden.
    ///
    /// The value belongs to the note: a voice keeps it apart from what
    /// [`set_param`](Self::set_param) gives every voice, uses it in its
    /// place until the next `set_param` of the same parameter, and forgets
    /// it in [`choke`](Self::choke), which every fresh note starts with, so
    /// that the next note on the voice follows the parameter again.
    fn set_note_param(&mut self, param: ParamId, value: f64) {
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

    /// The tuning the voice plays in from now on, given by
    /// [`Synth::set_tuning`] to every voice, sounding or not.
    ///
    /// A voice keeps its own copy (with [`Clone::clone_from`], which does
    /// not allocate: a [`Tuning`] owns no heap memory) and takes its key's
    /// pitch from it, [`Tuning::key_pitch`], where it would otherwise take
    /// the key number itself. The instruments of this crate do that and
    /// add pitch bend, glides, vibrato and their own transpositions on top
    /// as intervals in semitones, as synthesizers that follow the MIDI
    /// Tuning Standard do; a voice that would rather bend through the
    /// tuning's keys can use [`Tuning::frequency`] of the bent key. The
    /// instruments apply a new tuning from their next note at the latest.
    ///
    /// The default ignores the tuning, so a voice that does not override
    /// this always plays in 12-tone equal temperament with A4 at 440 Hz,
    /// which is also the tuning every voice starts with.
    fn set_tuning(&mut self, tuning: &Tuning) {
        let _ = tuning;
    }

    /// How many tiers of quality the voice can render its note at, for a
    /// [`Synth`] spending a budget ([`Synth::process_budgeted`]): tier 0 is
    /// full quality, and each one after it cheaper. At least 1, which is the
    /// default: the voice is rendered whole or not at all.
    fn tier_count(&self) -> usize {
        1
    }

    /// What rendering the voice's note at `tier` costs a block, in the cost
    /// units of the instrument (ADR 0018): only how the costs of its voices
    /// and tiers compare matters, and a later tier must not cost more than
    /// an earlier one. A cost that is not a finite number, or is below 0,
    /// counts as 0. 1 by default, so a synth of such voices spends its
    /// budget as a number of voices.
    fn tier_cost(&self, tier: usize) -> f64 {
        let _ = tier;
        1.0
    }

    /// Renders at `tier`, below [`tier_count`](Self::tier_count), from the
    /// next frame on. The voice crosses from what it was doing to the new
    /// tier over `fade_frames` frames, so that nothing clicks; with 0 it
    /// changes at once, which the synth asks only of a voice that has not
    /// been heard since it changed. [`choke`](Self::choke) puts a voice back
    /// on tier 0. Does nothing by default, which suits a voice of one tier.
    fn set_tier(&mut self, tier: usize, fade_frames: usize) {
        let _ = (tier, fade_frames);
    }

    /// How audible the note is next to the others, in any unit that grows
    /// with it (a gain, say): when the budget does not cover every voice,
    /// the synth gives it first to the keys held, then to the notes the
    /// pedal holds, then to those released, and within each to the more
    /// audible, and then to the newer. A voice that renders at full quality
    /// counts as a quarter more audible than it says, so that two of nearly
    /// the same audibility do not take turns. 1 by default.
    fn audibility(&self) -> f32 {
        1.0
    }

    /// Whether the note, with its key held, can be left unrendered for a
    /// while and taken up again later, as the sustained tone of an organ
    /// can. A synth that cannot afford such a voice suspends it: it fades it
    /// out, stops rendering it, and fades it back in once the budget allows.
    /// One that answers `false`, as by default, is stolen instead: faded
    /// out, choked and free for another note.
    fn can_suspend(&self) -> bool {
        false
    }

    /// `frames` frames of the timeline went by while the voice was
    /// suspended and not rendered, for what a voice must keep in time with
    /// the others. Does nothing by default.
    fn skip_suspended(&mut self, frames: usize) {
        let _ = frames;
    }
}

/// How long a [`Synth`] spending a budget takes to fade out a voice it can
/// no longer afford and to fade it back in, and what it gives
/// [`Voice::set_tier`] to cross-fade over, in seconds.
pub const BUDGET_FADE_SECONDS: f64 = 0.006;

/// How much more audible a voice rendering at full quality counts than it
/// says it is, when a budget is shared out.
const KEEP_BONUS: f32 = 1.25;

/// What the budget gives a voice.
#[derive(Debug, Clone, Copy)]
struct Share {
    /// The tier asked of the voice.
    tier: usize,
    /// Whether the voice is to be heard.
    wanted: bool,
    /// How far it is on the way to silence, in frames of the fade: 0 is full
    /// level, and the fade's length is silent.
    depth: usize,
    /// A note not yet rendered, which starts as it is shared out, with no
    /// fade.
    fresh: bool,
}

/// What every voice has with no budget: all of it.
const WHOLE: Share = Share {
    tier: 0,
    wanted: true,
    depth: 0,
    fresh: false,
};

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

/// A voice's place in the order a budget is shared out in.
#[derive(Debug, Clone, Copy)]
struct Rank {
    /// Held, then held by the pedal, then released.
    stage: u8,
    audibility: f32,
    /// When its note started: higher is newer.
    started: u64,
    index: usize,
}

#[derive(Debug)]
struct Slot<V> {
    voice: V,
    note: NoteRef,
    stage: Stage,
    /// Value of the synth's clock when the note started. Lower is older.
    started: u64,
    share: Share,
}

impl<V> Slot<V> {
    /// Whether the budget has silenced the voice, so that it is not
    /// rendered.
    fn is_silenced(&self, fade: usize) -> bool {
        self.stage != Stage::Idle && !self.share.wanted && self.share.depth >= fade
    }

    /// Stops the voice at once and frees it.
    fn stop<S: Sample>(&mut self)
    where
        V: Voice<S>,
    {
        self.voice.choke();
        self.stage = Stage::Idle;
        self.share = WHOLE;
    }

    /// Whether a silenced voice waits for the budget instead of being
    /// stolen.
    fn may_wait<S: Sample>(&self) -> bool
    where
        V: Voice<S>,
    {
        matches!(self.stage, Stage::Held | Stage::Sustained) && self.voice.can_suspend()
    }

    /// What the voice costs on the tier it is on, if it makes sound.
    fn cost<S: Sample>(&self, tier: usize) -> f64
    where
        V: Voice<S>,
    {
        let cost = self.voice.tier_cost(tier);
        if cost.is_finite() { cost.max(0.0) } else { 0.0 }
    }

    /// Renders a voice that is fading out or back in, or is silenced, into
    /// `piece`, through `scratch`. Returns the frames of `piece` it rendered.
    fn render_fading<S: Sample>(
        &mut self,
        piece: &mut AudioBuffer<S>,
        scratch: &mut AudioBuffer<S>,
        fade: usize,
    ) -> usize
    where
        V: Voice<S>,
    {
        let frames = piece.num_frames();
        let mut done = 0;
        while done < frames {
            let wanted = self.share.wanted;
            let left = if wanted {
                self.share.depth
            } else {
                fade.saturating_sub(self.share.depth)
            };
            if left == 0 {
                if wanted {
                    piece.windowed(done..frames, |rest| self.voice.render(rest));
                    return frames;
                }
                break;
            }
            let count = left.min(frames - done).min(scratch.capacity());
            if count == 0 || scratch.num_channels() != piece.num_channels() {
                // A synth that was not prepared for such blocks has nowhere
                // to fade in: the voice changes at once.
                self.share.depth = if wanted { 0 } else { fade };
                continue;
            }
            scratch.set_num_frames(count);
            scratch.clear();
            self.voice.render(scratch);
            let depth = self.share.depth;
            for (out, from) in piece.iter_channels_mut().zip(scratch.iter_channels()) {
                for (frame, (out, &sample)) in out[done..].iter_mut().zip(from).enumerate() {
                    let depth = if wanted {
                        depth.saturating_sub(frame + 1)
                    } else {
                        depth + frame + 1
                    };
                    let gain = 1.0 - depth as f64 / fade as f64;
                    *out += sample * S::from_f64(gain);
                }
            }
            self.share.depth = if wanted { depth - count } else { depth + count };
            done += count;
        }
        if self.is_silenced(fade) {
            if self.may_wait() {
                self.voice.skip_suspended(frames - done);
            } else {
                self.stop();
            }
        }
        done
    }
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
/// - **Parameter changes** reach every voice, and a value given to one
///   note alone ([`EventKind::NoteParamValue`]) only the voice playing it,
///   until the next change of the parameter for every note.
/// - **The sustain pedal** (controller 64) keeps released notes sounding
///   until it is lifted, per channel.
/// - **Pitch bend** is converted to semitones using
///   [`set_pitch_bend_range`](Self::set_pitch_bend_range).
/// - **The wheel and the controllers are remembered** per channel. A voice
///   starting a note is told where they stand, so the note sounds the same
///   whichever voice it lands on and whether or not that voice was sounding
///   when the wheel moved. Controller 121 puts them back.
/// - Controller 120 silences a whole channel. Controllers 123 to 127 release
///   one: all notes off, and the four mode messages that MIDI makes include
///   it, whose modes the synth does not otherwise follow.
/// - **The tuning** ([`set_tuning`](Self::set_tuning)) reaches every voice,
///   and a note on a key it leaves unmapped does not start.
///
/// When every voice is busy, a new note takes over the voice that will be
/// missed least: one that is already fading out if there is one, then one
/// held only by the pedal, then the oldest held note.
///
/// Two things have no MIDI message and are reached by calling the synth
/// directly: an accent ([`note_on_accented`](Self::note_on_accented)), and
/// legato playing ([`set_legato`](Self::set_legato)), in which a note
/// struck while another is held takes that note's voice over and slurs
/// from it instead of starting a voice of its own.
///
/// The output replaces whatever was in the buffer.
///
/// # A budget
///
/// A synth can render within the budget of a processor budget governor
/// ([`manguebyte_core::governor`], ADR 0018), through
/// [`process_governed`](Self::process_governed), or
/// [`process_budgeted`](Self::process_budgeted) with an allowance of the
/// caller's. Nothing changes unless one of them is called.
///
/// At the start of each block, and where a note starts or ends inside it,
/// the budget is shared out in order of priority (see
/// [`Voice::audibility`]): each voice gets the best of its
/// [tiers](Voice::tier_count) that still fits. A voice that moves to
/// another tier cross-fades itself ([`Voice::set_tier`]). One that no tier
/// fits for is faded out over [`BUDGET_FADE_SECONDS`] and then suspended, to
/// be faded back in later, if it [can be](Voice::can_suspend), and stolen if
/// not; a new note that nothing fits for makes no sound at all. A voice
/// fading out is rendered for as long as its fade lasts, over the budget if
/// need be, and what it costs is counted as spent.
///
/// A block whose [`ProcessContext::offline`] is set renders everything,
/// whatever the budget. While the budget covers every voice at tier 0, the
/// output is that of [`process_with`](Processor::process_with), bit for
/// bit. A budget that does not cover them is shared out where blocks start,
/// so what is heard then depends on where the host cuts them, and with a
/// governor on a real clock, on the machine. A call to `process_with` after
/// a budget brings every voice back to full quality, with the same fades.
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
    /// Whether a note struck while another is held slurs from it.
    legato: bool,
    /// How many of the voices, from the first, new notes may take.
    voice_limit: usize,
    /// What every voice was last given by `set_tuning`.
    tuning: Tuning,
    sample_rate: f64,
    /// Where a voice that is fading under a budget renders.
    scratch: AudioBuffer<S>,
    /// The voices in order of priority, as a budget is shared out; has room
    /// for them all.
    order: Vec<Rank>,
    /// The fade of a voice the budget cuts, in frames.
    fade: usize,
    /// Whether some voice is not at full quality, for a budget's sake.
    thinned: bool,
    /// Whether notes started or ended since the budget was shared out.
    changed: bool,
}

impl<S: Sample, V: Voice<S>> Synth<V, S> {
    /// Creates a synth from a set of voices. Its polyphony is their number.
    pub fn new(voices: impl IntoIterator<Item = V>) -> Self {
        let slots: Vec<Slot<V>> = voices
            .into_iter()
            .map(|voice| Slot {
                voice,
                note: NoteRef::new(0, 0),
                stage: Stage::Idle,
                started: 0,
                share: WHOLE,
            })
            .collect();
        Self {
            voice_limit: slots.len(),
            order: Vec::with_capacity(slots.len()),
            slots,
            clock: 0,
            sustain: [false; 16],
            bend: [0; 16],
            controllers: [[UNMOVED; 128]; 16],
            bend_range: 2.0,
            legato: false,
            tuning: Tuning::default(),
            sample_rate: 48_000.0,
            scratch: AudioBuffer::new(0, 0),
            fade: (BUDGET_FADE_SECONDS * 48_000.0) as usize,
            thinned: false,
            changed: false,
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

    /// Turns legato playing on or off. Off is the default.
    ///
    /// With legato on, a note that starts while a key is held on its
    /// channel does not get a voice of its own: it takes over the voice of
    /// the newest held note on that channel through [`Voice::legato`], and
    /// the note it replaces is forgotten, so that releasing that note's key
    /// later changes nothing. A voice that implements `legato` glides to
    /// the new pitch without striking again, as a slide does on a
    /// monophonic bass synthesiser; one that does not strikes the note
    /// again on the same voice. A note that starts while no key is held on
    /// its channel (released notes and notes held only by the sustain
    /// pedal do not count) starts as usual.
    ///
    /// Legato makes each channel monophonic while its notes overlap; a
    /// synth of one voice plays like a monophonic synthesiser. Releasing
    /// the newest note does not return to an older one still held.
    pub fn set_legato(&mut self, legato: bool) {
        self.legato = legato;
    }

    /// Whether legato playing is on. See [`set_legato`](Self::set_legato).
    pub fn is_legato(&self) -> bool {
        self.legato
    }

    /// Gives every voice a new tuning, sounding or not, through
    /// [`Voice::set_tuning`]. Real-time safe: the tuning is copied into the
    /// synth and into each voice, which allocates nothing, so a tuning
    /// built off the audio thread can be handed over through a queue and
    /// applied at the start of a block (see [the tuning
    /// module](crate::tuning)).
    ///
    /// From then on a note on a key the tuning leaves unmapped
    /// ([`Tuning::is_mapped`]) does not start; notes already sounding on
    /// such keys carry on until released. A new synth, and every voice in
    /// it, plays 12-tone equal temperament with A4 at 440 Hz.
    pub fn set_tuning(&mut self, tuning: &Tuning) {
        self.tuning.clone_from(tuning);
        for slot in &mut self.slots {
            slot.voice.set_tuning(&self.tuning);
        }
    }

    /// The tuning last given to [`set_tuning`](Self::set_tuning).
    pub fn tuning(&self) -> &Tuning {
        &self.tuning
    }

    /// How many notes can sound at once.
    pub fn polyphony(&self) -> usize {
        self.slots.len()
    }

    /// Lets new notes take only the first `limit` voices, at least one and
    /// at most [`polyphony`](Self::polyphony): fewer notes sound at once,
    /// and a note beyond them takes one of them over as usual. Real-time
    /// safe, so that a synth made with as many voices as it may ever need
    /// can play fewer where each costs more. Voices beyond a new limit
    /// that are sounding ring on until they are done; a note struck again
    /// on one of them still restarts it. Every voice by default.
    pub fn set_voice_limit(&mut self, limit: usize) {
        self.voice_limit = limit.clamp(1, self.slots.len().max(1));
    }

    /// How many voices new notes may take. See
    /// [`set_voice_limit`](Self::set_voice_limit).
    pub fn voice_limit(&self) -> usize {
        self.voice_limit
    }

    /// How many voices are sounding right now, not counting those a budget
    /// has [suspended](Self::suspended_voices).
    pub fn active_voices(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| {
                slot.stage != Stage::Idle && !slot.is_silenced(self.fade) && slot.voice.is_active()
            })
            .count()
    }

    /// How many voices a [budget](Self#a-budget) has on a tier below full
    /// quality, those still sounding.
    pub fn demoted_voices(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.stage != Stage::Idle && slot.share.wanted && slot.share.tier > 0)
            .count()
    }

    /// How many notes a [budget](Self#a-budget) has silenced, waiting to be
    /// taken up again.
    pub fn suspended_voices(&self) -> usize {
        self.slots
            .iter()
            .filter(|slot| slot.is_silenced(self.fade))
            .count()
    }

    /// Renders a block as [`process_with`](Processor::process_with) does,
    /// within what is left of `allowance`, to which it adds what it spent,
    /// marked [limited](Allowance::limited) if a voice got less than full
    /// quality. See [a budget](Self#a-budget). Costs are units a block, as
    /// [`Voice::tier_cost`] states them: a voice rendered for part of the
    /// block spends that part of its cost. Real-time safe.
    ///
    /// Several instruments can share one allowance: each spends what those
    /// before it left. A block whose [`ProcessContext::offline`] is set is
    /// rendered whole whatever is left.
    ///
    /// ```
    /// use manguebyte_core::AudioBuffer;
    /// use manguebyte_core::governor::Allowance;
    /// use manguebyte_dsp::event::NoteRef;
    /// use manguebyte_dsp::instruments::{OrganParams, OrganVoice};
    /// use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth};
    ///
    /// let mut organ = Synth::new(OrganVoice::voices(8, OrganParams::full()));
    /// organ.prepare(&ProcessSpec { sample_rate: 48_000.0, max_block_size: 128, num_channels: 1 });
    /// for key in [48, 55, 64, 67] {
    ///     organ.note_on(NoteRef::new(0, key), 1.0);
    /// }
    /// // Room for two notes whole: the other two make do or wait.
    /// let mut allowance = Allowance::new(2.0 * OrganVoice::FULL_COST);
    /// let mut block = AudioBuffer::<f32>::new(1, 128);
    /// organ.process_budgeted(&mut block, &ProcessContext::empty(), &mut allowance);
    /// assert!(allowance.limited() && allowance.spent() <= allowance.budget());
    /// assert_eq!(organ.active_voices() + organ.suspended_voices(), 4);
    /// ```
    pub fn process_budgeted(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        context: &ProcessContext<'_, S>,
        allowance: &mut Allowance,
    ) {
        let budget = if context.offline {
            f64::INFINITY
        } else {
            allowance.remaining()
        };
        buffer.clear();
        let frames = buffer.num_frames();
        // Cost units times frames.
        let mut spent = 0.0;
        let mut limited = false;
        self.changed = true;
        split_at_events(
            self,
            buffer,
            context.events,
            |synth, event| synth.handle_event(event),
            |synth, piece| {
                if core::mem::take(&mut synth.changed) {
                    limited |= synth.share_out(budget);
                }
                spent += synth.render_shares(piece);
            },
        );
        self.reclaim_finished();
        if frames > 0 {
            allowance.force(spent / frames as f64);
        }
        if limited {
            allowance.mark_limited();
        }
        self.thinned = self.slots.iter().any(|slot| {
            slot.stage != Stage::Idle
                && (slot.share.depth > 0 || slot.share.tier > 0 || !slot.share.wanted)
        });
    }

    /// Renders a block within `governor`'s budget, and has it learn from the
    /// block: [`process_budgeted`](Self::process_budgeted) inside
    /// [`Governor::govern`]. The governor takes the sample rate the synth
    /// was prepared for. Offline ([`ProcessContext::offline`], or a
    /// governor set offline) everything is rendered, the clock is not read
    /// and nothing is learnt. Real-time safe.
    ///
    /// The governor should measure this synth alone. For several
    /// instruments on one governor, call [`Governor::govern`] once and give
    /// each the allowance.
    pub fn process_governed<C: Clock>(
        &mut self,
        buffer: &mut AudioBuffer<S>,
        context: &ProcessContext<'_, S>,
        governor: &mut Governor<C>,
    ) {
        governor.set_sample_rate(self.sample_rate);
        governor.govern(buffer.num_frames(), context.offline, |allowance| {
            self.process_budgeted(buffer, context, allowance);
        });
    }

    /// Shares `budget` out among the voices, the most important first.
    /// Returns whether some voice got less than full quality.
    fn share_out(&mut self, budget: f64) -> bool {
        let fade = self.fade;
        for slot in &mut self.slots {
            if slot.is_silenced(fade) && !slot.may_wait() {
                slot.stop();
            }
        }
        self.order.clear();
        // Each voice is asked once, so that the order cannot change while
        // it is sorted.
        for (index, slot) in self.slots.iter().enumerate() {
            let stage = match slot.stage {
                Stage::Idle => continue,
                Stage::Held => 0,
                Stage::Sustained => 1,
                Stage::Released => 2,
            };
            let audibility = slot.voice.audibility();
            let mut audibility = if audibility.is_nan() { 0.0 } else { audibility };
            if slot.share.wanted && slot.share.tier == 0 {
                audibility *= KEEP_BONUS;
            }
            // Within the room made for every voice: nothing grows.
            self.order.push(Rank {
                stage,
                audibility,
                started: slot.started,
                index,
            });
        }
        // Sorts in place, with no memory of its own.
        self.order.sort_unstable_by(|a, b| {
            a.stage
                .cmp(&b.stage)
                .then(b.audibility.total_cmp(&a.audibility))
                .then(b.started.cmp(&a.started))
                .then(a.index.cmp(&b.index))
        });

        let mut allowance = Allowance::new(budget);
        let mut limited = false;
        for rank in &self.order {
            let slot = &mut self.slots[rank.index];
            let fresh = core::mem::take(&mut slot.share.fresh);
            if !slot.voice.is_active() {
                continue;
            }
            let tiers = slot.voice.tier_count().max(1);
            let fits = (0..tiers).find(|&tier| allowance.try_take(slot.cost(tier)));
            match fits {
                Some(tier) => {
                    if tier != slot.share.tier {
                        let unheard = fresh || slot.share.depth >= fade;
                        slot.voice.set_tier(tier, if unheard { 0 } else { fade });
                        slot.share.tier = tier;
                    }
                    slot.share.wanted = true;
                    limited |= tier > 0;
                }
                None => {
                    limited = true;
                    slot.share.wanted = false;
                    if fresh {
                        slot.share.depth = fade;
                        if !slot.may_wait() {
                            slot.stop();
                        }
                    }
                }
            }
        }
        limited
    }

    /// Adds every voice to `piece` as its share says. Returns the cost units
    /// spent, times the frames each was spent for.
    fn render_shares(&mut self, piece: &mut AudioBuffer<S>) -> f64 {
        let frames = piece.num_frames();
        let mut spent = 0.0;
        for slot in &mut self.slots {
            if slot.stage == Stage::Idle {
                continue;
            }
            let cost = if slot.voice.is_active() {
                slot.cost(slot.share.tier)
            } else {
                0.0
            };
            let rendered = if slot.share.wanted && slot.share.depth == 0 {
                slot.voice.render(piece);
                frames
            } else {
                slot.render_fading(piece, &mut self.scratch, self.fade)
            };
            spent += cost * rendered as f64;
        }
        spent
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
        self.note_on_accented(note, velocity, false);
    }

    /// Starts a note now, accented or not. Real-time safe.
    ///
    /// This is [`note_on`](Self::note_on) with an accent, which reaches the
    /// voice through [`Voice::note_on_accented`] (or [`Voice::legato`]).
    /// The event stream has no accent: a note started by an event is never
    /// accented.
    ///
    /// A note on a key the tuning leaves unmapped is ignored.
    pub fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
        if !self.tuning.is_mapped(note.key) {
            return;
        }
        let velocity = velocity_in_range(velocity);
        self.reclaim_finished();
        self.clock += 1;
        self.changed = true;

        if self.legato {
            // The newest note held on this channel, whatever its key.
            let held = self
                .slots
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.stage == Stage::Held && slot.note.channel == note.channel)
                .max_by_key(|(_, slot)| slot.started)
                .map(|(index, _)| index);
            if let Some(index) = held {
                let slot = &mut self.slots[index];
                slot.note = note;
                slot.started = self.clock;
                slot.voice.legato(note, velocity, accent);
                return;
            }
        }

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

        let fade = self.fade;
        let slot = &mut self.slots[index];
        // A voice a budget silenced has nothing to carry on with: struck
        // again, it starts as one taken over.
        let silenced = slot.is_silenced(fade);
        if same_note.is_none() || silenced {
            // A fresh note always starts from a clean voice, whether the
            // voice was idle or is being taken over. See `Voice::choke`.
            slot.voice.choke();
            if silenced {
                // What the first choke may have kept to fade out was not
                // heard, and must not be heard now.
                slot.voice.choke();
            }
            // A voice still partly faded by a budget comes back up from
            // where it is; any other starts at full level.
            let depth = if slot.stage == Stage::Idle || silenced {
                0
            } else {
                slot.share.depth
            };
            slot.share = Share {
                depth,
                fresh: depth == 0,
                ..WHOLE
            };
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
        } else {
            slot.share.wanted = true;
        }
        slot.note = note;
        slot.stage = Stage::Held;
        slot.started = self.clock;
        slot.voice.note_on_accented(note, velocity, accent);
    }

    /// Releases a note now. Real-time safe. The velocity is limited as for
    /// [`note_on`](Self::note_on).
    pub fn note_off(&mut self, note: NoteRef, velocity: f32) {
        let velocity = velocity_in_range(velocity);
        let pedal_down = self.sustain[usize::from(note.channel & 0x0F)];
        self.changed = true;
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
        self.changed = true;
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
                self.changed = true;
                for slot in self.sounding(move |slot| note.matches(&slot.note)) {
                    slot.stop();
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
            EventKind::NoteParamValue { param, value, note } => {
                for slot in self.sounding(move |slot| note.matches(&slot.note)) {
                    slot.voice.set_note_param(param, value);
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
                self.changed = true;
                for slot in self.sounding(move |slot| slot.note.channel == channel) {
                    slot.stop();
                }
            }
            MidiMessage::ControlChange {
                channel,
                controller: 123..=127,
                ..
            } => {
                self.sustain[usize::from(channel & 0x0F)] = false;
                self.changed = true;
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
        self.changed = true;
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
                slot.share = WHOLE;
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
        let cost = |slot: &Slot<V>| match slot.stage {
            Stage::Idle => 0,
            // A note a budget has silenced is not heard at all.
            _ if slot.is_silenced(self.fade) => 1,
            Stage::Released => 2,
            Stage::Sustained => 3,
            Stage::Held => 4,
        };
        self.slots[..self.voice_limit.min(self.slots.len())]
            .iter()
            .enumerate()
            .min_by_key(|(_, slot)| {
                let age = if slot.stage == Stage::Idle {
                    0
                } else {
                    slot.started
                };
                (cost(slot), age)
            })
            .map(|(index, _)| index)
    }
}

impl<S: Sample, V: Voice<S>> Processor<S> for Synth<V, S> {
    fn prepare(&mut self, spec: &ProcessSpec) {
        for slot in &mut self.slots {
            slot.voice.prepare(spec);
        }
        if spec.sample_rate.is_finite() && spec.sample_rate > 0.0 {
            self.sample_rate = spec.sample_rate;
        }
        self.fade = ((BUDGET_FADE_SECONDS * self.sample_rate).round() as usize).max(1);
        self.scratch = AudioBuffer::new(spec.num_channels, spec.max_block_size);
        self.reset();
    }

    fn process(&mut self, buffer: &mut AudioBuffer<S>) {
        self.process_with(buffer, &ProcessContext::empty());
    }

    fn process_with(&mut self, buffer: &mut AudioBuffer<S>, context: &ProcessContext<'_, S>) {
        if self.thinned {
            // Voices a budget thinned come back to full quality.
            let mut allowance = Allowance::new(f64::INFINITY);
            self.process_budgeted(buffer, context, &mut allowance);
            return;
        }
        buffer.clear();
        split_at_events(
            self,
            buffer,
            context.events,
            |synth, event| synth.handle_event(event),
            |synth, piece| {
                for slot in &mut synth.slots {
                    if slot.stage != Stage::Idle {
                        slot.share.fresh = false;
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
        self.thinned = false;
        for slot in &mut self.slots {
            slot.stop();
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
        accent: bool,
        legatos: usize,
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
        fn note_on_accented(&mut self, note: NoteRef, velocity: f32, accent: bool) {
            self.accent = accent;
            self.note_on(note, velocity);
        }
        fn legato(&mut self, note: NoteRef, velocity: f32, accent: bool) {
            // Slurs: the pitch changes, nothing is struck again.
            self.key = f32::from(note.key);
            self.velocity = velocity;
            self.accent = accent;
            self.legatos += 1;
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
        fn set_note_param(&mut self, param: ParamId, value: f64) {
            // Kept in the same place: a later value for every voice
            // replaces it.
            self.set_param(param, value);
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
    fn a_voice_limit_plays_fewer_notes_on_the_first_voices() {
        let mut synth = synth(4);
        assert_eq!(synth.voice_limit(), 4);
        synth.set_voice_limit(2);
        assert_eq!((synth.voice_limit(), synth.polyphony()), (2, 4));
        // Three notes on two voices: the third takes the oldest over.
        let output = run(&mut synth, 1, &[(0, on(10)), (0, on(20)), (0, on(30))]);
        assert_eq!(output, [50.0]);
        assert_eq!(starts(&mut synth), [2, 1, 0, 0]);
        // Raised again, the other voices take notes; a note beyond a
        // lowered limit rings on.
        synth.set_voice_limit(9);
        assert_eq!(synth.voice_limit(), 4);
        run(&mut synth, 1, &[(0, on(40)), (0, on(50))]);
        assert_eq!(synth.active_voices(), 4);
        synth.set_voice_limit(0);
        assert_eq!(synth.voice_limit(), 1);
        assert_eq!(run(&mut synth, 1, &[]), [30.0 + 20.0 + 40.0 + 50.0]);
        assert_eq!(starts(&mut synth), [2, 1, 1, 1]);
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
    fn a_value_for_one_note_reaches_its_voice_until_the_parameter_moves() {
        let mut synth = synth(3);
        let low = NoteRef::new(0, 10);
        run(
            &mut synth,
            3,
            &[
                (0, on(10)),
                (0, on(20)),
                (
                    1,
                    EventKind::NoteParamValue {
                        param: CUTOFF,
                        value: 250.0,
                        note: low,
                    },
                ),
            ],
        );
        {
            let voices: Vec<&mut Probe> = synth.voices_mut().collect();
            assert_eq!(voices[0].cutoff, 250.0);
            // Neither the other note nor the idle voice.
            assert_eq!((voices[1].cutoff, voices[2].cutoff), (0.0, 0.0));
        }
        let all = EventKind::ParamValue {
            param: CUTOFF,
            value: 900.0,
        };
        run(&mut synth, 1, &[(0, all)]);
        assert!(synth.voices_mut().all(|voice| voice.cutoff == 900.0));
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
    fn the_mode_messages_release_a_channel_as_all_notes_off_does() {
        let played = |number| {
            let mut synth = synth(4);
            let output = run(
                &mut synth,
                6,
                &[(0, on(10)), (0, on(20)), (2, controller(number, 0))],
            );
            (output, synth.active_voices())
        };
        let expected = played(123);
        assert_eq!(expected.1, 0, "released and faded");
        for mode in 124..=127 {
            assert_eq!(played(mode), expected, "controller {mode}");
        }
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

    #[test]
    fn an_accent_reaches_the_voice_and_events_are_never_accented() {
        let mut synth = synth(2);
        synth.note_on_accented(NoteRef::new(0, 10), 0.5, true);
        let voice = synth.voices_mut().next().unwrap();
        assert!(voice.accent);
        assert_eq!((voice.key, voice.velocity, voice.starts), (10.0, 0.5, 1));

        // The velocity is limited as for a plain note.
        synth.note_on_accented(NoteRef::new(0, 20), f32::NAN, true);
        let voice = synth.voices_mut().nth(1).unwrap();
        assert!(voice.accent);
        assert_eq!(voice.velocity, 0.0);

        // A note from the event stream, or a plain note on, is not.
        run(&mut synth, 2, &[(0, on(10))]);
        assert!(!synth.voices_mut().next().unwrap().accent);
        synth.note_on(NoteRef::new(0, 20), 1.0);
        assert!(!synth.voices_mut().nth(1).unwrap().accent);
    }

    #[test]
    fn legato_slurs_overlapping_notes_on_one_voice() {
        let mut synth = synth(4);
        synth.set_legato(true);
        assert!(synth.is_legato());
        // 20 starts while 10 is held: it takes 10's voice over.
        let output = run(&mut synth, 4, &[(0, on(10)), (2, on(20))]);
        assert_eq!(output, [10.0, 10.0, 20.0, 20.0]);
        assert_eq!(starts(&mut synth), [1, 0, 0, 0]);
        assert_eq!(synth.voices_mut().next().unwrap().legatos, 1);
        assert_eq!(synth.active_voices(), 1);

        // Releasing 10 changes nothing: that note was replaced.
        let output = run(&mut synth, 2, &[(0, off(10))]);
        assert_eq!(output, [20.0, 20.0]);
        // An accent travels with the slur.
        synth.note_on_accented(NoteRef::new(0, 30), 1.0, true);
        let voice = synth.voices_mut().next().unwrap();
        assert_eq!((voice.key, voice.accent, voice.legatos), (30.0, true, 2));
        // Releasing the newest note releases the voice.
        let output = run(&mut synth, 4, &[(0, off(30))]);
        assert_eq!(output, [15.0, 15.0, 0.0, 0.0]);

        // With no key down, a note starts as usual, on a fresh voice.
        run(&mut synth, 2, &[(0, on(40))]);
        assert_eq!(starts(&mut synth), [2, 0, 0, 0]);

        // Another channel is not slurred into.
        let elsewhere = EventKind::NoteOn {
            note: NoteRef::new(1, 50),
            velocity: 1.0,
        };
        run(&mut synth, 2, &[(0, elsewhere)]);
        assert_eq!(starts(&mut synth), [2, 1, 0, 0]);
        assert_eq!(synth.active_voices(), 2);

        // A note held only by the pedal is not slurred into either.
        run(&mut synth, 2, &[(0, controller(64, 127)), (0, off(40))]);
        run(&mut synth, 2, &[(0, on(60))]);
        assert_eq!(starts(&mut synth), [2, 1, 1, 0]);

        // Off again, overlapping notes get voices of their own.
        synth.set_legato(false);
        run(&mut synth, 2, &[(0, on(70))]);
        assert_eq!(starts(&mut synth), [2, 1, 1, 1]);
    }

    #[test]
    fn a_voice_with_no_legato_of_its_own_is_struck_again_in_place() {
        /// Counts its starts and chokes, and knows nothing of accents or
        /// legato.
        #[derive(Default)]
        struct Plain {
            key: f32,
            starts: usize,
            chokes: usize,
        }
        impl Voice for Plain {
            fn prepare(&mut self, _: &ProcessSpec) {}
            fn note_on(&mut self, note: NoteRef, _: f32) {
                self.key = f32::from(note.key);
                self.starts += 1;
            }
            fn note_off(&mut self, _: f32) {}
            fn choke(&mut self) {
                self.chokes += 1;
            }
            fn is_active(&self) -> bool {
                true
            }
            fn render(&mut self, _: &mut AudioBuffer) {}
        }

        let mut synth = Synth::with_voices(2, Plain::default);
        synth.prepare(&ProcessSpec {
            sample_rate: 48_000.0,
            max_block_size: 16,
            num_channels: 1,
        });
        synth.set_legato(true);
        synth.note_on_accented(NoteRef::new(0, 10), 1.0, true);
        synth.note_on(NoteRef::new(0, 20), 1.0);
        let voices: Vec<&mut Plain> = synth.voices_mut().collect();
        // Both notes went to the first voice, which was choked once by
        // `prepare` and once before its fresh note, and not before the
        // slur.
        assert_eq!(
            (voices[0].key, voices[0].starts, voices[0].chokes),
            (20.0, 2, 2)
        );
        assert_eq!((voices[1].starts, voices[1].chokes), (0, 1));
    }
}
