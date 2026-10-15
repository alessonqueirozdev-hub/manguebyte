//! The platform-independent engine.

use core::fmt;

use manguebyte_core::{AudioBuffer, NoDenormals};
use manguebyte_dsp::{ProcessSpec, Processor, Voice};
use manguebyte_params::{ParamId, ParamSet};

use crate::chain::ChainDsp;
use crate::drums::DrumKit;
use crate::params::{InstrumentParams, PartParams};
use crate::queue::Pending;
use crate::record::{Event, EventRecord, RECORD_SIZE};
use crate::ring::RingConsumer;
use crate::synth::{SynthInstrument, new_synth};

use manguebyte_dsp::event::NoteRef;

/// What a part plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InstrumentKind {
    /// Code 0: the polyphonic synth, eight voices.
    Synth,
    /// Code 1: the drum kit of [`DRUM_MAP`](crate::DRUM_MAP).
    Drums,
}

impl InstrumentKind {
    /// The code the WebAssembly exports use.
    pub const fn code(self) -> u32 {
        match self {
            Self::Synth => 0,
            Self::Drums => 1,
        }
    }

    /// The kind with this code, if there is one.
    pub const fn from_code(code: u32) -> Option<Self> {
        match code {
            0 => Some(Self::Synth),
            1 => Some(Self::Drums),
            _ => None,
        }
    }
}

/// Which optional stages a part's chain has. The order is fixed:
/// instrument, low-pass, reverb, then gain and pan, which every part has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chain {
    /// The resonant low-pass filter (`lowpass.cutoff`, `lowpass.resonance`).
    pub lowpass: bool,
    /// The reverb (`reverb.mix`, `reverb.decay`).
    pub reverb: bool,
}

impl Chain {
    /// Every stage.
    pub const FULL: Self = Self {
        lowpass: true,
        reverb: true,
    };
    /// Only gain and pan.
    pub const NONE: Self = Self {
        lowpass: false,
        reverb: false,
    };
    /// The bit of [`bits`](Self::bits) for the low-pass.
    pub const LOWPASS_BIT: u32 = 1;
    /// The bit of [`bits`](Self::bits) for the reverb.
    pub const REVERB_BIT: u32 = 2;

    /// The chain as bits, as the WebAssembly exports take it.
    pub const fn bits(self) -> u32 {
        (if self.lowpass { Self::LOWPASS_BIT } else { 0 })
            | (if self.reverb { Self::REVERB_BIT } else { 0 })
    }

    /// The chain these bits stand for, or `None` if an unknown bit is set.
    ///
    /// ```
    /// use manguebyte_web::Chain;
    ///
    /// assert_eq!(Chain::from_bits(3), Some(Chain::FULL));
    /// assert_eq!(Chain::from_bits(0), Some(Chain::NONE));
    /// assert_eq!(Chain::from_bits(4), None);
    /// ```
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !(Self::LOWPASS_BIT | Self::REVERB_BIT) != 0 {
            return None;
        }
        Some(Self {
            lowpass: bits & Self::LOWPASS_BIT != 0,
            reverb: bits & Self::REVERB_BIT != 0,
        })
    }
}

impl Default for Chain {
    /// [`Chain::FULL`].
    fn default() -> Self {
        Self::FULL
    }
}

/// How an engine is built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EngineConfig {
    /// Sample rate in hertz, 8 000 to 384 000.
    pub sample_rate: f64,
    /// The most frames one [`Engine::render`] call produces, 1 to 8 192.
    pub max_quantum: usize,
    /// How many parts can exist at once, 1 to 1 024.
    pub max_parts: usize,
    /// How many events can wait for their frame, 1 to 1 048 576.
    pub pending_capacity: usize,
}

impl EngineConfig {
    /// A configuration for `sample_rate` with room for quanta of 512
    /// frames, 32 parts and 4 096 waiting events.
    pub const fn new(sample_rate: f64) -> Self {
        Self {
            sample_rate,
            max_quantum: 512,
            max_parts: 32,
            pending_capacity: 4_096,
        }
    }

