//! Files and text dragged from other programs, run as drags of the
//! interface's own with no source widget (see `drag.rs`).
//!
//! A window layer reports the system's drag to [`Root::file_drop`]: when it
//! comes in, the root starts a drag whose payload holds the [`DropData`],
//! with no ghost, since the system draws the drag's own image; moves offer
//! it to the widget under the pointer and those containing it, through
//! [`Widget::drag`](crate::Widget::drag), exactly as a drag inside the
//! interface is offered, and the frame clock scrolls a view whose edge
//! the pointer is near; a drop drops it on the widget that accepts it. The
//! answer tells the system whether to show that the drag would be taken.

use manguebyte_gfx::Point;

use super::Root;
use crate::drag::{DragOutcome, DragPayload};
use crate::event::{DropData, FileDrop, Handled};

impl Root {
    /// Runs a drag from another program over the interface, and returns
    /// whether a widget takes it where the pointer is (for a drop: whether
    /// the widget took it).
    ///
    /// - [`FileDrop::Enter`] or [`FileDrop::Over`] (a window layer may send
    ///   either for a move) starts the drag, with a payload holding a copy
    ///   of `data` and described by [`DropData::description`], if none is
    ///   running, and offers it at the pointer. A drag that carries nothing
    ///   is refused. A drag inside the interface that is running already
    ///   keeps going, and the one from outside is refused.
    /// - [`FileDrop::Drop`] offers it at the drop's position and drops it
    ///   on the widget that accepts it there.
    /// - [`FileDrop::Leave`] cancels it: the target hears it left.
    ///
    /// As for any drag, menus close and the tooltip goes when it starts.
    /// Positions that are not numbers cancel it. Nothing is drawn until the
    /// next frame.
    ///
    /// ```
    /// use std::rc::Rc;
    /// use manguebyte_gfx::{Point, Scene, Size};
    /// use manguebyte_plugin::editor::EditorContext;
    /// use manguebyte_plugin::editor::testing::RecordingEditorHost;
    /// use manguebyte_ui::access::{AccessNode, Role};
    /// use manguebyte_ui::{
    ///     AccessCx, DragEvent, DropData, EventCx, FileDrop, Handled, MeasureCx, PaintCx, Root,
    ///     Widget,
    /// };
    ///
    /// /// Takes drags that carry a `.wav` file, and counts what was dropped.
    /// struct Pad { dropped: usize }
    ///
    /// impl Widget for Pad {
    ///     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
    ///         AccessNode::new(Role::Group, "Pad")
    ///     }
    ///     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
    ///         Size::new(50.0, 50.0)
    ///     }
    ///     fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
    ///     fn drag(&mut self, _cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
    ///         let samples = |payload: &manguebyte_ui::DragPayload| {
    ///             payload
    ///                 .get::<DropData>()
    ///                 .filter(|data| data.has_file_with_extension(&["wav"]))
    ///                 .map(|data| data.files.len())
    ///         };
    ///         match event {
    ///             DragEvent::Over { payload, .. } if samples(payload).is_some() => Handled::Yes,
    ///             DragEvent::Drop { payload, .. } => match samples(payload) {
    ///                 Some(count) => {
    ///                     self.dropped += count;
    ///                     Handled::Yes
    ///                 }
    ///                 None => Handled::No,
    ///             },
    ///             _ => Handled::No,
    ///         }
    ///     }
    /// }
    ///
    /// let (context, _link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
    /// let mut root = Root::new(Pad { dropped: 0 }, Size::new(50.0, 50.0), 1.0, context);
    /// root.frame(std::time::Duration::ZERO);
    /// let at = Point::new(10.0, 10.0);
    /// let samples = DropData::files(["kick.wav"]);
    /// assert_eq!(root.file_drop(FileDrop::Enter { at }, &samples), Handled::Yes);
    /// assert_eq!(root.dragging().map(|payload| payload.description()), Some("kick.wav"));
    /// assert_eq!(root.file_drop(FileDrop::Drop { at }, &samples), Handled::Yes);
    /// assert!(root.dragging().is_none());
    /// let text = DropData::text("not a sample");
    /// assert_eq!(root.file_drop(FileDrop::Enter { at }, &text), Handled::No);
    /// root.file_drop(FileDrop::Leave, &text);
    /// ```
    pub fn file_drop(&mut self, drop: FileDrop, data: &DropData) -> Handled {
        match drop {
            FileDrop::Enter { at } | FileDrop::Over { at } => self.offer_file_drag(at, data),
            FileDrop::Leave => {
                if self.external_drag() {
                    self.finish_drag(None);
                }
                Handled::No
            }
            FileDrop::Drop { at } => {
                if !at.is_finite() {
                    return self.file_drop(FileDrop::Leave, data);
                }
                match &self.drag {
                    // A drag inside the interface has the pointer.
                    Some(drag) if !drag.external => return Handled::No,
                    Some(_) => {}
                    // Dropped with no move before it.
                    None if data.is_empty() => return Handled::No,
                    None => self.start_file_drag(at, data),
                }
                // Offered once more where it is dropped, then dropped.
                match self.finish_drag(Some(at)) {
                    Some(DragOutcome::Dropped) => Handled::Yes,
                    _ => Handled::No,
                }
            }
        }
    }

    /// Whether the drag running is one from outside the window.
    fn external_drag(&self) -> bool {
        self.drag.as_ref().is_some_and(|drag| drag.external)
    }

    /// Starts a drag of `data` from outside at `at`, which offers it there.
    fn start_file_drag(&mut self, at: Point, data: &DropData) {
        let payload = DragPayload::new(data.clone(), data.description());
        self.begin_drag_at(None, payload, at, true);
    }

    /// Starts the drag from outside if it is not running, and offers it at
    /// `at`.
    fn offer_file_drag(&mut self, at: Point, data: &DropData) -> Handled {
        if !at.is_finite() {
            self.file_drop(FileDrop::Leave, data);
            return Handled::No;
        }
        match &mut self.drag {
            // A drag inside the interface has the pointer.
            Some(drag) if !drag.external => return Handled::No,
            Some(drag) => {
                drag.at = at;
                self.offer_drag(at);
            }
            None => {
                if data.is_empty() {
                    return Handled::No;
                }
                self.start_file_drag(at, data);
            }
        }
        if self.drop_target().is_some() {
            Handled::Yes
        } else {
            Handled::No
        }
    }
}
