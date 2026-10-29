//! Application windows, their loop, drops and dialogs, on the headless
//! backend: the same code the Win32 backend runs for everything but the
//! system calls, on every system.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use manguebyte_gfx::{Point, Size};
use manguebyte_window::{
    App, AppWindowOptions, DialogKind, DragData, Event, FileDialog, Handled, HeadlessApp,
    PhysicalPoint, PhysicalSize, RawInput, RunEnd, TimerId, Window, WindowCx, WindowError,
    WindowOptions,
};

type Log = Rc<RefCell<Vec<Event>>>;

fn options(title: &str, width: f32, height: f32) -> AppWindowOptions {
    AppWindowOptions::new(title, Size::new(width, height))
}

/// An application window whose handler logs every event and agrees to
/// every close request.
fn logged(app: &App, options: AppWindowOptions) -> (Window, Log) {
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    let window = app
        .open_window(options, move |cx: &WindowCx<'_>, event| {
            sink.borrow_mut().push(event);
            if event == Event::CloseRequested {
                cx.close();
            }
            Handled::Yes
        })
        .expect("valid options");
    (window, log)
}

fn take(log: &Log) -> Vec<Event> {
    log.borrow_mut().drain(..).collect()
}

fn at(x: i32, y: i32) -> PhysicalPoint {
    PhysicalPoint::new(x, y)
}

#[test]
fn the_loop_ends_when_the_last_window_closes() {
    let (app, test) = App::headless();
    let (first, first_log) = logged(&app, options("One", 200.0, 100.0));
    let (second, second_log) = logged(&app, options("Two", 200.0, 100.0));
    assert_eq!(app.open_windows(), 2);
    let windows = test.windows();
    assert_eq!(windows.len(), 2);

    let one = windows[0].clone();
    test.post(move || {
        one.request_close();
    });
    // After the first closes, the loop goes on for the second.
    let two = windows[1].clone();
    let watch = app.clone();
    test.post(move || {
        assert_eq!(watch.open_windows(), 1);
        two.request_close();
    });
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
    assert_eq!(take(&first_log), [Event::CloseRequested, Event::Closed]);
    assert_eq!(take(&second_log), [Event::CloseRequested, Event::Closed]);
    assert!(!first.is_open() && !second.is_open());
    assert_eq!(app.open_windows(), 0);
    // Nothing open: a new run returns at once.
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
}

#[test]
fn a_close_request_the_application_refuses_leaves_the_window_open() {
    let (app, test) = App::headless();
    let asked = Rc::new(RefCell::new(0));
    let count = Rc::clone(&asked);
    let window = app
        .open_window(
            options("Unsaved", 100.0, 100.0),
            move |_: &WindowCx<'_>, event| {
                if event == Event::CloseRequested {
                    *count.borrow_mut() += 1;
                }
                Handled::Yes
            },
        )
        .expect("valid options");
    let controller = test.windows()[0].clone();
    test.post(move || {
        controller.request_close();
    });
    assert_eq!(app.run(), Ok(RunEnd::Idle));
    assert_eq!(*asked.borrow(), 1);
    assert!(window.is_open());
    assert_eq!(app.open_windows(), 1);
}

#[test]
fn quit_ends_the_loop_with_windows_open() {
    let (app, test) = App::headless();
    let quitter = app.clone();
    let window = app
        .open_window(options("Q", 50.0, 50.0), move |_: &WindowCx<'_>, event| {
            if event == Event::Timer(TimerId(9)) {
                quitter.quit();
                // The loop is running: it may not be run again from here.
                assert_eq!(quitter.run(), Err(WindowError::Busy));
                assert!(quitter.is_running());
            }
            Handled::Yes
        })
        .expect("valid options");
    window.set_timer(TimerId(9), 10).expect("open");
    let controller = test.windows()[0].clone();
    test.post(move || {
        controller.fire_timers();
    });
    test.post(|| panic!("the loop ran past the quit"));
    assert_eq!(app.run(), Ok(RunEnd::Quit));
    assert!(window.is_open());
    assert!(!app.is_running());
    assert_eq!(test.pending_steps(), 1);
    // A quit asked between runs ends the next at once.
    app.quit();
    assert_eq!(app.run(), Ok(RunEnd::Quit));
}

