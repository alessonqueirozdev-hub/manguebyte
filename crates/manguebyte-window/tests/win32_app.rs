//! Application windows on Win32: real top-level windows, the real message
//! loop, OLE drops through the window's registered `IDropTarget`, and the
//! file dialogs configured through COM (never shown: a dialog in CI would
//! wait for a person, so showing them is tested by hand only).
//!
//! A test that reads pixels fails when this session cannot read windows
//! back, unless `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is set (CI never sets
//! it); then it says so on stderr and skips those checks.

#![cfg(windows)]

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use manguebyte_gfx::{IRect, Pixmap, Point, Size};
use manguebyte_window::win32::testing::{self, Drag, Message, Ole, ReadBack, TopLevel};
use manguebyte_window::{
    App, AppWindowOptions, DialogKind, DragData, Event, FileDialog, Handled, PhysicalPoint,
    PhysicalSize, RunEnd, TimerId, Window, WindowCx, WindowOptions,
};
use windows_sys::Win32::System::Ole::{DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE};
use windows_sys::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR,
    FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS,
};

type Log = Rc<RefCell<Vec<Event>>>;

fn options(title: &str, width: f32, height: f32) -> AppWindowOptions {
    // Shown without taking the keyboard from the person at the desktop.
    AppWindowOptions::new(title, Size::new(width, height)).without_activation()
}

/// An application window whose handler logs every event and agrees to a
/// close request only when `agree` says so.
fn logged(app: &App, options: AppWindowOptions, agree: Rc<Cell<bool>>) -> (Window, Log) {
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    let window = app
        .open_window(options, move |cx: &WindowCx<'_>, event| {
            sink.borrow_mut().push(event);
            if event == Event::CloseRequested && agree.get() {
                cx.close();
            }
            Handled::Yes
        })
        .expect("an application window");
    (window, log)
}

fn take(log: &Log) -> Vec<Event> {
    log.borrow_mut().drain(..).collect()
}

fn handle(window: &Window) -> usize {
    window.raw_handle().expect("open")
}

fn at(x: i32, y: i32) -> PhysicalPoint {
    PhysicalPoint::new(x, y)
}

/// What the frame adds to the window's inside, in pixels.
fn frame_of(window: &Window) -> (u32, u32) {
    let outer = testing::window_rect(handle(window)).expect("a window");
    let (width, height) = testing::client_size_of(handle(window)).expect("a window");
    (outer.width - width, outer.height - height)
}

#[test]
fn an_app_window_is_a_framed_top_level_window_of_its_logical_size() {
    let _aware = testing::PerMonitorDpi::enter().expect("per-monitor awareness v2");
    let app = App::new().expect("Win32");
    let (window, log) = logged(
        &app,
        options("Parabolic \u{2014} Kit 1", 320.0, 200.0),
        Rc::new(Cell::new(true)),
    );
    let hwnd = handle(&window);
    let scale = window.scale();
    eprintln!("the window's scale on this thread: {scale}");
    assert_eq!(
        testing::window_dpi(hwnd).map(|dpi| f64::from(dpi) / 96.0),
        Some(scale)
    );
    let want = PhysicalSize::new(
        (320.0 * scale).round() as u32,
        (200.0 * scale).round() as u32,
    );
    assert_eq!(window.size(), want);
    assert_eq!(
        testing::client_size_of(hwnd),
        Some((want.width, want.height))
    );
    assert_eq!(testing::parent_of_any(hwnd), None);
    assert_eq!(
        testing::window_title(hwnd).as_deref(),
        Some("Parabolic \u{2014} Kit 1")
    );
    assert_eq!(testing::is_resizable(hwnd), Some(true));
    assert!(window.is_app_window());
    assert!(window.headless_controller().is_none());
    // Made and sized quietly: the handler heard nothing yet.
    assert!(
        take(&log)
            .iter()
            .all(|event| !matches!(event, Event::Resized(_) | Event::ScaleChanged(_))),
    );
    window.set_title("Kit 2").expect("open");
    assert_eq!(testing::window_title(hwnd).as_deref(), Some("Kit 2"));

    let (fixed, _) = logged(
        &app,
        options("Fixed", 100.0, 100.0).fixed(),
        Rc::new(Cell::new(true)),
    );
    assert_eq!(testing::is_resizable(handle(&fixed)), Some(false));
}

