//! Samples streamed from disk: the options, the voice slots that carry
//! frames from the reader threads to the audio thread, and the readers.
//!
//! A streamed sample keeps a few stretches in memory (its head, from the
//! earliest `offset` of the regions that play it, and its loops); the rest
//! is read while it plays. Each voice playing such a sample holds a
//! [`Lease`] on one [`Slot`] of the instrument's pool: a ring of frames, a
//! request (which sample, from which frame) and two positions, all atomics.
//! The voice writes the request and how far it has read; a reader thread
//! fills the ring ahead of it and publishes how far it has filled. Neither
//! side ever waits for the other, and neither allocates once the pool is
//! made. See ADR 0014.

use std::fmt;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use manguebyte_core::AudioBuffer;
use manguebyte_formats::AudioReader;

use crate::instrument::Part;
use crate::packed::BLOCK;

/// Frames of each streamed sample kept in memory from its start, unless
/// [`StreamOptions::preload_frames`] says otherwise: about 1.4 s at
/// 48 kHz, far more than a disk takes to answer.
pub const DEFAULT_PRELOAD_FRAMES: usize = 65_536;

/// Frames of each voice's ring unless [`StreamOptions::ring_frames`] says
/// otherwise.
pub const DEFAULT_RING_FRAMES: usize = 65_536;

/// Longest loop kept in memory unless [`StreamOptions::loop_cache_frames`]
/// says otherwise.
pub const DEFAULT_LOOP_CACHE_FRAMES: usize = 1 << 20;

/// Voices that can stream at once unless [`StreamOptions::voices`] says
/// otherwise.
pub const DEFAULT_STREAM_VOICES: usize = 256;

/// Frames a reader reads at a time.
const CHUNK_FRAMES: usize = 4_096;

/// Files each reader thread keeps open; the one used longest ago is
/// closed when another is needed.
const OPEN_FILES: usize = 64;

/// Frames before and after a position that the interpolator may read:
/// more than the widest stretched sinc kernel reaches.
pub(crate) const MARGIN: u64 = crate::interpolate::MAX_TAPS as u64;

/// A request and a progress both pack a 16-bit generation above a 48-bit
/// frame, so that each is one atomic and cannot be read torn.
const GENERATION_SHIFT: u32 = 48;
const FRAME_MASK: u64 = (1 << GENERATION_SHIFT) - 1;
const GENERATION_MASK: u64 = 0xFFFF;

/// A function run at the start of each reader thread, with the thread's
/// number: the place to raise its priority with whatever system interface
/// the program has.
pub type ReaderStart = Arc<dyn Fn(usize) + Send + Sync>;

/// How an instrument streams its samples from disk, for
/// [`Instrument::open_streaming`](crate::Instrument::open_streaming) and
/// [`Instrument::load_streaming`](crate::Instrument::load_streaming).
///
/// The head of each sample (its first [`preload_frames`](Self::preload_frames)
/// from the earliest `offset` that plays it) and each of its loops stay in
/// memory, so a note starts at once and loops never touch the disk. The
/// rest is read while the note plays, by reader threads, into a ring per
/// voice of [`ring_frames`](Self::ring_frames). The preload is what a voice
/// plays while its first read is under way: at 48 kHz the default covers
/// 1.4 s of disk latency at the sample's own pitch, 0.7 s an octave up,
/// whatever the block size.
///
/// ```
/// use std::time::Duration;
/// use manguebyte_sampler::StreamOptions;
///
/// let options = StreamOptions::new()
///     .preload_frames(16_384)
///     .voices(128)
///     .reader_threads(2)
///     .poll_interval(Duration::from_micros(500));
/// assert_eq!(options.preload(), 16_384);
/// ```
#[derive(Clone)]
pub struct StreamOptions {
    pub(crate) preload_frames: usize,
    pub(crate) loop_cache_frames: usize,
    pub(crate) ring_frames: usize,
    pub(crate) voices: usize,
    pub(crate) reader_threads: usize,
    pub(crate) poll_interval: Duration,
    pub(crate) read_delay: Duration,
    pub(crate) on_reader_start: Option<ReaderStart>,
    pub(crate) ram_budget: Option<usize>,
    pub(crate) compress: bool,
}

