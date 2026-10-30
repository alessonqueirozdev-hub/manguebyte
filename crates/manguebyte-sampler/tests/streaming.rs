//! Samples streamed from disk play as they do from memory while the disk
//! keeps up, fade out without a click when it does not, stream for many
//! voices at once, stay within their memory budget, and stop their threads
//! when dropped. The files are made in `CARGO_TARGET_TMPDIR` and deleted
//! at the end of each test.

mod common;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Transport};
use manguebyte_formats::flac::{self, FlacOptions};
use manguebyte_formats::wav::{self, WavFormat};
use manguebyte_sampler::{
    CursorRead, FileSource, Instrument, LoadError, SampleStore, Sampler, Sfz, StoreError,
    StreamOptions,
};

/// A directory of its own under `CARGO_TARGET_TMPDIR`, deleted on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        // The process id keeps two runs that share a target directory
        // apart.
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join(format!("streaming-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `frames` of a decaying harmonic tone with a little noise, as a pipe
/// sounds: six harmonics at 1/k, falling by about 3 dB a second.
fn tone(frequency: f64, rate: f64, frames: usize, seed: u64) -> Vec<f32> {
    let mut random = Random::new(seed);
    (0..frames)
        .map(|n| {
            let t = n as f64 / rate;
            let mut value = 0.0;
            for k in 1..=6 {
                value += (core::f64::consts::TAU * frequency * k as f64 * t).sin() / k as f64;
            }
            let noise = (random.unit() - 0.5) * 2e-3;
            (0.25 * value * (-t / 3.0).exp() + noise) as f32
        })
        .collect()
}

fn write_wav(path: &Path, channels: &[Vec<f32>], rate: u32, format: WavFormat) {
    let mut buffer = AudioBuffer::<f32>::new(channels.len(), channels[0].len());
    for (index, channel) in channels.iter().enumerate() {
        buffer.channel_mut(index).copy_from_slice(channel);
    }
    wav::write_file(path, &buffer, rate, format).unwrap();
}

fn write_flac(path: &Path, channels: &[Vec<f32>], rate: u32) {
    let mut buffer = AudioBuffer::<f32>::new(channels.len(), channels[0].len());
    for (index, channel) in channels.iter().enumerate() {
        buffer.channel_mut(index).copy_from_slice(channel);
    }
    flac::write_file(path, &buffer, rate, &FlacOptions::new(16)).unwrap();
}

/// The set of the first test: 16-bit and 24-bit WAV, stereo FLAC, played
/// at their own pitch and others, from an offset, round a sustain loop that
/// lies past the head, and from a release trigger.
const TEXT: &str = "
<global> ampeg_release=0.05
<region> sample=a.wav key=60 pitch_keycenter=60
<region> sample=a.wav key=62 pitch_keycenter=60 offset=1000
<region> sample=b.flac key=64 pitch_keycenter=66
<region> sample=c.wav key=67 pitch_keycenter=67 loop_mode=loop_sustain loop_start=20000 loop_end=29999 loop_crossfade=0.01
<region> sample=c.wav key=69 pitch_keycenter=71 trigger=release
";

fn write_set(dir: &Path) {
    write_wav(
        &dir.join("a.wav"),
        &[tone(220.0, 44_100.0, 88_200, 1)],
        44_100,
        WavFormat::Int16,
    );
    write_flac(
        &dir.join("b.flac"),
        &[
            tone(330.0, 48_000.0, 72_000, 2),
            tone(331.0, 48_000.0, 72_000, 3),
        ],
        48_000,
    );
    write_wav(
        &dir.join("c.wav"),
        &[tone(261.0, 48_000.0, 72_000, 4)],
        48_000,
        WavFormat::Int24,
    );
}

fn events() -> Vec<(usize, EventKind)> {
    vec![
        (100, on(60, 0.9)),
        (3_000, on(62, 0.7)),
        (7_000, on(64, 1.0)),
        (9_000, on(67, 0.8)),
        (9_001, on(69, 0.8)),
        (40_000, off(62)),
        (70_000, off(60)),
        (90_000, off(69)),
        (100_000, off(67)),
        (110_000, off(64)),
    ]
}

const FRAMES: usize = 140_000;

/// Renders `events` in blocks of `block`, waiting for the disk before each
/// block when `wait` says so.
fn play(
    instrument: Instrument,
    block: usize,
    wait: bool,
    events: &[(usize, EventKind)],
) -> Vec<f32> {
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(instrument, 256);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: block,
        num_channels: 2,
    });
    let mut output = Vec::with_capacity(FRAMES * 2);
    let mut buffer = AudioBuffer::<f32>::new(2, block);
    let mut list = EventBuffer::with_capacity(events.len().max(16));
    let mut next = 0;
    let mut at = 0;
    while at < FRAMES {
        let frames = block.min(FRAMES - at);
        buffer.set_num_frames(frames);
        list.clear();
        while next < events.len() && events[next].0 < at + frames {
            let (when, kind) = events[next];
            list.push(Event {
                offset: (when - at) as u32,
                kind,
            })
            .unwrap();
            next += 1;
        }
        if wait {
            let instrument = sampler.instrument().unwrap();
            assert!(instrument.wait_for_streams(Duration::from_secs(20)));
        }
        sampler.process_with(
            &mut buffer,
            &ProcessContext::new(&list, &Transport::STOPPED),
        );
        for frame in 0..frames {
            output.push(buffer.channel(0)[frame]);
            output.push(buffer.channel(1)[frame]);
        }
        at += frames;
    }
    output
}

