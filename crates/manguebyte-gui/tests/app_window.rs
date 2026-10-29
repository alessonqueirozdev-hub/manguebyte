//! Interfaces in application windows, on the headless backend: an editor
//! run as a standalone program's window, its loop and close requests,
//! gestures with no host, files dropped onto widgets, and dialogs.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use manguebyte_gfx::{Point, Scene, Size};
use manguebyte_gui::{Editor, HeadlessEditor, open_app_window};
use manguebyte_params::{FloatParam, FloatRange, ParamSet, param_set};
use manguebyte_plugin::editor::testing::RecordingEditorHost;
use manguebyte_plugin::editor::{EditorContext, EditorSpec, HostSize, ParentWindow, Resize};
use manguebyte_ui::access::{AccessNode, Role};
use manguebyte_ui::{
    AccessCx, DragEvent, DropData, EventCx, Handled, IntoView, MeasureCx, PaintCx, ParamHandle,
    Theme, Widget, knob, label, row,
};
use manguebyte_window::{App, AppWindowOptions, DragData, FileDialog, PhysicalSize, RunEnd};

struct Params {
    gain: FloatParam,
}
param_set!(Params { gain });

fn params() -> Arc<Params> {
    Arc::new(Params {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5).with_precision(2),
    })
}

/// Files dropped on the bin, kept where a test reads them.
type Dropped = Arc<Mutex<Vec<PathBuf>>>;

/// A widget that takes drags carrying `.wav` files and keeps them.
struct Bin {
    dropped: Dropped,
}

impl Widget for Bin {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Group, "Samples")
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(80.0, 80.0)
    }

    fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}

    fn drag(&mut self, _cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let wav = |payload: &manguebyte_ui::DragPayload| {
            payload
                .get::<DropData>()
                .filter(|data| data.has_file_with_extension(&["wav"]))
                .cloned()
        };
        match event {
            DragEvent::Over { payload, .. } if wav(payload).is_some() => Handled::Yes,
            DragEvent::Drop { payload, .. } => match wav(payload) {
                Some(data) => {
                    self.dropped.lock().expect("one thread").extend(data.files);
                    Handled::Yes
                }
                None => Handled::No,
            },
            _ => Handled::No,
        }
    }
}

/// An editor with a gain knob and a sample bin, resizable.
fn editor(params: &Arc<Params>, dropped: &Dropped) -> Editor {
    let (params, dropped) = (Arc::clone(params), Arc::clone(dropped));
    Editor::new(HostSize::new(200, 100), move || {
        row((
            knob(ParamHandle::new(Arc::clone(&params), |p| &p.gain)),
            Bin {
                dropped: Arc::clone(&dropped),
            },
        ))
        .padding(8.0)
    })
    .with_resize(Resize::Free {
        min: HostSize::new(150, 80),
        max: HostSize::new(400, 200),
    })
}

fn center(ui: &HeadlessEditor, role: Role, name: &str) -> Point {
    ui.find(role, name).expect("drawn").bounds.center()
}

#[test]
fn an_editor_opens_in_an_application_window_and_closes_on_request() {
    let (app, test) = App::headless();
    assert!(test.set_system_scale(1.5));
    let params = params();
    let dropped = Dropped::default();
    let as_set: Arc<dyn ParamSet> = params.clone();
    let editor = editor(&params, &dropped)
        .open_in_app(&app, "Gain", Some(as_set))
        .expect("headless windows always open");
    let window = editor.window();
    assert!(window.is_app_window());
    assert_eq!(window.title().as_deref(), Some("Gain"));
    assert_eq!(window.size(), PhysicalSize::new(300, 150));
    assert_eq!(
        window.size_limits(),
        Some((PhysicalSize::new(225, 120), PhysicalSize::new(600, 300)))
    );
    assert!(window.accepts_drops());
    let ui = editor.headless().expect("headless");
    assert_eq!(ui.tick(), 1, "frames start at once");
    assert_eq!(ui.layout(), Some((Size::new(200.0, 100.0), 1.5)));
    assert!(ui.find(Role::Slider, "Gain").is_some());

    let controller = ui.window().clone();
    test.post(move || {
        controller.request_close();
    });
    assert_eq!(app.run(), Ok(RunEnd::LastWindowClosed));
    assert!(!editor.is_open());
    assert!(!ui.is_open());
}

#[test]
fn gestures_with_no_host_set_the_parameters() {
    let (app, _test) = App::headless();
    let params = params();
    let as_set: Arc<dyn ParamSet> = params.clone();
    let editor = editor(&params, &Dropped::default())
        .open_in_app(&app, "Gain", Some(as_set))
        .expect("open");
    let ui = editor.headless().expect("headless");
    ui.tick();
    let at = center(&ui, Role::Slider, "Gain");
    ui.press(at);
    ui.move_to(Point::new(at.x, at.y - 40.0));
    ui.release(Point::new(at.x, at.y - 40.0));
    assert!(params.gain.value() > 0.6, "{}", params.gain.value());
    ui.tick();
    assert_eq!(
        ui.find(Role::Slider, "Gain")
            .and_then(|knob| knob.node.value),
        Some(format!("{:.2}", params.gain.value()))
    );
}

