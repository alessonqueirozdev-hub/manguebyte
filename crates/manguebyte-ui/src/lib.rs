//! The Manguebyte user interface.
//!
//! This crate is the toolkit layer of ADR 0007: a retained tree of
//! widgets with layout, design tokens, parameter gestures, damage, an
//! accessibility model and a test harness. It draws through
//! `manguebyte-gfx` into a back buffer in memory and depends on no window
//! system, so every part of it runs in a unit test on any machine. It is
//! never a dependency of anything that processes audio.
//!
//! # The pieces
//!
//! - **Views** ([`View`], [`IntoView`]) are how an interface is written:
//!   plain functions such as [`row`], [`column()`], [`knob`] and [`meter`],
//!   composed as values, with layout set by methods such as
//!   [`spacing`](IntoView::spacing) and [`flex`](IntoView::flex). The
//!   [`layout`] itself is a pure function, tested with exact rectangles.
//! - **[`Root`]** builds the tree from a view once, and from then on runs
//!   frames and takes input. The tree is an arena in tree order; a frame
//!   polls every widget, lays out what changed size, records a scene for
//!   each widget that changed, and draws only the device rectangles those
//!   scenes covered, before and after. An interface where nothing changes
//!   draws nothing.
//! - **[`Widget`]** is what a node of the tree is. Its description for
//!   assistive technology, [`Widget::access`], has no default.
//! - **Sources** ([`Source`], [`State`], [`AtomicSource`]) are where shown
//!   values come from, polled once per frame and compared with the last
//!   value seen: no listeners, and no callbacks into widgets. A [`State`]
//!   kept by the plugin's editor spec survives closing and reopening the
//!   editor.
//! - **Parameters** ([`ParamHandle`], [`ParamBinding`]) are read from
//!   their atomics each frame and changed only through the editor
//!   contract's [`Gesture`](manguebyte_plugin::editor::Gesture), over the
//!   [`EditorContext`](manguebyte_plugin::editor::EditorContext) the host
//!   gave the editor. While a gesture is open a control shows the value it
//!   is sending, and after an edit the value it sent, until the host
//!   echoes it or [`ParamBinding::ECHO_WAIT`] has passed. Every edit starts
//!   from the value read at that moment.
//! - **Text** ([`text_field`], and a value typed into a [`knob`] or a
//!   [`slider`] after a double click): a caret, a selection, word jumps,
//!   the [`clipboard`], typed characters in the person's layout
//!   ([`Event::Text`]), input methods' compositions ([`Composition`]),
//!   filters and validation, Enter to take and Escape to throw away.
//! - **[Menus](menu)**: popup menus with submenus, drawn by the
//!   interface over everything else, opened by a widget with
//!   [`EventCx::open_menu`] or at a point with [`EventCx::open_menu_at`],
//!   and the [`combo`] box that picks a parameter's value from one.
//! - **[Scrolling](scroll)**: a [`scroll_view`] shows one child larger
//!   than itself through a viewport, with scroll bars, the wheel and a
//!   touchpad's finer steps, the keys, nested views that hand the wheel
//!   on at their edges, Tab scrolling to what it focuses, and an offset
//!   the application reads and sets through a [`State`]. Only what can be
//!   seen is drawn or can be pressed.
//! - **Lists** ([`list_view`]): rows the application keeps through
//!   [`ListData`], only those in sight drawn and described, with single
//!   and multiple selection, the keyboard, typing to find a row, and
//!   activation, all polled from a [`ListState`].
//! - **[Drag and drop](drag)** inside the interface: a widget starts a
//!   drag with a [`DragPayload`] of the application's own type, targets
//!   accept or refuse it by type and place, the root draws a ghost over
//!   everything and scrolls views whose edge the pointer is near, and
//!   Escape or a release where nothing accepts cancels. Lists reorder and
//!   move rows this way.
//! - **Pictures** ([`image`]): an image fitted to its rectangle, with a
//!   version per screen density so that it stays sharp at 2x.
//! - **Audio widgets**: a [`waveform_view`] of a [`Waveform`] drawn from
//!   its multi-resolution summary, with zoom, markers, a selection and a
//!   playhead; a [`piano_keyboard`] that plays [`NoteEvent`]s and shows
//!   notes sounding and key ranges; an [`envelope`] editor bound to four
//!   parameters; a [`zone_map`] of keys by velocities that lines up with
//!   a keyboard over the same [`KeySpan`] and takes dropped samples. Each
//!   shares what it shows and changes through a state the application
//!   polls.
//! - **Hover and tooltips**: the widget under the pointer hears it
//!   ([`Widget::hover_changed`]), and a view given a
//!   [`tooltip`](IntoView::tooltip) shows it after the pointer rests on it
//!   for [`TOOLTIP_DELAY`]; the text is the view's description for
//!   assistive technology too.
//! - **[`Theme`]**: colours, sizes, radii and type as typed data, with no
//!   inheritance.
//! - **[Motion](motion)**: [`Animated`] values that ease to a new target
//!   on the frame clock, the theme's duration tokens, and hover, press
//!   and focus amounts the root animates for every widget. Only what moves
//!   is drawn again, and frames stop asking for more once everything has
//!   settled; with motion turned off (the theme or the system) changes
//!   jump.
//! - **[Accessibility](access)**: roles, names, values, ranges, states and
//!   actions, keyboard focus in tree order, [`Root::act`] for requests
//!   from assistive technology, and a check of the tree and the theme's
//!   contrast ([`testing::Harness::assert_accessible`]).
//! - **[`testing::Harness`]**: drives an interface with no window: finds
//!   widgets by role and name only, sends drags, clicks, wheel turns and
//!   keys, runs frames on a clock the test controls, and shows pixels,
//!   scene dumps, damage and the gestures a host received.
//!
//! # Threads
//!
//! Everything here runs on one thread, the host's main thread inside a
//! plugin. Nothing keeps global state or starts a thread. The audio thread
//! never touches this crate: it stores into parameters and into atomics
//! such as [`manguebyte_core::AtomicF32`], which the interface reads.
//!
//! # Example
//!
//! A small editor: a title, two knobs and a meter the audio thread feeds.
//!
//! ```
//! use std::rc::Rc;
//! use std::sync::Arc;
//! use std::sync::atomic::Ordering;
//! use std::time::Duration;
//! use manguebyte_core::AtomicF32;
//! use manguebyte_gfx::{IRect, Size};
//! use manguebyte_params::{FloatParam, FloatRange, param_set};
//! use manguebyte_plugin::editor::EditorContext;
//! use manguebyte_plugin::editor::testing::RecordingEditorHost;
//! use manguebyte_ui::{
//!     AtomicSource, IntoView, ParamHandle, Role, Root, column, knob, label, meter, row,
//! };
//!
//! struct Params { threshold: FloatParam, depth: FloatParam }
//! param_set!(Params { threshold, depth });
//! let params = Arc::new(Params {
//!     threshold: FloatParam::new("threshold", "Threshold", FloatRange::linear(-60.0, 0.0), -24.0)
//!         .with_unit("dB")
//!         .with_precision(1),
//!     depth: FloatParam::new("depth", "Depth", FloatRange::linear(0.0, 24.0), 12.0)
//!         .with_unit("dB")
//!         .with_precision(1),
//! });
//! // What the audio thread stores once per block.
//! let reduction = Arc::new(AtomicF32::new(0.0));
//!
//! let view = column((
//!     label("Duck").title(),
//!     row((
//!         knob(ParamHandle::new(Arc::clone(&params), |p| &p.threshold)),
//!         knob(ParamHandle::new(Arc::clone(&params), |p| &p.depth)),
//!         meter("Reduction", AtomicSource::new(Arc::clone(&reduction)))
//!             .range(0.0, 24.0)
//!             .unit("dB")
//!             .precision(1),
//!     ))
//!     .spacing(8.0),
//! ))
//! .spacing(8.0)
//! .padding(12.0);
//!
//! // A wrapper hands the editor a context when the host opens it.
//! let (context, link) = EditorContext::attach(Rc::new(RecordingEditorHost::new()));
//! let mut root = Root::new(view, Size::new(240.0, 190.0), 2.0, context);
//!
//! // The first frame draws everything, at 2x.
//! let damage = root.frame(Duration::ZERO).damage.to_vec();
//! assert_eq!(damage, [IRect::new(0, 0, 480, 380)]);
//!
//! // Nothing changed, so nothing is drawn.
//! assert!(root.frame(Duration::from_millis(16)).damage.is_empty());
//!
//! // The audio thread publishes a level: only the meter is drawn again.
//! reduction.store(6.0, Ordering::Relaxed);
//! let damage = root.frame(Duration::from_millis(32)).damage.to_vec();
//! let meter = root
//!     .access_tree()
//!     .into_iter()
//!     .find(|entry| entry.node.role == Role::Meter)
//!     .expect("there is a meter");
//! assert_eq!(meter.node.value.as_deref(), Some("6.0 dB"));
//! assert!(!damage.is_empty());
//! assert!(damage.iter().all(|rect| meter.device.contains_rect(rect)));
//!
//! // Closing is dropping the interface, then the link.
//! drop(root);
//! drop(link);
//! ```

