//! An SFZ instrument with its samples decoded, ready to play.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use manguebyte_formats::FormatError;

use crate::controllers::{Controls, MAX_CONTROLLERS};
use crate::envelope::Stages;
use crate::interpolate::SincTable;
use crate::packed::{BLOCK, PackedChannel};
use crate::region::{
    EnvelopeSpec, EnvelopeStage, FilterType, KeyCenter, LoopMode, ModulationTarget, OffMode,
    Region, Trigger,
};
use crate::sfz::{Location, Sfz, SfzError, Warning};
use crate::source::{FileSource, SampleSource};
use crate::store::{Planned, fit_preload, make_pool, probe};
use crate::stream::{MARGIN, Pool, StreamFile, StreamOptions};

/// Longest sample, in frames. Positions are kept with 32 bits of fraction
/// in 64 bits, which leaves room for this many frames and a pitch far up.
pub const MAX_SAMPLE_FRAMES: usize = 1 << 30;

/// The seed an instrument's random numbers start from unless
/// [`Instrument::set_seed`] says otherwise.
pub const DEFAULT_SEED: u64 = 0x5EED_5A3B_1E00_0001;

/// What went wrong while loading an instrument's samples.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoadError {
    /// The source could not supply a sample.
    Read {
        /// The sample's path.
        path: String,
        /// The region that asked for it.
        location: Location,
        /// What the source said.
        error: io::Error,
    },
    /// A sample's bytes are not audio this framework can decode.
    Decode {
        /// The sample's path.
        path: String,
        /// The region that asked for it.
        location: Location,
        /// What the decoder said.
        error: FormatError,
    },
    /// A sample name starting with `*` that is not `*sine` or `*silence`.
    UnknownGenerator {
        /// The name.
        path: String,
        /// The region that asked for it.
        location: Location,
    },
    /// A sample longer than [`MAX_SAMPLE_FRAMES`].
    TooLong {
        /// The sample's path.
        path: String,
        /// The region that asked for it.
        location: Location,
    },
    /// A thread to read streamed samples could not be started.
    Thread(io::Error),
    /// The samples would need more memory than
    /// [`StreamOptions::ram_budget`] allows, even with the shortest head.
    Budget {
        /// Bytes they would need.
        needed: usize,
        /// The budget.
        budget: usize,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read {
                path,
                location,
                error,
            } => write!(f, "{location}: could not read sample \"{path}\": {error}"),
            Self::Decode {
                path,
                location,
                error,
            } => write!(f, "{location}: could not decode sample \"{path}\": {error}"),
            Self::UnknownGenerator { path, location } => {
                write!(f, "{location}: unknown generator \"{path}\"")
            }
            Self::TooLong { path, location } => write!(
                f,
                "{location}: sample \"{path}\" is longer than {MAX_SAMPLE_FRAMES} frames"
            ),
            Self::Thread(error) => write!(f, "could not start a reader thread: {error}"),
            Self::Budget { needed, budget } => write!(
                f,
                "the samples need {needed} bytes in memory, more than the budget of {budget}"
            ),
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { error, .. } => Some(error),
            Self::Decode { error, .. } => Some(error),
            Self::Thread(error) => Some(error),
            _ => None,
        }
    }
}

/// Anything that can stop SFZ text from becoming an [`Instrument`].
#[derive(Debug)]
pub enum InstrumentError {
    /// The text is not valid SFZ.
    Sfz(SfzError),
    /// A sample could not be loaded.
    Load(LoadError),
}

impl fmt::Display for InstrumentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sfz(error) => error.fmt(f),
            Self::Load(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for InstrumentError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sfz(error) => Some(error),
            Self::Load(error) => Some(error),
        }
    }
}

impl From<SfzError> for InstrumentError {
    fn from(error: SfzError) -> Self {
        Self::Sfz(error)
    }
}

impl From<LoadError> for InstrumentError {
    fn from(error: LoadError) -> Self {
        Self::Load(error)
    }
}

/// A decoded sample: one or two channels, whole in memory or, when it is
/// streamed, the stretches of it kept there.
pub(crate) struct SampleData {
    /// The stretches in memory, in order, apart from one another. One that
    /// starts at 0 and holds every frame when the sample is not streamed.
    pub(crate) parts: Vec<Part>,
    /// Frames in the whole sample.
    pub(crate) frames: usize,
    pub(crate) stereo: bool,
    pub(crate) rate: f64,
    /// The first loop a WAV file's `smpl` chunk gives, both ends included.
    pub(crate) file_loop: Option<(u64, u64)>,
    /// The key a WAV file's `smpl` chunk says the sample sounds at.
    pub(crate) root_key: Option<u8>,
}

/// A stretch of a sample held in memory: its frames as they are, or
/// packed (see [`PackedChannel`]), when `packed` is set and `left` and
/// `right` are empty.
pub(crate) struct Part {
    pub(crate) start: u64,
    pub(crate) frames: usize,
    pub(crate) left: Vec<f32>,
    pub(crate) right: Option<Vec<f32>>,
    pub(crate) packed: Option<Packed>,
}

/// A part's channels, packed.
pub(crate) struct Packed {
    pub(crate) left: PackedChannel,
    pub(crate) right: Option<PackedChannel>,
}

impl Part {
    /// A part of the frames as they are.
    pub(crate) fn plain(start: u64, left: Vec<f32>, right: Option<Vec<f32>>) -> Self {
        Self {
            start,
            frames: left.len(),
            left,
            right,
            packed: None,
        }
    }

    /// The frame after its last.
    pub(crate) fn end(&self) -> u64 {
        self.start + self.frames as u64
    }

    /// Packs its frames, if they are not packed yet.
    pub(crate) fn pack(&mut self) {
        if self.packed.is_some() {
            return;
        }
        let left = PackedChannel::pack(&self.left);
        let right = self.right.as_deref().map(PackedChannel::pack);
        self.left = Vec::new();
        self.right = None;
        self.packed = Some(Packed { left, right });
    }

    /// Frame `index` of `channel` (0 or 1; the left for the right of a mono
    /// part). Unpacks a whole block of a packed part, so it is for loading,
    /// not for playing.
    fn frame(&self, channel: usize, index: usize) -> Option<f32> {
        match &self.packed {
            None => match (channel, &self.right) {
                (1, Some(right)) => right.get(index).copied(),
                _ => self.left.get(index).copied(),
            },
            Some(packed) => {
                let source = match (channel, &packed.right) {
                    (1, Some(right)) => right,
                    _ => &packed.left,
                };
                let mut block = [0.0; BLOCK];
                let frames = source.unpack(index / BLOCK, &mut block);
                block[..frames].get(index % BLOCK).copied()
            }
        }
    }

    /// The sum of the squares of frames `low..high` of `channel`, counted
    /// from the part's start.
    fn squares(&self, channel: usize, low: usize, high: usize) -> f64 {
        let square = |&sample: &f32| f64::from(sample) * f64::from(sample);
        match &self.packed {
            None => {
                let data = match (channel, &self.right) {
                    (1, Some(right)) => right,
                    _ => &self.left,
                };
                data[low..high].iter().map(square).sum::<f64>()
            }
            Some(packed) => {
                let source = match (channel, &packed.right) {
                    (1, Some(right)) => right,
                    _ => &packed.left,
                };
                let mut block = [0.0; BLOCK];
                let mut total = 0.0;
                for index in low / BLOCK..high.div_ceil(BLOCK) {
                    let frames = source.unpack(index, &mut block);
                    let from = low.saturating_sub(index * BLOCK).min(frames);
                    let to = (high - index * BLOCK).min(frames);
                    total += block[from..to].iter().map(square).sum::<f64>();
                }
                total
            }
        }
    }

    /// Bytes it takes.
    fn bytes(&self) -> usize {
        match &self.packed {
            None => (self.left.len() + self.right.as_ref().map_or(0, Vec::len)) * 4,
            Some(packed) => {
                packed.left.bytes() + packed.right.as_ref().map_or(0, PackedChannel::bytes)
            }
        }
    }
}

impl SampleData {
    /// A sample held whole.
    pub(crate) fn whole(
        left: Vec<f32>,
        right: Option<Vec<f32>>,
        rate: f64,
        file_loop: Option<(u64, u64)>,
        root_key: Option<u8>,
    ) -> Self {
        Self {
            frames: left.len(),
            stereo: right.is_some(),
            parts: vec![Part::plain(0, left, right)],
            rate,
            file_loop,
            root_key,
        }
    }

    pub(crate) fn frames(&self) -> usize {
        self.frames
    }

    /// Whether every frame is in memory.
    pub(crate) fn is_resident(&self) -> bool {
        matches!(&self.parts[..], [part] if part.start == 0 && part.frames == self.frames)
    }

    /// Whether any part is packed.
    pub(crate) fn is_packed(&self) -> bool {
        self.parts.iter().any(|part| part.packed.is_some())
    }

    /// Packs every part.
    pub(crate) fn pack(&mut self) {
        for part in &mut self.parts {
            part.pack();
        }
    }

    /// The stretch in memory that holds frame `at`.
    #[inline]
    pub(crate) fn part_at(&self, at: u64) -> Option<&Part> {
        self.parts
            .iter()
            .find(|part| at >= part.start && at < part.end())
    }

