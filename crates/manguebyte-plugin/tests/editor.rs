//! The editor contract from the outside: the exact calls a host receives
//! from gestures on every path, what the link to the host guarantees when
//! an editor closes, waking an editor from other threads, resizing against
//! a host that answers synchronously, the test doubles, and the harness
//! opening a plugin's editor while it processes.

use std::cell::{Cell, RefCell};
use std::fmt::{self, Write as _};
use std::mem::ManuallyDrop;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use manguebyte_core::AudioBuffer;
use manguebyte_dsp::event::EventKind;
use manguebyte_dsp::{ProcessContext, ProcessSpec};
use manguebyte_params::{FloatParam, FloatRange, Param, ParamId, param_set};
use manguebyte_plugin::editor::testing::{HostCall, ManualLoop, RecordingEditorHost};
use manguebyte_plugin::editor::{
    EditorContext, EditorError, EditorHost, EditorSpec, Gesture, HostLink, HostLoop, HostSize, Key,
    KeyEvent, LoopHandle, LoopToken, Modifiers, OpenEditor, ParentWindow, Resize, WindowApi,
};
use manguebyte_plugin::{AudioLayout, Harness, Plugin, PluginInfo, PluginKind};

use HostCall::{BeginGesture as Begin, EndGesture as End, Perform};

const CUTOFF: ParamId = ParamId::from_name("cutoff");
const DEPTH: ParamId = ParamId::from_name("depth");
const MIX: ParamId = ParamId::from_name("mix");

/// A recording host, a context attached to it, and the link.
fn attached() -> (Rc<RecordingEditorHost>, EditorContext, HostLink) {
    let recorder = Rc::new(RecordingEditorHost::new());
    let (context, link) = EditorContext::attach(recorder.clone());
    (recorder, context, link)
}

/// A host that records every call and, when armed, panics in
/// `begin_gesture` or `perform` after recording it, as a broken wrapper
/// might.
struct Flaky {
    inner: RecordingEditorHost,
    panic_in_begin: Cell<bool>,
    panic_in_perform: Cell<bool>,
}

impl Flaky {
    fn new() -> Self {
        Self {
            inner: RecordingEditorHost::new(),
            panic_in_begin: Cell::new(false),
            panic_in_perform: Cell::new(false),
        }
    }
}

impl EditorHost for Flaky {
    fn begin_gesture(&self, param: ParamId) {
        self.inner.begin_gesture(param);
        if self.panic_in_begin.get() {
            panic!("the host failed in begin_gesture");
        }
    }
    fn perform(&self, param: ParamId, normalized: f64) {
        self.inner.perform(param, normalized);
        if self.panic_in_perform.get() {
            panic!("the host failed in perform");
        }
    }
    fn end_gesture(&self, param: ParamId) {
        self.inner.end_gesture(param);
    }
    fn values_changed(&self) {
        self.inner.values_changed();
    }
    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        self.inner.request_resize(size)
    }
    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        None
    }
    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        None
    }
    fn closed(&self) {
        self.inner.closed();
    }
}

// --- The guard, path by path. ---

#[test]
fn a_drag_is_begin_values_end() {
    let (recorder, context, _link) = attached();
    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    assert_eq!(recorder.calls(), [Begin(CUTOFF)]);
    assert_eq!(drag.sent(), None);
    drag.set(0.3);
    drag.set(0.3); // the same value again is not sent
    drag.set(f64::NAN); // not a number: ignored
    drag.set(1.7); // clamped
    drag.set(f64::INFINITY); // clamped to 1.0, which was just sent
    drag.set(-0.0); // clamped to plain zero
    assert_eq!(drag.sent(), Some(0.0));
    assert_eq!(drag.start(), Some(0.25));
    assert_eq!(drag.param(), CUTOFF);
    assert!(drag.is_live());
    drop(drag);

    let calls = recorder.take_calls();
    assert_eq!(
        calls,
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.3),
            Perform(CUTOFF, 1.0),
            Perform(CUTOFF, 0.0),
            End(CUTOFF),
        ]
    );
    // Not a negative zero.
    assert!(matches!(calls[3], Perform(_, value) if value.is_sign_positive()));
    assert!(context.open_gestures().open_params().is_empty());
}

#[test]
fn ending_by_hand_is_the_same_as_dropping() {
    let (recorder, context, _link) = attached();
    let mut drag = Gesture::begin(&context, CUTOFF, 0.5);
    drag.set(0.75);
    drag.end();
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.75), End(CUTOFF)]
    );
}

#[test]
fn cancel_puts_the_starting_value_back_then_ends() {
    let (recorder, context, _link) = attached();
    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    drag.set(0.5);
    drag.set(0.9);
    drag.cancel();
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.9),
            Perform(CUTOFF, 0.25),
            End(CUTOFF),
        ]
    );

    // Nothing was sent, so there is nothing to put back.
    Gesture::begin(&context, CUTOFF, 0.25).cancel();
    assert_eq!(recorder.take_calls(), [Begin(CUTOFF), End(CUTOFF)]);

    // No starting value was known.
    let mut drag = Gesture::begin(&context, CUTOFF, f64::NAN);
    drag.set(0.5);
    assert_eq!(drag.start(), None);
    drag.cancel();
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.5), End(CUTOFF)]
    );

    // A starting value out of range is put back clamped.
    let mut drag = Gesture::begin(&context, CUTOFF, 4.0);
    drag.set(0.5);
    drag.cancel();
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 1.0),
            End(CUTOFF)
        ]
    );

    // The value was moved away and back: the host already has the start.
    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    drag.set(0.5);
    drag.set(0.25);
    drag.cancel();
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.25),
            End(CUTOFF)
        ]
    );
}

#[test]
fn a_panic_during_a_drag_still_ends_the_gesture() {
    let (recorder, context, _link) = attached();
    let result = catch_unwind(AssertUnwindSafe(|| {
        let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
        drag.set(0.5);
        let mut other = Gesture::begin(&context, DEPTH, 0.0);
        other.set(0.125);
        panic!("a widget failed in the middle of a drag");
    }));
    assert!(result.is_err());
    // Locals drop in reverse order while unwinding.
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Begin(DEPTH),
            Perform(DEPTH, 0.125),
            End(DEPTH),
            End(CUTOFF),
        ]
    );
    assert!(context.open_gestures().open_params().is_empty());
}

#[test]
fn nested_gestures_on_one_parameter_reach_the_host_as_one() {
    let (recorder, context, _link) = attached();
    let mut mouse = Gesture::begin(&context, CUTOFF, 0.25);
    mouse.set(0.5);
    let mut reader = Gesture::begin(&context, CUTOFF, 0.5); // a screen reader joins
    assert_eq!(context.open_gestures().depth(CUTOFF), 2);
    reader.set(0.6);
    drop(mouse); // the first to begin is the first to end: no end yet
    reader.set(0.7);
    assert_eq!(recorder.open_at_host(), [CUTOFF]);
    drop(reader);

    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.6),
            Perform(CUTOFF, 0.7),
            End(CUTOFF),
        ]
    );
    assert_eq!(context.open_gestures().depth(CUTOFF), 0);
}

