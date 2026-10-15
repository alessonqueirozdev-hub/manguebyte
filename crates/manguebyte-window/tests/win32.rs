//! The Win32 backend against real windows: a top-level window owned by the
//! test plays the host's window, and the window under test is embedded in
//! it. Input is sent with `SendMessageW`; pixels are read back from the
//! window's surface.
//!
//! A test that reads pixels fails when this session cannot read windows
//! back, unless `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is set (CI never sets
//! it); then it says so on stderr and skips those checks.

#![cfg(windows)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use manguebyte_gfx::{Color, IRect, Pixmap, Point};
use manguebyte_window::win32::testing::{self, Message, ReadBack, Received, TopLevel};
use manguebyte_window::{
    Button, Damage, Event, Handled, Key, ParentWindow, PhysicalPoint, PhysicalSize, PointerEvent,
    TimerId, Window, WindowCx, WindowError, WindowOptions,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DLGC_WANTALLKEYS, DLGC_WANTARROWS, DLGC_WANTCHARS, DLGC_WANTTAB, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
    WM_MOUSEWHEEL, WM_SYSKEYDOWN, WM_SYSKEYUP,
};

type Log = Rc<RefCell<Vec<Event>>>;

fn size(width: u32, height: u32) -> PhysicalSize {
    PhysicalSize::new(width, height)
}

fn at(x: i32, y: i32) -> PhysicalPoint {
    PhysicalPoint::new(x, y)
}

/// A window inside `parent` whose handler logs every event.
fn logged(parent: &TopLevel, options: WindowOptions) -> (Window, Log) {
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    let window = Window::open(parent.parent(), options, move |_: &WindowCx<'_>, event| {
        sink.borrow_mut().push(event);
        Handled::Yes
    })
    .expect("a child window");
    (window, log)
}

fn take(log: &Log) -> Vec<Event> {
    log.borrow_mut().drain(..).collect()
}

fn handle(window: &Window) -> usize {
    window.raw_handle().expect("open")
}

/// Every pixel differs from its neighbours; some are translucent.
fn pattern(width: u32, height: u32) -> Pixmap {
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            let a = if (x + y) % 5 == 0 { 128 } else { 255 };
            let r = ((x * 9 + y) % 256) as u8;
            let g = ((y * 7 + 3) % 256) as u8;
            let b = ((x * 3 + y * 5) % 256) as u8;
            let scale = |c: u8| (u32::from(c) * a / 255) as u8;
            data.extend_from_slice(&[scale(r), scale(g), scale(b), a as u8]);
        }
    }
    Pixmap::from_premultiplied(width, height, data).expect("premultiplied")
}

/// What GDI shows for premultiplied pixels: the colour over black, opaque.
fn opaque(pixels: &Pixmap) -> Pixmap {
    let mut data = pixels.data().to_vec();
    for pixel in data.chunks_exact_mut(4) {
        pixel[3] = 255;
    }
    Pixmap::from_premultiplied(pixels.width(), pixels.height(), data).expect("opaque")
}

fn first_mismatch(got: &Pixmap, want: &Pixmap) -> Option<String> {
    if (got.width(), got.height()) != (want.width(), want.height()) {
        return Some(format!(
            "read {}x{}, expected {}x{}",
            got.width(),
            got.height(),
            want.width(),
            want.height()
        ));
    }
    for y in 0..want.height() {
        for x in 0..want.width() {
            if got.pixel(x, y) != want.pixel(x, y) {
                return Some(format!(
                    "({x}, {y}): read {:?}, expected {:?}",
                    got.pixel(x, y),
                    want.pixel(x, y)
                ));
            }
        }
    }
    None
}

/// Checks that the pixels on the window's surface are `want`, exactly.
fn assert_shows(window: &Window, want: &Pixmap) {
    let got = testing::read_back(&window.cx(), ReadBack::Screen).expect("our window");
    assert_eq!(first_mismatch(&got, want), None, "the window's surface");
}

