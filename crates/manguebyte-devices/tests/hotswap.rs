//! Changing devices in the middle of a song, without stopping the engine.
//!
//! A callback started with `DeviceManager::start_engine` runs at one fixed
//! format, prepared once. These tests render a song through it on the
//! headless backend and change the output device under it: to another
//! device at the same rate, to one at 44.1 kHz, to one with another block
//! size and channel count, through a driver's reset request, and between
//! two devices that cannot run at once. They prove that:
//!
//! - (a) the callback is never prepared or released again;
//! - (b) what the engine renders, at its own rate, is bit for bit what it
//!   renders when nothing changes;
//! - (c) every device plays it with no gap and no discontinuity beyond the
//!   documented fades, and, where two devices play at once, the level never
//!   dips;
//! - (d) nothing on the devices' thread touches the heap, swaps included.
//!
//! The devices are simulated by one thread that calls each device's stream
//! when its own clock says it is due, in real time, as sound cards would.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::f64::consts::TAU;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use manguebyte_core::spsc::{self, Consumer, Producer};
use manguebyte_devices::{
    AudioCallback, DEFAULT_CROSSFADE, DeviceChoice, DeviceEvent, DeviceIo, DeviceManager,
    DeviceRef, DeviceSetup, Direction, EngineFormat, LossCause, MockBackend, StreamFormat,
    SwapKind,
};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is bumping a thread-local counter, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// The engine's rate.
const RATE: u32 = 48_000;
/// The reference tone on the song's right channel, which the checks on
/// the devices' side follow.
const TONE: f64 = 441.0;
const TONE_LEVEL: f64 = 0.5;

/// A song: a melody of decaying notes over filtered noise on the left, a
/// steady tone on the right. Every sample follows from the state the one
/// before left, so a frame skipped, repeated or rendered twice would change
/// everything after it.
#[derive(Clone)]
struct Song {
    frame: u64,
    note: usize,
    phase: f64,
    envelope: f64,
    noise: u32,
    filtered: f64,
    tone_phase: f64,
}

impl Song {
    fn new() -> Self {
        Self {
            frame: 0,
            note: 0,
            phase: 0.0,
            envelope: 0.0,
            noise: 1,
            filtered: 0.0,
            tone_phase: 0.0,
        }
    }

    fn next(&mut self) -> (f32, f32) {
        const NOTES: [f64; 8] = [220.0, 277.18, 329.63, 440.0, 392.0, 329.63, 293.66, 246.94];
        if self.frame % 6_000 == 0 {
            self.note = (self.note + 1) % NOTES.len();
            self.envelope = 0.4;
        }
        self.phase += TAU * NOTES[self.note] / f64::from(RATE);
        if self.phase > TAU {
            self.phase -= TAU;
        }
        self.envelope *= 0.999_85;
        self.noise = self
            .noise
            .wrapping_mul(1_664_525)
            .wrapping_add(1_013_904_223);
        let white = f64::from(self.noise >> 8) / f64::from(1u32 << 24) - 0.5;
        self.filtered += 0.05 * (white - self.filtered);
        let left = self.envelope * self.phase.sin() + 0.1 * self.filtered;
        self.tone_phase += TAU * TONE / f64::from(RATE);
        if self.tone_phase > TAU {
            self.tone_phase -= TAU;
        }
        self.frame += 1;
        (left as f32, (TONE_LEVEL * self.tone_phase.sin()) as f32)
    }

    /// The first `frames` of the song, rendered in one go with no device.
    fn reference(frames: usize) -> Vec<(f32, f32)> {
        let mut song = Self::new();
        (0..frames).map(|_| song.next()).collect()
    }
}

/// What the engine says about itself, from the audio thread.
#[derive(Default)]
struct Reports {
    prepared: AtomicUsize,
    released: AtomicUsize,
    /// Blocks whose frame position, rate or channels were not the ones
    /// the engine expected.
    wrong_blocks: AtomicUsize,
    blocks: AtomicUsize,
    smallest: AtomicUsize,
    largest: AtomicUsize,
}

/// The engine: plays the song, and keeps a copy of everything it renders.
struct Player {
    song: Song,
    next_frame: u64,
    copy: Producer<f32>,
    reports: Arc<Reports>,
}

impl AudioCallback for Player {
    fn prepare(&mut self, format: &StreamFormat) {
        assert_eq!(format.sample_rate, f64::from(RATE));
        assert_eq!(format.output_channels, 2);
        self.reports.prepared.fetch_add(1, Ordering::Relaxed);
    }

