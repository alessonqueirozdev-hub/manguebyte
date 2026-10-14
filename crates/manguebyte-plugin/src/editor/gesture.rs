//! Edits made by a person: begin, set, end, and cancel.

use std::cell::Cell;
use std::fmt;

use manguebyte_core::ParamId;

use super::EditorContext;

/// A person changing one parameter, from the moment they take hold of a
/// control until they let go.
///
/// Creating a `Gesture` tells the host the edit has begun
/// ([`EditorHost::begin_gesture`](super::EditorHost::begin_gesture));
/// [`set`](Self::set) sends values
/// ([`EditorHost::perform`](super::EditorHost::perform)); dropping it tells
/// the host the edit is over
/// ([`EditorHost::end_gesture`](super::EditorHost::end_gesture)). The end
/// cannot happen twice, and it happens on every path that drops the
/// gesture: a control removed in the middle of a drag, an editor closed
/// while the mouse button is down, a panic in widget code, and a host that
/// panics while it is told about the edit. [`cancel`](Self::cancel) ends
/// the gesture after putting the starting value back.
///
/// A gesture that is never dropped, because it was passed to
/// [`std::mem::forget`] or sits in a cycle of `Rc`s, is ended when the
/// wrapper drops the editor's [`HostLink`](super::HostLink); after that it
/// sends nothing.
///
/// Gestures on one parameter that overlap, such as a drag and a
/// screen-reader action at the same time, reach the host as one: the first
/// to begin sends the begin, the last to end sends the end, and values from
/// either are sent in between. A value is not sent again if it is the last
/// one sent on that parameter, by whichever gesture.
///
/// A gesture holds an [`EditorContext`], so it stays on the thread that
/// made it, which is the host's main thread.
///
/// ```
/// use std::rc::Rc;
/// use manguebyte_plugin::editor::testing::{HostCall::*, RecordingEditorHost};
/// use manguebyte_plugin::editor::{EditorContext, Gesture};
/// use manguebyte_params::ParamId;
///
/// const CUTOFF: ParamId = ParamId::from_name("cutoff");
/// let recorder = Rc::new(RecordingEditorHost::new());
/// let (context, _link) = EditorContext::attach(recorder.clone());
///
/// // Pointer down on a knob showing 0.25, two moves, pointer up.
/// let mut drag = Gesture::begin(&context, CUTOFF, 0.25);
/// drag.set(0.5);
/// drag.set(0.75);
/// drop(drag);
///
/// // Escape pressed during a second drag.
/// let mut drag = Gesture::begin(&context, CUTOFF, 0.75);
/// drag.set(0.9);
/// drag.cancel();
///
/// assert_eq!(recorder.take_calls(), [
///     BeginGesture(CUTOFF), Perform(CUTOFF, 0.5), Perform(CUTOFF, 0.75), EndGesture(CUTOFF),
///     BeginGesture(CUTOFF), Perform(CUTOFF, 0.9), Perform(CUTOFF, 0.75), EndGesture(CUTOFF),
/// ]);
/// ```
///
/// A gesture that is not kept would end at once, which is never what was
/// meant, so the compiler warns about it (an error here, where warnings
/// are denied):
///
/// ```compile_fail
/// #![deny(unused_must_use)]
/// # use std::rc::Rc;
/// # use manguebyte_plugin::editor::testing::RecordingEditorHost;
/// # use manguebyte_plugin::editor::{EditorContext, Gesture};
/// # use manguebyte_params::ParamId;
/// let (context, _link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
/// Gesture::begin(&context, ParamId::from_name("cutoff"), 0.25);
/// ```
#[must_use = "the gesture ends as soon as this value is dropped"]
pub struct Gesture {
    context: EditorContext,
    param: ParamId,
    /// The value to put back on cancel, if it was a number.
    start: Option<f64>,
    /// The table's epoch when the gesture began. A different epoch means
    /// the link has ended the gesture already.
    epoch: u64,
    /// The parameter's send count when the gesture began.
    since: u64,
    /// Whether the gesture still has its end to deal with.
    live: bool,
}

impl Gesture {
    /// Begins a gesture on `param`, whose normalized value is `start` when
    /// it begins.
    ///
    /// `start` is what [`cancel`](Self::cancel) puts back. If it is not a
    /// number there is nothing to put back, and cancelling only ends the
    /// gesture. On a context that is detached the gesture sends nothing.
    pub fn begin(context: &EditorContext, param: ParamId, start: f64) -> Self {
        let gestures = context.open_gestures();
        let mut gesture = Self {
            context: context.clone(),
            param,
            start: (!start.is_nan()).then(|| normalized(start)),
            epoch: gestures.epoch(),
            since: 0,
            live: false,
        };
        let Some(host) = context.live_host() else {
            return gesture;
        };
        let (first, since) = gestures.enter(param);
        gesture.since = since;
        // Live before the host is called, so that if the host panics the
        // gesture is still ended as it unwinds.
        gesture.live = true;
        if first {
            host.begin_gesture(param);
        }
        gesture
    }

