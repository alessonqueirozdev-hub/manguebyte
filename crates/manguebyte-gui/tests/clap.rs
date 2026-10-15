//! A headless editor driven end to end through the real CLAP interface
//! (ADR 0007, phase 1, item 7).
//!
//! The test plugin has a knob for its gain and a meter for the level it
//! outputs. It is exported with `manguebyte-clap`'s entry function and
//! driven by a fake host through the C interface: the `gui` extension
//! opens, sizes, scales and closes the editor; `process` and
//! `params.flush` carry the editor's gestures to the host. The editor's
//! window is headless, so a test plays the person (pointer input in
//! logical pixels) and the system (the frame timer) through a
//! `HeadlessEditor`.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use manguebyte_gfx::{IRect, Point, Size};
use manguebyte_gui::HeadlessEditor;
use manguebyte_plugin::editor::{HostSize, Key, Modifiers};
use manguebyte_ui::Role;
use manguebyte_window::{Button, PhysicalPoint, RawInput};

use common::*;

fn up(at: Point, by: f32) -> Point {
    Point::new(at.x, at.y - by)
}

fn knob_center(editor: &HeadlessEditor) -> Point {
    get(editor, Role::Slider, "Gain").bounds.center()
}

#[test]
fn a_drag_on_the_knob_reaches_the_host_from_process_while_it_processes() {
    watch_panics();
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    assert_eq!(editor.tick(), 1, "the frame timer runs");
    assert_eq!(knob_text(&editor).as_deref(), Some("0.50"));
    session.take_calls();

    // Pointer down on the knob, two moves up, pointer up: one block after
    // each. Twenty logical pixels up is a tenth of the theme's 200-pixel
    // drag range.
    let at = knob_center(&editor);
    editor.press(at);
    assert_events(&session.process(), &[Out::Begin(GAIN)]);
    editor.move_to(up(at, 10.0));
    assert_events(&session.process(), &[Out::Value(GAIN, 0.55)]);
    editor.move_to(up(at, 20.0));
    // The interface stored the value at once; the host hears it in the
    // next block.
    assert!((params().gain.value() - 0.6).abs() < 1e-6);
    assert_events(&session.process(), &[Out::Value(GAIN, 0.6)]);
    assert_events(&session.process(), &[]);
    editor.release(up(at, 20.0));
    assert_events(&session.process(), &[Out::End(GAIN)]);
    assert_events(&session.process(), &[]);

    // A whole drag between two blocks arrives whole in the next one.
    editor.press(at);
    editor.move_to(up(at, 30.0));
    editor.release(up(at, 30.0));
    assert_events(
        &session.process(),
        &[Out::Begin(GAIN), Out::Value(GAIN, 0.75), Out::End(GAIN)],
    );
    assert!((session.value(GAIN) - 0.75).abs() < 1e-6);

    // While the host processes it is never asked for a flush.
    assert_eq!(session.take_calls(), []);
    // The knob shows the value it set.
    editor.tick();
    assert_eq!(knob_text(&editor).as_deref(), Some("0.75"));
    ui.destroy();
    assert!(!editor.is_open());
    assert_eq!(panics(), 0);
}