    fn process(&mut self, io: &mut DeviceIo<'_>) {
        let frames = io.num_frames();
        let right = io.info.frame == self.next_frame
            && io.info.sample_rate == f64::from(RATE)
            && io.output.num_channels() == 2;
        if !right {
            self.reports.wrong_blocks.fetch_add(1, Ordering::Relaxed);
        }
        let (left, right) = io.output.channel_pair_mut(0, 1);
        for (left, right) in left.iter_mut().zip(right.iter_mut()) {
            let (l, r) = self.song.next();
            *left = l;
            *right = r;
            self.copy.push_slice(&[l, r]);
        }
        self.next_frame = io.info.frame + frames as u64;
        self.reports.blocks.fetch_add(1, Ordering::Relaxed);
        self.reports.smallest.fetch_min(frames, Ordering::Relaxed);
        self.reports.largest.fetch_max(frames, Ordering::Relaxed);
    }

    fn release(&mut self) {
        self.reports.released.fetch_add(1, Ordering::Relaxed);
    }
}

fn player(seconds: f64) -> (Player, Consumer<f32>, Arc<Reports>) {
    let (copy, rendered) = spsc::channel((seconds * f64::from(RATE)) as usize * 2 + (1 << 14));
    let reports = Arc::new(Reports {
        smallest: AtomicUsize::new(usize::MAX),
        ..Reports::default()
    });
    let player = Player {
        song: Song::new(),
        next_frame: 0,
        copy,
        reports: Arc::clone(&reports),
    };
    (player, rendered, reports)
}

/// Everything the engine rendered, as frames.
fn rendered(mut copy: Consumer<f32>) -> Vec<(f32, f32)> {
    let mut samples = vec![0.0; copy.len()];
    let count = copy.pop_slice(&mut samples);
    samples[..count]
        .chunks_exact(2)
        .map(|frame| (frame[0], frame[1]))
        .collect()
}

/// A simulated sound card.
#[derive(Debug, Clone, Copy)]
struct Device {
    id: &'static str,
    rate: u32,
    channels: u16,
    block: usize,
}

impl Device {
    fn info(&self) -> manguebyte_devices::DeviceInfo {
        MockBackend::device(self.id, self.id, self.channels, self.rate)
    }

    fn choice(&self) -> DeviceChoice {
        DeviceChoice::Device(DeviceRef::new(self.id, self.id))
    }
}

/// What the devices played, each from the start of the room's clock, and
/// how often their thread used the heap.
struct Played {
    takes: Vec<Vec<f32>>,
    heap_calls: usize,
}

/// The devices, called by one thread in real time, each on its own clock.
struct Room {
    stop: Arc<AtomicBool>,
    /// How far, in nanoseconds, every device has played.
    clock: Arc<AtomicU64>,
    thread: Option<JoinHandle<Played>>,
}

impl Room {
    fn start(mock: &MockBackend, devices: &[Device], seconds: f64) -> Self {
        let mock = mock.clone();
        let devices = devices.to_vec();
        let stop = Arc::new(AtomicBool::new(false));
        let clock = Arc::new(AtomicU64::new(0));
        let thread = {
            let (stop, clock) = (Arc::clone(&stop), Arc::clone(&clock));
            std::thread::spawn(move || {
                let mut takes: Vec<Vec<f32>> = devices
                    .iter()
                    .map(|device| {
                        let frames = (seconds * f64::from(device.rate)) as usize + device.block;
                        Vec::with_capacity(frames * usize::from(device.channels))
                    })
                    .collect();
                let mut blocks: Vec<Vec<f32>> = devices
                    .iter()
                    .map(|device| vec![0.0; device.block * usize::from(device.channels)])
                    .collect();
                let mut frames = vec![0u64; devices.len()];
                let origin = Instant::now();
                HEAP_CALLS.with(|calls| calls.set(0));
                WATCHING.with(|watching| watching.set(true));
                while !stop.load(Ordering::Acquire) {
                    let mut next = 0;
                    let mut due = f64::MAX;
                    for (index, device) in devices.iter().enumerate() {
                        let at = frames[index] as f64 / f64::from(device.rate);
                        if at < due {
                            (next, due) = (index, at);
                        }
                    }
                    if due >= seconds {
                        break;
                    }
                    clock.store((due * 1e9) as u64, Ordering::Release);
                    let at = origin + Duration::from_secs_f64(due);
                    loop {
                        let now = Instant::now();
                        if now >= at {
                            break;
                        }
                        std::thread::sleep((at - now).min(Duration::from_millis(1)));
                    }
                    let device = &devices[next];
                    let block = &mut blocks[next];
                    if !mock.render_on(device.id, block) {
                        block.fill(0.0);
                    }
                    if takes[next].len() + block.len() <= takes[next].capacity() {
                        takes[next].extend_from_slice(block);
                    }
                    frames[next] += device.block as u64;
                }
                WATCHING.with(|watching| watching.set(false));
                clock.store(u64::MAX, Ordering::Release);
                Played {
                    takes,
                    heap_calls: HEAP_CALLS.with(Cell::get),
                }
            })
        };
        Self {
            stop,
            clock,
            thread: Some(thread),
        }
    }