#![forbid(unsafe_code)]

pub mod access;
pub mod clipboard;
mod damage;
pub mod drag;
pub mod event;
pub mod layout;
pub mod menu;
pub mod motion;
mod param;
mod root;
pub mod scroll;
pub mod source;
pub mod testing;
mod theme;
mod tree;
mod view;
mod widget;
pub mod widgets;

pub use access::{AccessEntry, AccessItem, AccessNode, Action, Actions, Role};
pub use clipboard::{Clipboard, MemoryClipboard};
pub use damage::MAX_DAMAGE;
pub use drag::{DragEvent, DragOutcome, DragPayload};
pub use event::{
    Button, Composition, DropData, Event, FileDrop, Handled, Key, KeyEvent, MenuEvent, Modifiers,
};
pub use menu::{Menu, MenuItem};
pub use motion::{Animated, Easing, Motion, Transition};
pub use param::{ParamBinding, ParamHandle};
pub use root::{Frame, MAX_BUFFER_SIDE, Root, TOOLTIP_DELAY};
pub use scroll::ScrollMetrics;
pub use source::{AtomicSource, Source, State, Watch};
pub use theme::{Corners, Elevation, Hue, Palette, Sizes, Spacing, Theme, Typography};
pub use tree::NodeId;
pub use view::{Children, IntoView, View, column, filled, panel, row, stack};
pub use widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};
pub use widgets::{
    Canvas, CanvasCx, ComboBox, Container, Fit, ImageView, Knob, Label, ListData, ListEvent,
    ListState, ListView, MAX_PIXELS_PER_FRAME, MAX_ZERO_SEARCH, Marker, MarkerKind, MarkerOrder,
    Meter, RowCx, ScrollView, ScrollViewBuilder, Slider, TextField, Toggle, Waveform,
    WaveformEvent, WaveformState, WaveformView, ZOOM_STEP, canvas, combo, image, knob, label,
    list_view, meter, scroll_view, slider, text_field, toggle, waveform_view,
};
pub use widgets::{
    EnvelopeEditor, KeyRange, KeySpan, KeyboardState, NoteEvent, PianoKeyboard, Zone, ZoneEvent,
    ZoneMap, ZoneMapState, envelope, is_black, note_name, note_name_portuguese, piano_keyboard,
    zone_map,
};
