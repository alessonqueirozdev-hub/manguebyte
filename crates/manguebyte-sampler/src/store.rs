//! Samples kept partly in memory and streamed from disk, for any player:
//! [`SampleStore`] and [`StreamCursor`]. The sampler's own voices read
//! through the same layout, rings and readers.

use std::fmt;
use std::io;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_formats::{AudioReader, FileFormat, FormatError};

use crate::instrument::{Part, SampleData, wav_file_sample_info};
use crate::stream::{Lease, Pool, StreamFile, StreamOptions};

/// The shortest head a [`ram_budget`](StreamOptions::ram_budget) can cut a
/// preload to.
pub(crate) const MIN_PRELOAD: u64 = 4_096;

/// Frames read at a time while loading.
const LOAD_CHUNK: usize = 4_096;

/// What a file holds, as its header says.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FileInfo {
    pub(crate) frames: u64,
    pub(crate) channels: usize,
    pub(crate) rate: f64,
    pub(crate) file_loop: Option<(u64, u64)>,
    pub(crate) root_key: Option<u8>,
}

/// Reads the header of the file at `path`: `None` when it is not one that
/// can be streamed (WAV, AIFF and FLAC can, when they state their length).
pub(crate) fn probe(path: &Path) -> Result<Option<FileInfo>, FormatError> {
    let reader = AudioReader::open(path)?;
    let format = reader.format();
    if !matches!(
        format,
        FileFormat::Wav | FileFormat::Aiff | FileFormat::Flac
    ) {
        return Ok(None);
    }
    let Some(frames) = reader.len_frames() else {
        return Ok(None);
    };
    let (file_loop, root_key) = if format == FileFormat::Wav {
        let mut file = std::fs::File::open(path)?;
        wav_file_sample_info(&mut file)
    } else {
        (None, None)
    };
    Ok(Some(FileInfo {
        frames,
        channels: reader.num_channels(),
        rate: f64::from(reader.sample_rate().max(1)),
        file_loop: file_loop.filter(|&(start, end)| start <= end && end < frames),
        root_key,
    }))
}

/// A file to stream, and what it keeps in memory.
#[derive(Debug, Clone)]
pub(crate) struct Planned {
    pub(crate) path: PathBuf,
    pub(crate) info: FileInfo,
    /// The first frame of the head.
    pub(crate) head: u64,
    /// Stretches kept besides the head, as `start..end`.
    pub(crate) keep: Vec<(u64, u64)>,
    /// Kept whole, as when a loop is longer than the loop cache.
    pub(crate) whole: bool,
}