#[test]
fn what_an_app_window_presents_reads_back_exactly() {
    let probe = TopLevel::new(64, 64).expect("a desktop");
    let app = App::new().expect("Win32");
    let frame = {
        let mut data = Vec::new();
        for y in 0..90u32 {
            for x in 0..120u32 {
                data.extend_from_slice(&[(x * 2) as u8, (y * 2) as u8, 77, 255]);
            }
        }
        Pixmap::from_premultiplied(120, 90, data).expect("opaque")
    };
    let presents = Rc::new(Cell::new(0));
    let window = {
        let (frame, presents) = (frame.clone(), Rc::clone(&presents));
        app.open_window(
            options("Pixels", 120.0, 90.0).at(at(40, 40)),
            move |cx: &WindowCx<'_>, event| {
                if let Event::Paint(damage) = event {
                    cx.present(&frame, damage.rects()).expect("open");
                    presents.set(presents.get() + 1);
                }
                Handled::Yes
            },
        )
        .expect("an application window")
    };
    assert_eq!(window.size(), PhysicalSize::new(120, 90), "DPI unaware");
    assert!(testing::update(handle(&window)));
    testing::pump(Duration::from_millis(30));
    assert!(presents.get() >= 1);
    if testing::require_read_back(&probe, IRect::new(0, 0, 32, 32)) {
        let shown = testing::read_back(&window.cx(), ReadBack::Screen).expect("our window");
        for (x, y) in [(0, 0), (119, 0), (0, 89), (119, 89), (60, 45)] {
            assert_eq!(
                shown.pixel(x, y).map(|p| p.to_array()),
                frame.pixel(x, y).map(|p| p.to_array()),
                "({x}, {y})"
            );
        }
    }
}

#[test]
fn the_close_button_and_the_window_menu_ask_and_the_application_decides() {
    let app = App::new().expect("Win32");
    let agree = Rc::new(Cell::new(false));
    let (window, log) = logged(&app, options("Ask", 100.0, 80.0), Rc::clone(&agree));
    let hwnd = handle(&window);
    take(&log);
    testing::send(hwnd, Message::Close).expect("ours");
    // Alt+F4, the window menu and the taskbar send SC_CLOSE; the system's
    // default handling turns it into WM_CLOSE.
    testing::send(hwnd, Message::SysCommandClose).expect("ours");
    let asked: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| *event == Event::CloseRequested)
        .collect();
    assert_eq!(asked.len(), 2);
    assert!(window.is_open() && testing::is_window(hwnd));

    agree.set(true);
    testing::send(hwnd, Message::Close).expect("ours");
    assert!(!window.is_open());
    assert!(!testing::is_window(hwnd));
    let events = take(&log);
    assert_eq!(
        events[events.len() - 2..],
        [Event::CloseRequested, Event::Closed]
    );
    assert_eq!(app.open_windows(), 0);
}

#[test]
fn the_system_loop_runs_until_the_last_window_closes() {
    let app = App::new().expect("Win32");
    let ticks = Rc::new(Cell::new(0));
    let window = {
        let ticks = Rc::clone(&ticks);
        app.open_window(
            options("Loop", 60.0, 40.0),
            move |cx: &WindowCx<'_>, event| {
                if event == Event::Timer(TimerId(1)) {
                    ticks.set(ticks.get() + 1);
                    if ticks.get() == 3 {
                        cx.close();
                    }
                }
                Handled::Yes
            },
        )
        .expect("an application window")
    };
    window.set_timer(TimerId(1), 10).expect("open");
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
    assert_eq!(ticks.get(), 3);
}

#[test]
fn quit_and_the_quit_message_end_the_system_loop() {
    let app = App::new().expect("Win32");
    let step = Rc::new(Cell::new(0));
    let window = {
        let (app, step) = (app.clone(), Rc::clone(&step));
        app.clone()
            .open_window(
                options("Quit", 60.0, 40.0),
                move |_: &WindowCx<'_>, event| {
                    if event == Event::Timer(TimerId(1)) {
                        step.set(step.get() + 1);
                        match step.get() {
                            1 => app.quit(),
                            2 => testing::post_quit(7),
                            _ => {}
                        }
                    }
                    Handled::Yes
                },
            )
            .expect("an application window")
    };
    window.set_timer(TimerId(1), 10).expect("open");
    assert_eq!(app.run(), Ok(RunEnd::Quit));
    assert!(window.is_open());
    assert_eq!(app.run(), Ok(RunEnd::QuitMessage(7)));
    assert!(window.is_open());
}

