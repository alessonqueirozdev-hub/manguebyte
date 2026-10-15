//! Proves that Mangue Duck's processing never touches the heap, while its
//! editor is opened, used and closed again and again on another thread.
//!
//! The global allocator counts heap calls made by a thread while it is
//! watched. The audio thread is watched for every block it processes; the
//! main thread meanwhile opens the editor headless, drags a knob, runs
//! frames that read the meter the audio thread stores, and closes it,
//! hundreds of times.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use mangue_duck::Duck;
use manguebyte::AudioBuffer;
use manguebyte::ParamId;
use manguebyte::dsp::event::{Event, EventBuffer, EventKind};
use manguebyte::dsp::{ProcessContext, ProcessSpec, Transport};
use manguebyte::plugin::Plugin;
use manguebyte::plugin::editor::testing::RecordingEditorHost;
use manguebyte::plugin::editor::{EditorContext, EditorSpec, ParentWindow};
use manguebyte_gfx::Point;
use manguebyte_gui::HeadlessEditor;
use manguebyte_ui::Role;

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
const BLOCK: usize = 256;
const OPENS: usize = 300;

thread_local! {
    static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

#[test]
fn processing_never_touches_the_heap_while_the_editor_opens_and_closes() {
    let mut duck = Duck::new();
    let params = duck.params();
    let meter = duck.reduction_meter();
    let spec = duck
        .editor_spec()
        .on_headless_open(|ui| OPENED.with(|slot| *slot.borrow_mut() = Some(ui)));

    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicU64::new(0));
    let audio = {
        let stop = Arc::clone(&stop);
        let blocks = Arc::clone(&blocks);
        std::thread::spawn(move || {
            duck.prepare(&ProcessSpec {
                sample_rate: RATE,
                max_block_size: BLOCK,
                num_channels: 2,
            });
            let mut buffer = AudioBuffer::new(2, BLOCK);
            let mut sidechain = AudioBuffer::new(2, BLOCK);
            let mut events = EventBuffer::with_capacity(8);
            let transport = Transport::playing_at(120.0);
            let mut heap = 0;
            let mut count = 0_u64;
            // At least as many blocks as editors opened, and on until the
            // main thread is done.
            while !stop.load(Ordering::Acquire) || count < OPENS as u64 {
                let loud = (count / 50) % 2 == 0;
                heap += heap_calls_during(|| {
                    for channel in buffer.iter_channels_mut() {
                        channel.fill(0.5);
                    }
                    for channel in sidechain.iter_channels_mut() {
                        channel.fill(if loud { 0.9 } else { 0.0 });
                    }
                    events.clear();
                    // Automation on some blocks, bypass on others.
                    let _ = events.push(Event {
                        offset: (count % BLOCK as u64) as u32,
                        kind: EventKind::ParamValue {
                            param: ParamId::from_name("depth"),
                            value: (count % 40) as f64,
                        },
                    });
                    if count % 97 == 0 {
                        let _ = events.push(Event {
                            offset: 3,
                            kind: EventKind::ParamValue {
                                param: ParamId::from_name("bypass"),
                                value: ((count / 97) % 2) as f64,
                            },
                        });
                    }
                    let context =
                        ProcessContext::new(&events, &transport).with_sidechain(&sidechain);
                    duck.process(&mut buffer, &context);
                });
                count += 1;
                blocks.store(count, Ordering::Release);
            }
            (heap, count)
        })
    };

    // The main thread: the editor, again and again.
    let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let mut seen = Vec::new();
    for open in 0..OPENS {
        let (context, link) = EditorContext::attach(host.clone());
        let parent = ParentWindow::win32(0x10).expect("not null");
        let scale = [1.0, 1.5, 2.0][open % 3];
        let editor = spec
            .open(parent, Some(scale), context)
            .expect("a headless editor always opens");
        let ui = OPENED
            .with(|slot| slot.borrow_mut().take())
            .expect("the hook ran");
        ui.tick();
        if open % 10 == 0 {
            let at = ui
                .find(Role::Slider, "Attack")
                .expect("a knob")
                .bounds
                .center();
            ui.press(at);
            ui.move_to(Point::new(at.x, at.y - (open % 7) as f32));
            ui.tick();
            ui.release(Point::new(at.x, at.y - (open % 7) as f32));
        }
        ui.tick();
        seen.push(meter.load(Ordering::Relaxed));
        drop(editor);
        drop(link);
        assert!(!ui.is_open());
    }
    stop.store(true, Ordering::Release);
    let (heap, count) = audio.join().expect("the audio thread did not panic");

    assert_eq!(
        heap, 0,
        "process used the heap {heap} times in {count} blocks"
    );
    assert!(count >= OPENS as u64, "{count} blocks");
    assert_eq!(host.well_formed(), Ok(()));
    assert_eq!(host.open_at_host(), []);
    // The meter's level stayed a reduction in decibels throughout.
    assert!(seen.iter().all(|db| (0.0..=40.0).contains(db)), "{seen:?}");
    assert!(blocks.load(Ordering::Acquire) == count);
}
