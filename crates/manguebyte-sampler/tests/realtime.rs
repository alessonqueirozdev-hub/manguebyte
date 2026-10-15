//! Proves that playing never touches the heap: the global allocator is
//! replaced with one that counts, and every feature runs while it watches,
//! instrument swaps included.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use common::*;
use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::{Event, EventBuffer, EventKind, Expression, NoteRef};
use manguebyte_dsp::{ProcessContext, ProcessSpec, Processor, Synth, Transport};
use manguebyte_sampler::{Instrument, Interpolation, MemorySource, Sampler, SamplerVoice};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    fn record() {
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

fn assert_no_heap_use(what: &str, work: impl FnOnce()) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    let calls = HEAP_CALLS.with(Cell::get);
    assert_eq!(calls, 0, "{what} used the heap {calls} time(s)");
}

const TEXT: &str = "
<global> ampeg_attack=0.001 ampeg_release=0.01
<group> lokey=40 hikey=70 seq_length=2
<region> sample=a.wav seq_position=1 loop_mode=loop_sustain loop_start=100 loop_end=3000 loop_crossfade=0.005
<region> sample=b.wav seq_position=2 pan=30
<group> lokey=71 hikey=90 group=5 off_by=6 polyphony=2
<region> sample=a.wav hirand=0.5 offset=10 off_mode=time off_time=0.002
<region> sample=b.wav lorand=0.5 loop_mode=one_shot
<group> key=36 group=6
<region> sample=*sine
<group> lokey=40 hikey=90
<region> sample=a.wav trigger=release rt_decay=3
<region> sample=*silence trigger=release_key end=-1 group=6
";

fn source() -> MemorySource {
    let stereo = wav_bytes(
        &[
            sine(330.0, 44_100.0, 8_000, 0.4),
            sine(331.0, 44_100.0, 8_000, 0.4),
        ],
        44_100,
    );
    MemorySource::new()
        .with("a.wav", sine_wav(440.0, 48_000, 0.1, 0.5))
        .with("b.wav", stereo)
}

/// A block's worth of every kind of event.
fn busy_events(seed: u64) -> EventBuffer {
    let mut random = Random::new(seed);
    let mut events = EventBuffer::with_capacity(64);
    for _ in 0..24 {
        let offset = random.below(256) as u32;
        let key = 36 + random.below(55) as u8;
        let note = NoteRef::new(random.below(2) as u8, key);
        let kind = match random.below(9) {
            0 | 1 => EventKind::NoteOn {
                note,
                velocity: random.unit() as f32,
            },
            2 | 3 => EventKind::NoteOff {
                note,
                velocity: 0.5,
            },
            4 => EventKind::NoteChoke { note },
            5 => EventKind::NoteExpression {
                note,
                expression: [Expression::Volume, Expression::Pan, Expression::Tuning]
                    [random.below(3)],
                value: random.unit(),
            },
            6 => controller(
                [64, 120, 121, 123][random.below(4)],
                random.below(128) as u8,
            ),
            7 => bend(random.below(16_384) as i16 - 8_192),
            _ => EventKind::NoteOn {
                note: NoteRef::with_id(0, key, random.below(4) as u32),
                velocity: 1.0,
            },
        };
        events.push(Event { offset, kind }).unwrap();
    }
    events
}

#[test]
fn the_sampler_never_allocates_while_playing_or_swapping() {
    let spec = ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 256,
        num_channels: 2,
    };
    let instrument = || Instrument::from_sfz(TEXT, &mut source()).unwrap();
    for interpolation in [Interpolation::Sinc, Interpolation::Cubic] {
        for channels in [1, 2, 3] {
            let (mut sampler, mut loader) = Sampler::<f32>::new(8);
            sampler.set_interpolation(interpolation);
            sampler.prepare(&ProcessSpec {
                num_channels: channels,
                ..spec
            });
            loader.load(instrument()).unwrap();
            let blocks: Vec<EventBuffer> = (0..40).map(busy_events).collect();
            let later = instrument();
            let mut buffer = AudioBuffer::<f32>::new(channels, 256);
            let transport = Transport::STOPPED;
            assert_no_heap_use("the sampler", || {
                for (index, events) in blocks.iter().enumerate() {
                    buffer.set_num_frames([256, 1, 77, 256][index % 4]);
                    sampler.process_with(&mut buffer, &ProcessContext::new(events, &transport));
                }
                sampler.reset();
                sampler.process(&mut buffer);
            });
            assert!(sampler.instrument().is_some());

            // A swap while notes sound: the arrival is taken, the old
            // instrument keeps its notes and then goes back to the loader.
            loader.load(later).unwrap();
            loader.load(instrument()).unwrap();
            assert_no_heap_use("swapping instruments", || {
                for (index, events) in blocks.iter().enumerate() {
                    buffer.set_num_frames([256, 1, 77, 256][index % 4]);
                    sampler.process_with(&mut buffer, &ProcessContext::new(events, &transport));
                }
            });
            // Everything the audio thread let go of is freed here.
            for events in &blocks {
                sampler.process_with(&mut buffer, &ProcessContext::new(events, &transport));
            }
            sampler.reset();
            sampler.process(&mut buffer);
            assert!(loader.collect() >= 2);
            assert_eq!(loader.in_flight(), 1);
        }
    }
}

#[test]
fn the_voice_never_allocates_inside_a_synth() {
    let instrument = Arc::new(Instrument::from_sfz(TEXT, &mut source()).unwrap());
    let mut synth = Synth::with_voices(6, || SamplerVoice::new(Arc::clone(&instrument)));
    synth.prepare(&ProcessSpec {
        sample_rate: 48_000.0,
        max_block_size: 256,
        num_channels: 2,
    });
    let blocks: Vec<EventBuffer> = (100..140).map(busy_events).collect();
    let mut buffer = AudioBuffer::<f32>::new(2, 256);
    let transport = Transport::STOPPED;
    assert_no_heap_use("the synth of sampler voices", || {
        for events in &blocks {
            synth.process_with(&mut buffer, &ProcessContext::new(events, &transport));
        }
        synth.reset();
    });
}
