//! The window seam, driven through the headless backend: the same code the
//! Win32 backend runs for everything but the system calls.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use manguebyte_gfx::{Color, IRect, MAX_SIDE, Pixmap, Point, PremulColor};
use manguebyte_window::{
    Button, Damage, Event, Handled, Headless, Key, KeyEvent, MAX_RECTS, Modifiers, ParentWindow,
    PhysicalPoint, PhysicalSize, PointerEvent, RawInput, TimerId, WHEEL_DELTA, Window, WindowApi,
    WindowCx, WindowError, WindowOptions,
};

type Log = Rc<RefCell<Vec<Event>>>;

/// A window whose handler logs every event and answers `Handled::Yes`.
fn logged(options: WindowOptions) -> (Window, Headless, Log) {
    let log: Log = Rc::default();
    let sink = Rc::clone(&log);
    let (window, headless) = Window::headless(options, move |_: &WindowCx<'_>, event| {
        sink.borrow_mut().push(event);
        Handled::Yes
    })
    .expect("valid options");
    (window, headless, log)
}

fn take(log: &Log) -> Vec<Event> {
    log.borrow_mut().drain(..).collect()
}

fn size(width: u32, height: u32) -> PhysicalSize {
    PhysicalSize::new(width, height)
}

fn at(x: i32, y: i32) -> PhysicalPoint {
    PhysicalPoint::new(x, y)
}

fn down(x: i32, y: i32, button: Button, time_ms: u32) -> RawInput {
    RawInput::PointerDown {
        at: at(x, y),
        button,
        modifiers: Modifiers::NONE,
        time_ms,
    }
}

fn up(x: i32, y: i32, button: Button) -> RawInput {
    RawInput::PointerUp {
        at: at(x, y),
        button,
        modifiers: Modifiers::NONE,
    }
}

fn moved(x: i32, y: i32) -> RawInput {
    RawInput::PointerMove {
        at: at(x, y),
        modifiers: Modifiers::NONE,
    }
}

fn pointer(x: i32, y: i32, scale: f64) -> PointerEvent {
    PointerEvent {
        position: Point::new((f64::from(x) / scale) as f32, (f64::from(y) / scale) as f32),
        physical: at(x, y),
        modifiers: Modifiers::NONE,
    }
}

/// A pixmap whose every pixel differs from its neighbours.
fn pattern(width: u32, height: u32, salt: u8) -> Pixmap {
    let mut data = Vec::new();
    for y in 0..height {
        for x in 0..width {
            data.extend_from_slice(&[
                (x as u8).wrapping_mul(7).wrapping_add(salt) % 200,
                (y as u8).wrapping_mul(11) % 200,
                salt % 200,
                200 + (((x + y) as u8) % 56),
            ]);
        }
    }
    Pixmap::from_premultiplied(width, height, data).expect("premultiplied")
}

#[test]
fn the_first_expose_asks_for_the_whole_window_and_only_once() {
    let (_window, headless, log) = logged(WindowOptions::new(size(30, 20)));
    assert_eq!(
        headless.pending_damage().rects(),
        [IRect::new(0, 0, 30, 20)]
    );
    assert!(headless.expose());
    assert_eq!(
        take(&log),
        [Event::Paint(Damage::of(IRect::new(0, 0, 30, 20)))]
    );
    // An expose with nothing damaged is answered from the back buffer.
    assert!(!headless.expose());
    assert!(take(&log).is_empty());
}

#[test]
fn presenting_converts_and_copies_only_the_damaged_rectangles() {
    let (window, headless, _log) = logged(WindowOptions::new(size(16, 12)));
    let first = pattern(16, 12, 3);
    window.present_all(&first).expect("open");
    assert_eq!(window.presented_pixels(), 16 * 12);
    assert_eq!(headless.take_blits(), [IRect::new(0, 0, 16, 12)]);

    let second = pattern(16, 12, 90);
    let damage = [IRect::new(2, 3, 4, 5), IRect::new(10, 0, 3, 2)];
    window.present(&second, &damage).expect("open");
    assert_eq!(window.presented_pixels(), 16 * 12 + 20 + 6);
    assert_eq!(headless.take_blits(), damage);

    let shown = headless.frame().expect("exists");
    for y in 0..12 {
        for x in 0..16 {
            let inside = damage
                .iter()
                .any(|rect| rect.contains(i64::from(x), i64::from(y)));
            let expected = if inside { &second } else { &first };
            assert_eq!(shown.pixel(x, y), expected.pixel(x, y), "({x}, {y})");
        }
    }
}