#[test]
fn cancelling_a_nested_gesture_restores_its_own_start_and_ends_nothing() {
    let (recorder, context, _link) = attached();
    let mut outer = Gesture::begin(&context, CUTOFF, 0.1);
    outer.set(0.2);
    let mut inner = Gesture::begin(&context, CUTOFF, 0.2);
    inner.set(0.8);
    inner.cancel();
    outer.set(0.3);
    outer.cancel();
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.2),
            Perform(CUTOFF, 0.8),
            Perform(CUTOFF, 0.2), // the inner one's start
            Perform(CUTOFF, 0.3),
            Perform(CUTOFF, 0.1), // the outer one's start
            End(CUTOFF),
        ]
    );
}

/// A drag sets 0.5, a key press sets 0.9 on the same parameter while the
/// drag is open, and the drag moves back to 0.5. The host holds 0.9 at
/// that point, so 0.5 is sent again; and both gestures report what the
/// host was last sent.
#[test]
fn overlapping_gestures_do_not_swallow_a_value_the_host_no_longer_has() {
    let (recorder, context, _link) = attached();
    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    drag.set(0.5);
    let mut key = Gesture::begin(&context, CUTOFF, 0.5);
    assert_eq!(key.sent(), None, "nothing sent since the key went down");
    key.set(0.9);
    assert_eq!(drag.sent(), Some(0.9));
    drop(key);
    drag.set(0.5);
    assert_eq!(drag.sent(), Some(0.5));
    drop(drag);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.9),
            Perform(CUTOFF, 0.5),
            End(CUTOFF),
        ],
        "the host was left at 0.9 while the drag shows 0.5"
    );

    // The same with a whole edit in the middle of the drag.
    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    drag.set(0.5);
    Gesture::set_once(&context, CUTOFF, 0.9);
    drag.set(0.5);
    drop(drag);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.9),
            Perform(CUTOFF, 0.5),
            End(CUTOFF),
        ]
    );
}

#[test]
fn gestures_on_different_parameters_are_independent() {
    let (recorder, context, _link) = attached();
    let mut cutoff = Gesture::begin(&context, CUTOFF, 0.0);
    let mut depth = Gesture::begin(&context, DEPTH, 0.0);
    cutoff.set(0.5);
    depth.set(0.5);
    drop(cutoff);
    depth.set(0.25);
    drop(depth);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Begin(DEPTH),
            Perform(CUTOFF, 0.5),
            Perform(DEPTH, 0.5),
            End(CUTOFF),
            Perform(DEPTH, 0.25),
            End(DEPTH),
        ]
    );
}

#[test]
fn a_single_edit_is_a_whole_gesture() {
    let (recorder, context, _link) = attached();
    Gesture::set_once(&context, DEPTH, 0.75);
    Gesture::set_once(&context, DEPTH, f64::NAN);
    assert_eq!(
        recorder.take_calls(),
        [Begin(DEPTH), Perform(DEPTH, 0.75), End(DEPTH)]
    );

    // Inside a drag on the same parameter it joins the drag.
    let mut drag = Gesture::begin(&context, DEPTH, 0.75);
    Gesture::set_once(&context, DEPTH, 0.0);
    drag.set(0.5);
    drop(drag);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(DEPTH),
            Perform(DEPTH, 0.0),
            Perform(DEPTH, 0.5),
            End(DEPTH)
        ]
    );
}

#[test]
fn a_begin_that_panics_is_still_followed_by_its_end() {
    let failing = Rc::new(Flaky::new());
    let (context, _link) = EditorContext::attach(failing.clone());
    failing.panic_in_begin.set(true);
    let result = catch_unwind(AssertUnwindSafe(|| Gesture::begin(&context, CUTOFF, 0.0)));
    assert!(result.is_err());
    assert_eq!(failing.inner.calls(), [Begin(CUTOFF), End(CUTOFF)]);
    assert!(!context.open_gestures().is_open(CUTOFF));
}

/// The host panics while a cancel puts the starting value back. The end is
/// still sent, and the parameter is free for the next gesture, which
/// reaches the host whole.
#[test]
fn a_cancel_whose_restore_panics_still_ends_the_gesture() {
    let failing = Rc::new(Flaky::new());
    let (context, _link) = EditorContext::attach(failing.clone());

    let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
    drag.set(0.5);
    failing.panic_in_perform.set(true);
    let result = catch_unwind(AssertUnwindSafe(|| drag.cancel()));
    assert!(result.is_err());
    failing.panic_in_perform.set(false);

    assert_eq!(
        failing.inner.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.5),
            Perform(CUTOFF, 0.25),
            End(CUTOFF)
        ],
        "the gesture was left open at the host"
    );
    assert_eq!(context.open_gestures().depth(CUTOFF), 0);

    let mut next = Gesture::begin(&context, CUTOFF, 0.25);
    next.set(0.75);
    drop(next);
    assert_eq!(
        failing.inner.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.75), End(CUTOFF)]
    );
}

/// A host whose own methods let the editor's code run again, as a host
/// that pumps window messages inside `performEdit` does: here `perform`
/// starts and ends a gesture on the same parameter. The table is
/// consistent while the host is called.
struct Reentrant {
    inner: RecordingEditorHost,
    context: RefCell<Option<EditorContext>>,
    depth_seen_in_begin: Cell<u32>,
}

impl EditorHost for Reentrant {
    fn begin_gesture(&self, param: ParamId) {
        let context = self.context.borrow().clone();
        if let Some(context) = context {
            self.depth_seen_in_begin
                .set(context.open_gestures().depth(param));
        }
        self.inner.begin_gesture(param);
    }
    fn perform(&self, param: ParamId, normalized: f64) {
        self.inner.perform(param, normalized);
        let context = self.context.borrow().clone();
        if let Some(context) = context {
            let echo = Gesture::begin(&context, param, normalized);
            drop(echo);
        }
    }
    fn end_gesture(&self, param: ParamId) {
        self.inner.end_gesture(param);
    }
    fn values_changed(&self) {}
    fn request_resize(&self, _: HostSize) -> Option<HostSize> {
        None
    }
    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        None
    }
    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        None
    }
    fn closed(&self) {}
}

#[test]
fn a_host_may_let_gestures_start_from_inside_its_own_methods() {
    let reentrant = Rc::new(Reentrant {
        inner: RecordingEditorHost::new(),
        context: RefCell::new(None),
        depth_seen_in_begin: Cell::new(0),
    });
    let (context, link) = EditorContext::attach(reentrant.clone());
    *reentrant.context.borrow_mut() = Some(context.clone());

    let mut drag = Gesture::begin(&context, CUTOFF, 0.0);
    assert_eq!(reentrant.depth_seen_in_begin.get(), 1);
    drag.set(0.5);
    drop(drag);
    assert_eq!(
        reentrant.inner.calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.5), End(CUTOFF)]
    );

    // Break the cycle between the host and the context.
    reentrant.context.borrow_mut().take();
    drop(link);
}

/// A writer that fails, as a broken log sink might.
struct Boom;

impl fmt::Write for Boom {
    fn write_str(&mut self, _: &str) -> fmt::Result {
        panic!("the log sink failed");
    }
}

