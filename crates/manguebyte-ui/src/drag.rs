//! Drag and drop inside the interface.
//!
//! A widget starts a drag with [`EventCx::start_drag`] while a button is
//! held, handing over a [`DragPayload`]: data of a type the application
//! defines, and a description for people and for assistive technology.
//! From then on the [`Root`](crate::Root) runs the drag, and widgets take
//! part through [`Widget::drag`]:
//!
//! - Each move, and each frame, the widget under the pointer is offered
//!   [`DragEvent::Over`], then the widgets containing it, until one
//!   returns [`Handled::Yes`]: that one accepts the payload at that point
//!   and is the drop target. A target shows that it accepts (an insertion
//!   line, a highlight) from its own state. Offers that are refused cost
//!   nothing.
//! - The target hears [`DragEvent::Left`] when the pointer goes off it, or
//!   another widget accepts, or the drag ends without a drop on it.
//! - Releasing the button over a target delivers [`DragEvent::Drop`] to
//!   it; it returns [`Handled::Yes`] if it took the payload.
//! - Releasing anywhere else, Escape, a second button, the system taking
//!   the pointer away, or the window losing the keyboard, cancels.
//! - The widget that started the drag hears [`DragEvent::Ended`] last,
//!   with how it ended.
//!
//! While a drag runs the root draws a ghost under the pointer, over
//! everything, with the payload's description (it is a
//! [`Role::DragItem`](crate::Role::DragItem) to assistive technology),
//! and scrolls a [`scroll_view`](crate::scroll_view) whose edge the pointer
//! is near. Nothing is sent anywhere outside the interface: what a drop
//! means is the target's, and the application's, business.
//!
//! The target side does not care where a payload came from, so the same
//! [`DragEvent::Over`] and [`DragEvent::Drop`] carry files and text
//! dragged from other programs too, as payloads holding a
//! [`DropData`](crate::DropData), with no source widget and no ghost
//! ([`Root::file_drop`](crate::Root::file_drop); ADR 0009 and ADR 0010).
//!
//! ```
//! use manguebyte_gfx::{Point, Scene, Size};
//! use manguebyte_ui::access::{AccessNode, Role};
//! use manguebyte_ui::drag::{DragEvent, DragOutcome, DragPayload};
//! use manguebyte_ui::testing::Harness;
//! use manguebyte_ui::{
//!     AccessCx, Event, EventCx, Handled, IntoView, MeasureCx, PaintCx, State, Widget, row,
//! };
//!
//! /// What the application drags: a colour by name.
//! struct Swatch(&'static str);
//!
//! /// A square that can be dragged.
//! struct Source(&'static str);
//! impl Widget for Source {
//!     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
//!         AccessNode::new(Role::Image, self.0)
//!     }
//!     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
//!         Size::new(40.0, 40.0)
//!     }
//!     fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
//!     fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
//!         match event {
//!             Event::PointerDown { .. } => {
//!                 cx.capture_pointer();
//!                 Handled::Yes
//!             }
//!             Event::PointerMove { .. } if cx.has_pointer() => {
//!                 cx.start_drag(DragPayload::new(Swatch(self.0), self.0));
//!                 Handled::Yes
//!             }
//!             _ => Handled::No,
//!         }
//!     }
//! }
//!
//! /// A well that takes swatches and remembers the last one.
//! struct Well(State<&'static str>);
//! impl Widget for Well {
//!     fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
//!         AccessNode::new(Role::Image, "Well").with_value(self.0.get())
//!     }
//!     fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
//!         Size::new(40.0, 40.0)
//!     }
//!     fn paint(&mut self, _cx: &PaintCx<'_>, _scene: &mut Scene) {}
//!     fn drag(&mut self, _cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
//!         match event {
//!             DragEvent::Over { payload, .. } if payload.get::<Swatch>().is_some() => Handled::Yes,
//!             DragEvent::Drop { payload, .. } => match payload.get::<Swatch>() {
//!                 Some(Swatch(name)) => {
//!                     self.0.set(name);
//!                     Handled::Yes
//!                 }
//!                 None => Handled::No,
//!             },
//!             _ => Handled::No,
//!         }
//!     }
//! }
//!
//! let well = State::new("");
//! let view = row((Source("Teal"), Well(well.clone()))).spacing(20.0);
//! let mut ui = Harness::new(view, Size::new(100.0, 40.0), 1.0);
//! ui.frame();
//! let teal = ui.get(Role::Image, "Teal");
//! let target = ui.get(Role::Image, "Well");
//! ui.press(teal.center());
//! ui.move_to(Point::new(30.0, 20.0));
//! ui.frame();
//! // The ghost shows what is being dragged.
//! assert!(ui.find(Role::DragItem, "Teal").is_some());
//! ui.move_to(target.center());
//! ui.release(target.center());
//! ui.frame();
//! assert_eq!(well.get(), "Teal");
//! assert!(ui.find(Role::DragItem, "Teal").is_none());
//! ```
//!
//! [`EventCx::start_drag`]: crate::EventCx::start_drag
//! [`Widget::drag`]: crate::Widget::drag
//! [`Handled::Yes`]: crate::Handled::Yes

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use manguebyte_gfx::Point;