#[test]
fn hostile_rectangles_are_cut_to_the_window_and_the_pixmap() {
    let (window, headless, _log) = logged(WindowOptions::new(size(10, 10)));
    let small = pattern(4, 6, 1);
    window
        .present(
            &small,
            &[
                IRect::new(i32::MIN, i32::MIN, u32::MAX, u32::MAX),
                IRect::new(i32::MAX, 0, u32::MAX, 3),
                IRect::new(-5, 2, 0, 9),
            ],
        )
        .expect("open");
    // Only the pixmap's 4x6 pixels were taken.
    assert_eq!(window.presented_pixels(), 24);
    assert_eq!(headless.take_blits(), [IRect::new(0, 0, 4, 6)]);
    let shown = headless.frame().expect("exists");
    assert_eq!(shown.pixel(3, 5), small.pixel(3, 5));
    assert_eq!(shown.pixel(4, 5), Some(PremulColor::TRANSPARENT));
    assert_eq!(shown.pixel(3, 6), Some(PremulColor::TRANSPARENT));

    // A pixmap larger than the window shows only what fits.
    let large = pattern(40, 40, 2);
    window.present_all(&large).expect("open");
    assert_eq!(headless.take_blits(), [IRect::new(0, 0, 10, 10)]);
    assert_eq!(
        headless.frame().expect("exists").pixel(9, 9),
        large.pixel(9, 9)
    );
}

#[test]
fn many_damaged_rectangles_are_merged_and_none_is_lost() {
    let (window, headless, _log) = logged(WindowOptions::new(size(100, 4)));
    let pixels = pattern(100, 4, 5);
    let rects: Vec<IRect> = (0..20).map(|i| IRect::new(i * 5, 0, 1, 1)).collect();
    window.present(&pixels, &rects).expect("open");
    let blits = headless.take_blits();
    assert!(blits.len() <= MAX_RECTS, "{blits:?}");
    let shown = headless.frame().expect("exists");
    for rect in &rects {
        assert_eq!(
            shown.pixel(rect.x as u32, 0),
            pixels.pixel(rect.x as u32, 0)
        );
    }
}

#[test]
fn invalidated_rectangles_reach_the_next_expose_cut_and_merged() {
    let (window, headless, log) = logged(WindowOptions::new(size(50, 40)));
    headless.expose();
    take(&log);
    window.invalidate(IRect::new(5, 5, 10, 10));
    window.invalidate(IRect::new(6, 6, 2, 2));
    window.invalidate(IRect::new(45, 35, 20, 20));
    window.invalidate(IRect::new(100, 100, 5, 5));
    assert_eq!(
        headless.take_invalidations(),
        [
            IRect::new(5, 5, 10, 10),
            IRect::new(6, 6, 2, 2),
            IRect::new(45, 35, 5, 5)
        ]
    );
    headless.expose();
    let mut expected = Damage::new();
    expected.add(IRect::new(5, 5, 10, 10));
    expected.add(IRect::new(45, 35, 5, 5));
    assert_eq!(take(&log), [Event::Paint(expected)]);
}

#[test]
fn pointer_positions_are_exact_at_scale_1_and_1_5() {
    for scale in [1.0, 1.5] {
        let (_window, headless, log) = logged(WindowOptions::new(size(300, 150)).with_scale(scale));
        headless.input(down(15, 30, Button::Primary, 0));
        headless.input(moved(7, -3));
        headless.input(up(299, 149, Button::Primary));
        headless.input(RawInput::Wheel {
            at: at(3, 4),
            delta_x: 0,
            delta_y: -2 * WHEEL_DELTA,
            modifiers: Modifiers::NONE,
        });
        assert_eq!(
            take(&log),
            [
                Event::Focus(true),
                Event::PointerDown {
                    pointer: pointer(15, 30, scale),
                    button: Button::Primary,
                    clicks: 1
                },
                Event::PointerMove(pointer(7, -3, scale)),
                Event::PointerUp {
                    pointer: pointer(299, 149, scale),
                    button: Button::Primary
                },
                Event::Wheel {
                    pointer: pointer(3, 4, scale),
                    delta: Point::new(0.0, -2.0)
                },
            ],
            "scale {scale}"
        );
    }
    // The values themselves, at 1.5.
    let p = pointer(15, 30, 1.5).position;
    assert_eq!((p.x, p.y), (10.0, 20.0));
    let p = pointer(7, -3, 1.5).position;
    assert_eq!((p.x, p.y), (4.6666665, -2.0));
}