/// Printing the gesture table with a formatter that panics loses nothing:
/// the open gesture is still open, and its end still reaches the host.
#[test]
fn printing_the_table_cannot_lose_open_gestures() {
    let (recorder, context, _link) = attached();
    let drag = Gesture::begin(&context, CUTOFF, 0.5);
    for printed in [
        catch_unwind(AssertUnwindSafe(|| {
            let _ = write!(Boom, "{:?}", context.open_gestures());
        })),
        catch_unwind(AssertUnwindSafe(|| {
            let _ = write!(Boom, "{context:?}");
        })),
        catch_unwind(AssertUnwindSafe(|| {
            let _ = write!(Boom, "{drag:?}");
        })),
    ] {
        assert!(printed.is_err());
        assert_eq!(
            context.open_gestures().depth(CUTOFF),
            1,
            "the table was lost"
        );
    }
    let mut printed = String::new();
    write!(printed, "{:?}", context.open_gestures()).expect("a String accepts text");
    assert_eq!(printed, format!("{{{CUTOFF:?}: 1}}"));
    drop(drag);
    assert_eq!(recorder.take_calls(), [Begin(CUTOFF), End(CUTOFF)]);
}

/// Random programs of begins, values, ends, cancels, whole edits, leaked
/// gestures and closed editors, on three parameters with several gestures
/// open at once and a host that sometimes panics, compared call for call
/// with a model of the rules, and checked after every step against the
/// rules plugin formats impose.
#[test]
fn random_gesture_programs_follow_the_model_exactly() {
    const PARAMS: [ParamId; 3] = [CUTOFF, DEPTH, MIX];

    /// One open gesture, as the model sees it.
    #[derive(Clone, Copy)]
    struct Guard {
        which: usize,
        start: Option<f64>,
        since: u64,
    }

    /// One parameter, as the model sees it.
    #[derive(Clone, Copy, Default)]
    struct Open {
        depth: u32,
        last: Option<f64>,
        sends: u64,
    }

    #[derive(Default)]
    struct Model {
        params: [Open; 3],
        /// Open parameters, in the order they were first opened.
        order: Vec<usize>,
        expected: Vec<HostCall>,
    }

    impl Model {
        fn enter(&mut self, which: usize) -> (bool, u64) {
            let open = &mut self.params[which];
            if open.depth == 0 {
                *open = Open {
                    depth: 1,
                    ..Open::default()
                };
                self.order.push(which);
                self.expected.push(Begin(PARAMS[which]));
                (true, 0)
            } else {
                open.depth += 1;
                (false, open.sends)
            }
        }

        fn send(&mut self, which: usize, value: f64) -> bool {
            let open = &mut self.params[which];
            if open.last == Some(value) {
                return false;
            }
            open.last = Some(value);
            open.sends += 1;
            self.expected.push(Perform(PARAMS[which], value));
            true
        }

        fn restore(&mut self, guard: Guard) -> bool {
            let open = &self.params[guard.which];
            match guard.start {
                Some(start) if open.sends != guard.since && open.last != Some(start) => {
                    self.send(guard.which, start)
                }
                _ => false,
            }
        }

        fn leave(&mut self, which: usize) {
            let open = &mut self.params[which];
            open.depth -= 1;
            if open.depth == 0 {
                self.order.retain(|&w| w != which);
                self.expected.push(End(PARAMS[which]));
            }
        }

        fn close_all(&mut self) {
            for which in std::mem::take(&mut self.order) {
                self.params[which] = Open::default();
                self.expected.push(End(PARAMS[which]));
            }
        }

        fn sent(&self, guard: Guard) -> Option<f64> {
            let open = &self.params[guard.which];
            if open.sends != guard.since {
                open.last
            } else {
                None
            }
        }
    }

    fn clamp(value: f64) -> f64 {
        if value > 1.0 {
            1.0
        } else if value > 0.0 {
            value
        } else {
            0.0
        }
    }

    let mut random = Random(7);
    let programs = if cfg!(miri) { 12 } else { 2_000 };
    for _ in 0..programs {
        let host = Rc::new(Flaky::new());
        let (mut context, mut link) = EditorContext::attach(host.clone());
        let mut real: Vec<Option<Gesture>> = Vec::new();
        let mut guards: Vec<Option<Guard>> = Vec::new();
        // Gestures kept alive after their editor closed.
        let mut zombies: Vec<Gesture> = Vec::new();
        // Gestures the editor leaked: their destructors do not run until
        // the program is over and every link has been dropped.
        let mut leaked: Vec<ManuallyDrop<Gesture>> = Vec::new();
        let mut model = Model::default();

        for _ in 0..40 {
            let live: Vec<usize> = (0..real.len()).filter(|&i| real[i].is_some()).collect();
            let choice = random.below(20);
            let panic_in_begin = random.below(6) == 0;
            let panic_in_perform = random.below(6) == 0;
            host.panic_in_begin.set(panic_in_begin);
            host.panic_in_perform.set(panic_in_perform);

            if live.is_empty() || choice < 5 {
                // Begin.
                let which = random.below(3);
                let start = random.value();
                let (first, since) = model.enter(which);
                let panics = panic_in_begin && first;
                if panics {
                    model.leave(which);
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    Gesture::begin(&context, PARAMS[which], start)
                }));
                assert_eq!(result.is_err(), panics);
                if let Ok(gesture) = result {
                    real.push(Some(gesture));
                    guards.push(Some(Guard {
                        which,
                        start: (!start.is_nan()).then(|| clamp(start)),
                        since,
                    }));
                }
            } else if choice < 11 {
                // A value.
                let slot = live[random.below(live.len())];
                let guard = guards[slot].expect("live");
                let value = random.value();
                let panics =
                    !value.is_nan() && model.send(guard.which, clamp(value)) && panic_in_perform;
                let gesture = real[slot].as_mut().expect("live");
                let result = catch_unwind(AssertUnwindSafe(|| gesture.set(value)));
                assert_eq!(result.is_err(), panics);
            } else if choice < 14 {
                // End or cancel.
                let slot = live[random.below(live.len())];
                let guard = guards[slot].take().expect("live");
                let cancel = random.below(2) == 0;
                let panics = cancel && model.restore(guard) && panic_in_perform;
                model.leave(guard.which);
                let gesture = real[slot].take().expect("live");
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if cancel {
                        gesture.cancel();
                    } else {
                        gesture.end();
                    }
                }));
                assert_eq!(result.is_err(), panics);
            } else if choice < 16 {
                // A whole edit.
                let which = random.below(3);
                let value = random.value();
                let mut panics = false;
                if !value.is_nan() {
                    let (first, _) = model.enter(which);
                    if panic_in_begin && first {
                        panics = true;
                    } else {
                        panics = model.send(which, clamp(value)) && panic_in_perform;
                    }
                    model.leave(which);
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    Gesture::set_once(&context, PARAMS[which], value);
                }));
                assert_eq!(result.is_err(), panics);
            } else if choice == 16 {
                // The editor leaks a gesture: it stays open.
                let slot = live[random.below(live.len())];
                guards[slot] = None;
                leaked.push(ManuallyDrop::new(real[slot].take().expect("live")));
            } else if choice == 17 {
                // A gesture kept after its editor closed is used: nothing
                // reaches the host.
                if !zombies.is_empty() {
                    let mut zombie = zombies.swap_remove(random.below(zombies.len()));
                    assert!(!zombie.is_live());
                    assert_eq!(zombie.sent(), None);
                    match random.below(4) {
                        0 => {
                            zombie.set(random.value());
                            zombies.push(zombie);
                        }
                        1 => zombie.cancel(),
                        2 => zombie.end(),
                        _ => drop(zombie),
                    }
                }
            } else {
                // The editor closes. It drops what it holds, keeping some
                // gestures alive past its end, and the wrapper drops the
                // link, which ends whatever is still open.
                for (slot, guard) in guards.iter_mut().enumerate() {
                    if let Some(guard) = guard.take() {
                        let gesture = real[slot].take().expect("live");
                        if random.below(2) == 0 {
                            zombies.push(gesture);
                        } else {
                            model.leave(guard.which);
                            drop(gesture);
                        }
                    }
                }
                model.close_all();
                drop(link);
                assert!(!context.is_attached());
                (context, link) = EditorContext::attach(host.clone());
                real.clear();
                guards.clear();
            }
            host.panic_in_begin.set(false);
            host.panic_in_perform.set(false);

            // After every step: the exact calls, the format's rules, a host
            // that sees a gesture open exactly where one is open, and what
            // each gesture reports as sent.
            assert_eq!(host.inner.calls(), model.expected);
            assert_eq!(host.inner.well_formed(), Ok(()));
            let mut at_host = host.inner.open_at_host();
            at_host.sort();
            let mut open: Vec<ParamId> = model.order.iter().map(|&w| PARAMS[w]).collect();
            assert_eq!(context.open_gestures().open_params(), open);
            open.sort();
            assert_eq!(at_host, open);
            for (which, &param) in PARAMS.iter().enumerate() {
                assert_eq!(
                    context.open_gestures().depth(param),
                    model.params[which].depth
                );
            }
            for (gesture, guard) in real.iter().zip(&guards) {
                if let (Some(gesture), Some(guard)) = (gesture, guard) {
                    assert_eq!(gesture.sent(), model.sent(*guard));
                }
            }
        }
        for (slot, guard) in guards.iter_mut().enumerate() {
            if let Some(guard) = guard.take() {
                model.leave(guard.which);
                drop(real[slot].take());
            }
        }
        model.close_all();
        drop(link);
        drop(zombies);
        // The leaked gestures are dropped at last, long after their
        // editors closed: nothing more reaches the host.
        for gesture in leaked {
            assert!(!gesture.is_live());
            drop(ManuallyDrop::into_inner(gesture));
        }
        assert_eq!(host.inner.calls(), model.expected);
        assert_eq!(host.inner.well_formed(), Ok(()));
        assert!(host.inner.open_at_host().is_empty());
    }
}