impl fmt::Debug for StreamOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamOptions")
            .field("preload_frames", &self.preload_frames)
            .field("loop_cache_frames", &self.loop_cache_frames)
            .field("ring_frames", &self.ring_frames)
            .field("voices", &self.voices)
            .field("reader_threads", &self.reader_threads)
            .field("poll_interval", &self.poll_interval)
            .field("read_delay", &self.read_delay)
            .field("on_reader_start", &self.on_reader_start.is_some())
            .field("ram_budget", &self.ram_budget)
            .field("compress", &self.compress)
            .finish()
    }
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamOptions {
    /// The defaults: [`DEFAULT_PRELOAD_FRAMES`], [`DEFAULT_RING_FRAMES`],
    /// [`DEFAULT_LOOP_CACHE_FRAMES`], [`DEFAULT_STREAM_VOICES`], one reader
    /// thread polling every millisecond.
    pub fn new() -> Self {
        Self {
            preload_frames: DEFAULT_PRELOAD_FRAMES,
            loop_cache_frames: DEFAULT_LOOP_CACHE_FRAMES,
            ring_frames: DEFAULT_RING_FRAMES,
            voices: DEFAULT_STREAM_VOICES,
            reader_threads: 1,
            poll_interval: Duration::from_millis(1),
            read_delay: Duration::ZERO,
            on_reader_start: None,
            ram_budget: None,
            compress: false,
        }
    }

    /// Frames kept in memory from the start of each sample (at least 1).
    pub fn preload_frames(mut self, frames: usize) -> Self {
        self.preload_frames = frames.max(1);
        self
    }

    /// The frames [`preload_frames`](Self::preload_frames) set.
    pub fn preload(&self) -> usize {
        self.preload_frames
    }

    /// Longest loop, in frames, kept in memory. A sample with a longer
    /// loop is kept in memory whole: loops are never streamed.
    pub fn loop_cache_frames(mut self, frames: usize) -> Self {
        self.loop_cache_frames = frames;
        self
    }

    /// Frames of each voice's ring, rounded up to a power of two and to at
    /// least four reads of 4096 frames. It must hold what a voice reads in
    /// a block, at its highest pitch, plus what the disk takes to answer.
    pub fn ring_frames(mut self, frames: usize) -> Self {
        self.ring_frames = frames;
        self
    }

    /// How many voices can stream at once (1 to 4096). A voice that finds
    /// every one in use plays what is in memory and then fades out,
    /// counted as an underrun.
    pub fn voices(mut self, voices: usize) -> Self {
        self.voices = voices.clamp(1, crate::MAX_POLYPHONY);
        self
    }

    /// How many threads read (1 to 16). The voices are shared out among
    /// them; more than one helps when the files are on several disks, or on
    /// one that serves reads in parallel, as solid-state disks do.
    pub fn reader_threads(mut self, threads: usize) -> Self {
        self.reader_threads = threads.clamp(1, 16);
        self
    }

    /// How long a reader with nothing to do sleeps before looking again.
    /// The audio thread never wakes it (that would be a system call), so
    /// this is the most a new voice waits before its first read starts.
    pub fn poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval.max(Duration::from_micros(50));
        self
    }

    /// Runs `start` at the start of each reader thread, with its number
    /// from 0: where a program raises the threads' priority (on Windows,
    /// `SetThreadPriority` and an I/O priority hint) with the system
    /// interface it already uses. This crate calls none itself.
    pub fn on_reader_start(mut self, start: impl Fn(usize) + Send + Sync + 'static) -> Self {
        self.on_reader_start = Some(Arc::new(start));
        self
    }

    /// Most bytes of audio the samples may keep in memory: their heads,
    /// their loops and the samples kept whole. When the preload asked for
    /// would take more, every head is shortened alike, to the longest that
    /// fits, but not below 4096 frames; loading fails with what it would
    /// need if even that does not fit. The rings (see
    /// [`ring_frames`](Self::ring_frames)) are counted apart.
    pub fn ram_budget(mut self, bytes: usize) -> Self {
        self.ram_budget = Some(bytes);
        self
    }

    /// Holds what stays in memory (heads, loops, samples kept whole)
    /// packed without loss, block by block, and unpacks each block into the
    /// voice's slot as the voice reaches it: the same bits come back, in
    /// about half the memory of 16-bit material. Every voice that plays a
    /// packed sample then needs a slot, as a streaming one does. To hold
    /// whole samples packed in memory, with no streaming, ask for a preload
    /// of `usize::MAX` as well. The budget is checked against the sizes
    /// unpacked, so a packed set stays well within it.
    pub fn compress(mut self, compress: bool) -> Self {
        self.compress = compress;
        self
    }

    /// Makes each reader sleep this long after every read of 4096 frames:
    /// a slow disk on demand, for testing how a sampler copes with one.
    pub fn read_delay(mut self, delay: Duration) -> Self {
        self.read_delay = delay;
        self
    }

    /// The ring's frames as allocated.
    pub(crate) fn ring_capacity(&self) -> usize {
        self.ring_frames
            .clamp(4 * CHUNK_FRAMES, 1 << 30)
            .next_power_of_two()
    }
}