    /// A whole edit in one call, for a click on a switch, a reset to the
    /// default or a typed value: begin, send `value`, end.
    ///
    /// A value that is not a number is ignored, and nothing is sent.
    pub fn set_once(context: &EditorContext, param: ParamId, value: f64) {
        if !value.is_nan() {
            let mut gesture = Self::begin(context, param, f64::NAN);
            gesture.set(value);
        }
    }

    /// Sends a new normalized value for the parameter.
    ///
    /// The value is clamped to `0..=1`. A value that is not a number is
    /// ignored, and so is the value last sent on this parameter by any
    /// gesture, so a control held against its end does not flood the host.
    pub fn set(&mut self, value: f64) {
        if value.is_nan() || !self.is_current() {
            return;
        }
        let Some(host) = self.context.live_host() else {
            return;
        };
        let value = normalized(value);
        if self.context.open_gestures().send(self.param, value) {
            host.perform(self.param, value);
        }
    }

    /// Ends the gesture. The same as dropping it, written out.
    pub fn end(mut self) {
        self.finish(false);
    }

    /// Ends the gesture after putting back the value the parameter had when
    /// it began, as when Escape is pressed during a drag.
    ///
    /// The starting value is sent only if it is a number, if some value has
    /// been sent on the parameter since this gesture began, by it or by a
    /// gesture overlapping it, and if the last value sent is not already
    /// the starting value. The end is sent even if the host panics while it
    /// is sent the starting value.
    pub fn cancel(mut self) {
        self.finish(true);
    }

    /// The parameter being changed.
    pub fn param(&self) -> ParamId {
        self.param
    }

    /// The normalized value when the gesture began, clamped to `0..=1`, or
    /// `None` if none was given.
    pub fn start(&self) -> Option<f64> {
        self.start
    }

    /// The value the host was last sent for this parameter since this
    /// gesture began, whether this gesture sent it or one overlapping it
    /// did; `None` if nothing has been sent since, or if the gesture no
    /// longer reaches the host.
    ///
    /// While the gesture is open a control shows this, falling back to
    /// [`start`](Self::start), rather than the parameter's own value, which
    /// the host may echo back late.
    pub fn sent(&self) -> Option<f64> {
        if !self.is_current() {
            return None;
        }
        self.context
            .open_gestures()
            .sent_since(self.param, self.since)
    }

    /// Whether the gesture still reaches the host: false once the editor's
    /// link has been dropped, or if the gesture began on a detached
    /// context.
    pub fn is_live(&self) -> bool {
        self.is_current() && self.context.is_attached()
    }

    fn is_current(&self) -> bool {
        self.live && self.context.open_gestures().epoch() == self.epoch
    }

    fn finish(&mut self, restore: bool) {
        if !std::mem::replace(&mut self.live, false) {
            return;
        }
        if self.context.open_gestures().epoch() != self.epoch {
            // The link has ended this gesture already.
            return;
        }
        // The end is sent by this guard, so that it is sent even if the
        // host panics while it is sent the starting value.
        let _end = EndOnDrop {
            context: &self.context,
            param: self.param,
            epoch: self.epoch,
        };
        let (true, Some(start)) = (restore, self.start) else {
            return;
        };
        if let Some(host) = self.context.live_host() {
            if self
                .context
                .open_gestures()
                .restore(self.param, start, self.since)
            {
                host.perform(self.param, start);
            }
        }
    }
}

impl Drop for Gesture {
    fn drop(&mut self) {
        self.finish(false);
    }
}

impl fmt::Debug for Gesture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gesture")
            .field("param", &self.param)
            .field("start", &self.start)
            .field("sent", &self.sent())
            .field("live", &self.is_live())
            .finish_non_exhaustive()
    }
}

/// Leaves the table, and sends the end if this was the last gesture open on
/// the parameter, when dropped.
struct EndOnDrop<'a> {
    context: &'a EditorContext,
    param: ParamId,
    epoch: u64,
}

impl Drop for EndOnDrop<'_> {
    fn drop(&mut self) {
        let gestures = self.context.open_gestures();
        if gestures.epoch() == self.epoch && gestures.leave(self.param) {
            if let Some(host) = self.context.host_for_ends() {
                host.end_gesture(self.param);
            }
        }
    }
}

/// Clamps a value that is a number to `0..=1`, with no negative zero.
fn normalized(value: f64) -> f64 {
    if value > 1.0 {
        1.0
    } else if value > 0.0 {
        value
    } else {
        0.0
    }
}

/// The gestures open on each parameter of one [`EditorContext`], as
/// [`EditorContext::open_gestures`] shows them.
///
/// [`Gesture`]s use it to send one begin and one end however many of them
/// overlap on a parameter, and to send a value only if it is not the last
/// one sent on that parameter. It is never borrowed while the host is
/// called, so a host may start or end gestures from inside its own methods.
pub struct OpenGestures {
    /// Parameters with at least one open gesture, in the order they were
    /// first opened.
    entries: Cell<Vec<Entry>>,
    /// Bumped by [`close_all`](Self::close_all); gestures begun before
    /// then no longer touch the table.
    epoch: Cell<u64>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    param: ParamId,
    /// How many gestures are open on the parameter.
    depth: u32,
    /// The last value sent on the parameter while it has been open.
    last: Option<f64>,
    /// How many values have been sent on the parameter while it has been
    /// open.
    sends: u64,
}