#[test]
fn a_drag_holds_the_pointer_until_the_last_button_is_up() {
    let (window, headless, log) = logged(WindowOptions::new(size(100, 100)));
    headless.input(down(10, 10, Button::Primary, 0));
    assert!(headless.has_capture() && window.has_capture());
    headless.input(down(11, 10, Button::Secondary, 1));
    // The system's leave notice during a drag is held back.
    assert_eq!(headless.input(RawInput::PointerLeft), None);
    headless.input(moved(-40, 300));
    headless.input(up(-40, 300, Button::Primary));
    assert!(headless.has_capture());
    headless.input(up(-40, 300, Button::Secondary));
    assert!(!headless.has_capture() && !window.has_capture());
    let events = take(&log);
    assert_eq!(events.len(), 7, "{events:#?}");
    assert_eq!(
        events[5],
        Event::PointerUp {
            pointer: pointer(-40, 300, 1.0),
            button: Button::Secondary
        }
    );
    // The drag ended outside, so the window says the pointer left.
    assert_eq!(events[6], Event::PointerLeave);
    // An up for a button that never went down here is not reported.
    assert_eq!(headless.input(up(5, 5, Button::Middle)), None);
    assert!(take(&log).is_empty());
}

#[test]
fn losing_the_pointer_cancels_the_drag_once() {
    let (_window, headless, log) = logged(WindowOptions::new(size(100, 100)));
    headless.input(down(10, 10, Button::Primary, 0));
    take(&log);
    headless.input(RawInput::CaptureLost);
    headless.input(RawInput::CaptureLost);
    headless.input(up(10, 10, Button::Primary));
    assert_eq!(take(&log), [Event::PointerCancel]);
}

#[test]
fn quick_presses_count_as_double_and_triple_clicks() {
    let (_window, headless, log) = logged(WindowOptions::new(size(100, 100)));
    let mut clicks = Vec::new();
    for (time, x) in [(0, 10), (300, 11), (600, 12), (1200, 12), (1250, 30)] {
        headless.input(down(x, 10, Button::Primary, time));
        headless.input(up(x, 10, Button::Primary));
    }
    for event in take(&log) {
        if let Event::PointerDown { clicks: n, .. } = event {
            clicks.push(n);
        }
    }
    assert_eq!(clicks, [1, 2, 3, 1, 1]);
}

#[test]
fn leaving_is_reported_once_and_repeated_moves_are_dropped() {
    let (_window, headless, log) = logged(WindowOptions::new(size(100, 100)));
    headless.input(moved(5, 5));
    headless.input(moved(5, 5));
    headless.input(moved(6, 5));
    headless.input(RawInput::PointerLeft);
    headless.input(RawInput::PointerLeft);
    assert_eq!(
        take(&log),
        [
            Event::PointerMove(pointer(5, 5, 1.0)),
            Event::PointerMove(pointer(6, 5, 1.0)),
            Event::PointerLeave,
        ]
    );
    // The window asked to hear of the leave once per entry.
    assert_eq!(headless.leave_requests(), 1);
    headless.input(moved(6, 5));
    assert_eq!(headless.leave_requests(), 2);
}