/// What is being dragged: data of a type the application defines, shared
/// cheaply, and a description of it.
///
/// A target asks for the type it takes with [`get`](DragPayload::get) and
/// refuses the drag if it is not there, so one interface can drag several
/// kinds of things without any of them knowing the others. The data is
/// `Send + Sync` so that a payload can be kept in a [`State`](crate::State)
/// or a [`ListState`](crate::ListState), which an editor spec may keep
/// across editor openings; it is only ever read on the interface's
/// thread.
///
/// ```
/// use manguebyte_ui::drag::DragPayload;
///
/// struct Zone { key: u8 }
/// let payload = DragPayload::new(Zone { key: 60 }, "Zone C3");
/// assert_eq!(payload.get::<Zone>().map(|zone| zone.key), Some(60));
/// assert!(payload.get::<String>().is_none());
/// assert_eq!(payload.description(), "Zone C3");
/// let copy = payload.clone();
/// assert!(copy.same_as(&payload));
/// ```
#[derive(Clone)]
pub struct DragPayload {
    data: Arc<dyn Any + Send + Sync>,
    description: String,
}

impl DragPayload {
    /// A payload holding `data`, described as `description`: what the
    /// ghost under the pointer shows and a screen reader reads, such as
    /// "Kick.wav" or "3 samples".
    pub fn new<T: Any + Send + Sync>(data: T, description: impl Into<String>) -> DragPayload {
        DragPayload {
            data: Arc::new(data),
            description: description.into(),
        }
    }

    /// The data, if it is a `T`.
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.data.downcast_ref::<T>()
    }

    /// Whether the data is a `T`.
    pub fn is<T: Any>(&self) -> bool {
        self.data.is::<T>()
    }

    /// What the payload is, in a few words.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Whether `other` is this payload or a clone of it.
    pub fn same_as(&self, other: &DragPayload) -> bool {
        Arc::ptr_eq(&self.data, &other.data)
    }
}

impl fmt::Debug for DragPayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DragPayload")
            .field("description", &self.description)
            .finish_non_exhaustive()
    }
}

/// How a drag ended, as the widget that started it hears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DragOutcome {
    /// A target took the payload.
    Dropped,
    /// Nothing took it: Escape, a release where nothing accepts it or
    /// whose target refused the drop, a second button, or the pointer or
    /// the keyboard taken away.
    Cancelled,
}

/// What a widget hears about a drag (see the [module](self)).
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub enum DragEvent<'a> {
    /// The pointer is over the widget, or over a widget inside it that did
    /// not accept, with `payload`: [`Handled::Yes`](crate::Handled::Yes)
    /// accepts it at `at`. Offered on every move and again every frame,
    /// so that what is under a still pointer can change, as when a view
    /// scrolls under it.
    Over {
        /// Where the pointer is, in logical pixels.
        at: Point,
        /// What is dragged.
        payload: &'a DragPayload,
    },
    /// The widget accepted the last offer, and the pointer has left it, or
    /// another widget accepts now, or the drag ended without a drop here.
    Left,
    /// The button was released over the widget, which accepted the last
    /// offer: [`Handled::Yes`](crate::Handled::Yes) if it took `payload`.
    Drop {
        /// Where the pointer is, in logical pixels.
        at: Point,
        /// What is dropped.
        payload: &'a DragPayload,
    },
    /// The drag this widget started ended. Sent last, after the target
    /// heard its [`Drop`](DragEvent::Drop) or [`Left`](DragEvent::Left).
    Ended(DragOutcome),
}