#[test]
fn presented_pixels_read_back_exactly() {
    let parent = TopLevel::new(200, 150).expect("a desktop");
    let presents = Rc::new(Cell::new(0));
    let frame = pattern(120, 80);
    let window = {
        let (presents, frame) = (Rc::clone(&presents), frame.clone());
        Window::open(
            parent.parent(),
            WindowOptions::new(size(120, 80)),
            move |window: &WindowCx<'_>, event| {
                if let Event::Paint(damage) = event {
                    assert_eq!(damage, Damage::of(IRect::new(0, 0, 120, 80)));
                    window.present(&frame, damage.rects()).expect("open");
                    presents.set(presents.get() + 1);
                }
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    assert_eq!(testing::parent_of(handle(&window)), parent.handle());
    // The first paint asks for the whole window, once.
    assert!(testing::update(handle(&window)));
    testing::pump(Duration::from_millis(20));
    assert_eq!(presents.get(), 1);
    assert_eq!(window.presented_pixels(), 120 * 80);

    let want = opaque(&frame);
    // The window answers WM_PRINTCLIENT from its back buffer.
    let printed = testing::read_back(&window.cx(), ReadBack::PrintClient).expect("our window");
    assert_eq!(first_mismatch(&printed, &want), None, "WM_PRINTCLIENT");
    if testing::require_read_back(&parent, IRect::new(130, 90, 60, 50)) {
        assert_shows(&window, &want);
    }
}

#[test]
fn only_the_damaged_rectangle_changes() {
    let parent = TopLevel::new(100, 80).expect("a desktop");
    let (window, _log) = logged(&parent, WindowOptions::new(size(64, 48)));
    let mut red = Pixmap::new(64, 48).expect("small");
    red.fill(Color::rgb(255, 0, 0));
    window.present_all(&red).expect("open");
    let blue = pattern(64, 48);
    let rect = IRect::new(5, 7, 20, 9);
    let before = window.presented_pixels();
    window.present(&blue, &[rect]).expect("open");
    assert_eq!(window.presented_pixels() - before, 180);

    let mut want = opaque(&red);
    want.copy_from(&opaque(&blue), rect, rect.x, rect.y);
    let printed = testing::read_back(&window.cx(), ReadBack::PrintClient).expect("our window");
    assert_eq!(first_mismatch(&printed, &want), None, "WM_PRINTCLIENT");
    if testing::require_read_back(&parent, IRect::new(70, 50, 30, 30)) {
        assert_shows(&window, &want);
    }
}

/// A read-back that only asked the window for its back buffer would pass
/// with a broken present. Here something else paints over the window's
/// surface: the pixel read-back sees it, and only the `WM_PRINTCLIENT`
/// answer still shows the back buffer.
#[test]
fn the_read_backs_read_the_surface_not_the_back_buffer() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    if !testing::require_read_back(&parent, IRect::new(60, 60, 30, 30)) {
        return;
    }
    let (window, _log) = logged(&parent, WindowOptions::new(size(40, 30)));
    testing::update(handle(&window));
    let mut frame = Pixmap::new(40, 30).expect("small");
    frame.fill(Color::rgb(10, 200, 30));
    window.present_all(&frame).expect("open");
    assert_shows(&window, &frame);

    assert!(testing::draw_over(
        handle(&window),
        IRect::new(0, 0, 40, 30),
        [0, 0, 255]
    ));
    let mut blue = Pixmap::new(40, 30).expect("small");
    blue.fill(Color::rgb(0, 0, 255));
    assert_shows(&window, &blue);
    let printed = testing::read_back(&window.cx(), ReadBack::PrintClient).expect("our window");
    assert_eq!(first_mismatch(&printed, &frame), None, "WM_PRINTCLIENT");
}

#[test]
fn an_expose_is_answered_from_the_back_buffer_without_the_handler() {
    let parent = TopLevel::new(80, 60).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(40, 30)));
    testing::update(handle(&window));
    assert_eq!(
        take(&log),
        [Event::Paint(Damage::of(IRect::new(0, 0, 40, 30)))]
    );
    let frame = pattern(40, 30);
    window.present_all(&frame).expect("open");
    // Hide and show again: the system repaints, the handler is not asked.
    window.set_visible(false).expect("open");
    window.set_visible(true).expect("open");
    testing::update(handle(&window));
    testing::pump(Duration::from_millis(20));
    assert!(
        take(&log)
            .iter()
            .all(|event| !matches!(event, Event::Paint(_))),
        "the handler was asked to repaint"
    );
    let printed = testing::read_back(&window.cx(), ReadBack::PrintClient).expect("our window");
    assert_eq!(first_mismatch(&printed, &opaque(&frame)), None);
    // The expose drew the back buffer on the window's surface.
    if testing::require_read_back(&parent, IRect::new(45, 35, 30, 20)) {
        assert_shows(&window, &opaque(&frame));
    }
}

/// An animation or a widget that changes while it is drawn invalidates
/// from inside its paint: that rectangle must be painted again later.
#[test]
fn an_invalidate_made_while_painting_is_painted_later() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let paints: Rc<RefCell<Vec<Damage>>> = Rc::default();
    let window = {
        let paints = Rc::clone(&paints);
        Window::open(
            parent.parent(),
            WindowOptions::new(size(40, 30)),
            move |window: &WindowCx<'_>, event| {
                if let Event::Paint(damage) = event {
                    let first = paints.borrow().is_empty();
                    paints.borrow_mut().push(damage);
                    if first {
                        window.invalidate(IRect::new(0, 0, 5, 5));
                    }
                }
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    testing::update(handle(&window));
    assert!(testing::pump_until(Duration::from_secs(2), || paints
        .borrow()
        .len()
        >= 2));
    testing::pump(Duration::from_millis(20));
    assert_eq!(
        *paints.borrow(),
        [
            Damage::of(IRect::new(0, 0, 40, 30)),
            Damage::of(IRect::new(0, 0, 5, 5))
        ]
    );
}

/// The control for the test above: the same invalidation made outside the
/// handler is painted once.
#[test]
fn an_invalidate_made_outside_painting_is_painted() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(40, 30)));
    testing::update(handle(&window));
    window.invalidate(IRect::new(0, 0, 5, 5));
    testing::pump_until(Duration::from_secs(2), || log.borrow().len() >= 2);
    testing::pump(Duration::from_millis(20));
    assert_eq!(
        take(&log),
        [
            Event::Paint(Damage::of(IRect::new(0, 0, 40, 30))),
            Event::Paint(Damage::of(IRect::new(0, 0, 5, 5)))
        ]
    );
}