impl OpenGestures {
    pub(super) fn new() -> Self {
        Self {
            entries: Cell::new(Vec::new()),
            epoch: Cell::new(0),
        }
    }

    /// How many gestures are open on `param`.
    pub fn depth(&self, param: ParamId) -> u32 {
        self.with(|entries| find(entries, param).map_or(0, |entry| entry.depth))
    }

    /// Whether any gesture is open on `param`.
    pub fn is_open(&self, param: ParamId) -> bool {
        self.depth(param) > 0
    }

    /// The parameters with an open gesture, in the order they were first
    /// opened.
    pub fn open_params(&self) -> Vec<ParamId> {
        self.with(|entries| entries.iter().map(|entry| entry.param).collect())
    }

    pub(super) fn epoch(&self) -> u64 {
        self.epoch.get()
    }

    /// Records a gesture opening on `param`. Returns whether it is the only
    /// one, and the parameter's send count.
    pub(super) fn enter(&self, param: ParamId) -> (bool, u64) {
        self.with(|entries| match find_mut(entries, param) {
            Some(entry) => {
                entry.depth = entry.depth.saturating_add(1);
                (false, entry.sends)
            }
            None => {
                entries.push(Entry {
                    param,
                    depth: 1,
                    last: None,
                    sends: 0,
                });
                (true, 0)
            }
        })
    }

    /// Records `value` as sent on `param`, if a gesture is open on it and
    /// `value` is not the last value sent. True if it is to be sent.
    pub(super) fn send(&self, param: ParamId, value: f64) -> bool {
        self.with(|entries| match find_mut(entries, param) {
            Some(entry) if entry.last != Some(value) => {
                entry.last = Some(value);
                entry.sends = entry.sends.wrapping_add(1);
                true
            }
            _ => false,
        })
    }

    /// Records `start` as sent on `param`, if something has been sent since
    /// the send count was `since` and the last value is not `start`. True
    /// if it is to be sent.
    pub(super) fn restore(&self, param: ParamId, start: f64, since: u64) -> bool {
        self.with(|entries| match find_mut(entries, param) {
            Some(entry) if entry.sends != since && entry.last != Some(start) => {
                entry.last = Some(start);
                entry.sends = entry.sends.wrapping_add(1);
                true
            }
            _ => false,
        })
    }

    /// The last value sent on `param`, if any was sent since the send count
    /// was `since`.
    pub(super) fn sent_since(&self, param: ParamId, since: u64) -> Option<f64> {
        self.with(|entries| {
            find(entries, param)
                .filter(|entry| entry.sends != since)
                .and_then(|entry| entry.last)
        })
    }

    /// Records a gesture on `param` ending. True if it was the last one.
    pub(super) fn leave(&self, param: ParamId) -> bool {
        self.with(|entries| {
            let Some(index) = entries.iter().position(|entry| entry.param == param) else {
                return false;
            };
            let entry = &mut entries[index];
            entry.depth = entry.depth.saturating_sub(1);
            if entry.depth == 0 {
                entries.remove(index);
                true
            } else {
                false
            }
        })
    }

    /// Forgets every open gesture and returns their parameters, in the
    /// order they were first opened. Gestures begun before this no longer
    /// touch the table.
    pub(super) fn close_all(&self) -> Vec<ParamId> {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.with(|entries| entries.drain(..).map(|entry| entry.param).collect())
    }

    /// Runs `work` on the table. The table is taken out of its cell for the
    /// duration and put back afterwards, even if `work` panics; `work`
    /// never calls out of this module.
    fn with<R>(&self, work: impl FnOnce(&mut Vec<Entry>) -> R) -> R {
        let mut table = PutBack {
            cell: &self.entries,
            entries: self.entries.take(),
        };
        work(&mut table.entries)
    }
}

/// Puts the table back into its cell when dropped.
struct PutBack<'a> {
    cell: &'a Cell<Vec<Entry>>,
    entries: Vec<Entry>,
}

impl Drop for PutBack<'_> {
    fn drop(&mut self) {
        self.cell.set(std::mem::take(&mut self.entries));
    }
}

fn find(entries: &[Entry], param: ParamId) -> Option<&Entry> {
    entries.iter().find(|entry| entry.param == param)
}

fn find_mut(entries: &mut [Entry], param: ParamId) -> Option<&mut Entry> {
    entries.iter_mut().find(|entry| entry.param == param)
}

impl fmt::Debug for OpenGestures {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Copied out first: the formatter may panic, and must not do so
        // while the table is out of its cell.
        let entries = self.with(|entries| entries.clone());
        f.debug_map()
            .entries(entries.iter().map(|entry| (entry.param, entry.depth)))
            .finish()
    }
}
