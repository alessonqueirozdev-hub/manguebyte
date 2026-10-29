//! The editor through the editor contract alone, with a recording host in
//! place of a wrapper: keys a host delivers, the system's scale, a handler
//! that fails, and what closing leaves behind.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use manguebyte_gfx::{Color, Rect, Size};
use manguebyte_gui::{Editor, FRAME_PERIOD_MS, HeadlessEditor};
use manguebyte_params::{FloatParam, FloatRange, Param, param_set};
use manguebyte_plugin::editor::testing::{HostCall, RecordingEditorHost};
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorSpec, HostLink, HostSize, Key, KeyEvent, Modifiers,
    OpenEditor, ParentWindow, Resize, WindowApi,
};
use manguebyte_ui::{IntoView, MAX_BUFFER_SIDE, ParamHandle, Role, canvas, knob, label, row};
use manguebyte_window::{PhysicalSize, TimerId};

struct Params {
    gain: FloatParam,
}
param_set!(Params { gain });

thread_local! {
    static OPENED: RefCell<Option<HeadlessEditor>> = const { RefCell::new(None) };
}

fn opened() -> HeadlessEditor {
    OPENED
        .with(|slot| slot.borrow_mut().take())
        .expect("the hook ran")
}

fn params() -> Arc<Params> {
    Arc::new(Params {
        gain: FloatParam::new("gain", "Gain", FloatRange::linear(0.0, 1.0), 0.5).with_precision(2),
    })
}

/// A knob, and a canvas that panics while it draws once `fail` is set and
/// `redraw` changes.
fn spec(params: &Arc<Params>, fail: &Arc<AtomicBool>, redraw: &Arc<AtomicU64>) -> Editor {
    let (params, fail, redraw) = (Arc::clone(params), Arc::clone(fail), Arc::clone(redraw));
    Editor::new(HostSize::new(200, 100), move || {
        let fail = Arc::clone(&fail);
        let redraw = Arc::clone(&redraw);
        row((
            knob(ParamHandle::new(Arc::clone(&params), |p| &p.gain)),
            canvas("Picture", Size::new(40.0, 40.0), move |_, scene| {
                assert!(!fail.load(Ordering::Relaxed), "the picture cannot be drawn");
                scene.fill(Rect::new(0.0, 0.0, 10.0, 10.0), Color::rgb(200, 0, 0));
            })
            .redraw_on(move || redraw.load(Ordering::Relaxed)),
        ))
        .padding(8.0)
    })
    .with_resize(Resize::Free {
        min: HostSize::new(100, 50),
        max: HostSize::new(400, 200),
    })
    .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)))
}

struct Open {
    host: Rc<RecordingEditorHost>,
    editor: Box<dyn OpenEditor>,
    link: HostLink,
    ui: HeadlessEditor,
}

fn open(spec: &Editor, params: &Arc<Params>, scale: Option<f64>) -> Open {
    let host = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let (context, link) = EditorContext::attach(host.clone());
    let parent = ParentWindow::win32(0x1F00).expect("not null");
    let mut editor = spec
        .open(parent, scale, context)
        .expect("headless windows open");
    let ui = opened();
    // It opens hidden, with no frame timer, and starts when the host shows
    // it, as hosts do.
    assert!(!ui.window().is_visible());
    assert_eq!(ui.window().timers(), []);
    editor.set_visible(true);
    Open {
        host,
        editor,
        link,
        ui,
    }
}

fn key(key: Key) -> KeyEvent {
    KeyEvent {
        key,
        modifiers: Modifiers::NONE,
        pressed: true,
    }
}

#[test]
fn the_spec_gives_its_size_and_rules_and_opens_in_any_parent_headless() {
    let params = params();
    let spec = spec(&params, &Arc::default(), &Arc::default());
    assert_eq!(spec.size(1.0), HostSize::new(200, 100));
    assert_eq!(spec.size(1.25), HostSize::new(250, 125));
    assert!(spec.resize().can_resize());
    for api in [WindowApi::Win32, WindowApi::Cocoa, WindowApi::X11] {
        assert!(spec.supports(api), "{api:?}");
    }
    for parent in [
        ParentWindow::cocoa(0x40).expect("not null"),
        ParentWindow::x11(7).expect("not null"),
    ] {
        let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
        let editor = spec.open(parent, None, context).map(|_| ());
        assert_eq!(editor, Ok::<(), EditorError>(()));
        opened();
        drop(link);
    }
}