fn small() -> StreamOptions {
    StreamOptions::new()
        .preload_frames(4_096)
        .ring_frames(16_384)
        .voices(16)
}

#[test]
fn streamed_samples_play_as_they_do_from_memory() {
    let scratch = Scratch::new("bits");
    write_set(scratch.path());
    let sfz = Sfz::parse(TEXT).unwrap();
    let memory = Instrument::load(&sfz, &mut FileSource::new(scratch.path())).unwrap();
    let memory_bytes = memory.sample_bytes();
    let reference = play(memory, 512, false, &events());
    assert!(reference.iter().any(|&s| s.abs() > 0.1));

    // Streamed, streamed and packed, and whole in memory but packed.
    let modes = [
        (512, 1, false, false),
        (77, 2, false, false),
        (1_024, 3, false, false),
        (512, 1, true, false),
        (77, 2, true, false),
        (256, 1, true, true),
    ];
    for (block, threads, compress, whole) in modes {
        let mut options = small().reader_threads(threads).compress(compress);
        if whole {
            options = options.preload_frames(usize::MAX);
        }
        let instrument = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
        let what = format!("block {block}, {threads} threads, packed {compress}, whole {whole}");
        assert_eq!(instrument.is_streaming(), !whole, "{what}");
        // Heads of 4096 frames and the loop: far less than the files.
        let bytes = instrument.sample_bytes();
        if whole {
            assert!(
                bytes * 2 < memory_bytes,
                "{what}: {bytes} of {memory_bytes}"
            );
        } else {
            assert!(
                bytes * 3 < memory_bytes,
                "{what}: {bytes} of {memory_bytes}"
            );
        }
        let probe = instrument.clone();
        let output = play(instrument, block, true, &events());
        assert_eq!(probe.stream_underruns(), 0, "{what}");
        assert_eq!(probe.stream_read_errors(), 0);
        let same = output
            .iter()
            .zip(&reference)
            .all(|(a, b)| a.to_bits() == b.to_bits());
        assert!(same, "{what}");
    }
}

/// A sine for the slow disk: the largest step between frames of one is
/// `amplitude * 2 pi f / rate`.
fn sine_set(dir: &Path) {
    write_wav(
        &dir.join("s.wav"),
        &[sine(440.0, 48_000.0, 96_000, 0.5)],
        48_000,
        WavFormat::Float32,
    );
}