fn pointer(x: i32, y: i32, scale: f64, shift: bool) -> PointerEvent {
    PointerEvent {
        position: Point::new((f64::from(x) / scale) as f32, (f64::from(y) / scale) as f32),
        physical: at(x, y),
        modifiers: manguebyte_window::Modifiers {
            shift,
            ..manguebyte_window::Modifiers::NONE
        },
    }
}

/// Drops the Alt and Windows-key states, which come from the real
/// keyboard rather than from the message.
fn without_keyboard_modifiers(event: Event) -> Event {
    let strip = |mut p: PointerEvent| {
        p.modifiers.alt = false;
        p.modifiers.meta = false;
        p
    };
    match event {
        Event::PointerDown {
            pointer,
            button,
            clicks,
        } => Event::PointerDown {
            pointer: strip(pointer),
            button,
            clicks,
        },
        Event::PointerMove(pointer) => Event::PointerMove(strip(pointer)),
        Event::PointerUp { pointer, button } => Event::PointerUp {
            pointer: strip(pointer),
            button,
        },
        Event::Wheel { pointer, delta } => Event::Wheel {
            pointer: strip(pointer),
            delta,
        },
        other => other,
    }
}

#[test]
fn input_arrives_with_exact_coordinates_at_scale_1_and_1_5() {
    for scale in [1.0, 1.5] {
        let parent = TopLevel::new(400, 300).expect("a desktop");
        let (window, log) = logged(
            &parent,
            WindowOptions::new(size(300, 150)).with_scale(scale),
        );
        let hwnd = handle(&window);
        testing::send(
            hwnd,
            Message::ButtonDown {
                button: Button::Primary,
                at: at(15, 30),
                shift: true,
                control: false,
            },
        )
        .expect("ours");
        // The press took the keyboard and the pointer.
        assert!(testing::has_system_focus(hwnd) && window.has_focus());
        assert!(testing::has_system_capture(hwnd) && window.has_capture());
        testing::send(
            hwnd,
            Message::MouseMove {
                at: at(-7, 301),
                shift: false,
                control: false,
            },
        );
        testing::send(
            hwnd,
            Message::ButtonUp {
                button: Button::Primary,
                at: at(7, 9),
            },
        );
        assert!(!testing::has_system_capture(hwnd) && !window.has_capture());
        testing::send(
            hwnd,
            Message::Wheel {
                at: at(3, 4),
                delta: 120,
                horizontal: false,
            },
        );
        testing::send(
            hwnd,
            Message::Wheel {
                at: at(3, 4),
                delta: -60,
                horizontal: true,
            },
        );
        testing::send(hwnd, Message::MouseLeave);
        let events: Vec<Event> = take(&log)
            .into_iter()
            .filter(|event| !matches!(event, Event::Paint(_)))
            .map(without_keyboard_modifiers)
            .collect();
        assert_eq!(
            events,
            [
                Event::Focus(true),
                Event::PointerDown {
                    pointer: pointer(15, 30, scale, true),
                    button: Button::Primary,
                    clicks: 1
                },
                Event::PointerMove(pointer(-7, 301, scale, false)),
                Event::PointerUp {
                    pointer: pointer(7, 9, scale, false),
                    button: Button::Primary
                },
                Event::Wheel {
                    pointer: pointer(3, 4, scale, false),
                    delta: Point::new(0.0, 1.0)
                },
                Event::Wheel {
                    pointer: pointer(3, 4, scale, false),
                    delta: Point::new(-0.5, 0.0)
                },
                Event::PointerLeave,
            ],
            "scale {scale}"
        );
    }
    // The logical values at 1.5, written out.
    assert_eq!(pointer(15, 30, 1.5, false).position, Point::new(10.0, 20.0));
    assert_eq!(
        pointer(-7, 301, 1.5, false).position,
        Point::new(-4.6666665, 200.66667)
    );
}