#[test]
fn keys_the_host_delivers_reach_the_interface() {
    let params = params();
    let mut open = open(
        &spec(&params, &Arc::default(), &Arc::default()),
        &params,
        Some(1.0),
    );
    open.ui.tick();
    // Nothing has the focus: a key is left to the host.
    assert!(!open.editor.key(key(Key::Up)));
    // Tab gives the knob the focus, and an arrow is one edit.
    assert!(open.editor.key(key(Key::Tab)));
    assert!(open.editor.key(key(Key::Up)));
    let id = params.gain.param_id();
    let calls = open.host.take_calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert_eq!(calls[0], HostCall::BeginGesture(id));
    assert!(
        matches!(calls[1], HostCall::Perform(param, value) if param == id && (value - 0.51).abs() < 1e-6)
    );
    assert_eq!(calls[2], HostCall::EndGesture(id));
    // A key no widget uses goes back.
    assert!(!open.editor.key(key(Key::Character('q'))));
    open.ui.tick();
    assert_eq!(
        open.ui
            .find(Role::Slider, "Gain")
            .and_then(|knob| knob.node.value),
        Some("0.51".to_string())
    );
    drop(open.editor);
    drop(open.link);
}

#[test]
fn without_a_host_scale_the_editor_follows_the_systems() {
    let params = params();
    let spec = spec(&params, &Arc::default(), &Arc::default());
    let open = open(&spec, &params, None);
    open.ui.tick();
    assert_eq!(open.editor.size(), HostSize::new(200, 100));
    open.host.take_calls();

    // The window moves to a screen at 150 %: the editor asks for its size
    // there, keeping its logical size, and lays itself out at 1.5.
    assert!(open.ui.window().set_system_scale(1.5));
    assert_eq!(
        open.host.take_calls(),
        [HostCall::RequestResize(HostSize::new(300, 150))]
    );
    assert_eq!(open.editor.size(), HostSize::new(300, 150));
    open.ui.tick();
    assert_eq!(open.ui.layout(), Some((Size::new(200.0, 100.0), 1.5)));

    // A host that refuses: the window keeps its size.
    open.host.set_resize_answer(|_| None);
    assert!(open.ui.window().set_system_scale(2.0));
    assert_eq!(
        open.host.take_calls(),
        [HostCall::RequestResize(HostSize::new(400, 200))]
    );
    assert_eq!(open.editor.size(), HostSize::new(300, 150));
    open.ui.tick();
    assert_eq!(open.ui.layout(), Some((Size::new(150.0, 75.0), 2.0)));
    drop(open.editor);
    drop(open.link);

    // Given the host's scale, the system's no longer matters.
    let open = self::open(&spec, &params, Some(1.25));
    open.ui.tick();
    open.host.take_calls();
    assert!(open.ui.window().set_system_scale(2.0));
    assert_eq!(open.host.take_calls(), []);
    assert_eq!(open.editor.size(), HostSize::new(250, 125));
    open.ui.tick();
    assert_eq!(open.ui.layout(), Some((Size::new(200.0, 100.0), 1.25)));
    drop(open.editor);
    drop(open.link);
}

#[test]
fn sizes_the_host_sets_follow_the_rules_at_the_scale_in_use() {
    let params = params();
    let mut open = open(
        &spec(&params, &Arc::default(), &Arc::default()),
        &params,
        Some(2.0),
    );
    let editor = &mut open.editor;
    assert_eq!(editor.size(), HostSize::new(400, 200));
    assert_eq!(editor.fit(HostSize::new(1, 1)), HostSize::new(200, 100));
    assert_eq!(
        editor.fit(HostSize::new(9999, 9999)),
        HostSize::new(800, 400)
    );
    assert!(editor.set_size(HostSize::new(500, 300)));
    assert_eq!(editor.size(), HostSize::new(500, 300));
    assert!(!editor.set_size(HostSize::new(801, 300)));
    assert!(!editor.set_size(HostSize::new(0, 0)));
    assert_eq!(editor.size(), HostSize::new(500, 300));
    // The same size again changes nothing and is accepted.
    assert!(editor.set_size(HostSize::new(500, 300)));
    open.ui.tick();
    assert_eq!(open.ui.layout(), Some((Size::new(250.0, 150.0), 2.0)));

    // A scale from the host keeps the logical size: 250 by 150 at 1 is
    // asked for, and granted.
    open.host.take_calls();
    assert!(open.editor.set_scale(1.0));
    assert_eq!(
        open.host.take_calls(),
        [HostCall::RequestResize(HostSize::new(250, 150))]
    );
    assert_eq!(open.editor.size(), HostSize::new(250, 150));
    for hostile in [f64::NAN, 0.0, -2.0, 100.0] {
        assert!(!open.editor.set_scale(hostile));
    }
    drop(open.editor);
    drop(open.link);
}

