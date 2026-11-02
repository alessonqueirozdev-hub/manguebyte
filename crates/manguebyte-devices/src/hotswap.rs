//! Changing devices under a running engine, without stopping it.
//!
//! A device manager started with [`DeviceManager::start_engine`] runs its
//! callback at a fixed [`EngineFormat`]: one sample rate, one largest block
//! and channel counts, chosen once and told to
//! [`AudioCallback::prepare`](crate::AudioCallback::prepare) once. Only the
//! edge between the engine and the device adapts when the device changes:
//!
//! - A device at the engine's rate with the engine's output channels is
//!   handed the engine's output directly, with nothing copied and no
//!   latency added.
//! - A device at another rate gets the engine's output through a resampler
//!   ([`Resampler`]), whose output does not depend on how the device cuts
//!   its blocks. It adds the resampler's delay, half its kernel: 32 engine
//!   frames when the device's rate is higher, 35 from 48 kHz down to
//!   44.1 kHz (0.7 ms), and up to two frames of carry.
//! - Other channel counts are mapped: an engine with fewer channels than
//!   the device is repeated across the device's channels (mono goes to
//!   every channel, stereo to 1-2, 3-4, ...); one with more is folded, each
//!   device channel taking the mean of the engine channels that fall on it
//!   modulo the device's count (stereo to mono is the mean of left and
//!   right). The input is mapped the same way, the other way round.
//!
//! # How a device is swapped
//!
//! The engine is one object, which only one thread may run at a time. It
//! lives in a box that moves between the device threads through a
//! wait-free queue, so no audio thread allocates, locks or waits.
//!
//! **Crossfade**, when the backend can run two streams at once and the
//! output device changes: the manager opens the new stream next to the old
//! one. The old device keeps running the engine and also copies what it
//! renders, at the engine's rate, into a ring the new device reads. Once
//! the ring holds a block of each device, the new device starts playing it
//! and fades in; at its next block the old device fades out over the same
//! time. When its fade ends, the old device hands the engine itself to the
//! new one, which plays what is left in the ring and then runs the engine
//! directly. The engine never stops, never runs twice and never skips a
//! frame. During the crossfade the new device plays that ring's reserve
//! behind the old one (a block of each device), and the fades may be up to
//! one block of the old device apart, so the level can rise for that long,
//! by about 1 dB when that block is a third of the fade and up to 3 dB when
//! it is as long, but it never dips.
//!
//! **Gap**, when the two cannot run at once (an ASIO driver per process,
//! or the same device opened again for a new rate, block size or a driver
//! reset): the old device fades out, the manager stops it, opens the new
//! one and hands it the engine, which fades in. The engine waits during
//! the gap and goes on from the frame after the last one faded out, so
//! nothing is lost. What is heard is the fade-out, the time the backend
//! takes to close one stream and open the other (measured by
//! [`SwapReport::silence`]), and the fade-in.
//!
//! A stream whose device was lost is replaced the same way, with no
//! fade-out, since nothing plays it any more.
//!
//! [`DeviceManager::start_engine`]: crate::DeviceManager::start_engine

use std::f64::consts::FRAC_PI_2;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::time::Duration;

use manguebyte_core::AudioBuffer;
use manguebyte_core::spsc::{Consumer, Producer};
use manguebyte_dsp::Resampler;

use crate::CallbackInfo;
use crate::backend::OpenedFormat;
use crate::duplex::monotonic_ns;
use crate::manager::{DeviceIo, Parked, StreamFormat};
use crate::timing::frame_duration;

/// How long the manager fades from one device to the next unless told
/// otherwise ([`DeviceManager::set_crossfade`](crate::DeviceManager::set_crossfade)).
pub const DEFAULT_CROSSFADE: Duration = Duration::from_millis(30);

/// Extra input frames kept between the input resampler and the engine,
/// which absorb the frame by which the two resamplers can disagree.
const INPUT_RESERVE: usize = 8;

/// The format an engine runs at, fixed for as long as it runs: devices
/// change under it, it does not change.
///
/// ```
/// use manguebyte_devices::EngineFormat;
///
/// // Stereo out, no input, 48 kHz, blocks of up to 512 frames.
/// let format = EngineFormat::new(48_000, 512, 0, 2);
/// assert_eq!(format.output_channels, 2);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EngineFormat {
    /// Sample rate, in hertz.
    pub sample_rate: u32,
    /// The largest block the callback is handed, in frames.
    pub max_block_size: usize,
    /// Input channels the callback is handed, whatever the device has.
    pub input_channels: usize,
    /// Output channels the callback fills, whatever the device has.
    pub output_channels: usize,
}

impl EngineFormat {
    /// An engine format.
    ///
    /// # Panics
    ///
    /// Panics if `sample_rate` or `max_block_size` is zero.
    pub fn new(
        sample_rate: u32,
        max_block_size: usize,
        input_channels: usize,
        output_channels: usize,
    ) -> Self {
        assert!(sample_rate > 0, "the sample rate must be non-zero");
        assert!(max_block_size > 0, "the block size must be non-zero");
        Self {
            sample_rate,
            max_block_size,
            input_channels,
            output_channels,
        }
    }