#[test]
fn the_limits_reach_the_system_as_tracking_sizes() {
    let app = App::new().expect("Win32");
    let (window, _) = logged(
        &app,
        options("Limits", 300.0, 200.0)
            .with_min_size(Size::new(200.0, 120.0))
            .with_max_size(Size::new(640.0, 480.0)),
        Rc::new(Cell::new(true)),
    );
    let (frame_width, frame_height) = frame_of(&window);
    let (min, max) = testing::tracking_limits(handle(&window)).expect("ours");
    assert_eq!(
        min,
        PhysicalSize::new(200 + frame_width, 120 + frame_height)
    );
    assert_eq!(
        max,
        PhysicalSize::new(640 + frame_width, 480 + frame_height)
    );
    // A window with no limits leaves the system's own.
    let (free, _) = logged(
        &app,
        options("Free", 300.0, 200.0),
        Rc::new(Cell::new(true)),
    );
    assert_eq!(
        testing::tracking_limits(handle(&free)),
        Some((PhysicalSize::new(1, 1), PhysicalSize::new(100_000, 100_000)))
    );
    // The program keeps within the limits too.
    window.resize(PhysicalSize::new(5000, 50)).expect("open");
    assert_eq!(window.size(), PhysicalSize::new(640, 120));
    assert_eq!(testing::client_size_of(handle(&window)), Some((640, 120)));
}

#[test]
fn a_move_to_a_monitor_of_another_dpi_changes_the_scale_then_the_size() {
    let app = App::new().expect("Win32");
    let (window, log) = logged(&app, options("DPI", 200.0, 100.0), Rc::new(Cell::new(true)));
    let hwnd = handle(&window);
    let before = testing::window_rect(hwnd).expect("a window");
    let (frame_width, frame_height) = frame_of(&window);
    take(&log);
    // The system suggests the same logical inside at 144 DPI, moved.
    let suggested = IRect::new(
        before.x + 10,
        before.y + 20,
        300 + frame_width,
        150 + frame_height,
    );
    testing::dpi_changed(hwnd, 144, suggested).expect("ours");
    let events: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| !matches!(event, Event::Paint(_)))
        .collect();
    assert_eq!(events[0], Event::ScaleChanged(1.5), "{events:?}");
    let Some(Event::Resized(size)) = events.get(1).copied() else {
        panic!("{events:?}")
    };
    assert_eq!(window.scale(), 1.5);
    assert_eq!(window.size(), size);
    let (width, height) = testing::client_size_of(hwnd).expect("a window");
    assert_eq!(PhysicalSize::new(width, height), size);
    // The frame was measured at the new DPI, which this unaware test
    // thread only pretends to have: allow its difference.
    assert!(
        width.abs_diff(300) <= 8 && height.abs_diff(150) <= 16,
        "{size:?}"
    );
    let after = testing::window_rect(hwnd).expect("a window");
    assert_eq!((after.x, after.y), (suggested.x, suggested.y));
}

/// An application window that takes `.wav` drags over its left half.
fn drop_zone(app: &App) -> (Window, Log, Rc<RefCell<Vec<DragData>>>) {
    let log: Log = Rc::default();
    let seen: Rc<RefCell<Vec<DragData>>> = Rc::default();
    let (sink, carried) = (Rc::clone(&log), Rc::clone(&seen));
    let window = app
        .open_window(
            options("Drops", 200.0, 100.0).with_file_drops(),
            move |cx: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                if let Some(data) = cx.drag_data() {
                    carried.borrow_mut().push((*data).clone());
                }
                match event {
                    Event::DragEnter(pointer) | Event::DragOver(pointer) | Event::Drop(pointer) => {
                        if pointer.position.x < 100.0 {
                            Handled::Yes
                        } else {
                            Handled::No
                        }
                    }
                    _ => Handled::Yes,
                }
            },
        )
        .expect("an application window");
    (window, log, seen)
}

fn drags(log: &Log) -> Vec<Event> {
    take(log)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::DragEnter(_) | Event::DragOver(_) | Event::DragLeave | Event::Drop(_)
            )
        })
        .collect()
}