    /// `count` frames from `start`, if one stretch in memory holds them
    /// all, as they are: the left channel and, for a stereo sample, the
    /// right.
    #[inline]
    pub(crate) fn slice(&self, start: u64, count: usize) -> Option<(&[f32], Option<&[f32]>)> {
        let part = self.part_at(start)?;
        let from = (start - part.start) as usize;
        let left = part.left.get(from..from + count)?;
        let right = match &part.right {
            Some(right) => Some(right.get(from..from + count)?),
            None => None,
        };
        Some((left, right))
    }

    /// Frame `at` of `channel` (0 or 1), if it is in memory; the left for
    /// the right of a mono sample. For loading, not for playing.
    pub(crate) fn ram(&self, channel: usize, at: u64) -> Option<f32> {
        let part = self.part_at(at)?;
        part.frame(channel, (at - part.start) as usize)
    }

    /// Bytes of audio in memory.
    pub(crate) fn bytes(&self) -> usize {
        self.parts.iter().map(Part::bytes).sum()
    }
}

/// A loop of a zone, with the frames its crossfade replaces.
pub(crate) struct Loop {
    pub(crate) start: u64,
    /// Last frame of the loop, included.
    pub(crate) end: u64,
    pub(crate) sustain_only: bool,
    /// The last `tail_left.len()` frames of the loop, crossfaded into the
    /// frames before its start.
    pub(crate) tail_left: Vec<f32>,
    pub(crate) tail_right: Vec<f32>,
}

impl Loop {
    pub(crate) fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    pub(crate) fn crossfade(&self) -> u64 {
        self.tail_left.len() as u64
    }
}

/// A region turned into numbers, with its sample found.
pub(crate) struct Zone {
    /// `None` when the region never sounds: `end=-1`, an empty sample, or
    /// an offset past its end.
    pub(crate) sample: Option<usize>,
    pub(crate) lokey: u8,
    pub(crate) hikey: u8,
    pub(crate) lovel: u8,
    pub(crate) hivel: u8,
    /// Channels counted from 0.
    pub(crate) lochan: u8,
    pub(crate) hichan: u8,
    pub(crate) lorand: f64,
    pub(crate) hirand: f64,
    pub(crate) seq_length: u32,
    pub(crate) seq_position: u32,
    pub(crate) trigger: Trigger,
    /// Linear gain from `volume` and `amplitude`.
    pub(crate) gain: f64,
    /// -1 to 1.
    pub(crate) pan: f64,
    /// -1 to 1.
    pub(crate) veltrack: f64,
    pub(crate) rt_decay: f64,
    pub(crate) keycenter: f64,
    pub(crate) keytrack: f64,
    /// `transpose` and `tune`, in cents.
    pub(crate) detune: f64,
    pub(crate) offset: u64,
    /// Last frame played, included.
    pub(crate) end: u64,
    pub(crate) one_shot: bool,
    pub(crate) lp: Option<Loop>,
    pub(crate) envelope: crate::region::EnvelopeSpec,
    pub(crate) group: i64,
    pub(crate) off_by: Option<i64>,
    pub(crate) off_mode: OffMode,
    pub(crate) off_time: f64,
    pub(crate) polyphony: Option<u32>,
    /// The voices the polyphony limit counts.
    pub(crate) polyphony_scope: PolyphonyScope,
    pub(crate) bend_up: f64,
    pub(crate) bend_down: f64,
    /// `volume`, in decibels, and `amplitude`, as a fraction: `gain` is
    /// made of them, and modulations change them.
    pub(crate) volume_db: f64,
    pub(crate) amplitude: f64,
    /// The filter, when the region has a `cutoff` of a type played here.
    pub(crate) filter: Option<ZoneFilter>,
    /// The modulations by controllers.
    pub(crate) modulations: Box<[ZoneModulation]>,
    /// Which kinds of thing the modulations move ([`MODULATES_GAIN`] and
    /// the others).
    pub(crate) modulates: u8,
}

/// A region's filter, its numbers resolved.
pub(crate) struct ZoneFilter {
    pub(crate) kind: FilterType,
    /// Hertz.
    pub(crate) cutoff: f64,
    /// Decibels.
    pub(crate) resonance: f64,
    /// Cents per key from `keycenter`.
    pub(crate) keytrack: f64,
    pub(crate) keycenter: f64,
    /// Cents at full velocity.
    pub(crate) veltrack: f64,
    /// Cents at the envelope's full level, and added at full velocity.
    pub(crate) depth: f64,
    pub(crate) vel2depth: f64,
    pub(crate) envelope: EnvelopeSpec,
}

/// One modulation by a controller: what it adds, per unit of the
/// controller's value, to its slot of a [`Sums`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct ZoneModulation {
    pub(crate) slot: usize,
    pub(crate) controller: u16,
    pub(crate) depth: f64,
}

/// What every modulation of a region adds up to, slot by slot.
pub(crate) type Sums = [f64; SLOTS];

/// The slots of [`Sums`].
pub(crate) const AMPLITUDE: usize = 0;
pub(crate) const VOLUME: usize = 1;
pub(crate) const PAN: usize = 2;
pub(crate) const PITCH: usize = 3;
pub(crate) const CUTOFF: usize = 4;
pub(crate) const RESONANCE: usize = 5;
pub(crate) const FILEG_DEPTH: usize = 6;
/// The first of the amplitude envelope's stages, in [`EnvelopeStage`]
/// order.
pub(crate) const AMPEG: usize = 7;
/// The first of the filter envelope's stages.
pub(crate) const FILEG: usize = AMPEG + EnvelopeStage::COUNT;
pub(crate) const SLOTS: usize = FILEG + EnvelopeStage::COUNT;

/// Bits of [`Zone::modulates`].
pub(crate) const MODULATES_GAIN: u8 = 1;
pub(crate) const MODULATES_PITCH: u8 = 2;
pub(crate) const MODULATES_AMPEG: u8 = 4;
pub(crate) const MODULATES_FILEG: u8 = 8;

/// The slot a modulation target adds to, and the bit it sets.
fn slot(target: ModulationTarget) -> (usize, u8) {
    match target {
        ModulationTarget::Amplitude => (AMPLITUDE, MODULATES_GAIN),
        ModulationTarget::Volume => (VOLUME, MODULATES_GAIN),
        ModulationTarget::Pan => (PAN, MODULATES_GAIN),
        ModulationTarget::Pitch => (PITCH, MODULATES_PITCH),
        ModulationTarget::Cutoff => (CUTOFF, 0),
        ModulationTarget::Resonance => (RESONANCE, 0),
        ModulationTarget::FilegDepth => (FILEG_DEPTH, 0),
        ModulationTarget::Ampeg(stage) => (AMPEG + stage as usize, MODULATES_AMPEG),
        ModulationTarget::Fileg(stage) => (FILEG + stage as usize, MODULATES_FILEG),
    }
}

/// The stages of an envelope a [`Sums`] holds from `first`.
pub(crate) fn stages(sums: &Sums, first: usize) -> Stages {
    let mut stages = [0.0; EnvelopeStage::COUNT];
    stages.copy_from_slice(&sums[first..first + EnvelopeStage::COUNT]);
    stages
}

/// The voices a region's `polyphony` limit counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PolyphonyScope {
    /// Those of the regions with this `group` value.
    Group(i64),
    /// Those of the regions under the header with this number.
    Header(u32),
}

impl Zone {
    pub(crate) fn accepts(&self, channel: u8, key: u8, velocity: u8) -> bool {
        (self.lokey..=self.hikey).contains(&key)
            && (self.lovel..=self.hivel).contains(&velocity)
            && (self.lochan..=self.hichan).contains(&channel)
    }

    pub(crate) fn accepts_random(&self, random: f64) -> bool {
        random >= self.lorand && (random < self.hirand || self.hirand >= 1.0)
    }

    /// Whether a voice playing it has anything to update as it plays: a
    /// filter, or a modulation by a controller.
    pub(crate) fn ticks(&self) -> bool {
        self.filter.is_some() || !self.modulations.is_empty()
    }

    /// What its modulations add up to with the controllers as they are at
    /// `frame`.
    pub(crate) fn sums<C: Controls + ?Sized>(&self, controls: &C, frame: u64) -> Sums {
        let mut sums = [0.0; SLOTS];
        for modulation in &self.modulations {
            sums[modulation.slot] +=
                modulation.depth * controls.value(modulation.controller, frame);
        }
        sums
    }

    /// The linear gain of `volume` and `amplitude` with what `sums` adds
    /// to them; the amplitude never goes below 0.
    pub(crate) fn gain_with(&self, sums: &Sums) -> f64 {
        if self.modulates & MODULATES_GAIN == 0 {
            return self.gain;
        }
        let amplitude = (self.amplitude + sums[AMPLITUDE] / 100.0).max(0.0);
        10f64.powf((self.volume_db + sums[VOLUME]) / 20.0) * amplitude
    }

    /// The pan, -1 to 1 before it is held there, with what `sums` adds.
    pub(crate) fn pan_with(&self, sums: &Sums) -> f64 {
        self.pan + sums[PAN] / 100.0
    }

