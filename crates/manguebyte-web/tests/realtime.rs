//! Proves that pushing events, draining the ring and rendering never touch
//! the heap.
//!
//! The global allocator is replaced with one that counts, as in
//! `crates/manguebyte-dsp/tests/realtime.rs`, and the test fails if
//! anything allocates, reallocates or frees while the engine runs.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use manguebyte_web::ffi;
use manguebyte_web::{
    Chain, Drum, Engine, EngineConfig, EventRecord, InstrumentKind, ParamId, event_ring, ids,
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

/// Runs `work` and returns how many heap calls it made.
fn heap_calls(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

#[test]
fn the_counter_counts() {
    assert!(heap_calls(|| drop(vec![1u8; 16])) >= 2);
}

fn events(start: u64) -> Vec<EventRecord> {
    let id = ParamId::from_name;
    let mut records = Vec::new();
    for step in 0..32u64 {
        let frame = start + step * 300;
        let drum = Drum::ALL[(step % 6) as usize];
        records.push(EventRecord::note_on(frame, 0, drum.note(), 0.9));
        records.push(EventRecord::note_on(frame + 7, 1, 40 + (step % 12) as u8, 0.8));
        records.push(EventRecord::note_off(frame + 250, 1, 40 + (step % 12) as u8));
        records.push(EventRecord::param(frame + 1, 1, id(ids::SYNTH_CUTOFF), 200.0 + step as f64 * 90.0));
        records.push(EventRecord::param(frame + 2, 1, id(ids::LOWPASS_CUTOFF), 3_000.0));
        records.push(EventRecord::param(frame + 3, 0, id(ids::REVERB_MIX), (step % 3) as f64 * 0.3));
        records.push(EventRecord::param(frame + 4, 0, id(ids::PAN), -0.5));
        records.push(EventRecord::param(frame + 5, 0, id(ids::KIT_TUNE), 2.0));
        records.push(EventRecord::param(frame + 6, 1, id(ids::SYNTH_WAVE), (step % 4) as f64));
    }
    records.push(EventRecord::all_notes_off(start + 5_000, 1));
    records.push(EventRecord::choke(start + 6_000, 0));
    records.push(EventRecord::note_on(start, 9, 60, 1.0)); // no such part
    records.push(EventRecord { kind: 200, ..EventRecord::default() }); // ignored
    records
}

#[test]
fn pushing_draining_and_rendering_never_allocate() {
    let mut engine = Engine::new(EngineConfig::new(48_000.0)).unwrap();
    engine.create_part(0, InstrumentKind::Drums).unwrap();
    engine.create_part(1, InstrumentKind::Synth).unwrap();
    engine
        .create_part_with(2, InstrumentKind::Synth, Chain::NONE)
        .unwrap();
    let (mut producer, mut consumer) = event_ring(1_024);
    let first = events(0);
    let second = events(20_000);
    let mut bytes = Vec::new();
    for record in events(40_000) {
        bytes.extend_from_slice(&record.to_bytes());
    }
    let mut left = vec![0.0f32; 1_000];
    let mut right = vec![0.0f32; 1_000];

    let calls = heap_calls(|| {
        for &record in &first {
            engine.push(record);
        }
        for &record in &second {
            producer.push(record).unwrap();
        }
        engine.drain(&mut consumer);
        engine.push_bytes(&bytes);
        // Every quantum size, and a long stretch of silence for the reverb
        // to fall asleep in, then sound to wake it.
        for quantum in [1, 7, 128, 512, 333] {
            for _ in 0..40 {
                engine.render(quantum);
            }
        }
        while engine.frame() < 120_000 {
            engine.render(128);
        }
        engine.push(EventRecord::note_on(0, 0, 38, 1.0)); // late
        engine.render_into(&mut left, &mut right);
        engine.clear_pending();
    });
    assert_eq!(calls, 0, "the engine touched the heap {calls} times");
    assert_eq!(engine.stats().dropped, 0);
    assert!(engine.stats().applied > 800);
}

#[test]
fn the_webassembly_exports_never_allocate_while_rendering() {
    let engine = ffi::mb_engine_new(48_000.0, 128, 4, 1_024, 64);
    assert!(!engine.is_null());
    // SAFETY: `engine` came from `mb_engine_new` and is freed only at the
    // end; the staging area holds 64 records, and the records written fit.
    unsafe {
        assert_eq!(ffi::mb_create_part(engine, 0, 1, 3), ffi::status::OK);
        assert_eq!(ffi::mb_create_part(engine, 1, 0, 3), ffi::status::OK);
        let staging = ffi::mb_staging_ptr(engine);
        let records = events(0);
        let calls = heap_calls(|| {
            for chunk in records.chunks(64) {
                for (index, record) in chunk.iter().enumerate() {
                    let bytes = record.to_bytes();
                    core::ptr::copy_nonoverlapping(bytes.as_ptr(), staging.add(index * 32), 32);
                }
                ffi::mb_push_staged(engine, chunk.len() as u32);
            }
            for _ in 0..200 {
                ffi::mb_render(engine, 128);
                ffi::mb_output_ptr(engine, 0);
                ffi::mb_output_ptr(engine, 1);
                ffi::mb_pending_room(engine);
                ffi::mb_frame(engine);
                ffi::mb_stat(engine, 4);
            }
        });
        assert_eq!(calls, 0, "the exports touched the heap {calls} times");
        assert!(ffi::mb_stat(engine, 4) > 200.0);
        ffi::mb_engine_free(engine);
    }
}