#[test]
fn a_window_opened_from_inside_the_loop_keeps_it_running() {
    let (app, test) = App::headless();
    let (first, _) = logged(&app, options("First", 100.0, 100.0));
    let opened: Rc<RefCell<Vec<Window>>> = Rc::default();
    {
        let (app, test, opened) = (app.clone(), test.clone(), Rc::clone(&opened));
        let first = test.windows()[0].clone();
        test.clone().post(move || {
            let (second, _) = logged(&app, options("Second", 100.0, 100.0));
            opened.borrow_mut().push(second);
            first.request_close();
            let second = test.windows()[1].clone();
            test.post(move || {
                second.request_close();
            });
        });
    }
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
    assert!(!first.is_open());
    assert!(!opened.borrow()[0].is_open());
}

#[test]
fn a_child_window_never_hears_a_close_request_and_has_no_title() {
    let (window, headless) = Window::headless(
        WindowOptions::new(PhysicalSize::new(10, 10)),
        |_: &WindowCx<'_>, event| {
            assert_ne!(event, Event::CloseRequested);
            Handled::Yes
        },
    )
    .expect("valid options");
    assert_eq!(headless.request_close(), None);
    assert!(!window.is_app_window());
    assert_eq!(window.title(), None);
    assert_eq!(window.set_title("x"), Err(WindowError::NotAppWindow));
    assert_eq!(window.size_limits(), None);
    // A child closes itself as it would be closed under it.
    window.close();
    assert!(!window.is_open());
}

#[test]
fn the_title_can_change() {
    let (app, _test) = App::headless();
    let (window, _) = logged(&app, options("Untitled", 10.0, 10.0));
    assert!(window.is_app_window());
    window.set_title("Kit 1 \u{2014} Parabolic").expect("open");
    assert_eq!(window.title().as_deref(), Some("Kit 1 \u{2014} Parabolic"));
    window.set_title("cut\0here").expect("open");
    assert_eq!(window.title().as_deref(), Some("cut"));
    window.close();
    assert_eq!(window.set_title("late"), Err(WindowError::Closed));
}

#[test]
fn a_window_opens_at_its_logical_size_times_the_systems_scale() {
    let (app, test) = App::headless();
    assert!(test.set_system_scale(1.5));
    assert!(!test.set_system_scale(9.0));
    let (window, log) = logged(&app, options("Big", 400.0, 300.0));
    assert_eq!(window.size(), PhysicalSize::new(600, 450));
    assert_eq!(window.scale(), 1.5);
    assert_eq!(window.logical_size(), Size::new(400.0, 300.0));
    // Opening tells the handler nothing; the first expose draws it all.
    assert_eq!(take(&log), []);
    let headless = test.windows()[0].clone();
    assert!(headless.expose());
    let Event::Paint(damage) = take(&log)[0] else {
        panic!("no paint")
    };
    assert_eq!(damage.rects(), [PhysicalSize::new(600, 450).bounds()]);
}

#[test]
fn the_limits_hold_for_the_person_and_for_the_program() {
    let (app, test) = App::headless();
    test.set_system_scale(2.0);
    let (window, log) = logged(
        &app,
        options("Limits", 300.0, 200.0)
            .with_min_size(Size::new(200.0, 100.0))
            .with_max_size(Size::new(500.0, 400.0)),
    );
    assert_eq!(
        window.size_limits(),
        Some((PhysicalSize::new(400, 200), PhysicalSize::new(1000, 800)))
    );
    let headless = test.windows()[0].clone();
    assert_eq!(
        headless.drag_frame_to(PhysicalSize::new(100, 5000)),
        Some(PhysicalSize::new(400, 800))
    );
    assert_eq!(take(&log), [Event::Resized(PhysicalSize::new(400, 800))]);
    window.resize(PhysicalSize::new(5000, 10)).expect("open");
    assert_eq!(window.size(), PhysicalSize::new(1000, 200));
    // A window that opens larger than its maximum opens at the maximum.
    let (small, _) = logged(
        &app,
        options("Small", 900.0, 900.0).with_max_size(Size::new(100.0, 50.0)),
    );
    assert_eq!(small.size(), PhysicalSize::new(200, 100));
}

#[test]
fn a_fixed_window_keeps_its_size_against_the_person_only() {
    let (app, test) = App::headless();
    let (window, log) = logged(&app, options("Fixed", 120.0, 80.0).fixed());
    let headless = test.windows()[0].clone();
    assert_eq!(
        headless.drag_frame_to(PhysicalSize::new(300, 300)),
        Some(PhysicalSize::new(120, 80))
    );
    assert_eq!(take(&log), []);
    window.resize(PhysicalSize::new(240, 160)).expect("open");
    assert_eq!(window.size(), PhysicalSize::new(240, 160));
}

