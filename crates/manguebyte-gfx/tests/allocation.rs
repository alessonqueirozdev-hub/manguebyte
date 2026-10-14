//! Proves where drawing does not touch the heap.
//!
//! Nothing in this crate runs on the audio thread, so the real-time rule
//! does not bind it. But the interface records scenes on every frame, and a
//! frame that allocates is a frame that can stall, so the claims made in
//! the documentation are checked here with a counting allocator, as the
//! audio crates check theirs (see `crates/manguebyte-dsp/tests/realtime.rs`):
//!
//! - re-recording a scene into a cleared [`Scene`] and a cleared [`Path`]
//!   makes no heap call once they have reached their size, text included;
//! - a [`Renderer`] that has drawn a scene of rectangles, columns, images
//!   and text once draws it again with no heap call, whole or in part (its
//!   glyph masks are then all in its cache);
//! - the glyph cache holds no more heap than its budget, counts exactly
//!   what it holds, and gives all of it back when cleared.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use manguebyte_gfx::{
    Circle, Color, Extend, IRect, Image, Layer, LinearGradient, Path, Pixmap, Point, Rect,
    Renderer, Sampling, Scene, Stop, Stroke, TextLayout, TextStyle, Transform,
};

thread_local! {
    static WATCHING: Cell<bool> = const { Cell::new(false) };
    static HEAP_CALLS: Cell<usize> = const { Cell::new(0) };
    /// Bytes allocated less bytes freed while watching.
    static NET_BYTES: Cell<isize> = const { Cell::new(0) };
}

struct CountingAllocator;

impl CountingAllocator {
    /// Counts one heap call that changes the bytes held by `bytes`.
    fn record(bytes: isize) {
        // `try_with` because the allocator is still called while a thread's
        // locals are being torn down.
        let _ = WATCHING.try_with(|watching| {
            if watching.get() {
                let _ = HEAP_CALLS.try_with(|calls| calls.set(calls.get() + 1));
                let _ = NET_BYTES.try_with(|net| net.set(net.get() + bytes));
            }
        });
    }
}

// SAFETY: every method forwards to the system allocator unchanged; the only
// addition is updating thread-local counters, which does not allocate.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::record(layout.size() as isize);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        Self::record(-(layout.size() as isize));
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::record(new_size as isize - layout.size() as isize);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