    fn check(&self) -> Result<(), EngineError> {
        let rate = self.sample_rate;
        if !(8_000.0..=384_000.0).contains(&rate) {
            return Err(EngineError::InvalidConfig("sample rate"));
        }
        if !(1..=8_192).contains(&self.max_quantum) {
            return Err(EngineError::InvalidConfig("max quantum"));
        }
        if !(1..=1_024).contains(&self.max_parts) {
            return Err(EngineError::InvalidConfig("max parts"));
        }
        if !(1..=1 << 20).contains(&self.pending_capacity) {
            return Err(EngineError::InvalidConfig("pending capacity"));
        }
        Ok(())
    }
}

/// Why a control call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineError {
    /// A field of the [`EngineConfig`] is out of range; the text names it.
    InvalidConfig(&'static str),
    /// Every part slot is taken.
    TooManyParts {
        /// The configured maximum.
        max: usize,
    },
    /// There is no part with this number.
    NoSuchPart(u16),
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(field) => write!(f, "the {field} is out of range"),
            Self::TooManyParts { max } => write!(f, "the engine already has {max} parts"),
            Self::NoSuchPart(part) => write!(f, "there is no part {part}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// What became of a pushed record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pushed {
    /// It waits for its frame.
    Queued,
    /// [`EventRecord::decode`] refused it.
    Ignored,
    /// No room was left; it was dropped.
    Full,
}

/// Counts of what has happened to events since the engine was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EngineStats {
    /// Records accepted and queued.
    pub queued: u64,
    /// Records refused by [`EventRecord::decode`].
    pub ignored: u64,
    /// Valid records dropped because the queue of waiting events was full.
    pub dropped: u64,
    /// Events applied at the start of a quantum because their frame had
    /// already passed.
    pub late: u64,
    /// Events applied to the engine.
    pub applied: u64,
    /// Applied events that reached nothing: no such part, or no such
    /// parameter in it.
    pub unrouted: u64,
}

enum Instrument {
    Synth(Box<SynthInstrument>),
    Drums(Box<DrumKit>),
}

struct Part {
    id: u16,
    params: PartParams,
    instrument: Instrument,
    chain: ChainDsp,
}

impl Part {
    fn new(id: u16, kind: InstrumentKind, stages: Chain, spec: &ProcessSpec) -> Self {
        let params = PartParams::new(kind, stages);
        let instrument = match &params.instrument {
            InstrumentParams::Synth(synth) => Instrument::Synth(Box::new(new_synth(
                synth,
                &ProcessSpec {
                    num_channels: 1,
                    ..*spec
                },
            ))),
            InstrumentParams::Drums(kit) => {
                Instrument::Drums(Box::new(DrumKit::new(kit, spec.sample_rate)))
            }
        };
        let chain = ChainDsp::new(&params.chain, spec);
        Self {
            id,
            params,
            instrument,
            chain,
        }
    }

    fn kind(&self) -> InstrumentKind {
        match self.instrument {
            Instrument::Synth(_) => InstrumentKind::Synth,
            Instrument::Drums(_) => InstrumentKind::Drums,
        }
    }

    /// Sets a parameter and passes the value it ends up with to whatever
    /// uses it. `false` if the part has no such parameter.
    fn set_param(&mut self, id: ParamId, value: f64) -> bool {
        let Some(param) = self.params.find_param(id) else {
            return false;
        };
        param.set_plain(value);
        let plain = param.plain();
        if self.chain.set_param(id, plain) {
            return true;
        }
        match &mut self.instrument {
            Instrument::Synth(synth) => {
                for voice in synth.voices_mut() {
                    voice.set_param(id, plain);
                }
                true
            }
            Instrument::Drums(kit) => kit.set_param(id, plain),
        }
    }