/// A file a streamed sample is read from.
#[derive(Debug, Clone)]
pub(crate) struct StreamFile {
    pub(crate) path: PathBuf,
    pub(crate) frames: u64,
}

/// One voice's ring and the atomics that coordinate it.
pub(crate) struct Slot {
    claimed: AtomicBool,
    /// Generation and first frame of what the voice wants.
    request: AtomicU64,
    sample: AtomicU32,
    /// The lowest frame the voice may still read; the reader writes no
    /// frame a ring's length or more beyond it.
    consumed: AtomicU64,
    /// Generation of the request served, and the frame after the last one
    /// written for it.
    progress: AtomicU64,
    left: Box<[AtomicU32]>,
    right: Box<[AtomicU32]>,
    mask: u64,
    /// Blocks of packed parts, unpacked: [`CACHE_BLOCKS`] of
    /// [`BLOCK`] frames of each of `cache_channels` channels. Only the
    /// voice that holds the slot touches them.
    cache: Box<[AtomicU32]>,
    cache_channels: usize,
    /// For each cached block, the address of its part (0 for none), its
    /// number in the part, and when it was last used.
    cache_part: [AtomicUsize; CACHE_BLOCKS],
    cache_block: [AtomicUsize; CACHE_BLOCKS],
    cache_used: [AtomicU64; CACHE_BLOCKS],
    cache_clock: AtomicU64,
}

/// Blocks of packed parts each voice keeps unpacked: enough for a window
/// of the interpolator across two blocks, and the two at the ends of a
/// loop.
const CACHE_BLOCKS: usize = 4;

impl Slot {
    fn new(capacity: usize, stereo: bool, cache_channels: usize) -> Self {
        let ring = |frames: usize| (0..frames).map(|_| AtomicU32::new(0)).collect();
        Self {
            cache: ring(CACHE_BLOCKS * BLOCK * cache_channels),
            cache_channels,
            cache_part: [const { AtomicUsize::new(0) }; CACHE_BLOCKS],
            cache_block: [const { AtomicUsize::new(0) }; CACHE_BLOCKS],
            cache_used: [const { AtomicU64::new(0) }; CACHE_BLOCKS],
            cache_clock: AtomicU64::new(0),
            claimed: AtomicBool::new(false),
            request: AtomicU64::new(0),
            sample: AtomicU32::new(0),
            consumed: AtomicU64::new(0),
            progress: AtomicU64::new(0),
            left: ring(capacity),
            right: if stereo { ring(capacity) } else { ring(0) },
            mask: capacity as u64 - 1,
        }
    }

    fn capacity(&self) -> u64 {
        self.mask + 1
    }

    fn bytes(&self) -> usize {
        (self.left.len() + self.right.len() + self.cache.len()) * 4
    }

