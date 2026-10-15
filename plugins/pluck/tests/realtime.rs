//! Proves that Mangue Pluck's processing and reset never touch the heap.
//!
//! The global allocator counts heap calls made by this thread while it is
//! watched. It is watched for every block: dense notes, more than the
//! sixteen voices, automation of every parameter on odd frames, per-note
//! expression, hostile values (velocities included), block lengths that
//! change on every call, and resets.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use mangue_pluck::Pluck;
use manguebyte::prelude::*;

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

/// Runs `work` with this thread watched, and returns how many heap calls it
/// made.
fn heap_calls_during(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

const RATE: f64 = 48_000.0;
const MAX_BLOCK: usize = 512;
const BLOCKS: u64 = 2_000;

#[test]
fn processing_and_reset_never_touch_the_heap() {
    // The allocator works: a vector is counted.
    assert!(heap_calls_during(|| drop(vec![0_u8; 16])) >= 2);

    let mut pluck = Pluck::new();
    pluck.prepare(&ProcessSpec {
        sample_rate: RATE,
        max_block_size: MAX_BLOCK,
        num_channels: 2,
    });
    let mut buffer = AudioBuffer::<f32>::new(2, MAX_BLOCK);
    let mut events = EventBuffer::with_capacity(64);
    let transport = Transport::playing_at(120.0);
    let params =
        ["cutoff", "resonance", "decay", "release", "reverb", "level"].map(ParamId::from_name);
    let values = [f64::NAN, -1e300, 1e300, 0.0, 0.3, 5.0, 2_000.0, -6.0];
    // What a host can send as a velocity, and what a CLAP velocity of
    // 1e300 becomes once the wrapper has made it an `f32`.
    let velocities = [
        1.0,
        0.7,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::MAX,
        -1.0,
        2.0,
        1e300_f64 as f32,
    ];
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    let mut below = move |n: u64| {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D) % n
    };

    let mut heap = 0;
    let mut loudest = 0.0_f32;
    for block in 0..BLOCKS {
        let frames = 1 + below(MAX_BLOCK as u64) as usize;
        buffer.set_num_frames(frames);
        events.clear();
        for _ in 0..below(8) {
            let offset = below(frames as u64) as u32;
            let note = NoteRef::new(0, 36 + below(61) as u8);
            let kind = match below(5) {
                0 | 1 => EventKind::NoteOn {
                    note,
                    velocity: velocities[below(velocities.len() as u64) as usize],
                },
                2 => EventKind::NoteOff {
                    note,
                    velocity: velocities[below(velocities.len() as u64) as usize],
                },
                3 => EventKind::ParamValue {
                    param: params[below(6) as usize],
                    value: values[below(values.len() as u64) as usize],
                },
                _ => EventKind::NoteExpression {
                    note,
                    expression: Expression::Pressure,
                    value: values[below(values.len() as u64) as usize],
                },
            };
            let _ = events.push(Event { offset, kind });
        }
        heap += heap_calls_during(|| {
            pluck.process(&mut buffer, &ProcessContext::new(&events, &transport));
            if block % 500 == 499 {
                pluck.reset();
            }
        });
        for channel in 0..2 {
            let bad = buffer.channel(channel).iter().position(|s| !s.is_finite());
            assert_eq!(bad, None, "block {block}, channel {channel}");
        }
        loudest = loudest.max(buffer.peak());
    }
    assert_eq!(
        heap, 0,
        "the plugin used the heap {heap} times in {BLOCKS} blocks"
    );
    assert!(loudest > 0.1 && loudest.is_finite(), "{loudest}");
}