impl Planned {
    /// The stretches kept in memory with a head of `preload` frames, in
    /// order, apart from one another.
    pub(crate) fn layout(&self, preload: u64) -> Vec<(u64, u64)> {
        let frames = self.info.frames;
        if self.whole {
            return vec![(0, frames)];
        }
        let mut stretches = self.keep.clone();
        stretches.push((self.head, self.head.saturating_add(preload)));
        stretches.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::new();
        for (start, end) in stretches {
            let (start, end) = (start.min(frames), end.min(frames));
            match merged.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ if start < end => merged.push((start, end)),
                _ => {}
            }
        }
        merged
    }

    /// Bytes the layout of a head of `preload` frames takes.
    fn bytes(&self, preload: u64) -> usize {
        let frames: u64 = self
            .layout(preload)
            .iter()
            .map(|(start, end)| end - start)
            .sum();
        frames as usize * self.info.channels.min(2) * 4
    }

    /// Reads the stretches of its layout with a head of `preload` frames:
    /// the sample, and the file the rest is streamed from (`None` when it
    /// is all in memory).
    pub(crate) fn read(
        &self,
        preload: u64,
        compress: bool,
    ) -> Result<(SampleData, Option<StreamFile>), FormatError> {
        let layout = self.layout(preload);
        let channels = self.info.channels;
        let mut reader = AudioReader::open(&self.path)?;
        let mut parts = Vec::with_capacity(layout.len());
        let mut buffer = AudioBuffer::<f32>::new(reader.num_channels().max(1), LOAD_CHUNK);
        for &(start, end) in &layout {
            let len = (end - start) as usize;
            let mut left = Vec::with_capacity(len);
            let mut right = (channels > 1).then(|| Vec::with_capacity(len));
            reader.seek(start)?;
            while left.len() < len {
                buffer.set_num_frames((len - left.len()).min(LOAD_CHUNK));
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    return Err(FormatError::Unsupported(
                        "the file ends before the length its header gives",
                    ));
                }
                left.extend_from_slice(&buffer.channel(0)[..read]);
                if let Some(right) = &mut right {
                    right.extend_from_slice(&buffer.channel(1)[..read]);
                }
            }
            parts.push(Part::plain(start, left, right));
        }
        if compress {
            for part in &mut parts {
                part.pack();
            }
        }
        let resident = layout == [(0, self.info.frames)];
        let data = SampleData {
            parts,
            frames: self.info.frames as usize,
            stereo: channels > 1,
            rate: self.info.rate,
            file_loop: self.info.file_loop,
            root_key: self.info.root_key,
        };
        let file = (!resident).then(|| StreamFile {
            path: self.path.clone(),
            frames: self.info.frames,
        });
        Ok((data, file))
    }
}

/// The longest head, up to the preload `options` asks for, with which the
/// files of `planned` and `fixed` bytes kept whole fit the budget. On
/// failure, the bytes the shortest head would need.
pub(crate) fn fit_preload(
    planned: &[Planned],
    fixed: usize,
    options: &StreamOptions,
) -> Result<u64, usize> {
    let wanted = options.preload_frames as u64;
    let bytes = |preload: u64| -> usize {
        fixed.saturating_add(
            planned
                .iter()
                .map(|file| file.bytes(preload))
                .fold(0usize, usize::saturating_add),
        )
    };
    let Some(budget) = options.ram_budget else {
        return Ok(wanted);
    };
    if bytes(wanted) <= budget {
        return Ok(wanted);
    }
    let shortest = MIN_PRELOAD.min(wanted);
    if bytes(shortest) > budget {
        return Err(bytes(shortest));
    }
    // The bytes grow with the head: the longest that fits, by bisection.
    let (mut fits, mut too_long) = (shortest, wanted);
    while too_long - fits > 1 {
        let middle = fits + (too_long - fits) / 2;
        if bytes(middle) <= budget {
            fits = middle;
        } else {
            too_long = middle;
        }
    }
    Ok(fits)
}

/// What went wrong while opening a [`SampleStore`].
#[derive(Debug)]
#[non_exhaustive]
pub enum StoreError {
    /// A file could not be read or decoded.
    File {
        /// The file.
        path: PathBuf,
        /// What the decoder said.
        error: FormatError,
    },
    /// The heads, loops and whole samples would not fit the
    /// [`ram_budget`](StreamOptions::ram_budget), even with the shortest
    /// head.
    Budget {
        /// Bytes they would need.
        needed: usize,
        /// The budget.
        budget: usize,
    },
    /// A reader thread could not be started.
    Thread(io::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Budget { needed, budget } => write!(
                f,
                "the samples need {needed} bytes in memory, more than the budget of {budget}"
            ),
            Self::Thread(error) => write!(f, "could not start a reader thread: {error}"),
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::File { error, .. } => Some(error),
            Self::Thread(error) => Some(error),
            Self::Budget { .. } => None,
        }
    }
}

/// A sample of a [`SampleStore`], by its place in the list it was opened
/// with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SampleHandle(u32);

