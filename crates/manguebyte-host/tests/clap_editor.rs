//! Opens the editors of Mangue Pluck and Mangue Duck, hosted through the
//! shared libraries cargo builds for them, as an application does.
//!
//! On Windows each editor goes inside a real top-level window of the
//! test's, with the message loop running, so the editor's frame timer fires
//! and it paints. The test checks its size, resizes it (both editors are of
//! fixed size, so it keeps its own), scales it, closes and opens it again,
//! and drops the plugin with its editor open. Then it drags one of the
//! editor's knobs with mouse messages, as the system delivers them, and
//! checks that the edit reaches the host the way an edit made by a person
//! does: as a gesture with values from `pump`, which come through the
//! plugin's processor for Mangue Pluck, active and processing, and through
//! a flush on the main thread for Mangue Duck, not active.
//!
//! The editors run their frames on timers of their own windows, so the
//! timers the host keeps for a plugin are not what they use on Windows;
//! `clap_editor_calls.rs` checks those with a plugin written for the
//! purpose.
//!
//! Elsewhere neither editor can go inside a window the system has, and the
//! test checks that a Win32 window is refused cleanly.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "clap", not(miri)))]

mod common;

use manguebyte_host::clap::{ClapError, ClapLibrary};
use manguebyte_host::editor::{ParentWindow, WindowApi};

/// The two plugins: the crate that builds each, its id, and its editor's
/// size at scale 1, written out (naming the plugin crates' constants would
/// link both into this executable, and their exported entry points would
/// clash).
const PLUGINS: [(&str, &str, (u32, u32)); 2] = [
    ("mangue_pluck", "org.manguebyte.pluck", (520, 160)),
    ("mangue_duck", "org.manguebyte.duck", (440, 210)),
];

fn library(crate_name: &str) -> ClapLibrary {
    common::load(common::built_library(crate_name)).expect("the example plugin loads")
}

#[test]
fn an_editor_is_offered_where_it_can_be_shown() {
    for (crate_name, id, _) in PLUGINS {
        let mut plugin = library(crate_name).create(id).unwrap();
        // The framework's editors live in Win32 windows, and in no other
        // kind yet.
        assert_eq!(plugin.has_editor(), cfg!(windows), "{id}");
        for parent in [
            ParentWindow::cocoa(0x7000).unwrap(),
            ParentWindow::x11(0x0040_0001).unwrap(),
        ] {
            assert_eq!(
                plugin.open_editor(parent, None),
                Err(ClapError::EditorApiUnsupported(parent.api())),
                "{id}"
            );
        }
        if !cfg!(windows) {
            let win32 = ParentWindow::win32(0x1234).unwrap();
            assert_eq!(
                plugin.open_editor(win32, Some(1.0)),
                Err(ClapError::EditorApiUnsupported(WindowApi::Win32)),
                "{id}"
            );
        }
        assert!(!plugin.is_editor_open(), "{id}");
        assert_eq!(plugin.editor_size(), None, "{id}");
        assert!(plugin.editor_fds().is_empty(), "{id}");
    }
}

#[cfg(windows)]
mod windows {
    //! Every `unsafe` block here sends a mouse message, which carries only
    //! numbers, to a window this thread made.
    #![allow(clippy::undocumented_unsafe_blocks)]

    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::time::Duration;

    use manguebyte_core::{AudioBuffer, ParamId};
    use manguebyte_dsp::{ProcessContext, Processor};
    use manguebyte_host::clap::{ClapError, ClapPlugin, ClapProcessor, HostEvent, OutputKind};
    use manguebyte_host::editor::{EditorRequest, EditorSize};
    use manguebyte_window::win32::testing::{self, TopLevel};

    use super::{PLUGINS, library};

