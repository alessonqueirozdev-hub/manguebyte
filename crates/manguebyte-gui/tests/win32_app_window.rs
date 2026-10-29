//! An interface in a real Win32 application window: its pixels read back,
//! files dropped through the window's registered `IDropTarget`, and the
//! close button.
//!
//! A read-back fails when this session cannot read windows back, unless
//! `MANGUEBYTE_WINDOW_SKIP_READ_BACK` is set (CI never sets it); then it
//! says so on stderr and skips those checks.

#![cfg(windows)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use manguebyte_gfx::{Color, IRect, Pixmap, Scene, Size};
use manguebyte_gui::open_app_window;
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::{
    AccessCx, DragEvent, DropData, EventCx, Handled, MeasureCx, PaintCx, Theme, Widget,
};
use manguebyte_window::win32::testing::{self, Drag, Message, ReadBack, TopLevel};
use manguebyte_window::{App, AppWindowOptions, DragData, PhysicalPoint, RunEnd};

/// `DROPEFFECT_COPY`.
const COPY: u32 = 1;

type Dropped = Arc<Mutex<Vec<PathBuf>>>;

/// Fills the window in a colour of its own and keeps the files dropped.
struct Bin {
    dropped: Dropped,
}

impl Widget for Bin {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "Samples")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(10.0, 10.0)
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        scene.fill(cx.rect(), Color::rgb(30, 160, 90));
    }

    fn drag(&mut self, _cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        match event {
            DragEvent::Over { payload, .. } if payload.get::<DropData>().is_some() => Handled::Yes,
            DragEvent::Drop { payload, .. } => match payload.get::<DropData>() {
                Some(data) => {
                    self.dropped
                        .lock()
                        .expect("one thread")
                        .extend(data.files.iter().cloned());
                    Handled::Yes
                }
                None => Handled::No,
            },
            _ => Handled::No,
        }
    }
}

#[test]
fn an_interface_in_an_application_window_shows_takes_drops_and_closes() {
    let probe = TopLevel::new(64, 64).expect("a desktop");
    let app = App::new().expect("Win32");
    let dropped = Dropped::default();
    let editor = open_app_window(
        &app,
        AppWindowOptions::new("Samples", Size::new(160.0, 100.0))
            .at(PhysicalPoint::new(80, 80))
            .without_activation(),
        Theme::default(),
        None,
        Bin {
            dropped: Arc::clone(&dropped),
        },
    )
    .expect("an application window");
    let window = editor.window();
    let hwnd = window.raw_handle().expect("open");
    assert!(window.accepts_drops(), "the application initialised OLE");
    assert_eq!(testing::window_title(hwnd).as_deref(), Some("Samples"));
    // Frames run from the window's timer on the thread's loop.
    testing::pump(Duration::from_millis(200));
    if testing::require_read_back(&probe, IRect::new(0, 0, 32, 32)) {
        let shown = testing::read_back(&window.cx(), ReadBack::Screen).expect("our window");
        let mut want = Pixmap::new(1, 1).expect("small");
        want.fill(Color::rgb(30, 160, 90));
        for (x, y) in [(2, 2), (150, 90), (80, 50)] {
            assert_eq!(
                shown.pixel(x, y).map(|p| p.to_array()),
                want.pixel(0, 0).map(|p| p.to_array()),
                "({x}, {y})"
            );
        }
    }

    let sample = PathBuf::from(r"C:\Samples\Rim.wav");
    let drag = Drag::new(&window.cx(), &DragData::files([sample.clone()])).expect("a target");
    let at = PhysicalPoint::new(40, 30);
    assert_eq!(drag.enter(at, 0, COPY), COPY);
    assert_eq!(drag.over(PhysicalPoint::new(41, 31), 0, COPY), COPY);
    assert_eq!(drag.drop_at(PhysicalPoint::new(41, 31), 0, COPY), COPY);
    assert_eq!(*dropped.lock().expect("one thread"), [sample]);

    // The close button closes it: no answer was set, so the window agrees.
    testing::send(hwnd, Message::Close).expect("ours");
    assert!(!editor.is_open());
    assert!(!testing::is_window(hwnd));
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
}