#[test]
fn a_drag_reaches_the_host_from_flush_when_it_is_not_processing() {
    watch_panics();
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(None);
    editor.tick();
    session.take_calls();

    // Not even activated: each edit asks the host for a flush.
    let at = knob_center(&editor);
    editor.press(at);
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_events(&session.flush(), &[Out::Begin(GAIN)]);
    editor.move_to(up(at, 40.0));
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_events(&session.flush(), &[Out::Value(GAIN, 0.7)]);
    assert_events(&session.flush(), &[]);
    editor.release(up(at, 40.0));
    assert_eq!(session.take_calls(), [HostCall::RequestFlush]);
    assert_events(&session.flush(), &[Out::End(GAIN)]);

    // Activated but not processing: the same.
    session.activate();
    editor.press(at);
    editor.move_to(up(at, 20.0));
    editor.release(up(at, 20.0));
    assert_eq!(session.take_calls(), [HostCall::RequestFlush; 3]);
    assert_events(
        &session.flush(),
        &[Out::Begin(GAIN), Out::Value(GAIN, 0.8), Out::End(GAIN)],
    );
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn host_automation_moves_the_knob_and_is_not_reported_back() {
    watch_panics();
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    editor.tick();
    session.take_calls();
    let (_, presented) = editor.frames();

    // The host plays an automation point.
    assert_events(&session.process_with(&[automation(GAIN, 0.8)]), &[]);
    assert_eq!(params().gain.value(), 0.8);
    editor.tick();
    let knob = get(&editor, Role::Slider, "Gain");
    assert_eq!(knob.node.value.as_deref(), Some("0.80"));
    assert!(
        knob.node
            .numeric
            .is_some_and(|value| (value - 0.8).abs() < 1e-6)
    );
    assert_eq!(editor.frames().1, presented + 1, "the knob was drawn again");

    // Nothing goes back to the host: no event, no flush request.
    for _ in 0..4 {
        assert_events(&session.process(), &[]);
        editor.tick();
    }
    assert_eq!(session.take_calls(), []);

    // A point in the middle of a drag: the drag's values stay the only
    // edits the host hears of.
    let at = knob_center(&editor);
    editor.press(at);
    assert_events(
        &session.process_with(&[automation(GAIN, 0.3)]),
        &[Out::Begin(GAIN)],
    );
    editor.tick();
    editor.move_to(up(at, 20.0));
    // The drag moves on from where the knob was taken hold of.
    assert_events(&session.process(), &[Out::Value(GAIN, 0.9)]);
    editor.release(up(at, 20.0));
    assert_events(&session.process(), &[Out::End(GAIN)]);
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn the_meter_shows_the_level_the_processor_publishes() {
    watch_panics();
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    editor.tick();
    assert_eq!(
        get(&editor, Role::Meter, "Output").node.value.as_deref(),
        Some("-60.0 dB")
    );

    // A block at the default gain of 0.5: 20 log10(0.5) is -6.02 dB.
    assert_events(&session.process(), &[]);
    assert!((level().load(Ordering::Relaxed) + 6.0206).abs() < 1e-3);
    editor.window().take_blits();
    editor.tick();
    let meter = get(&editor, Role::Meter, "Output");
    assert_eq!(meter.node.value.as_deref(), Some("-6.0 dB"));
    // Only the meter was drawn again and copied to the window.
    let blits = editor.window().take_blits();
    assert!(!blits.is_empty());
    for blit in &blits {
        assert!(
            meter.device.contains_rect(blit),
            "{blit:?} is outside {meter:?}"
        );
    }

    // The host turns the gain down to a quarter: -12.04 dB.
    session.process_with(&[automation(GAIN, 0.25)]);
    editor.tick();
    assert_eq!(
        get(&editor, Role::Meter, "Output").node.value.as_deref(),
        Some("-12.0 dB")
    );
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn frames_where_nothing_changed_present_nothing() {
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    session.process();
    // The first frame draws everything; the next ones nothing.
    editor.tick();
    let size = BASE;
    assert_eq!(
        editor.window().take_blits(),
        [IRect::new(0, 0, size.width, size.height)]
    );
    let (frames, presented) = editor.frames();
    let work = editor.work();
    for _ in 0..30 {
        assert_eq!(editor.tick(), 1);
    }
    // Each tick polled the knob's parameter and the meter's level, and
    // found nothing to lay out, draw or present.
    assert_eq!(editor.frames(), (frames + 30, presented));
    assert_eq!(editor.work(), work, "an idle frame drew or laid out");
    assert_eq!(editor.window().take_blits(), []);
    // A level that changes is drawn by the next tick: the meter alone.
    level().store(-20.0, Ordering::Relaxed);
    assert_eq!(editor.tick(), 1);
    assert_eq!(editor.work(), (work.0 + 1, work.1));
    assert_eq!(editor.frames(), (frames + 31, presented + 1));
    let (frames, _) = editor.frames();

    // What the window shows is what the interface drew.
    assert_eq!(editor.shown(), editor.pixels());

    // Hidden, the editor runs no frames at all; shown again, it does.
    assert!(ui.hide());
    assert_eq!(editor.tick(), 0);
    assert_eq!(editor.frames().0, frames);
    assert!(ui.show());
    assert_eq!(editor.tick(), 1);
    assert_eq!(editor.frames().0, frames + 1);
    ui.destroy();
}

/// "An editor that is hidden does no work" (`OpenEditor::set_visible`):
/// a size the host sets while the editor is hidden is only taken note of,
/// and drawn, in one frame, when the editor is shown again.
#[test]
fn a_hidden_editor_does_not_draw_when_the_host_resizes_it() {
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(1.0));
    editor.tick();
    assert!(ui.hide());
    editor.window().take_blits();
    let (frames, presented) = editor.frames();

    assert!(ui.set_size(HostSize::new(300, 200)));
    assert_eq!(ui.size(), Some(HostSize::new(300, 200)));
    assert_eq!(
        editor.frames(),
        (frames, presented),
        "a hidden editor ran and presented a frame"
    );
    assert_eq!(
        editor.window().take_blits(),
        [],
        "a hidden editor copied pixels to its window"
    );

    assert!(ui.show());
    assert_eq!(editor.frames(), (frames + 1, presented + 1));
    assert_eq!(editor.window().take_blits(), [IRect::new(0, 0, 300, 200)]);
    assert_eq!(editor.layout(), Some((Size::new(300.0, 200.0), 1.0)));
    assert_eq!(editor.shown(), editor.pixels());
    // Nothing is left to catch up on.
    assert!(ui.hide());
    assert!(ui.show());
    assert_eq!(editor.frames(), (frames + 1, presented + 1));
    ui.destroy();
}

/// The same for a scale the host sets while the editor is hidden.
#[test]
fn a_hidden_editor_does_not_draw_when_the_host_changes_its_scale() {
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(1.0));
    editor.tick();
    assert!(ui.hide());
    editor.window().take_blits();
    let (frames, presented) = editor.frames();
    session.take_calls();

    assert!(ui.set_scale(2.0));
    assert_eq!(session.take_calls(), [HostCall::RequestResize(480, 360)]);
    assert_eq!(ui.size(), Some(HostSize::new(480, 360)));
    assert_eq!(
        editor.frames(),
        (frames, presented),
        "a hidden editor ran and presented frames"
    );
    assert_eq!(editor.window().take_blits(), []);

    assert!(ui.show());
    assert_eq!(editor.frames(), (frames + 1, presented + 1));
    assert_eq!(editor.window().take_blits(), [IRect::new(0, 0, 480, 360)]);
    assert_eq!(editor.layout(), Some((Size::new(240.0, 180.0), 2.0)));
    ui.destroy();
}

/// The host moves the editor from scale 1 to 2 and grants the new size:
/// one frame, laid out at the editor's logical size and drawn at the size
/// the host granted, and nothing at the old size and the new scale (120
/// by 90 logical pixels, below the editor's minimum).
#[test]
fn a_new_scale_is_presented_once_at_the_size_the_host_grants() {
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(1.0));
    editor.tick();
    editor.window().take_blits();
    let (frames, presented) = editor.frames();

    assert!(ui.set_scale(2.0));
    assert_eq!(ui.size(), Some(HostSize::new(480, 360)));
    let blits = editor.window().take_blits();
    assert_eq!(
        editor.frames(),
        (frames + 1, presented + 1),
        "{blits:?} for one scale change"
    );
    assert_eq!(blits, [IRect::new(0, 0, 480, 360)]);
    assert_eq!(editor.layout(), Some((Size::new(240.0, 180.0), 2.0)));
    assert_eq!(editor.shown(), editor.pixels());

    // Refused: one frame at the new scale in the size the window kept.
    session.grant_resizes(false);
    assert!(ui.set_scale(1.0));
    assert_eq!(ui.size(), Some(HostSize::new(480, 360)));
    assert_eq!(editor.frames(), (frames + 2, presented + 2));
    assert_eq!(editor.window().take_blits(), [IRect::new(0, 0, 480, 360)]);
    assert_eq!(editor.layout(), Some((Size::new(480.0, 360.0), 1.0)));
    ui.destroy();
}