/// A knob that the person holds, and a canvas that panics while drawing.
/// A CLAP host may make no editor call for as long as the editor is on
/// screen, so the glue cannot wait for one: the interface is dropped from
/// inside the failing frame, which ends the gesture, and the host is told
/// there and then.
#[test]
fn a_failing_interface_ends_its_gesture_and_tells_the_host_at_once() {
    let params = params();
    let fail = Arc::new(AtomicBool::new(false));
    let redraw = Arc::new(AtomicU64::new(0));
    let mut open = open(&spec(&params, &fail, &redraw), &params, Some(1.0));
    open.ui.tick();
    // Taken hold of: a gesture is open.
    let knob = open.ui.find(Role::Slider, "Gain").expect("a knob");
    open.ui.press(knob.bounds.center());
    open.host.take_calls();

    // The picture panics while it draws.
    fail.store(true, Ordering::Relaxed);
    redraw.store(1, Ordering::Relaxed);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let fired = open.ui.tick();
    std::panic::set_hook(hook);
    assert_eq!(fired, 1);
    let id = params.gain.param_id();
    assert_eq!(
        open.host.take_calls(),
        [HostCall::EndGesture(id), HostCall::Closed],
        "the host still holds an open gesture from a dead editor"
    );
    assert!(!open.ui.is_open());
    // Its frames stopped: a second of ticks runs nothing.
    for _ in 0..60 {
        assert_eq!(open.ui.tick(), 0, "a failed editor runs nothing");
    }

    // What the host calls next does nothing, and the host is told nothing
    // more; showing the editor does not start its frames again.
    open.editor.set_visible(true);
    assert_eq!(open.ui.tick(), 0);
    open.editor.set_visible(false);
    assert!(!open.editor.key(key(Key::Up)));
    assert!(open.editor.set_scale(2.0));
    assert_eq!(open.ui.tick(), 0);
    assert_eq!(open.host.take_calls(), []);
    drop(open.editor);
    drop(open.link);
    assert_eq!(open.host.well_formed(), Ok(()));
}

/// `manguebyte-ui` draws at most `MAX_BUFFER_SIDE` device pixels a side,
/// and a window larger than that would show pixels nobody drew. Rules that
/// allow more at a high scale are capped there.
#[test]
fn no_size_is_larger_than_the_interface_can_draw() {
    let spec = Editor::new(HostSize::new(100, 40), || {
        canvas("Empty", Size::new(1.0, 1.0), |_, _| {})
    })
    .with_resize(Resize::Free {
        min: HostSize::new(10, 10),
        max: HostSize::new(1100, 40),
    })
    .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
    let host = Rc::new(RecordingEditorHost::new());
    let (context, link) = EditorContext::attach(host);
    let parent = ParentWindow::win32(0x1F00).expect("not null");
    let mut editor = spec.open(parent, Some(8.0), context).expect("headless");
    let ui = opened();
    editor.set_visible(true);
    ui.tick();
    assert_eq!(editor.size(), HostSize::new(800, 320));

    // 1100 logical pixels at 8 would be 8800 device pixels.
    let wide = HostSize::new(8800, 320);
    let widest = HostSize::new(MAX_BUFFER_SIDE, 320);
    assert_eq!(editor.fit(wide), widest);
    assert_eq!(
        editor.fit(HostSize::new(u32::MAX, u32::MAX)),
        widest,
        "the height is capped by the rules"
    );
    assert!(!editor.set_size(wide));
    assert_eq!(editor.size(), HostSize::new(800, 320));
    assert!(editor.set_size(widest));
    ui.tick();
    // What the window shows is the back buffer, to its last pixel.
    let shown = ui.shown().expect("open");
    assert_eq!((shown.width(), shown.height()), (MAX_BUFFER_SIDE, 320));
    let background = shown.pixel(8, 300).expect("inside").to_array();
    let far_right = shown.pixel(MAX_BUFFER_SIDE - 1, 300).expect("inside");
    assert_eq!(far_right.to_array(), background);
    assert!(Some(&shown) == ui.pixels().as_ref());
    drop(editor);
    drop(link);

    // A fixed editor that large opens, and says it opens, at the cap.
    let fixed = Editor::new(HostSize::new(1100, 2000), || {
        canvas("Empty", Size::new(1.0, 1.0), |_, _| {})
    })
    .on_headless_open(|editor| OPENED.with(|slot| *slot.borrow_mut() = Some(editor)));
    assert_eq!(
        fixed.size(8.0),
        HostSize::new(MAX_BUFFER_SIDE, MAX_BUFFER_SIDE)
    );
    assert_eq!(fixed.size(1.0), HostSize::new(1100, 2000));
    let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    let editor = fixed.open(parent, Some(8.0), context).expect("headless");
    let ui = opened();
    assert_eq!(
        editor.size(),
        HostSize::new(MAX_BUFFER_SIDE, MAX_BUFFER_SIDE)
    );
    assert_eq!(
        ui.size(),
        Some(PhysicalSize::new(MAX_BUFFER_SIDE, MAX_BUFFER_SIDE))
    );
    drop(editor);
    drop(link);
}