    /// Waits until every device has played `seconds`, polling the manager
    /// as an application's timer would, which closes the old stream of a
    /// finished crossfade. Nothing else may happen on the way.
    fn wait(&self, manager: &mut DeviceManager, seconds: f64) {
        while self.clock.load(Ordering::Acquire) < (seconds * 1e9) as u64 {
            let events = manager.poll_devices();
            assert!(events.is_empty(), "{events:?}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// How far every device has played, in seconds of the song.
    fn now(&self) -> f64 {
        self.clock.load(Ordering::Acquire) as f64 / 1e9
    }

    fn finish(mut self) -> Played {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap()
    }
}

/// One device's recording.
struct Take<'a> {
    samples: &'a [f32],
    channels: usize,
    rate: u32,
}

impl<'a> Take<'a> {
    fn new(played: &'a Played, devices: &[Device], id: &str) -> Self {
        let index = devices.iter().position(|device| device.id == id).unwrap();
        Self {
            samples: &played.takes[index],
            channels: usize::from(devices[index].channels),
            rate: devices[index].rate,
        }
    }

    fn frames(&self) -> usize {
        self.samples.len() / self.channels
    }

    fn at(&self, frame: usize, channel: usize) -> f32 {
        self.samples[frame * self.channels + channel]
    }

    /// The channels the right of the song lands on.
    fn tone_channels(&self) -> impl Iterator<Item = usize> {
        (1..self.channels).step_by(2)
    }

    fn tone_step(&self) -> f64 {
        TAU * TONE / f64::from(self.rate)
    }

    /// The level of the tone around `frame`, relative to the song's, from
    /// two samples: for y = A sin(wn), A² sin²w = y0² + y1² - 2 y0 y1 cos w.
    fn gain(&self, frame: usize) -> f64 {
        let frame = frame.min(self.frames() - 2);
        let w = self.tone_step();
        let (y0, y1) = (
            f64::from(self.at(frame, 1)),
            f64::from(self.at(frame + 1, 1)),
        );
        let squared = (y0 * y0 + y1 * y1 - 2.0 * y0 * y1 * w.cos()) / (w.sin() * w.sin());
        squared.max(0.0).sqrt() / TONE_LEVEL
    }

    fn gain_at(&self, seconds: f64) -> f64 {
        self.gain((seconds * f64::from(self.rate)) as usize)
    }

    /// The largest amount by which the tone breaks the recurrence every
    /// sine obeys, y[n+1] = 2 cos(w) y[n] - y[n-1], anywhere in the take:
    /// in silence, through fades and across every change. A frame dropped
    /// or repeated breaks it by about A w, 0.03 here; the fades by under
    /// 1e-4.
    fn worst_step(&self) -> f64 {
        self.worst_step_after(0)
    }

    /// [`worst_step`](Self::worst_step) from frame `skip` on.
    fn worst_step_after(&self, skip: usize) -> f64 {
        let twice_cos = 2.0 * self.tone_step().cos();
        let mut worst = 0.0f64;
        for channel in self.tone_channels() {
            for frame in skip + 1..self.frames() - 1 {
                let y = |offset: usize| f64::from(self.at(frame + offset - 1, channel));
                worst = worst.max((y(2) - twice_cos * y(1) + y(0)).abs());
            }
        }
        worst
    }

    /// Frames in which anything at all plays.
    fn sounding(&self) -> Vec<usize> {
        (0..self.frames())
            .filter(|&frame| (0..self.channels).any(|channel| self.at(frame, channel) != 0.0))
            .collect()
    }

    /// The longest run of exact silence between two frames that sound, in
    /// frames, and where it starts.
    fn longest_gap(&self) -> (usize, usize) {
        let sounding = self.sounding();
        sounding
            .windows(2)
            .map(|pair| (pair[1] - pair[0] - 1, pair[0] + 1))
            .max()
            .unwrap_or((0, 0))
    }
}

/// Where a device at the engine's rate played the engine's frames: its
/// frame `first` was the engine's frame `first + offset`, and so on to
/// `last`.
#[derive(Debug, Clone, Copy)]
struct Placement {
    offset: isize,
    first: usize,
    last: usize,
    /// Frames played at a gain below one: the fades.
    faded: usize,
}

/// Checks that a device at the engine's rate played one unbroken stretch
/// of the engine's frames, every frame either exactly as rendered or, in a
/// fade at its start or its end, the same frame scaled by one gain on
/// every channel, the gain moving one way only.
fn placement(take: &Take<'_>, engine: &[(f32, f32)], fade: usize) -> Placement {
    let sounding = take.sounding();
    let (first, last) = (sounding[0], *sounding.last().unwrap());
    // A stretch in the middle is played at full level: find it among the
    // engine's frames.
    let middle = (first + last) / 2;
    let probe = 32;
    let matches = |start: usize| {
        (0..probe).all(|frame| {
            let (l, r) = engine[start + frame];
            take.at(middle + frame, 0) == l && take.at(middle + frame, 1) == r
        })
    };
    let found = (0..engine.len() - probe)
        .find(|&start| matches(start))
        .expect("the device plays the engine's frames");
    let offset = found as isize - middle as isize;

    let mut faded = 0;
    let mut gains: Vec<(usize, f64)> = Vec::new();
    for frame in first..=last {
        let (l, r) = engine[(frame as isize + offset) as usize];
        for pair in (0..take.channels).step_by(2) {
            let (dl, dr) = (take.at(frame, pair), take.at(frame, pair + 1));
            if (dl, dr) == (l, r) {
                continue;
            }
            // One gain on both channels: dl / l == dr / r.
            let cross = f64::from(dl) * f64::from(r) - f64::from(dr) * f64::from(l);
            assert!(
                cross.abs() < 1e-6,
                "frame {frame}: {dl}, {dr} is not {l}, {r} scaled"
            );
            if pair == 0 {
                faded += 1;
                if f64::from(r).abs() > 0.2 {
                    gains.push((frame, f64::from(dr) / f64::from(r)));
                }
            }
        }
    }
    // The faded frames are a fade-in at the start and a fade-out at the
    // end, each at most `fade` long.
    for &(frame, gain) in &gains {
        assert!((0.0..1.0).contains(&gain), "frame {frame}: gain {gain}");
        let from_start = frame - first;
        let from_end = last - frame;
        assert!(
            from_start < fade + 1 || from_end < fade + 1,
            "frame {frame} is faded in the middle of the stretch {first}..={last}"
        );
    }
    for pair in gains.windows(2) {
        let ((a, ga), (b, gb)) = (pair[0], pair[1]);
        if b - first < fade + 1 && a - first < fade + 1 {
            assert!(gb >= ga - 1e-6, "the fade-in falls at frame {b}");
        }
        if last - a < fade + 1 && last - b < fade + 1 {
            assert!(gb <= ga + 1e-6, "the fade-out rises at frame {b}");
        }
    }
    Placement {
        offset,
        first,
        last,
        faded,
    }
}

fn fade_frames(rate: u32) -> usize {
    (DEFAULT_CROSSFADE.as_secs_f64() * f64::from(rate)).round() as usize
}

/// Checks that, around a crossfade at `seconds` from `old` to `new`, the
/// two devices together never dip: their gains add up in power to at least
/// 0.9 of the song's level, and to no more than 1.3.
fn assert_no_dip(old: &Take<'_>, new: &Take<'_>, seconds: f64, until: f64, what: &str) {
    let mut lowest = f64::MAX;
    let mut highest = 0.0f64;
    let mut t = seconds - 0.02;
    while t < until.min(seconds + 0.25) {
        let total = old.gain_at(t).hypot(new.gain_at(t));
        lowest = lowest.min(total);
        highest = highest.max(total);
        t += 0.000_25;
    }
    assert!(
        (0.9..=1.3).contains(&lowest) && highest <= 1.3,
        "{what}: the level went from {lowest:.3} to {highest:.3}"
    );
}

const A: Device = Device {
    id: "a",
    rate: 48_000,
    channels: 2,
    block: 256,
};
const B: Device = Device {
    id: "b",
    rate: 48_000,
    channels: 2,
    block: 128,
};
const C: Device = Device {
    id: "c",
    rate: 44_100,
    channels: 2,
    block: 441,
};
const D: Device = Device {
    id: "d",
    rate: 48_000,
    channels: 4,
    block: 64,
};

/// The headline: a song rendered through the manager while the output
/// changes four times under it, in the four ways a device can change.
#[test]
#[cfg_attr(miri, ignore = "real time, and too long for Miri")]
fn the_song_goes_on_while_the_device_changes_under_it() {
    let devices = [A, B, C, D];
    let seconds = 1.5;
    let mock = MockBackend::new();
    mock.set_concurrent_streams(true);
    for device in devices {
        mock.add_device(Direction::Output, device.info());
    }
    let (player, copy, reports) = player(seconds);
    let mut manager = DeviceManager::headless(&mock);
    let format = EngineFormat::new(RATE, 512, 0, 2);
    let setup = |device: Device| DeviceSetup::default().with_output(device.choice());
    let report = manager.start_engine(&setup(A), format, player).unwrap();
    assert!(report.is_exact(), "{:?}", report.fallbacks);
    assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Start);

    let room = Room::start(&mock, &devices, seconds);

    // Each change is asked for at a time of the song, but a busy machine
    // can let this thread wake later than that: where in the song each one
    // really happened is noted, and the checks below are made from there.
    // To another device at the same rate, with a smaller block.
    room.wait(&mut manager, 0.3);
    let to_b = room.now();
    manager.apply(&setup(B)).unwrap();
    assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Crossfade);