#[test]
fn a_slow_disk_counts_underruns_and_fades_without_a_click() {
    let scratch = Scratch::new("slow");
    sine_set(scratch.path());
    let text = "<region> sample=s.wav pitch_keycenter=69";
    let sfz = Sfz::parse(text).unwrap();
    let events = [(0, on(69, 1.0))];
    let reference = play(
        Instrument::load(&sfz, &mut FileSource::new(scratch.path())).unwrap(),
        256,
        false,
        &events,
    );
    // A reader that takes two seconds over each read: the head runs out
    // long before the second read arrives.
    let options = small().read_delay(Duration::from_secs(2));
    let streamed = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
    let probe = streamed.clone();
    let output = play(streamed, 256, false, &events);
    assert!(probe.stream_underruns() >= 1);

    let left: Vec<f32> = output.iter().step_by(2).copied().collect();
    let expected: Vec<f32> = reference.iter().step_by(2).copied().collect();
    // Up to the underrun, the same bits; after it, a fade to silence.
    let first_different = left
        .iter()
        .zip(&expected)
        .position(|(a, b)| a.to_bits() != b.to_bits())
        .expect("the underrun changes the output");
    assert!(first_different >= 4_096, "{first_different}");
    let silent_from = left.iter().rposition(|&s| s != 0.0).unwrap() + 1;
    let fade = silent_from - first_different;
    assert!(fade <= 290, "faded over {fade} frames");
    // The largest step between frames is no more than the sine's own.
    let bound = 0.5 * core::f64::consts::TAU * 440.0 / 48_000.0 + 1e-4;
    let largest = left
        .windows(2)
        .map(|pair| f64::from((pair[1] - pair[0]).abs()))
        .fold(0.0, f64::max);
    assert!(largest <= bound, "a step of {largest} against {bound}");

    // The drop stops the reader, cutting its two seconds short.
    let started = std::time::Instant::now();
    drop(probe);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn two_hundred_and_fifty_six_voices_stream_at_once() {
    let scratch = Scratch::new("many");
    let mut text = String::new();
    for file in 0..8 {
        let name = format!("v{file}.wav");
        write_wav(
            &scratch.path().join(&name),
            &[tone(100.0 + 37.0 * file as f64, 48_000.0, 48_000, file)],
            48_000,
            WavFormat::Int16,
        );
        // Two layers on every key, at the sample's own pitch.
        for layer in 0..2 {
            let low = file * 16;
            text.push_str(&format!(
                "<region> sample=v{}.wav lokey={low} hikey={} pitch_keytrack=0 volume=-30\n",
                (file + layer) % 8,
                low + 15
            ));
        }
    }
    let sfz = Sfz::parse(&text).unwrap();
    let events: Vec<(usize, EventKind)> = (0..128u8)
        .map(|key| (usize::from(key) * 3, on(key, 0.8)))
        .collect();
    let reference = play(
        Instrument::load(&sfz, &mut FileSource::new(scratch.path())).unwrap(),
        1_024,
        false,
        &events,
    );
    let options = small().voices(256).reader_threads(4);
    let streamed = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
    let probe = streamed.clone();
    let (mut sampler, _loader) = Sampler::<f32>::with_instrument(streamed, 256);
    sampler.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: 1_024,
        num_channels: 2,
    });
    // All 256 hold a ring once the last key is down.
    let mut list = EventBuffer::with_capacity(256);
    for &(at, kind) in &events {
        list.push(Event {
            offset: at as u32,
            kind,
        })
        .unwrap();
    }
    let mut buffer = AudioBuffer::<f32>::new(2, 1_024);
    sampler.process_with(
        &mut buffer,
        &ProcessContext::new(&list, &Transport::STOPPED),
    );
    assert_eq!(probe.streaming_voices(), 256);
    assert_eq!(sampler.active_voices(), 256);
    drop(sampler);

    let output = play(probe.clone(), 1_024, true, &events);
    assert_eq!(probe.stream_underruns(), 0);
    assert!(
        output
            .iter()
            .zip(&reference)
            .all(|(a, b)| a.to_bits() == b.to_bits())
    );
}