    /// The sums of the squares of the first `frames` frames a note reads
    /// from `sample`, its sample, on the left and on the right (the same
    /// for a mono sample): from the offset, round the loop if it has one,
    /// to the end. Reads each frame twice at most, however often the loop
    /// goes round.
    fn squares(&self, sample: &SampleData, frames: u64) -> (f64, f64) {
        let sums = |from: u64, count: u64| -> (f64, f64) {
            // Only what is in memory counts, for a streamed sample.
            let range = |channel: usize| -> f64 {
                let end = from.saturating_add(count);
                let mut total = 0.0;
                for part in &sample.parts {
                    let (low, high) = (from.max(part.start), end.min(part.end()));
                    let (Ok(low), Ok(high), Ok(base)) = (
                        usize::try_from(low),
                        usize::try_from(high),
                        usize::try_from(part.start),
                    ) else {
                        continue;
                    };
                    if low < high {
                        total += part.squares(channel, low - base, high - base);
                    }
                }
                total
            };
            let left = range(0);
            let right = if sample.stereo { range(1) } else { left };
            (left, right)
        };
        let Some(lp) = &self.lp else {
            let count = (self.end + 1).saturating_sub(self.offset).min(frames);
            return sums(self.offset, count);
        };
        // Where `Player::start` puts the first frame read: an offset past
        // the loop goes round it.
        let first = if self.offset > lp.end {
            lp.start + (self.offset - lp.start) % lp.len()
        } else {
            self.offset
        };
        let before = (lp.end + 1 - first).min(frames);
        let (mut left, mut right) = sums(first, before);
        let rest = frames - before;
        let rounds = rest / lp.len();
        let part = rest % lp.len();
        if rounds > 0 {
            let (whole_left, whole_right) = sums(lp.start, lp.len());
            left += whole_left * rounds as f64;
            right += whole_right * rounds as f64;
        }
        let (part_left, part_right) = sums(lp.start, part);
        (left + part_left, right + part_right)
    }
}

/// An SFZ instrument with every sample decoded and every region resolved,
/// ready to be handed to a [`Sampler`](crate::Sampler) or
/// [`SamplerVoice`](crate::SamplerVoice).
///
/// Building one reads and decodes files and allocates, so it is done off
/// the audio thread. The instrument does not depend on the output sample
/// rate: pitch is worked out per note from each sample's own rate.
///
/// The instrument also holds the state the format makes it keep between
/// notes: the round-robin counter of each region and the random number
/// generator. Both are atomics, so an instrument shared by several voices
/// keeps one sequence; the audio thread only ever loads and stores them,
/// which never waits.
///
/// Cloning is cheap: the clone shares the decoded samples and the resolved
/// regions, which never change once loaded, and copies only the state kept
/// between notes, from which it then goes its own way. So one instrument
/// can play in a [`Sampler`](crate::Sampler) while a clone of it is played
/// elsewhere, measured say, without either one's notes moving the other's
/// round robins; the samples are freed with the last of them. Like every
/// drop of an instrument, dropping a clone may free, so it belongs off the
/// audio thread.
///
/// ```
/// use manguebyte_sampler::{Instrument, MemorySource};
///
/// let instrument = Instrument::from_sfz(
///     "<region> sample=*sine lokey=0 hikey=127",
///     &mut MemorySource::new(),
/// )?;
/// assert_eq!(instrument.num_regions(), 1);
/// assert!(instrument.warnings().is_empty());
/// let clone = instrument.clone();
/// assert_eq!(clone.sample_bytes(), instrument.sample_bytes());
/// # Ok::<(), manguebyte_sampler::InstrumentError>(())
/// ```
pub struct Instrument {
    pub(crate) zones: Arc<[Zone]>,
    pub(crate) samples: Arc<[SampleData]>,
    /// Per key, the zones that start on a key going down.
    pub(crate) attack_by_key: Arc<[Vec<u32>]>,
    /// Per key, the zones that start on a key coming up.
    pub(crate) release_by_key: Arc<[Vec<u32>]>,
    pub(crate) sequence: Vec<AtomicU32>,
    random: AtomicU64,
    seed: u64,
    pub(crate) note_shift: i32,
    /// The value each of the [`MAX_CONTROLLERS`] controllers starts at, 0
    /// to 1 (`set_ccN`, `set_hdccN`).
    pub(crate) controller_defaults: Arc<[f64]>,
    /// Shared by every instrument: see [`prepare_tables`](crate::prepare_tables).
    pub(crate) sinc: Arc<SincTable>,
    warnings: Arc<[Warning]>,
    /// The voice slots and reader threads, when samples are streamed.
    pub(crate) pool: Option<Arc<Pool>>,
}

impl Clone for Instrument {
    /// The same instrument, its samples and regions shared, its round
    /// robins and random numbers where this one's are, and from then on its
    /// own. Allocates (the round-robin counters).
    fn clone(&self) -> Self {
        Self {
            zones: Arc::clone(&self.zones),
            samples: Arc::clone(&self.samples),
            attack_by_key: Arc::clone(&self.attack_by_key),
            release_by_key: Arc::clone(&self.release_by_key),
            sequence: self
                .sequence
                .iter()
                .map(|counter| AtomicU32::new(counter.load(Ordering::Relaxed)))
                .collect(),
            random: AtomicU64::new(self.random.load(Ordering::Relaxed)),
            seed: self.seed,
            note_shift: self.note_shift,
            controller_defaults: Arc::clone(&self.controller_defaults),
            sinc: Arc::clone(&self.sinc),
            warnings: Arc::clone(&self.warnings),
            pool: self.pool.clone(),
        }
    }
}

impl fmt::Debug for Instrument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instrument")
            .field("regions", &self.zones.len())
            .field("samples", &self.samples.len())
            .field("seed", &self.seed)
            .field("warnings", &self.warnings.len())
            .field("streaming", &self.pool.is_some())
            .finish_non_exhaustive()
    }
}

impl Instrument {
    /// Parses `text`, reading `#include`d files and samples from `source`,
    /// and loads the result.
    pub fn from_sfz(text: &str, source: &mut dyn SampleSource) -> Result<Self, InstrumentError> {
        let sfz = Sfz::parse_with(text, source)?;
        Ok(Self::load(&sfz, source)?)
    }

