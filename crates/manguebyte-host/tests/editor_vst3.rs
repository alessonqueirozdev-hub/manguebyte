//! The editors of Mangue Pluck and Mangue Duck, the framework's example
//! plugins, hosted as VST 3 plugins and opened in real windows.
//!
//! On Windows each editor is opened in a top-level window of the test's
//! own, as an application would, from the shared library cargo builds for
//! the plugin. The test runs the message loop, so the editors' frame
//! timers fire and they paint, and checks what the host says against the
//! windows the system has: the size of the editor's window, after a new
//! scale, after it closes and opens again, and after the plugin is dropped
//! with its editor open.
//!
//! Then a person's drag on a knob, delivered to the editor's window as the
//! system delivers it, has to come out of `pump` as one gesture and reach
//! the hosted processor as a change it acts on from the first frame of the
//! next block. For that the plugin is reached through the factory it
//! exports, linked into this executable rather than loaded from its
//! library: its windows are then of the window code compiled into the
//! test, which is what the test's input helpers can talk to.
//!
//! Elsewhere, a Win32 window is no place for either editor, and opening one
//! there has to fail cleanly.
//!
//! Not under Miri, which cannot load a shared library.
#![cfg(all(feature = "vst3", not(miri)))]

mod common;

#[cfg(windows)]
mod on_windows {
    use std::time::{Duration, Instant};

    use manguebyte_core::{AudioBuffer, ParamId};
    use manguebyte_dsp::event::EventBuffer;
    use manguebyte_dsp::{ProcessContext, Processor, Transport};
    use manguebyte_host::editor::{EditorRequest, EditorSize};
    use manguebyte_host::vst3::{HostEvent, Vst3Error, Vst3Module, Vst3Plugin, Vst3Processor};
    use manguebyte_window::win32::testing::{self, Message, TopLevel};
    use manguebyte_window::{Button, PhysicalPoint};

    use super::common;

    /// Runs the message loop for `duration`, with the plugin's `pump` on
    /// every turn, and returns what the plugin reported meanwhile.
    fn run(plugin: &mut Vst3Plugin, duration: Duration) -> Vec<HostEvent> {
        let mut events = Vec::new();
        let end = Instant::now() + duration;
        testing::pump_until(duration, || {
            events.extend(plugin.pump());
            Instant::now() >= end
        });
        events
    }

    /// Runs the message loop until the plugin reports what `wanted` looks
    /// for, or five seconds pass. Returns everything it reported.
    fn run_until(plugin: &mut Vst3Plugin, wanted: impl Fn(&HostEvent) -> bool) -> Vec<HostEvent> {
        let mut events = Vec::new();
        testing::pump_until(Duration::from_secs(5), || {
            events.extend(plugin.pump());
            events.iter().any(&wanted)
        });
        events
    }

    /// The windows inside `parent`, once the loop has had a moment.
    fn editor_windows(parent: &TopLevel) -> Vec<usize> {
        testing::children_of(parent.handle().expect("the window is there"))
    }

    fn scaled(size: EditorSize, scale: u32) -> EditorSize {
        EditorSize::new(size.width * scale / 2, size.height * scale / 2)
    }