#[test]
fn a_store_streams_for_voices_of_its_own() {
    let scratch = Scratch::new("store");
    let frames = 50_000;
    let data = tone(200.0, 48_000.0, frames, 9);
    let right = tone(201.0, 48_000.0, frames, 10);
    write_flac(
        &scratch.path().join("p.flac"),
        &[data.clone(), right.clone()],
        48_000,
    );
    write_wav(
        &scratch.path().join("q.wav"),
        std::slice::from_ref(&data),
        48_000,
        WavFormat::Float32,
    );
    let decoded = manguebyte_formats::read_file(scratch.path().join("p.flac")).unwrap();

    let options = small();
    let store = SampleStore::open_keeping(
        [
            (
                scratch.path().join("p.flac"),
                &[30_000..31_000, 45_000..45_500][..],
            ),
            (scratch.path().join("q.wav"), &[][..]),
        ],
        &options,
    )
    .unwrap();
    assert_eq!(store.len(), 2);
    let p = store.handle(0).unwrap();
    let q = store.handle(1).unwrap();
    assert_eq!((store.frames(p), store.channels(p)), (frames as u64, 2));
    assert_eq!(store.channels(q), 1);
    assert!(!store.is_resident(p));
    // Two heads of 4096 frames, and the kept stretches.
    assert_eq!(store.ram_bytes(), (4_096 * 2 + 1_500 * 2 + 4_096) * 4);

    // Read whole, waiting for the disk: the file, bit for bit.
    let mut cursor = store.cursor(p, 0);
    assert!(cursor.is_streaming());
    let (mut left, mut right) = (vec![0.0f32; 700], vec![0.0f32; 700]);
    let mut read_left = Vec::new();
    let mut read_right = Vec::new();
    loop {
        assert!(store.wait_for_streams(Duration::from_secs(20)));
        match cursor.read(&mut left, &mut right) {
            CursorRead::Frames(0) => break,
            CursorRead::Frames(count) => {
                read_left.extend_from_slice(&left[..count]);
                read_right.extend_from_slice(&right[..count]);
            }
            CursorRead::Underrun(count) => panic!("underrun after {count}"),
        }
    }
    assert_eq!(read_left, decoded.buffer.channel(0));
    assert_eq!(read_right, decoded.buffer.channel(1));

    // A loop kept in memory: back to its start costs no new request, and
    // the frames after it keep coming.
    let mut cursor = store.cursor(p, 29_000);
    assert!(store.wait_for_streams(Duration::from_secs(20)));
    for _ in 0..3 {
        cursor.seek(30_000);
        let mut chunk = [0.0f32; 1_000];
        let mut chunk_right = [0.0f32; 1_000];
        assert_eq!(
            cursor.read(&mut chunk, &mut chunk_right),
            CursorRead::Frames(1_000)
        );
        assert_eq!(&chunk[..], &decoded.buffer.channel(0)[30_000..31_000]);
    }
    assert!(store.wait_for_streams(Duration::from_secs(20)));
    let mut chunk = [0.0f32; 4_000];
    let mut chunk_right = [0.0f32; 4_000];
    assert_eq!(
        cursor.read(&mut chunk, &mut chunk_right),
        CursorRead::Frames(4_000)
    );
    assert_eq!(&chunk[..], &decoded.buffer.channel(0)[31_000..35_000]);

    // A mono file gives both sides the same.
    let mut cursor = store.cursor(q, 0);
    let (mut left, mut right) = ([0.0f32; 64], [0.0f32; 64]);
    assert_eq!(cursor.read(&mut left, &mut right), CursorRead::Frames(64));
    assert_eq!(left, right);
    assert_eq!(&left[..], &data[..64]);
    assert_eq!(store.underruns(), 0);
}

#[test]
fn a_slow_store_reports_underruns_and_carries_on() {
    let scratch = Scratch::new("store-slow");
    let data = tone(200.0, 48_000.0, 40_000, 11);
    write_wav(
        &scratch.path().join("p.wav"),
        std::slice::from_ref(&data),
        48_000,
        WavFormat::Float32,
    );
    let options = small().read_delay(Duration::from_millis(300));
    let store = SampleStore::open([scratch.path().join("p.wav")], &options).unwrap();
    let mut cursor = store.cursor(store.handle(0).unwrap(), 0);
    let mut left = vec![0.0f32; 12_000];
    let mut right = vec![0.0f32; 12_000];
    let CursorRead::Underrun(count) = cursor.read(&mut left, &mut right) else {
        panic!("the disk cannot have kept up");
    };
    assert!(count >= 4_096);
    assert_eq!(&left[..count], &data[..count]);
    assert_eq!(cursor.position(), count as u64);
    assert!(store.underruns() >= 1);
    // Once the frames arrive, the cursor reads on from where it stopped.
    assert!(store.wait_for_streams(Duration::from_secs(20)));
    let mut more = [0.0f32; 100];
    let mut more_right = [0.0f32; 100];
    assert_eq!(
        cursor.read(&mut more, &mut more_right),
        CursorRead::Frames(100)
    );
    assert_eq!(&more[..], &data[count..count + 100]);
}

#[test]
fn a_budget_shortens_the_heads_or_refuses() {
    let scratch = Scratch::new("budget");
    let mut paths = Vec::new();
    for file in 0..10 {
        let path = scratch.path().join(format!("b{file}.wav"));
        write_wav(
            &path,
            &[tone(300.0, 48_000.0, 60_000, file)],
            48_000,
            WavFormat::Int16,
        );
        paths.push(path);
    }
    let budget = 10 * 10_000 * 4;
    let options = StreamOptions::new()
        .preload_frames(32_768)
        .ram_budget(budget);
    let store = SampleStore::open(&paths, &options).unwrap();
    assert_eq!(store.preload_frames(), 10_000);
    assert!(store.ram_bytes() <= budget);
    drop(store);

    let refused = SampleStore::open(&paths, &StreamOptions::new().ram_budget(1_000));
    assert!(matches!(
        refused,
        Err(StoreError::Budget { needed, budget: 1_000 }) if needed == 10 * 4_096 * 4
    ));
    let text = "<region> sample=b0.wav\n<region> sample=b1.wav key=61";
    let refused = Instrument::load_streaming(
        &Sfz::parse(text).unwrap(),
        scratch.path(),
        &StreamOptions::new().ram_budget(1_000),
    );
    assert!(matches!(refused, Err(LoadError::Budget { .. })));
}