#[test]
fn wheel_turns_arrive_in_lines() {
    let (_window, headless, log) = logged(WindowOptions::new(size(100, 100)));
    let ctrl = Modifiers {
        control: true,
        ..Modifiers::NONE
    };
    for (dx, dy) in [(0, 120), (0, -240), (60, 0), (0, 15), (0, 0)] {
        headless.input(RawInput::Wheel {
            at: at(1, 2),
            delta_x: dx,
            delta_y: dy,
            modifiers: ctrl,
        });
    }
    let deltas: Vec<(f32, f32)> = take(&log)
        .into_iter()
        .map(|event| match event {
            Event::Wheel { delta, pointer } => {
                assert_eq!(pointer.modifiers, ctrl);
                (delta.x, delta.y)
            }
            other => panic!("{other:?}"),
        })
        .collect();
    // A turn of nothing is no event.
    assert_eq!(deltas, [(0.0, 1.0), (0.0, -2.0), (0.5, 0.0), (0.0, 0.125)]);
}

#[test]
fn keys_and_text_arrive_as_typed() {
    let (_window, headless, log) = logged(WindowOptions::new(size(10, 10)));
    let z = KeyEvent {
        key: Key::Character('z'),
        modifiers: Modifiers {
            control: true,
            ..Modifiers::NONE
        },
        pressed: true,
    };
    headless.input(RawInput::Key {
        key: z,
        repeat: false,
    });
    headless.input(RawInput::Key {
        key: z,
        repeat: true,
    });
    for unit in "é😀\r".encode_utf16() {
        headless.input(RawInput::Utf16(unit));
    }
    assert_eq!(
        take(&log),
        [
            Event::Key {
                key: z,
                repeat: false
            },
            Event::Key {
                key: z,
                repeat: true
            },
            Event::Text('é'),
            Event::Text('😀'),
        ]
    );
}

#[test]
fn focus_is_reported_once_per_change_and_a_press_takes_it() {
    let (window, headless, log) = logged(WindowOptions::new(size(10, 10)));
    headless.input(RawInput::Focus(true));
    headless.input(RawInput::Focus(true));
    assert!(window.has_focus());
    // Already focused: a press does not ask again.
    headless.input(down(1, 1, Button::Primary, 0));
    assert_eq!(headless.focus_requests(), 0);
    headless.input(RawInput::Focus(false));
    assert_eq!(
        take(&log),
        [
            Event::Focus(true),
            Event::PointerDown {
                pointer: pointer(1, 1, 1.0),
                button: Button::Primary,
                clicks: 1
            },
            Event::Focus(false)
        ]
    );

    let (_window, headless, log) =
        logged(WindowOptions::new(size(10, 10)).without_focus_on_click());
    headless.input(down(1, 1, Button::Primary, 0));
    assert_eq!(headless.focus_requests(), 0);
    assert!(matches!(take(&log)[..], [Event::PointerDown { .. }]));
}

#[test]
fn resizing_damages_only_what_became_visible() {
    let (window, headless, log) = logged(WindowOptions::new(size(40, 30)));
    headless.expose();
    take(&log);
    let pixels = pattern(40, 30, 7);
    window.present_all(&pixels).expect("open");

    window.resize(size(50, 35)).expect("open");
    assert_eq!(window.size(), size(50, 35));
    assert_eq!(take(&log), [Event::Resized(size(50, 35))]);
    assert_eq!(
        headless.pending_damage().rects(),
        [IRect::new(40, 0, 10, 35), IRect::new(0, 30, 40, 5)]
    );
    // What both sizes share is kept.
    let shown = headless.frame().expect("exists");
    assert_eq!((shown.width(), shown.height()), (50, 35));
    assert_eq!(shown.pixel(39, 29), pixels.pixel(39, 29));
    assert_eq!(shown.pixel(45, 2), Some(PremulColor::TRANSPARENT));

    // The host shrinks it: nothing new to draw, and stale damage is cut.
    headless.resize(size(20, 10));
    assert_eq!(take(&log), [Event::Resized(size(20, 10))]);
    assert!(headless.pending_damage().is_empty());
    // The same size again is no event.
    headless.resize(size(20, 10));
    window.resize(size(20, 10)).expect("open");
    assert!(take(&log).is_empty());
    // A zero size is allowed, as hosts use it.
    headless.resize(size(0, 0));
    assert_eq!(take(&log), [Event::Resized(size(0, 0))]);
    assert_eq!(
        window.resize(size(MAX_SIDE + 1, 5)),
        Err(WindowError::InvalidSize(size(MAX_SIDE + 1, 5)))
    );
}

