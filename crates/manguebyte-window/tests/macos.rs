//! The macOS backend on a real view (ADR 0028, decision 5): a view inside a
//! window's view, as a host's, presents a known picture that is read back by
//! having the view and its layer draw into bitmaps, and receives `NSEvent`s.
//!
//! AppKit works on a process's first thread only, so this test has no
//! harness: `main` runs the checks one after another and fails if any does.
//! On other systems it does nothing. The check with two binaries needs the
//! `macos_view` example built, which `cargo test` does unless it is told to
//! build this test alone.

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    {
        macos::main()
    }
    #[cfg(not(target_os = "macos"))]
    {
        println!("macOS only: nothing to check on this system");
        std::process::ExitCode::SUCCESS
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::cell::{Cell, RefCell};
    use std::ffi::{CStr, CString, c_char, c_int, c_void};
    use std::panic;
    use std::path::{Path, PathBuf};
    use std::process::ExitCode;
    use std::rc::Rc;
    use std::time::Duration;

    use manguebyte_gfx::{Color, IRect, Pixmap};
    use manguebyte_window::macos::testing::{self, Host, Input, PassedOn};
    use manguebyte_window::{
        Button, Event, FileDialog, Handled, Modifiers, ParentWindow, PhysicalPoint, PhysicalSize,
        TimerId, Window, WindowApi, WindowCx, WindowError, WindowOptions,
    };

    const WIDTH: u32 = 120;
    const HEIGHT: u32 = 80;

    pub(super) fn main() -> ExitCode {
        let checks: &[(&str, fn())] = &[
            ("a_view_opens_at_the_top_left_of_the_parent", opens),
            (
                "presented_pixels_are_what_the_view_and_its_layer_draw",
                pixels,
            ),
            ("a_present_changes_only_its_rectangles", partial_presents),
            ("damage_comes_back_as_paints", damage),
            ("buttons_and_moves_arrive_as_pointer_events", pointer),
            ("the_keyboard_follows_the_first_responder", focus),
            ("wheel_turns_arrive_in_lines_and_go_on_when_unused", wheel),
            ("keys_and_text_arrive_and_go_on_when_unused", keys),
            ("command_combinations_are_offered_once", commands),
            ("the_text_caret_is_where_input_methods_are_told", caret),
            ("timers_fire_until_killed", timers),
            ("resizes_by_either_side_are_reported", resizes),
            (
                "visibility_and_the_hosts_scale_are_followed",
                visibility_and_scale,
            ),
            ("a_view_the_host_takes_away_closes_the_window", host_closes),
            ("closing_and_dropping_take_the_view_away", own_closes),
            ("a_panicking_handler_poisons_the_window_only", panics),
            ("what_is_not_a_view_is_refused", refusals),
            ("what_is_not_there_yet_says_so", missing),
            ("two_binaries_have_classes_of_their_own", two_binaries),
        ];
        let mut failed = Vec::new();
        for (name, check) in checks {
            let passed = panic::catch_unwind(check).is_ok();
            println!("test {name} ... {}", if passed { "ok" } else { "FAILED" });
            if !passed {
                failed.push(*name);
            }
        }
        if failed.is_empty() {
            println!("test result: ok. {} passed", checks.len());
            ExitCode::SUCCESS
        } else {
            println!("test result: FAILED. {failed:?}");
            ExitCode::FAILURE
        }
    }

    type Events = Rc<RefCell<Vec<Event>>>;

    /// What a handler answers.
    #[derive(Clone, Copy, PartialEq)]
    enum Uses {
        Everything,
        Nothing,
        TextOnly,
    }

    /// A window of the usual size in a host of its own, recording every
    /// event, painting `pattern` when asked.
    struct Probe {
        window: Window,
        host: Host,
        events: Events,
        uses: Rc<Cell<Uses>>,
    }

    impl Probe {
        fn new() -> Probe {
            Probe::with(WindowOptions::new(size(WIDTH, HEIGHT)))
        }

        fn with(options: WindowOptions) -> Probe {
            let host = Host::new(200.0, 150.0).expect("a host window");
            let events = Events::default();
            let uses = Rc::new(Cell::new(Uses::Everything));
            let handler = {
                let events = Rc::clone(&events);
                let uses = Rc::clone(&uses);
                move |cx: &WindowCx<'_>, event: Event| {
                    events.borrow_mut().push(event);
                    if let Event::Paint(damage) = event {
                        let frame = pattern(cx.size().width, cx.size().height);
                        cx.present(&frame, damage.rects()).expect("open");
                    }
                    match uses.get() {
                        Uses::Everything => Handled::Yes,
                        Uses::TextOnly if matches!(event, Event::Text(_)) => Handled::Yes,
                        _ => Handled::No,
                    }
                }
            };
            let window = Window::open(host.parent(), options, handler).expect("a view");
            Probe {
                window,
                host,
                events,
                uses,
            }
        }

        /// The events so far, forgetting them. Paints are left out: they
        /// come when AppKit displays.
        fn take(&self) -> Vec<Event> {
            let mut events = std::mem::take(&mut *self.events.borrow_mut());
            events.retain(|event| !matches!(event, Event::Paint(_)));
            events
        }

        fn paints(&self) -> usize {
            self.events
                .borrow()
                .iter()
                .filter(|event| matches!(event, Event::Paint(_)))
                .count()
        }

        fn show(&self) {
            testing::display(&self.window);
            testing::pump(Duration::from_millis(100));
            testing::display(&self.window);
        }

        fn send(&self, input: Input<'_>) {
            assert!(testing::send(&self.window, input), "{input:?}");
        }

        fn key(&self, code: u16, characters: &str, modifiers: Modifiers) {
            self.send(Input::Key {
                code,
                characters,
                modifiers,
                pressed: true,
                repeat: false,
            });
        }
    }

    fn size(width: u32, height: u32) -> PhysicalSize {
        PhysicalSize::new(width, height)
    }

    fn at(x: i32, y: i32) -> PhysicalPoint {
        PhysicalPoint::new(x, y)
    }

    /// Every pixel differs from its neighbours, and a fifth are
    /// translucent, so a swapped channel, a flipped row or an off-by-one
    /// copy all show.
    fn pattern(width: u32, height: u32) -> Pixmap {
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let a = if (x + y) % 5 == 0 { 128 } else { 255 };
                let scale = |c: u32| (c % 256 * a / 255) as u8;
                data.extend_from_slice(&[
                    scale(x * 9 + y),
                    scale(y * 7 + 3),
                    scale(x * 3 + y * 5),
                    a as u8,
                ]);
            }
        }
        Pixmap::from_premultiplied(width, height, data).expect("premultiplied")
    }

    /// What a window shows of `pixels`: each premultiplied colour, opaque.
    fn shown(pixels: &Pixmap) -> Vec<u8> {
        let mut data = pixels.data().to_vec();
        for pixel in data.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
        data
    }

    #[track_caller]
    fn assert_pixels(what: &str, got: Option<Pixmap>, expected: &[u8], width: u32, height: u32) {
        let got = got.unwrap_or_else(|| panic!("{what}: nothing to read"));
        assert_eq!((got.width(), got.height()), (width, height), "{what}");
        let differing: Vec<usize> = (0..expected.len() / 4)
            .filter(|i| got.data()[i * 4..i * 4 + 4] != expected[i * 4..i * 4 + 4])
            .collect();
        if let Some(&first) = differing.first() {
            let largest = got
                .data()
                .iter()
                .zip(expected)
                .map(|(a, b)| a.abs_diff(*b))
                .max();
            panic!(
                "{what}: {} of {} pixels differ, by up to {largest:?}; the first at ({}, {}) is \
                 {:?}, not {:?}",
                differing.len(),
                expected.len() / 4,
                first % width as usize,
                first / width as usize,
                &got.data()[first * 4..first * 4 + 4],
                &expected[first * 4..first * 4 + 4],
            );
        }
    }

    #[track_caller]
    fn assert_close(what: &str, got: &[f64], expected: &[f64]) {
        assert!(
            got.len() == expected.len()
                && got.iter().zip(expected).all(|(a, b)| (a - b).abs() < 0.01),
            "{what}: {got:?}, not {expected:?}"
        );
    }

    fn opens() {
        let probe = Probe::new();
        let window = &probe.window;
        let scale = probe.host.scale();
        println!("  backing scale factor: {scale}");
        assert!(probe.host.is_visible(), "the runner shows no window");
        assert!(window.is_open());
        assert!(!window.is_app_window());
        assert!(window.raw_handle().is_some());
        assert_eq!(window.size(), size(WIDTH, HEIGHT));
        assert_eq!(window.scale(), scale);
        assert_eq!(probe.take(), [], "the starting state sends no event");
        assert_eq!(probe.host.children(), 1);
        let (width, height) = (f64::from(WIDTH) / scale, f64::from(HEIGHT) / scale);
        assert_close(
            "the view's frame",
            &testing::view_frame(window).expect("open"),
            &[0.0, 150.0 - height, width, height],
        );
        let name = testing::view_class_name(window).expect("open");
        assert!(name.starts_with("ManguebyteView_"), "{name}");
        // A second window of this binary uses the class already declared.
        let second = Window::open(
            probe.host.parent(),
            WindowOptions::new(size(10, 10)),
            |_: &WindowCx<'_>, _| Handled::No,
        )
        .expect("a second view");
        assert_eq!(testing::view_class_name(&second), Some(name));
        assert_eq!(probe.host.children(), 2);
    }

    fn pixels() {
        let probe = Probe::new();
        probe.show();
        assert!(probe.paints() >= 1, "AppKit asked for no paint");
        let expected = shown(&pattern(WIDTH, HEIGHT));
        let window = &probe.window;
        assert_pixels(
            "the layer's image",
            testing::layer_pixels(window),
            &expected,
            WIDTH,
            HEIGHT,
        );
        let paints = probe.paints();
        assert_pixels(
            "the view drawn into a bitmap",
            testing::view_pixels(window),
            &expected,
            WIDTH,
            HEIGHT,
        );
        assert_eq!(probe.paints(), paints, "drawing asks the handler nothing");
        assert_pixels(
            "the layer drawn by Core Animation",
            testing::layer_rendering(window),
            &expected,
            WIDTH,
            HEIGHT,
        );
    }

    fn partial_presents() {
        let probe = Probe::new();
        probe.show();
        let window = &probe.window;
        let converted = window.presented_pixels();
        let mut solid = Pixmap::new(WIDTH, HEIGHT).expect("small");
        solid.fill(Color::rgb(200, 100, 50));
        let part = IRect::new(7, 11, 13, 5);
        window.present(&solid, &[part]).expect("open");
        let bar = IRect::new(100, 70, 100, 100);
        window
            .present_color(Color::rgb(1, 2, 3), &[bar])
            .expect("open");
        assert_eq!(window.presented_pixels() - converted, 13 * 5 + 20 * 10);
        probe.show();
        let mut expected = shown(&pattern(WIDTH, HEIGHT));
        for y in 0..HEIGHT as i64 {
            for x in 0..WIDTH as i64 {
                let i = (y * i64::from(WIDTH) + x) as usize * 4;
                if part.contains(x, y) {
                    expected[i..i + 4].copy_from_slice(&[200, 100, 50, 255]);
                } else if bar.contains(x, y) {
                    expected[i..i + 4].copy_from_slice(&[1, 2, 3, 255]);
                }
            }
        }
        assert_pixels(
            "the layer's image",
            testing::layer_pixels(window),
            &expected,
            WIDTH,
            HEIGHT,
        );
    }

    fn damage() {
        let host = Host::new(200.0, 150.0).expect("a host window");
        let seen = Rc::new(RefCell::new(Vec::new()));
        let handler = {
            let seen = Rc::clone(&seen);
            move |cx: &WindowCx<'_>, event: Event| {
                if let Event::Paint(damage) = event {
                    seen.borrow_mut().push(damage.rects().to_vec());
                    // Invalidated while painting: another paint must come.
                    if seen.borrow().len() == 2 {
                        cx.invalidate(IRect::new(1, 1, 2, 2));
                    }
                }
                Handled::Yes
            }
        };
        let window = Window::open(
            host.parent(),
            WindowOptions::new(size(WIDTH, HEIGHT)),
            handler,
        )
        .expect("a view");
        let wait = || {
            testing::display(&window);
            testing::pump(Duration::from_millis(100));
        };
        wait();
        assert_eq!(*seen.borrow(), [vec![IRect::new(0, 0, WIDTH, HEIGHT)]]);
        window.invalidate(IRect::new(10, 20, 30, 40));
        wait();
        wait();
        assert_eq!(
            seen.borrow()[1..],
            [
                vec![IRect::new(10, 20, 30, 40)],
                vec![IRect::new(1, 1, 2, 2)]
            ]
        );
    }

    fn pointer() {
        let probe = Probe::new();
        let scale = probe.host.scale();
        let shift_command = Modifiers {
            shift: true,
            meta: true,
            ..Modifiers::NONE
        };
        let time = testing::uptime();
        let press = |button, time| {
            probe.send(Input::Press {
                at: at(10, 20),
                button,
                modifiers: shift_command,
                time,
            });
        };
        let release = |button, at| probe.send(Input::Release { at, button });

        press(Button::Primary, time);
        let events = probe.take();
        assert!(events.contains(&Event::Focus(true)), "{events:?}");
        let Some(&Event::PointerDown {
            pointer,
            button: Button::Primary,
            clicks: 1,
        }) = events.last()
        else {
            panic!("{events:?}");
        };
        assert_eq!(pointer.physical, at(10, 20));
        assert_eq!(pointer.modifiers, shift_command);
        assert_close(
            "the logical position",
            &[f64::from(pointer.position.x), f64::from(pointer.position.y)],
            &[10.0 / scale, 20.0 / scale],
        );
        assert!(probe.window.has_capture());

        probe.send(Input::Drag {
            at: at(150, 100),
            button: Button::Primary,
        });
        let events = probe.take();
        assert!(
            matches!(events[..], [Event::PointerMove(p)] if p.physical == at(150, 100)),
            "{events:?}"
        );
        release(Button::Primary, at(150, 100));
        let events = probe.take();
        assert!(
            matches!(
                events[..],
                [
                    Event::PointerUp { pointer, button: Button::Primary },
                    Event::PointerLeave
                ] if pointer.physical == at(150, 100)
            ),
            "{events:?}"
        );
        assert!(!probe.window.has_capture());

        // A double click.
        press(Button::Primary, time + 5.0);
        release(Button::Primary, at(10, 20));
        press(Button::Primary, time + 5.05);
        release(Button::Primary, at(10, 20));
        let clicks: Vec<u32> = probe
            .take()
            .iter()
            .filter_map(|event| match event {
                Event::PointerDown { clicks, .. } => Some(*clicks),
                _ => None,
            })
            .collect();
        assert_eq!(clicks, [1, 2]);

        for button in [Button::Secondary, Button::Middle] {
            press(button, time + 10.0);
            release(button, at(10, 20));
            let events = probe.take();
            assert!(
                matches!(
                    events[..],
                    [
                        Event::PointerDown { pointer: down, button: pressed, .. },
                        Event::PointerUp { button: released, .. },
                    ] if pressed == button && released == button && down.physical == at(10, 20)
                ),
                "{button:?}: {events:?}"
            );
        }

        probe.send(Input::Hover { at: at(30, 40) });
        probe.send(Input::Hover { at: at(500, 500) });
        probe.send(Input::Leave);
        let events = probe.take();
        assert!(
            matches!(
                events[..],
                [Event::PointerMove(p), Event::PointerLeave] if p.physical == at(30, 40)
            ),
            "{events:?}"
        );
    }

    fn focus() {
        let probe = Probe::with(WindowOptions::new(size(WIDTH, HEIGHT)).without_focus_on_click());
        probe.send(Input::Press {
            at: at(5, 5),
            button: Button::Primary,
            modifiers: Modifiers::NONE,
            time: testing::uptime(),
        });
        probe.send(Input::Release {
            at: at(5, 5),
            button: Button::Primary,
        });
        assert!(!probe.take().contains(&Event::Focus(true)));
        assert!(!probe.window.has_focus());
        assert!(!testing::view_is_first_responder(&probe.window));
        probe.window.take_focus();
        assert_eq!(probe.take(), [Event::Focus(true)]);
        assert!(probe.window.has_focus());
        assert!(testing::view_is_first_responder(&probe.window));
        testing::resign_view(&probe.window);
        assert_eq!(probe.take(), [Event::Focus(false)]);
        assert!(!probe.window.has_focus());
    }

    fn wheel() {
        let probe = Probe::new();
        let turns = || -> Vec<(f32, f32)> {
            probe
                .take()
                .iter()
                .filter_map(|event| match event {
                    Event::Wheel { delta, .. } => Some((delta.x, delta.y)),
                    _ => None,
                })
                .collect()
        };
        let scroll = |dx, dy, precise| probe.send(Input::Scroll { dx, dy, precise });
        scroll(0, 1, false);
        scroll(1, 0, false);
        scroll(0, -20, true);
        assert_eq!(turns(), [(0.0, 1.0), (-1.0, 0.0), (0.0, -2.0)]);
        assert_eq!(probe.host.take_passed_on(), PassedOn::default());
        probe.uses.set(Uses::Nothing);
        scroll(0, 1, false);
        assert_eq!(turns(), [(0.0, 1.0)]);
        assert_eq!(
            probe.host.take_passed_on(),
            PassedOn {
                scrolls: 1,
                ..PassedOn::default()
            }
        );
    }

    /// The keys and the text among `events`, in order.
    fn typed(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Key { key, repeat } => Some(format!(
                    "{:?} {}{}{}",
                    key.key,
                    if key.pressed { "down" } else { "up" },
                    if *repeat { " again" } else { "" },
                    if key.modifiers.shift { " shifted" } else { "" },
                )),
                Event::Text(c) => Some(format!("text {c}")),
                _ => None,
            })
            .collect()
    }

    fn keys() {
        let probe = Probe::new();
        probe.window.take_focus();
        probe.take();
        let shift = Modifiers {
            shift: true,
            ..Modifiers::NONE
        };
        probe.key(0, "a", Modifiers::NONE);
        assert_eq!(typed(&probe.take()), ["Character('a') down", "text a"]);
        probe.key(0, "A", shift);
        assert_eq!(
            typed(&probe.take()),
            ["Character('a') down shifted", "text A"]
        );
        probe.key(123, "\u{F702}", Modifiers::NONE);
        probe.key(36, "\r", Modifiers::NONE);
        assert_eq!(typed(&probe.take()), ["Left down", "Enter down"]);
        probe.send(Input::Key {
            code: 0,
            characters: "a",
            modifiers: Modifiers::NONE,
            pressed: true,
            repeat: true,
        });
        probe.send(Input::Key {
            code: 0,
            characters: "a",
            modifiers: Modifiers::NONE,
            pressed: false,
            repeat: false,
        });
        assert_eq!(
            typed(&probe.take()),
            ["Character('a') down again", "text a", "Character('a') up"]
        );
        assert_eq!(probe.host.take_passed_on(), PassedOn::default());

        // The text was used, so the key stays.
        probe.uses.set(Uses::TextOnly);
        probe.key(0, "a", Modifiers::NONE);
        assert_eq!(typed(&probe.take()), ["Character('a') down", "text a"]);
        assert_eq!(probe.host.take_passed_on(), PassedOn::default());

        // Nothing used: the key goes on to the host once.
        probe.uses.set(Uses::Nothing);
        probe.key(0, "a", Modifiers::NONE);
        probe.key(123, "\u{F702}", Modifiers::NONE);
        probe.send(Input::Key {
            code: 0,
            characters: "a",
            modifiers: Modifiers::NONE,
            pressed: false,
            repeat: false,
        });
        probe.send(Input::Modifier {
            code: 56,
            modifiers: shift,
        });
        probe.send(Input::Modifier {
            code: 56,
            modifiers: Modifiers::NONE,
        });
        assert_eq!(
            typed(&probe.take()),
            [
                "Character('a') down",
                "text a",
                "Left down",
                "Character('a') up",
                "Other down shifted",
                "Other up",
            ]
        );
        assert_eq!(
            probe.host.take_passed_on(),
            PassedOn {
                key_downs: 2,
                key_ups: 1,
                flag_changes: 2,
                scrolls: 0,
            }
        );
    }

    fn commands() {
        let probe = Probe::new();
        probe.window.take_focus();
        probe.take();
        let command = Modifiers {
            meta: true,
            ..Modifiers::NONE
        };
        probe.key(8, "c", command);
        assert_eq!(typed(&probe.take()), ["Character('c') down"]);
        assert_eq!(probe.host.take_passed_on(), PassedOn::default());
        probe.uses.set(Uses::Nothing);
        probe.key(8, "c", command);
        assert_eq!(typed(&probe.take()), ["Character('c') down"]);
        assert_eq!(
            probe.host.take_passed_on(),
            PassedOn {
                key_downs: 1,
                ..PassedOn::default()
            }
        );
    }

    fn caret() {
        let probe = Probe::new();
        probe.window.take_focus();
        probe.window.set_text_caret(Some(IRect::new(10, 20, 2, 16)));
        let scale = probe.host.scale();
        // The caret's bottom is 36 pixels below the view's top, and the
        // view's top is the parent's.
        let [x, y] = probe.host.to_screen(10.0 / scale, 150.0 - 36.0 / scale);
        assert_close(
            "the caret",
            &testing::caret_on_screen(&probe.window).expect("open"),
            &[x, y, 2.0 / scale, 16.0 / scale],
        );
    }

    fn timers() {
        let probe = Probe::new();
        let window = &probe.window;
        window.set_timer(TimerId(7), 10).expect("open");
        window.set_timer(TimerId(8), 3_600_000).expect("open");
        testing::pump(Duration::from_millis(300));
        let events = probe.take();
        assert!(events.len() >= 3, "{events:?}");
        assert!(
            events.iter().all(|e| *e == Event::Timer(TimerId(7))),
            "{events:?}"
        );
        assert!(window.kill_timer(TimerId(7)));
        probe.take();
        testing::pump(Duration::from_millis(100));
        assert_eq!(probe.take(), []);
        // They go on while AppKit tracks a menu or a resize.
        window.set_timer(TimerId(9), 10).expect("open");
        probe.take();
        testing::pump_tracking(Duration::from_millis(100));
        let events = probe.take();
        assert!(events.contains(&Event::Timer(TimerId(9))), "{events:?}");
        assert!(window.kill_timer(TimerId(9)));
        // A new period replaces the old.
        window.set_timer(TimerId(8), 10).expect("open");
        testing::pump(Duration::from_millis(100));
        assert!(probe.take().contains(&Event::Timer(TimerId(8))));
    }

    fn resizes() {
        let probe = Probe::new();
        probe.show();
        probe.take();
        let window = &probe.window;
        let scale = probe.host.scale();
        window.resize(size(160, 100)).expect("open");
        assert_eq!(window.size(), size(160, 100));
        assert_eq!(probe.take(), [Event::Resized(size(160, 100))]);
        let (width, height) = (160.0 / scale, 100.0 / scale);
        assert_close(
            "the frame after a resize",
            &testing::view_frame(window).expect("open"),
            &[0.0, 150.0 - height, width, height],
        );
        probe.show();
        assert_pixels(
            "the larger view",
            testing::view_pixels(window),
            &shown(&pattern(160, 100)),
            160,
            100,
        );
        // The parent grows: the view keeps its size at the top-left.
        probe.host.resize_parent(300.0, 250.0);
        assert_close(
            "the frame in a larger parent",
            &testing::view_frame(window).expect("open"),
            &[0.0, 250.0 - height, width, height],
        );
        assert_eq!(probe.take(), []);
        // The host resizes the view itself.
        testing::resize_view_as_host(window, 90.0 / scale, 60.0 / scale);
        assert_eq!(window.size(), size(90, 60));
        assert_eq!(probe.take(), [Event::Resized(size(90, 60))]);
    }

    fn visibility_and_scale() {
        let probe = Probe::with(WindowOptions::new(size(WIDTH, HEIGHT)).hidden());
        let window = &probe.window;
        assert_eq!(testing::view_is_hidden(window), Some(true));
        window.set_visible(true).expect("open");
        assert_eq!(testing::view_is_hidden(window), Some(false));
        window.set_visible(false).expect("open");
        assert_eq!(testing::view_is_hidden(window), Some(true));
        probe.take();
        window.set_host_scale(Some(3.0)).expect("open");
        assert_eq!(probe.take(), [Event::ScaleChanged(3.0)]);
        assert_eq!(window.size(), size(WIDTH, HEIGHT));
        window.set_visible(true).expect("open");
        probe.send(Input::Hover { at: at(30, 60) });
        let events = probe.take();
        assert!(
            matches!(
                events[..],
                [Event::PointerMove(p)]
                    if p.physical == at(30, 60) && p.position.x == 10.0 && p.position.y == 20.0
            ),
            "{events:?}"
        );
    }

    #[track_caller]
    fn assert_closed(probe: &Probe) {
        assert_eq!(probe.take().last(), Some(&Event::Closed));
        let window = &probe.window;
        assert!(!window.is_open());
        assert_eq!(window.raw_handle(), None);
        assert_eq!(window.resize(size(1, 1)), Err(WindowError::Closed));
        assert_eq!(window.set_timer(TimerId(1), 10), Err(WindowError::Closed));
        let frame = pattern(2, 2);
        assert_eq!(window.present_all(&frame), Err(WindowError::Closed));
        testing::pump(Duration::from_millis(50));
        assert_eq!(probe.take(), [], "nothing arrives after Closed");
    }

    fn host_closes() {
        let probe = Probe::new();
        probe.window.take_focus();
        probe.window.set_timer(TimerId(1), 10).expect("open");
        testing::remove_view_as_host(&probe.window);
        assert_closed(&probe);
        assert_eq!(probe.host.children(), 0);

        let probe = Probe::new();
        probe.window.set_timer(TimerId(1), 10).expect("open");
        probe.show();
        probe.host.destroy_parent();
        testing::pump(Duration::from_millis(50));
        let events = probe.take();
        assert!(events.contains(&Event::Closed), "{events:?}");
        assert!(!probe.window.is_open());
    }

    fn own_closes() {
        let probe = Probe::new();
        probe.window.set_timer(TimerId(1), 10).expect("open");
        probe.window.close();
        assert_closed(&probe);
        assert_eq!(probe.host.children(), 0);

        let Probe {
            window,
            host,
            events,
            ..
        } = Probe::new();
        window.take_focus();
        window.set_timer(TimerId(1), 10).expect("open");
        events.borrow_mut().clear();
        drop(window);
        assert_eq!(host.children(), 0);
        testing::pump(Duration::from_millis(50));
        assert_eq!(*events.borrow(), [], "a dropped window hears nothing");

        // Closed from inside the handler, in the middle of a press.
        let host = Host::new(200.0, 150.0).expect("a host window");
        let closed = Rc::new(Cell::new(false));
        let handler = {
            let closed = Rc::clone(&closed);
            move |cx: &WindowCx<'_>, event: Event| {
                match event {
                    Event::PointerDown { .. } => cx.close(),
                    Event::Closed => closed.set(true),
                    _ => {}
                }
                Handled::Yes
            }
        };
        let window =
            Window::open(host.parent(), WindowOptions::new(size(40, 30)), handler).expect("a view");
        assert!(testing::send(
            &window,
            Input::Press {
                at: at(1, 1),
                button: Button::Primary,
                modifiers: Modifiers::NONE,
                time: testing::uptime(),
            }
        ));
        assert!(closed.get());
        assert!(!window.is_open());
        assert_eq!(host.children(), 0);
    }

    fn panics() {
        let host = Host::new(200.0, 150.0).expect("a host window");
        let window = Window::open(
            host.parent(),
            WindowOptions::new(size(40, 30)),
            |_: &WindowCx<'_>, event: Event| -> Handled {
                if matches!(event, Event::PointerMove(_)) {
                    panic!("a handler bug (expected by this check)");
                }
                Handled::No
            },
        )
        .expect("a view");
        assert!(testing::send(&window, Input::Hover { at: at(1, 1) }));
        assert!(window.is_poisoned());
        // Keys go on to the host, since nobody can use them.
        window.take_focus();
        assert!(testing::send(
            &window,
            Input::Key {
                code: 0,
                characters: "a",
                modifiers: Modifiers::NONE,
                pressed: true,
                repeat: false,
            }
        ));
        assert_eq!(host.take_passed_on().key_downs, 1);
    }

    fn refusals() {
        let host = Host::new(200.0, 150.0).expect("a host window");
        let open = |parent| {
            Window::open(
                parent,
                WindowOptions::new(size(4, 4)),
                |_: &WindowCx<'_>, _| Handled::No,
            )
            .err()
        };
        let block = Box::new([0usize; 32]);
        for address in [
            8,
            0x1235,
            usize::MAX - 7,
            std::ptr::from_ref(&*block).addr(),
            host.window_address(),
        ] {
            let error = open(ParentWindow::cocoa(address).expect("not null"));
            assert!(
                matches!(error, Some(WindowError::InvalidParent(_))),
                "{address:#x}: {error:?}"
            );
        }
        assert_eq!(
            open(ParentWindow::x11(7).expect("not zero")),
            Some(WindowError::Unsupported(WindowApi::X11))
        );
        let parent = host.parent();
        let elsewhere = std::thread::spawn(move || {
            Window::open(
                parent,
                WindowOptions::new(size(4, 4)),
                |_: &WindowCx<'_>, _| Handled::No,
            )
            .err()
        })
        .join()
        .expect("the thread ran");
        assert!(
            matches!(elsewhere, Some(WindowError::InvalidParent(_))),
            "{elsewhere:?}"
        );
        assert_eq!(host.children(), 0);
    }

    fn missing() {
        let probe = Probe::with(WindowOptions::new(size(10, 10)).with_file_drops());
        let window = &probe.window;
        assert!(!window.accepts_drops());
        assert_eq!(window.read_clipboard_text(), None);
        assert!(matches!(
            window.write_clipboard_text("text"),
            Err(WindowError::Platform { .. })
        ));
        assert!(matches!(
            window.open_files(&FileDialog::new()),
            Err(WindowError::Platform { .. })
        ));
        assert_eq!(window.set_title("no"), Err(WindowError::NotAppWindow));
    }

    // Two binaries.

    unsafe extern "C" {
        fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
        fn dlsym(library: *mut c_void, name: *const c_char) -> *mut c_void;
    }

    /// `RTLD_NOW | RTLD_LOCAL` (`dlfcn.h` on macOS).
    const LOAD: c_int = 0x2 | 0x4;

    /// The functions of one copy of the `macos_view` example.
    struct Binary {
        open: extern "C" fn(usize, u32, u32) -> *mut c_void,
        class: unsafe extern "C" fn(*const c_void) -> *const c_char,
        presses: unsafe extern "C" fn(*const c_void) -> u32,
        close: unsafe extern "C" fn(*mut c_void),
    }

    impl Binary {
        fn load(path: &Path) -> Binary {
            let name = CString::new(path.to_str().expect("UTF-8")).expect("no NUL");
            // SAFETY: a library this build made, whose initialisers are
            // Rust's; it stays loaded for the life of the process.
            let library = unsafe { dlopen(name.as_ptr(), LOAD) };
            assert!(!library.is_null(), "cannot load {}", path.display());
            let find = |symbol: &CStr| {
                // SAFETY: a loaded library and a NUL-terminated name.
                let found = unsafe { dlsym(library, symbol.as_ptr()) };
                assert!(!found.is_null(), "no symbol {symbol:?}");
                found
            };
            // SAFETY: each symbol is the function of that name in
            // `examples/macos_view.rs`, with this signature.
            unsafe {
                Binary {
                    open: std::mem::transmute::<
                        *mut c_void,
                        extern "C" fn(usize, u32, u32) -> *mut c_void,
                    >(find(c"mangue_view_open")),
                    class: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const c_void) -> *const c_char,
                    >(find(c"mangue_view_class")),
                    presses: std::mem::transmute::<
                        *mut c_void,
                        unsafe extern "C" fn(*const c_void) -> u32,
                    >(find(c"mangue_view_presses")),
                    close: std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut c_void)>(
                        find(c"mangue_view_close"),
                    ),
                }
            }
        }
    }

    /// The `macos_view` example's library, beside this test's own folder.
    fn example_library() -> PathBuf {
        let test = std::env::current_exe().expect("this test's path");
        let examples = test
            .parent()
            .and_then(Path::parent)
            .expect("the target folder")
            .join("examples");
        let library = examples.join("libmacos_view.dylib");
        assert!(
            library.is_file(),
            "{} is not built: run `cargo test -p manguebyte-window`, or `cargo build -p \
             manguebyte-window --example macos_view` first",
            library.display()
        );
        library
    }

    fn two_binaries() {
        let library = example_library();
        let folder =
            std::env::temp_dir().join(format!("manguebyte-two-binaries-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("a temporary folder");
        let binaries: Vec<Binary> = ["first.dylib", "second.dylib"]
            .iter()
            .map(|name| {
                let copy = folder.join(name);
                std::fs::copy(&library, &copy).expect("a copy of the library");
                Binary::load(&copy)
            })
            .collect();
        // This test's own view makes a third copy of the crate.
        let own = Probe::new();
        let mut classes = vec![testing::view_class_name(&own.window).expect("open")];
        let open: Vec<(Host, *mut c_void)> = binaries
            .iter()
            .map(|binary| {
                let host = Host::new(200.0, 150.0).expect("a host window");
                let ParentWindow::Cocoa { ns_view } = host.parent() else {
                    panic!("a Cocoa parent");
                };
                let view = (binary.open)(ns_view.get(), 100, 60);
                assert!(!view.is_null(), "the library opened no view");
                (host, view)
            })
            .collect();
        for (binary, (_, view)) in binaries.iter().zip(&open) {
            // SAFETY: a view that library opened and has not closed; the
            // name lives as long as it.
            let class = unsafe { CStr::from_ptr((binary.class)(*view)) };
            classes.push(class.to_string_lossy().into_owned());
        }
        println!("  classes: {classes:?}");
        assert_ne!(classes[0], classes[1]);
        assert_ne!(classes[0], classes[2]);
        assert_ne!(classes[1], classes[2]);
        // Each view gets its own presses: near the host's top-left corner,
        // where the views are.
        open[0].0.click(2.0, 148.0);
        open[1].0.click(2.0, 148.0);
        open[1].0.click(2.0, 148.0);
        for (binary, ((_, view), expected)) in binaries.iter().zip(open.iter().zip([1, 2])) {
            // SAFETY: as above.
            assert_eq!(unsafe { (binary.presses)(*view) }, expected);
        }
        for (binary, (_, view)) in binaries.iter().zip(&open) {
            // SAFETY: closed once, by the library that opened it.
            unsafe { (binary.close)(*view) };
        }
        assert_eq!(open[0].0.children(), 0);
        let _ = std::fs::remove_dir_all(&folder);
    }
}