impl SampleHandle {
    /// Its place in the list the store was opened with.
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Audio files kept partly in memory and streamed from disk, for a player
/// that has voices of its own: each voice reads a sample through a
/// [`StreamCursor`].
///
/// Each WAV, AIFF or FLAC file keeps its head in memory (its first
/// [`preload_frames`](StreamOptions::preload_frames), shortened alike for
/// every file if a [`ram_budget`](StreamOptions::ram_budget) asks), the
/// stretches the caller asks to keep (its loops, say), and the loop of a
/// WAV file's `smpl` chunk if it is no longer than
/// [`loop_cache_frames`](StreamOptions::loop_cache_frames). The rest is
/// read by the reader threads while a cursor plays. A file in another
/// format, or a FLAC file that does not state its length, is kept whole.
/// [`Instrument::load_streaming`](crate::Instrument::load_streaming) lays
/// its samples out the same way and its voices read through the same rings
/// and threads.
///
/// ```no_run
/// use manguebyte_sampler::{CursorRead, SampleStore, StreamOptions};
///
/// // Off the audio thread.
/// let options = StreamOptions::new().ram_budget(512 << 20);
/// let store = SampleStore::open(["pipes/c4.wav", "pipes/c#4.wav"], &options)?;
/// let pipe = store.handle(0).unwrap();
///
/// // On the audio thread: never waits, never allocates.
/// let mut cursor = store.cursor(pipe, 0);
/// let (mut left, mut right) = ([0.0f32; 256], [0.0f32; 256]);
/// match cursor.read(&mut left, &mut right) {
///     CursorRead::Frames(frames) => { /* `frames` frames, fewer only at the end */ }
///     CursorRead::Underrun(frames) => { /* the disk is late after `frames` */ }
/// }
/// # Ok::<(), manguebyte_sampler::StoreError>(())
/// ```
///
/// # Threads
///
/// Opening reads and allocates. [`cursor`](Self::cursor), and everything a
/// cursor does, is real-time safe. A cursor keeps the store's memory
/// alive, so drop cursors before the store, or the last one dropped frees
/// it (dropping the store also stops the reader threads, and waits for
/// them, wherever it happens).
pub struct SampleStore {
    samples: Arc<[SampleData]>,
    pool: Option<Arc<Pool>>,
    preload: u64,
}

impl fmt::Debug for SampleStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SampleStore")
            .field("samples", &self.samples.len())
            .field("preload", &self.preload)
            .field("ram_bytes", &self.ram_bytes())
            .finish_non_exhaustive()
    }
}

impl SampleStore {
    /// Opens the files at `paths`, keeping in memory what the type
    /// describes.
    ///
    /// # Errors
    ///
    /// [`StoreError`] for the first file that cannot be read, for a budget
    /// too small, or for a thread that cannot be started.
    pub fn open<P: AsRef<Path>>(
        paths: impl IntoIterator<Item = P>,
        options: &StreamOptions,
    ) -> Result<Self, StoreError> {
        Self::open_keeping(paths.into_iter().map(|path| (path, &[][..])), options)
    }