#[test]
fn files_dropped_reach_the_handler_through_the_registered_drop_target() {
    let app = App::new().expect("Win32");
    let (window, log, seen) = drop_zone(&app);
    let hwnd = handle(&window);
    assert!(window.accepts_drops(), "OLE took the window");
    assert!(testing::is_drop_target(hwnd));

    let files = vec![
        PathBuf::from(r"C:\Samples\Kick 01.wav"),
        PathBuf::from(r"C:\Samples\Snäre.wav"),
    ];
    let carried = DragData::files(files.clone()).with_text("two samples");
    let drag = Drag::new(&window.cx(), &carried).expect("a drop target");
    let allowed = DROPEFFECT_COPY | DROPEFFECT_MOVE;
    assert_eq!(drag.enter(at(10, 20), 0, allowed), DROPEFFECT_COPY);
    assert_eq!(
        drag.over(at(150, 20), testing::DRAG_SHIFT, allowed),
        DROPEFFECT_NONE
    );
    // Only a move allowed: never accepted, as the source would delete.
    assert_eq!(drag.over(at(20, 20), 0, DROPEFFECT_MOVE), DROPEFFECT_NONE);
    assert_eq!(drag.drop_at(at(30, 40), 0, allowed), DROPEFFECT_COPY);
    let events = drags(&log);
    assert_eq!(events.len(), 4, "{events:?}");
    let pointer = |event: Event| match event {
        Event::DragEnter(p) | Event::DragOver(p) | Event::Drop(p) => p,
        other => panic!("{other:?}"),
    };
    assert_eq!(pointer(events[0]).physical, at(10, 20));
    assert_eq!(pointer(events[0]).position, Point::new(10.0, 20.0));
    assert!(pointer(events[1]).modifiers.shift);
    assert!(matches!(events[3], Event::Drop(_)));
    assert_eq!(pointer(events[3]).physical, at(30, 40));
    // Every event saw the files and the text, read through CF_HDROP and
    // CF_UNICODETEXT.
    assert!(seen.borrow().iter().all(|data| *data == carried));
    assert_eq!(seen.borrow().len(), 4);
    assert!(window.drag_data().is_none());
    // The target kept no reference to the data object.
    assert_eq!(drag.data_references(), 1);

    // A drag that leaves.
    assert_eq!(drag.enter(at(10, 20), 0, allowed), DROPEFFECT_COPY);
    drag.leave();
    assert!(matches!(
        drags(&log)[..],
        [Event::DragEnter(_), Event::DragLeave]
    ));

    // Closed, the window is no drop target, and the target OLE may still
    // hold refuses everything.
    window.close();
    assert!(!testing::is_drop_target(hwnd));
    assert_eq!(drag.enter(at(10, 20), 0, allowed), DROPEFFECT_NONE);
}

#[test]
fn a_drag_that_comes_while_the_handler_runs_is_refused_then_heard() {
    let app = App::new().expect("Win32");
    let drag: Rc<RefCell<Option<Drag>>> = Rc::default();
    let answers = Rc::new(RefCell::new(Vec::new()));
    let log: Log = Rc::default();
    let window = {
        let (drag, answers, sink) = (Rc::clone(&drag), Rc::clone(&answers), Rc::clone(&log));
        app.open_window(
            options("Busy", 100.0, 100.0).with_file_drops(),
            move |_: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                if event == Event::Timer(TimerId(3)) {
                    let drag = drag.borrow();
                    let drag = drag.as_ref().expect("set");
                    answers
                        .borrow_mut()
                        .push(drag.enter(at(5, 5), 0, DROPEFFECT_COPY));
                }
                Handled::Yes
            },
        )
        .expect("an application window")
    };
    *drag.borrow_mut() = Drag::new(&window.cx(), &DragData::files(["late.wav"]));
    window.set_timer(TimerId(3), 1000).expect("open");
    testing::send(handle(&window), Message::Timer(3)).expect("ours");
    assert_eq!(*answers.borrow(), [DROPEFFECT_NONE]);
    assert!(drags(&log).is_empty());
    let drag = drag.borrow();
    let drag = drag.as_ref().expect("set");
    assert_eq!(drag.over(at(6, 6), 0, DROPEFFECT_COPY), DROPEFFECT_COPY);
    assert!(matches!(drags(&log)[..], [Event::DragEnter(_)]));
    assert_eq!(drag.drop_at(at(6, 6), 0, DROPEFFECT_COPY), DROPEFFECT_COPY);
}