    /// Reads the `.sfz` file at `path` and loads it, with samples and
    /// includes relative to the file's directory.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, InstrumentError> {
        let path = path.as_ref();
        let text = std::fs::read(path).map_err(|error| {
            InstrumentError::Load(LoadError::Read {
                path: path.display().to_string(),
                location: Location {
                    file: None,
                    line: 0,
                },
                error,
            })
        })?;
        let directory = path.parent().unwrap_or(Path::new(""));
        let mut source = FileSource::new(directory);
        Self::from_sfz(&String::from_utf8_lossy(&text), &mut source)
    }

    /// Loads the samples of a parsed instrument from `source`. Each
    /// different path is read and decoded once.
    ///
    /// Samples may be WAV, AIFF or FLAC, of any rate, with one or two
    /// channels (a file with more plays its first two). A WAV file's
    /// `smpl` chunk supplies its first loop and its root key, used when the
    /// SFZ text does not give them. Two generators need no file: `*sine`,
    /// one cycle of a sine that sounds at the pitch of the key played, and
    /// `*silence`.
    pub fn load(sfz: &Sfz, source: &mut dyn SampleSource) -> Result<Self, LoadError> {
        let mut samples: Vec<SampleData> = Vec::new();
        let mut by_path: HashMap<Arc<str>, Option<usize>> = HashMap::new();
        for region in &sfz.regions {
            if !by_path.contains_key(&*region.sample) {
                let loaded = load_sample(&region.sample, &region.location, source)?;
                let index = loaded.map(|data| {
                    samples.push(data);
                    samples.len() - 1
                });
                by_path.insert(Arc::clone(&region.sample), index);
            }
        }
        Ok(Self::assemble(sfz, &by_path, samples))
    }

    /// Reads the `.sfz` file at `path` and loads it as
    /// [`load_streaming`](Self::load_streaming) does, with samples and
    /// includes relative to the file's directory.
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open), and [`LoadError::Thread`] if a reader
    /// thread cannot be started.
    pub fn open_streaming(
        path: impl AsRef<Path>,
        options: &StreamOptions,
    ) -> Result<Self, InstrumentError> {
        let path = path.as_ref();
        let text = std::fs::read(path).map_err(|error| {
            InstrumentError::Load(LoadError::Read {
                path: path.display().to_string(),
                location: Location {
                    file: None,
                    line: 0,
                },
                error,
            })
        })?;
        let directory = path.parent().unwrap_or(Path::new(""));
        let mut source = FileSource::new(directory);
        let sfz = Sfz::parse_with(&String::from_utf8_lossy(&text), &mut source)?;
        Ok(Self::load_streaming(&sfz, directory, options)?)
    }

    /// Loads the samples of a parsed instrument from the files under
    /// `root`, keeping in memory only part of each, and starts the threads
    /// that read the rest while notes play: see [`StreamOptions`].
    ///
    /// Each WAV, AIFF or FLAC file keeps in memory its first
    /// [`preload_frames`](StreamOptions::preload_frames) from the earliest
    /// `offset` of the regions that play it, and every loop they play
    /// (with the frames its crossfade and the interpolator read round it).
    /// A sample whose stretches cover it whole, a sample with a loop longer
    /// than [`loop_cache_frames`](StreamOptions::loop_cache_frames), a file
    /// in another format and a FLAC file that does not state its length are
    /// loaded whole, as [`load`](Self::load) loads them. What plays is the
    /// same, bit for bit, as from [`load`](Self::load), as long as the disk
    /// keeps up.
    ///
    /// A voice that plays a streamed sample asks for the frames after the
    /// stretch it starts in as it starts. If they have not arrived when it
    /// needs them (the disk is too slow, the ring too small, or every one
    /// of the [`voices`](StreamOptions::voices) is streaming already), the
    /// voice holds its last value and fades out over 6 ms, and
    /// [`stream_underruns`](Self::stream_underruns) counts it. A region
    /// whose `offset` lies past the stretches of its sample in memory
    /// starts that way, silent.
    ///
    /// The threads stop, and the files close, when the last clone of the
    /// instrument is dropped.
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load), and [`LoadError::Thread`] if a reader
    /// thread cannot be started.
    pub fn load_streaming(
        sfz: &Sfz,
        root: impl AsRef<Path>,
        options: &StreamOptions,
    ) -> Result<Self, LoadError> {
        enum Entry {
            Planned(usize),
            Whole(Option<SampleData>),
        }
        let root = root.as_ref();
        let mut uses: HashMap<&str, Vec<&Region>> = HashMap::new();
        for region in &sfz.regions {
            uses.entry(&*region.sample).or_default().push(region);
        }
        let mut source = FileSource::new(root);
        let mut entries: Vec<(&Region, Entry)> = Vec::new();
        let mut planned: Vec<Planned> = Vec::new();
        let mut seen: HashMap<&str, ()> = HashMap::new();
        for region in &sfz.regions {
            if seen.insert(&*region.sample, ()).is_some() {
                continue;
            }
            let path = &*region.sample;
            let location = &region.location;
            if path.starts_with('*') {
                let data = load_sample(path, location, &mut source)?;
                entries.push((region, Entry::Whole(data)));
                continue;
            }
            let full = root.join(path);
            let info = probe(&full).map_err(|error| format_error(path, location, error))?;
            let Some(info) = info else {
                let data = load_sample(path, location, &mut source)?;
                entries.push((region, Entry::Whole(data)));
                continue;
            };
            if info.frames == 0 || info.channels == 0 {
                entries.push((region, Entry::Whole(None)));
                continue;
            }
            if info.frames > MAX_SAMPLE_FRAMES as u64 {
                return Err(LoadError::TooLong {
                    path: path.to_owned(),
                    location: location.clone(),
                });
            }
            // The head starts at the earliest offset; each loop is kept
            // with the frames its crossfade and the interpolator read.
            let regions = uses.get(path).map_or(&[][..], Vec::as_slice);
            let head = regions
                .iter()
                .map(|region| region.offset)
                .min()
                .unwrap_or(0);
            let mut keep = Vec::new();
            let mut whole = false;
            for region in regions {
                let span = span(region, info.frames, info.rate, info.file_loop);
                let Some((start, end, crossfade)) = span.and_then(|span| span.lp) else {
                    continue;
                };
                if end - start + 1 > options.loop_cache_frames as u64 {
                    whole = true;
                }
                keep.push((start.saturating_sub(crossfade + MARGIN), end + 1 + MARGIN));
            }
            entries.push((region, Entry::Planned(planned.len())));
            planned.push(Planned {
                path: full,
                info,
                head,
                keep,
                whole,
            });
        }
        let fixed = entries
            .iter()
            .map(|(_, entry)| match entry {
                Entry::Whole(Some(data)) => data.bytes(),
                _ => 0,
            })
            .sum();
        let preload =
            fit_preload(&planned, fixed, options).map_err(|needed| LoadError::Budget {
                needed,
                budget: options.ram_budget.unwrap_or(usize::MAX),
            })?;
        let mut samples: Vec<SampleData> = Vec::new();
        let mut files: Vec<Option<StreamFile>> = Vec::new();
        let mut by_path: HashMap<Arc<str>, Option<usize>> = HashMap::new();
        for (region, entry) in entries {
            let loaded = match entry {
                Entry::Whole(data) => data.map(|mut data| {
                    // Generators stay as they are.
                    if options.compress && !region.sample.starts_with('*') {
                        data.pack();
                    }
                    (data, None)
                }),
                Entry::Planned(at) => {
                    let (data, file) = planned[at]
                        .read(preload, options.compress)
                        .map_err(|error| format_error(&region.sample, &region.location, error))?;
                    Some((data, file))
                }
            };
            let index = loaded.map(|(data, file)| {
                samples.push(data);
                files.push(file);
                samples.len() - 1
            });
            by_path.insert(Arc::clone(&region.sample), index);
        }
        let pool = make_pool(&samples, files, options).map_err(LoadError::Thread)?;
        let mut instrument = Self::assemble(sfz, &by_path, samples);
        instrument.pool = pool;
        Ok(instrument)
    }

    /// Whether any of its samples is streamed from disk.
    pub fn is_streaming(&self) -> bool {
        self.pool.as_ref().is_some_and(|pool| pool.streams())
    }

    /// How many times a voice found that the frames it needed had not
    /// arrived from disk, and faded out, since the instrument was loaded
    /// (shared with its clones). Real-time safe; poll it.
    pub fn stream_underruns(&self) -> u64 {
        self.pool.as_ref().map_or(0, |pool| pool.underruns())
    }

    /// How many reads of a streamed file have failed (shared with its
    /// clones). A voice whose read fails runs out of frames and counts an
    /// underrun.
    pub fn stream_read_errors(&self) -> u64 {
        self.pool.as_ref().map_or(0, |pool| pool.read_errors())
    }

    /// How many voices are streaming now.
    pub fn streaming_voices(&self) -> usize {
        self.pool.as_ref().map_or(0, |pool| pool.streaming())
    }

    /// Bytes of the voices' rings, which a streamed instrument holds in
    /// memory beside [`sample_bytes`](Self::sample_bytes).
    pub fn stream_buffer_bytes(&self) -> usize {
        self.pool.as_ref().map_or(0, |pool| pool.ring_bytes())
    }

    /// Waits until every streaming voice has as much read ahead as its ring
    /// holds (or the rest of its file), or `timeout` has passed, and says
    /// whether it got there. For rendering faster than real time, between
    /// blocks, so that the output is what real time would give with a disk
    /// that keeps up. It sleeps: never call it on the audio thread. True at
    /// once for an instrument that does not stream.
    pub fn wait_for_streams(&self, timeout: std::time::Duration) -> bool {
        self.pool.as_ref().is_none_or(|pool| pool.wait(timeout))
    }

    /// Resolves every region of `sfz` against its sample, whose place in
    /// `samples` `by_path` gives (`None` for one with no frames).
    fn assemble(
        sfz: &Sfz,
        by_path: &HashMap<Arc<str>, Option<usize>>,
        samples: Vec<SampleData>,
    ) -> Self {
        let zones: Vec<Zone> = sfz
            .regions
            .iter()
            .map(|region| {
                let index = by_path.get(&*region.sample).copied().flatten();
                let generated = region.sample.starts_with('*');
                zone(region, index.map(|i| (i, &samples[i])), generated)
            })
            .collect();
        let mut attack_by_key = vec![Vec::new(); 128];
        let mut release_by_key = vec![Vec::new(); 128];
        for (index, zone) in zones.iter().enumerate() {
            let lists = match zone.trigger {
                Trigger::Attack | Trigger::First | Trigger::Legato => &mut attack_by_key,
                Trigger::Release | Trigger::ReleaseKey => &mut release_by_key,
            };
            for key in zone.lokey..=zone.hikey.min(127) {
                lists[usize::from(key)].push(index as u32);
            }
        }
        let sequence = zones.iter().map(|_| AtomicU32::new(0)).collect();
        let controller_defaults = (0..MAX_CONTROLLERS)
            .map(|number| sfz.control.initial_value(number as u16))
            .collect();
        Self {
            zones: zones.into(),
            samples: samples.into(),
            attack_by_key: attack_by_key.into(),
            release_by_key: release_by_key.into(),
            sequence,
            random: AtomicU64::new(DEFAULT_SEED),
            seed: DEFAULT_SEED,
            note_shift: sfz.control.note_shift(),
            controller_defaults,
            sinc: SincTable::shared(),
            warnings: sfz.warnings.as_slice().into(),
            pool: None,
        }
    }

    /// Whether a note on `key` starts a region when its key goes down: a
    /// region with an `attack`, `first` or `legato` trigger whose keys,
    /// after `note_offset` and `octave_offset`, take it in, whatever its
    /// velocities, channels, random range and round robin. A region that
    /// never sounds (`*silence`, `end=-1`) counts, since it is there on
    /// purpose. Real-time safe.
    ///
    /// ```
    /// use manguebyte_sampler::{Instrument, MemorySource};
    ///
    /// let instrument = Instrument::from_sfz(
    ///     "<region> sample=*sine key=36
    ///      <region> sample=*silence key=38 trigger=release",
    ///     &mut MemorySource::new(),
    /// )?;
    /// assert!(instrument.covers_key(36));
    /// assert!(!instrument.covers_key(38), "only when it comes up");
    /// assert!(!instrument.covers_key(88));
    /// # Ok::<(), manguebyte_sampler::InstrumentError>(())
    /// ```
    pub fn covers_key(&self, key: u8) -> bool {
        let key = i32::from(key.min(127)) + self.note_shift;
        usize::try_from(key)
            .ok()
            .and_then(|key| self.attack_by_key.get(key))
            .is_some_and(|zones| !zones.is_empty())
    }

    /// About how much sound a note plays: the energy of what a note on
    /// `key` at `velocity` (0 to 1, as [`Sampler::note_on`](crate::Sampler::note_on)
    /// takes it), its key held, reads from its samples in its first
    /// `seconds`, at the gains it plays them with, both channels added, in
    /// full-scale seconds: a sine at full scale in both channels for a second
    /// is 1, so is 0 dBFS of it on one of them for two.
    ///
    /// Every region the note starts on channel 0 counts: layers add up, and
    /// regions that take turns (`seq_length`) or share the random range
    /// (`lorand`, `hirand`) count for the share of the notes they play.
    /// Each is read as the note reads it: from its `offset`, at its pitch
    /// for the key, round its loop, to its `end`, at its `volume`,
    /// `amplitude`, velocity tracking and pan. Its envelope and a loop's
    /// crossfade are left out, as is everything after the first `seconds`,
    /// so this is an estimate, for a level to start from before the
    /// instrument is played and measured. It reads each frame of a region
    /// twice at most, however often a loop goes round, so it costs about a
    /// pass over what the note plays in `seconds`, and does not allocate.
    /// A velocity or a length of time that is not a positive number, or an
    /// infinite length, gives 0.
    ///
    /// ```
    /// use manguebyte_sampler::{Instrument, MemorySource};
    ///
    /// let instrument = Instrument::from_sfz("<region> sample=*sine", &mut MemorySource::new())?;
    /// // A full-scale sine in both channels, middle C, for a second.
    /// assert!((instrument.note_energy(60, 1.0, 1.0) - 1.0).abs() < 1e-3);
    /// // At half the velocity, the square law: a quarter of the gain.
    /// let half = instrument.note_energy(60, 0.5, 1.0);
    /// assert!((half - (64.0f64 / 127.0).powi(4)).abs() < 1e-3);
    /// # Ok::<(), manguebyte_sampler::InstrumentError>(())
    /// ```
    pub fn note_energy(&self, key: u8, velocity: f32, seconds: f64) -> f64 {
        if velocity.is_nan() || velocity <= 0.0 || !seconds.is_finite() || seconds <= 0.0 {
            return 0.0;
        }
        let velocity = (f64::from(velocity) * 127.0).round().clamp(1.0, 127.0) as u8;
        let curve = (f64::from(velocity) / 127.0).powi(2);
        let Some(key) = u8::try_from(i32::from(key.min(127)) + self.note_shift)
            .ok()
            .filter(|&key| key < 128)
        else {
            return 0.0;
        };
        let Some(zones) = self.attack_by_key.get(usize::from(key)) else {
            return 0.0;
        };
        let mut energy = 0.0;
        for &index in zones {
            let Some(zone) = self.zones.get(index as usize) else {
                continue;
            };
            // A legato region waits for another key to be down.
            if zone.trigger == Trigger::Legato || !zone.accepts(0, key, velocity) {
                continue;
            }
            let Some(sample) = zone.sample.and_then(|index| self.samples.get(index)) else {
                continue;
            };
            let low = zone.lorand.clamp(0.0, 1.0);
            let high = if zone.hirand >= 1.0 {
                1.0
            } else {
                zone.hirand.clamp(0.0, 1.0)
            };
            let share = (high - low).max(0.0) / f64::from(zone.seq_length.max(1));
            // The gain, velocity tracking and pan law of `Player`.
            let velocity_gain = if zone.veltrack >= 0.0 {
                1.0 - zone.veltrack + zone.veltrack * curve
            } else {
                1.0 + zone.veltrack * curve
            };
            // Controllers at their initial values.
            let sums = zone.sums(&*self.controller_defaults, 0);
            let gain = zone.gain_with(&sums) * velocity_gain;
            let pan = zone.pan_with(&sums).clamp(-1.0, 1.0);
            let (left_gain, right_gain) = if sample.stereo {
                ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
            } else {
                let angle = (pan + 1.0) * core::f64::consts::FRAC_PI_4;
                (
                    core::f64::consts::SQRT_2 * angle.cos(),
                    core::f64::consts::SQRT_2 * angle.sin(),
                )
            };
            let cents = (f64::from(key) - zone.keycenter) * zone.keytrack + zone.detune;
            let speed = 2f64.powf(cents / 1_200.0).clamp(1.0 / 65_536.0, 256.0);
            // Frames of the sample a second of the note reads.
            let rate = sample.rate * speed;
            let frames = (seconds * rate).round();
            if !rate.is_finite() || rate <= 0.0 || frames.is_nan() || frames < 1.0 {
                continue;
            }
            let (left, right) = zone.squares(sample, frames as u64);
            energy += share
                * gain
                * gain
                * (left_gain * left_gain * left + right_gain * right_gain * right)
                / rate;
        }
        energy
    }

    /// The value controller `controller` starts at, 0 to 1: its `set_ccN`
    /// (divided by 127) or `set_hdccN`, or 0. Also 0 for a number of
    /// [`MAX_CONTROLLERS`] or more.
    pub fn controller_default(&self, controller: u16) -> f64 {
        self.controller_defaults
            .get(usize::from(controller))
            .copied()
            .unwrap_or(0.0)
    }

    /// How many regions it has.
    pub fn num_regions(&self) -> usize {
        self.zones.len()
    }

    /// How many different samples it loaded.
    pub fn num_samples(&self) -> usize {
        self.samples.len()
    }

    /// Bytes of decoded audio it holds.
    pub fn sample_bytes(&self) -> usize {
        self.samples.iter().map(SampleData::bytes).sum()
    }

    /// What the parser had to say about the text.
    pub fn warnings(&self) -> &[Warning] {
        &self.warnings
    }

    /// Sets the seed of the random numbers that choose between regions with
    /// `lorand` and `hirand`, and starts the sequence again from it. The
    /// same seed and the same notes give the same choices.
    pub fn set_seed(&mut self, seed: u64) {
        self.seed = seed;
        self.random.store(seed, Ordering::Relaxed);
    }

    /// The seed set last.
    pub fn seed(&self) -> u64 {
        self.seed
    }

    /// Starts every round robin and the random sequence again, as when the
    /// instrument was loaded. Real-time safe.
    pub fn restart(&self) {
        for counter in &self.sequence {
            counter.store(0, Ordering::Relaxed);
        }
        self.random.store(self.seed, Ordering::Relaxed);
    }

    /// The next random number, in `0..1`. Real-time safe.
    pub(crate) fn next_random(&self) -> f64 {
        // SplitMix64 (G. Steele, D. Lea, C. Flood, "Fast splittable
        // pseudorandom number generators", OOPSLA 2014).
        let state = self
            .random
            .load(Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        self.random.store(state, Ordering::Relaxed);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Advances the round robin of zone `index` and says whether this is
    /// its turn. Real-time safe.
    pub(crate) fn take_turn(&self, index: usize) -> bool {
        let (Some(zone), Some(counter)) = (self.zones.get(index), self.sequence.get(index)) else {
            return false;
        };
        let count = counter.load(Ordering::Relaxed);
        counter.store(
            count.wrapping_add(1) % zone.seq_length.max(1),
            Ordering::Relaxed,
        );
        count % zone.seq_length.max(1) == zone.seq_position.saturating_sub(1)
    }
}

/// An [`Instrument`] loaded one sample at a time, for a thread that may
/// only work in short steps, such as a browser's audio thread between two
/// render quanta. [`Instrument::load`] does the same work in one go and
/// gives the same instrument.
///
/// [`paths`](Self::paths) lists the files the instrument needs, each once;
/// [`load`](Self::load) reads and decodes one of them; and
/// [`finish`](Self::finish) loads whatever is still missing and resolves the
/// regions against their samples.
///
/// ```
/// use manguebyte_sampler::{InstrumentBuilder, MemorySource, Sfz};
/// # use manguebyte_core::AudioBuffer;
/// # use manguebyte_formats::wav::{self, WavFormat};
/// # let mut wav_bytes = Vec::new();
/// # wav::write(&mut wav_bytes, &AudioBuffer::<f32>::new(1, 64), 24_000, WavFormat::Int16).unwrap();
///
/// let sfz = Sfz::parse(
///     "<control> default_path=kit/
///      <region> sample=kick.wav key=36
///      <region> sample=kick.wav key=35
///      <region> sample=*sine key=60",
/// )?;
/// let mut builder = InstrumentBuilder::new(sfz);
/// assert_eq!(builder.paths().collect::<Vec<_>>(), ["kit/kick.wav"]);
///
/// // One step: this file, now.
/// let mut source = MemorySource::new().with("kit/kick.wav", wav_bytes);
/// assert!(builder.load("kit/kick.wav", &mut source)?);
/// assert_eq!(builder.missing().count(), 0);
///
/// let instrument = builder.finish(&mut source)?;
/// assert_eq!(instrument.num_samples(), 2, "the file and the sine");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub struct InstrumentBuilder {
    sfz: Sfz,
    /// Every different sample the regions name, in the order they first
    /// name it, with the region that does.
    order: Vec<(Arc<str>, Location)>,
    /// The samples loaded so far, by name: their place in `samples`, or
    /// `None` for one with no frames.
    by_path: HashMap<Arc<str>, Option<usize>>,
    samples: Vec<SampleData>,
}