// --- The link: what is left when an editor closes. ---

/// An editor leaks a gesture in the middle of a drag. The parameter stays
/// open until the editor closes; then the link ends it, and the next editor
/// starts with nothing open.
#[test]
fn a_leaked_gesture_is_ended_when_the_link_is_dropped() {
    let (recorder, context, link) = attached();
    let mut drag = Gesture::begin(&context, CUTOFF, 0.5);
    drag.set(0.6);
    // Leaked: its destructor does not run until the end of the test.
    let leaked = ManuallyDrop::new(drag);

    // A later drag on the same parameter joins the leaked gesture.
    let mut drag = Gesture::begin(&context, CUTOFF, 0.6);
    drag.set(0.7);
    drop(drag);
    assert_eq!(recorder.open_at_host(), [CUTOFF]);

    drop(link);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(CUTOFF),
            Perform(CUTOFF, 0.6),
            Perform(CUTOFF, 0.7),
            End(CUTOFF)
        ]
    );
    assert!(context.open_gestures().open_params().is_empty());

    // The next editor gets a context of its own.
    let (context, _link) = EditorContext::attach(recorder.clone());
    let mut drag = Gesture::begin(&context, CUTOFF, 0.7);
    drag.set(0.8);
    drop(drag);
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.8), End(CUTOFF)]
    );

    // The leaked gesture is dropped at last, while the next editor is open
    // with nothing on that parameter: it sends nothing, and it does not
    // touch the next editor's gestures.
    assert!(!leaked.is_live());
    let mut next = Gesture::begin(&context, CUTOFF, 0.8);
    drop(ManuallyDrop::into_inner(leaked));
    assert_eq!(context.open_gestures().depth(CUTOFF), 1);
    next.set(0.9);
    drop(next);
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.9), End(CUTOFF)]
    );
    assert_eq!(recorder.well_formed(), Ok(()));
}

/// An editor keeps a gesture, and a clone of its context, after it is
/// closed. Nothing done with them reaches the host, and the host itself is
/// let go of when the link is dropped.
#[test]
fn a_context_kept_after_its_editor_closed_reaches_nothing() {
    let host_loop = Rc::new(ManualLoop::new());
    let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
    let (context, link) = EditorContext::attach(recorder.clone());
    let mut kept = Gesture::begin(&context, DEPTH, 0.0);
    kept.set(0.25);
    assert!(kept.is_live());
    assert!(context.has_host_loop());
    assert!(context.main_thread_waker().is_some());

    drop(link);
    assert_eq!(
        recorder.take_calls(),
        [Begin(DEPTH), Perform(DEPTH, 0.25), End(DEPTH)]
    );
    assert_eq!(Rc::strong_count(&recorder), 1, "the host was let go of");

    assert!(!context.is_attached());
    assert!(!kept.is_live());
    assert_eq!(kept.sent(), None);
    kept.set(0.5);
    kept.cancel();
    let mut late = Gesture::begin(&context, DEPTH, 0.5);
    late.set(0.75);
    assert!(!late.is_live());
    drop(late);
    Gesture::set_once(&context, MIX, 1.0);
    assert_eq!(context.request_resize(HostSize::new(10, 10)), None);
    assert!(context.every(16, || {}).is_none());
    assert!(context.watch_fd(3, || {}).is_none());
    assert!(context.main_thread_waker().is_none());
    assert!(!context.has_host_loop());
    context.values_changed();
    context.closed();

    assert!(recorder.take_calls().is_empty());
    assert!(host_loop.timers().is_empty());
    assert_eq!(host_loop.cancels(), 0);
    assert!(format!("{context:?}").contains("attached: false"));
}

/// An editor that leaks its timer and descriptor handles: the link cancels
/// both, their callbacks are dropped, and nothing fires after the editor
/// is gone. Handles dropped after that do not touch the loop.
#[test]
fn timers_and_watches_an_editor_leaks_are_cancelled_by_the_link() {
    let host_loop = Rc::new(ManualLoop::new());
    let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
    let (context, link) = EditorContext::attach(recorder.clone());

    let fired = Rc::new(Cell::new(0));
    let (on_tick, on_ready) = (Rc::clone(&fired), Rc::clone(&fired));
    let timer = context
        .every(16, move || on_tick.set(on_tick.get() + 1))
        .expect("a timer");
    let watch = context
        .watch_fd(5, move || on_ready.set(on_ready.get() + 1))
        .expect("a watch");
    let kept = context.every(33, || {}).expect("another timer");
    assert_eq!(host_loop.timers(), [16, 33]);
    assert_eq!(host_loop.watched_fds(), [5]);
    assert_eq!(host_loop.fire_timers(), 2);
    assert_eq!(host_loop.fire_fd(5), 1);
    assert_eq!(fired.get(), 2);

    // Leaked: their destructors do not run until the end of the test.
    let leaked = ManuallyDrop::new((timer, watch));
    drop(link);
    assert!(host_loop.timers().is_empty());
    assert!(host_loop.watched_fds().is_empty());
    assert_eq!(host_loop.fire_timers(), 0);
    assert_eq!(host_loop.fire_fd(5), 0);
    assert_eq!(fired.get(), 2);
    assert_eq!(Rc::strong_count(&fired), 1, "the callbacks were dropped");
    assert_eq!(host_loop.cancels(), 3);

    // The handle the editor kept, and at last the leaked ones, are dropped
    // late: the loop is not called.
    drop(kept);
    drop(ManuallyDrop::into_inner(leaked));
    assert_eq!(host_loop.cancels(), 3);
    assert!(recorder.take_calls().is_empty());
}