    // To one at 44.1 kHz: the engine stays at 48 kHz, the edge resamples.
    room.wait(&mut manager, 0.6);
    assert_eq!(mock.stream_count(), 1, "the crossfade is over");
    let to_c = room.now();
    manager.apply(&setup(C)).unwrap();
    assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Crossfade);
    assert_eq!(manager.format().unwrap().sample_rate, 44_100.0);

    // Its driver asks to be reset: the same device cannot be open twice,
    // so it fades out, closes, opens again and fades in.
    room.wait(&mut manager, 0.9);
    assert_eq!(mock.stream_count(), 1);
    let reset_asked = room.now();
    assert!(mock.request_restart(Direction::Output));
    let events = manager.poll_devices();
    assert!(
        matches!(
            events.as_slice(),
            [DeviceEvent::StreamLost {
                cause: LossCause::RestartRequested,
                ..
            }]
        ),
        "{events:?}"
    );
    manager.follow(&events[0]).unwrap().unwrap();
    let reset = manager.last_swap().unwrap();
    assert_eq!(reset.kind, SwapKind::Gap);
    let reset_done = room.now();

    // To one with four channels and blocks of 64 frames.
    room.wait(&mut manager, 1.2);
    let reset = manager.last_swap().unwrap_or(reset);
    let to_d = room.now();
    manager.apply(&setup(D).with_buffer_size(64)).unwrap();
    assert_eq!(manager.last_swap().unwrap().kind, SwapKind::Crossfade);
    assert_eq!(manager.format().unwrap().output_channels, 4);

    room.wait(&mut manager, seconds - 0.05);
    let played = room.finish();
    assert_eq!(manager.last_swap().unwrap().underruns, 0);

    // (a) Prepared once, never released, every block where it belonged.
    assert_eq!(reports.prepared.load(Ordering::Relaxed), 1);
    assert_eq!(reports.released.load(Ordering::Relaxed), 0);
    assert_eq!(reports.wrong_blocks.load(Ordering::Relaxed), 0);
    let (smallest, largest) = (
        reports.smallest.load(Ordering::Relaxed),
        reports.largest.load(Ordering::Relaxed),
    );
    assert!(
        smallest < 64 && largest > 256,
        "blocks of {smallest} to {largest} frames"
    );

    // (b) The engine's own stream is the song, bit for bit.
    let engine = rendered(copy);
    assert!(
        engine == Song::reference(engine.len()),
        "the engine's output differs from the song rendered with no device"
    );

    // (d) The devices' thread never used the heap.
    assert_eq!(played.heap_calls, 0, "the devices' thread used the heap");

    // (c) On the devices: no break in the tone anywhere, ...
    let takes: Vec<Take<'_>> = devices
        .iter()
        .map(|device| Take::new(&played, &devices, device.id))
        .collect();
    for (take, device) in takes.iter().zip(&devices) {
        let step = take.worst_step();
        assert!(step < 2e-3, "{}: a step of {step:e} in the tone", device.id);
    }
    // ... the devices at the engine's rate play its frames unbroken, ...
    let (a, b, d) = (
        placement(&takes[0], &engine, fade_frames(48_000)),
        placement(&takes[1], &engine, fade_frames(48_000)),
        placement(&takes[3], &engine, fade_frames(48_000)),
    );
    assert_eq!((a.offset, a.first), (0, 0), "a starts the song");
    assert!(a.faded <= fade_frames(48_000) + 1, "a has no fade-in");
    assert!(b.faded <= 2 * fade_frames(48_000) + 2);
    // ... at full level outside the fades, halfway between changes, ...
    let end = seconds - 0.05;
    for (take, from, to) in [
        (&takes[0], 0.0, to_b),
        (&takes[1], to_b, to_c),
        (&takes[2], to_c, reset_asked),
        (&takes[2], reset_done, to_d),
        (&takes[3], to_d, end),
    ] {
        assert!(
            to - from > 0.1,
            "only {:.3} s between changes at {from:.3} and {to:.3} s",
            to - from
        );
        let at = (from + to) / 2.0;
        let gain = take.gain_at(at);
        assert!((gain - 1.0).abs() < 0.01, "a gain of {gain} at {at:.3} s");
    }
    // ... with no dip while two devices play at once, ...
    assert_no_dip(&takes[0], &takes[1], to_b, to_c, "a to b");
    assert_no_dip(&takes[1], &takes[2], to_c, reset_asked, "b to c");
    assert_no_dip(&takes[2], &takes[3], to_d, end, "c to d");
    // ... and a short gap where the device was opened again.
    let (gap, at) = takes[2].longest_gap();
    let gap_seconds = gap as f64 / 44_100.0;
    let at_seconds = at as f64 / 44_100.0;
    println!(
        "reset of c at {at_seconds:.3} s: {:.1} ms of silence on the device, {:?} by the manager's \
         clocks, closed and opened again in {:?}; d took over {} frames into the song",
        gap_seconds * 1e3,
        reset.silence,
        reset.reopen,
        d.first as isize + d.offset
    );
    assert!(
        (reset_asked - 0.01..reset_done + 0.1).contains(&at_seconds),
        "the gap is at {at_seconds} s, the reset was asked for at {reset_asked:.3} s"
    );
    assert!(gap_seconds < 0.1, "a gap of {gap_seconds} s");
    // The engine had played the song up to where the room stopped, less
    // the short gap of the reset, which it may wait through.
    let engine_seconds = engine.len() as f64 / f64::from(RATE);
    assert!(
        engine_seconds > seconds - 0.1 - gap_seconds,
        "the engine rendered {engine_seconds} s, with a gap of {gap_seconds} s; changes at          {to_b:.3}, {to_c:.3}, {reset_asked:.3}-{reset_done:.3} and {to_d:.3} s"
    );
    assert!(
        reset
            .silence
            .is_some_and(|silence| silence < Duration::from_millis(100))
    );

    // The callback comes back as it went in, and is released once.
    assert!(manager.take_callback().is_some());
    assert_eq!(reports.released.load(Ordering::Relaxed), 1);
    assert_eq!(reports.prepared.load(Ordering::Relaxed), 1);
}