    fn apply(&mut self, event: Event) -> bool {
        if let Event::Param { param, value, .. } = event {
            return self.set_param(param, value);
        }
        match (&mut self.instrument, event) {
            (Instrument::Synth(synth), Event::NoteOn { channel, note, velocity, .. }) => {
                synth.note_on(NoteRef::new(channel, note), velocity);
            }
            (Instrument::Synth(synth), Event::NoteOff { channel, note, .. }) => {
                synth.note_off(NoteRef::new(channel, note), 0.0);
            }
            (Instrument::Synth(synth), Event::Choke { .. }) => synth.reset(),
            (Instrument::Synth(synth), Event::AllNotesOff { .. }) => synth.all_notes_off(),
            (Instrument::Drums(kit), Event::NoteOn { note, velocity, .. }) => {
                kit.note_on(note, velocity);
            }
            // Drums are one-shots: letting go of a key changes nothing.
            (Instrument::Drums(_), Event::NoteOff { .. }) => {}
            (Instrument::Drums(kit), Event::Choke { .. }) => kit.choke(),
            (Instrument::Drums(kit), Event::AllNotesOff { .. }) => kit.damp(),
            (_, Event::Param { .. }) => {}
        }
        true
    }

    fn active_voices(&self) -> usize {
        match &self.instrument {
            Instrument::Synth(synth) => synth.active_voices(),
            Instrument::Drums(kit) => kit.active_voices(),
        }
    }

    /// Renders the part's sound into `stereo`, sized to the frames wanted.
    fn render(&mut self, mono: &mut AudioBuffer<f32>, stereo: &mut AudioBuffer<f32>) {
        match &mut self.instrument {
            Instrument::Synth(synth) => synth.process(mono),
            Instrument::Drums(kit) => kit.render(mono.channel_mut(0)),
        }
        for channel in stereo.iter_channels_mut() {
            channel.copy_from_slice(mono.channel(0));
        }
        self.chain.process(self.params.stages, stereo);
    }
}