    /// The format the callback is prepared with.
    pub(crate) fn stream_format(&self) -> StreamFormat {
        StreamFormat {
            sample_rate: f64::from(self.sample_rate),
            max_block_size: self.max_block_size,
            input_channels: self.input_channels,
            output_channels: self.output_channels,
        }
    }
}

/// How the last device change went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SwapKind {
    /// The first stream: nothing was swapped.
    Start,
    /// Both devices played at once, the new one fading in while the old
    /// one faded out.
    Crossfade,
    /// The old device faded out and was closed before the new one opened
    /// and faded in.
    Gap,
    /// The old stream had stopped by itself, its device lost; the new one
    /// faded in.
    Recovered,
}

/// What [`DeviceManager::last_swap`](crate::DeviceManager::last_swap)
/// reports about the last device change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct SwapReport {
    /// How the change was made.
    pub kind: SwapKind,
    /// How long each fade lasted.
    pub fade: Duration,
    /// How long the manager took to close the old stream and open the new
    /// one; zero for a crossfade, where neither waited for the other.
    pub reopen: Duration,
    /// How long nothing was heard: from the end of the old device's
    /// fade-out to the first block of the new device. Zero for a
    /// crossfade; `None` until the new device has run, and for a start or
    /// a recovery, where the old device's end is unknown.
    pub silence: Option<Duration>,
    /// Blocks in which the new device had nothing to play since it
    /// started: the old device fell behind during a crossfade. Zero when
    /// all went well.
    pub underruns: usize,
}

/// What an edge tells the manager, and the edge that follows it.
#[derive(Debug, Default)]
pub(crate) struct EdgeShared {
    /// The device has called once.
    started: AtomicBool,
    /// The edge has begun to play the engine (its fade-in started).
    audible: AtomicBool,
    audible_ns: AtomicU64,
    /// The edge's fade-out is over; with a successor, the engine has been
    /// handed to it.
    faded: AtomicBool,
    faded_ns: AtomicU64,
    /// The largest block the device has asked for, in device frames.
    largest_block: AtomicUsize,
    /// Blocks in which the edge had nothing to play.
    underruns: AtomicUsize,
    /// The edge has been dropped: nothing may be handed to it any more.
    dropped: AtomicBool,
}

impl EdgeShared {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub(crate) fn is_faded(&self) -> bool {
        self.faded.load(Ordering::Acquire)
    }

    pub(crate) fn underruns(&self) -> usize {
        self.underruns.load(Ordering::Relaxed)
    }

    fn audible_at(&self) -> Option<u64> {
        self.audible
            .load(Ordering::Acquire)
            .then(|| self.audible_ns.load(Ordering::Relaxed))
    }

    fn faded_at(&self) -> Option<u64> {
        self.is_faded()
            .then(|| self.faded_ns.load(Ordering::Relaxed))
    }
}

/// The last swap, as the manager keeps it until it is asked.
pub(crate) struct SwapRecord {
    pub(crate) kind: SwapKind,
    pub(crate) fade: Duration,
    pub(crate) reopen: Duration,
    pub(crate) old: Option<Arc<EdgeShared>>,
    pub(crate) new: Arc<EdgeShared>,
}

impl SwapRecord {
    pub(crate) fn report(&self) -> SwapReport {
        let silence = match self.kind {
            SwapKind::Start | SwapKind::Recovered => None,
            SwapKind::Crossfade => self.new.audible_at().map(|_| Duration::ZERO),
            SwapKind::Gap => {
                let ended = self.old.as_ref().and_then(|old| old.faded_at());
                match (ended, self.new.audible_at()) {
                    (Some(ended), Some(began)) => {
                        Some(Duration::from_nanos(began.saturating_sub(ended)))
                    }
                    _ => None,
                }
            }
        };
        SwapReport {
            kind: self.kind,
            fade: self.fade,
            reopen: self.reopen,
            silence,
            underruns: self.new.underruns(),
        }
    }
}

/// The engine: the application's callback, what it reads MIDI from, and
/// how far it has run. Moves between edges in a box.
pub(crate) struct Core {
    pub(crate) parked: Parked,
    /// Engine frames run so far.
    pub(crate) frame: u64,
}

impl Core {
    pub(crate) fn new(parked: Parked) -> Box<Self> {
        Box::new(Self { parked, frame: 0 })
    }

    /// Runs the callback on one block. Real-time safe.
    fn run(
        &mut self,
        input: &AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &CallbackInfo,
    ) {
        let frames = output.num_frames();
        let mut info = *info;
        info.frame = self.frame;
        let mut io = DeviceIo {
            input,
            output,
            info,
            #[cfg(feature = "midi")]
            midi: &mut self.parked.midi,
        };
        self.parked.callback.process(&mut io);
        self.frame += frames as u64;
    }
}