/// Two devices that cannot run at once, as two ASIO drivers cannot: the
/// first fades out, closes, the second opens and fades in, and not one of
/// the engine's frames is lost on the way.
#[test]
#[cfg_attr(miri, ignore = "real time, and too long for Miri")]
fn devices_that_cannot_overlap_hand_over_after_a_short_faded_gap() {
    let devices = [A, Device { block: 192, ..B }];
    let seconds = 0.6;
    let mock = MockBackend::new();
    for device in devices {
        mock.add_device(Direction::Output, device.info());
    }
    let (player, copy, reports) = player(seconds);
    let mut manager = DeviceManager::headless(&mock);
    let format = EngineFormat::new(RATE, 512, 0, 2);
    let setup = |device: Device| DeviceSetup::default().with_output(device.choice());
    manager.start_engine(&setup(A), format, player).unwrap();
    let room = Room::start(&mock, &devices, seconds);
    room.wait(&mut manager, 0.25);
    manager.apply(&setup(devices[1])).unwrap();
    let swap = manager.last_swap().unwrap();
    assert_eq!(swap.kind, SwapKind::Gap);
    room.wait(&mut manager, seconds - 0.05);
    let played = room.finish();
    let swap = manager.last_swap().unwrap();

    assert_eq!(reports.prepared.load(Ordering::Relaxed), 1);
    assert_eq!(reports.wrong_blocks.load(Ordering::Relaxed), 0);
    assert_eq!(played.heap_calls, 0, "the devices' thread used the heap");
    let engine = rendered(copy);
    assert!(engine == Song::reference(engine.len()));

    let first = Take::new(&played, &devices, "a");
    let second = Take::new(&played, &devices, "b");
    let fade = fade_frames(48_000);
    let (a, b) = (
        placement(&first, &engine, fade),
        placement(&second, &engine, fade),
    );
    // The fade-out and the fade-in, each the documented length.
    assert!(
        (fade - 2..=fade).contains(&a.faded),
        "a fade-out of {}",
        a.faded
    );
    assert!(
        (fade - 2..=fade).contains(&b.faded),
        "a fade-in of {}",
        b.faded
    );
    // The engine waited during the gap: the second device starts on the
    // frame after the last one the first device played.
    assert_eq!(
        b.first as isize + b.offset,
        a.last as isize + a.offset + 1,
        "frames were lost or played twice between the devices"
    );
    let gap = b.first as f64 / 48_000.0 - (a.last + 1) as f64 / 48_000.0;
    println!(
        "a to b: {:.1} ms of silence between the fades, {:?} by the manager's clocks, closed and \
         opened in {:?}",
        gap * 1e3,
        swap.silence,
        swap.reopen
    );
    assert!(gap < 0.1, "a gap of {gap} s");
    assert!(
        swap.silence
            .is_some_and(|silence| silence < Duration::from_millis(100))
    );
    for take in [&first, &second] {
        assert!(take.worst_step() < 2e-3);
    }
}