/// `set_system_scale` says whether the window took the scale: a window
/// whose parent was destroyed takes none, and says so.
#[test]
fn a_closed_window_reports_the_system_scale_as_not_taken() {
    let (window, headless, log) = logged(WindowOptions::new(size(10, 10)));
    headless.destroy_parent();
    take(&log);
    assert!(!headless.set_system_scale(2.0));
    assert_eq!(window.scale(), 1.0, "a closed window keeps its scale");
    assert!(take(&log).is_empty());
    drop(window);
    assert!(!headless.set_system_scale(2.0), "nor a window that is gone");
}

#[test]
fn scale_changes_damage_everything_and_the_host_scale_wins() {
    let (window, headless, log) = logged(WindowOptions::new(size(30, 20)));
    headless.expose();
    take(&log);
    assert_eq!(window.scale(), 1.0);
    assert!(headless.set_system_scale(1.25));
    assert_eq!(take(&log), [Event::ScaleChanged(1.25)]);
    assert_eq!(
        headless.pending_damage().rects(),
        [IRect::new(0, 0, 30, 20)]
    );
    assert_eq!(window.logical_size().width, 24.0);

    window.set_host_scale(Some(2.0)).expect("valid");
    assert_eq!(take(&log), [Event::ScaleChanged(2.0)]);
    // The system's changes no longer matter while the host's scale holds.
    assert!(headless.set_system_scale(1.5));
    assert!(take(&log).is_empty());
    window.set_host_scale(None).expect("valid");
    assert_eq!(take(&log), [Event::ScaleChanged(1.5)]);

    for bad in [0.0, -1.0, f64::NAN, f64::INFINITY, 9.0, 0.2] {
        assert!(matches!(
            window.set_host_scale(Some(bad)),
            Err(WindowError::InvalidScale(_))
        ));
        assert!(!headless.set_system_scale(bad));
    }
    assert_eq!(window.scale(), 1.5);
    assert!(take(&log).is_empty());
    assert_eq!(window.to_logical(at(3, 6)), Point::new(2.0, 4.0));
}

#[test]
fn timers_fire_until_killed_and_never_after() {
    let (window, headless, log) = logged(WindowOptions::new(size(10, 10)));
    window.set_timer(TimerId(7), 16).expect("open");
    window.set_timer(TimerId(2), 100).expect("open");
    window.set_timer(TimerId(7), 33).expect("open");
    assert_eq!(headless.timers(), [(TimerId(7), 33), (TimerId(2), 100)]);
    assert_eq!(headless.fire_timers(), 2);
    assert_eq!(
        take(&log),
        [Event::Timer(TimerId(2)), Event::Timer(TimerId(7))]
    );

    assert!(window.kill_timer(TimerId(7)));
    assert!(!window.kill_timer(TimerId(7)));
    // A tick the system had already queued for the killed timer.
    assert_eq!(headless.input(RawInput::Timer(TimerId(7))), None);
    // A tick for an id that was never set.
    assert_eq!(headless.input(RawInput::Timer(TimerId(99))), None);
    assert!(take(&log).is_empty());
    assert_eq!(
        window.set_timer(TimerId(1), 0),
        Err(WindowError::InvalidPeriod)
    );
}