#[test]
fn keys_and_text_arrive_from_their_messages() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(50, 50)));
    let hwnd = handle(&window);
    testing::send(hwnd, Message::SetFocus);
    for message in [
        Message::KeyDown {
            vk: 0x25,
            repeat: false,
            system: false,
        },
        Message::KeyDown {
            vk: 0x25,
            repeat: true,
            system: false,
        },
        Message::KeyUp { vk: 0x25 },
        Message::KeyDown {
            vk: 0x74,
            repeat: false,
            system: true,
        },
        Message::Char(0xE9),
        Message::Char(0xD83D),
        Message::Char(0xDE00),
        Message::Char(0x0D),
        Message::KillFocus,
    ] {
        testing::send(hwnd, message).expect("ours");
    }
    let events: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| !matches!(event, Event::Paint(_)))
        .collect();
    let keys: Vec<(Key, bool, bool)> = events
        .iter()
        .filter_map(|event| match event {
            Event::Key { key, repeat } => Some((key.key, key.pressed, *repeat)),
            _ => None,
        })
        .collect();
    assert_eq!(
        keys,
        [
            (Key::Left, true, false),
            (Key::Left, true, true),
            (Key::Left, false, false),
            (Key::Function(5), true, false),
        ]
    );
    let other: Vec<Event> = events
        .into_iter()
        .filter(|event| !matches!(event, Event::Key { .. }))
        .collect();
    assert_eq!(
        other,
        [
            Event::Focus(true),
            Event::Text('é'),
            Event::Text('😀'),
            Event::Focus(false)
        ]
    );
}

#[test]
fn messages_sent_from_inside_the_handler_wait_for_it() {
    let parent = TopLevel::new(300, 200).expect("a desktop");
    let depth = Rc::new(Cell::new(0));
    let deepest = Rc::new(Cell::new(0));
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let window = {
        let (depth, deepest, log) = (Rc::clone(&depth), Rc::clone(&deepest), Rc::clone(&log));
        Window::open(
            parent.parent(),
            WindowOptions::new(size(100, 100)).without_focus_on_click(),
            move |window: &WindowCx<'_>, event| {
                depth.set(depth.get() + 1);
                deepest.set(deepest.get().max(depth.get()));
                if !matches!(event, Event::Paint(_)) {
                    log.borrow_mut().push(format!("{event:?}"));
                }
                if let Event::PointerDown { .. } = event {
                    let hwnd = window.raw_handle().expect("open");
                    // Each of these reaches the window procedure at once.
                    testing::send(
                        hwnd,
                        Message::MouseMove {
                            at: at(40, 41),
                            shift: false,
                            control: true,
                        },
                    )
                    .expect("ours");
                    window.resize(PhysicalSize::new(150, 120)).expect("open");
                    testing::send(hwnd, Message::Char(u16::from(b'k'))).expect("ours");
                    window.set_timer(TimerId(4), 1000).expect("open");
                    testing::send(hwnd, Message::Timer(4)).expect("ours");
                    log.borrow_mut().push("returning".to_string());
                }
                depth.set(depth.get() - 1);
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    let hwnd = handle(&window);
    testing::send(
        hwnd,
        Message::ButtonDown {
            button: Button::Primary,
            at: at(10, 12),
            shift: false,
            control: false,
        },
    )
    .expect("ours");
    assert_eq!(deepest.get(), 1, "the handler was re-entered");
    let log = log.borrow();
    let names: Vec<&str> = log
        .iter()
        .map(|line| line.split([' ', '(']).next().unwrap_or(""))
        .collect();
    assert_eq!(
        names,
        [
            "PointerDown",
            "returning",
            "PointerMove",
            "Resized",
            "Text",
            "Timer"
        ],
        "{log:#?}"
    );
    assert_eq!(log[3], "Resized(PhysicalSize { width: 150, height: 120 })");
    assert_eq!(log[4], "Text('k')");
    assert_eq!(log[5], "Timer(TimerId(4))");
    assert!(log[2].contains("x: 40, y: 41") && log[2].contains("control: true"));
    assert_eq!(window.deferred_events(), 4);
    assert_eq!(window.size(), size(150, 120));
}

#[test]
fn timers_tick_on_the_window_and_never_after_they_are_killed() {
    let parent = TopLevel::new(50, 50).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(20, 20)));
    let ticks = |log: &Log, id: u32| {
        log.borrow()
            .iter()
            .filter(|event| **event == Event::Timer(TimerId(id)))
            .count()
    };
    window.set_timer(TimerId(0), 10).expect("open");
    window.set_timer(TimerId(9), 15).expect("open");
    assert!(testing::pump_until(Duration::from_secs(5), || {
        ticks(&log, 0) >= 3 && ticks(&log, 9) >= 3
    }));
    assert!(window.kill_timer(TimerId(0)));
    assert!(window.kill_timer(TimerId(9)));
    let after_kill = log.borrow().len();
    testing::pump(Duration::from_millis(120));
    // A tick sent by hand for a killed timer, and for one never set.
    testing::send(handle(&window), Message::Timer(0)).expect("ours");
    testing::send(handle(&window), Message::Timer(77)).expect("ours");
    assert_eq!(log.borrow().len(), after_kill);
}