    /// Opens the files, each with the frames it must keep in memory besides
    /// its head (its loops, with what an interpolator reads round them, so
    /// that looping never waits on the disk).
    ///
    /// # Errors
    ///
    /// As [`open`](Self::open).
    pub fn open_keeping<'a, P: AsRef<Path>>(
        files: impl IntoIterator<Item = (P, &'a [Range<u64>])>,
        options: &StreamOptions,
    ) -> Result<Self, StoreError> {
        enum Entry {
            Planned(usize),
            Whole(SampleData),
        }
        let mut entries = Vec::new();
        let mut planned = Vec::new();
        let mut fixed = 0usize;
        for (path, keep) in files {
            let path = path.as_ref().to_path_buf();
            let failed = |error: FormatError| StoreError::File {
                path: path.clone(),
                error,
            };
            match probe(&path).map_err(failed)? {
                Some(info) => {
                    let mut keep: Vec<(u64, u64)> =
                        keep.iter().map(|range| (range.start, range.end)).collect();
                    if let Some((start, end)) = info.file_loop {
                        if end - start < options.loop_cache_frames as u64 {
                            let margin = crate::stream::MARGIN;
                            keep.push((start.saturating_sub(margin), end + 1 + margin));
                        }
                    }
                    entries.push(Entry::Planned(planned.len()));
                    planned.push(Planned {
                        path,
                        info,
                        head: 0,
                        keep,
                        whole: false,
                    });
                }
                None => {
                    let file = manguebyte_formats::read_file(&path).map_err(failed)?;
                    let left = file.buffer.channel(0).to_vec();
                    let right =
                        (file.buffer.num_channels() > 1).then(|| file.buffer.channel(1).to_vec());
                    let mut data = SampleData::whole(
                        left,
                        right,
                        f64::from(file.sample_rate.max(1)),
                        None,
                        None,
                    );
                    if options.compress {
                        data.pack();
                    }
                    fixed += data.bytes();
                    entries.push(Entry::Whole(data));
                }
            }
        }
        let preload =
            fit_preload(&planned, fixed, options).map_err(|needed| StoreError::Budget {
                needed,
                budget: options.ram_budget.unwrap_or(usize::MAX),
            })?;
        let mut samples = Vec::with_capacity(entries.len());
        let mut streamed = Vec::with_capacity(entries.len());
        for entry in entries {
            match entry {
                Entry::Planned(at) => {
                    let file = &planned[at];
                    let (data, stream) =
                        file.read(preload, options.compress)
                            .map_err(|error| StoreError::File {
                                path: file.path.clone(),
                                error,
                            })?;
                    samples.push(data);
                    streamed.push(stream);
                }
                Entry::Whole(data) => {
                    samples.push(data);
                    streamed.push(None);
                }
            }
        }
        let pool = make_pool(&samples, streamed, options).map_err(StoreError::Thread)?;
        Ok(Self {
            samples: samples.into(),
            pool,
            preload,
        })
    }

    /// How many samples it holds.
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The sample at `index` in the list it was opened with.
    pub fn handle(&self, index: usize) -> Option<SampleHandle> {
        (index < self.samples.len()).then_some(SampleHandle(index as u32))
    }

    /// Frames in the whole sample.
    pub fn frames(&self, sample: SampleHandle) -> u64 {
        self.data(sample).map_or(0, |data| data.frames as u64)
    }

    /// Channels a cursor reads: 1 or 2 (a file with more gives its first
    /// two).
    pub fn channels(&self, sample: SampleHandle) -> usize {
        self.data(sample)
            .map_or(0, |data| 1 + usize::from(data.stereo))
    }

    /// The sample's own rate.
    pub fn sample_rate(&self, sample: SampleHandle) -> f64 {
        self.data(sample).map_or(0.0, |data| data.rate)
    }

    /// The first loop of a WAV file's `smpl` chunk, both ends included.
    pub fn file_loop(&self, sample: SampleHandle) -> Option<(u64, u64)> {
        self.data(sample)?.file_loop
    }

    /// Whether the whole sample is in memory.
    pub fn is_resident(&self, sample: SampleHandle) -> bool {
        self.data(sample).is_some_and(SampleData::is_resident)
    }

    /// The head each file keeps, after the budget: the preload asked for,
    /// or less.
    pub fn preload_frames(&self) -> u64 {
        self.preload
    }

    /// Bytes of audio held in memory: heads, kept stretches and whole
    /// samples. What [`ram_budget`](StreamOptions::ram_budget) bounds.
    pub fn ram_bytes(&self) -> usize {
        self.samples.iter().map(SampleData::bytes).sum()
    }

    /// Bytes of the cursors' rings, apart from [`ram_bytes`](Self::ram_bytes).
    pub fn stream_buffer_bytes(&self) -> usize {
        self.pool.as_ref().map_or(0, |pool| pool.ring_bytes())
    }