/// The `headless` feature only adds API: an editor that was not given a
/// hook opens in the system's window even in a build that has the feature
/// on, as every test build of this crate does.
#[test]
fn an_editor_without_a_headless_hook_opens_in_the_systems_window() {
    let spec = Editor::new(HostSize::new(100, 50), || label("Plain"));
    assert_eq!(spec.supports(WindowApi::Win32), cfg!(windows));
    assert!(!spec.supports(WindowApi::Cocoa));
    assert!(!spec.supports(WindowApi::X11));
    for parent in [
        ParentWindow::cocoa(0x40).expect("not null"),
        ParentWindow::x11(7).expect("not null"),
    ] {
        let host = Rc::new(RecordingEditorHost::new());
        let (context, link) = EditorContext::attach(host.clone());
        let refused = spec.open(parent, None, context).map(|_| ());
        assert_eq!(refused, Err(EditorError::Unsupported(parent.api())));
        assert_eq!(host.take_calls(), []);
        drop(link);
    }
    OPENED.with(|slot| assert!(slot.borrow().is_none(), "no headless editor"));
}

#[test]
fn an_idle_editor_presents_nothing_and_a_hover_presents_only_while_it_moves() {
    let params = params();
    let open = open(
        &spec(&params, &Arc::default(), &Arc::default()),
        &params,
        Some(2.0),
    );
    open.ui.tick();
    // Many timer ticks with nothing happening: frames poll, and draw and
    // present nothing. The frame timer is the only timer.
    let (frames, presented) = open.ui.frames();
    let work = open.ui.work();
    for _ in 0..600 {
        open.ui.tick();
    }
    assert_eq!(open.ui.frames(), (frames + 600, presented));
    assert_eq!(open.ui.work(), work);
    assert_eq!(open.ui.window().timers(), [(TimerId(1), FRAME_PERIOD_MS)]);
    // The pointer comes over the knob: its hover eases in over the theme's
    // fast duration, presenting a frame each tick until it settles, and
    // nothing after.
    let knob = open.ui.find(Role::Slider, "Gain").expect("a knob");
    open.ui.move_to(knob.bounds.center());
    let (_, before) = open.ui.frames();
    for _ in 0..30 {
        open.ui.tick();
    }
    let (_, after) = open.ui.frames();
    let moving = after - before;
    assert!((4..=9).contains(&moving), "{moving} frames presented");
    for _ in 0..60 {
        open.ui.tick();
    }
    assert_eq!(open.ui.frames().1, after);
    drop(open.editor);
    drop(open.link);
}

#[test]
fn closing_leaves_no_window_timer_or_gesture_behind() {
    let params = params();
    let open = open(
        &spec(&params, &Arc::default(), &Arc::default()),
        &params,
        Some(1.5),
    );
    let window = open.ui.window().clone();
    assert_eq!(window.timers(), [(TimerId(1), FRAME_PERIOD_MS)]);
    open.ui.tick();
    let knob = open.ui.find(Role::Slider, "Gain").expect("a knob");
    open.ui.press(knob.bounds.center());
    open.host.take_calls();
    drop(open.editor);
    let id = params.gain.param_id();
    assert_eq!(open.host.take_calls(), [HostCall::EndGesture(id)]);
    assert!(!window.exists());
    assert_eq!(window.timers(), []);
    assert!(!open.ui.is_open());
    assert_eq!(open.ui.tick(), 0);
    drop(open.link);
    assert_eq!(open.host.well_formed(), Ok(()));
}