/// A timer whose callback cancels itself by dropping its own handle, and a
/// handle cancelled by hand: each is cancelled once, and the loop is called
/// once for each.
#[test]
fn loop_handles_cancel_once_whoever_drops_them() {
    let host_loop = Rc::new(ManualLoop::new());
    let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
    let (context, _link) = EditorContext::attach(recorder);

    let own: Rc<RefCell<Option<LoopHandle>>> = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&own);
    let ticks = Rc::new(Cell::new(0));
    let counter = Rc::clone(&ticks);
    let handle = context
        .every(10, move || {
            counter.set(counter.get() + 1);
            if counter.get() == 2 {
                // Dropping the handle from inside its own callback.
                slot.borrow_mut().take();
            }
        })
        .expect("a timer");
    *own.borrow_mut() = Some(handle);

    let by_hand = context.every(20, || {}).expect("a timer");
    assert_eq!(host_loop.fire_timers(), 2);
    assert_eq!(host_loop.fire_timers(), 2);
    assert_eq!(host_loop.timers(), [20]);
    assert_eq!(ticks.get(), 2);
    by_hand.cancel();
    assert!(host_loop.timers().is_empty());
    assert_eq!(host_loop.cancels(), 2);

    host_loop.set_refusing(true);
    assert!(context.every(10, || {}).is_none());
    assert_eq!(host_loop.cancels(), 2);
}

/// Dropping the link before the editor, the wrong way round, still sends
/// one end per gesture and cancels each timer once.
#[test]
fn dropping_the_link_first_still_ends_everything_once() {
    let host_loop = Rc::new(ManualLoop::new());
    let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
    let (context, link) = EditorContext::attach(recorder.clone());
    let parent = ParentWindow::win32(0x10).expect("not null");
    let mut editor = ToySpec.open(parent, None, context).expect("opens");
    assert!(editor.key(key_d(true)));
    assert_eq!(host_loop.timers(), [16]);

    drop(link);
    drop(editor);
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.75), End(CUTOFF)]
    );
    assert!(host_loop.timers().is_empty());
    assert_eq!(host_loop.cancels(), 1);
}

// --- Waking the editor on the main thread. ---

/// An editor that lists presets a worker thread finds, and shows them when
/// the host calls it back on the main thread.
struct PresetBrowser {
    inbox: mpsc::Receiver<String>,
    shown: Rc<RefCell<Vec<String>>>,
}

impl OpenEditor for PresetBrowser {
    fn size(&self) -> HostSize {
        HostSize::new(100, 100)
    }
    fn fit(&self, _: HostSize) -> HostSize {
        self.size()
    }
    fn set_size(&mut self, size: HostSize) -> bool {
        size == self.size()
    }
    fn set_scale(&mut self, _: f64) -> bool {
        false
    }
    fn set_visible(&mut self, _: bool) {}
    fn on_main_thread(&mut self) {
        while let Ok(preset) = self.inbox.try_recv() {
            self.shown.borrow_mut().push(preset);
        }
    }
}

#[test]
fn a_worker_thread_wakes_the_editor_on_the_main_thread() {
    let (recorder, context, link) = attached();
    let waker = context.main_thread_waker().expect("callbacks are offered");
    let (found, inbox) = mpsc::channel();
    let shown = Rc::new(RefCell::new(Vec::new()));
    let mut editor = PresetBrowser {
        inbox,
        shown: Rc::clone(&shown),
    };

    let worker = std::thread::spawn(move || {
        for preset in ["Warm", "Bright", "Dark"] {
            found.send(preset.to_owned()).expect("the editor is open");
            assert!(waker.wake());
        }
        waker
    });
    let waker = worker.join().expect("the worker finished");

    // The host was asked three times; the wrapper calls the editor back,
    // here once for all three.
    assert_eq!(
        recorder.take_calls(),
        [HostCall::Wake, HostCall::Wake, HostCall::Wake]
    );
    editor.on_main_thread();
    assert_eq!(*shown.borrow(), ["Warm", "Bright", "Dark"]);

    // Once the editor has closed, waking does nothing, from any thread.
    drop(editor);
    drop(link);
    let late = std::thread::spawn(move || waker.wake())
        .join()
        .expect("the late thread finished");
    assert!(!late);
    assert!(recorder.take_calls().is_empty());
}

/// A host whose main-thread callback says when it has been dropped.
struct CallbackHost {
    wakes: Arc<AtomicUsize>,
    dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

impl EditorHost for CallbackHost {
    fn begin_gesture(&self, _: ParamId) {}
    fn perform(&self, _: ParamId, _: f64) {}
    fn end_gesture(&self, _: ParamId) {}
    fn values_changed(&self) {}
    fn request_resize(&self, _: HostSize) -> Option<HostSize> {
        None
    }
    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        let wakes = Arc::clone(&self.wakes);
        let flag = SetOnDrop(Arc::clone(&self.dropped));
        Some(Box::new(move || {
            let _keep = &flag;
            wakes.fetch_add(1, Ordering::AcqRel);
        }))
    }
    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        None
    }
    fn closed(&self) {}
}

#[test]
fn the_hosts_callback_is_dropped_with_the_link() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let host = Rc::new(CallbackHost {
        wakes: Arc::clone(&wakes),
        dropped: Arc::clone(&dropped),
    });
    let (context, link) = EditorContext::attach(host);
    let waker = context.main_thread_waker().expect("offered");
    let copy = waker.clone();
    assert!(waker.wake() && copy.wake());
    assert_eq!(wakes.load(Ordering::Acquire), 2);
    assert!(!dropped.load(Ordering::Acquire));

    drop(link);
    assert!(
        dropped.load(Ordering::Acquire),
        "the function outlived the link"
    );
    assert!(!copy.wake());
    assert_eq!(wakes.load(Ordering::Acquire), 2);
    assert!(format!("{waker:?}").contains("attached: false"));
}

#[test]
fn a_host_without_callbacks_gives_no_waker() {
    let recorder = Rc::new(RecordingEditorHost::new().without_main_thread_callback());
    let (context, _link) = EditorContext::attach(recorder);
    assert!(context.main_thread_waker().is_none());
}

// --- Resizing against a host that answers synchronously. ---

/// A VST 3 wrapper and its host, in one. The host answers `resizeView` by
/// calling `onSize` before it returns, keeping windows within `limit`. The
/// wrapper follows the contract: it holds the editor in a `RefCell`, and a
/// size the host sets while the editor is asking for one becomes the answer
/// instead of a call into the editor.
struct SyncResizeWrapper {
    editor: RefCell<Option<Box<dyn OpenEditor>>>,
    limit: HostSize,
    requesting: Cell<bool>,
    set_during_request: Cell<Option<HostSize>>,
    requests: Cell<usize>,
}

impl SyncResizeWrapper {
    fn new(limit: HostSize) -> Rc<Self> {
        Rc::new(Self {
            editor: RefCell::new(None),
            limit,
            requesting: Cell::new(false),
            set_during_request: Cell::new(None),
            requests: Cell::new(0),
        })
    }

    /// `IPlugView::onSize`, as the host calls it.
    fn on_size(&self, size: HostSize) -> bool {
        if self.requesting.get() {
            self.set_during_request.set(Some(size));
            return true;
        }
        self.editor
            .borrow_mut()
            .as_mut()
            .is_some_and(|editor| editor.set_size(size))
    }