/// CLAP opens an editor with `set_parent` and shows it with `show`; until
/// then the editor should not be shown or run frames. The glue cannot do
/// this alone: VST 3 has no call that shows a view, and the VST 3 wrapper
/// does not call `set_visible`, so an editor that opened hidden would never
/// be shown there. Ignored until the VST 3 wrapper calls `set_visible(true)`
/// after `attached` and `set_visible(false)` in `removed`.
#[test]
#[ignore = "needs the VST 3 wrapper to call set_visible; see the test's comment"]
fn before_show_the_editor_is_not_shown_and_runs_no_frames() {
    let session = Session::new();
    let ui = session.ui();
    assert!(ui.create());
    assert!(ui.set_parent(PARENT));
    let editor = editor();
    let visible = editor.window().is_visible();
    let fired = editor.tick();
    ui.destroy();
    assert!(!visible, "the window was shown before the host's show");
    assert_eq!(fired, 0, "frames ran before the host's show");
}

#[test]
fn the_editor_follows_the_hosts_scale() {
    watch_panics();
    for scale in [1.0, 1.5, 2.0] {
        let session = Session::new();
        session.start();
        let ui = session.ui();
        assert!(ui.create());
        assert!(ui.set_scale(scale));
        let size = BASE.scale_by(scale);
        assert_eq!(ui.size(), Some(size), "at {scale}");
        assert!(ui.set_parent(PARENT));
        assert!(ui.show());
        let editor = editor();
        editor.tick();
        assert_eq!(ui.size(), Some(size), "at {scale}");
        assert_eq!(
            editor.layout(),
            Some((Size::new(240.0, 180.0), scale as f32)),
            "at {scale}"
        );
        let shown = editor.shown().expect("open");
        assert_eq!((shown.width(), shown.height()), (size.width, size.height));
        let knob = get(&editor, Role::Slider, "Gain");
        assert_eq!(
            knob.device,
            knob.bounds.to_device(scale as f32),
            "at {scale}"
        );

        // A drag of 20 logical pixels is a tenth of the range at any scale.
        let at = knob.bounds.center();
        editor.press(at);
        editor.move_to(up(at, 20.0));
        editor.release(up(at, 20.0));
        assert_events(
            &session.process(),
            &[Out::Begin(GAIN), Out::Value(GAIN, 0.6), Out::End(GAIN)],
        );
        ui.destroy();
    }

    // The largest scale the contract allows.
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(8.0));
    assert_eq!(editor.tick(), 1);
    assert_eq!(ui.size(), Some(HostSize::new(1920, 1440)));
    let shown = editor.shown().expect("open");
    assert_eq!((shown.width(), shown.height()), (1920, 1440));
    assert_eq!(shown, editor.pixels().expect("open"));
    ui.destroy();
    drop(session);

    // A scale the host changes while the editor is open: the editor asks
    // for its size at the new scale and takes what the host grants.
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(1.0));
    editor.tick();
    session.take_calls();
    assert!(ui.set_scale(2.0));
    assert_eq!(session.take_calls(), [HostCall::RequestResize(480, 360)]);
    assert_eq!(ui.size(), Some(HostSize::new(480, 360)));
    editor.tick();
    assert_eq!(editor.layout(), Some((Size::new(240.0, 180.0), 2.0)));
    assert_eq!(editor.shown().map(|shown| shown.width()), Some(480));

    // A host that refuses: the window keeps its size, and the interface
    // is laid out in it at the new scale.
    session.grant_resizes(false);
    assert!(ui.set_scale(1.5));
    assert_eq!(session.take_calls(), [HostCall::RequestResize(360, 270)]);
    assert_eq!(ui.size(), Some(HostSize::new(480, 360)));
    editor.tick();
    assert_eq!(editor.layout(), Some((Size::new(320.0, 240.0), 1.5)));

    // Scales outside the contract's range never reach the editor.
    for hostile in [0.0, -1.0, f64::NAN, f64::INFINITY, 0.1, 9.0] {
        assert!(!ui.set_scale(hostile), "{hostile}");
    }
    assert_eq!(editor.layout().map(|(_, scale)| scale), Some(1.5));
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn the_host_resizes_the_editor_through_adjust_size_and_set_size() {
    watch_panics();
    let session = Session::new();
    let ui = session.ui();
    let editor = ui.open(Some(1.5));
    editor.tick();
    assert!(ui.can_resize());
    let hints = ui.hints().expect("hints");
    assert!(hints.can_resize_horizontally && hints.can_resize_vertically);
    assert!(!hints.preserve_aspect_ratio);

    // The rules, at the host's scale.
    assert_eq!(
        ui.adjust(HostSize::new(10_000, 10_000)),
        Some(HostSize::new(720, 540))
    );
    assert_eq!(
        ui.adjust(HostSize::new(1, 1)),
        Some(HostSize::new(300, 225))
    );
    assert_eq!(
        ui.adjust(HostSize::new(0, u32::MAX)),
        Some(HostSize::new(300, 540))
    );
    let wanted = ui.adjust(HostSize::new(400, 300)).expect("adjusted");
    assert_eq!(wanted, HostSize::new(400, 300));

    // The host makes its window that size and tells the editor, which
    // lays itself out and draws everything again at once.
    editor.window().take_blits();
    assert!(ui.set_size(wanted));
    assert_eq!(ui.size(), Some(wanted));
    assert_eq!(editor.window().take_blits(), [IRect::new(0, 0, 400, 300)]);
    editor.tick();
    assert_eq!(editor.window().take_blits(), []);
    assert_eq!(
        editor.size().map(|size| (size.width, size.height)),
        Some((400, 300))
    );
    let (logical, scale) = editor.layout().expect("open");
    assert_eq!(scale, 1.5);
    assert!((logical.width - 400.0 / 1.5).abs() < 1e-3 && (logical.height - 200.0).abs() < 1e-3);
    assert_eq!(editor.shown(), editor.pixels());

    // A size the rules do not give is refused, and nothing changes.
    for refused in [
        HostSize::new(10, 10),
        HostSize::new(721, 300),
        HostSize::new(0, 0),
        HostSize::new(u32::MAX, u32::MAX),
    ] {
        assert!(!ui.set_size(refused), "{refused:?}");
        assert_eq!(ui.size(), Some(wanted));
    }
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn closing_in_the_middle_of_a_drag_ends_the_gesture() {
    watch_panics();
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    editor.tick();
    let at = knob_center(&editor);
    editor.press(at);
    assert_events(&session.process(), &[Out::Begin(GAIN)]);
    editor.move_to(up(at, 10.0));
    ui.destroy();
    assert!(!editor.is_open());
    assert_events(
        &session.process(),
        &[Out::Value(GAIN, 0.55), Out::End(GAIN)],
    );

    // Escape during a drag puts the starting value back.
    let editor = ui.open(None);
    editor.tick();
    editor.press(at);
    editor.move_to(up(at, 40.0));
    assert!(editor.key(Key::Escape));
    editor.release(up(at, 40.0));
    let events = session.process();
    assert_events(&events, &[Out::Begin(GAIN), Out::End(GAIN)]);
    assert!((session.value(GAIN) - 0.55).abs() < 1e-6);
    ui.destroy();
    assert_eq!(panics(), 0);
}

#[test]
fn a_parent_destroyed_by_the_host_closes_the_editor_and_tells_the_host() {
    watch_panics();
    let session = Session::new();
    session.start();
    let ui = session.ui();
    let editor = ui.open(None);
    editor.tick();
    let at = knob_center(&editor);
    editor.press(at);
    session.take_calls();
    editor.window().destroy_parent();
    assert!(!editor.is_open());
    assert_eq!(session.take_calls(), [HostCall::Closed(true)]);
    // The drag that was going on was ended with the interface.
    assert_events(&session.process(), &[Out::Begin(GAIN), Out::End(GAIN)]);
    // The host destroys the editor, as CLAP asks, and can open a new one.
    ui.destroy();
    let editor = ui.open(None);
    assert_eq!(editor.tick(), 1);
    ui.destroy();
    assert_eq!(panics(), 0);
}

/// ADR 0007's lifetime test: thousands of editors opened and closed while
/// `process` runs on another thread, with no heap call in `process`.
#[test]
fn ten_thousand_editors_open_and_close_while_process_runs_on_another_thread() {
    watch_panics();
    const CYCLES: usize = 10_000;
    let session = Session::new();
    session.activate();
    let ui = session.ui();
    let stop = Arc::new(AtomicBool::new(false));
    let audio = session.audio();
    let running = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        let audio = audio;
        assert!(audio.start());
        let (mut heap, mut blocks, mut gestures) = (0, 0_u64, 0);
        let mut open = Vec::with_capacity(4);
        while !running.load(Ordering::Acquire) {
            let mut outbox = Outbox::new();
            heap += audio.block(&[], &mut outbox);
            gestures += check_nesting(&outbox.to_vec(), &mut open);
            blocks += 1;
            // A host's audio thread waits for its device between blocks;
            // this one only lets the main thread run.
            std::thread::yield_now();
        }
        // Whatever is still on its way.
        for _ in 0..4 {
            let mut outbox = Outbox::new();
            heap += audio.block(&[], &mut outbox);
            gestures += check_nesting(&outbox.to_vec(), &mut open);
        }
        audio.stop();
        (heap, blocks, gestures, open.len(), panics())
    });

    let mut drags = 0;
    for cycle in 0..CYCLES {
        assert!(ui.create());
        if cycle % 3 == 1 {
            assert!(ui.set_scale([1.5, 2.0][cycle % 2]));
        }
        assert!(ui.set_parent(PARENT));
        assert!(ui.show());
        let editor = editor();
        if cycle % 500 == 0 {
            // Now and then a frame, and a drag the audio thread reports.
            assert_eq!(editor.tick(), 1);
            let at = knob_center(&editor);
            editor.press(at);
            editor.move_to(up(at, 1.0 + (cycle / 500) as f32));
            editor.release(up(at, 1.0 + (cycle / 500) as f32));
            drags += 1;
        }
        if cycle % 7 == 0 {
            assert!(ui.hide());
        }
        ui.destroy();
        assert!(!editor.is_open());
    }
    // Let the audio thread see the last edits.
    std::thread::sleep(Duration::from_millis(50));
    stop.store(true, Ordering::Release);
    let (heap, blocks, gestures, open, audio_panics) = thread.join().expect("the audio thread");
    assert_eq!(opens(), CYCLES);
    assert_eq!(heap, 0, "process used the heap");
    assert!(blocks > 0);
    assert_eq!(
        gestures, drags,
        "every drag reached the host as one gesture"
    );
    assert_eq!(open, 0, "a gesture was left open");
    assert_eq!((panics(), audio_panics), (0, 0));
    eprintln!("{CYCLES} editors opened and closed during {blocks} blocks");
}