#[test]
fn the_host_resizing_the_window_is_reported_with_the_new_damage() {
    let parent = TopLevel::new(300, 300).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(100, 60)));
    let hwnd = handle(&window);
    testing::update(hwnd);
    take(&log);
    assert!(testing::resize_as_host(hwnd, size(130, 70)));
    assert_eq!(window.size(), size(130, 70));
    testing::update(hwnd);
    let mut damage = Damage::new();
    damage.add(IRect::new(100, 0, 30, 70));
    damage.add(IRect::new(0, 60, 100, 10));
    assert_eq!(
        take(&log),
        [Event::Resized(size(130, 70)), Event::Paint(damage)]
    );
}

/// On a thread that is per-monitor DPI aware, the window's scale is the
/// DPI the system gives it over 96, and a DPI notice that changes nothing
/// makes no event. (The DPI here is the monitor's: 96 on a monitor at
/// 100 %, so this checks a scale other than 1 only on a scaled monitor.)
#[test]
fn a_per_monitor_aware_window_takes_the_monitors_dpi() {
    let _aware =
        testing::PerMonitorDpi::enter().expect("per-monitor awareness v2 (Windows 10 1703 on)");
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(10, 10)));
    let dpi = testing::window_dpi(handle(&window)).expect("GetDpiForWindow");
    eprintln!("the window's DPI on this thread: {dpi}");
    let scale = f64::from(dpi) / 96.0;
    assert_eq!(window.scale(), scale);
    // Positions arrive divided by the system's scale.
    testing::send(
        handle(&window),
        Message::MouseMove {
            at: at(25, 50),
            shift: false,
            control: false,
        },
    )
    .expect("ours");
    let moves: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| matches!(event, Event::PointerMove(_)))
        .map(without_keyboard_modifiers)
        .collect();
    assert_eq!(moves, [Event::PointerMove(pointer(25, 50, scale, false))]);
    testing::send(handle(&window), Message::DpiChangedAfterParent).expect("ours");
    assert!(
        take(&log)
            .iter()
            .all(|event| !matches!(event, Event::ScaleChanged(_)))
    );
    window.set_host_scale(Some(3.0)).expect("valid");
    window.set_host_scale(None).expect("valid");
    assert_eq!(window.scale(), f64::from(dpi) / 96.0);
    let changes: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| matches!(event, Event::ScaleChanged(_)))
        .collect();
    assert_eq!(
        changes,
        [
            Event::ScaleChanged(3.0),
            Event::ScaleChanged(f64::from(dpi) / 96.0)
        ]
    );
    drop(window);
    drop(parent);
}

#[test]
fn the_scale_is_the_systems_unless_the_host_gives_one() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(10, 10)));
    // A test binary declares no DPI awareness, so the system reports 96.
    assert_eq!(window.scale(), 1.0);
    testing::send(handle(&window), Message::DpiChangedAfterParent).expect("ours");
    window.set_host_scale(Some(1.5)).expect("valid");
    assert_eq!(window.scale(), 1.5);
    assert_eq!(window.logical_size().width, (10.0f64 / 1.5) as f32);
    let events: Vec<Event> = take(&log)
        .into_iter()
        .filter(|event| !matches!(event, Event::Paint(_)))
        .collect();
    assert_eq!(events, [Event::ScaleChanged(1.5)]);
}

#[test]
fn parents_that_are_not_windows_of_this_process_are_refused() {
    let nothing = |_: &WindowCx<'_>, _| Handled::No;
    for bad in [
        1usize,
        2,
        0xDEAD_BEEF,
        usize::MAX,
        testing::desktop_window(),
    ] {
        let parent = ParentWindow::win32(bad).expect("not null");
        let result = Window::open(parent, WindowOptions::new(size(10, 10)), nothing);
        assert!(
            matches!(result, Err(WindowError::InvalidParent(_))),
            "{bad:#x}: {result:?}"
        );
    }
    let gone = TopLevel::new(10, 10).expect("a desktop");
    let parent = gone.parent();
    gone.destroy();
    assert!(matches!(
        Window::open(parent, WindowOptions::new(size(10, 10)), nothing),
        Err(WindowError::InvalidParent(_))
    ));
}

#[test]
fn a_panicking_handler_does_not_unwind_into_the_system() {
    let parent = TopLevel::new(50, 50).expect("a desktop");
    let window = Window::open(
        parent.parent(),
        WindowOptions::new(size(20, 20)),
        |_: &WindowCx<'_>, event| {
            if let Event::Text(_) = event {
                panic!("a handler bug");
            }
            Handled::Yes
        },
    )
    .expect("a child window");
    let hwnd = handle(&window);
    assert_eq!(testing::send(hwnd, Message::Char(u16::from(b'a'))), Some(0));
    assert!(window.is_poisoned());
    // The window still answers the system.
    testing::send(hwnd, Message::Char(u16::from(b'b'))).expect("ours");
    testing::update(hwnd);
    let mut frame = Pixmap::new(20, 20).expect("small");
    frame.fill(Color::rgb(1, 2, 3));
    window.present_all(&frame).expect("open");
    let printed = testing::read_back(&window.cx(), ReadBack::PrintClient).expect("our window");
    assert_eq!(
        printed.pixel(19, 19).expect("inside").to_array(),
        [1, 2, 3, 255]
    );
    // A poisoned window passes the keys it can no longer use on.
    parent.take_received();
    testing::send(
        hwnd,
        Message::KeyDown {
            vk: 0x51,
            repeat: false,
            system: false,
        },
    );
    assert_eq!(pairs(&parent.take_received()), [(WM_KEYDOWN, 0x51)]);
}