    /// `IPlugFrame::resizeView`, as the host implements it: the window is
    /// resized at once, within the limit, and `onSize` follows.
    fn resize_view(&self, size: HostSize) -> bool {
        let granted = HostSize::new(
            size.width.min(self.limit.width),
            size.height.min(self.limit.height),
        );
        self.on_size(granted)
    }

    /// What the host does when it calls `setContentScaleFactor`.
    fn set_scale(&self, scale: f64) -> bool {
        self.editor
            .borrow_mut()
            .as_mut()
            .is_some_and(|editor| editor.set_scale(scale))
    }

    fn editor_size(&self) -> Option<HostSize> {
        self.editor.borrow().as_ref().map(|editor| editor.size())
    }
}

impl EditorHost for SyncResizeWrapper {
    fn begin_gesture(&self, _: ParamId) {}
    fn perform(&self, _: ParamId, _: f64) {}
    fn end_gesture(&self, _: ParamId) {}
    fn values_changed(&self) {}
    fn request_resize(&self, size: HostSize) -> Option<HostSize> {
        self.requests.set(self.requests.get() + 1);
        self.requesting.set(true);
        let accepted = self.resize_view(size);
        self.requesting.set(false);
        let set = self.set_during_request.take();
        accepted.then(|| set.unwrap_or(size))
    }
    fn main_thread_callback(&self) -> Option<Box<dyn Fn() + Send + Sync>> {
        None
    }
    fn host_loop(&self) -> Option<Rc<dyn HostLoop>> {
        None
    }
    fn closed(&self) {}
}

/// An editor that does what [`OpenEditor::set_scale`] says: when its size
/// changes with the scale, it asks for the new size and takes the answer.
struct Zoom {
    context: EditorContext,
    size: HostSize,
    set_size_calls: Rc<Cell<usize>>,
}

impl OpenEditor for Zoom {
    fn size(&self) -> HostSize {
        self.size
    }
    fn fit(&self, wanted: HostSize) -> HostSize {
        wanted
    }
    fn set_size(&mut self, size: HostSize) -> bool {
        self.set_size_calls.set(self.set_size_calls.get() + 1);
        self.size = size;
        true
    }
    fn set_scale(&mut self, scale: f64) -> bool {
        let wanted = HostSize::new(200, 100).scale_by(scale);
        match self.context.request_resize(wanted) {
            Some(granted) => {
                self.size = granted;
                true
            }
            None => false,
        }
    }
    fn set_visible(&mut self, _: bool) {}
}

/// Opens a [`Zoom`] inside a [`SyncResizeWrapper`].
fn zoom_in(limit: HostSize) -> (Rc<SyncResizeWrapper>, HostLink, Rc<Cell<usize>>) {
    let wrapper = SyncResizeWrapper::new(limit);
    let (context, link) = EditorContext::attach(wrapper.clone());
    let set_size_calls = Rc::new(Cell::new(0));
    *wrapper.editor.borrow_mut() = Some(Box::new(Zoom {
        context,
        size: HostSize::new(200, 100),
        set_size_calls: Rc::clone(&set_size_calls),
    }));
    (wrapper, link, set_size_calls)
}

/// The host calls `setContentScaleFactor(2)`; the editor asks for twice its
/// size from inside that call, and the host resizes synchronously. Nothing
/// re-enters the editor, which the wrapper still holds by `&mut`, and the
/// editor ends up at the size the host set.
#[test]
fn a_scale_change_that_resizes_synchronously_reaches_the_editor() {
    let (wrapper, link, set_size_calls) = zoom_in(HostSize::new(1000, 1000));
    let changed = catch_unwind(AssertUnwindSafe(|| wrapper.set_scale(2.0)));
    let size = wrapper.editor_size();
    let requests = wrapper.requests.get();
    wrapper.editor.borrow_mut().take(); // breaks the cycle
    drop(link);

    assert!(
        matches!(changed, Ok(true)),
        "the wrapper panicked re-entering the editor"
    );
    assert_eq!(size, Some(HostSize::new(400, 200)));
    assert_eq!(requests, 1);
    assert_eq!(
        set_size_calls.get(),
        0,
        "set_size was called from inside the request"
    );
}

/// The host grants less than the editor asked for: the editor takes the
/// size the host set, still without being called back.
#[test]
fn a_host_that_sets_another_size_answers_with_it() {
    let (wrapper, link, set_size_calls) = zoom_in(HostSize::new(300, 150));
    assert!(wrapper.set_scale(2.0));
    assert_eq!(wrapper.editor_size(), Some(HostSize::new(300, 150)));
    assert_eq!(set_size_calls.get(), 0);

    // A resize the host starts itself, outside any request, reaches the
    // editor through `set_size`.
    assert!(wrapper.on_size(HostSize::new(250, 120)));
    assert_eq!(wrapper.editor_size(), Some(HostSize::new(250, 120)));
    assert_eq!(set_size_calls.get(), 1);

    wrapper.editor.borrow_mut().take();
    drop(link);
}

// --- An editor that holds gestures and timers, opened and closed. ---

/// A toy editor: on open it starts a 16 ms timer on the host's loop (if
/// there is one) and can be dragged; dropping it must leave nothing.
struct ToySpec;

struct ToyEditor {
    context: EditorContext,
    drag: Option<Gesture>,
    _timer: Option<LoopHandle>,
    scale: f64,
    size: HostSize,
}

impl EditorSpec for ToySpec {
    fn size(&self, scale: f64) -> HostSize {
        HostSize::new(200, 100).scale_by(scale)
    }

    fn resize(&self) -> Resize {
        Resize::Free {
            min: HostSize::new(100, 50),
            max: HostSize::new(400, 200),
        }
    }

    fn supports(&self, api: WindowApi) -> bool {
        api == WindowApi::Win32
    }

    fn open(
        &self,
        parent: ParentWindow,
        scale: Option<f64>,
        context: EditorContext,
    ) -> Result<Box<dyn OpenEditor>, EditorError> {
        if !self.supports(parent.api()) {
            return Err(EditorError::Unsupported(parent.api()));
        }
        let timer = context.every(16, || {});
        let scale = scale.unwrap_or(1.0);
        Ok(Box::new(ToyEditor {
            context,
            drag: None,
            _timer: timer,
            scale,
            size: self.size(scale),
        }))
    }
}

impl OpenEditor for ToyEditor {
    fn size(&self) -> HostSize {
        self.size
    }
    fn fit(&self, wanted: HostSize) -> HostSize {
        ToySpec.resize().scaled(self.scale).fit(self.size, wanted)
    }
    fn set_size(&mut self, size: HostSize) -> bool {
        let accepted = self.fit(size) == size;
        if accepted {
            self.size = size;
        }
        accepted
    }
    fn set_scale(&mut self, _: f64) -> bool {
        false
    }
    fn set_visible(&mut self, _: bool) {}

    /// D held down drags the cutoff from 0.5 to 0.75; letting go ends it.
    fn key(&mut self, event: KeyEvent) -> bool {
        if event.key != Key::Character('d') {
            return false;
        }
        if event.pressed {
            let mut drag = Gesture::begin(&self.context, CUTOFF, 0.5);
            drag.set(0.75);
            self.drag = Some(drag);
        } else {
            self.drag = None;
        }
        true
    }
}