/// Renders `frames` of a mock's only stream in blocks of the sizes in
/// `pattern`, over and over.
fn render_in(mock: &MockBackend, channels: usize, frames: usize, pattern: &[usize]) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * channels);
    let mut block = vec![0.0f32; pattern.iter().max().unwrap() * channels];
    for &size in pattern.iter().cycle() {
        let size = size.min(frames - out.len() / channels);
        if size == 0 {
            break;
        }
        let block = &mut block[..size * channels];
        assert!(mock.render(block));
        out.extend_from_slice(block);
    }
    out
}

#[test]
fn a_device_at_another_rate_plays_the_same_whatever_its_blocks() {
    let frames = if cfg!(miri) { 2_000 } else { 30_000 };
    let mut takes = Vec::new();
    for pattern in [&[441][..], &[64, 1, 300, 1_024, 77, 512, 3]] {
        let mock = MockBackend::new();
        mock.add_device(Direction::Output, C.info());
        let (player, copy, reports) = player(1.0);
        let mut manager = DeviceManager::headless(&mock);
        manager
            .start_engine(
                &DeviceSetup::default(),
                EngineFormat::new(RATE, 256, 0, 2),
                player,
            )
            .unwrap();
        takes.push(render_in(&mock, 2, frames, pattern));
        let engine = rendered(copy);
        assert!(engine == Song::reference(engine.len()));
        assert_eq!(reports.wrong_blocks.load(Ordering::Relaxed), 0);
    }
    assert!(
        takes[0] == takes[1],
        "the output depends on the device's blocks"
    );
    // And it is the song's tone, at 44.1 kHz, once the resampler has
    // filled.
    let take = Take {
        samples: &takes[0],
        channels: 2,
        rate: 44_100,
    };
    // The song starts at full level on its first frame, which the
    // resampler's kernel spreads over its first 70 frames.
    assert!(take.worst_step_after(100) < 1e-4);
    assert!((take.gain(frames / 2) - 1.0).abs() < 1e-3);
}