impl fmt::Debug for InstrumentBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstrumentBuilder")
            .field("regions", &self.sfz.regions.len())
            .field("samples", &self.order.len())
            .field("loaded", &self.by_path.len())
            .finish_non_exhaustive()
    }
}

impl InstrumentBuilder {
    /// A builder for the parsed instrument `sfz`, with no sample loaded.
    pub fn new(sfz: Sfz) -> Self {
        let mut order: Vec<(Arc<str>, Location)> = Vec::new();
        let mut seen: HashMap<Arc<str>, ()> = HashMap::new();
        for region in &sfz.regions {
            if seen.insert(Arc::clone(&region.sample), ()).is_none() {
                order.push((Arc::clone(&region.sample), region.location.clone()));
            }
        }
        Self {
            sfz,
            order,
            by_path: HashMap::new(),
            samples: Vec::new(),
        }
    }

    /// The parsed instrument.
    pub fn sfz(&self) -> &Sfz {
        &self.sfz
    }

    /// The files the instrument reads, each once, in the order its regions
    /// first name them, as they are handed to a [`SampleSource`]: after
    /// `default_path`, normalised by [`normalize_path`](crate::normalize_path).
    /// Generators such as `*sine` are not files and are left out.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.order
            .iter()
            .map(|(path, _)| &**path)
            .filter(|path| !path.starts_with('*'))
    }

    /// The files of [`paths`](Self::paths) not loaded yet.
    pub fn missing(&self) -> impl Iterator<Item = &str> {
        self.paths()
            .filter(|path| !self.by_path.contains_key(*path))
    }

    /// Reads the file at `path` from `source` and decodes it, if the
    /// instrument needs it and it is not loaded yet. Returns whether it
    /// did.
    ///
    /// # Errors
    ///
    /// [`LoadError`] if the source cannot supply the file or it is not
    /// audio this framework decodes.
    pub fn load(&mut self, path: &str, source: &mut dyn SampleSource) -> Result<bool, LoadError> {
        if path.starts_with('*') || self.by_path.contains_key(path) {
            return Ok(false);
        }
        let Some(index) = self.order.iter().position(|(name, _)| &**name == path) else {
            return Ok(false);
        };
        self.load_index(index, source)?;
        Ok(true)
    }

    fn load_index(&mut self, index: usize, source: &mut dyn SampleSource) -> Result<(), LoadError> {
        let (path, location) = &self.order[index];
        let loaded = load_sample(path, location, source)?;
        let place = loaded.map(|data| {
            self.samples.push(data);
            self.samples.len() - 1
        });
        self.by_path.insert(Arc::clone(path), place);
        Ok(())
    }

    /// Loads every sample still missing (generators included) from
    /// `source`, and makes the instrument.
    ///
    /// # Errors
    ///
    /// [`LoadError`] for the first sample that cannot be loaded.
    pub fn finish(mut self, source: &mut dyn SampleSource) -> Result<Instrument, LoadError> {
        for index in 0..self.order.len() {
            if !self.by_path.contains_key(&self.order[index].0) {
                self.load_index(index, source)?;
            }
        }
        Ok(Instrument::assemble(&self.sfz, &self.by_path, self.samples))
    }
}