fn key_d(pressed: bool) -> KeyEvent {
    KeyEvent {
        key: Key::Character('d'),
        modifiers: Modifiers::NONE,
        pressed,
    }
}

#[test]
fn closing_an_editor_in_the_middle_of_a_drag_ends_the_gesture() {
    let (recorder, context, link) = attached();
    let parent = ParentWindow::win32(0x10).expect("not null");

    // A whole drag.
    let mut editor = ToySpec.open(parent, None, context.clone()).expect("opens");
    assert!(editor.key(key_d(true)));
    assert!(editor.key(key_d(false)));
    assert!(!editor.key(KeyEvent {
        key: Key::Escape,
        ..key_d(true)
    }));
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.75), End(CUTOFF)]
    );

    // The host closes the editor while D is still down.
    assert!(editor.key(key_d(true)));
    assert_eq!(recorder.calls(), [Begin(CUTOFF), Perform(CUTOFF, 0.75)]);
    drop(editor);
    assert_eq!(
        recorder.take_calls(),
        [Begin(CUTOFF), Perform(CUTOFF, 0.75), End(CUTOFF)]
    );
    assert!(context.open_gestures().open_params().is_empty());
    drop(link);
    assert!(
        recorder.take_calls().is_empty(),
        "the link had nothing to end"
    );
}

#[test]
fn an_editor_refuses_a_window_it_cannot_live_in() {
    let (_, context, _link) = attached();
    let parent = ParentWindow::x11(0x0420_0007).expect("an id");
    let error = ToySpec.open(parent, None, context).err().expect("refused");
    assert_eq!(error, EditorError::Unsupported(WindowApi::X11));
}

#[test]
fn sizes_and_resizing_go_through_the_open_editor() {
    let (_, context, _link) = attached();
    let parent = ParentWindow::win32(0x10).expect("not null");
    let mut editor = ToySpec.open(parent, Some(2.0), context).expect("opens");
    assert_eq!(editor.size(), HostSize::new(400, 200));
    // The limits, 100 by 50 to 400 by 200 at scale 1, double at scale 2.
    assert_eq!(editor.fit(HostSize::new(1, 999)), HostSize::new(200, 400));
    assert!(!editor.set_size(HostSize::new(1, 1)));
    assert_eq!(editor.size(), HostSize::new(400, 200));
    assert!(editor.set_size(HostSize::new(300, 150)));
    assert_eq!(editor.size(), HostSize::new(300, 150));
}

#[test]
fn timers_an_editor_registers_die_with_it() {
    let host_loop = Rc::new(ManualLoop::new());
    let recorder = Rc::new(RecordingEditorHost::new().with_loop(host_loop.clone()));
    let parent = ParentWindow::win32(0x10).expect("not null");
    let cycles = if cfg!(miri) { 5 } else { 100 };
    for cycle in 1..=cycles {
        let (context, link) = EditorContext::attach(recorder.clone());
        let editor = ToySpec.open(parent, None, context).expect("opens");
        assert_eq!(host_loop.timers(), [16]);
        assert_eq!(host_loop.fire_timers(), 1);
        drop(editor);
        assert!(host_loop.timers().is_empty());
        assert_eq!(host_loop.fire_timers(), 0);
        drop(link);
        assert_eq!(host_loop.cancels(), cycle, "one cancel per editor");
    }
    assert_eq!(Rc::strong_count(&recorder), 1);
}

// --- The test doubles themselves. ---

#[test]
fn the_recording_host_spots_calls_a_format_would_reject() {
    let recorder = RecordingEditorHost::new();
    recorder.perform(CUTOFF, 0.5);
    assert_eq!(
        recorder.well_formed(),
        Err(format!(
            "call 0: {:?} with no gesture open on that parameter",
            Perform(CUTOFF, 0.5)
        ))
    );

    let recorder = RecordingEditorHost::new();
    recorder.begin_gesture(CUTOFF);
    recorder.begin_gesture(DEPTH);
    recorder.begin_gesture(CUTOFF);
    assert!(
        recorder
            .well_formed()
            .is_err_and(|error| error.starts_with("call 2: BeginGesture"))
    );

    let recorder = RecordingEditorHost::new();
    recorder.end_gesture(DEPTH);
    assert!(
        recorder
            .well_formed()
            .is_err_and(|error| error.starts_with("call 0: EndGesture"))
    );

    // A gesture left open is not an error in itself, but it shows.
    let recorder = RecordingEditorHost::new();
    recorder.begin_gesture(DEPTH);
    recorder.values_changed();
    recorder.closed();
    assert_eq!(recorder.well_formed(), Ok(()));
    assert_eq!(recorder.open_at_host(), [DEPTH]);
    assert_eq!(
        recorder.take_calls(),
        [Begin(DEPTH), HostCall::ValuesChanged, HostCall::Closed]
    );
    assert!(recorder.calls().is_empty());
}

/// Values a format rejects, sent by something that calls a host directly:
/// not a number, and outside `0..=1`.
#[test]
fn the_recorder_rejects_values_a_format_rejects() {
    for (bad, call) in [(f64::NAN, 1), (7.0, 2), (-0.5, 1), (f64::INFINITY, 1)] {
        let recorder = RecordingEditorHost::new();
        recorder.begin_gesture(CUTOFF);
        if call == 2 {
            recorder.perform(CUTOFF, 1.0);
        }
        recorder.perform(CUTOFF, bad);
        recorder.end_gesture(CUTOFF);
        let error = recorder.well_formed().expect_err("a bad value");
        assert!(
            error.starts_with(&format!("call {call}: Perform"))
                && error.ends_with("with a value outside 0..=1"),
            "{error}"
        );
    }
    // The ends of the range, and a negative zero, are fine.
    let recorder = RecordingEditorHost::new();
    recorder.begin_gesture(CUTOFF);
    for good in [0.0, -0.0, 1.0, 0.5] {
        recorder.perform(CUTOFF, good);
    }
    recorder.end_gesture(CUTOFF);
    assert_eq!(recorder.well_formed(), Ok(()));
}

#[test]
fn the_recorder_answers_resizes_as_it_is_told() {
    let (recorder, context, _link) = attached();
    let wanted = HostSize::new(500, 300);
    assert_eq!(context.request_resize(wanted), Some(wanted));
    recorder.set_resize_answer(|_| None);
    assert_eq!(context.request_resize(wanted), None);
    recorder.set_resize_answer(|size| Some(HostSize::new(size.width / 2, size.height)));
    assert_eq!(
        context.request_resize(wanted),
        Some(HostSize::new(250, 300))
    );
    assert_eq!(recorder.take_calls(), [HostCall::RequestResize(wanted); 3]);
}

