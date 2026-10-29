//! System objects across many windows, and windows that outlive their
//! parents. These count objects of the whole process, so they run in a
//! test binary of their own, one at a time.

#![cfg(windows)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Mutex, MutexGuard, PoisonError};

use manguebyte_gfx::{Color, Pixmap};
use manguebyte_window::win32::testing::{self, Message, TopLevel};
use manguebyte_window::{
    Button, Event, Handled, PhysicalPoint, PhysicalSize, TimerId, Window, WindowCx, WindowError,
    WindowOptions,
};

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One window's whole life: made, painted, clicked, ticked, dropped.
fn cycle(parent: &TopLevel, frame: &Pixmap) {
    let window = Window::open(
        parent.parent(),
        WindowOptions::new(PhysicalSize::new(64, 48)),
        |window: &WindowCx<'_>, event| {
            if let Event::Paint(damage) = event {
                let mut frame = Pixmap::new(64, 48).expect("small");
                frame.fill(Color::rgb(40, 50, 60));
                window.present(&frame, damage.rects()).expect("open");
            }
            Handled::Yes
        },
    )
    .expect("a child window");
    let hwnd = window.raw_handle().expect("open");
    assert!(testing::class_registered());
    testing::update(hwnd);
    window.present_all(frame).expect("open");
    window.set_timer(TimerId(1), 50).expect("open");
    let at = PhysicalPoint::new(5, 6);
    testing::send(
        hwnd,
        Message::ButtonDown {
            button: Button::Primary,
            at,
            shift: false,
            control: false,
        },
    );
    testing::send(
        hwnd,
        Message::MouseMove {
            at,
            shift: false,
            control: false,
        },
    );
    testing::send(
        hwnd,
        Message::ButtonUp {
            button: Button::Primary,
            at,
        },
    );
    testing::send(hwnd, Message::KillFocus);
    drop(window);
    assert!(!testing::is_window(hwnd));
}

#[test]
fn a_thousand_windows_leave_no_objects_and_no_class_behind() {
    let _serial = serial();
    let parent = TopLevel::new(100, 80).expect("a desktop");
    let mut frame = Pixmap::new(64, 48).expect("small");
    frame.fill(Color::rgb(200, 100, 0));
    // Let the system create whatever it caches on first use.
    for _ in 0..20 {
        cycle(&parent, &frame);
    }
    testing::pump(std::time::Duration::from_millis(20));
    let before = testing::gui_resources();
    for _ in 0..1000 {
        cycle(&parent, &frame);
    }
    testing::pump(std::time::Duration::from_millis(20));
    let after = testing::gui_resources();
    eprintln!("GDI and USER objects: {before:?} before 1,000 windows, {after:?} after");
    // A leak of one object per window would show as 1,000 more; the
    // system may free something it cached on first use, so fewer is fine.
    assert!(
        after.gdi <= before.gdi && after.user <= before.user,
        "{before:?} before 1,000 windows, {after:?} after"
    );
    // The last window took the class with it, while the parent remains.
    assert!(!testing::class_registered());
    assert!(parent.is_open());
}

/// One application window's whole life: opened hidden with drops, shown,
/// painted, dragged onto, asked to close, closed and dropped.
fn app_cycle(app: &manguebyte_window::App) {
    use manguebyte_gfx::Size;
    use manguebyte_window::{AppWindowOptions, DragData};
    let window = app
        .open_window(
            AppWindowOptions::new("Cycle", Size::new(64.0, 48.0))
                .hidden()
                .without_activation()
                .with_file_drops(),
            |cx: &WindowCx<'_>, event| {
                match event {
                    Event::Paint(damage) => {
                        let mut frame = Pixmap::new(64, 48).expect("small");
                        frame.fill(Color::rgb(40, 50, 60));
                        cx.present(&frame, damage.rects()).expect("open");
                    }
                    Event::CloseRequested => cx.close(),
                    _ => {}
                }
                Handled::Yes
            },
        )
        .expect("an application window");
    let hwnd = window.raw_handle().expect("open");
    window.set_visible(true).expect("open");
    testing::update(hwnd);
    let drag = testing::Drag::new(&window.cx(), &DragData::files([r"C:\cycle.wav"]))
        .expect("a drop target");
    let at = PhysicalPoint::new(5, 6);
    assert_ne!(drag.enter(at, 0, 1), 0);
    assert_ne!(drag.drop_at(at, 0, 1), 0);
    testing::send(hwnd, Message::Close);
    assert!(!window.is_open());
    drop(drag);
    drop(window);
    assert!(!testing::is_window(hwnd));
}

