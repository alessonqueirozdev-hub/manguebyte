//! An SFZ instrument with its samples decoded, ready to play.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use manguebyte_formats::FormatError;

use crate::interpolate::SincTable;
use crate::region::{KeyCenter, LoopMode, OffMode, Region, Trigger};
use crate::sfz::{Location, Sfz, SfzError, Warning};
use crate::source::{FileSource, SampleSource};

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
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { error, .. } => Some(error),
            Self::Decode { error, .. } => Some(error),
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

/// A decoded sample: one or two channels.
pub(crate) struct SampleData {
    pub(crate) left: Vec<f32>,
    pub(crate) right: Option<Vec<f32>>,
    pub(crate) rate: f64,
    /// The first loop a WAV file's `smpl` chunk gives, both ends included.
    file_loop: Option<(u64, u64)>,
    /// The key a WAV file's `smpl` chunk says the sample sounds at.
    root_key: Option<u8>,
}

impl SampleData {
    pub(crate) fn frames(&self) -> usize {
        self.left.len()
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
/// ```
/// use manguebyte_sampler::{Instrument, MemorySource};
///
/// let instrument = Instrument::from_sfz(
///     "<region> sample=*sine lokey=0 hikey=127",
///     &mut MemorySource::new(),
/// )?;
/// assert_eq!(instrument.num_regions(), 1);
/// assert!(instrument.warnings().is_empty());
/// # Ok::<(), manguebyte_sampler::InstrumentError>(())
/// ```
pub struct Instrument {
    pub(crate) zones: Vec<Zone>,
    pub(crate) samples: Vec<SampleData>,
    /// Per key, the zones that start on a key going down.
    pub(crate) attack_by_key: Vec<Vec<u32>>,
    /// Per key, the zones that start on a key coming up.
    pub(crate) release_by_key: Vec<Vec<u32>>,
    pub(crate) sequence: Vec<AtomicU32>,
    random: AtomicU64,
    seed: u64,
    pub(crate) note_shift: i32,
    pub(crate) sinc: SincTable,
    warnings: Vec<Warning>,
}

impl fmt::Debug for Instrument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instrument")
            .field("regions", &self.zones.len())
            .field("samples", &self.samples.len())
            .field("seed", &self.seed)
            .field("warnings", &self.warnings.len())
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
        let mut zones = Vec::with_capacity(sfz.regions.len());
        for region in &sfz.regions {
            let index = match by_path.get(&*region.sample) {
                Some(&index) => index,
                None => {
                    let loaded = load_sample(&region.sample, &region.location, source)?;
                    let index = loaded.map(|data| {
                        samples.push(data);
                        samples.len() - 1
                    });
                    by_path.insert(Arc::clone(&region.sample), index);
                    index
                }
            };
            let generated = region.sample.starts_with('*');
            zones.push(zone(region, index.map(|i| (i, &samples[i])), generated));
        }
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
        Ok(Self {
            zones,
            samples,
            attack_by_key,
            release_by_key,
            sequence,
            random: AtomicU64::new(DEFAULT_SEED),
            seed: DEFAULT_SEED,
            note_shift: sfz.control.note_shift(),
            sinc: SincTable::new(),
            warnings: sfz.warnings.clone(),
        })
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
        self.samples
            .iter()
            .map(|s| (s.left.len() + s.right.as_ref().map_or(0, Vec::len)) * 4)
            .sum()
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
    Ok(Some(SampleData {
        left,
        right,
        rate: f64::from(file.sample_rate.max(1)),
        file_loop: file_loop.filter(|&(start, end)| start <= end && end < frames as u64),
        root_key,
    }))
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
    SampleData {
        left,
        right: None,
        rate: middle_c * SINE_FRAMES as f64,
        file_loop: Some((0, SINE_FRAMES as u64 - 1)),
        root_key: Some(60),
    }
}

/// The first loop and the root key in a WAV file's `smpl` chunk, if it has
/// one. Anything malformed is ignored.
fn wav_sample_info(bytes: &[u8]) -> (Option<(u64, u64)>, Option<u8>) {
    // Offsets are worked out in 64 bits, where a chunk walk of a file that
    // fits in memory cannot overflow, and only then turned into indices, so
    // that a hostile chunk size cannot overflow a 32-bit `usize`.
    let word = |at: u64| -> Option<u32> {
        let start = usize::try_from(at).ok()?;
        let slice = bytes.get(start..start.checked_add(4)?)?;
        Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
    };
    let is_wave =
        matches!(bytes.get(..4), Some(b"RIFF" | b"RF64")) && bytes.get(8..12) == Some(b"WAVE");
    if !is_wave {
        return (None, None);
    }
    let mut position = 12u64;
    while let Some(size) = position.checked_add(4).and_then(word) {
        let id = usize::try_from(position)
            .ok()
            .and_then(|start| bytes.get(start..start.checked_add(4)?));
        let Some(body) = position.checked_add(8) else {
            break;
        };
        let at = |offset: u64| body.checked_add(offset).and_then(word);
        if id == Some(b"smpl") {
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
    };
    let Some((index, data)) = sample else {
        return zone;
    };
    let last = data.frames() as u64 - 1;
    let end = match region.end {
        Some(end) if end < 0 => return zone,
        Some(0) | None => last,
        Some(end) => (end as u64).min(last),
    };
    if region.offset > end {
        return zone;
    }
    zone.sample = Some(index);
    zone.end = end;
    let mode = region.loop_mode.unwrap_or(if data.file_loop.is_some() {
        LoopMode::Continuous
    } else {
        LoopMode::NoLoop
    });
    zone.one_shot = mode == LoopMode::OneShot;
    if matches!(mode, LoopMode::Continuous | LoopMode::Sustain) {
        let start = region
            .loop_start
            .or(data.file_loop.map(|(start, _)| start))
            .unwrap_or(0);
        let loop_end = region
            .loop_end
            .filter(|&loop_end| loop_end > 0)
            .or(data.file_loop.map(|(_, loop_end)| loop_end))
            .unwrap_or(end)
            .min(end);
        if start <= loop_end {
            let len = loop_end - start + 1;
            let wanted = (region.loop_crossfade * data.rate).round();
            let crossfade = if wanted.is_finite() && wanted >= 1.0 {
                (wanted as u64).min(len).min(start)
            } else {
                0
            };
            let tail = |channel: &[f32]| -> Vec<f32> {
                (0..crossfade)
                    .map(|n| {
                        let at = (loop_end + 1 - crossfade + n) as usize;
                        let before = at - len as usize;
                        let fade = (n + 1) as f64 / (crossfade + 1) as f64;
                        let mixed = f64::from(channel[at]) * (1.0 - fade)
                            + f64::from(channel[before]) * fade;
                        mixed as f32
                    })
                    .collect()
            };
            zone.lp = Some(Loop {
                start,
                end: loop_end,
                sustain_only: mode == LoopMode::Sustain,
                tail_left: tail(&data.left),
                tail_right: data.right.as_deref().map(tail).unwrap_or_default(),
            });
        }
    }
    zone
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
}