thread_local! {
    static GUARD: Cell<Option<ReaderGuard>> = const { Cell::new(None) };
}

static LIVE_READERS: AtomicUsize = AtomicUsize::new(0);

/// Lives as long as the reader thread that made it.
struct ReaderGuard;

impl Drop for ReaderGuard {
    fn drop(&mut self) {
        LIVE_READERS.fetch_sub(1, Ordering::SeqCst);
    }
}

#[test]
fn the_readers_start_with_the_hook_and_stop_on_drop() {
    let scratch = Scratch::new("drop");
    sine_set(scratch.path());
    let started = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&started);
    let options = small().reader_threads(3).on_reader_start(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        LIVE_READERS.fetch_add(1, Ordering::SeqCst);
        GUARD.with(|guard| guard.set(Some(ReaderGuard)));
    });
    let sfz = Sfz::parse("<region> sample=s.wav").unwrap();
    let instrument = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
    let clone = instrument.clone();
    // A clone shares the readers; the last drop stops them.
    drop(instrument);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while started.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert_eq!(started.load(Ordering::SeqCst), 3);
    assert_eq!(LIVE_READERS.load(Ordering::SeqCst), 3);
    drop(clone);
    // Dropping waits for the threads, whose last act is to drop their
    // guards.
    assert_eq!(LIVE_READERS.load(Ordering::SeqCst), 0);
}

/// What each way of holding a set of organ pipes costs in memory, printed
/// and held to its expected bounds: 24 pipes of four seconds, each with a
/// two-second sustain loop.
#[test]
fn memory_per_mode() {
    let scratch = Scratch::new("memory");
    let mut text = String::new();
    let frames = 4 * 48_000;
    for pipe in 0..24u8 {
        let name = format!("pipe{pipe}.wav");
        let frequency = 65.4 * 2f64.powf(f64::from(pipe) / 12.0);
        write_wav(
            &scratch.path().join(&name),
            &[tone(frequency, 48_000.0, frames, u64::from(pipe))],
            48_000,
            WavFormat::Int16,
        );
        let key = 36 + pipe;
        text.push_str(&format!(
            "<region> sample={name} key={key} pitch_keycenter={key} loop_mode=loop_sustain loop_start=48000 loop_end=143999\n"
        ));
    }
    let sfz = Sfz::parse(&text).unwrap();
    let memory = Instrument::load(&sfz, &mut FileSource::new(scratch.path())).unwrap();
    let options = StreamOptions::new()
        .preload_frames(16_384)
        .ring_frames(32_768)
        .voices(32);
    let streamed = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
    let whole = memory.sample_bytes();
    let heads = streamed.sample_bytes();
    let rings = streamed.stream_buffer_bytes();
    println!("all in memory: {whole} bytes");
    println!("streamed: {heads} bytes of heads and loops, {rings} of rings");
    assert_eq!(whole, 24 * frames * 4);
    // Per pipe: the head and the loop with what is read round it.
    let kept = 16_384 + (96_000 + 2 * 256);
    assert_eq!(heads, 24 * kept * 4);
    assert_eq!(rings, 32 * 32_768 * 4);

    // The same, packed: the heads and loops of 16-bit pipes take about a
    // quarter of their floats; each voice keeps four blocks unpacked.
    let packed =
        Instrument::load_streaming(&sfz, scratch.path(), &options.clone().compress(true)).unwrap();
    let packed_heads = packed.sample_bytes();
    let packed_rings = packed.stream_buffer_bytes();
    println!(
        "streamed and packed: {packed_heads} bytes of heads and loops, {packed_rings} of rings and caches"
    );
    assert!(
        packed_heads * 7 < heads * 2,
        "{packed_heads} against {heads}"
    );
    assert_eq!(packed_rings, rings + 32 * 4 * 1_024 * 4);

    // Whole and packed, with no streaming.
    let options = StreamOptions::new()
        .preload_frames(usize::MAX)
        .voices(32)
        .compress(true);
    let resident = Instrument::load_streaming(&sfz, scratch.path(), &options).unwrap();
    assert!(!resident.is_streaming());
    let resident_bytes = resident.sample_bytes();
    let caches = resident.stream_buffer_bytes();
    println!("whole and packed: {resident_bytes} bytes, {caches} of caches");
    assert!(
        resident_bytes * 7 < whole * 2,
        "{resident_bytes} against {whole}"
    );
    assert!(caches <= 32 * (4 * 1_024 + 2) * 4);
}
