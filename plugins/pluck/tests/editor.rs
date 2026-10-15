//! Mangue Pluck's editor, the smallest there is: opened headless through
//! the plugin harness and `manguebyte-gui`, with a host that records every
//! call.

use std::cell::RefCell;
use std::rc::Rc;

use mangue_pluck::{EDITOR_SIZE, Pluck};
use manguebyte::ParamId;
use manguebyte::plugin::editor::testing::{HostCall, RecordingEditorHost};
use manguebyte::plugin::editor::{EditorContext, EditorHost, EditorSpec, ParentWindow};
use manguebyte::plugin::{Harness, Plugin};
use manguebyte_gui::HeadlessEditor;
use manguebyte_ui::Role;

thread_local! {
    static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

#[test]
fn the_editor_shows_the_level_and_a_drag_reaches_the_host() {
    let mut harness = Harness::<Pluck>::new(48_000.0, 256);
    let spec = harness
        .plugin_mut()
        .editor_spec()
        .on_headless_open(|ui| OPENED.with(|slot| *slot.borrow_mut() = Some(ui)));
    let offered = harness.editor().expect("Mangue Pluck has an editor");
    assert_eq!(offered.size(1.0), EDITOR_SIZE);
    assert_eq!(offered.size(2.0), spec.size(2.0));

    let host = Rc::new(RecordingEditorHost::new().with_params(harness.plugin_mut().params()));
    let host_dyn: Rc<dyn EditorHost> = host.clone();
    let (context, link) = EditorContext::attach(host_dyn);
    let parent = ParentWindow::win32(0x10).expect("not null");
    let editor = spec
        .open(parent, Some(2.0), context)
        .expect("a headless editor always opens");
    let ui = OPENED
        .with(|slot| slot.borrow_mut().take())
        .expect("the hook ran");
    assert_eq!(ui.tick(), 1);
    assert_eq!(editor.size(), EDITOR_SIZE.scale_by(2.0));

    let shown: Vec<(Role, String, Option<String>)> = ui
        .access_tree()
        .into_iter()
        .filter(|entry| entry.node.role != Role::Group)
        .map(|entry| (entry.node.role, entry.node.name, entry.node.value))
        .collect();
    assert_eq!(
        shown,
        [
            (Role::Label, "Mangue Pluck".to_owned(), None),
            (
                Role::Slider,
                "Level".to_owned(),
                Some("-12.0 dB".to_owned())
            ),
        ]
    );

    // Twenty logical pixels up: a tenth of -40 to 6 dB, 4.6 dB.
    let at = ui
        .find(Role::Slider, "Level")
        .expect("a knob")
        .bounds
        .center();
    ui.press(at);
    ui.move_to(manguebyte_gfx::Point::new(at.x, at.y - 20.0));
    ui.release(manguebyte_gfx::Point::new(at.x, at.y - 20.0));
    let level = ParamId::from_name("level");
    let calls = host.take_calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], HostCall::BeginGesture(level));
    assert!(
        matches!(calls[1], HostCall::Perform(param, value)
            if param == level && (value - (28.0 / 46.0 + 0.1)).abs() < 1e-6),
        "{calls:?}"
    );
    assert_eq!(calls[2], HostCall::EndGesture(level));
    assert!((harness.params().level.value() - -7.4).abs() < 1e-4);
    ui.tick();
    assert_eq!(
        ui.find(Role::Slider, "Level")
            .expect("a knob")
            .node
            .value
            .as_deref(),
        Some("-7.4 dB")
    );

    drop(editor);
    drop(link);
    assert!(!ui.is_open());
    assert_eq!(host.well_formed(), Ok(()));
}