/// A crossfade done by hand on one thread, so that every step can be seen:
/// the old device runs the engine until its fade-out ends, then the new one
/// does, and the manager closes the old stream on its next poll.
#[test]
fn a_crossfade_hands_the_engine_from_one_device_thread_to_the_other() {
    let mock = MockBackend::new();
    mock.set_concurrent_streams(true);
    mock.add_device(Direction::Output, A.info());
    mock.add_device(Direction::Output, B.info());
    let (player, copy, reports) = player(1.0);
    let mut manager = DeviceManager::headless(&mock);
    manager
        .start_engine(
            &DeviceSetup::default().with_output(A.choice()),
            EngineFormat::new(RATE, 256, 0, 2),
            player,
        )
        .unwrap();
    let mut old = vec![0.0f32; 2 * 256];
    let mut new = vec![0.0f32; 2 * 128];
    for _ in 0..4 {
        assert!(mock.render_on("a", &mut old));
    }
    manager
        .apply(&DeviceSetup::default().with_output(B.choice()))
        .unwrap();
    assert_eq!(mock.stream_count(), 2);
    let blocks = if cfg!(miri) { 30 } else { 60 };
    let (mut old_after, mut new_after) = (Vec::new(), Vec::new());
    for _ in 0..blocks {
        assert!(mock.render_on("a", &mut old));
        old_after.extend_from_slice(&old);
        for _ in 0..2 {
            assert!(mock.render_on("b", &mut new));
            new_after.extend_from_slice(&new);
        }
    }
    // The old device is silent now, and goes on the next poll.
    assert!(old.iter().all(|&sample| sample == 0.0));
    assert!(new.iter().any(|&sample| sample != 0.0));
    assert!(manager.poll_devices().is_empty());
    assert_eq!(mock.stream_count(), 1);
    assert!(!mock.render_on("a", &mut old));
    assert_eq!(reports.prepared.load(Ordering::Relaxed), 1);
    assert_eq!(reports.wrong_blocks.load(Ordering::Relaxed), 0);
    let engine = rendered(copy);
    assert!(engine == Song::reference(engine.len()));
    // Each device's tone is unbroken, and the new one plays at full level.
    for (samples, rate) in [(&old_after, 48_000), (&new_after, 48_000)] {
        let take = Take {
            samples,
            channels: 2,
            rate,
        };
        assert!(take.worst_step() < 2e-3);
    }
    let take = Take {
        samples: &new_after,
        channels: 2,
        rate: 48_000,
    };
    assert!((take.gain(take.frames() - 10) - 1.0).abs() < 0.01);
}