    /// The cache entry holding block `block` of `part`, unpacking it into
    /// the entry used longest ago if no entry does. `None` if the slot has
    /// no cache or the part is not packed. Never allocates.
    fn cached(&self, part: &Part, block: usize) -> Option<usize> {
        let packed = part.packed.as_ref()?;
        if self.cache_channels == 0 {
            return None;
        }
        let address = part as *const Part as usize;
        let now = self.cache_clock.load(Ordering::Relaxed) + 1;
        self.cache_clock.store(now, Ordering::Relaxed);
        let mut oldest = 0;
        for entry in 0..CACHE_BLOCKS {
            if self.cache_part[entry].load(Ordering::Relaxed) == address
                && self.cache_block[entry].load(Ordering::Relaxed) == block
            {
                self.cache_used[entry].store(now, Ordering::Relaxed);
                return Some(entry);
            }
            if self.cache_used[entry].load(Ordering::Relaxed)
                < self.cache_used[oldest].load(Ordering::Relaxed)
            {
                oldest = entry;
            }
        }
        let mut frames = [0.0f32; BLOCK];
        let base = oldest * BLOCK * self.cache_channels;
        let count = packed.left.unpack(block, &mut frames);
        for (offset, value) in frames[..count].iter().enumerate() {
            self.cache[base + offset].store(value.to_bits(), Ordering::Relaxed);
        }
        if let (Some(right), 2) = (&packed.right, self.cache_channels) {
            let count = right.unpack(block, &mut frames);
            for (offset, value) in frames[..count].iter().enumerate() {
                self.cache[base + BLOCK + offset].store(value.to_bits(), Ordering::Relaxed);
            }
        }
        self.cache_part[oldest].store(address, Ordering::Relaxed);
        self.cache_block[oldest].store(block, Ordering::Relaxed);
        self.cache_used[oldest].store(now, Ordering::Relaxed);
        Some(oldest)
    }
}

/// What the reader threads and the voices share.
struct Shared {
    slots: Box<[Arc<Slot>]>,
    /// By sample index; `None` for the samples held whole in memory.
    files: Box<[Option<StreamFile>]>,
    stop: AtomicBool,
    /// Where the next claim starts looking, so that voices spread over the
    /// slots and so over the threads.
    hint: AtomicUsize,
    read_errors: AtomicU64,
    underruns: AtomicU64,
    options: StreamOptions,
}

/// The voice slots of an instrument that streams, and its reader threads.
/// Dropping it stops the threads and waits for them to end.
pub(crate) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Pool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pool")
            .field("slots", &self.shared.slots.len())
            .field("threads", &self.threads.len())
            .finish_non_exhaustive()
    }
}

impl Pool {
    /// Makes the slots and starts the readers.
    /// `stereo` says whether a streamed sample has two channels, and
    /// `cache_channels` the most channels of a packed one (0 when none is
    /// packed). Without files to stream, no thread is started and the rings
    /// are left empty.
    pub(crate) fn new(
        files: Vec<Option<StreamFile>>,
        stereo: bool,
        cache_channels: usize,
        options: &StreamOptions,
    ) -> io::Result<Self> {
        let streams = files.iter().any(Option::is_some);
        let capacity = if streams { options.ring_capacity() } else { 1 };
        let slots = (0..options.voices)
            .map(|_| Arc::new(Slot::new(capacity, stereo && streams, cache_channels)))
            .collect();
        let shared = Arc::new(Shared {
            slots,
            files: files.into(),
            stop: AtomicBool::new(false),
            hint: AtomicUsize::new(0),
            read_errors: AtomicU64::new(0),
            underruns: AtomicU64::new(0),
            options: options.clone(),
        });
        let mut pool = Self {
            shared,
            threads: Vec::new(),
        };
        let count = if streams {
            options.reader_threads.min(options.voices).max(1)
        } else {
            0
        };
        for index in 0..count {
            let shared = Arc::clone(&pool.shared);
            // On failure the threads already started are stopped by the
            // pool's drop.
            let handle = thread::Builder::new()
                .name(format!("manguebyte-sampler-reader-{index}"))
                .spawn(move || read_loop(&shared, index, count))?;
            pool.threads.push(handle);
        }
        Ok(pool)
    }

    /// Whether any sample is read from a file.
    pub(crate) fn streams(&self) -> bool {
        self.shared.files.iter().any(Option::is_some)
    }

    /// Underruns counted by every voice so far.
    pub(crate) fn underruns(&self) -> u64 {
        self.shared.underruns.load(Ordering::Relaxed)
    }