#[test]
fn events_caused_inside_the_handler_wait_for_it_in_order() {
    let depth = Rc::new(Cell::new(0));
    let deepest = Rc::new(Cell::new(0));
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let controller: Rc<RefCell<Option<Headless>>> = Rc::default();
    let (window, headless) = {
        let (depth, deepest, log, controller) = (
            Rc::clone(&depth),
            Rc::clone(&deepest),
            Rc::clone(&log),
            Rc::clone(&controller),
        );
        Window::headless(
            WindowOptions::new(size(100, 100)),
            move |window: &WindowCx<'_>, event| {
                depth.set(depth.get() + 1);
                deepest.set(deepest.get().max(depth.get()));
                log.borrow_mut().push(format!("begin {event:?}"));
                if let Event::PointerDown { .. } = event {
                    let headless = controller.borrow().clone().expect("set");
                    assert!(headless.handler_running());
                    // Each of these makes an event at once.
                    window.resize(size(120, 100)).expect("open");
                    window.set_host_scale(Some(2.0)).expect("valid");
                    assert_eq!(headless.input(moved(50, 50)), None);
                    headless.input(RawInput::Utf16(u16::from(b'x')));
                    window.set_timer(TimerId(1), 10).expect("open");
                    headless.input(RawInput::Timer(TimerId(1)));
                }
                log.borrow_mut().push("end".to_string());
                depth.set(depth.get() - 1);
                Handled::Yes
            },
        )
        .expect("valid options")
    };
    *controller.borrow_mut() = Some(headless.clone());
    headless.input(RawInput::Focus(true));
    log.borrow_mut().clear();

    assert_eq!(
        headless.input(down(10, 10, Button::Primary, 0)),
        Some(Handled::Yes)
    );
    assert_eq!(deepest.get(), 1, "the handler was re-entered");
    let log = log.borrow();
    let begins: Vec<&str> = log
        .iter()
        .filter_map(|line| line.strip_prefix("begin "))
        .collect();
    assert_eq!(
        begins,
        [
            "PointerDown { pointer: PointerEvent { position: Point { x: 10.0, y: 10.0 }, physical: PhysicalPoint { x: 10, y: 10 }, modifiers: Modifiers { shift: false, control: false, alt: false, meta: false } }, button: Primary, clicks: 1 }",
            "Resized(PhysicalSize { width: 120, height: 100 })",
            "ScaleChanged(2.0)",
            "PointerMove(PointerEvent { position: Point { x: 25.0, y: 25.0 }, physical: PhysicalPoint { x: 50, y: 50 }, modifiers: Modifiers { shift: false, control: false, alt: false, meta: false } })",
            "Text('x')",
            "Timer(TimerId(1))",
        ]
    );
    // Every call ended before the next began.
    for pair in log.chunks(2) {
        assert!(
            pair[0].starts_with("begin ") && pair[1] == "end",
            "{pair:?}"
        );
    }
    assert_eq!(window.deferred_events(), 5);
}

#[test]
fn a_panicking_handler_poisons_the_window_without_unwinding() {
    let calls = Rc::new(Cell::new(0));
    let counter = Rc::clone(&calls);
    let (window, headless) = Window::headless(
        WindowOptions::new(size(10, 10)),
        move |_: &WindowCx<'_>, event| {
            counter.set(counter.get() + 1);
            if let Event::Key { .. } = event {
                panic!("a handler bug");
            }
            Handled::Yes
        },
    )
    .expect("valid options");
    let key = RawInput::Key {
        key: KeyEvent {
            key: Key::Enter,
            modifiers: Modifiers::NONE,
            pressed: true,
        },
        repeat: false,
    };
    assert_eq!(headless.input(key), None);
    assert!(window.is_poisoned());
    assert_eq!(headless.input(RawInput::Focus(true)), None);
    headless.expose();
    headless.input(RawInput::Utf16(u16::from(b'q')));
    assert_eq!(calls.get(), 1);
    drop(window);
    assert!(!headless.exists());
}

#[test]
fn a_window_dropped_from_inside_its_handler_goes_after_the_call() {
    let slot: Rc<RefCell<Option<Window>>> = Rc::default();
    let dropped = Rc::new(Cell::new(false));
    struct Probe(Rc<Cell<bool>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }
    let probe = Probe(Rc::clone(&dropped));
    let inside = Rc::clone(&slot);
    let flag = Rc::clone(&dropped);
    let (window, headless) = Window::headless(
        WindowOptions::new(size(10, 10)),
        move |window: &WindowCx<'_>, event| {
            let _keep = &probe;
            if event == Event::Focus(true) {
                let own = inside.borrow_mut().take();
                drop(own);
                // The window's value is gone, but this call is not over and
                // the handler is still here.
                assert!(!flag.get());
                assert_eq!(
                    window.present_all(&Pixmap::new(1, 1).expect("small")),
                    Err(WindowError::Closed)
                );
            }
            Handled::Yes
        },
    )
    .expect("valid options");
    *slot.borrow_mut() = Some(window);
    headless.input(RawInput::Focus(true));
    assert!(dropped.get(), "the handler was dropped after its call");
    assert!(!headless.exists());
}