#[test]
fn moving_to_another_monitor_changes_the_scale_and_then_the_size() {
    let (app, test) = App::headless();
    let (window, log) = logged(
        &app,
        options("Move", 300.0, 200.0).with_max_size(Size::new(350.0, 350.0)),
    );
    let headless = test.windows()[0].clone();
    assert!(headless.move_to_monitor(1.5));
    assert_eq!(
        take(&log),
        [
            Event::ScaleChanged(1.5),
            Event::Resized(PhysicalSize::new(450, 300)),
        ]
    );
    assert_eq!(window.logical_size(), Size::new(300.0, 200.0));
    // At 2 the same logical size is 600 by 400, but at most 700 by 700.
    assert!(headless.move_to_monitor(2.0));
    assert_eq!(window.size(), PhysicalSize::new(600, 400));
    assert!(!headless.move_to_monitor(100.0));
    window.close();
    assert!(!headless.move_to_monitor(1.0));
}

#[test]
fn options_no_window_can_have_are_refused() {
    let (app, _test) = App::headless();
    let refused = app.open_window(
        options("Bad", 100.0, 100.0)
            .with_min_size(Size::new(200.0, 10.0))
            .with_max_size(Size::new(100.0, 100.0)),
        |_: &WindowCx<'_>, _| Handled::Yes,
    );
    assert!(matches!(refused, Err(WindowError::InvalidOptions(_))));
    assert_eq!(app.open_windows(), 0);
}

#[test]
fn the_system_application_is_win32_only() {
    let app = App::new();
    assert_eq!(app.is_ok(), cfg!(windows), "{app:?}");
    match app {
        Ok(app) => {
            assert!(!app.is_headless());
            assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
        }
        Err(error) => {
            assert!(matches!(error, WindowError::Unsupported(_)), "{error}");
        }
    }
}

/// A handler that takes drags of files carrying a `.wav` over its left half
/// only, and logs what it is told and what the drags carry.
fn sampler(app: &App) -> (Window, Log, Rc<RefCell<Vec<DragData>>>) {
    let log: Log = Rc::default();
    let seen: Rc<RefCell<Vec<DragData>>> = Rc::default();
    let (sink, carried) = (Rc::clone(&log), Rc::clone(&seen));
    let window = app
        .open_window(
            options("Sampler", 200.0, 100.0).with_file_drops(),
            move |cx: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                let data = cx.drag_data();
                if let Some(data) = &data {
                    carried.borrow_mut().push((**data).clone());
                }
                let takes = |pointer: manguebyte_window::PointerEvent| {
                    let wav = data.as_ref().is_some_and(|data| {
                        data.files
                            .iter()
                            .any(|file| file.extension().is_some_and(|ext| ext == "wav"))
                    });
                    wav && pointer.position.x < 100.0
                };
                match event {
                    Event::DragEnter(pointer) | Event::DragOver(pointer) | Event::Drop(pointer)
                        if takes(pointer) =>
                    {
                        Handled::Yes
                    }
                    Event::DragEnter(_) | Event::DragOver(_) | Event::Drop(_) => Handled::No,
                    _ => Handled::Yes,
                }
            },
        )
        .expect("valid options");
    (window, log, seen)
}

fn kinds(log: &Log) -> Vec<&'static str> {
    take(log)
        .into_iter()
        .map(|event| match event {
            Event::DragEnter(_) => "enter",
            Event::DragOver(_) => "over",
            Event::DragLeave => "leave",
            Event::Drop(_) => "drop",
            _ => "other",
        })
        .collect()
}

#[test]
fn a_drop_of_files_reaches_the_handler_with_its_paths_and_position() {
    let (app, test) = App::headless();
    let (window, log, seen) = sampler(&app);
    assert!(window.accepts_drops());
    let headless = test.windows()[0].clone();
    let kit = DragData::files(["C:/kits/kick.wav", "C:/kits/snare.wav"]);

    assert_eq!(headless.drag_enter(at(10, 10), kit.clone()), Handled::Yes);
    assert_eq!(
        headless.drag_over(at(150, 10)),
        Handled::No,
        "the right half"
    );
    assert_eq!(headless.drag_over(at(50, 50)), Handled::Yes);
    assert_eq!(headless.drop_at(at(40, 60)), Handled::Yes);
    let events = take(&log);
    let Event::Drop(pointer) = events[3] else {
        panic!("{events:?}")
    };
    assert_eq!(pointer.position, Point::new(40.0, 60.0));
    assert_eq!(events.len(), 4, "{events:?}");
    // Every event of the drag saw what it carries; after it, nothing.
    assert_eq!(*seen.borrow(), vec![kit; 4]);
    assert!(window.drag_data().is_none());
}

