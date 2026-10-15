//! What the editor tests share: the plugin in the harness, its editor
//! opened headless through `manguebyte-gui` on a host that records every
//! call, and short ways to find widgets.

#![allow(dead_code)]

use std::cell::RefCell;
use std::rc::Rc;

use mangue_duck::Duck;
use manguebyte::AudioBuffer;
use manguebyte::ParamId;
use manguebyte::plugin::editor::testing::RecordingEditorHost;
use manguebyte::plugin::editor::{EditorContext, EditorSpec, HostLink, OpenEditor, ParentWindow};
use manguebyte::plugin::{Harness, Plugin};
use manguebyte_gui::HeadlessEditor;
use manguebyte_ui::{AccessEntry, Role};

pub const RATE: f64 = 48_000.0;
pub const BLOCK: usize = 256;

pub const THRESHOLD: ParamId = ParamId::from_name("threshold");
pub const DEPTH: ParamId = ParamId::from_name("depth");
pub const ATTACK: ParamId = ParamId::from_name("attack");
pub const RELEASE: ParamId = ParamId::from_name("release");
pub const BYPASS: ParamId = ParamId::from_name("bypass");

/// The window handle the tests give. A headless window never looks at it.
pub const PARENT: usize = 0x0004_05A2;

thread_local! {
    static OPENED: RefCell<Vec<HeadlessEditor>> = const { RefCell::new(Vec::new()) };
}

/// The plugin in the harness, with its editor open in a headless window.
pub struct Open {
    pub harness: Harness<Duck>,
    pub host: Rc<RecordingEditorHost>,
    pub editor: Option<Box<dyn OpenEditor>>,
    pub link: Option<HostLink>,
    pub ui: HeadlessEditor,
}

impl Open {
    /// Creates the plugin in a harness and opens its editor at `scale` (the
    /// host's scale, or none), as a wrapper does: from the spec the plugin
    /// returns, on a context tied to a host that stores what the editor
    /// performs into the parameters, as a standalone program does. Runs the
    /// first frame.
    pub fn new(scale: Option<f64>) -> Open {
        let mut harness = Harness::<Duck>::new(RATE, BLOCK);
        let host = Rc::new(RecordingEditorHost::new().with_params(harness.plugin_mut().params()));
        let (editor, link, ui) = open_editor(&mut harness, &host, scale);
        assert_eq!(ui.tick(), 1, "the frame timer runs");
        Open {
            harness,
            host,
            editor: Some(editor),
            link: Some(link),
            ui,
        }
    }

    /// The open editor, as the wrapper calls it.
    pub fn editor(&mut self) -> &mut dyn OpenEditor {
        self.editor.as_deref_mut().expect("open")
    }

    /// Closes the editor as a wrapper does: the editor first, then the
    /// link to the host.
    pub fn close(&mut self) {
        self.editor = None;
        self.link = None;
    }

    /// Closes the editor and opens it again on the same plugin and host.
    pub fn reopen(&mut self, scale: Option<f64>) {
        self.close();
        let (editor, link, ui) = open_editor(&mut self.harness, &self.host, scale);
        self.editor = Some(editor);
        self.link = Some(link);
        self.ui = ui;
        self.ui.tick();
    }

    /// The widget with `role` and `name`.
    pub fn get(&self, role: Role, name: &str) -> AccessEntry {
        get(&self.ui, role, name)
    }

    /// The text a widget shows for its value.
    pub fn text(&self, role: Role, name: &str) -> String {
        self.get(role, name)
            .node
            .value
            .unwrap_or_else(|| panic!("{role:?} {name:?} shows no value"))
    }

    /// Processes `frames` frames of a steady input at 1.0 with a steady
    /// sidechain at `key` (no sidechain at all for `None`), and returns
    /// what came out.
    pub fn process(&mut self, key: Option<f32>, frames: usize) -> AudioBuffer<f32> {
        process(&mut self.harness, key, frames)
    }
}

/// Opens the plugin's editor in a headless window.
pub fn open_editor(
    harness: &mut Harness<Duck>,
    host: &Rc<RecordingEditorHost>,
    scale: Option<f64>,
) -> (Box<dyn OpenEditor>, HostLink, HeadlessEditor) {
    let spec = harness
        .plugin_mut()
        .editor_spec()
        .on_headless_open(|ui| OPENED.with(|opened| opened.borrow_mut().push(ui)));
    // What a wrapper is given: the harness asked the plugin once, as one.
    let offered = harness.editor().expect("Mangue Duck has an editor");
    // The spec given the hook is the one the plugin offers.
    for at in [1.0, 1.5, 2.0] {
        assert_eq!(offered.size(at), spec.size(at));
    }
    assert_eq!(offered.resize(), spec.resize());

    let host_dyn: Rc<dyn manguebyte::plugin::editor::EditorHost> = host.clone();
    let (context, link) = EditorContext::attach(host_dyn);
    let parent = ParentWindow::win32(PARENT).expect("not null");
    let editor = spec
        .open(parent, scale, context)
        .expect("a headless editor always opens");
    let ui = OPENED
        .with(|opened| opened.borrow_mut().pop())
        .expect("the hook ran");
    (editor, link, ui)
}

/// The widget with `role` and `name` in `ui`.
pub fn get(ui: &HeadlessEditor, role: Role, name: &str) -> AccessEntry {
    ui.find(role, name).unwrap_or_else(|| {
        panic!(
            "no {role:?} named {name:?} in {:?}",
            ui.access_tree()
                .iter()
                .map(|entry| (entry.node.role, entry.node.name.clone()))
                .collect::<Vec<_>>()
        )
    })
}

/// A stereo buffer holding `level` on every frame.
pub fn steady(level: f32, frames: usize) -> AudioBuffer<f32> {
    let mut buffer = AudioBuffer::new(2, frames);
    for channel in buffer.iter_channels_mut() {
        channel.fill(level);
    }
    buffer
}

/// See [`Open::process`].
pub fn process(harness: &mut Harness<Duck>, key: Option<f32>, frames: usize) -> AudioBuffer<f32> {
    let input = steady(1.0, frames);
    match key {
        Some(level) => harness.process_with_sidechain(&input, &steady(level, frames)),
        None => harness.process(&input),
    }
}