/// What the manager tells a running edge: fade out, and hand the engine on
/// to `successor` if there is one, or keep it for the manager.
pub(crate) struct Retire {
    pub(crate) fade: Duration,
    pub(crate) successor: Option<Successor>,
}

/// The edge that takes over in a crossfade, as its predecessor sees it.
pub(crate) struct Successor {
    /// Where the predecessor copies what the engine renders.
    pub(crate) ring: Option<Producer<f32>>,
    /// Where it puts the engine when its fade-out ends.
    pub(crate) handoff: Producer<Box<Core>>,
    pub(crate) shared: Arc<EdgeShared>,
}

/// The edge that hands over in a crossfade, as its successor sees it.
pub(crate) struct Incoming {
    pub(crate) ring: Consumer<f32>,
    pub(crate) handoff: Consumer<Box<Core>>,
    pub(crate) predecessor: Arc<EdgeShared>,
    /// The predecessor's device rate, to read its block size in engine
    /// frames.
    pub(crate) predecessor_rate: u32,
}

/// Everything an edge starts with.
pub(crate) struct EdgeInit {
    pub(crate) engine: EngineFormat,
    pub(crate) fade: Duration,
    /// Whether to fade in when the edge starts to play.
    pub(crate) fade_in: bool,
    pub(crate) core: Option<Box<Core>>,
    pub(crate) incoming: Option<Incoming>,
    pub(crate) shared: Arc<EdgeShared>,
    pub(crate) commands: Consumer<Retire>,
    pub(crate) returns: Sender<Box<Core>>,
    /// The resamplers, made before the device opened, used if it opened
    /// at the rate they were made for.
    pub(crate) parts: Option<EdgeParts>,
}

/// The parts of an edge that take long to make: its resamplers, whose
/// kernel tables take milliseconds to compute. A swap makes them before it
/// stops the old device, when the new device's rate is known, so that they
/// do not lengthen the gap.
pub(crate) struct EdgeParts {
    device_rate: u32,
    output: Option<RateOut>,
    input: InputPath,
}