#[test]
fn a_drag_that_leaves_or_is_refused_is_never_dropped() {
    let (app, test) = App::headless();
    let (_window, log, _) = sampler(&app);
    let headless = test.windows()[0].clone();

    // Text only: the handler hears it and refuses it.
    assert_eq!(
        headless.drag_enter(at(10, 10), DragData::text("hello")),
        Handled::No
    );
    headless.drag_leave();
    assert_eq!(kinds(&log), ["enter", "leave"]);
    // A drag that carries nothing is refused without asking.
    assert_eq!(
        headless.drag_enter(at(10, 10), DragData::default()),
        Handled::No
    );
    assert_eq!(headless.drag_over(at(11, 10)), Handled::No);
    assert_eq!(headless.drop_at(at(11, 10)), Handled::No);
    assert_eq!(kinds(&log), [] as [&str; 0]);
    // A refused drag dropped anyway (OLE would not) is still offered.
    assert_eq!(
        headless.drop_files(at(150, 10), DragData::files(["a.wav"])),
        Handled::No
    );
    assert_eq!(kinds(&log), ["enter", "leave"]);
    // A new drag without a leave ends the old one first.
    headless.drag_enter(at(10, 10), DragData::files(["a.wav"]));
    headless.drag_enter(at(10, 10), DragData::files(["b.wav"]));
    assert_eq!(kinds(&log), ["enter", "leave", "enter"]);
}

#[test]
fn a_window_that_takes_no_drops_hears_none() {
    let (app, test) = App::headless();
    let (window, log) = logged(&app, options("No drops", 50.0, 50.0));
    assert!(!window.accepts_drops());
    let headless = test.windows()[0].clone();
    assert_eq!(
        headless.drop_files(at(1, 1), DragData::files(["x.wav"])),
        Handled::No
    );
    assert_eq!(take(&log), []);
}

#[test]
fn a_busy_handler_refuses_a_drag_and_hears_it_once_it_is_free() {
    let (app, test) = App::headless();
    let log: Log = Rc::default();
    let controller: Rc<RefCell<Option<manguebyte_window::Headless>>> = Rc::default();
    let answers = Rc::new(RefCell::new(Vec::new()));
    let window = {
        let (sink, controller, answers) =
            (Rc::clone(&log), Rc::clone(&controller), Rc::clone(&answers));
        app.open_window(
            options("Busy", 100.0, 100.0).with_file_drops(),
            move |_: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                if event == Event::Timer(TimerId(1)) {
                    // A drag comes in while the handler runs (a nested loop).
                    let window = controller.borrow().clone().expect("set");
                    answers
                        .borrow_mut()
                        .push(window.drag_enter(at(5, 5), DragData::files(["late.wav"])));
                }
                Handled::Yes
            },
        )
        .expect("valid options")
    };
    let headless = test.windows()[0].clone();
    *controller.borrow_mut() = Some(headless.clone());
    window.set_timer(TimerId(1), 10).expect("open");
    headless.input(RawInput::Timer(TimerId(1)));
    assert_eq!(*answers.borrow(), [Handled::No]);
    assert_eq!(take(&log), [Event::Timer(TimerId(1))]);
    // Free again: the next move tells it the drag came in.
    assert_eq!(headless.drag_over(at(6, 6)), Handled::Yes);
    let events = take(&log);
    assert!(matches!(events[..], [Event::DragEnter(_)]), "{events:?}");
    assert_eq!(headless.drop_at(at(6, 6)), Handled::Yes);
    assert!(matches!(take(&log)[..], [Event::Drop(_)]));
}

#[test]
fn a_child_window_takes_drops_only_when_asked() {
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    let (window, headless) = Window::headless(
        WindowOptions::new(PhysicalSize::new(20, 20)).with_file_drops(),
        move |_: &WindowCx<'_>, event| {
            sink.borrow_mut().push(event);
            Handled::Yes
        },
    )
    .expect("valid options");
    assert!(window.accepts_drops());
    assert_eq!(
        headless.drop_files(at(1, 1), DragData::files(["one.wav"])),
        Handled::Yes
    );
    assert_eq!(kinds(&log), ["enter", "drop"]);
    // Closed, it takes nothing.
    window.close();
    take(&log);
    assert_eq!(
        headless.drop_files(at(1, 1), DragData::files(["two.wav"])),
        Handled::No
    );
    assert_eq!(take(&log), []);
}