    /// Counts an underrun. Real-time safe.
    pub(crate) fn count_underrun(&self) {
        self.shared.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Reads that failed so far.
    pub(crate) fn read_errors(&self) -> u64 {
        self.shared.read_errors.load(Ordering::Relaxed)
    }

    /// Bytes of the rings.
    pub(crate) fn ring_bytes(&self) -> usize {
        self.shared.slots.iter().map(|slot| slot.bytes()).sum()
    }

    /// How many voices hold a slot now.
    pub(crate) fn streaming(&self) -> usize {
        self.shared
            .slots
            .iter()
            .filter(|slot| slot.claimed.load(Ordering::Relaxed))
            .count()
    }

    /// Waits until every voice's ring is full or holds the rest of its
    /// file, or `timeout` passes; returns whether it got there. For
    /// rendering faster than real time, where nothing else waits for the
    /// disk. Not for the audio thread.
    pub(crate) fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.shared.slots.iter().all(|slot| self.is_ready(slot)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_micros(100));
        }
    }

    fn is_ready(&self, slot: &Slot) -> bool {
        if !slot.claimed.load(Ordering::Acquire) {
            return true;
        }
        let request = slot.request.load(Ordering::Acquire);
        let progress = slot.progress.load(Ordering::Acquire);
        let Some(file) = self
            .shared
            .files
            .get(slot.sample.load(Ordering::Relaxed) as usize)
            .and_then(Option::as_ref)
        else {
            return true;
        };
        if progress >> GENERATION_SHIFT != request >> GENERATION_SHIFT {
            return (request & FRAME_MASK) >= file.frames;
        }
        let filled = progress & FRAME_MASK;
        let room = slot
            .consumed
            .load(Ordering::Acquire)
            .saturating_add(slot.capacity());
        filled >= file.frames.min(room)
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        for handle in &self.threads {
            handle.thread().unpark();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

/// A voice's hold on a slot: what it asked for and what it has seen
/// arrive. Dropping it gives the slot back. Real-time safe: claiming,
/// asking, reading and dropping never allocate, free, lock or wait (the
/// pool keeps every slot alive, so a lease never drops the last
/// reference).
pub(crate) struct Lease {
    slot: Option<Arc<Slot>>,
    generation: u64,
    sample: u32,
    /// First frame of the request.
    start: u64,
    /// The lowest frame still wanted, as last published.
    consumed: u64,
    /// The frame after the last one known to have arrived.
    available: u64,
}

impl fmt::Debug for Lease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease")
            .field("held", &self.slot.is_some())
            .field("start", &self.start)
            .field("available", &self.available)
            .finish_non_exhaustive()
    }
}

impl Clone for Lease {
    /// No lease: a slot has one holder.
    fn clone(&self) -> Self {
        Self::NONE
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(slot) = &self.slot {
            slot.claimed.store(false, Ordering::Release);
        }
    }
}

impl Lease {
    /// Holding no slot.
    pub(crate) const NONE: Self = Self {
        slot: None,
        generation: 0,
        sample: 0,
        start: 0,
        consumed: 0,
        available: 0,
    };