/// Reads and decodes one sample. `None` when it has no frames.
fn load_sample(
    path: &str,
    location: &Location,
    source: &mut dyn SampleSource,
) -> Result<Option<SampleData>, LoadError> {
    if let Some(name) = path.strip_prefix('*') {
        return match name {
            "sine" => Ok(Some(sine_cycle())),
            "silence" => Ok(None),
            _ => Err(LoadError::UnknownGenerator {
                path: path.to_owned(),
                location: location.clone(),
            }),
        };
    }
    let bytes = source.read(path).map_err(|error| LoadError::Read {
        path: path.to_owned(),
        location: location.clone(),
        error,
    })?;
    let file = manguebyte_formats::decode(&bytes).map_err(|error| LoadError::Decode {
        path: path.to_owned(),
        location: location.clone(),
        error,
    })?;
    let frames = file.buffer.num_frames();
    if frames == 0 || file.buffer.num_channels() == 0 {
        return Ok(None);
    }
    if frames > MAX_SAMPLE_FRAMES {
        return Err(LoadError::TooLong {
            path: path.to_owned(),
            location: location.clone(),
        });
    }
    let (file_loop, root_key) = wav_sample_info(&bytes);
    let left = file.buffer.channel(0).to_vec();
    let right = (file.buffer.num_channels() > 1).then(|| file.buffer.channel(1).to_vec());
    Ok(Some(SampleData::whole(
        left,
        right,
        f64::from(file.sample_rate.max(1)),
        file_loop.filter(|&(start, end)| start <= end && end < frames as u64),
        root_key,
    )))
}

/// A decoder's error about the sample at `path`, as a [`LoadError`].
fn format_error(path: &str, location: &Location, error: FormatError) -> LoadError {
    match error {
        FormatError::Io(error) => LoadError::Read {
            path: path.to_owned(),
            location: location.clone(),
            error,
        },
        error => LoadError::Decode {
            path: path.to_owned(),
            location: location.clone(),
            error,
        },
    }
}

/// Frames in the cycle `*sine` plays.
const SINE_FRAMES: usize = 2_048;

/// One cycle of a sine, looped, at a rate that makes it sound middle C
/// (261.63 Hz) at its key center, which is fixed at 60.
fn sine_cycle() -> SampleData {
    let left = (0..SINE_FRAMES)
        .map(|n| (core::f64::consts::TAU * n as f64 / SINE_FRAMES as f64).sin() as f32)
        .collect();
    let middle_c = 440.0 * 2f64.powf(-9.0 / 12.0);
    SampleData::whole(
        left,
        None,
        middle_c * SINE_FRAMES as f64,
        Some((0, SINE_FRAMES as u64 - 1)),
        Some(60),
    )
}

/// The first loop and the root key in a WAV file's `smpl` chunk, if it has
/// one. Anything malformed is ignored.
fn wav_sample_info(bytes: &[u8]) -> (Option<(u64, u64)>, Option<u8>) {
    // Offsets are worked out in 64 bits, where a chunk walk of a file that
    // fits in memory cannot overflow, and only then turned into indices, so
    // that a hostile chunk size cannot overflow a 32-bit `usize`.
    wav_sample_info_with(|at, out| {
        let slice = usize::try_from(at)
            .ok()
            .and_then(|start| bytes.get(start..start.checked_add(4)?));
        match slice {
            Some(slice) => {
                out.copy_from_slice(slice);
                true
            }
            None => false,
        }
    })
}

/// [`wav_sample_info`] of a file, read four bytes at a time where the walk
/// over its chunks leads.
pub(crate) fn wav_file_sample_info(file: &mut std::fs::File) -> (Option<(u64, u64)>, Option<u8>) {
    use std::io::{Read, Seek, SeekFrom};
    wav_sample_info_with(|at, out| {
        file.seek(SeekFrom::Start(at)).is_ok() && file.read_exact(out).is_ok()
    })
}

/// The walk of [`wav_sample_info`], over a file that `read` reads four
/// bytes of at a time, returning `false` past its end.
fn wav_sample_info_with(
    mut read: impl FnMut(u64, &mut [u8; 4]) -> bool,
) -> (Option<(u64, u64)>, Option<u8>) {
    let mut quad = |at: u64| -> Option<[u8; 4]> {
        let mut bytes = [0; 4];
        read(at, &mut bytes).then_some(bytes)
    };
    let is_wave = matches!(quad(0), Some(ref id) if id == b"RIFF" || id == b"RF64")
        && quad(8) == Some(*b"WAVE");
    if !is_wave {
        return (None, None);
    }
    let mut word = |at: u64| quad(at).map(u32::from_le_bytes);
    let mut position = 12u64;
    while let Some(size) = position.checked_add(4).and_then(&mut word) {
        let id = word(position).map(u32::to_le_bytes);
        let Some(body) = position.checked_add(8) else {
            break;
        };
        let mut at = |offset: u64| body.checked_add(offset).and_then(&mut word);
        if id == Some(*b"smpl") {
            let size = u64::from(size);
            let unity = at(12).filter(|_| size >= 36);
            let loops = at(28).filter(|_| size >= 36).unwrap_or(0);
            let root = unity
                .and_then(|key| u8::try_from(key).ok())
                .filter(|&key| key < 128);
            let first_loop = if loops > 0 && size >= 36 + 24 {
                match (at(36 + 8), at(36 + 12)) {
                    (Some(start), Some(end)) => Some((u64::from(start), u64::from(end))),
                    _ => None,
                }
            } else {
                None
            };
            return (first_loop, root);
        }
        let padded = u64::from(size) + u64::from(size & 1);
        let Some(next) = body.checked_add(padded) else {
            break;
        };
        position = next;
    }
    (None, None)
}