    /// Underruns the cursors have met so far. Real-time safe.
    pub fn underruns(&self) -> u64 {
        self.pool.as_ref().map_or(0, |pool| pool.underruns())
    }

    /// Reads that have failed so far.
    pub fn read_errors(&self) -> u64 {
        self.pool.as_ref().map_or(0, |pool| pool.read_errors())
    }

    /// Cursors holding a ring now.
    pub fn streaming_cursors(&self) -> usize {
        self.pool.as_ref().map_or(0, |pool| pool.streaming())
    }

    /// Waits until every cursor's ring is full or holds the rest of its
    /// file, or `timeout` passes, as
    /// [`Instrument::wait_for_streams`](crate::Instrument::wait_for_streams)
    /// does. Not for the audio thread.
    pub fn wait_for_streams(&self, timeout: Duration) -> bool {
        self.pool.as_ref().is_none_or(|pool| pool.wait(timeout))
    }

    /// A cursor on `sample` at frame `from`, which asks at once for the
    /// frames after the stretch in memory it starts in. If every one of the
    /// [`voices`](StreamOptions::voices) holds a ring, it reads only what is
    /// in memory. Real-time safe.
    pub fn cursor(&self, sample: SampleHandle, from: u64) -> StreamCursor {
        let mut cursor = StreamCursor {
            samples: Arc::clone(&self.samples),
            pool: self.pool.clone(),
            sample: sample.0,
            position: from,
            lease: Lease::NONE,
        };
        if let (Some(pool), Some(data)) = (&cursor.pool, self.data(sample)) {
            if !data.is_resident() || data.is_packed() {
                let start = stream_start(data, from);
                if let Some(lease) = Lease::claim(pool, sample.0, start) {
                    cursor.lease = lease;
                }
            }
        }
        cursor
    }

    fn data(&self, sample: SampleHandle) -> Option<&SampleData> {
        self.samples.get(sample.index())
    }
}

/// The pool for `samples`, if any is streamed from the files `streamed`
/// names.
pub(crate) fn make_pool(
    samples: &[SampleData],
    streamed: Vec<Option<StreamFile>>,
    options: &StreamOptions,
) -> io::Result<Option<Arc<Pool>>> {
    let packed = samples.iter().filter(|data| data.is_packed());
    let cache_channels = packed
        .map(|data| 1 + usize::from(data.stereo))
        .max()
        .unwrap_or(0);
    if streamed.iter().all(Option::is_none) && cache_channels == 0 {
        return Ok(None);
    }
    let stereo = samples
        .iter()
        .zip(&streamed)
        .any(|(data, file)| data.stereo && file.is_some());
    Ok(Some(Arc::new(Pool::new(
        streamed,
        stereo,
        cache_channels,
        options,
    )?)))
}

/// Where a voice reading from `at` asks the stream to start: after the
/// stretch in memory that holds `at`, or at `at`.
pub(crate) fn stream_start(sample: &SampleData, at: u64) -> u64 {
    sample.part_at(at).map_or(at, Part::end)
}

/// Frame `at` of `sample`, left and right (the left twice for a mono
/// sample), from memory or from the ring of `lease`; silence past its end.
/// `None` when it is streamed and has not arrived. Real-time safe.
#[inline]
pub(crate) fn frame(sample: &SampleData, lease: &Lease, at: u64) -> Option<(f32, f32)> {
    if let Some(part) = sample.part_at(at) {
        let index = (at - part.start) as usize;
        if part.packed.is_some() {
            let (left, right) = lease.packed_frame(part, index)?;
            return Some((left, if sample.stereo { right } else { left }));
        }
        let left = part.left.get(index).copied().unwrap_or(0.0);
        let right = match &part.right {
            Some(right) => right.get(index).copied().unwrap_or(0.0),
            None => left,
        };
        return Some((left, right));
    }
    if at >= sample.frames as u64 {
        return Some((0.0, 0.0));
    }
    let (left, right) = lease.frame(at)?;
    Some((left, if sample.stereo { right } else { left }))
}