    /// Claims a free slot of `pool` and asks for `sample` from `start`.
    /// `None` when every slot is taken.
    pub(crate) fn claim(pool: &Pool, sample: u32, start: u64) -> Option<Self> {
        let slots = &pool.shared.slots;
        let first = pool.shared.hint.fetch_add(1, Ordering::Relaxed);
        for turn in 0..slots.len() {
            let slot = &slots[(first + turn) % slots.len()];
            if slot
                .claimed
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                let generation = slot.request.load(Ordering::Relaxed) >> GENERATION_SHIFT;
                // What the slot's last holder unpacked may be of parts that
                // are gone.
                for part in &slot.cache_part {
                    part.store(0, Ordering::Relaxed);
                }
                let mut lease = Self {
                    slot: Some(Arc::clone(slot)),
                    generation,
                    ..Self::NONE
                };
                lease.request(sample, start);
                return Some(lease);
            }
        }
        None
    }

    /// Whether it holds a slot.
    pub(crate) fn is_held(&self) -> bool {
        self.slot.is_some()
    }

    /// The first frame asked for.
    pub(crate) fn start(&self) -> u64 {
        self.start
    }

    /// Asks for `sample` from `start` on, forgetting what was asked before.
    pub(crate) fn request(&mut self, sample: u32, start: u64) {
        let Some(slot) = &self.slot else {
            return;
        };
        let start = start.min(FRAME_MASK);
        self.generation = (self.generation + 1) & GENERATION_MASK;
        self.sample = sample;
        self.start = start;
        self.consumed = start;
        self.available = start;
        slot.consumed.store(start, Ordering::Relaxed);
        slot.sample.store(sample, Ordering::Relaxed);
        slot.request.store(
            (self.generation << GENERATION_SHIFT) | start,
            Ordering::Release,
        );
    }

    /// The lowest frame still wanted.
    pub(crate) fn consumed(&self) -> u64 {
        self.consumed
    }

    /// The frame after the last one known to have arrived.
    pub(crate) fn available(&self) -> u64 {
        self.available
    }

    /// Asks for the same sample from `start` on.
    pub(crate) fn restart(&mut self, start: u64) {
        self.request(self.sample, start);
    }

    /// Takes note of what has arrived since the last look.
    pub(crate) fn refresh(&mut self) {
        let Some(slot) = &self.slot else {
            return;
        };
        let progress = slot.progress.load(Ordering::Acquire);
        if progress >> GENERATION_SHIFT == self.generation {
            self.available = (progress & FRAME_MASK).max(self.start);
        }
    }

    /// Frame `index` of the packed `part`, left and right (0 for a mono
    /// part), unpacking its block into the slot's cache if it is not there.
    /// `None` without a slot. Real-time safe: unpacking a block is
    /// bounded work, and allocates nothing.
    #[inline]
    pub(crate) fn packed_frame(&self, part: &Part, index: usize) -> Option<(f32, f32)> {
        let slot = self.slot.as_ref()?;
        let entry = slot.cached(part, index / BLOCK)?;
        let base = entry * BLOCK * slot.cache_channels + index % BLOCK;
        let left = f32::from_bits(slot.cache.get(base)?.load(Ordering::Relaxed));
        let right = if slot.cache_channels == 2 {
            f32::from_bits(slot.cache.get(base + BLOCK)?.load(Ordering::Relaxed))
        } else {
            0.0
        };
        Some((left, right))
    }

    /// Unpacks, ahead of need, the block of `part` after the one that holds
    /// frame `index`, if there is one.
    pub(crate) fn unpack_ahead(&self, part: &Part, index: usize) {
        let next = index / BLOCK + 1;
        if let Some(slot) = &self.slot {
            if next * BLOCK < part.frames {
                slot.cached(part, next);
            }
        }
    }

    /// Lets the reader overwrite the frames before `frame`. Never moves
    /// back.
    pub(crate) fn release_before(&mut self, frame: u64) {
        let Some(slot) = &self.slot else {
            return;
        };
        let frame = frame.min(self.available);
        if frame > self.consumed {
            self.consumed = frame;
            slot.consumed.store(frame, Ordering::Release);
        }
    }

    /// The frame `at` of the left and right channels, if it has arrived
    /// and has not been given up (the right is 0 for a mono ring).
    #[inline]
    pub(crate) fn frame(&self, at: u64) -> Option<(f32, f32)> {
        let slot = self.slot.as_ref()?;
        if at < self.consumed || at >= self.available {
            return None;
        }
        let index = (at & slot.mask) as usize;
        let left = f32::from_bits(slot.left.get(index)?.load(Ordering::Relaxed));
        let right = slot
            .right
            .get(index)
            .map_or(0.0, |right| f32::from_bits(right.load(Ordering::Relaxed)));
        Some((left, right))
    }
}

/// What a reader thread knows of one of its slots.
#[derive(Clone, Copy)]
struct Serving {
    request: Option<u64>,
    sample: u32,
    next: u64,
    failed: bool,
}

const NOT_SERVING: Serving = Serving {
    request: None,
    sample: 0,
    next: 0,
    failed: false,
};

/// A file a reader holds open, with a buffer for what it reads.
struct OpenFile {
    sample: u32,
    reader: AudioReader<File>,
    scratch: AudioBuffer<f32>,
    used: u64,
}