#[test]
fn files_dropped_on_the_window_reach_the_widget_under_the_pointer() {
    let (app, _test) = App::headless();
    let dropped = Dropped::default();
    let editor = editor(&params(), &dropped)
        .open_in_app(&app, "Drops", None)
        .expect("open");
    let ui = editor.headless().expect("headless");
    ui.tick();
    let bin = center(&ui, Role::Group, "Samples");
    let knob = center(&ui, Role::Slider, "Gain");
    let kit = DragData::files([r"C:\kits\kick.wav", r"C:\kits\snare.wav"]);

    // Over the knob, nothing takes it ...
    assert!(!ui.drag_enter(knob, kit.clone()));
    assert!(!ui.drag_over(knob));
    // ... over the bin, the bin does.
    assert!(ui.drag_over(bin));
    assert!(ui.drop_at(bin));
    assert_eq!(
        *dropped.lock().expect("one thread"),
        [
            PathBuf::from(r"C:\kits\kick.wav"),
            PathBuf::from(r"C:\kits\snare.wav")
        ]
    );
    // Text is refused, and nothing more is dropped.
    assert!(!ui.drag_enter(bin, DragData::text("words")));
    assert!(!ui.drop_at(bin));
    assert_eq!(dropped.lock().expect("one thread").len(), 2);
}

#[test]
fn a_close_request_the_application_refuses_leaves_it_open() {
    let (app, test) = App::headless();
    let asked = Rc::new(RefCell::new(0));
    let editor = open_app_window(
        &app,
        AppWindowOptions::new("Unsaved", Size::new(120.0, 60.0)),
        Theme::default(),
        None,
        label("Unsaved changes"),
    )
    .expect("open");
    {
        let asked = Rc::clone(&asked);
        editor.on_close_request(move |_| {
            *asked.borrow_mut() += 1;
            *asked.borrow() >= 2
        });
    }
    let ui = editor.headless().expect("headless");
    let controller = ui.window().clone();
    test.post(move || {
        controller.request_close();
    });
    assert_eq!(app.run(), Ok(RunEnd::Idle));
    assert!(editor.is_open());
    ui.window().request_close();
    assert_eq!(*asked.borrow(), 2);
    assert!(!editor.is_open());
}

#[test]
fn the_interface_asks_for_files_through_the_windows_dialogs() {
    let (app, _test) = App::headless();
    let editor = open_app_window(
        &app,
        AppWindowOptions::new("Dialogs", Size::new(120.0, 60.0)),
        Theme::default(),
        None,
        label("Load"),
    )
    .expect("open");
    let dialogs = editor.dialogs();
    let window = editor.headless().expect("headless");
    window
        .window()
        .answer_next_dialog(Some(vec![PathBuf::from("a.wav"), PathBuf::from("b.wav")]));
    let picked = dialogs
        .open_files(&FileDialog::new().with_filter("Audio", ["wav"]).multiple())
        .expect("open");
    assert_eq!(picked.map(|files| files.len()), Some(2));
    assert_eq!(window.window().take_dialogs().len(), 1);
}

#[test]
fn a_move_to_another_monitor_redraws_at_its_scale_with_no_resize_of_its_own() {
    let (app, _test) = App::headless();
    let editor = editor(&params(), &Dropped::default())
        .open_in_app(&app, "Move", None)
        .expect("open");
    let ui = editor.headless().expect("headless");
    ui.tick();
    assert!(ui.window().move_to_monitor(2.0));
    ui.tick();
    assert_eq!(editor.window().size(), PhysicalSize::new(400, 200));
    assert_eq!(ui.layout(), Some((Size::new(200.0, 100.0), 2.0)));
    let shown = ui.shown().expect("open");
    assert_eq!((shown.width(), shown.height()), (400, 200));
}

thread_local! {
    static CHILD: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

#[test]
fn an_editor_inside_a_host_takes_drops_only_when_asked() {
    for asked in [false, true] {
        let dropped = Dropped::default();
        let mut spec = editor(&params(), &dropped)
            .on_headless_open(|ui| CHILD.with(|slot| *slot.borrow_mut() = Some(ui)));
        if asked {
            spec = spec.with_file_drops();
        }
        let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
        let parent = ParentWindow::win32(0x10).expect("not null");
        let mut opened = spec.open(parent, Some(1.0), context).expect("headless");
        opened.set_visible(true);
        let ui = CHILD
            .with(|slot| slot.borrow_mut().take())
            .expect("the hook ran");
        ui.tick();
        let bin = center(&ui, Role::Group, "Samples");
        assert_eq!(ui.drag_enter(bin, DragData::files(["hat.wav"])), asked);
        assert_eq!(ui.drop_at(bin), asked);
        assert_eq!(
            dropped.lock().expect("one thread").len(),
            usize::from(asked)
        );
        drop(opened);
        drop(link);
    }
}