#[test]
fn three_hundred_application_windows_leave_no_objects_and_no_class_behind() {
    let _serial = serial();
    let app = manguebyte_window::App::new().expect("Win32");
    for _ in 0..20 {
        app_cycle(&app);
    }
    testing::pump(std::time::Duration::from_millis(20));
    let before = testing::gui_resources();
    for _ in 0..300 {
        app_cycle(&app);
    }
    testing::pump(std::time::Duration::from_millis(20));
    let after = testing::gui_resources();
    eprintln!("GDI and USER objects: {before:?} before 300 windows, {after:?} after");
    assert!(
        after.gdi <= before.gdi && after.user <= before.user,
        "{before:?} before 300 windows, {after:?} after"
    );
    assert!(!testing::class_registered());
    assert_eq!(app.open_windows(), 0);
}

#[test]
fn a_window_outlives_its_parent() {
    let _serial = serial();
    let log: Rc<RefCell<Vec<Event>>> = Rc::default();
    let parent = TopLevel::new(100, 80).expect("a desktop");
    let window = {
        let log = Rc::clone(&log);
        Window::open(
            parent.parent(),
            WindowOptions::new(PhysicalSize::new(50, 40)),
            move |_: &WindowCx<'_>, event| {
                log.borrow_mut().push(event);
                Handled::Yes
            },
        )
        .expect("a child window")
    };
    window.set_timer(TimerId(3), 10).expect("open");
    let hwnd = window.raw_handle().expect("open");
    log.borrow_mut().clear();

    // The host destroys its window, and ours with it.
    parent.destroy();
    assert!(!testing::is_window(hwnd));
    assert_eq!(*log.borrow(), [Event::Closed]);
    assert!(!window.is_open());
    assert_eq!(window.raw_handle(), None);
    let frame = Pixmap::new(50, 40).expect("small");
    assert_eq!(window.present_all(&frame), Err(WindowError::Closed));
    assert_eq!(
        window.resize(PhysicalSize::new(5, 5)),
        Err(WindowError::Closed)
    );
    assert_eq!(window.set_timer(TimerId(4), 10), Err(WindowError::Closed));
    window.invalidate_all();
    testing::pump(std::time::Duration::from_millis(60));
    assert_eq!(log.borrow().len(), 1, "nothing after Closed");

    // The value is dropped much later; the class goes with it.
    drop(window);
    assert!(!testing::class_registered());
}

/// Something else registered a class under this binary's window class
/// name with another window procedure: the window refuses to use it
/// rather than trust what its windows would hold, and works again once
/// the class is gone.
#[test]
#[allow(unsafe_code)]
fn a_class_of_our_name_with_another_procedure_is_refused() {
    use windows_sys::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DefWindowProcW, RegisterClassExW, UnregisterClassW, WNDCLASSEXW,
    };

    let _serial = serial();
    let parent = TopLevel::new(50, 50).expect("a desktop");
    assert!(!testing::class_registered());
    let name: Vec<u16> = testing::window_class_name()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: no arguments beyond null: the module of this test program,
    // where the crate is linked.
    let module = unsafe { GetModuleHandleW(std::ptr::null()) };
    let impostor = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(DefWindowProcW),
        cbWndExtra: 64,
        hInstance: module,
        lpszClassName: name.as_ptr(),
        ..WNDCLASSEXW::default()
    };
    // SAFETY: a fully initialised class whose name outlives the call.
    assert_ne!(unsafe { RegisterClassExW(&impostor) }, 0);
    let refused = Window::open(
        parent.parent(),
        WindowOptions::new(PhysicalSize::new(10, 10)),
        |_: &WindowCx<'_>, _| Handled::Yes,
    );
    // SAFETY: the class registered above, with no windows.
    assert_ne!(unsafe { UnregisterClassW(name.as_ptr(), module) }, 0);
    assert_eq!(
        refused.err(),
        Some(WindowError::Platform {
            call: "RegisterClassExW",
            code: ERROR_CLASS_ALREADY_EXISTS
        })
    );
    let window = Window::open(
        parent.parent(),
        WindowOptions::new(PhysicalSize::new(10, 10)),
        |_: &WindowCx<'_>, _| Handled::Yes,
    )
    .expect("our own class");
    drop(window);
    assert!(!testing::class_registered());
}

#[test]
fn many_windows_at_once_share_one_class_until_the_last_goes() {
    let _serial = serial();
    let parent = TopLevel::new(300, 300).expect("a desktop");
    let before = testing::gui_resources();
    let mut windows: Vec<Window> = (0..50)
        .map(|i| {
            Window::open(
                parent.parent(),
                WindowOptions::new(PhysicalSize::new(10 + i, 10)),
                |_: &WindowCx<'_>, _| Handled::Yes,
            )
            .expect("a child window")
        })
        .collect();
    let during = testing::gui_resources();
    assert!(
        during.user >= before.user + 50,
        "{before:?} then {during:?}"
    );
    while let Some(window) = windows.pop() {
        assert!(testing::class_registered());
        drop(window);
    }
    assert!(!testing::class_registered());
    let after = testing::gui_resources();
    assert!(
        after.gdi <= before.gdi && after.user <= before.user,
        "{before:?} before 50 windows, {after:?} after"
    );
}