/// A small deterministic generator (xorshift64*), so that failures repeat.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn pick<T: Copy>(&mut self, from: &[T]) -> T {
        from[self.below(from.len() as u64) as usize]
    }
}

/// Every `gui` call a host can make, and every input a person or the
/// system can give the window, in random orders with hostile arguments.
/// Nothing panics, `process` and `flush` never use the heap, the events
/// the host receives are always well formed, and the size the host reads
/// is always the editor's.
#[test]
fn random_orders_of_host_calls_and_input_keep_every_promise() {
    watch_panics();
    let scales = [
        f64::NAN,
        f64::INFINITY,
        -1.0,
        0.0,
        0.1,
        0.25,
        1.0,
        1.25,
        1.5,
        2.0,
        8.5,
        f64::MAX,
    ];
    let sides = [
        0,
        1,
        150,
        199,
        200,
        300,
        360,
        480,
        481,
        720,
        4096,
        70_000,
        u32::MAX,
    ];
    let coordinates = [
        -1.0e9, -5.0, 0.0, 3.0, 60.0, 90.0, 120.0, 239.9, 500.0, 1.0e9,
    ];
    // Window input is in whole device pixels: the extremes of `i32`, and
    // wheel turns as far as `i32` goes, reach the window as they are.
    let raw = [i32::MIN, -1, 0, 1, 239, 4096, i32::MAX];
    for seed in 1..=3_u64 {
        let started = std::time::Instant::now();
        let mut random = Random(0x9E37_79B9_7F4A_7C15 ^ seed);
        let session = Session::new();
        let ui = session.ui();
        let mut processing = false;
        let mut open_at_host = Vec::new();
        let mut editor: Option<HeadlessEditor> = None;
        let mut opened_at = 0;
        for step in 0..1_500 {
            let point = |random: &mut Random| {
                Point::new(random.pick(&coordinates), random.pick(&coordinates))
            };
            match random.below(21) {
                0..=2 => {
                    let created = random.below(3) != 0 && ui.create();
                    let handle = random.pick(&[0, 1, PARENT, usize::MAX]);
                    if (created || random.below(2) == 0) && ui.set_parent(handle) {
                        editor = Some(common::editor());
                    }
                }
                3 => {
                    ui.destroy();
                }
                4 => {
                    let _ = ui.set_scale(random.pick(&scales));
                }
                5 => {
                    let size = HostSize::new(random.pick(&sides), random.pick(&sides));
                    let _ = ui.adjust(size);
                }
                6 => {
                    let size = HostSize::new(random.pick(&sides), random.pick(&sides));
                    let wanted = if random.below(2) == 0 {
                        ui.adjust(size).unwrap_or(size)
                    } else {
                        size
                    };
                    let _ = ui.set_size(wanted);
                }
                7 => {
                    let _ = (ui.show(), ui.hide(), ui.show());
                }
                8 => {
                    let _ = ui.hide();
                }
                9 | 10 => {
                    if let Some(editor) = &editor {
                        editor.tick();
                    }
                }
                11 => {
                    if let Some(editor) = &editor {
                        editor.press(point(&mut random));
                    }
                }
                12 => {
                    if let Some(editor) = &editor {
                        editor.move_to(point(&mut random));
                    }
                }
                13 => {
                    if let Some(editor) = &editor {
                        editor.release(point(&mut random));
                    }
                }
                14 => {
                    if let Some(editor) = &editor {
                        let key =
                            random.pick(&[Key::Up, Key::Down, Key::Tab, Key::Escape, Key::Home]);
                        editor.key(key);
                    }
                }
                15 => {
                    if processing {
                        let input = [automation(GAIN, random.below(101) as f64 / 100.0)];
                        let events = session.process_with(&input[..random.below(2) as usize]);
                        check_nesting(&events, &mut open_at_host);
                    } else {
                        check_nesting(&session.flush(), &mut open_at_host);
                    }
                }
                16 => {
                    if processing {
                        session.stop();
                    } else {
                        session.start();
                    }
                    processing = !processing;
                }
                17 => {
                    if let Some(editor) = &editor {
                        if random.below(8) == 0 {
                            editor.window().destroy_parent();
                        }
                    }
                }
                18 => {
                    ui.on_main_thread();
                }
                19 => {
                    if let Some(editor) = &editor {
                        let at = PhysicalPoint::new(random.pick(&raw), random.pick(&raw));
                        let modifiers = Modifiers::NONE;
                        let input = match random.below(4) {
                            0 => RawInput::PointerDown {
                                at,
                                button: random.pick(&[
                                    Button::Primary,
                                    Button::Secondary,
                                    Button::Middle,
                                ]),
                                modifiers,
                                time_ms: random.next() as u32,
                            },
                            1 => RawInput::PointerMove { at, modifiers },
                            2 => RawInput::PointerUp {
                                at,
                                button: Button::Primary,
                                modifiers,
                            },
                            _ => RawInput::Wheel {
                                at,
                                delta_x: random.pick(&raw),
                                delta_y: random.pick(&raw),
                                modifiers,
                            },
                        };
                        editor.window().input(input);
                    }
                }
                _ => {
                    if let Some(editor) = &editor {
                        editor.window().set_system_scale(random.pick(&scales));
                    }
                }
            }
            session.take_calls();
            // The size the host reads is the size of the editor's window.
            if let Some(editor) = editor.as_ref().filter(|editor| editor.is_open()) {
                opened_at += 1;
                let shown = editor.size().expect("open");
                assert_eq!(
                    ui.size(),
                    Some(HostSize::new(shown.width, shown.height)),
                    "seed {seed}, step {step}"
                );
            }
            assert_eq!(panics(), 0, "seed {seed}, step {step}");
        }
        ui.destroy();
        // Whatever was left open at the host is ended.
        if processing {
            check_nesting(&session.process(), &mut open_at_host);
        } else {
            check_nesting(&session.flush(), &mut open_at_host);
        }
        assert_eq!(open_at_host, [], "seed {seed}");
        drop(editor);
        eprintln!(
            "seed {seed}: {:?}, an editor was open after {opened_at} of 1,500 steps",
            started.elapsed()
        );
    }
}