#[test]
fn dialogs_answer_what_the_test_scripted_and_record_what_was_asked() {
    let (app, test) = App::headless();
    let (window, _) = logged(&app, options("Dialogs", 100.0, 100.0));
    let headless = test.windows()[0].clone();
    let samples = FileDialog::new()
        .with_title("Add samples")
        .with_filter("Audio", ["wav", "flac"])
        .multiple();
    headless.answer_next_dialog(Some(vec!["a.wav".into(), "b.flac".into()]));
    headless.answer_next_dialog(Some(vec!["one.wav".into(), "two.wav".into()]));
    headless.answer_next_dialog(None);
    headless.answer_next_dialog(Some(vec!["preset.json".into()]));
    assert_eq!(
        window.open_files(&samples),
        Ok(Some(vec![PathBuf::from("a.wav"), PathBuf::from("b.flac")]))
    );
    // Without `multiple`, the first only.
    assert_eq!(
        window.open_files(&FileDialog::new()),
        Ok(Some(vec![PathBuf::from("one.wav")]))
    );
    assert_eq!(window.pick_folder(&FileDialog::new()), Ok(None));
    let save = FileDialog::new()
        .with_filter("Presets", ["json"])
        .with_file_name("Init.json");
    assert_eq!(
        window.save_file(&save),
        Ok(Some(PathBuf::from("preset.json")))
    );
    // Nothing scripted is a cancel.
    assert_eq!(window.save_file(&save), Ok(None));
    let asked: Vec<(DialogKind, FileDialog)> = headless
        .take_dialogs()
        .into_iter()
        .map(|request| (request.kind, request.dialog))
        .collect();
    assert_eq!(
        asked,
        [
            (DialogKind::OpenFiles, samples),
            (DialogKind::OpenFiles, FileDialog::new()),
            (DialogKind::PickFolder, FileDialog::new()),
            (DialogKind::SaveFile, save.clone()),
            (DialogKind::SaveFile, save),
        ]
    );
    window.close();
    assert_eq!(
        window.open_files(&FileDialog::new()),
        Err(WindowError::Closed)
    );
}

/// A dialog shown from inside the handler runs a loop of its own: what
/// reaches the window meanwhile waits for the handler, merged, and a
/// second dialog cannot open over the first.
#[test]
fn input_during_a_dialog_waits_for_the_handler_that_opened_it() {
    let (app, test) = App::headless();
    let log: Log = Rc::default();
    let results = Rc::new(RefCell::new(Vec::new()));
    let window = {
        let (sink, results) = (Rc::clone(&log), Rc::clone(&results));
        app.open_window(
            options("Modal", 100.0, 100.0),
            move |cx: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                if event == Event::Timer(TimerId(2)) {
                    results
                        .borrow_mut()
                        .push(cx.open_files(&FileDialog::new().with_title("Load")));
                }
                Handled::Yes
            },
        )
        .expect("valid options")
    };
    window.set_timer(TimerId(2), 10).expect("open");
    let headless = test.windows()[0].clone();
    let during = headless.clone();
    let window_dialogs = window.dialogs();
    headless.on_next_dialog(move |request| {
        assert_eq!(request.dialog.title.as_deref(), Some("Load"));
        // The person moves the pointer and the timer ticks while the
        // dialog is open; the handler is the dialog's caller.
        for x in 0..50 {
            during.input(RawInput::PointerMove {
                at: at(x, 1),
                modifiers: manguebyte_window::Modifiers::NONE,
            });
            during.input(RawInput::Timer(TimerId(2)));
        }
        assert!(during.handler_running());
        assert_eq!(
            window_dialogs.pick_folder(&FileDialog::new()),
            Err(WindowError::Busy)
        );
        Some(vec!["loop.wav".into()])
    });
    headless.input(RawInput::Timer(TimerId(2)));
    assert_eq!(
        results.borrow()[0],
        Ok(Some(vec![PathBuf::from("loop.wav")]))
    );
    // The 50 moves and ticks came back as one each, after the dialog; the
    // tick that waited opened a second dialog, which nothing answered.
    let events = take(&log);
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(matches!(events[1], Event::PointerMove(_)));
    assert_eq!(events[2], Event::Timer(TimerId(2)));
    assert_eq!(results.borrow()[1], Ok(None));
    assert_eq!(headless.take_dialogs().len(), 2);
}

#[test]
fn a_headless_application_is_gone_with_its_last_handle() {
    let (app, test) = App::headless();
    let (window, _) = logged(&app, options("Gone", 10.0, 10.0));
    drop(app);
    // The window keeps the application alive.
    assert_eq!(test.windows().len(), 1);
    drop(window);
    assert!(test.windows().is_empty());
    assert!(!test.post(|| {}));
    assert!(!test.set_system_scale(2.0));
    let _: HeadlessApp = test;
}