/// A device that is unplugged plays nothing more: the manager does not wait
/// for it to fade out, and the engine carries on on the default device,
/// fading in.
#[test]
fn a_lost_device_is_replaced_without_preparing_again() {
    let mock = MockBackend::new();
    mock.add_device(Direction::Output, A.info());
    mock.add_device(Direction::Output, C.info());
    let (player, copy, reports) = player(1.0);
    let mut manager = DeviceManager::headless(&mock);
    manager
        .start_engine(
            &DeviceSetup::default(),
            EngineFormat::new(RATE, 256, 0, 2),
            player,
        )
        .unwrap();
    let mut out = vec![0.0f32; 2 * 256];
    for _ in 0..4 {
        assert!(mock.render(&mut out));
    }
    manager.poll_devices();
    assert!(mock.remove_device(Direction::Output, "a"));
    mock.set_default_device(Direction::Output, Some("c"));
    let began = Instant::now();
    let events = manager.poll_devices();
    let lost = events
        .iter()
        .find(|event| matches!(event, DeviceEvent::StreamLost { .. }))
        .unwrap();
    manager.follow(lost).unwrap().unwrap();
    assert!(
        began.elapsed() < Duration::from_millis(200),
        "it waited for a fade-out"
    );
    let swap = manager.last_swap().unwrap();
    assert_eq!(swap.kind, SwapKind::Recovered);
    assert_eq!(mock.running(Direction::Output).unwrap().0, "c");
    let mut out = vec![0.0f32; 2 * 441];
    for _ in 0..8 {
        assert!(mock.render(&mut out));
    }
    assert!(out.iter().any(|&sample| sample != 0.0));
    assert_eq!(reports.prepared.load(Ordering::Relaxed), 1);
    assert_eq!(reports.wrong_blocks.load(Ordering::Relaxed), 0);
    let engine = rendered(copy);
    assert!(engine == Song::reference(engine.len()));
}