fn read_loop(shared: &Shared, index: usize, threads: usize) {
    if let Some(start) = &shared.options.on_reader_start {
        start(index);
    }
    let mine: Vec<usize> = (index..shared.slots.len()).step_by(threads).collect();
    let mut serving = vec![NOT_SERVING; mine.len()];
    let mut open: Vec<OpenFile> = Vec::with_capacity(OPEN_FILES);
    let mut clock = 0u64;
    while !shared.stop.load(Ordering::Acquire) {
        // The slot whose voice has the least read ahead goes first.
        let mut urgent: Option<(usize, u64)> = None;
        for (k, &at) in mine.iter().enumerate() {
            let slot = &shared.slots[at];
            let state = &mut serving[k];
            if !slot.claimed.load(Ordering::Acquire) {
                *state = NOT_SERVING;
                continue;
            }
            let request = slot.request.load(Ordering::Acquire);
            if state.request != Some(request) {
                *state = Serving {
                    request: Some(request),
                    sample: slot.sample.load(Ordering::Relaxed),
                    next: request & FRAME_MASK,
                    failed: false,
                };
            }
            let Some(file) = shared
                .files
                .get(state.sample as usize)
                .and_then(Option::as_ref)
            else {
                continue;
            };
            if state.failed || state.next >= file.frames {
                continue;
            }
            let consumed = slot.consumed.load(Ordering::Acquire);
            if state.next >= consumed.saturating_add(slot.capacity()) {
                continue;
            }
            let lead = state.next.saturating_sub(consumed);
            if urgent.is_none_or(|(_, least)| lead < least) {
                urgent = Some((k, lead));
            }
        }
        let Some((k, _)) = urgent else {
            thread::park_timeout(shared.options.poll_interval);
            continue;
        };
        clock += 1;
        let slot = &shared.slots[mine[k]];
        let state = &mut serving[k];
        if fill(shared, slot, state, &mut open, clock).is_err() {
            state.failed = true;
            shared.read_errors.fetch_add(1, Ordering::Relaxed);
        }
        // The slow disk of a test, which a drop of the pool cuts short.
        let until = Instant::now() + shared.options.read_delay;
        while !shared.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= until {
                break;
            }
            thread::park_timeout(until - now);
        }
    }
}

/// Reads the next chunk of a slot's request into its ring and publishes
/// it, unless the request changed meanwhile.
fn fill(
    shared: &Shared,
    slot: &Slot,
    state: &mut Serving,
    open: &mut Vec<OpenFile>,
    clock: u64,
) -> Result<(), manguebyte_formats::FormatError> {
    let Some(request) = state.request else {
        return Ok(());
    };
    let Some(file) = shared
        .files
        .get(state.sample as usize)
        .and_then(Option::as_ref)
    else {
        return Ok(());
    };
    let consumed = slot.consumed.load(Ordering::Acquire);
    let room = consumed.saturating_add(slot.capacity());
    let count =
        (room.min(file.frames).saturating_sub(state.next)).min(CHUNK_FRAMES as u64) as usize;
    if count == 0 {
        return Ok(());
    }
    let entry = open_file(open, state.sample, file, clock)?;
    if entry.reader.position() != state.next {
        entry.reader.seek(state.next)?;
    }
    entry.scratch.set_num_frames(count);
    let read = entry.reader.read(&mut entry.scratch)?;
    if read == 0 {
        // The file is shorter than its header said.
        state.failed = true;
        return Ok(());
    }
    let stereo = entry.scratch.num_channels() > 1 && !slot.right.is_empty();
    for offset in 0..read {
        let at = ((state.next + offset as u64) & slot.mask) as usize;
        slot.left[at].store(
            entry.scratch.channel(0)[offset].to_bits(),
            Ordering::Relaxed,
        );
        if stereo {
            slot.right[at].store(
                entry.scratch.channel(1)[offset].to_bits(),
                Ordering::Relaxed,
            );
        }
    }
    if slot.claimed.load(Ordering::Acquire) && slot.request.load(Ordering::Acquire) == request {
        state.next += read as u64;
        let generation = request >> GENERATION_SHIFT;
        slot.progress.store(
            (generation << GENERATION_SHIFT) | state.next,
            Ordering::Release,
        );
    }
    Ok(())
}

/// The open file of `sample`, opening it (and closing the one used longest
/// ago, if as many as allowed are open) if it is not.
fn open_file<'a>(
    open: &'a mut Vec<OpenFile>,
    sample: u32,
    file: &StreamFile,
    clock: u64,
) -> Result<&'a mut OpenFile, manguebyte_formats::FormatError> {
    let found = match open.iter().position(|entry| entry.sample == sample) {
        Some(found) => found,
        None => {
            let reader = AudioReader::open(&file.path)?;
            let scratch = AudioBuffer::new(reader.num_channels().max(1), CHUNK_FRAMES);
            let entry = OpenFile {
                sample,
                reader,
                scratch,
                used: clock,
            };
            if open.len() < OPEN_FILES {
                open.push(entry);
                open.len() - 1
            } else {
                let oldest = open
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.used)
                    .map_or(0, |(at, _)| at);
                open[oldest] = entry;
                oldest
            }
        }
    };
    let entry = &mut open[found];
    entry.used = clock;
    Ok(entry)
}