    /// Opens, resizes, rescales, paints, closes, opens again and drops the
    /// editor of the plugin in `library`, of fixed size `size` at scale 1.
    fn take_the_editor_through_its_life(library: &str, size: EditorSize) {
        let module = common::load_vst3(common::built_library(library)).expect("the plugin loads");
        let mut plugin = module
            .create(module.classes()[0].id)
            .expect("the plugin is created");
        assert!(plugin.has_editor());
        assert!(!plugin.is_editor_open());
        assert_eq!(plugin.editor_size(), None);
        let parent = TopLevel::new(size.width * 2 + 20, size.height * 2 + 20).expect("a desktop");

        // Opened at scale 1, it is its own size, in one window of its own.
        assert_eq!(plugin.open_editor(parent.parent(), Some(1.0)), Ok(size));
        assert!(plugin.is_editor_open());
        assert_eq!(plugin.editor_size(), Some(size));
        assert_eq!(
            plugin.open_editor(parent.parent(), Some(1.0)),
            Err(Vst3Error::EditorAlreadyOpen)
        );
        let [child] = editor_windows(&parent)[..] else {
            panic!("{library}: expected one editor window");
        };
        assert_eq!(
            testing::client_size_of(child),
            Some((size.width, size.height))
        );
        // It paints, and asks for nothing.
        let events = run(&mut plugin, Duration::from_millis(300));
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, HostEvent::Editor(_))),
            "{library}: {events:?}"
        );
        // The example editors run from their own timers on Windows.
        assert_eq!(plugin.next_editor_timer(), None);
        assert!(plugin.editor_fds().is_empty());

        // Of fixed size: it takes its own size and no other.
        assert!(!plugin.can_resize_editor());
        assert_eq!(plugin.set_editor_size(size), Ok(size));
        assert_eq!(
            plugin.set_editor_size(EditorSize::new(size.width + 10, size.height)),
            Err(Vst3Error::EditorRefused("onSize"))
        );
        assert_eq!(plugin.editor_size(), Some(size));

        // At a new scale it asks for its size at that scale, which the
        // application grants.
        let larger = scaled(size, 3);
        plugin
            .set_editor_scale(1.5)
            .expect("the editor takes the scale");
        let events = run_until(&mut plugin, |event| {
            matches!(event, HostEvent::Editor(EditorRequest::Resize(_)))
        });
        assert!(
            events.contains(&HostEvent::Editor(EditorRequest::Resize(larger))),
            "{library}: {events:?}"
        );
        assert_eq!(plugin.set_editor_size(larger), Ok(larger));
        run(&mut plugin, Duration::from_millis(200));
        assert_eq!(
            testing::client_size_of(child),
            Some((larger.width, larger.height))
        );
        assert_eq!(plugin.editor_size(), Some(larger));
        for scale in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            assert!(
                matches!(
                    plugin.set_editor_scale(scale),
                    Err(Vst3Error::InvalidArgument(_))
                ),
                "{library}: {scale}"
            );
        }

        // Closed: its window goes, and the parent is left as it was.
        plugin.close_editor();
        run(&mut plugin, Duration::from_millis(100));
        assert!(!plugin.is_editor_open());
        assert!(!testing::is_window(child), "{library}");
        assert_eq!(editor_windows(&parent), Vec::<usize>::new(), "{library}");
        plugin.close_editor();

        // Opened again, at another scale, from scratch.
        let largest = scaled(size, 4);
        assert_eq!(plugin.open_editor(parent.parent(), Some(2.0)), Ok(largest));
        let [child] = editor_windows(&parent)[..] else {
            panic!("{library}: expected one editor window again");
        };
        assert_eq!(
            testing::client_size_of(child),
            Some((largest.width, largest.height))
        );
        run(&mut plugin, Duration::from_millis(200));

        // Dropped with the editor open: the editor goes with it.
        drop(plugin);
        testing::pump(Duration::from_millis(100));
        assert!(!testing::is_window(child), "{library}");
        assert!(parent.is_open());
        drop(module);
    }

    #[test]
    fn mangue_ducks_editor_opens_resizes_and_closes_in_a_real_window() {
        take_the_editor_through_its_life("mangue_duck", EditorSize::new(440, 210));
    }

    #[test]
    fn mangue_plucks_editor_opens_resizes_and_closes_in_a_real_window() {
        take_the_editor_through_its_life("mangue_pluck", EditorSize::new(520, 160));
    }

    const RATE: f64 = 48_000.0;
    const BLOCK: usize = 4_800;

    /// One block of a constant 1.0 in, with the sidechain at full scale,
    /// which the duck answers by ducking by its depth. Returns the first
    /// and the last sample out of the first channel.
    fn block(processor: &mut Vst3Processor) -> (f32, f32) {
        let events = EventBuffer::with_capacity(4);
        let transport = Transport::playing_at(120.0);
        let mut side = AudioBuffer::<f32>::new(2, BLOCK);
        let mut audio = AudioBuffer::<f32>::new(2, BLOCK);
        for channel in side.iter_channels_mut().chain(audio.iter_channels_mut()) {
            channel.fill(1.0);
        }
        let context = ProcessContext::new(&events, &transport).with_sidechain(&side);
        processor.process_with(&mut audio, &context);
        let out = audio.channel(0);
        (out[0], out[BLOCK - 1])
    }

    fn gain_of_db(db: f64) -> f32 {
        10f64.powf(db / 20.0) as f32
    }

    #[test]
    fn a_drag_on_a_knob_in_the_editor_reaches_the_hosted_processor() {
        // SAFETY: the factory Mangue Duck exports, made by the framework's
        // VST 3 wrapper, which keeps to the VST 3 interfaces. Its one
        // reference is handed over.
        let module = unsafe {
            Vst3Module::from_factory(mangue_duck::GetPluginFactory().cast(), "Mangue Duck")
        }
        .expect("the factory is read");
        let mut plugin = module
            .create(module.classes()[0].id)
            .expect("the plugin is created");
        let depth = common::vst3_id(ParamId::from_name("depth"));
        let mut processor = plugin.activate(RATE, BLOCK).expect("it activates");
        plugin.pump();

        // Ducked by the default depth, 12 dB, once the follower settles.
        let mut settled = (0.0, 0.0);
        for _ in 0..5 {
            settled = block(&mut processor);
        }
        assert!((settled.1 - gain_of_db(-12.0)).abs() < 1e-4, "{settled:?}");

        let parent = TopLevel::new(440, 210).expect("a desktop");
        assert_eq!(
            plugin.open_editor(parent.parent(), Some(1.0)),
            Ok(EditorSize::new(440, 210))
        );
        run(&mut plugin, Duration::from_millis(300));
        let [child] = editor_windows(&parent)[..] else {
            panic!("expected one editor window");
        };

        // The depth knob is the second of four that share the width the
        // meter leaves, below the title: x = 140 is inside it however wide
        // the meter is, and the knob is found going down from below the
        // title. A drag 40 pixels up is a fifth of the drag range. The
        // controller shows the edit at once; nothing else hears of it
        // before `pump`.
        let default = plugin.param_value(depth).expect("the depth has a value");
        let mut reached = false;
        for y in (60..=190).step_by(10) {
            let at = |dy: i32| PhysicalPoint::new(140, y - dy);
            testing::send(
                child,
                Message::ButtonDown {
                    button: Button::Primary,
                    at: at(0),
                    shift: false,
                    control: false,
                },
            )
            .expect("the editor's window takes input");
            for step in 1..=4 {
                testing::send(
                    child,
                    Message::MouseMove {
                        at: at(10 * step),
                        shift: false,
                        control: false,
                    },
                );
            }
            testing::send(
                child,
                Message::ButtonUp {
                    button: Button::Primary,
                    at: at(40),
                },
            );
            if plugin.param_value(depth) != Some(default) {
                reached = true;
                break;
            }
        }
        assert!(reached, "the drag reached no knob");

        // Until the host passes the edit on, the processor goes on as it
        // was: the edit reaches it through the host, as VST 3 has it.
        let before = block(&mut processor);
        assert!((before.0 - gain_of_db(-12.0)).abs() < 1e-4, "{before:?}");
        let edits = plugin.pump();

        // One gesture on the depth, with the values in between.
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            panic!("the drag reached no knob");
        };
        assert_eq!(
            *first,
            HostEvent::GestureBegin { param: depth },
            "{edits:?}"
        );
        assert_eq!(*last, HostEvent::GestureEnd { param: depth }, "{edits:?}");
        let values: Vec<f64> = edits[1..edits.len() - 1]
            .iter()
            .map(|event| match *event {
                HostEvent::ParamValue { param, value } if param == depth => value,
                ref other => panic!("{other:?} in the middle of the gesture: {edits:?}"),
            })
            .collect();
        let value = *values.last().expect("the drag changed the depth");
        assert!(value > 0.4, "{values:?}");
        assert_eq!(plugin.param_value(depth), Some(value));
        let db = plugin
            .normalized_to_plain(depth, value)
            .expect("the plugin says what the value is");
        assert!(db > 12.0, "{db}");

        // The processor hears it from the first frame of the next block:
        // the follower has settled, so the gain steps to the new depth at
        // once.
        let after = block(&mut processor);
        let expected = gain_of_db(-db);
        assert!((after.0 - expected).abs() < 1e-4, "{after:?} for {db} dB");
        assert!((after.1 - expected).abs() < 1e-4, "{after:?} for {db} dB");

        plugin.close_editor();
        assert!(!testing::is_window(child));
        drop(processor);
        plugin.deactivate().expect("it deactivates");
        assert_eq!(plugin.lost_events(), 0);
    }
}

#[cfg(not(windows))]
#[test]
fn a_win32_window_is_no_place_for_the_example_editors() {
    use manguebyte_host::editor::{ParentWindow, WindowApi};
    use manguebyte_host::vst3::Vst3Error;

    for library in ["mangue_duck", "mangue_pluck"] {
        let module = common::load_vst3(common::built_library(library)).expect("the plugin loads");
        let mut plugin = module
            .create(module.classes()[0].id)
            .expect("the plugin is created");
        assert!(plugin.has_editor());
        let parent = ParentWindow::win32(0x1234).expect("a handle");
        // Refused, and nothing is left half open: the same again.
        for _ in 0..2 {
            assert_eq!(
                plugin.open_editor(parent, Some(1.0)),
                Err(Vst3Error::PlatformUnsupported(WindowApi::Win32)),
                "{library}"
            );
            assert!(!plugin.is_editor_open());
            assert_eq!(plugin.editor_size(), None);
            assert_eq!(plugin.next_editor_timer(), None);
            assert!(plugin.editor_fds().is_empty());
        }
    }
}