#[test]
fn a_window_whose_parent_was_destroyed_refuses_everything_and_drops_cleanly() {
    let (window, headless, log) = logged(WindowOptions::new(size(10, 10)));
    window.set_timer(TimerId(1), 10).expect("open");
    headless.input(down(1, 1, Button::Primary, 0));
    take(&log);
    headless.destroy_parent();
    headless.destroy_parent();
    assert_eq!(take(&log), [Event::Closed]);
    assert!(!window.is_open());
    assert!(!window.has_capture());
    let pixels = Pixmap::new(10, 10).expect("small");
    assert_eq!(window.present_all(&pixels), Err(WindowError::Closed));
    assert_eq!(window.resize(size(5, 5)), Err(WindowError::Closed));
    assert_eq!(window.set_timer(TimerId(2), 10), Err(WindowError::Closed));
    assert_eq!(window.set_visible(true), Err(WindowError::Closed));
    assert_eq!(window.set_host_scale(None), Err(WindowError::Closed));
    window.invalidate_all();
    assert_eq!(headless.fire_timers(), 0);
    assert_eq!(headless.input(moved(2, 2)), None);
    headless.resize(size(3, 3));
    assert!(!headless.expose());
    assert!(take(&log).is_empty());
    assert!(headless.frame().is_some());
    drop(window);
    assert!(!headless.exists());
}

#[test]
fn hostile_options_are_refused() {
    let nothing = |_: &WindowCx<'_>, _| Handled::No;
    for bad in [size(MAX_SIDE + 1, 1), size(1, u32::MAX)] {
        assert_eq!(
            Window::headless(WindowOptions::new(bad), nothing).err(),
            Some(WindowError::InvalidSize(bad))
        );
    }
    for bad in [0.0, f64::NAN, -2.0, 100.0] {
        assert!(matches!(
            Window::headless(WindowOptions::new(size(1, 1)).with_scale(bad), nothing).err(),
            Some(WindowError::InvalidScale(_))
        ));
    }
    let cocoa = ParentWindow::cocoa(0x1000).expect("not null");
    let x11 = ParentWindow::x11(7).expect("not zero");
    assert_eq!(
        Window::open(cocoa, WindowOptions::new(size(1, 1)), nothing).err(),
        Some(WindowError::Unsupported(WindowApi::Cocoa))
    );
    assert_eq!(
        Window::open(x11, WindowOptions::new(size(1, 1)), nothing).err(),
        Some(WindowError::Unsupported(WindowApi::X11))
    );
    #[cfg(not(windows))]
    assert_eq!(
        Window::open(
            ParentWindow::win32(0x10).expect("not null"),
            WindowOptions::new(size(1, 1)),
            nothing
        )
        .err(),
        Some(WindowError::Unsupported(WindowApi::Win32))
    );
}

#[test]
fn a_hidden_window_shows_when_asked() {
    let (window, headless, _log) = logged(WindowOptions::new(size(5, 5)).hidden());
    assert!(!window.is_visible() && !headless.is_visible());
    window.set_visible(true).expect("open");
    assert!(window.is_visible() && headless.is_visible());
}