/// The message and `WPARAM` of what reached the parent.
fn pairs(received: &[Received]) -> Vec<(u32, usize)> {
    received.iter().map(|r| (r.message, r.wparam)).collect()
}

fn key_down(vk: u16) -> Message {
    Message::KeyDown {
        vk,
        repeat: false,
        system: false,
    }
}

/// Keys, characters and wheel turns the handler does not use reach the
/// host's window; those it uses do not.
#[test]
fn what_the_handler_does_not_use_goes_on_to_the_host() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let window = Window::open(
        parent.parent(),
        WindowOptions::new(size(50, 50)),
        |_: &WindowCx<'_>, event| match event {
            Event::Key { key, .. } if key.key == Key::Character(' ') => Handled::Yes,
            Event::Text('x') => Handled::Yes,
            Event::Wheel { delta, .. } if delta.y > 0.0 => Handled::Yes,
            _ => Handled::No,
        },
    )
    .expect("a child window");
    let hwnd = handle(&window);
    testing::send(hwnd, Message::SetFocus);
    let wheel = |delta: i16| Message::Wheel {
        at: at(3, 3),
        delta,
        horizontal: false,
    };
    for message in [
        key_down(0x20),
        key_down(0x51),
        Message::Char(u16::from(b'q')),
        Message::KeyUp { vk: 0x51 },
        Message::Char(u16::from(b'x')),
        wheel(120),
        wheel(-120),
        // Enter is declined, so the control character it types goes too.
        key_down(0x0D),
        Message::Char(0x0D),
        // Space is used, so the control character after it stays.
        key_down(0x20),
        Message::Char(0x09),
        Message::Char(0xD83D),
        Message::Char(0xDE00),
        // Alt+Q, without the Alt and F10 releases that would start the
        // test parent's menu loop.
        Message::KeyDown {
            vk: 0x51,
            repeat: false,
            system: true,
        },
        Message::SysKeyUp { vk: 0x51 },
    ] {
        assert_eq!(testing::send(hwnd, message), Some(0), "{message:?}");
    }
    let received = parent.take_received();
    assert_eq!(
        pairs(&received),
        [
            (WM_KEYDOWN, 0x51),
            (WM_CHAR, usize::from(b'q')),
            (WM_KEYUP, 0x51),
            (WM_MOUSEWHEEL, 0xFF88_0000),
            (WM_KEYDOWN, 0x0D),
            (WM_CHAR, 0x0D),
            (WM_CHAR, 0xD83D),
            (WM_CHAR, 0xDE00),
            (WM_SYSKEYDOWN, 0x51),
            (WM_SYSKEYUP, 0x51),
        ]
    );
    // The parameters are the message's own.
    assert_eq!(received[0].lparam, 1);
    assert_eq!(received[2].lparam, (1usize | (3usize << 30)) as isize);
}

/// A key that arrives while the handler runs waits for it, and then goes
/// on to the host or not by the answer it gets, like any other.
#[test]
fn keys_that_waited_for_the_handler_go_on_by_its_answer() {
    let parent = Rc::new(TopLevel::new(100, 100).expect("a desktop"));
    let window = {
        let parent = Rc::clone(&parent);
        Window::open(
            parent.parent(),
            WindowOptions::new(size(50, 50)).without_focus_on_click(),
            move |window: &WindowCx<'_>, event| match event {
                Event::PointerDown { .. } => {
                    let hwnd = window.raw_handle().expect("open");
                    testing::send(hwnd, key_down(0x51));
                    testing::send(hwnd, key_down(0x20));
                    // Nothing went on yet: both wait for this call.
                    assert_eq!(parent.take_received(), []);
                    Handled::Yes
                }
                Event::Key { key, .. } if key.key == Key::Character(' ') => Handled::Yes,
                _ => Handled::No,
            },
        )
        .expect("a child window")
    };
    testing::send(
        handle(&window),
        Message::ButtonDown {
            button: Button::Primary,
            at: at(1, 1),
            shift: false,
            control: false,
        },
    );
    assert_eq!(pairs(&parent.take_received()), [(WM_KEYDOWN, 0x51)]);
    assert_eq!(window.deferred_events(), 2);
}