#[test]
fn a_child_window_takes_drops_only_where_the_host_has_ole() {
    let parent = TopLevel::new(120, 100).expect("a desktop");
    let log: Log = Rc::default();
    let open = |log: &Log| {
        let sink = Rc::clone(log);
        Window::open(
            parent.parent(),
            WindowOptions::new(PhysicalSize::new(60, 40)).with_file_drops(),
            move |_: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    // This test's thread has no OLE, as a host's may not: the window never
    // initialises it for the host's thread, and takes no drops. Another
    // thread of the process has OLE meanwhile, which once made the
    // system's RegisterDragDrop fail and still mark the window.
    let (ready, wait) = std::sync::mpsc::channel();
    let (done, finish) = std::sync::mpsc::channel::<()>();
    let other = std::thread::spawn(move || {
        let ole = Ole::initialise();
        ready.send(ole.is_initialised()).expect("the test waits");
        finish.recv().ok();
    });
    assert!(wait.recv().expect("the other thread"));
    let without = open(&log);
    assert!(!without.accepts_drops());
    assert!(!testing::is_drop_target(handle(&without)));
    assert!(Drag::new(&without.cx(), &DragData::files(["x.wav"])).is_none());
    drop(without);
    done.send(()).expect("the other thread waits");
    other.join().expect("the other thread");

    let ole = Ole::initialise();
    assert!(ole.is_initialised());
    let with = open(&log);
    assert!(with.accepts_drops());
    let hwnd = handle(&with);
    assert!(testing::is_drop_target(hwnd));
    let drag = Drag::new(&with.cx(), &DragData::files([r"C:\a.wav"])).expect("a target");
    assert_eq!(drag.enter(at(1, 1), 0, DROPEFFECT_COPY), DROPEFFECT_COPY);
    assert_eq!(drag.drop_at(at(2, 2), 0, DROPEFFECT_COPY), DROPEFFECT_COPY);
    assert!(matches!(
        drags(&log)[..],
        [Event::DragEnter(_), Event::Drop(_)]
    ));
    // The host destroying the parent revokes the target with the window.
    parent.destroy();
    assert!(!with.is_open());
    assert!(!testing::is_drop_target(hwnd));
    drop(drag);
    drop(with);
    drop(ole);
}

#[test]
fn the_file_dialogs_are_made_and_configured_as_asked() {
    let folder = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(&folder).expect("the target's temporary folder");
    let canonical = |path: &PathBuf| std::fs::canonicalize(path).expect("exists");

    let open = FileDialog::new()
        .with_title("Add samples")
        .with_filter("Audio", ["wav", "flac"])
        .with_filter("Everything", [""; 0])
        .in_folder(&folder)
        .multiple();
    let prepared = testing::prepare_dialog(DialogKind::OpenFiles, &open).expect("COM");
    let ours = testing::dialog_options(DialogKind::OpenFiles, &open);
    assert_eq!(prepared.options & ours, ours);
    for flag in [
        FOS_ALLOWMULTISELECT,
        FOS_FILEMUSTEXIST,
        FOS_FORCEFILESYSTEM,
        FOS_NOCHANGEDIR,
    ] {
        assert_ne!(prepared.options & flag, 0, "{flag:#x}");
    }
    assert_eq!(prepared.file_type_index, 1);
    assert_eq!(
        prepared.folder.as_ref().map(canonical),
        Some(canonical(&folder))
    );

    let folder_dialog = FileDialog::new().with_title("Sample folder");
    let prepared = testing::prepare_dialog(DialogKind::PickFolder, &folder_dialog).expect("COM");
    assert_ne!(prepared.options & FOS_PICKFOLDERS, 0);
    assert_eq!(prepared.options & FOS_ALLOWMULTISELECT, 0);

    // On a thread with OLE, as an application's, the same.
    let _ole = Ole::initialise();
    let save = FileDialog::new()
        .with_filter("Presets", ["mbpreset"])
        .with_file_name("Init.mbpreset")
        .in_folder(&folder);
    // (The name it offers cannot be read back before it is shown:
    // `GetFileName` fails until then.)
    let prepared = testing::prepare_dialog(DialogKind::SaveFile, &save).expect("COM");
    assert_ne!(prepared.options & FOS_OVERWRITEPROMPT, 0);
    assert_eq!(prepared.file_type_index, 1);
    assert_eq!(
        prepared.folder.as_ref().map(canonical),
        Some(canonical(&folder))
    );
}