/// The engine: parts that play sample-exact events into a stereo output.
///
/// Time is a frame counter that starts at 0 and moves on by the number of
/// frames each [`render`](Self::render) produces. An event lands on the
/// frame it names when that frame is rendered; an event whose frame has
/// passed lands on the first frame of the next quantum rendered; an event
/// further ahead waits, in order, with others on the same frame applied in
/// the order they were pushed.
///
/// Control calls (making parts, setting their chains, describing their
/// parameters) allocate and belong outside the audio callback. Pushing
/// events, draining a ring and rendering never allocate, lock or panic.
///
/// ```
/// use manguebyte_web::{Engine, EngineConfig, EventRecord, InstrumentKind};
///
/// let mut engine = Engine::new(EngineConfig::new(48_000.0)).unwrap();
/// engine.create_part(0, InstrumentKind::Synth).unwrap();
/// engine.push(EventRecord::note_on(100, 0, 57, 0.9));
///
/// let rendered = engine.render(128);
/// let (left, _right) = engine.output();
/// assert_eq!(rendered, 128);
/// assert!(left[..100].iter().all(|&sample| sample == 0.0));
/// assert!(left[100..].iter().any(|&sample| sample != 0.0));
/// assert_eq!(engine.frame(), 128);
/// ```
pub struct Engine {
    config: EngineConfig,
    spec: ProcessSpec,
    frame: u64,
    parts: Vec<Part>,
    pending: Pending,
    stats: EngineStats,
    mono: AudioBuffer<f32>,
    stereo: AudioBuffer<f32>,
    left: Vec<f32>,
    right: Vec<f32>,
    rendered: usize,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("config", &self.config)
            .field("frame", &self.frame)
            .field("parts", &self.part_ids().collect::<Vec<_>>())
            .field("pending", &self.pending.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl Engine {
    /// Makes an engine with no parts. Allocates.
    ///
    /// # Errors
    ///
    /// [`EngineError::InvalidConfig`] if a field of `config` is out of range.
    pub fn new(config: EngineConfig) -> Result<Self, EngineError> {
        config.check()?;
        let frames = config.max_quantum;
        Ok(Self {
            config,
            spec: ProcessSpec {
                sample_rate: config.sample_rate,
                max_block_size: frames,
                num_channels: 2,
            },
            frame: 0,
            parts: Vec::with_capacity(config.max_parts),
            pending: Pending::with_capacity(config.pending_capacity),
            stats: EngineStats::default(),
            mono: AudioBuffer::new(1, frames),
            stereo: AudioBuffer::new(2, frames),
            left: vec![0.0; frames],
            right: vec![0.0; frames],
            rendered: 0,
        })
    }

    /// The configuration the engine was made with.
    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    /// The frame the next [`render`](Self::render) starts on.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Counts of what has happened to events.
    pub fn stats(&self) -> EngineStats {
        self.stats
    }

    /// Makes part `part` play `kind` through the full chain, replacing any
    /// part already there. Allocates.
    ///
    /// # Errors
    ///
    /// [`EngineError::TooManyParts`] if `part` is new and every slot is
    /// taken.
    pub fn create_part(&mut self, part: u16, kind: InstrumentKind) -> Result<(), EngineError> {
        self.create_part_with(part, kind, Chain::FULL)
    }

    /// Makes part `part` play `kind` through `chain`, replacing any part
    /// already there. Its parameters start at their defaults. Allocates.
    ///
    /// # Errors
    ///
    /// [`EngineError::TooManyParts`] if `part` is new and every slot is
    /// taken.
    pub fn create_part_with(
        &mut self,
        part: u16,
        kind: InstrumentKind,
        chain: Chain,
    ) -> Result<(), EngineError> {
        let slot = self.parts.binary_search_by_key(&part, |existing| existing.id);
        if slot.is_err() && self.parts.len() >= self.config.max_parts {
            return Err(EngineError::TooManyParts {
                max: self.config.max_parts,
            });
        }
        let new = Part::new(part, kind, chain, &self.spec);
        match slot {
            Ok(index) => self.parts[index] = new,
            Err(index) => self.parts.insert(index, new),
        }
        Ok(())
    }

    /// Removes a part. Its sound stops at once.
    ///
    /// # Errors
    ///
    /// [`EngineError::NoSuchPart`] if there is none.
    pub fn remove_part(&mut self, part: u16) -> Result<(), EngineError> {
        let index = self.index_of(part)?;
        self.parts.remove(index);
        Ok(())
    }

    /// Changes which optional stages a part's chain has. A stage that is
    /// added starts with an empty memory; its parameters keep their values.
    /// The parameters of a stage that is not in the chain are left out of
    /// the part's description, and events that set them are not routed.
    ///
    /// # Errors
    ///
    /// [`EngineError::NoSuchPart`] if there is no such part.
    pub fn set_chain(&mut self, part: u16, chain: Chain) -> Result<(), EngineError> {
        let index = self.index_of(part)?;
        let part = &mut self.parts[index];
        let old = part.params.stages;
        if chain.lowpass && !old.lowpass {
            part.chain.clear_lowpass();
        }
        if chain.reverb && !old.reverb {
            part.chain.clear_reverb();
        }
        part.params.stages = chain;
        Ok(())
    }

    fn index_of(&self, part: u16) -> Result<usize, EngineError> {
        self.parts
            .binary_search_by_key(&part, |existing| existing.id)
            .map_err(|_| EngineError::NoSuchPart(part))
    }

    fn part(&self, part: u16) -> Option<&Part> {
        self.index_of(part).ok().map(|index| &self.parts[index])
    }

    /// The parts, in increasing order.
    pub fn part_ids(&self) -> impl Iterator<Item = u16> + '_ {
        self.parts.iter().map(|part| part.id)
    }

    /// What a part plays.
    pub fn part_kind(&self, part: u16) -> Option<InstrumentKind> {
        self.part(part).map(Part::kind)
    }

    /// Which stages a part's chain has.
    pub fn part_chain(&self, part: u16) -> Option<Chain> {
        self.part(part).map(|part| part.params.stages)
    }

    /// How many voices of a part are sounding.
    pub fn active_voices(&self, part: u16) -> Option<usize> {
        self.part(part).map(Part::active_voices)
    }

    /// Whether a part's reverb is asleep: cleared and skipped because
    /// nothing has reached it for a while. A reverb asleep and one that is
    /// running on silence sound the same; this is for tests and meters.
    pub fn reverb_asleep(&self, part: u16) -> Option<bool> {
        self.part(part).map(|part| part.chain.reverb_asleep())
    }

    /// The value a parameter of a part holds, in its own units.
    pub fn param_value(&self, part: u16, id: &str) -> Option<f64> {
        self.part(part)?.params.find(id).map(|param| param.plain())
    }

    /// The value the parameter with this [`ParamId`] holds.
    pub fn param_value_by_id(&self, part: u16, id: ParamId) -> Option<f64> {
        self.part(part)?
            .params
            .find_param(id)
            .map(|param| param.plain())
    }

    /// The parameters of a part as a JSON array, in the format of
    /// [`ParamSet::descriptions_json`]: the instrument's, then those of the
    /// chain stages it has, then `gain` and `pan`. Allocates.
    pub fn params_json(&self, part: u16) -> Option<String> {
        self.part(part).map(|part| part.params.descriptions_json())
    }

    /// The text of the value `plain` for a parameter of a part, with its
    /// unit, as [`ParamSet::value_to_text`] gives it. Allocates.
    pub fn value_to_text(&self, part: u16, id: &str, plain: f64) -> Option<String> {
        self.part(part)?.params.value_to_text(id, plain)
    }

    /// Accepts a record. Real-time safe.
    pub fn push(&mut self, record: EventRecord) -> Pushed {
        let Some(event) = record.decode() else {
            self.stats.ignored += 1;
            return Pushed::Ignored;
        };
        match self.pending.push(record.frame, event) {
            Ok(()) => {
                self.stats.queued += 1;
                Pushed::Queued
            }
            Err(_) => {
                self.stats.dropped += 1;
                Pushed::Full
            }
        }
    }

    /// Accepts records laid end to end, 32 bytes each, as a message from
    /// another thread carries them. Bytes after the last whole record are
    /// ignored. Returns how many were queued. Real-time safe.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> usize {
        let mut queued = 0;
        for chunk in bytes.chunks_exact(RECORD_SIZE) {
            let mut record = [0u8; RECORD_SIZE];
            record.copy_from_slice(chunk);
            if self.push(EventRecord::from_bytes(&record)) == Pushed::Queued {
                queued += 1;
            }
        }
        queued
    }