/// A host that sends the keys it gets back to its focused child must not
/// start a loop: what comes back while the window passes it on is dropped.
#[test]
fn a_parent_that_sends_keys_back_does_not_loop() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = {
        let log: Log = Rc::default();
        let sink = Rc::clone(&log);
        let window = Window::open(
            parent.parent(),
            WindowOptions::new(size(50, 50)),
            move |_: &WindowCx<'_>, event| {
                sink.borrow_mut().push(event);
                Handled::No
            },
        )
        .expect("a child window");
        (window, log)
    };
    let hwnd = handle(&window);
    parent.send_back_to(Some(hwnd));
    take(&log);
    testing::send(hwnd, key_down(0x51));
    testing::send(hwnd, Message::Char(u16::from(b'q')));
    testing::send(
        hwnd,
        Message::Wheel {
            at: at(3, 3),
            delta: 120,
            horizontal: false,
        },
    );
    parent.send_back_to(None);
    let keys = take(&log)
        .into_iter()
        .filter(|event| {
            matches!(
                event,
                Event::Key { .. } | Event::Text(_) | Event::Wheel { .. }
            )
        })
        .count();
    assert_eq!(keys, 3, "each reached the handler once");
    assert_eq!(
        pairs(&parent.take_received()),
        [
            (WM_KEYDOWN, 0x51),
            (WM_CHAR, usize::from(b'q')),
            (WM_MOUSEWHEEL, 0x0078_0000)
        ]
    );
}

/// A dialog keeps Tab, Enter and Escape unless the window asks for them.
#[test]
fn the_window_asks_a_dialog_for_all_keys_only_when_it_wants_them() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, _log) = logged(&parent, WindowOptions::new(size(20, 20)));
    let hwnd = handle(&window);
    let some = (DLGC_WANTARROWS | DLGC_WANTCHARS) as isize;
    let all = (DLGC_WANTARROWS | DLGC_WANTCHARS | DLGC_WANTTAB | DLGC_WANTALLKEYS) as isize;
    assert_eq!(testing::send(hwnd, Message::GetDlgCode), Some(some));
    window.set_wants_all_keys(true);
    assert_eq!(testing::send(hwnd, Message::GetDlgCode), Some(all));
    window.set_wants_all_keys(false);
    assert_eq!(testing::send(hwnd, Message::GetDlgCode), Some(some));
}

/// A handler stuck in something long (a nested loop) while ticks and
/// moves arrive comes back to one tick and the last move.
#[test]
fn ticks_and_moves_that_wait_are_merged() {
    let parent = TopLevel::new(100, 100).expect("a desktop");
    let log: Log = Rc::default();
    let window = {
        let log = Rc::clone(&log);
        Window::open(
            parent.parent(),
            WindowOptions::new(size(50, 50)).without_focus_on_click(),
            move |window: &WindowCx<'_>, event| {
                if !matches!(event, Event::Paint(_)) {
                    log.borrow_mut().push(event);
                }
                if let Event::PointerDown { .. } = event {
                    let hwnd = window.raw_handle().expect("open");
                    window.set_timer(TimerId(3), 1000).expect("open");
                    for i in 0..500 {
                        testing::send(hwnd, Message::Timer(3));
                        testing::send(
                            hwnd,
                            Message::MouseMove {
                                at: at(i % 40, 7),
                                shift: false,
                                control: false,
                            },
                        );
                    }
                }
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    testing::send(
        handle(&window),
        Message::ButtonDown {
            button: Button::Primary,
            at: at(1, 1),
            shift: false,
            control: false,
        },
    );
    let events = take(&log);
    assert_eq!(events.len(), 3, "{events:#?}");
    assert_eq!(events[1], Event::Timer(TimerId(3)));
    let Event::PointerMove(last) = events[2] else {
        panic!("{:?}", events[2])
    };
    assert_eq!(last.physical, at(499 % 40, 7));
    assert_eq!(window.deferred_events(), 1000);
    assert_eq!(window.dropped_events(), 0);
}

/// `GWLP_USERDATA` is anyone's to write; the window keeps its state in
/// its class's own bytes and carries on.
#[test]
#[allow(unsafe_code)]
fn a_window_does_not_trust_its_user_data() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWLP_USERDATA, GetWindowLongPtrW, SetWindowLongPtrW,
    };

    let parent = TopLevel::new(100, 100).expect("a desktop");
    let (window, log) = logged(&parent, WindowOptions::new(size(20, 20)));
    let hwnd: windows_sys::Win32::Foundation::HWND =
        std::ptr::with_exposed_provenance_mut(handle(&window));
    // SAFETY: a live window of this thread; the slot is a plain number to
    // the system.
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0x1234_5678);
    }
    take(&log);
    testing::send(handle(&window), Message::Char(u16::from(b'a'))).expect("ours");
    assert_eq!(take(&log), [Event::Text('a')]);
    // SAFETY: as above.
    let kept = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) };
    assert_eq!(kept, 0x1234_5678, "the window left the slot alone");
}