/// A fixed-seed walk through everything a host and a user can do, in any
/// order and with hostile values, checked against a model of what the
/// window must show and report.
#[test]
fn fuzzed_orders_and_values_keep_every_promise() {
    let mut seed = 0x9e37_79b9_u32;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let rounds = if cfg!(miri) { 2 } else { 40 };
    for round in 0..rounds {
        let log: Rc<RefCell<Vec<Event>>> = Rc::default();
        let depth = Rc::new(Cell::new(0u32));
        let (window, headless) = {
            let (log, depth) = (Rc::clone(&log), Rc::clone(&depth));
            Window::headless(
                WindowOptions::new(size(24, 16)),
                move |window: &WindowCx<'_>, event| {
                    depth.set(depth.get() + 1);
                    assert_eq!(depth.get(), 1, "re-entered");
                    // A handler that itself asks for things now and then.
                    if let Event::Timer(TimerId(3)) = event {
                        let _ = window.resize(size(30, 20));
                    }
                    log.borrow_mut().push(event);
                    depth.set(depth.get() - 1);
                    Handled::Yes
                },
            )
            .expect("valid options")
        };
        // The model of what the window shows.
        let mut model = Pixmap::new(24, 16).expect("small");
        let mut closed = false;
        for _ in 0..300 {
            let pick = next() % 15;
            let x = (next() % 80) as i32 - 20;
            let y = (next() % 80) as i32 - 20;
            match pick {
                0 => {
                    let rect = IRect::new(x, y, next() % 40, next() % 40);
                    let mut pixels = Pixmap::new(next() % 40, next() % 40).expect("small");
                    pixels.fill(Color::rgba(
                        next() as u8,
                        next() as u8,
                        next() as u8,
                        (next() % 2 * 255) as u8,
                    ));
                    let result = window.present(&pixels, &[rect]);
                    assert_eq!(result.is_ok(), !closed);
                    if !closed {
                        let shown = window.size().bounds();
                        if let Some(r) = rect
                            .intersect(&pixels.bounds())
                            .and_then(|r| r.intersect(&shown))
                        {
                            model.copy_from(&pixels, r, r.x, r.y);
                        }
                    }
                }
                1 => {
                    let wanted = size(next() % 48, next() % 48);
                    let result = window.resize(wanted);
                    assert_eq!(result.is_ok(), !closed);
                }
                2 => headless.resize(size(next() % 48, next() % 48)),
                3 => window.invalidate(IRect::new(x, y, next() % 50, next() % 50)),
                4 => {
                    headless.expose();
                }
                5 => {
                    let _ = window.set_timer(TimerId(next() % 4), next() % 3);
                }
                6 => {
                    window.kill_timer(TimerId(next() % 4));
                }
                7 => {
                    headless.fire_timers();
                }
                8 => {
                    let button =
                        [Button::Primary, Button::Secondary, Button::Middle][(next() % 3) as usize];
                    let input = if next() % 2 == 0 {
                        down(x, y, button, next())
                    } else {
                        up(x, y, button)
                    };
                    headless.input(input);
                }
                9 => {
                    headless.input(moved(x, y));
                }
                10 => {
                    let input = [
                        RawInput::PointerLeft,
                        RawInput::CaptureLost,
                        RawInput::Focus(next() % 2 == 0),
                        RawInput::Utf16(next() as u16),
                        RawInput::Timer(TimerId(next() % 6)),
                    ][(next() % 5) as usize];
                    headless.input(input);
                }
                11 => {
                    let scale = [1.0, 1.5, 2.0, f64::NAN, -1.0, 0.0][(next() % 6) as usize];
                    let _ = window.set_host_scale((next() % 3 != 0).then_some(scale));
                }
                12 => {
                    headless.set_system_scale([1.0, 1.25, 3.0, 1e9][(next() % 4) as usize]);
                }
                13 => {
                    if next() % 40 == 0 {
                        headless.destroy_parent();
                        closed = true;
                    }
                }
                _ => {
                    headless.input(RawInput::Wheel {
                        at: at(x, y),
                        delta_x: next() as i32 % 1000,
                        delta_y: next() as i32 % 1000,
                        modifiers: Modifiers::NONE,
                    });
                }
            }
            // The model follows the window's size, keeping what both share.
            let now = window.size();
            if (model.width(), model.height()) != (now.width, now.height) {
                let mut resized = Pixmap::new(now.width, now.height).expect("small");
                resized.copy_from(&model, model.bounds(), 0, 0);
                model = resized;
            }
            let shown = headless.frame().expect("exists");
            assert_eq!(shown, model, "round {round}");
            assert!(
                headless
                    .pending_damage()
                    .rects()
                    .iter()
                    .all(|r| now.bounds().contains_rect(r))
            );
            assert_eq!(window.is_open(), !closed);
            assert!(window.scale().is_finite() && window.scale() > 0.0);
            if !window.has_capture() {
                assert!(!headless.has_capture());
            }
        }
        // Nothing reached the handler after it was told the window closed.
        let events = log.borrow();
        if let Some(index) = events.iter().position(|e| *e == Event::Closed) {
            assert_eq!(index, events.len() - 1, "round {round}");
        }
    }
}