/// Resolves a region against its sample.
fn zone(region: &Region, sample: Option<(usize, &SampleData)>, generated: bool) -> Zone {
    let keycenter = if generated {
        60
    } else {
        match region.pitch_keycenter {
            KeyCenter::Key(key) => key,
            KeyCenter::FromSample => sample.and_then(|(_, data)| data.root_key).unwrap_or(60),
        }
    };
    let gain = 10f64.powf(region.volume / 20.0) * region.amplitude / 100.0;
    // A key of -1 is below every key: `lokey=-1` lets key 0 in, and
    // `hikey=-1` keeps every key out, which an empty range does.
    let (lokey, hikey) = match u8::try_from(region.hikey) {
        Ok(hikey) => (region.lokey.max(0) as u8, hikey),
        Err(_) => (1, 0),
    };
    let polyphony_scope = if region.group_set {
        PolyphonyScope::Group(region.group)
    } else {
        PolyphonyScope::Header(region.polyphony_header)
    };
    let mut zone = Zone {
        sample: None,
        lokey,
        hikey,
        lovel: region.lovel,
        hivel: region.hivel,
        lochan: region.lochan.saturating_sub(1),
        hichan: region.hichan.saturating_sub(1),
        lorand: region.lorand,
        hirand: region.hirand,
        seq_length: region.seq_length.max(1),
        seq_position: region.seq_position.max(1),
        trigger: region.trigger,
        gain,
        pan: region.pan / 100.0,
        veltrack: region.amp_veltrack / 100.0,
        rt_decay: region.rt_decay,
        keycenter: f64::from(keycenter),
        keytrack: region.pitch_keytrack,
        detune: f64::from(region.transpose) * 100.0 + region.tune,
        offset: region.offset,
        end: 0,
        one_shot: false,
        lp: None,
        envelope: region.ampeg,
        group: region.group,
        off_by: region.off_by,
        off_mode: region.off_mode,
        off_time: region.off_time,
        polyphony: region.polyphony,
        polyphony_scope,
        bend_up: region.bend_up,
        bend_down: region.bend_down,
        volume_db: region.volume,
        amplitude: region.amplitude / 100.0,
        filter: None,
        modulations: Box::default(),
        modulates: 0,
    };
    zone.filter = match (region.cutoff, region.fil_type) {
        (Some(cutoff), Some(kind)) => Some(ZoneFilter {
            kind,
            cutoff,
            resonance: region.resonance,
            keytrack: region.fil_keytrack,
            keycenter: f64::from(region.fil_keycenter),
            veltrack: region.fil_veltrack,
            depth: region.fileg_depth,
            vel2depth: region.fileg_vel2depth,
            envelope: region.fileg,
        }),
        _ => None,
    };
    let mut modulates = 0;
    zone.modulations = region
        .modulations
        .iter()
        .filter(|modulation| modulation.depth != 0.0)
        .filter_map(|modulation| {
            let (slot, bit) = slot(modulation.target);
            // What moves the filter means nothing without one.
            if (bit == 0 || bit == MODULATES_FILEG) && zone.filter.is_none() {
                return None;
            }
            modulates |= bit;
            Some(ZoneModulation {
                slot,
                controller: modulation.controller,
                depth: modulation.depth,
            })
        })
        .collect();
    zone.modulates = modulates;
    let Some((index, data)) = sample else {
        return zone;
    };
    let Some(span) = span(region, data.frames() as u64, data.rate, data.file_loop) else {
        return zone;
    };
    zone.sample = Some(index);
    zone.end = span.end;
    zone.one_shot = span.mode == LoopMode::OneShot;
    if let Some((start, loop_end, crossfade)) = span.lp {
        let len = loop_end - start + 1;
        let tail = |channel: usize| -> Vec<f32> {
            (0..crossfade)
                .map(|n| {
                    let at = loop_end + 1 - crossfade + n;
                    let before = at - len;
                    let fade = (n + 1) as f64 / (crossfade + 1) as f64;
                    // A streamed sample keeps its loops, and the frames
                    // their crossfades read, in memory.
                    let frame = |at: u64| f64::from(data.ram(channel, at).unwrap_or(0.0));
                    let mixed = frame(at) * (1.0 - fade) + frame(before) * fade;
                    mixed as f32
                })
                .collect()
        };
        zone.lp = Some(Loop {
            start,
            end: loop_end,
            sustain_only: span.mode == LoopMode::Sustain,
            tail_left: tail(0),
            tail_right: if data.stereo { tail(1) } else { Vec::new() },
        });
    }
    zone
}

/// What a region plays of its sample.
struct Span {
    /// Its last frame, included.
    end: u64,
    mode: LoopMode,
    /// Its loop's first and last frames, included, and the frames of its
    /// crossfade.
    lp: Option<(u64, u64, u64)>,
}

