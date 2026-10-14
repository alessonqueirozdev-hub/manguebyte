//! Proves that what the audio thread does with gestures and parameter
//! values never touches the heap.
//!
//! A CLAP wrapper reads [`GestureCounters`] from `process` and `flush`
//! through a [`GestureReader`], and before each block announces and stores
//! parameter values with a `ParamWatcher`. This test replaces the global
//! allocator with one that counts, and fails if any of that allocates,
//! reallocates or frees, including while another thread is busy beginning
//! and ending gestures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use manguebyte_plugin::editor::{GestureCounters, GestureEdges, GestureReader};

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

/// Runs `work` and panics if it used the heap in any way on this thread.
fn assert_no_heap_use(what: &str, work: impl FnOnce()) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    let calls = HEAP_CALLS.with(Cell::get);
    assert_eq!(calls, 0, "{what} used the heap {calls} time(s)");
}

#[test]
fn the_guard_itself_catches_an_allocation() {
    let caught = std::panic::catch_unwind(|| {
        assert_no_heap_use("a Vec", || drop(std::hint::black_box(vec![0u8; 64])));
    });
    WATCHING.with(|watching| watching.set(false));
    assert!(caught.is_err(), "the allocation guard is not working");
}

#[test]
fn counting_and_reading_gestures_do_not_use_the_heap() {
    const PARAMS: usize = 64;
    let counters = Arc::new(GestureCounters::new(PARAMS));
    let values: Vec<AtomicU32> = (0..PARAMS).map(|_| AtomicU32::new(0)).collect();
    let mut reader = GestureReader::new(Arc::clone(&counters));
    let (mut begins, mut sent) = (0, 0_u32);
    assert_no_heap_use("counting and reading gestures", || {
        for round in 0..100 {
            for index in (round % 3..PARAMS).step_by(3) {
                counters.begin(index);
                values[index].store(round as u32, Ordering::Relaxed);
                if round % 2 == 0 {
                    counters.end(index);
                    counters.end(index); // a stray end: ignored
                }
            }
            // Out of range: ignored, and still no heap.
            counters.begin(PARAMS);
            counters.end(PARAMS);
            for index in 0..=PARAMS {
                let (edges, value) = reader.read_with(index, || {
                    values.get(index).map_or(0, |v| v.load(Ordering::Relaxed))
                });
                begins += usize::from(edges.begin);
                sent = sent.max(value);
                if !counters.is_open(index) {
                    assert!(!reader.is_open(index));
                }
            }
            for index in 0..PARAMS {
                if counters.is_open(index) {
                    counters.end(index);
                }
            }
        }
    });
    assert!(begins > 0, "the work above was not optimised away");
    assert_eq!(sent, 99);
}

/// What `process` does while a person drags controls on the main thread:
/// the audio side reads every parameter's counters, over and over, and
/// never allocates. The measured stretch lasts until it has seen gestures
/// begin and end, so the test cannot pass by reading while the other
/// thread does nothing.
#[test]
fn reading_while_another_thread_gestures_does_not_use_the_heap() {
    const PARAMS: usize = 16;
    let counters = Arc::new(GestureCounters::new(PARAMS));
    let started = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let editor = {
        let counters = Arc::clone(&counters);
        let (started, stop) = (Arc::clone(&started), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut gestures = 0_usize;
            while !stop.load(Ordering::Acquire) {
                let index = gestures % PARAMS;
                counters.begin(index);
                counters.end(index);
                gestures += 1;
                started.store(true, Ordering::Release);
            }
            gestures
        })
    };
    while !started.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }

    let mut reader = GestureReader::new(Arc::clone(&counters));
    let mut seen = GestureEdges::default();
    let mut blocks = 0_u64;
    let at_least = if cfg!(miri) { 20 } else { 20_000 };
    // Far more than any machine needs; only a writer that never runs again
    // could reach it.
    let at_most = if cfg!(miri) { 20_000 } else { 200_000_000 };
    assert_no_heap_use("reading gestures during edits", || {
        while blocks < at_least || !(seen.begin && seen.end) {
            for index in 0..PARAMS {
                let read = reader.read(index);
                seen.begin |= read.begin;
                seen.end |= read.end;
            }
            blocks += 1;
            if blocks == at_most {
                break;
            }
        }
    });
    stop.store(true, Ordering::Release);
    let gestures = editor.join().expect("the editor thread panicked");
    assert!(gestures > 0);
    assert!(
        seen.begin && seen.end,
        "no gesture was seen in {blocks} blocks while the other thread made {gestures}"
    );
}

/// What a wrapper does with parameters before each block: announce the
/// changes the plugin has not seen, then store and remember the host's.
#[test]
fn announcing_and_storing_parameter_values_does_not_use_the_heap() {
    use manguebyte_dsp::event::{Event, EventBuffer, EventKind};
    use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
    use manguebyte_plugin::ParamWatcher;

    struct Params {
        edited: FloatParam,
        automated: FloatParam,
    }
    param_set!(Params { edited, automated });
    let range = || FloatRange::linear(0.0, 1.0);
    let params = Params {
        edited: FloatParam::new("edited", "Edited", range(), 0.0),
        automated: FloatParam::new("automated", "Automated", range(), 0.0),
    };
    let automated = params.automated.param_id();
    let mut watcher = ParamWatcher::new(&params);
    let mut events = EventBuffer::with_capacity(16);
    let mut announced_in_all = 0;
    assert_no_heap_use("announcing and storing parameter values", || {
        for block in 1..=100_u8 {
            events.clear();
            params.edited.set(f32::from(block) / 100.0);
            watcher.emit_changes(&params, &mut events);
            let announced = events.len();
            announced_in_all += announced;
            let value = f64::from(block) / 200.0;
            let kind = EventKind::ParamValue {
                param: automated,
                value,
            };
            assert!(events.push(Event { offset: 3, kind }).is_ok());
            watcher.apply_and_remember(&params, &events.as_slice()[announced..]);
        }
    });
    // Each block announced the edit and nothing else: the host's values
    // were remembered as they were stored.
    assert_eq!(announced_in_all, 100);
    assert_eq!(params.automated.value(), 0.5);
    assert_eq!(params.edited.value(), 1.0);
}