    /// Moves records from a ring into the engine, as many as there is room
    /// for, so that a full engine leaves the rest in the ring instead of
    /// dropping them. Call it before each [`render`](Self::render). Returns
    /// how many records were taken. Real-time safe.
    pub fn drain(&mut self, ring: &mut RingConsumer) -> usize {
        let mut taken = 0;
        while self.pending.room() > 0 {
            let Some(record) = ring.pop() else {
                break;
            };
            self.push(record);
            taken += 1;
        }
        taken
    }

    /// How many more events can wait.
    pub fn pending_room(&self) -> usize {
        self.pending.room()
    }

    /// How many events are waiting.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Forgets every waiting event. Real-time safe.
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Renders the next `frames` frames, at most
    /// [`max_quantum`](EngineConfig::max_quantum), applying every event on
    /// its frame. Returns how many were rendered; [`output`](Self::output)
    /// holds them. Real-time safe.
    ///
    /// The output is the sum of the parts, in increasing part order, each
    /// sample limited to -1..=1, with anything that is not a number
    /// written as 0. The result does not depend on how the frames are cut
    /// into quanta: rendering 512 frames at once or one at a time gives the
    /// same samples, bit for bit, as long as every event is pushed before
    /// its frame is rendered.
    pub fn render(&mut self, frames: usize) -> usize {
        let frames = frames.min(self.config.max_quantum);
        let _denormals = NoDenormals::new();
        let start = self.frame;
        let end = start.saturating_add(frames as u64);
        self.left[..frames].fill(0.0);
        self.right[..frames].fill(0.0);

        let mut position = 0;
        while let Some(waiting) = self.pending.pop_before(end) {
            let at = waiting.frame.saturating_sub(start) as usize;
            if waiting.frame < start {
                self.stats.late += 1;
            }
            if at > position {
                self.render_span(position, at);
                position = at;
            }
            self.stats.applied += 1;
            let routed = self
                .parts
                .binary_search_by_key(&waiting.event.part(), |part| part.id)
                .is_ok_and(|index| self.parts[index].apply(waiting.event));
            if !routed {
                self.stats.unrouted += 1;
            }
        }
        self.render_span(position, frames);

        for sample in self.left[..frames]
            .iter_mut()
            .chain(&mut self.right[..frames])
        {
            *sample = if sample.is_finite() {
                sample.clamp(-1.0, 1.0)
            } else {
                0.0
            };
        }
        self.frame = end;
        self.rendered = frames;
        frames
    }

