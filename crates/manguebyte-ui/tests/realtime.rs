//! Proves that the interface never makes the audio thread touch the heap,
//! and that a frame in which nothing changed does not touch it either.
//!
//! The global allocator here counts the heap calls of the threads that are
//! being watched (pattern: `crates/manguebyte-dsp/tests/realtime.rs`).
//! What the audio thread does while an interface is open is store levels
//! into atomics and read parameters; that must make no heap call at all,
//! however busy the interface is on its own thread.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;

use manguebyte_gfx::Point;
use manguebyte_ui::testing::Harness;
use manguebyte_ui::{Key, Role};

use common::{PANEL, levels, panel_view, params};

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

/// Runs `work` on this thread with its heap calls counted, and returns
/// what `work` returned and how many heap calls it made.
fn counted<R>(work: impl FnOnce() -> R) -> (R, usize) {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    let result = work();
    WATCHING.with(|watching| watching.set(false));
    (result, HEAP_CALLS.with(Cell::get))
}

#[test]
fn the_guard_itself_catches_an_allocation() {
    let (_, calls) = counted(|| drop(std::hint::black_box(vec![0u8; 64])));
    assert_eq!(calls, 2, "one allocation and one free");
}

#[test]
fn the_audio_thread_makes_no_heap_call_while_the_interface_works() {
    let params = params();
    let levels = levels();
    let stop = Arc::new(AtomicBool::new(false));
    let blocks = Arc::new(AtomicU64::new(0));

    let audio = {
        let params = Arc::clone(&params);
        let left = Arc::clone(&levels.left);
        let right = Arc::clone(&levels.right);
        let stop = Arc::clone(&stop);
        let blocks = Arc::clone(&blocks);
        thread::spawn(move || {
            counted(|| {
                let mut level = -60.0_f32;
                let mut sum = 0.0_f32;
                while !stop.load(Ordering::Acquire) {
                    // What a plugin does once per block: read its
                    // parameters, which the interface writes, and publish
                    // its levels, which the interface reads.
                    sum += params.cutoff.value() + params.drive.value() + params.mix.value();
                    sum += f32::from(u8::from(params.bypass.value()));
                    level = if level > 0.0 { -60.0 } else { level + 0.75 };
                    left.store(level, Ordering::Relaxed);
                    right.store(-level - 60.0, Ordering::Relaxed);
                    blocks.fetch_add(1, Ordering::Relaxed);
                    std::hint::spin_loop();
                }
                std::hint::black_box(sum);
            })
            .1
        })
    };

    // The interface, on this thread: frames, drags, keys, clicks.
    let mut ui = Harness::with_params(
        panel_view(&params, &levels),
        PANEL,
        2.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.frame();
    for round in 0..12 {
        let knob = ui.get(Role::Slider, ["Cutoff", "Drive", "Resonance"][round % 3]);
        let at = knob.center();
        ui.press(at);
        for step in 1..=4 {
            ui.move_to(Point::new(at.x, at.y - 6.0 * step as f32));
            ui.advance_ms(16);
        }
        ui.release(Point::new(at.x, at.y - 24.0));
        ui.key(Key::Down);
        let bypass = ui.get(Role::Switch, "Bypass");
        ui.click(bypass.center());
        ui.advance_ms(48);
    }
    let frames = ui.frames_rendered();
    stop.store(true, Ordering::Release);
    let heap_calls = audio.join().expect("the audio thread ran");

    assert!(frames > 24, "the interface drew {frames} frames");
    assert!(blocks.load(Ordering::Relaxed) > 0);
    assert_eq!(heap_calls, 0, "the audio thread touched the heap");
    assert_eq!(ui.host().well_formed(), Ok(()));
}

#[test]
fn a_frame_in_which_nothing_changed_makes_no_heap_call() {
    let params = params();
    let levels = levels();
    let mut ui = Harness::with_params(
        panel_view(&params, &levels),
        PANEL,
        2.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    // Settle: the first frames lay out, measure text and fill the caches.
    ui.advance_ms(100);
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(calls, 0, "100 steady frames made {calls} heap calls");
    // A frame in which something did change is allowed to, and does.
    levels.left.store(-10.0, Ordering::Relaxed);
    let (summary, _) = counted(|| ui.frame());
    assert_eq!(summary.painted, 1);
}

#[test]
fn a_steady_frame_while_text_is_typed_makes_no_heap_call() {
    let params = params();
    let name = manguebyte_ui::State::new(String::from("Lead"));
    let view = manguebyte_ui::column((
        manguebyte_ui::text_field("Name", name.clone()),
        manguebyte_ui::knob(manguebyte_ui::ParamHandle::new(Arc::clone(&params), |p| {
            &p.cutoff
        })),
    ));
    let mut ui = Harness::with_params(
        view,
        manguebyte_gfx::Size::new(200.0, 140.0),
        2.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.advance_ms(50);
    // A field being typed into, with a selection and a caret.
    assert!(ui.tab());
    ui.type_text(" synth");
    ui.advance_ms(50);
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(calls, 0, "100 steady frames made {calls} heap calls");
    // A value being typed into a knob.
    let cutoff = ui.get(Role::Slider, "Cutoff");
    ui.double_click(cutoff.center());
    ui.type_text("2k");
    ui.advance_ms(50);
    assert!(ui.root().wants_text());
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(calls, 0, "100 steady frames made {calls} heap calls");
}

#[test]
fn a_steady_frame_with_a_tooltip_waiting_or_shown_makes_no_heap_call() {
    use manguebyte_ui::IntoView;
    let params = params();
    let view = manguebyte_ui::knob(manguebyte_ui::ParamHandle::new(Arc::clone(&params), |p| {
        &p.cutoff
    }))
    .tooltip("Where the filter starts to cut");
    let mut ui = Harness::with_params(
        view,
        manguebyte_gfx::Size::new(120.0, 140.0),
        2.0,
        Arc::clone(&params) as Arc<dyn manguebyte_params::ParamSet>,
    );
    ui.advance_ms(50);
    let cutoff = ui.get(Role::Slider, "Cutoff");
    ui.move_to(cutoff.center());
    ui.frame();
    // Waiting: frames ask for more, and touch no memory.
    let ((), calls) = counted(|| {
        for _ in 0..20 {
            assert!(ui.frame().animating);
        }
    });
    assert_eq!(calls, 0, "20 frames waiting made {calls} heap calls");
    ui.advance(manguebyte_ui::TOOLTIP_DELAY);
    assert!(
        ui.everything()
            .iter()
            .any(|found| found.role() == Role::Tooltip)
    );
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(calls, 0, "100 frames showing made {calls} heap calls");
}

#[test]
fn a_steady_frame_with_names_that_follow_the_language_makes_no_heap_call() {
    use common::languages::{language_params, screen};

    let params = language_params();
    let language = manguebyte_ui::State::new(String::from("en"));
    let mut ui = screen(&params, &language);
    ui.advance_ms(100);
    // Every name is read on each of these frames and found unchanged.
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(calls, 0, "100 steady frames made {calls} heap calls");

    // A switch may touch the heap: names are laid out and drawn again.
    language.set(String::from("pt"));
    let (summary, _) = counted(|| ui.frame());
    assert_eq!(summary.painted, 4);
    assert_eq!(ui.access_updates().len(), 8);
    ui.advance_ms(100);
    let ((), calls) = counted(|| {
        for _ in 0..100 {
            ui.frame();
        }
    });
    assert_eq!(
        calls, 0,
        "100 frames after the switch made {calls} heap calls"
    );
}

#[test]
fn a_name_built_as_a_string_every_frame_touches_the_heap_every_frame() {
    // What the documentation of `TextSource` warns about, and what the
    // test above would catch.
    let language = manguebyte_ui::State::new(String::from("en"));
    let read = language.clone();
    let view = manguebyte_ui::Label::dynamic(move || read.with(|code| format!("Language: {code}")));
    let mut ui = Harness::new(view, manguebyte_gfx::Size::new(200.0, 30.0), 1.0);
    ui.advance_ms(50);
    let ((), calls) = counted(|| {
        for _ in 0..10 {
            ui.frame();
        }
    });
    assert!(calls >= 10, "10 frames made only {calls} heap calls");
}