/// What [`StreamCursor::read`] gave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorRead {
    /// This many frames, as many as asked for but at the end of the
    /// sample.
    Frames(usize),
    /// This many frames, and then one that had not arrived from disk. The
    /// cursor stands on that frame; a later read may find it.
    Underrun(usize),
}

/// One voice's reader of a sample in a [`SampleStore`]: frames in order
/// from where it stands, from memory or from its ring. Every method is
/// real-time safe: none waits, locks or allocates.
pub struct StreamCursor {
    samples: Arc<[SampleData]>,
    pool: Option<Arc<Pool>>,
    sample: u32,
    position: u64,
    lease: Lease,
}

impl fmt::Debug for StreamCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamCursor")
            .field("sample", &self.sample)
            .field("position", &self.position)
            .field("streaming", &self.lease.is_held())
            .finish_non_exhaustive()
    }
}

impl StreamCursor {
    /// The frame the next read starts at.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// Whether it holds a ring (it does not when its sample is all in
    /// memory, or every ring was taken when it was made).
    pub fn is_streaming(&self) -> bool {
        self.lease.is_held()
    }

    /// Moves to `frame`. The stream starts again after the stretch in
    /// memory that holds `frame` (or at `frame`) unless it already reads
    /// from there, so going back to the start of a loop kept in memory
    /// costs nothing.
    pub fn seek(&mut self, frame: u64) {
        self.position = frame;
        if !self.lease.is_held() {
            return;
        }
        let Some(data) = self.samples.get(self.sample as usize) else {
            return;
        };
        let keep = if data.part_at(frame).is_some() {
            // The stream must hold every frame after this stretch.
            let start = stream_start(data, frame);
            start == self.lease.start() && self.lease.consumed() <= start
        } else {
            frame >= self.lease.consumed() && frame <= self.lease.available()
        };
        if !keep {
            self.lease.restart(stream_start(data, frame));
        }
    }

    /// Reads the next frames into `left` and `right` (as many as the
    /// shorter holds; a mono sample gives both the same). See
    /// [`CursorRead`]. An underrun is counted in
    /// [`SampleStore::underruns`].
    pub fn read(&mut self, left: &mut [f32], right: &mut [f32]) -> CursorRead {
        let Some(data) = self.samples.get(self.sample as usize) else {
            return CursorRead::Frames(0);
        };
        self.lease.refresh();
        let wanted = left.len().min(right.len());
        let total = data.frames as u64;
        let mut done = 0;
        while done < wanted && self.position < total {
            // A run from one stretch in memory, copied whole.
            if let Some(part) = data
                .part_at(self.position)
                .filter(|part| part.packed.is_none())
            {
                let from = (self.position - part.start) as usize;
                let run = (wanted - done).min(part.left.len() - from);
                left[done..done + run].copy_from_slice(&part.left[from..from + run]);
                let source = part.right.as_deref().unwrap_or(&part.left);
                right[done..done + run].copy_from_slice(&source[from..from + run]);
                done += run;
                self.position += run as u64;
                continue;
            }
            match frame(data, &self.lease, self.position) {
                Some((l, r)) => {
                    left[done] = l;
                    right[done] = r;
                    done += 1;
                    self.position += 1;
                }
                None => {
                    if let Some(pool) = &self.pool {
                        pool.count_underrun();
                    }
                    self.lease.release_before(self.position);
                    return CursorRead::Underrun(done);
                }
            }
        }
        self.lease.release_before(self.position);
        if let Some(part) = data
            .part_at(self.position)
            .filter(|part| part.packed.is_some())
        {
            self.lease
                .unpack_ahead(part, (self.position - part.start) as usize);
        }
        CursorRead::Frames(done)
    }
}