    /// Renders every part over frames `from..to` of the quantum and adds
    /// them to the output.
    fn render_span(&mut self, from: usize, to: usize) {
        if to <= from {
            return;
        }
        let frames = to - from;
        self.mono.set_num_frames(frames);
        self.stereo.set_num_frames(frames);
        for part in &mut self.parts {
            part.render(&mut self.mono, &mut self.stereo);
            for (output, part) in [&mut self.left, &mut self.right]
                .into_iter()
                .zip(self.stereo.iter_channels())
            {
                for (output, &sample) in output[from..to].iter_mut().zip(part) {
                    *output += sample;
                }
            }
        }
    }

    /// The left and right channels of what the last
    /// [`render`](Self::render) produced.
    pub fn output(&self) -> (&[f32], &[f32]) {
        (&self.left[..self.rendered], &self.right[..self.rendered])
    }

    /// Where channel `channel` (0 left, 1 right) of the output starts in
    /// memory. The buffers never move: they hold
    /// [`max_quantum`](EngineConfig::max_quantum) frames from the start.
    pub(crate) fn output_ptr(&self, channel: usize) -> *const f32 {
        match channel {
            0 => self.left.as_ptr(),
            _ => self.right.as_ptr(),
        }
    }

    /// Renders as many frames as `left` and `right` both hold, quantum by
    /// quantum, into them. Real-time safe.
    pub fn render_into(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        let mut done = 0;
        while done < frames {
            let count = self.render(frames - done);
            left[done..done + count].copy_from_slice(&self.left[..count]);
            right[done..done + count].copy_from_slice(&self.right[..count]);
            done += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrument_codes_round_trip() {
        for kind in [InstrumentKind::Synth, InstrumentKind::Drums] {
            assert_eq!(InstrumentKind::from_code(kind.code()), Some(kind));
        }
        assert_eq!(InstrumentKind::from_code(2), None);
        for bits in 0..4 {
            assert_eq!(Chain::from_bits(bits).map(Chain::bits), Some(bits));
        }
        assert_eq!(Chain::from_bits(u32::MAX), None);
    }

    #[test]
    fn bad_configurations_are_refused() {
        let good = EngineConfig::new(48_000.0);
        for (config, field) in [
            (
                EngineConfig {
                    sample_rate: f64::NAN,
                    ..good
                },
                "sample rate",
            ),
            (
                EngineConfig {
                    sample_rate: 1_000.0,
                    ..good
                },
                "sample rate",
            ),
            (
                EngineConfig {
                    max_quantum: 0,
                    ..good
                },
                "max quantum",
            ),
            (
                EngineConfig {
                    max_parts: 0,
                    ..good
                },
                "max parts",
            ),
            (
                EngineConfig {
                    pending_capacity: 0,
                    ..good
                },
                "pending capacity",
            ),
        ] {
            assert_eq!(
                Engine::new(config).err(),
                Some(EngineError::InvalidConfig(field))
            );
        }
        assert!(Engine::new(good).is_ok());
    }
}