#[test]
fn a_parent_destroyed_from_inside_the_handler_closes_the_window_after_the_call() {
    let parent = Rc::new(TopLevel::new(50, 50).expect("a desktop"));
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let window = {
        let (parent, log) = (Rc::clone(&parent), Rc::clone(&log));
        Window::open(
            parent.parent(),
            WindowOptions::new(size(20, 20)).without_focus_on_click(),
            move |window: &WindowCx<'_>, event| {
                if !matches!(event, Event::Paint(_)) {
                    log.borrow_mut().push(format!("{event:?}"));
                }
                if let Event::PointerDown { .. } = event {
                    parent.destroy();
                    // Destroyed already, though the handler hears of it later.
                    assert!(!window.is_open());
                    log.borrow_mut().push("returning".to_string());
                }
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    let hwnd = handle(&window);
    testing::send(
        hwnd,
        Message::ButtonDown {
            button: Button::Primary,
            at: at(1, 1),
            shift: false,
            control: false,
        },
    );
    let log = log.borrow();
    assert_eq!(log.len(), 3, "{log:#?}");
    assert_eq!(log[1], "returning");
    assert_eq!(log[2], "Closed");
    assert!(!testing::is_window(hwnd));
    assert_eq!(window.raw_handle(), None);
    assert!(!testing::has_system_capture(hwnd));
}

/// A fixed-seed walk through messages and requests in any order, with
/// hostile values, on a real window.
#[test]
fn fuzzed_messages_and_requests_keep_the_window_consistent() {
    let parent = TopLevel::new(200, 200).expect("a desktop");
    let depth = Rc::new(Cell::new(0));
    let window = {
        let depth = Rc::clone(&depth);
        Window::open(
            parent.parent(),
            WindowOptions::new(size(64, 64)),
            move |window: &WindowCx<'_>, event| {
                depth.set(depth.get() + 1);
                assert_eq!(depth.get(), 1, "re-entered");
                if let Event::Paint(damage) = event {
                    let mut frame = Pixmap::new(64, 64).expect("small");
                    frame.fill(Color::rgb(9, 8, 7));
                    window.present(&frame, damage.rects()).expect("open");
                }
                if let Event::Timer(TimerId(2)) = event {
                    let _ = window.resize(size(40, 30));
                }
                depth.set(depth.get() - 1);
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    let hwnd = handle(&window);
    let mut seed = 0x1234_5678_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let buttons = [Button::Primary, Button::Secondary, Button::Middle];
    for step in 0..1500 {
        let p = at((next() % 200) as i32 - 50, (next() % 200) as i32 - 50);
        let message = match next() % 14 {
            0 => Message::MouseMove {
                at: p,
                shift: next() % 2 == 0,
                control: next() % 2 == 0,
            },
            1 | 2 => Message::ButtonDown {
                button: buttons[(next() % 3) as usize],
                at: p,
                shift: false,
                control: false,
            },
            3 | 4 => Message::ButtonUp {
                button: buttons[(next() % 3) as usize],
                at: p,
            },
            5 => Message::MouseLeave,
            6 => Message::CaptureChanged,
            7 => Message::Wheel {
                at: p,
                delta: next() as i16,
                horizontal: next() % 2 == 0,
            },
            8 => Message::KeyDown {
                vk: (next() % 0x100) as u16,
                repeat: next() % 2 == 0,
                system: next() % 4 == 0,
            },
            9 => Message::Char(next() as u16),
            10 => Message::Timer(next() % 4),
            11 => Message::SetFocus,
            12 => Message::KillFocus,
            _ => Message::DpiChangedAfterParent,
        };
        testing::send(hwnd, message).expect("ours");
        match next() % 8 {
            0 => {
                let _ = window.resize(size(next() % 120, next() % 120));
            }
            1 => window.invalidate(IRect::new(p.x, p.y, next() % 100, next() % 100)),
            2 => {
                let _ = window.set_timer(TimerId(next() % 4), 10 + next() % 20);
            }
            3 => {
                window.kill_timer(TimerId(next() % 4));
            }
            4 => {
                testing::update(hwnd);
            }
            5 => {
                let _ = window.set_host_scale(
                    [None, Some(1.0), Some(2.5), Some(f64::NAN)][(next() % 4) as usize],
                );
            }
            6 => {
                testing::resize_as_host(hwnd, size(next() % 120, next() % 120));
            }
            _ => testing::pump(Duration::from_millis(1)),
        }
        assert_eq!(
            window.has_capture(),
            testing::has_system_capture(hwnd),
            "step {step}"
        );
        assert!(window.is_open() && !window.is_poisoned(), "step {step}");
    }
    assert_eq!(depth.get(), 0);
}

#[test]
fn class_names_carry_this_binary_and_this_copy_of_the_crate() {
    let name = testing::window_class_name();
    assert!(name.starts_with(&format!("Manguebyte.Window.{}.", env!("CARGO_PKG_VERSION"))));
    let parts: Vec<&str> = name.split('.').collect();
    assert_eq!(parts.len(), 7, "{name}");
    assert!(u64::from_str_radix(parts[5], 16).expect("hex module") > 0);
    assert!(u64::from_str_radix(parts[6], 16).expect("hex address") > 0);
}