    /// Keeps the tests of this module apart: they drive the mouse of
    /// windows on their threads, and a capture taken by one should not
    /// meet the other.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SendMessageW(hwnd: *mut c_void, message: u32, wparam: usize, lparam: isize) -> isize;
    }

    const WM_MOUSEMOVE: u32 = 0x0200;
    const WM_LBUTTONDOWN: u32 = 0x0201;
    const WM_LBUTTONUP: u32 = 0x0202;
    const MK_LBUTTON: usize = 0x0001;

    /// Sends a mouse message at `(x, y)` of the window `handle`'s client
    /// area. `testing::send` does the same for windows of this
    /// executable's own window code; an editor's window belongs to the
    /// plugin's copy of it.
    fn mouse(handle: usize, message: u32, buttons: usize, (x, y): (i32, i32)) {
        let at = (((y as u32 & 0xFFFF) << 16) | (x as u32 & 0xFFFF)) as isize;
        unsafe {
            SendMessageW(
                std::ptr::with_exposed_provenance_mut(handle),
                message,
                buttons,
                at,
            );
        }
    }

    /// Runs the message loop and the plugin's `pump` for `duration`, and
    /// returns what `pump` said.
    fn run(plugin: &mut ClapPlugin, duration: Duration) -> Vec<HostEvent> {
        let mut events = Vec::new();
        testing::pump_until(duration, || {
            events.extend(plugin.pump());
            false
        });
        events
    }

    fn editor_requests(events: &[HostEvent]) -> Vec<EditorRequest> {
        events
            .iter()
            .filter_map(|event| match event {
                HostEvent::Editor(request) => Some(*request),
                _ => None,
            })
            .collect()
    }

    /// The editor's window: the one window inside `parent`.
    fn editor_window(parent: &TopLevel) -> usize {
        let children = testing::children_of(parent.handle().expect("open"));
        assert_eq!(children.len(), 1, "one window inside the parent");
        children[0]
    }

    /// Processes a block, if there is an audio half, and returns whether
    /// the plugin reported the beginning of a gesture in it.
    fn process(processor: Option<&mut ClapProcessor>) -> bool {
        let Some(processor) = processor else {
            return false;
        };
        let mut block = AudioBuffer::<f32>::new(2, 256);
        processor.process_with(&mut block, &ProcessContext::empty());
        processor
            .output_events()
            .iter()
            .any(|event| matches!(event.kind, OutputKind::GestureBegin { .. }))
    }

    /// The gesture in `events`: the parameter, and the last value sent
    /// with it. Panics if the events hold anything but one whole gesture.
    fn gesture(events: &[HostEvent]) -> Option<(ParamId, f64)> {
        let edits: Vec<&HostEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    HostEvent::GestureBegin { .. }
                        | HostEvent::ParamValue { .. }
                        | HostEvent::GestureEnd { .. }
                )
            })
            .collect();
        let (first, last) = (edits.first()?, edits.last()?);
        let HostEvent::GestureBegin { param } = **first else {
            panic!("a gesture begins first: {edits:?}");
        };
        assert_eq!(**last, HostEvent::GestureEnd { param }, "{edits:?}");
        let values: Vec<f64> = edits
            .iter()
            .filter_map(|event| match **event {
                HostEvent::ParamValue { param: of, value } => {
                    assert_eq!(of, param, "{edits:?}");
                    Some(value)
                }
                _ => None,
            })
            .collect();
        assert!(!values.is_empty(), "a value inside the gesture: {edits:?}");
        Some((param, *values.last().unwrap()))
    }

    /// Drags a knob of the editor in `child` up by 30 pixels, trying the
    /// places in `spots` until one is a knob, and returns the gesture that
    /// reached the host, with the value the parameter had before. With an
    /// audio half, the gesture has to have come through it.
    fn drag_a_knob(
        plugin: &mut ClapPlugin,
        mut processor: Option<&mut ClapProcessor>,
        child: usize,
        spots: &[(i32, i32)],
    ) -> (ParamId, f64, f64) {
        let active = processor.is_some();
        let before: Vec<(ParamId, Option<f64>)> = plugin
            .params()
            .iter()
            .map(|param| (param.id, plugin.param_value(param.id)))
            .collect();
        for &(x, y) in spots {
            mouse(child, WM_MOUSEMOVE, 0, (x, y));
            mouse(child, WM_LBUTTONDOWN, MK_LBUTTON, (x, y));
            for step in 1..=6 {
                mouse(child, WM_MOUSEMOVE, MK_LBUTTON, (x, y - 5 * step));
            }
            mouse(child, WM_LBUTTONUP, 0, (x, y - 30));
            // The edit reaches the plugin's processor with the next block,
            // and the processor reports it.
            let mut processed = process(processor.as_deref_mut());
            let mut events = run(plugin, Duration::from_millis(60));
            processed |= process(processor.as_deref_mut());
            events.extend(run(plugin, Duration::from_millis(20)));
            if let Some((param, value)) = gesture(&events) {
                assert_eq!(processed, active, "the processor reported the gesture");
                if !active {
                    // With no audio half, the plugin asked for a flush,
                    // which `pump` made there and then.
                    assert!(!events.contains(&HostEvent::FlushRequested));
                }
                let was = before
                    .iter()
                    .find(|(id, _)| *id == param)
                    .and_then(|(_, value)| *value)
                    .expect("a parameter the plugin lists");
                return (param, was, value);
            }
        }
        panic!("no knob at any of {spots:?}");
    }

    /// The scenario for one plugin: `active` hosts it with an audio half
    /// that processes, and `spots` are places in its editor at scale 1
    /// where a knob may be.
    fn editor_scenario(index: usize, active: bool, spots: &[(i32, i32)]) {
        let _alone = ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (crate_name, id, (width, height)) = PLUGINS[index];
        let base = EditorSize::new(width, height);
        let double = EditorSize::new(width * 2, height * 2);
        let library = library(crate_name);
        let mut plugin = library.create(id).unwrap();
        assert!(plugin.has_editor(), "{id}");
        let mut processor = active.then(|| plugin.activate(48_000.0, 256).unwrap());
        // Started, so that edits are sent while processing.
        assert!(!process(processor.as_mut()));

        // --- Opened at scale 1 inside a window of the test's.
        let parent = TopLevel::new(width * 2 + 20, height * 2 + 20).expect("a desktop");
        assert_eq!(plugin.open_editor(parent.parent(), Some(1.0)), Ok(base));
        assert!(plugin.is_editor_open());
        assert_eq!(plugin.editor_size(), Some(base));
        let child = editor_window(&parent);
        assert_eq!(testing::parent_of_any(child), parent.handle());
        assert_eq!(testing::client_size_of(child), Some((width, height)));
        // It runs its frames; nothing is asked of the window meanwhile.
        let events = run(&mut plugin, Duration::from_millis(250));
        assert!(editor_requests(&events).is_empty(), "{id}: {events:?}");

        // --- An edit in the editor reaches the host.
        let (param, was, value) = drag_a_knob(&mut plugin, processor.as_mut(), child, spots);
        let info = plugin.param(param).unwrap().clone();
        assert!(
            value > was && value <= info.max,
            "{id}: {} went up from {was} to {value}",
            info.name
        );
        // The host's view of the value is the one the gesture sent.
        assert_eq!(
            plugin.param_value(param),
            Some(value),
            "{id}: {}",
            info.name
        );
        eprintln!("{id}: dragged {} from {was} to {value}", info.name);

        // --- A fixed editor keeps its size when its window is resized.
        assert!(!plugin.can_resize_editor(), "{id}");
        assert_eq!(
            plugin.set_editor_size(EditorSize::new(width + 100, height + 50)),
            Ok(base),
            "{id}"
        );
        assert_eq!(testing::client_size_of(child), Some((width, height)));

        // --- At scale 2 it asks for twice its size, and takes it.
        assert_eq!(plugin.set_editor_scale(2.0), Ok(()), "{id}");
        assert_eq!(plugin.editor_size(), Some(double), "{id}");
        let events = run(&mut plugin, Duration::from_millis(50));
        assert_eq!(
            editor_requests(&events),
            [EditorRequest::Resize(double)],
            "{id}"
        );
        // The application makes its window that size and tells the plugin.
        assert_eq!(plugin.set_editor_size(double), Ok(double), "{id}");
        let grown = testing::pump_until(Duration::from_secs(2), || {
            testing::client_size_of(child) == Some((double.width, double.height))
        });
        assert!(grown, "{id}: {:?}", testing::client_size_of(child));
        assert!(matches!(
            plugin.set_editor_scale(f64::NAN),
            Err(ClapError::InvalidArgument(_))
        ));
        run(&mut plugin, Duration::from_millis(100));

        // --- Closed: its window is gone, and it opens again.
        plugin.close_editor();
        assert!(!plugin.is_editor_open());
        assert!(
            !testing::is_window(child),
            "{id}: the editor's window is gone"
        );
        assert!(testing::children_of(parent.handle().unwrap()).is_empty());
        plugin.close_editor();
        assert_eq!(
            plugin.open_editor(parent.parent(), Some(1.25)),
            Ok(base.scaled(1.25))
        );
        let again = editor_window(&parent);
        assert_eq!(
            testing::client_size_of(again),
            Some((base.scaled(1.25).width, base.scaled(1.25).height))
        );
        assert_eq!(
            plugin.open_editor(parent.parent(), None),
            Err(ClapError::EditorAlreadyOpen)
        );
        run(&mut plugin, Duration::from_millis(100));

        // --- Dropped with its editor open, and for Mangue Pluck with its
        // audio half still there: the editor goes with the main half, on
        // this thread, and the plugin with the audio half.
        drop(plugin);
        assert!(
            !testing::is_window(again),
            "{id}: dropping closed the editor"
        );
        process(processor.as_mut());
        drop(processor);
        testing::pump(Duration::from_millis(20));
        drop(parent);
    }

    /// Sizes at another scale, rounded as the editors round them.
    trait Scaled {
        fn scaled(self, scale: f64) -> EditorSize;
    }

    impl Scaled for EditorSize {
        fn scaled(self, scale: f64) -> EditorSize {
            let at = |length: u32| (f64::from(length) * scale).round() as u32;
            EditorSize::new(at(self.width), at(self.height))
        }
    }

    #[test]
    fn mangue_pluck_opens_its_editor_and_edits_reach_its_processor() {
        // Six knobs in a row under the title, each 76 pixels wide: the
        // second one's middle, and around it.
        editor_scenario(0, true, &[(134, 96), (134, 80), (134, 112), (134, 70)]);
    }

    #[test]
    fn mangue_duck_opens_its_editor_and_edits_reach_it_by_a_flush() {
        // Four knobs and a meter under the title and the bypass switch:
        // the first knob, around its middle.
        editor_scenario(
            1,
            false,
            &[(55, 125), (55, 110), (55, 140), (45, 95), (45, 155)],
        );
    }
}
