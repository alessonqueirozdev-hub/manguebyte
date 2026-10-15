//! Mangue Pluck's editor, a knob for each parameter: opened headless
//! through the plugin harness and `manguebyte-gui`, with a host that
//! records every call.

use std::cell::RefCell;
use std::rc::Rc;

use mangue_pluck::{EDITOR_SIZE, Pluck};
use manguebyte::ParamId;
use manguebyte::plugin::editor::testing::{HostCall, RecordingEditorHost};
use manguebyte::plugin::editor::{
    EditorContext, EditorHost, EditorSpec, HostLink, OpenEditor, ParentWindow,
};
use manguebyte::plugin::{Harness, Plugin};
use manguebyte_gfx::{IRect, Rect};
use manguebyte_gui::HeadlessEditor;
use manguebyte_ui::Role;

thread_local! {
    static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

/// Opens the editor of the plugin in `harness` at `scale`.
fn open(
    harness: &mut Harness<Pluck>,
    host: &Rc<RecordingEditorHost>,
    scale: f64,
) -> (Box<dyn OpenEditor>, HostLink, HeadlessEditor) {
    let spec = harness
        .plugin_mut()
        .editor_spec()
        .on_headless_open(|ui| OPENED.with(|slot| *slot.borrow_mut() = Some(ui)));
    let host_dyn: Rc<dyn EditorHost> = host.clone();
    let (context, link) = EditorContext::attach(host_dyn);
    let parent = ParentWindow::win32(0x10).expect("not null");
    let editor = spec
        .open(parent, Some(scale), context)
        .expect("a headless editor always opens");
    let ui = OPENED
        .with(|slot| slot.borrow_mut().take())
        .expect("the hook ran");
    assert_eq!(ui.tick(), 1);
    (editor, link, ui)
}

#[test]
fn the_editor_shows_every_parameter_and_a_drag_reaches_the_host() {
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
    let slider = |name: &str, value: &str| (Role::Slider, name.to_owned(), Some(value.to_owned()));
    assert_eq!(
        shown,
        [
            (Role::Label, "Mangue Pluck".to_owned(), None),
            slider("Cutoff", "1800 Hz"),
            slider("Resonance", "2.00"),
            slider("Decay", "0.30 s"),
            slider("Release", "0.40 s"),
            slider("Reverb", "0.25"),
            slider("Level", "-12.0 dB"),
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

/// Every control lies inside the window, clear of the others, at every
/// scale from 1 to 2, in the same place but for rounding.
#[test]
fn the_layout_fits_at_every_scale_from_1_to_2() {
    let controls = |ui: &HeadlessEditor| -> Vec<(String, Rect, IRect)> {
        ui.access_tree()
            .into_iter()
            .filter(|entry| entry.node.role != Role::Group)
            .map(|entry| (entry.node.name, entry.bounds, entry.device))
            .collect()
    };
    let mut reference = None;
    for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
        let mut harness = Harness::<Pluck>::new(48_000.0, 256);
        let host = Rc::new(RecordingEditorHost::new().with_params(harness.plugin_mut().params()));
        let (editor, link, ui) = open(&mut harness, &host, scale);
        let size = EDITOR_SIZE.scale_by(scale);
        assert_eq!(editor.size(), size, "at {scale}");
        let found = controls(&ui);
        assert_eq!(found.len(), 7, "at {scale}");
        let window = IRect::new(0, 0, size.width, size.height);
        let logical = Rect::new(
            2.0,
            2.0,
            EDITOR_SIZE.width as f32 - 4.0,
            EDITOR_SIZE.height as f32 - 4.0,
        );
        let at_one: &Vec<(String, Rect, IRect)> = reference.get_or_insert_with(|| found.clone());
        for (index, (name, bounds, device)) in found.iter().enumerate() {
            assert!(
                window.contains_rect(device),
                "{name} at {scale}: {device:?}"
            );
            assert!(
                bounds.x >= logical.x
                    && bounds.y >= logical.y
                    && bounds.x + bounds.width <= logical.x + logical.width
                    && bounds.y + bounds.height <= logical.y + logical.height,
                "{name} at {scale}: {bounds:?}"
            );
            assert!(
                bounds.width > 0.0 && bounds.height > 0.0,
                "{name} at {scale}"
            );
            let (other_name, one, _) = &at_one[index];
            assert_eq!(name, other_name);
            for (a, b) in [
                (bounds.x, one.x),
                (bounds.y, one.y),
                (bounds.width, one.width),
                (bounds.height, one.height),
            ] {
                assert!((a - b).abs() <= 1.5, "{name} at {scale}: {bounds:?}");
            }
            for (other, other_bounds, other_device) in &found[index + 1..] {
                assert!(
                    bounds
                        .intersect(other_bounds)
                        .is_none_or(|r| r.width * r.height == 0.0),
                    "{name} and {other} overlap at {scale}"
                );
                assert!(
                    device
                        .intersect(other_device)
                        .is_none_or(|r| r.width * r.height == 0),
                    "{name} and {other} overlap in device pixels at {scale}"
                );
            }
        }
        drop(editor);
        drop(link);
        assert!(!ui.is_open());
        assert_eq!(host.take_calls(), []);
    }
}