impl EdgeParts {
    /// The parts for a device at `device_rate`. Allocates.
    pub(crate) fn new(engine: &EngineFormat, device_rate: u32) -> Self {
        let block = engine.max_block_size;
        let channels = engine.output_channels;
        let output = (device_rate != engine.sample_rate).then(|| {
            let resampler = Resampler::new(
                f64::from(engine.sample_rate),
                f64::from(device_rate),
                channels,
                block,
            );
            let most = resampler.max_output_frames(block);
            RateOut {
                ratio: resampler.ratio(),
                produced: AudioBuffer::new(channels, most),
                fifo: AudioBuffer::new(channels, block + most),
                len: 0,
                flushed: false,
                resampler,
            }
        });
        Self {
            device_rate,
            output,
            input: InputPath::new(engine, device_rate),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Playing, nothing asked.
    Normal,
    /// Retiring: waiting for the successor's device to call.
    Waiting,
    /// Retiring: copying the engine's output to the successor, which is
    /// priming.
    Teeing,
    /// Fading out, this many frames in.
    FadingOut(usize),
    /// Faded out: silent, the engine handed on or kept for the manager.
    Done,
}

/// The engine's output at the device's rate, through a resampler.
struct RateOut {
    resampler: Resampler<f32>,
    produced: AudioBuffer<f32>,
    /// Converted frames waiting for the device, `len` of them.
    fifo: AudioBuffer<f32>,
    len: usize,
    /// Device frames per engine frame.
    ratio: f64,
    /// Whether the resampler has been emptied after a fade-out.
    flushed: bool,
}

/// The device's input on its way to the engine, at the engine's rate and
/// with its channels.
struct InputPath {
    channels: usize,
    mapped: AudioBuffer<f32>,
    resampler: Option<(Resampler<f32>, AudioBuffer<f32>)>,
    fifo: AudioBuffer<f32>,
    len: usize,
}

impl InputPath {
    fn new(engine: &EngineFormat, device_rate: u32) -> Self {
        let channels = engine.input_channels;
        let block = engine.max_block_size;
        let resampler = (channels > 0 && device_rate != engine.sample_rate).then(|| {
            let resampler = Resampler::new(
                f64::from(device_rate),
                f64::from(engine.sample_rate),
                channels,
                block,
            );
            let produced = AudioBuffer::new(channels, resampler.max_output_frames(block));
            (resampler, produced)
        });
        let most = resampler
            .as_ref()
            .map_or(block, |(_, produced)| produced.capacity());
        let mut path = Self {
            channels,
            mapped: AudioBuffer::new(channels, block),
            fifo: AudioBuffer::new(channels, block + most + 2 * INPUT_RESERVE),
            resampler,
            len: 0,
        };
        path.reset();
        path
    }

    /// Starts again: empty at the same rate, a few silent frames ahead with
    /// a resampler.
    fn reset(&mut self) {
        self.len = if self.resampler.is_some() {
            INPUT_RESERVE
        } else {
            0
        };
        let len = self.len;
        for channel in self.fifo.iter_channels_mut() {
            channel[..len].fill(0.0);
        }
    }

    /// Takes `frames` of the device's input from `start`.
    fn push(&mut self, device: &AudioBuffer<f32>, start: usize, frames: usize) {
        if self.channels == 0 {
            return;
        }
        self.mapped.set_num_frames(frames);
        let source = (device.num_frames() >= start + frames).then_some(device);
        match source {
            Some(device) => map_channels(device, start, &mut self.mapped, 0, frames, None),
            None => self.mapped.clear(),
        }
        let (source, count) = match &mut self.resampler {
            Some((resampler, produced)) => {
                let count = resampler.process(&self.mapped, produced);
                (&*produced, count)
            }
            None => (&self.mapped, frames),
        };
        let capacity = self.fifo.capacity();
        if self.len + count > capacity {
            // The engine fell behind: the oldest input goes.
            let drop = (self.len + count - capacity).min(self.len);
            for channel in self.fifo.iter_channels_mut() {
                channel.copy_within(drop..self.len, 0);
            }
            self.len -= drop;
        }
        let count = count.min(capacity - self.len);
        for (to, from) in self.fifo.iter_channels_mut().zip(source.iter_channels()) {
            to[self.len..self.len + count].copy_from_slice(&from[..count]);
        }
        self.len += count;
    }

    /// Hands the engine its next `frames` of input, silence where there is
    /// none.
    fn pop(&mut self, engine_in: &mut AudioBuffer<f32>, frames: usize) {
        engine_in.set_num_frames(frames);
        if self.channels == 0 {
            return;
        }
        let take = self.len.min(frames);
        for (to, from) in engine_in
            .iter_channels_mut()
            .zip(self.fifo.iter_channels_mut())
        {
            to[..take].copy_from_slice(&from[..take]);
            to[take..].fill(0.0);
            from.copy_within(take..self.len, 0);
        }
        self.len -= take;
    }

    /// After a block at the engine's own rate, input the engine did not
    /// take (because the ring played instead) is let go, so that it adds
    /// no latency.
    fn settle(&mut self) {
        if self.resampler.is_none() {
            self.len = 0;
        }
    }
}

/// Where the edge's engine audio comes from: the engine, or the ring its
/// predecessor fills.
struct Source {
    core: Option<Box<Core>>,
    incoming: Option<Incoming>,
    primed: bool,
    engine_in: AudioBuffer<f32>,
    input: InputPath,
    /// Interleaved frames to and from the rings.
    interleaved: Vec<f32>,
    /// Where the engine's output is copied for a successor.
    tee: Option<Producer<f32>>,
    teeing: bool,
}

impl Source {
    fn ring_is_empty(&self) -> bool {
        self.incoming
            .as_ref()
            .is_none_or(|incoming| incoming.ring.is_empty())
    }

    /// Fills `frames` of `target`, at the engine's rate and with its
    /// channels: first from the ring, then from the engine, and silence if
    /// neither has anything. `info` is moved on by what the engine ran.
    fn pull(
        &mut self,
        target: &mut AudioBuffer<f32>,
        frames: usize,
        info: &mut CallbackInfo,
        shared: &EdgeShared,
    ) {
        target.set_num_frames(frames);
        let channels = target.num_channels();
        let mut done = 0;
        if let (Some(incoming), true) = (&mut self.incoming, self.primed) {
            if let Some(held) = incoming.ring.len().checked_div(channels) {
                let take = held.min(frames);
                let samples = &mut self.interleaved[..take * channels];
                let popped = incoming.ring.pop_slice(samples) / channels;
                deinterleave_into(&samples[..popped * channels], target, 0);
                done = popped;
            }
        }
        if done < frames {
            let count = frames - done;
            if let Some(core) = &mut self.core {
                self.input.pop(&mut self.engine_in, count);
                let engine_in = &self.engine_in;
                target.windowed(done..frames, |window| {
                    window.clear();
                    core.run(engine_in, window, info);
                });
                *info = info.advanced(count);
            } else {
                for channel in target.iter_channels_mut() {
                    channel[done..].fill(0.0);
                }
                if self.primed {
                    shared.underruns.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        if self.teeing && channels > 0 {
            if let Some(tee) = &mut self.tee {
                let fits = (tee.free_len() / channels).min(frames);
                let samples = &mut self.interleaved[..fits * channels];
                interleave_from(target, samples);
                tee.push_slice(samples);
            }
        }
    }
}

/// Stands between the engine and one device stream: adapts rate and
/// channels, fades, and hands the engine on.
pub(crate) struct Edge {
    engine: EngineFormat,
    device_rate: u32,
    device_in: usize,
    device_out: usize,
    source: Source,
    rate: Option<RateOut>,
    /// The engine's output at the device's rate, before its channels are
    /// mapped.
    mix: AudioBuffer<f32>,
    /// The engine's output at its own rate, on its way to the resampler.
    engine_out: AudioBuffer<f32>,
    gains: Vec<f32>,
    /// How long a fade lasts, in engine frames.
    fade_frames: usize,
    fade_in_wanted: bool,
    fade_in: Option<usize>,
    audible: bool,
    /// Whether the manager has been told the device fell silent.
    faded: bool,
    started: bool,
    phase: Phase,
    retire: Option<Retire>,
    commands: Consumer<Retire>,
    shared: Arc<EdgeShared>,
    returns: Sender<Box<Core>>,
}

/// The gain of a fade-in `position` frames into `length`: a quarter sine,
/// above zero from the first frame and one from `length` on. A fade-out
/// is the same curve backwards, so that the two always add up to one in
/// power.
fn fade_in_gain(position: usize, length: usize) -> f64 {
    if position >= length {
        1.0
    } else {
        (FRAC_PI_2 * (position + 1) as f64 / (length + 1) as f64).sin()
    }
}

impl Edge {
    /// An edge for a device running in `device`'s format. Allocates.
    pub(crate) fn new(init: EdgeInit, device: OpenedFormat) -> Self {
        let engine = init.engine;
        let block = engine.max_block_size;
        let device_rate = if device.sample_rate == 0 {
            engine.sample_rate
        } else {
            device.sample_rate
        };
        let channels = engine.output_channels;
        let parts = init
            .parts
            .filter(|parts| parts.device_rate == device_rate)
            .unwrap_or_else(|| EdgeParts::new(&engine, device_rate));
        let fade_frames =
            (init.fade.as_secs_f64() * f64::from(engine.sample_rate)).round() as usize;
        let primed = init.incoming.is_none();
        Self {
            engine,
            device_rate,
            device_in: device.input_channels,
            device_out: device.output_channels,
            source: Source {
                core: init.core,
                incoming: init.incoming,
                primed,
                engine_in: AudioBuffer::new(engine.input_channels, block),
                input: parts.input,
                interleaved: vec![0.0; block * channels.max(1)],
                tee: None,
                teeing: false,
            },
            rate: parts.output,
            mix: AudioBuffer::new(channels, block),
            engine_out: AudioBuffer::new(channels, block),
            gains: vec![1.0; block],
            fade_frames,
            fade_in_wanted: init.fade_in,
            fade_in: None,
            audible: false,
            faded: false,
            started: false,
            phase: Phase::Normal,
            retire: None,
            commands: init.commands,
            shared: init.shared,
            returns: init.returns,
        }
    }

    /// Runs one device block, on the device's thread. Real-time safe: it
    /// allocates nothing, takes no lock and makes no system call beyond
    /// reading the clock when it starts or stops playing.
    pub(crate) fn run(
        &mut self,
        input: &AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &CallbackInfo,
    ) {
        let frames = output.num_frames();
        self.begin(frames);
        let mut engine_info = CallbackInfo {
            sample_rate: f64::from(self.engine.sample_rate),
            ..*info
        };
        if frames <= self.engine.max_block_size && self.is_direct() {
            self.direct(input, output, &engine_info);
            return;
        }
        let mut start = 0;
        while start < frames {
            let len = (frames - start).min(self.engine.max_block_size);
            self.piece(input, output, &mut engine_info, start, len);
            start += len;
        }
        self.source.input.settle();
        self.note_silence();
    }

    /// Whether the engine can write straight into the device's buffer.
    fn is_direct(&self) -> bool {
        self.rate.is_none()
            && self.source.input.resampler.is_none()
            && self.device_out == self.engine.output_channels
            && matches!(self.phase, Phase::Normal | Phase::Waiting)
            && self.fade_in.is_none()
            && self.source.core.is_some()
            && self.source.ring_is_empty()
    }

    /// The plain path: same rate, same output channels, nothing to fade.
    fn direct(
        &mut self,
        input: &AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &CallbackInfo,
    ) {
        let Some(core) = &mut self.source.core else {
            return;
        };
        if self.device_in == self.engine.input_channels {
            core.run(input, output, info);
        } else {
            let frames = output.num_frames();
            let engine_in = &mut self.source.engine_in;
            engine_in.set_num_frames(frames);
            if input.num_frames() >= frames {
                map_channels(input, 0, engine_in, 0, frames, None);
            } else {
                engine_in.clear();
            }
            core.run(engine_in, output, info);
        }
    }

    /// Reads what the manager and the other edge said, before a block.
    fn begin(&mut self, frames: usize) {
        if !self.started {
            self.started = true;
            self.shared.started.store(true, Ordering::Release);
        }
        if frames > self.shared.largest_block.load(Ordering::Relaxed) {
            self.shared.largest_block.store(frames, Ordering::Relaxed);
        }
        if self.retire.is_none() {
            if let Some(mut retire) = self.commands.pop() {
                self.phase = match &mut retire.successor {
                    Some(successor) => {
                        self.source.tee = successor.ring.take();
                        Phase::Waiting
                    }
                    None => Phase::FadingOut(0),
                };
                self.fade_frames = (retire.fade.as_secs_f64() * f64::from(self.engine.sample_rate))
                    .round() as usize;
                self.retire = Some(retire);
            }
        }
        let successor = self
            .retire
            .as_ref()
            .and_then(|retire| retire.successor.as_ref());
        match (self.phase, successor) {
            (Phase::Waiting, Some(successor))
                if successor.shared.started.load(Ordering::Acquire) =>
            {
                self.source.teeing = true;
                self.phase = Phase::Teeing;
            }
            (Phase::Teeing, Some(successor))
                if successor.shared.audible.load(Ordering::Acquire) =>
            {
                self.phase = Phase::FadingOut(0);
            }
            _ => {}
        }
        if let Phase::FadingOut(position) = self.phase {
            if position >= self.fade_frames {
                // No fade at all.
                self.finish_fade_out();
            }
        }

        // The engine, handed over by the predecessor.
        if self.source.core.is_none() {
            if let Some(incoming) = &mut self.source.incoming {
                if let Some(core) = incoming.handoff.pop() {
                    self.source.core = Some(core);
                    self.source.input.reset();
                    self.source.primed = true;
                }
            }
        }
        // Enough in the ring to start playing it.
        if !self.source.primed {
            if let Some(incoming) = &mut self.source.incoming {
                let channels = self.engine.output_channels.max(1);
                let engine_rate = f64::from(self.engine.sample_rate);
                let theirs = incoming.predecessor.largest_block.load(Ordering::Relaxed) as f64
                    * engine_rate
                    / f64::from(incoming.predecessor_rate.max(1));
                let ours = frames as f64 * engine_rate / f64::from(self.device_rate);
                let reserve = (theirs + ours).ceil() as usize + 2;
                let held = incoming.ring.len() / channels;
                if held >= reserve {
                    // What piled up before this device first called is
                    // let go, so that the reserve is all that is added.
                    incoming.ring.skip((held - reserve) * channels);
                    self.source.primed = true;
                }
            }
        }
        if !self.audible
            && self.source.primed
            && (self.source.core.is_some() || self.source.incoming.is_some())
        {
            self.audible = true;
            if self.fade_in_wanted && self.fade_frames > 0 {
                self.fade_in = Some(0);
            }
            self.shared
                .audible_ns
                .store(monotonic_ns(), Ordering::Relaxed);
            self.shared.audible.store(true, Ordering::Release);
        }
    }

    /// How many of the engine's next `frames` are to be played at all:
    /// fewer when a fade-out ends in them, none once it has.
    fn playable(&self, frames: usize) -> usize {
        match self.phase {
            Phase::FadingOut(position) => frames.min(self.fade_frames.saturating_sub(position)),
            Phase::Done => 0,
            _ => frames,
        }
    }

    /// Fills the gains of the engine's next `frames`, and says whether
    /// they are all one.
    fn fill_gains(&mut self, frames: usize) -> bool {
        let fade_out = match self.phase {
            Phase::FadingOut(position) => Some(position),
            _ => None,
        };
        if self.fade_in.is_none() && fade_out.is_none() {
            return true;
        }
        let fade = self.fade_frames;
        for (frame, gain) in self.gains[..frames].iter_mut().enumerate() {
            let mut value = 1.0;
            if let Some(position) = self.fade_in {
                value *= fade_in_gain(position + frame, fade);
            }
            if let Some(position) = fade_out {
                value *= fade_in_gain(fade.saturating_sub(position + frame + 1), fade);
            }
            *gain = value as f32;
        }
        false
    }

    /// Fades the engine's next `frames`, which have just been pulled into
    /// `self.mix` (`resampled` false) or `self.engine_out` (true), and
    /// moves the fades on.
    fn fade(&mut self, frames: usize, resampled: bool) {
        if !self.fill_gains(frames) {
            let buffer = if resampled {
                &mut self.engine_out
            } else {
                &mut self.mix
            };
            for channel in buffer.iter_channels_mut() {
                for (sample, gain) in channel.iter_mut().zip(&self.gains[..frames]) {
                    *sample *= gain;
                }
            }
        }
        self.advance(frames);
    }

    /// Moves the fades on by `frames`, and ends a fade-out that is over.
    fn advance(&mut self, frames: usize) {
        if let Some(position) = self.fade_in {
            let position = position + frames;
            self.fade_in = (position < self.fade_frames).then_some(position);
        }
        if let Phase::FadingOut(position) = self.phase {
            let position = position + frames;
            if position >= self.fade_frames {
                self.finish_fade_out();
            } else {
                self.phase = Phase::FadingOut(position);
            }
        }
    }

    /// The engine's frames have faded out: a successor gets the engine.
    /// The device itself falls silent when the resampler, if any, has
    /// played out what it held ([`run`](Self::run) then says so).
    fn finish_fade_out(&mut self) {
        self.phase = Phase::Done;
        self.source.teeing = false;
        if let Some(successor) = self
            .retire
            .as_mut()
            .and_then(|retire| retire.successor.as_mut())
        {
            if let Some(core) = self.source.core.take() {
                if let Err(core) = successor.handoff.push(core) {
                    self.source.core = Some(core);
                }
            }
        }
    }

    /// Once the device plays nothing more after a fade-out, tells the
    /// manager and the successor.
    fn note_silence(&mut self) {
        let silent = self.phase == Phase::Done
            && self
                .rate
                .as_ref()
                .is_none_or(|rate| rate.flushed && rate.len == 0);
        if silent && !self.faded {
            self.faded = true;
            self.shared
                .faded_ns
                .store(monotonic_ns(), Ordering::Relaxed);
            self.shared.faded.store(true, Ordering::Release);
        }
    }

    /// One piece of a block, at most the engine's largest block long.
    ///
    /// The fades are applied to the engine's frames, before the resampler
    /// when there is one, so that it never sees the music start or stop
    /// at once, which its kernel would smear into a click.
    fn piece(
        &mut self,
        input: &AudioBuffer<f32>,
        output: &mut AudioBuffer<f32>,
        info: &mut CallbackInfo,
        start: usize,
        len: usize,
    ) {
        let mut ready = 0;
        let tail = self
            .rate
            .as_ref()
            .is_some_and(|rate| !rate.flushed || rate.len > 0);
        if self.audible && (self.phase != Phase::Done || tail) {
            if self.source.core.is_some() {
                self.source.input.push(input, start, len);
            }
            // Taken out for the time of the piece, so that the edge's own
            // methods can run meanwhile; nothing is allocated.
            let mut rate = self.rate.take();
            if let Some(rate) = &mut rate {
                let device_latency = info.output_latency;
                // The engine is heard later by what the resampler holds.
                info.output_latency = device_latency.map(|latency| {
                    latency
                        + frame_duration(
                            rate.resampler.latency_frames().round() as usize + rate.len,
                            f64::from(self.device_rate),
                        )
                });
                // Each round makes at least one frame; the bound only
                // guards against a resampler that would not.
                let mut rounds = 0;
                while rate.len < len && rounds < 2 * len + 8 {
                    rounds += 1;
                    let need = ((len - rate.len) as f64 / rate.ratio).ceil() as usize + 1;
                    let count = self.playable(need.min(self.engine.max_block_size));
                    if count > 0 {
                        self.source
                            .pull(&mut self.engine_out, count, info, &self.shared);
                        self.fade(count, true);
                    } else if !rate.flushed {
                        // The fade-out is over, but the last half kernel
                        // of it is still in the resampler: silence pushes
                        // it out, so that the device ends as it faded.
                        rate.flushed = true;
                        let held = (rate.resampler.latency_frames() / rate.ratio).ceil() as usize;
                        let held = (held + 2).min(self.engine.max_block_size);
                        self.engine_out.set_num_frames(held);
                        self.engine_out.clear();
                    } else {
                        break;
                    }
                    let made = rate.resampler.process(&self.engine_out, &mut rate.produced);
                    let made = made.min(rate.fifo.capacity() - rate.len);
                    for (to, from) in rate
                        .fifo
                        .iter_channels_mut()
                        .zip(rate.produced.iter_channels())
                    {
                        to[rate.len..rate.len + made].copy_from_slice(&from[..made]);
                    }
                    rate.len += made;
                }
                info.output_latency = device_latency;
                ready = rate.len.min(len);
                self.mix.set_num_frames(ready);
                for (to, from) in self
                    .mix
                    .iter_channels_mut()
                    .zip(rate.fifo.iter_channels_mut())
                {
                    to.copy_from_slice(&from[..ready]);
                    from.copy_within(ready..rate.len, 0);
                }
                rate.len -= ready;
            } else {
                ready = self.playable(len);
                if ready > 0 {
                    self.source.pull(&mut self.mix, ready, info, &self.shared);
                    self.fade(ready, false);
                }
            }
            self.rate = rate;
            map_channels(&self.mix, 0, output, start, ready, None);
        }
        for channel in output.iter_channels_mut() {
            channel[start + ready..start + len].fill(0.0);
        }
    }
}

impl Drop for Edge {
    fn drop(&mut self) {
        // Edges are dropped by the manager, one after another, never two
        // at once: so a successor that has not said it is gone is still
        // there to take the engine.
        if self.retire.is_none() {
            self.retire = self.commands.pop();
        }
        let core = self.source.core.take().or_else(|| {
            self.source
                .incoming
                .as_mut()
                .and_then(|incoming| incoming.handoff.pop())
        });
        if let Some(core) = core {
            let successor = self
                .retire
                .as_mut()
                .and_then(|retire| retire.successor.as_mut())
                .filter(|successor| !successor.shared.dropped.load(Ordering::Acquire));
            let left = match successor {
                Some(successor) => successor.handoff.push(core).err(),
                None => Some(core),
            };
            if let Some(core) = left {
                let _ = self.returns.send(core);
            }
        }
        self.shared.dropped.store(true, Ordering::Release);
    }
}

/// Copies `frames` of `from`, from `from_start`, into `to` at `to_start`,
/// mapping the channels: repeated when `to` has more, folded into their
/// mean when it has fewer. Multiplies by `gains` when given.
pub(crate) fn map_channels(
    from: &AudioBuffer<f32>,
    from_start: usize,
    to: &mut AudioBuffer<f32>,
    to_start: usize,
    frames: usize,
    gains: Option<&[f32]>,
) {
    let sources = from.num_channels();
    let destinations = to.num_channels();
    for index in 0..destinations {
        let out = &mut to.channel_mut(index)[to_start..to_start + frames];
        if sources == 0 {
            out.fill(0.0);
            continue;
        }
        if sources <= destinations {
            out.copy_from_slice(&from.channel(index % sources)[from_start..from_start + frames]);
        } else {
            out.copy_from_slice(&from.channel(index)[from_start..from_start + frames]);
            let mut count = 1;
            for other in (index + destinations..sources).step_by(destinations) {
                for (sample, &value) in out
                    .iter_mut()
                    .zip(&from.channel(other)[from_start..from_start + frames])
                {
                    *sample += value;
                }
                count += 1;
            }
            if count > 1 {
                let scale = 1.0 / count as f32;
                for sample in out.iter_mut() {
                    *sample *= scale;
                }
            }
        }
        if let Some(gains) = gains {
            for (sample, gain) in out.iter_mut().zip(gains) {
                *sample *= gain;
            }
        }
    }
}

/// Writes interleaved `data` into `buffer`'s channels from frame `start`.
fn deinterleave_into(data: &[f32], buffer: &mut AudioBuffer<f32>, start: usize) {
    let channels = buffer.num_channels();
    if channels == 0 || data.is_empty() {
        return;
    }
    for (index, channel) in buffer.iter_channels_mut().enumerate() {
        for (sample, &value) in channel[start..]
            .iter_mut()
            .zip(data[index..].iter().step_by(channels))
        {
            *sample = value;
        }
    }
}

/// Writes the first frames of `buffer`, as many as `out` holds, into `out`
/// interleaved.
fn interleave_from(buffer: &AudioBuffer<f32>, out: &mut [f32]) {
    let channels = buffer.num_channels();
    if out.len() < channels {
        return;
    }
    for (index, channel) in buffer.iter_channels().enumerate() {
        for (slot, &sample) in out[index..].iter_mut().step_by(channels).zip(channel) {
            *slot = sample;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(channels: &[&[f32]]) -> AudioBuffer<f32> {
        let frames = channels.first().map_or(0, |channel| channel.len());
        let mut buffer = AudioBuffer::new(channels.len(), frames);
        for (index, values) in channels.iter().enumerate() {
            buffer.channel_mut(index).copy_from_slice(values);
        }
        buffer
    }

    #[test]
    fn channels_are_repeated_or_folded() {
        let stereo = buffer(&[&[1.0, 2.0], &[3.0, 4.0]]);
        let mut quad = AudioBuffer::new(4, 2);
        map_channels(&stereo, 0, &mut quad, 0, 2, None);
        assert_eq!(quad.channel(2), &[1.0, 2.0]);
        assert_eq!(quad.channel(3), &[3.0, 4.0]);

        let mut mono = AudioBuffer::new(1, 2);
        map_channels(&stereo, 0, &mut mono, 0, 2, Some(&[1.0, 0.5]));
        assert_eq!(mono.channel(0), &[2.0, 1.5]);

        // Three into two: the first output takes the mean of the first and
        // the third, the second keeps the second alone.
        let three = buffer(&[&[1.0], &[2.0], &[5.0]]);
        let mut two = AudioBuffer::new(2, 1);
        map_channels(&three, 0, &mut two, 0, 1, None);
        assert_eq!((two.channel(0)[0], two.channel(1)[0]), (3.0, 2.0));

        let nothing = AudioBuffer::<f32>::new(0, 2);
        quad.channel_mut(0).fill(9.0);
        map_channels(&nothing, 0, &mut quad, 0, 2, None);
        assert_eq!(quad.peak(), 0.0);
    }

    #[test]
    fn the_fades_keep_the_power_and_never_reach_zero_inside() {
        let length = 100;
        for position in 0..length {
            let fade_in = fade_in_gain(position, length);
            let fade_out = fade_in_gain(length - position - 1, length);
            assert!(fade_in > 0.0 && fade_in < 1.0);
            assert!((fade_in * fade_in + fade_out * fade_out - 1.0).abs() < 1e-12);
        }
        assert_eq!(fade_in_gain(length, length), 1.0);
        assert_eq!(fade_in_gain(0, 0), 1.0);
    }
}