#[test]
fn the_manual_loop_lets_callbacks_cancel_and_register() {
    let host_loop = Rc::new(ManualLoop::new());
    let log = Rc::new(RefCell::new(Vec::new()));

    // A timer that cancels itself on its second tick.
    let own_token = Rc::new(Cell::new(None));
    let token = {
        // Weak, so that the loop and its callbacks do not keep each other
        // alive.
        let (host_loop_in, log, own_token) = (
            Rc::downgrade(&host_loop),
            Rc::clone(&log),
            Rc::clone(&own_token),
        );
        let mut ticks = 0;
        host_loop.every(
            10,
            Box::new(move || {
                ticks += 1;
                log.borrow_mut()
                    .push(format!("self-cancelling tick {ticks}"));
                if let (2, Some(host_loop)) = (ticks, host_loop_in.upgrade()) {
                    host_loop.cancel(own_token.get().expect("set"));
                }
            }),
        )
    };
    own_token.set(token);

    // A descriptor watch that registers a timer the first time it fires.
    {
        let (host_loop_in, log) = (Rc::downgrade(&host_loop), Rc::clone(&log));
        host_loop.watch_fd(
            5,
            Box::new(move || {
                log.borrow_mut().push("fd 5 readable".to_owned());
                let Some(host_loop) = host_loop_in.upgrade() else {
                    return;
                };
                if host_loop.timers().len() < 2 {
                    let log = Rc::clone(&log);
                    host_loop.every(
                        20,
                        Box::new(move || log.borrow_mut().push("late timer".to_owned())),
                    );
                }
            }),
        );
    }

    assert_eq!(host_loop.fire_fd(4), 0);
    assert_eq!(host_loop.fire_fd(5), 1);
    assert_eq!(host_loop.timers(), [10, 20]);
    assert_eq!(host_loop.fire_timers(), 2);
    assert_eq!(host_loop.fire_timers(), 2); // the first cancels itself
    assert_eq!(host_loop.timers(), [20]);
    assert_eq!(host_loop.fire_timers(), 1);
    assert_eq!(host_loop.watched_fds(), [5]);

    // Cancelling twice, or a token never given, does nothing.
    host_loop.cancel(token.expect("registered"));
    host_loop.cancel(LoopToken::new(999));

    host_loop.set_refusing(true);
    assert_eq!(host_loop.every(1, Box::new(|| {})), None);
    assert_eq!(host_loop.watch_fd(1, Box::new(|| {})), None);

    assert_eq!(
        *log.borrow(),
        [
            "fd 5 readable",
            "self-cancelling tick 1",
            "late timer",
            "self-cancelling tick 2",
            "late timer",
            "late timer",
        ]
    );
    // Only the loop's own references to `log` remain: the self-cancelled
    // timer's closure has been dropped.
    assert_eq!(Rc::strong_count(&log), 3);
}

/// Wakes are recorded where they happen on the reading thread, and from
/// another thread no later than the next call or read.
#[test]
fn the_recorder_records_wakes_in_order() {
    let (recorder, context, _link) = attached();
    let waker = context.main_thread_waker().expect("offered");
    Gesture::set_once(&context, MIX, 0.5);
    assert!(waker.wake());
    context.values_changed();
    let remote = waker.clone();
    std::thread::spawn(move || remote.wake())
        .join()
        .expect("the thread finished");
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(MIX),
            Perform(MIX, 0.5),
            End(MIX),
            HostCall::Wake,
            HostCall::ValuesChanged,
            HostCall::Wake,
        ]
    );
}

// --- Plugins with and without an editor, in the harness. ---

struct EchoParams {
    level: FloatParam,
}
param_set!(EchoParams { level });

thread_local! {
    /// How many times `editor` was called on this thread.
    static EDITOR_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Records the value of every parameter event it is given.
struct Echo {
    params: Arc<EchoParams>,
    heard: Vec<f64>,
}

impl Plugin for Echo {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.echo",
        name: "Echo",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Remembers parameter events",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = EchoParams;

    fn new() -> Self {
        let level = FloatParam::new("level", "Level", FloatRange::linear(0.0, 10.0), 5.0);
        Self {
            params: Arc::new(EchoParams { level }),
            heard: Vec::new(),
        }
    }

    fn params(&self) -> Arc<EchoParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, _: &mut AudioBuffer<f32>, context: &ProcessContext<'_, f32>) {
        for event in context.events {
            if let EventKind::ParamValue { value, .. } = event.kind {
                self.heard.push(value);
            }
        }
    }

    fn editor(&self) -> Option<Box<dyn EditorSpec>> {
        EDITOR_CALLS.with(|calls| calls.set(calls.get() + 1));
        Some(Box::new(ToySpec))
    }
}

/// A plugin with no editor: nothing to write.
struct Plain {
    params: Arc<EchoParams>,
}

impl Plugin for Plain {
    const INFO: PluginInfo = PluginInfo {
        id: "org.manguebyte.test.plain",
        name: "Plain",
        vendor: "Manguebyte tests",
        version: "0.1.0",
        description: "Has no editor",
        kind: PluginKind::Effect,
    };
    const LAYOUT: AudioLayout = AudioLayout::STEREO_EFFECT;
    type Params = EchoParams;

    fn new() -> Self {
        let level = FloatParam::new("level", "Level", FloatRange::linear(0.0, 10.0), 5.0);
        Self {
            params: Arc::new(EchoParams { level }),
        }
    }

    fn params(&self) -> Arc<EchoParams> {
        Arc::clone(&self.params)
    }

    fn prepare(&mut self, _: &ProcessSpec) {}

    fn process(&mut self, _: &mut AudioBuffer<f32>, _: &ProcessContext<'_, f32>) {}
}

#[test]
fn a_plugin_without_an_editor_has_none() {
    assert!(Plain::new().editor().is_none());
    assert!(Harness::<Plain>::new(48_000.0, 64).editor().is_none());
}

#[test]
fn the_harness_asks_for_the_editor_once_and_the_plugin_hears_its_edits() {
    EDITOR_CALLS.with(|calls| calls.set(0));
    let mut harness = Harness::<Echo>::new(48_000.0, 64);
    assert_eq!(EDITOR_CALLS.with(Cell::get), 1);

    let params: Arc<EchoParams> = harness.plugin_mut().params();
    let spec = harness.editor().expect("Echo has an editor");
    assert_eq!(spec.size(1.0), HostSize::new(200, 100));
    assert_eq!(EDITOR_CALLS.with(Cell::get), 1, "asked once, kept after");

    // The host stores values into the parameters, as a standalone program
    // does; the plugin then hears each change as an event on its next
    // block.
    let recorder = Rc::new(RecordingEditorHost::new().with_params(params.clone()));
    let (context, link) = EditorContext::attach(recorder.clone());
    let parent = ParentWindow::win32(0x10).expect("not null");
    let editor = spec.open(parent, None, context.clone()).expect("opens");

    let level = params.level.param_id();
    let mut drag = Gesture::begin(&context, level, params.level.normalized());
    drag.set(0.8);
    harness.process(&AudioBuffer::new(2, 64));
    drag.set(0.2);
    harness.process(&AudioBuffer::new(2, 64));
    drag.cancel();
    harness.process(&AudioBuffer::new(2, 64));
    drop(editor);
    drop(link);

    assert_eq!(harness.plugin_mut().heard, [8.0, 2.0, 5.0]);
    assert_eq!(params.level.value(), 5.0);
    assert_eq!(
        recorder.take_calls(),
        [
            Begin(level),
            Perform(level, 0.8),
            Perform(level, 0.2),
            Perform(level, 0.5),
            End(level),
        ]
    );
}

/// A small deterministic generator (xorshift64*), so that failures repeat.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, limit: usize) -> usize {
        (self.next() % limit as u64) as usize
    }

    /// A value for a gesture, hostile one time in three.
    fn value(&mut self) -> f64 {
        match self.below(12) {
            0 => f64::NAN,
            1 => f64::INFINITY,
            2 => f64::NEG_INFINITY,
            3 => -0.0,
            4 => 1.5,
            5 => 0.5,
            _ => (self.next() >> 11) as f64 / (1_u64 << 53) as f64,
        }
    }
}