/// What `region` plays of a sample of `frames` frames at `rate` whose file
/// gives `file_loop`; `None` when it plays nothing.
fn span(region: &Region, frames: u64, rate: f64, file_loop: Option<(u64, u64)>) -> Option<Span> {
    let last = frames.checked_sub(1)?;
    let end = match region.end {
        Some(end) if end < 0 => return None,
        Some(0) | None => last,
        Some(end) => (end as u64).min(last),
    };
    if region.offset > end {
        return None;
    }
    let mode = region.loop_mode.unwrap_or(if file_loop.is_some() {
        LoopMode::Continuous
    } else {
        LoopMode::NoLoop
    });
    let mut lp = None;
    if matches!(mode, LoopMode::Continuous | LoopMode::Sustain) {
        let start = region
            .loop_start
            .or(file_loop.map(|(start, _)| start))
            .unwrap_or(0);
        let loop_end = region
            .loop_end
            .filter(|&loop_end| loop_end > 0)
            .or(file_loop.map(|(_, loop_end)| loop_end))
            .unwrap_or(end)
            .min(end);
        if start <= loop_end {
            let len = loop_end - start + 1;
            let wanted = (region.loop_crossfade * rate).round();
            let crossfade = if wanted.is_finite() && wanted >= 1.0 {
                (wanted as u64).min(len).min(start)
            } else {
                0
            };
            lp = Some((start, loop_end, crossfade));
        }
    }
    Some(Span { end, mode, lp })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemorySource;

    /// A WAV file of `frames` frames with a `smpl` chunk.
    fn wav_with_smpl(frames: usize, unity: u32, start: u32, end: u32) -> Vec<u8> {
        let buffer = manguebyte_core::AudioBuffer::<f32>::new(1, frames);
        let mut bytes = Vec::new();
        manguebyte_formats::wav::write(
            &mut bytes,
            &buffer,
            44_100,
            manguebyte_formats::wav::WavFormat::Float32,
        )
        .unwrap();
        let mut chunk = Vec::new();
        for value in [0u32, 0, 22_675, unity, 0, 0, 0, 1, 0] {
            chunk.extend_from_slice(&value.to_le_bytes());
        }
        for value in [0u32, 0, start, end, 0, 0] {
            chunk.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(b"smpl");
        bytes.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&chunk);
        let riff = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&riff.to_le_bytes());
        bytes
    }

    /// A WAV file of 44 bytes of header and four frames of 16-bit audio,
    /// in a data chunk that claims 0xFFFFFFD0 bytes, so that the chunk after
    /// it would start at 0xFFFFFFFC and the offsets past that overflow 32
    /// bits. `examples/wasm_smoke.rs` builds the same file and loads it on
    /// a 32-bit target.
    fn hostile_wav() -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        // PCM, one channel, 48 kHz, 96 000 bytes a second, 2-byte frames,
        // 16 bits.
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&0xFFFF_FFD0u32.to_le_bytes());
        for sample in [0i16, 1_000, 2_000, 1_000] {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn smpl_chunk_gives_loop_and_root() {
        let bytes = wav_with_smpl(100, 57, 10, 89);
        assert_eq!(wav_sample_info(&bytes), (Some((10, 89)), Some(57)));
        // Damaged chunks are ignored, never trusted.
        for cut in [12, 20, bytes.len() - 20] {
            assert_eq!(wav_sample_info(&bytes[..cut]).0, None, "{cut}");
        }
        assert_eq!(
            wav_sample_info(b"RIFF\xff\xff\xff\xffWAVEsmpl\xff\xff\xff\xff"),
            (None, None)
        );
        let mut source = MemorySource::new().with("a.wav", bytes);
        let sfz = Sfz::parse("<region> sample=a.wav pitch_keycenter=sample").unwrap();
        let instrument = Instrument::load(&sfz, &mut source).unwrap();
        let zone = &instrument.zones[0];
        assert_eq!(zone.keycenter, 57.0);
        let lp = zone.lp.as_ref().unwrap();
        assert_eq!((lp.start, lp.end, lp.sustain_only), (10, 89, false));
    }

    #[test]
    fn a_chunk_size_that_runs_the_walk_to_the_end_of_32_bits_is_ignored() {
        // The chunk after the data would start four bytes short of 2^32;
        // with 32-bit offsets the next ones would overflow. On a 64-bit
        // host this shows the walk ends and the file loads; the
        // `wasm_smoke` example checks it on wasm32.
        let bytes = hostile_wav();
        assert_eq!(wav_sample_info(&bytes), (None, None));
        let mut source = MemorySource::new().with("h.wav", bytes);
        let instrument = Instrument::from_sfz("<region> sample=h.wav", &mut source).unwrap();
        assert_eq!(instrument.sample_bytes(), 16);
    }

    #[test]
    fn regions_resolve_against_their_samples() {
        let mut source = MemorySource::new().with("a.wav", wav_with_smpl(100, 60, 0, 0));
        let text = "<region> sample=a.wav end=-1
                    <region> sample=a.wav end=500 loop_mode=loop_sustain loop_start=20 loop_end=900 loop_crossfade=1
                    <region> sample=a.wav offset=200
                    <region> sample=a.wav loop_mode=one_shot
                    <region> sample=*silence";
        let sfz = Sfz::parse(text).unwrap();
        let instrument = Instrument::load(&sfz, &mut source).unwrap();
        assert_eq!(instrument.num_samples(), 1);
        assert_eq!(instrument.sample_bytes(), 400);
        let zones = &instrument.zones;
        assert!(zones[0].sample.is_none());
        assert_eq!(zones[1].end, 99);
        let lp = zones[1].lp.as_ref().unwrap();
        // The loop end is held to the end, and the crossfade to the frames
        // before the loop start.
        assert_eq!(
            (lp.start, lp.end, lp.len(), lp.crossfade()),
            (20, 99, 80, 20)
        );
        assert!(zones[2].sample.is_none());
        assert!(zones[3].one_shot);
        assert!(zones[4].sample.is_none());
    }

    #[test]
    fn load_errors_name_the_region() {
        let sfz = Sfz::parse("\n<region> sample=missing.wav").unwrap();
        let error = Instrument::load(&sfz, &mut MemorySource::new()).unwrap_err();
        assert!(matches!(&error, LoadError::Read { location, .. } if location.line == 2));
        assert!(error.to_string().contains("missing.wav"));

        let mut source = MemorySource::new().with("bad.wav", b"RIFF1234WAVEjunk".to_vec());
        let sfz = Sfz::parse("<region> sample=bad.wav").unwrap();
        assert!(matches!(
            Instrument::load(&sfz, &mut source),
            Err(LoadError::Decode { .. })
        ));
        let sfz = Sfz::parse("<region> sample=*saw").unwrap();
        assert!(matches!(
            Instrument::load(&sfz, &mut source),
            Err(LoadError::UnknownGenerator { .. })
        ));
        let error = Instrument::open("no such dir/x.sfz").unwrap_err();
        assert!(matches!(
            error,
            InstrumentError::Load(LoadError::Read { .. })
        ));
    }

    #[test]
    fn random_numbers_and_round_robin_restart() {
        let mut source = MemorySource::new();
        let mut instrument = Instrument::from_sfz(
            "<region> sample=*sine seq_length=3 seq_position=2",
            &mut source,
        )
        .unwrap();
        let first: Vec<f64> = (0..4).map(|_| instrument.next_random()).collect();
        assert!(first.iter().all(|r| (0.0..1.0).contains(r)));
        let turns: Vec<bool> = (0..6).map(|_| instrument.take_turn(0)).collect();
        assert_eq!(turns, [false, true, false, false, true, false]);
        instrument.restart();
        let again: Vec<f64> = (0..4).map(|_| instrument.next_random()).collect();
        assert_eq!(first, again);
        assert!(!instrument.take_turn(0) && instrument.take_turn(0));
        instrument.set_seed(7);
        assert_eq!(instrument.seed(), 7);
        assert_ne!(instrument.next_random(), first[0]);
        assert!(!instrument.take_turn(99));
    }

    /// A WAV file of `channels` channels, each holding `samples`, at 1 kHz.
    fn plain_wav(samples: &[f32], channels: usize) -> Vec<u8> {
        let mut buffer = manguebyte_core::AudioBuffer::<f32>::new(channels, samples.len());
        for channel in 0..channels {
            buffer.channel_mut(channel).copy_from_slice(samples);
        }
        let mut bytes = Vec::new();
        manguebyte_formats::wav::write(
            &mut bytes,
            &buffer,
            1_000,
            manguebyte_formats::wav::WavFormat::Float32,
        )
        .unwrap();
        bytes
    }

    #[test]
    fn a_notes_energy_is_what_it_reads_at_its_gains() {
        // A second of 0.5 at 1 kHz: 0.25 a frame, in both channels of a mono
        // sample at the centre, 0.5 full-scale seconds in all.
        let half = vec![0.5f32; 1_000];
        let mut source = MemorySource::new()
            .with("mono.wav", plain_wav(&half, 1))
            .with("stereo.wav", plain_wav(&half, 2));
        let mut energy = |text: &str, key: u8, velocity: f32, seconds: f64| {
            Instrument::from_sfz(text, &mut source)
                .unwrap()
                .note_energy(key, velocity, seconds)
        };
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let mono = "<region> sample=mono.wav pitch_keycenter=60";
        assert!(close(energy(mono, 60, 1.0, 10.0), 0.5), "to its end");
        assert!(close(energy(mono, 60, 1.0, 0.25), 0.125), "for a quarter");
        // An octave up reads the second in half the time.
        assert!(close(energy(mono, 72, 1.0, 10.0), 0.25));
        assert!(close(
            energy(
                "<control> note_offset=12 <region> sample=mono.wav pitch_keycenter=72",
                60,
                1.0,
                10.0
            ),
            0.5
        ));
        // Gains: volume, amplitude, velocity tracking, the pan of a stereo
        // sample, which turns the other side down.
        let quarter = energy("<region> sample=mono.wav volume=-6.0206", 60, 1.0, 10.0);
        assert!((quarter - 0.125).abs() < 1e-6, "{quarter}");
        assert!(close(
            energy("<region> sample=mono.wav amplitude=50", 60, 1.0, 10.0),
            0.125
        ));
        let soft = (64.0f64 / 127.0).powi(4) * 0.5;
        assert!(close(energy(mono, 60, 0.5, 10.0), soft));
        assert!(close(
            energy("<region> sample=mono.wav amp_veltrack=0", 60, 0.5, 10.0),
            0.5
        ));
        assert!(close(
            energy("<region> sample=stereo.wav", 60, 1.0, 10.0),
            0.5
        ));
        assert!(close(
            energy("<region> sample=stereo.wav pan=100", 60, 1.0, 10.0),
            0.25
        ));
        // A mono sample panned keeps its power.
        assert!(close(
            energy("<region> sample=mono.wav pan=-60", 60, 1.0, 10.0),
            0.5
        ));
        // Layers add up; turns and random ranges share.
        let layers = "<region> sample=mono.wav <region> sample=mono.wav";
        assert!(close(energy(layers, 60, 1.0, 10.0), 1.0));
        let turns = "<region> sample=mono.wav seq_length=2 seq_position=1
                     <region> sample=mono.wav seq_length=2 seq_position=2";
        assert!(close(energy(turns, 60, 1.0, 10.0), 0.5));
        let chance = "<region> sample=mono.wav hirand=0.25 <region> sample=mono.wav lorand=0.25";
        assert!(close(energy(chance, 60, 1.0, 10.0), 0.5));
        // What a note on its own does not start counts for nothing.
        for silent in [
            "<region> sample=mono.wav trigger=legato",
            "<region> sample=mono.wav trigger=release",
            "<region> sample=mono.wav lovel=127",
            "<region> sample=mono.wav key=61",
            "<region> sample=mono.wav end=-1",
        ] {
            assert_eq!(energy(silent, 60, 0.5, 10.0), 0.0, "{silent}");
        }
        assert_eq!(energy(mono, 60, 0.0, 10.0), 0.0);
        assert_eq!(energy(mono, 60, f32::NAN, 10.0), 0.0);
        assert_eq!(energy(mono, 60, 1.0, f64::INFINITY), 0.0);
        assert_eq!(energy(mono, 60, 1.0, -1.0), 0.0);
    }

    #[test]
    fn a_notes_energy_goes_round_its_loop() {
        // 0.5 for 500 frames, then a loop of 100 frames of 1.
        let mut frames = vec![0.5f32; 500];
        frames.extend([1.0f32; 100]);
        let mut source = MemorySource::new().with("loop.wav", plain_wav(&frames, 1));
        let looped = Instrument::from_sfz(
            "<region> sample=loop.wav loop_mode=loop_continuous loop_start=500 loop_end=599",
            &mut source,
        )
        .unwrap();
        // Half a second before the loop, then the loop for the rest: 2 a
        // second of it, both channels.
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(close(looped.note_energy(60, 1.0, 0.5), 0.25));
        assert!(close(looped.note_energy(60, 1.0, 10.0), 0.25 + 9.5 * 2.0));
        assert!(close(looped.note_energy(60, 1.0, 0.55), 0.25 + 0.1));
        // An offset past the loop starts inside it.
        let inside = Instrument::from_sfz(
            "<region> sample=loop.wav offset=650 end=599 loop_mode=loop_sustain loop_start=500 loop_end=599",
            &mut source,
        )
        .unwrap();
        assert_eq!(
            inside.note_energy(60, 1.0, 1.0),
            0.0,
            "an offset past the end plays nothing"
        );
        let wrapped = Instrument::from_sfz(
            "<region> sample=loop.wav offset=550 loop_mode=loop_sustain loop_start=100 loop_end=199",
            &mut source,
        )
        .unwrap();
        // An offset past the loop enters it where `Player` does, at 150;
        // the loop holds 0.5, so a second of it is 0.25 a frame.
        assert!(close(wrapped.note_energy(60, 1.0, 1.0), 2.0 * 0.25 * 1.0));
    }

    #[test]
    fn a_clone_shares_the_samples_and_keeps_its_own_round_robins() {
        let mut source = MemorySource::new().with("a.wav", wav_with_smpl(100, 60, 0, 0));
        let mut instrument = Instrument::from_sfz(
            "<region> sample=a.wav seq_length=2 seq_position=1
             <region> sample=a.wav seq_length=2 seq_position=2",
            &mut source,
        )
        .unwrap();
        instrument.set_seed(11);
        assert!(instrument.take_turn(0));
        let _ = instrument.next_random();
        let clone = instrument.clone();
        // The same memory, not a copy of it.
        assert!(Arc::ptr_eq(&clone.samples, &instrument.samples));
        assert!(Arc::ptr_eq(&clone.zones, &instrument.zones));
        assert!(Arc::ptr_eq(&clone.attack_by_key, &instrument.attack_by_key));
        assert_eq!(clone.sample_bytes(), instrument.sample_bytes());
        assert_eq!((clone.num_regions(), clone.seed()), (2, 11));
        // It starts where the original was, then goes its own way: had the
        // two shared one counter, the original's call here would have been
        // its turn.
        assert_eq!(clone.next_random(), instrument.next_random());
        assert!(!clone.take_turn(0) && !instrument.take_turn(0));
        let turns: Vec<bool> = (0..4).map(|_| clone.take_turn(0)).collect();
        assert_eq!(turns, [true, false, true, false]);
        assert!(instrument.take_turn(0), "the original's turn is its own");
        // Dropped, the original lets go of the samples the clone still has.
        drop(instrument);
        assert_eq!(Arc::strong_count(&clone.samples), 1);
        assert_eq!(clone.samples[0].frames(), 100);
    }
}