/// Runs `work` and returns how many heap calls it made on this thread.
fn heap_calls(work: impl FnOnce()) -> usize {
    HEAP_CALLS.with(|calls| calls.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    HEAP_CALLS.with(Cell::get)
}

/// Runs `work` and returns how many bytes it left allocated on this
/// thread (negative if it freed more than it allocated).
fn net_bytes(work: impl FnOnce()) -> isize {
    NET_BYTES.with(|net| net.set(0));
    WATCHING.with(|watching| watching.set(true));
    work();
    WATCHING.with(|watching| watching.set(false));
    NET_BYTES.with(Cell::get)
}

#[test]
fn the_counter_itself_sees_an_allocation() {
    assert!(heap_calls(|| drop(std::hint::black_box(vec![0u8; 64]))) >= 1);
}

/// What a meter-and-knob widget might record on every frame.
fn record_frame(scene: &mut Scene, path: &mut Path, image: &Image, label: &TextLayout, frame: u32) {
    let level = (frame % 10) as f32 / 10.0;
    let low = [0.0f32; 64];
    let mut high = [0.0f32; 64];
    for (i, h) in high.iter_mut().enumerate() {
        *h = level * (i % 7) as f32 / 7.0;
    }
    let stops = [Stop::new(0.0, Color::BLACK), Stop::new(1.0, Color::WHITE)];

    scene.clear();
    path.clear();
    path.move_to(Point::new(0.0, 50.0));
    for i in 1..200 {
        let x = i as f32;
        path.line_to(Point::new(x, 50.0 + (x * 0.1 + level).sin() * 20.0));
    }
    scene.fill(Rect::new(0.0, 0.0, 200.0, 100.0), Color::hex(0x202020));
    scene.text(label, Point::new(4.0 + level, 80.0));
    scene.stroke(&*path, Stroke::round(1.5), Color::hex(0xFF7A00));
    scene.columns(
        Rect::new(10.0, 10.0, 64.0, 80.0),
        &low,
        &high,
        LinearGradient {
            start: Point::new(0.0, 90.0),
            end: Point::new(0.0, 10.0),
            stops: &stops,
            extend: Extend::Pad,
        },
    );
    scene.clipped(Rect::new(100.0, 0.0, 100.0, 100.0), |s| {
        s.transformed(
            Transform::rotate_about(level, Point::new(150.0, 50.0)),
            |s| {
                s.layer(Layer::with_opacity(0.8), |s| {
                    s.fill(Circle::new(Point::new(150.0, 50.0), 30.0), Color::WHITE);
                    s.image(image, Rect::new(130.0, 30.0, 40.0, 40.0), Sampling::Nearest);
                });
            },
        );
    });
}

#[test]
fn re_recording_a_scene_makes_no_heap_call() {
    let image = Image::new(Pixmap::new(4, 4).expect("small"));
    let label = TextLayout::new("Gain -6.0 dB", &TextStyle::new(11.0), None);
    let mut scene = Scene::new();
    let mut path = Path::new();
    record_frame(&mut scene, &mut path, &image, &label, 0);
    let expected = scene.dump();
    let calls = heap_calls(|| {
        for frame in 1..=30 {
            record_frame(&mut scene, &mut path, &image, &label, frame);
        }
    });
    assert_eq!(calls, 0, "re-recording made {calls} heap calls");
    // Frame 30 draws what frame 0 drew (30 is a multiple of 10).
    assert_eq!(scene.dump(), expected);
}

#[test]
fn redrawing_rectangles_columns_images_and_text_makes_no_heap_call() {
    let mut source = Pixmap::new(2, 2).expect("small");
    source.fill(Color::rgb(10, 200, 30));
    let image = Image::new(source);
    let mut scene = Scene::new();
    scene.fill(Rect::new(0.0, 0.0, 64.0, 48.0), Color::hex(0x101820));
    scene.fill(
        Rect::new(4.5, 4.25, 20.0, 9.5),
        Color::rgba(255, 255, 255, 128),
    );
    scene.columns(
        Rect::new(2.0, 20.0, 60.0, 26.0),
        &[0.0, 0.1, 0.2, 0.0, 0.3],
        &[0.5, 0.9, 0.4, 1.0, 0.6],
        Color::hex(0x40E080),
    );
    scene.image(&image, Rect::new(40.0, 2.0, 16.0, 16.0), Sampling::Nearest);
    let label = TextLayout::new(
        "Release 120 ms",
        &TextStyle::new(10.0).with_color(Color::WHITE),
        None,
    );
    scene.text(&label, Point::new(3.0, 8.0));
    let mut target = Pixmap::new(128, 96).expect("small");
    let mut renderer = Renderer::new();
    // The first frames size the renderer's buffers.
    renderer.render(&scene, &mut target, 2.0);
    renderer.render_damage(&scene, &mut target, 2.0, &[IRect::new(8, 8, 40, 30)]);
    let calls = heap_calls(|| {
        for _ in 0..10 {
            renderer.render(&scene, &mut target, 2.0);
            renderer.render_damage(&scene, &mut target, 2.0, &[IRect::new(8, 8, 40, 30)]);
        }
    });
    assert_eq!(calls, 0, "redrawing made {calls} heap calls");
    // The text was drawn from masks the cache held.
    assert!(renderer.glyph_cache().hits > 0);
}

#[test]
fn the_glyph_cache_holds_no_more_heap_than_its_budget() {
    // One full stop drawn at many scales: each scale is a size of its own
    // in device pixels, so each needs a mask of a pixel or two, the case
    // where the cache's own tables weigh most against its pixels.
    let layout = TextLayout::new(".", &TextStyle::new(12.0), None);
    let mut scene = Scene::new();
    scene.text(&layout, Point::new(1.0, 1.0));
    let mut target = Pixmap::new(40, 40).expect("small");
    let mut renderer = Renderer::new();
    // Size every other buffer of the renderer first, at the largest scale,
    // with a cache that keeps nothing.
    renderer.set_glyph_cache_limit(0);
    renderer.render(&scene, &mut target, 2.0);
    let scales: Vec<f32> = (0..3_000).map(|i| 1.0 + i as f32 / 3_000.0).collect();
    let draw_all = |renderer: &mut Renderer, target: &mut Pixmap| {
        for &scale in &scales {
            renderer.render(&scene, target, scale);
        }
    };
    assert_eq!(net_bytes(|| draw_all(&mut renderer, &mut target)), 0);

    let limit = 64 * 1024;
    renderer.set_glyph_cache_limit(limit);
    let held = net_bytes(|| draw_all(&mut renderer, &mut target));
    let stats = renderer.glyph_cache();
    // The cache was filled and had to drop masks to stay within budget.
    assert!(stats.evictions > 0 && stats.masks > 100, "{stats:?}");
    assert!(stats.bytes <= limit, "{stats:?}");
    // What it says it holds is exactly what it holds.
    assert_eq!(held, stats.bytes as isize, "{stats:?}");

    // A lower budget gives memory back at once.
    let freed = -net_bytes(|| renderer.set_glyph_cache_limit(limit / 4));
    let lower = renderer.glyph_cache();
    assert!(lower.bytes <= limit / 4, "{lower:?}");
    assert_eq!(held - freed, lower.bytes as isize);

    // Clearing gives back every byte.
    let released = -net_bytes(|| renderer.clear_glyph_cache());
    assert_eq!(released, lower.bytes as isize);
    assert_eq!(renderer.glyph_cache().bytes, 0);
}
