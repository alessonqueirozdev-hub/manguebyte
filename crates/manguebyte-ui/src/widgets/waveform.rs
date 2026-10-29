//! A view of a sample buffer: its waveform, markers a person drags, a
//! selection and a playhead.

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_gfx::{
    Align, Extend, LinearGradient, Path, Point, Rect, RoundedRect, Scene, Size, Stop, Stroke,
};

use super::wave_data::{MAX_ZERO_SEARCH, Waveform};
use super::{focus_ring, hline, line, line_height, snap_px, value_style, vline};
use crate::access::{AccessItem, AccessNode, AccessRange, Action, Actions, Role, selection_color};
use crate::event::{Button, Event, Handled, Key, KeyEvent, Modifiers};
use crate::motion::Lerp;
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// How many times narrower one notch of the wheel with Ctrl held makes
/// the frames shown.
pub const ZOOM_STEP: f64 = 1.5;

/// The most logical pixels a frame is drawn across, zoomed all the way
/// in.
pub const MAX_PIXELS_PER_FRAME: f64 = 16.0;

/// How near a marker's line, in logical pixels, a press takes the marker.
const GRAB: f32 = 5.0;

/// How far the pointer moves before a press becomes a selection.
const SELECT_START: f32 = 3.0;

/// How far, in logical pixels, a marker dragged with snapping on looks for
/// a zero crossing either way.
const SNAP_PIXELS: f64 = 6.0;

/// What a marker is for, which sets its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum MarkerKind {
    /// Where playing starts or ends: the theme's focus colour.
    Edge,
    /// Where a loop starts or ends: the theme's meter colour.
    Loop,
    /// Any other point: the theme's muted text colour.
    Cue,
}

/// A point in a sample that the application defines and a person drags:
/// where it starts, where its loop ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The application's name for it, unique among the markers of a view.
    pub id: u32,
    /// What it is called, for people and assistive technology.
    pub name: String,
    /// Where it is: the first frame after it, from 0 to the sample's
    /// length.
    pub frame: u64,
    /// What it is for.
    pub kind: MarkerKind,
}

impl Marker {
    /// Marker `id` called `name` at `frame`.
    pub fn new(id: u32, name: impl Into<String>, frame: u64, kind: MarkerKind) -> Marker {
        Marker {
            id,
            name: name.into(),
            frame,
            kind,
        }
    }
}

/// A rule two markers keep while either moves: the marker `first` stays
/// at least `gap` frames before the marker `second`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MarkerOrder {
    /// The marker that comes first.
    pub first: u32,
    /// The marker that comes after it.
    pub second: u32,
    /// How many frames apart they stay at least: 1 for "before", 0 for
    /// "not after".
    pub gap: u64,
}

impl MarkerOrder {
    /// `first` stays at least `gap` frames before `second`.
    pub const fn new(first: u32, second: u32, gap: u64) -> MarkerOrder {
        MarkerOrder { first, second, gap }
    }
}

/// Something that happened in a waveform view, for the application to act
/// on; it takes them with [`WaveformState::take_events`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WaveformEvent {
    /// A person moved the marker `id` to `frame`: once per move, when a
    /// drag ends or a key or assistive technology moved it. The marker's
    /// place in the state follows the pointer during a drag.
    MarkerMoved {
        /// The marker.
        id: u32,
        /// Where it is now.
        frame: u64,
    },
    /// A person chose these frames, or none, by dragging across the
    /// waveform or clicking away a selection.
    Selected(Option<Range<u64>>),
    /// A person clicked at `frame` without dragging, as to play from
    /// there.
    Clicked(u64),
}

#[derive(Debug, Default)]
struct WaveInner {
    waveform: Option<Arc<Waveform>>,
    /// The frames shown, from and to; `None` for all of them.
    shown: Option<(f64, f64)>,
    selection: Option<Range<u64>>,
    markers: Vec<Marker>,
    order: Vec<MarkerOrder>,
    snap: bool,
    playhead: Option<u64>,
    events: Vec<WaveformEvent>,
    version: u64,
}

impl WaveInner {
    fn len(&self) -> u64 {
        self.waveform.as_ref().map_or(0, |wave| wave.len() as u64)
    }

    /// The frames shown, within the sample.
    fn span(&self) -> (f64, f64) {
        let len = self.len() as f64;
        match self.shown {
            Some((start, end)) if end > start && start >= 0.0 && end <= len => (start, end),
            _ => (0.0, len),
        }
    }

    fn find(&self, id: u32) -> Option<&Marker> {
        self.markers.iter().find(|marker| marker.id == id)
    }

    /// The lowest and highest frame the marker at `index` may move to.
    fn bounds(&self, index: usize) -> (u64, u64) {
        let Some(marker) = self.markers.get(index) else {
            return (0, 0);
        };
        let (mut low, mut high) = (0, self.len());
        for order in &self.order {
            if order.second == marker.id {
                if let Some(first) = self.find(order.first) {
                    low = low.max(first.frame.saturating_add(order.gap));
                }
            }
            if order.first == marker.id {
                if let Some(second) = self.find(order.second) {
                    high = high.min(second.frame.saturating_sub(order.gap));
                }
            }
        }
        if low > high {
            // The rules cannot all hold: the marker stays where it is.
            (marker.frame, marker.frame)
        } else {
            (low, high)
        }
    }

    /// Where the marker at `index` goes when it is asked to go to
    /// `wanted`: within its bounds, and on the zero crossing nearest it,
    /// no further than `within` frames, when snapping is on and there is
    /// one inside the bounds.
    fn place(&self, index: usize, wanted: u64, within: Option<u64>) -> u64 {
        let (low, high) = self.bounds(index);
        let clamped = wanted.clamp(low, high);
        match (&self.waveform, within) {
            (Some(wave), Some(within)) if self.snap => wave
                .zero_crossing_near(wanted, within)
                .filter(|frame| (low..=high).contains(frame))
                .unwrap_or(clamped),
            _ => clamped,
        }
    }

    /// Moves the marker at `index` to `frame` and returns whether it moved.
    fn move_marker(&mut self, index: usize, frame: u64) -> bool {
        match self.markers.get_mut(index) {
            Some(marker) if marker.frame != frame => {
                marker.frame = frame;
                true
            }
            _ => false,
        }
    }
}

/// What a [`WaveformView`] shares with the application: the sample shown,
/// the frames in sight, the selection, the markers and the rules they
/// keep, the playhead, and what happened in the view.
///
/// Both read and write it, and its [`version`](WaveformState::version)
/// counts every change: the view draws again when it moves, and the
/// application can tell that a person changed something without reading
/// everything. It is `Send`, so a plugin's editor spec can keep it and a
/// reopened editor shows the same sample, zoom and markers. Nothing calls
/// the application back: it polls.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_ui::{Waveform, WaveformState};
///
/// let state = WaveformState::new();
/// state.set_waveform(Some(Arc::new(Waveform::new(vec![vec![0.0; 48_000]], 48_000.0))));
/// state.set_sample_points(0, 48_000, Some(12_000..36_000));
/// // The loop start cannot be put before the start, nor the end before the loop end.
/// assert_eq!(state.set_marker_frame(WaveformState::END, 1_000), Some(36_000));
/// assert_eq!(state.marker(WaveformState::END).map(|m| m.frame), Some(36_000));
/// ```
#[derive(Clone, Default)]
pub struct WaveformState {
    inner: Arc<Mutex<WaveInner>>,
}

impl WaveformState {
    /// The id [`set_sample_points`](WaveformState::set_sample_points)
    /// gives the marker where playing starts.
    pub const START: u32 = 0;
    /// The id of the marker where playing ends.
    pub const END: u32 = 1;
    /// The id of the marker where the loop starts.
    pub const LOOP_START: u32 = 2;
    /// The id of the marker where the loop ends.
    pub const LOOP_END: u32 = 3;

    /// No sample, no markers.
    pub fn new() -> WaveformState {
        WaveformState::default()
    }

    fn lock(&self) -> MutexGuard<'_, WaveInner> {
        // Nothing that can panic runs while the lock is held.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change<R>(&self, change: impl FnOnce(&mut WaveInner) -> R) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        inner.version = inner.version.wrapping_add(1);
        result
    }

    /// A number that changes with every change to the state.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// The sample shown, if any.
    pub fn waveform(&self) -> Option<Arc<Waveform>> {
        self.lock().waveform.clone()
    }

    /// Shows `waveform`, all of it, with nothing selected. The markers stay
    /// as they are.
    pub fn set_waveform(&self, waveform: Option<Arc<Waveform>>) {
        self.change(|inner| {
            inner.waveform = waveform;
            inner.shown = None;
            inner.selection = None;
        });
    }

    /// The markers, in the order the application gave them.
    pub fn markers(&self) -> Vec<Marker> {
        self.lock().markers.clone()
    }

    /// The marker `id`, if there is one.
    pub fn marker(&self, id: u32) -> Option<Marker> {
        self.lock().find(id).cloned()
    }

    /// Replaces the markers. Their order is the order the keyboard and
    /// assistive technology go through them in.
    pub fn set_markers(&self, markers: Vec<Marker>) {
        self.change(|inner| inner.markers = markers);
    }

    /// Replaces the rules the markers keep while a person moves them. The
    /// application's own changes are not held to them.
    pub fn set_order(&self, order: Vec<MarkerOrder>) {
        self.change(|inner| inner.order = order);
    }

    /// Moves the marker `id` to `frame`, or as near it as its rules allow,
    /// and returns where it went: `None` if there is no such marker.
    pub fn set_marker_frame(&self, id: u32, frame: u64) -> Option<u64> {
        self.change(|inner| {
            let index = inner.markers.iter().position(|marker| marker.id == id)?;
            let placed = inner.place(index, frame, None);
            inner.move_marker(index, placed);
            Some(placed)
        })
    }

    /// The markers of a sampler's sample: a start and an end
    /// ([`MarkerKind::Edge`]), and, if `looped` is given, a loop start and
    /// a loop end ([`MarkerKind::Loop`]), with the ids
    /// [`START`](Self::START), [`END`](Self::END),
    /// [`LOOP_START`](Self::LOOP_START) and [`LOOP_END`](Self::LOOP_END)
    /// and the rules that the start comes before the end and the loop lies
    /// within them, the loop start before the loop end.
    pub fn set_sample_points(&self, start: u64, end: u64, looped: Option<Range<u64>>) {
        let mut markers = vec![
            Marker::new(Self::START, "Start", start, MarkerKind::Edge),
            Marker::new(Self::END, "End", end, MarkerKind::Edge),
        ];
        let mut order = vec![MarkerOrder::new(Self::START, Self::END, 1)];
        if let Some(looped) = looped {
            markers.push(Marker::new(
                Self::LOOP_START,
                "Loop start",
                looped.start,
                MarkerKind::Loop,
            ));
            markers.push(Marker::new(
                Self::LOOP_END,
                "Loop end",
                looped.end,
                MarkerKind::Loop,
            ));
            order.extend([
                MarkerOrder::new(Self::START, Self::LOOP_START, 0),
                MarkerOrder::new(Self::LOOP_START, Self::LOOP_END, 1),
                MarkerOrder::new(Self::LOOP_END, Self::END, 0),
            ]);
        }
        self.change(|inner| {
            inner.markers = markers;
            inner.order = order;
        });
    }

    /// Whether markers a person drags land on the nearest zero crossing,
    /// so that playing from them does not click.
    pub fn snaps_to_zero(&self) -> bool {
        self.lock().snap
    }

    /// Turns snapping to zero crossings on or off.
    pub fn set_snap_to_zero(&self, snap: bool) {
        self.change(|inner| inner.snap = snap);
    }

    /// The frames selected, if any.
    pub fn selection(&self) -> Option<Range<u64>> {
        self.lock().selection.clone()
    }

    /// Selects `frames`, or nothing.
    pub fn set_selection(&self, frames: Option<Range<u64>>) {
        self.change(|inner| inner.selection = frames.filter(|range| !range.is_empty()));
    }

    /// Where the playhead is, if it is shown.
    pub fn playhead(&self) -> Option<u64> {
        self.lock().playhead
    }

    /// Shows the playhead at `frame`, or hides it: what the application
    /// sets, as often as once a frame, from what its audio thread
    /// publishes.
    pub fn set_playhead(&self, frame: Option<u64>) {
        let mut inner = self.lock();
        if inner.playhead != frame {
            inner.playhead = frame;
            inner.version = inner.version.wrapping_add(1);
        }
    }

    /// The frames shown, from the left edge to the right.
    pub fn shown(&self) -> Range<f64> {
        let (start, end) = self.lock().span();
        start..end
    }

    /// Shows `frames`, kept within the sample and to at most
    /// [`MAX_PIXELS_PER_FRAME`] when drawn.
    pub fn show(&self, frames: Range<f64>) {
        self.change(|inner| {
            inner.shown = (frames.start.is_finite() && frames.end.is_finite())
                .then_some((frames.start, frames.end));
        });
    }

    /// Shows the whole sample.
    pub fn show_all(&self) {
        self.change(|inner| inner.shown = None);
    }

    /// What happened in the view since the application last asked, oldest
    /// first, forgetting it.
    pub fn take_events(&self) -> Vec<WaveformEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

impl std::fmt::Debug for WaveformState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("WaveformState")
            .field("waveform", &inner.waveform)
            .field("markers", &inner.markers.len())
            .field("version", &inner.version)
            .finish_non_exhaustive()
    }
}

/// Where things are in a view, for one rectangle and one state.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    /// The strip along the top where markers' names are.
    strip: Rect,
    /// Where the channels are drawn.
    area: Rect,
    start: f64,
    end: f64,
    len: u64,
}

impl Geometry {
    fn new(rect: Rect, theme: &Theme, inner: &WaveInner) -> Geometry {
        let strip_height = (line_height(&value_style(theme)) + 2.0).min(rect.height);
        let strip = Rect::new(rect.x, rect.y, rect.width, strip_height);
        let area = Rect::new(
            rect.x,
            rect.y + strip_height,
            rect.width,
            (rect.height - strip_height).max(0.0),
        );
        let (start, end) = inner.span();
        Geometry {
            strip,
            area,
            start,
            end,
            len: inner.len(),
        }
    }

    fn span(&self) -> f64 {
        self.end - self.start
    }

    /// Frames per logical pixel.
    fn per_pixel(&self) -> f64 {
        self.span() / f64::from(self.area.width.max(1.0))
    }

    fn x_of(&self, frame: f64) -> f32 {
        let span = self.span();
        if span <= 0.0 {
            return self.area.x;
        }
        self.area.x + ((frame - self.start) / span * f64::from(self.area.width)) as f32
    }

    /// The frame at `x`, within the sample.
    fn frame_at(&self, x: f32) -> u64 {
        let frame = self.start + f64::from(x - self.area.x) * self.per_pixel();
        if frame.is_finite() {
            frame.round().clamp(0.0, self.len as f64) as u64
        } else {
            0
        }
    }

    /// The fewest frames the view shows.
    fn min_span(&self) -> f64 {
        (f64::from(self.area.width) / MAX_PIXELS_PER_FRAME)
            .max(2.0)
            .min(self.len as f64)
    }

    /// The frames to show for `span` frames with the frame `anchor` at
    /// `x`, within the sample; `None` for all of them.
    fn zoomed(&self, span: f64, anchor: f64, x: f32) -> Option<(f64, f64)> {
        let len = self.len as f64;
        let span = span.clamp(self.min_span(), len);
        if span >= len {
            return None;
        }
        let along = f64::from((x - self.area.x) / self.area.width.max(1.0)).clamp(0.0, 1.0);
        let start = (anchor - along * span).clamp(0.0, len - span);
        Some((start, start + span))
    }
}

/// The lowest and highest sample of `channel` over the frames that `count`
/// columns of a view of `start..end` show, as fractions of the height from
/// the bottom, for [`Scene::columns`].
fn columns(wave: &Waveform, channel: usize, start: f64, end: f64, count: usize) -> Columns {
    let span = end - start;
    let mut columns = Columns {
        low: Vec::with_capacity(count),
        high: Vec::with_capacity(count),
        core_low: Vec::with_capacity(count),
        core_high: Vec::with_capacity(count),
    };
    let mut spans = Vec::with_capacity(count);
    let mut energy = Vec::with_capacity(count);
    for column in 0..count {
        let from = (start + span * column as f64 / count as f64).floor() as usize;
        let to = (start + span * (column + 1) as f64 / count as f64).floor() as usize;
        // Each column takes in the last frame of the one before, so that
        // neighbouring columns join.
        let frames = from.saturating_sub(1)..to.max(from + 1);
        let (l, h, rms) = wave.summary(channel, frames).unwrap_or((0.0, 0.0, 0.0));
        columns.low.push((l + 1.0) * 0.5);
        columns.high.push((h + 1.0) * 0.5);
        spans.push((l, h));
        energy.push(rms * rms);
    }
    // The core: the root mean square either side of the middle, over a
    // few columns either way so that it reads as a smooth body rather
    // than as each column's share of a cycle, and within each span.
    for (column, &(l, h)) in spans.iter().enumerate() {
        let near = &energy[column.saturating_sub(CORE_REACH)..(column + CORE_REACH + 1).min(count)];
        let rms = (near.iter().sum::<f32>() / near.len() as f32).sqrt();
        columns.core_low.push((l.max(-rms) + 1.0) * 0.5);
        columns.core_high.push((h.min(rms) + 1.0) * 0.5);
    }
    columns
}

/// How many columns either way a column's core is averaged over.
const CORE_REACH: usize = 4;

/// A channel's columns: the span of each, from its lowest to highest
/// sample, and its core, the root mean square either side of the middle,
/// as fractions of the lane from its bottom.
#[derive(Debug, Clone)]
struct Columns {
    low: Vec<f32>,
    high: Vec<f32>,
    core_low: Vec<f32>,
    core_high: Vec<f32>,
}

/// How one channel is drawn.
#[derive(Debug, Clone)]
enum Shape {
    /// A span per device pixel column, from its lowest to highest sample,
    /// with a brighter core.
    Columns(Columns),
    /// A line through the samples, zoomed in further than a frame per
    /// device pixel.
    Line(Path),
}

/// What the channels were last drawn from.
#[derive(Debug, Clone, Copy, PartialEq)]
struct CacheKey {
    wave: usize,
    start: f64,
    end: f64,
    area: Rect,
    scale: f32,
}

/// A press, until it is released.
#[derive(Debug, Clone)]
enum Press {
    /// On the marker at `index`, which was at `from`, `offset` logical
    /// pixels right of its line.
    Marker {
        index: usize,
        from: u64,
        offset: f32,
    },
    /// On the waveform, at `frame` and `at`; `before` is what was
    /// selected.
    Select {
        frame: u64,
        at: Point,
        moved: bool,
        before: Option<Range<u64>>,
    },
}

/// The colour of a marker of `kind`.
fn marker_color(theme: &Theme, kind: MarkerKind) -> manguebyte_gfx::Color {
    match kind {
        MarkerKind::Edge => theme.palette.text,
        MarkerKind::Loop => theme.palette.hues[1].color,
        MarkerKind::Cue => theme.palette.text_secondary,
    }
}

/// `frame` of `wave` as a person reads it: seconds and the frame.
fn frame_text(wave: Option<&Waveform>, frame: u64) -> String {
    match wave {
        Some(wave) => format!("{:.3} s, frame {frame}", wave.frame_to_seconds(frame)),
        None => format!("frame {frame}"),
    }
}

/// A view of a sample: its waveform drawn from the [`Waveform`]'s summary,
/// markers a person drags, a selection and a playhead, shared with the
/// application through a [`WaveformState`].
///
/// - Each channel has a lane of its own. Each device pixel column shows
///   the lowest and highest sample of the frames under it, read from the
///   summary, so a ten-minute file draws as fast as a short one; zoomed in
///   past a frame per device pixel the samples are joined by a line. The
///   columns are kept until the frames shown, the size or the scale
///   change, so a playhead moving every frame reads nothing.
/// - Ctrl (Command on macOS) and the wheel zoom in and out around the
///   pointer, by [`ZOOM_STEP`] a notch; a precision touchpad's pinch
///   arrives as Ctrl and fractions of a notch, and zooms smoothly. The
///   wheel alone, or sideways, scrolls; at an end, or showing everything,
///   it is left to a [`scroll_view`](crate::scroll_view) around.
/// - A press near a marker's line drags it, within the rules the state
///   holds ([`MarkerOrder`]: a start before an end, a loop inside them),
///   and onto the nearest zero crossing when the state
///   [snaps](WaveformState::set_snap_to_zero). Escape puts it back.
/// - A drag anywhere else selects frames; a click reports where it was
///   ([`WaveformEvent::Clicked`]) and clears the selection.
/// - With the keyboard focus: Up and Down choose a marker, Left and Right
///   move it by a pixel's worth of frames (a frame with Shift; to the
///   next zero crossing when snapping), Home and End to as far as its
///   rules allow, `+` and `-` zoom around it.
///
/// For assistive technology the view is a [`Role::Group`] whose value
/// says what is shown, and each marker a [`Role::Slider`] whose value is
/// its place in seconds and frames, which can be raised, lowered and set.
pub struct WaveformView {
    name: String,
    state: WaveformState,
    version: Option<u64>,
    press: Option<Press>,
    /// The marker the keyboard moves.
    cursor: usize,
    cache: Option<(CacheKey, Vec<Shape>)>,
}

/// A view called `name` of the sample `state` holds.
///
/// ```
/// use std::sync::Arc;
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Key, Modifiers, Role, Waveform, WaveformEvent, WaveformState, waveform_view};
///
/// let state = WaveformState::new();
/// let tone: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
/// state.set_waveform(Some(Arc::new(Waveform::new(vec![tone], 48_000.0).named("Tone"))));
/// state.set_sample_points(0, 4800, None);
/// let mut ui = Harness::new(waveform_view("Sample", state.clone()), Size::new(480.0, 120.0), 1.0);
/// ui.frame();
/// assert_eq!(ui.get(Role::Slider, "End").value_text(), Some("0.100 s, frame 4800"));
///
/// // The start marker, dragged 48 pixels: ten frames a pixel.
/// let start = ui.get(Role::Slider, "Start");
/// ui.drag(start.center(), (48.0, 0.0), 4);
/// assert_eq!(state.marker(WaveformState::START).map(|m| m.frame), Some(480));
/// assert_eq!(state.take_events(), [WaveformEvent::MarkerMoved { id: 0, frame: 480 }]);
/// // The press gave it the keyboard: Shift and Right move it one frame.
/// assert!(ui.key_with(Key::Right, Modifiers { shift: true, ..Modifiers::NONE }));
/// assert_eq!(state.marker(WaveformState::START).map(|m| m.frame), Some(481));
/// ```
pub fn waveform_view(name: impl Into<String>, state: WaveformState) -> WaveformView {
    WaveformView {
        name: name.into(),
        state,
        version: None,
        press: None,
        cursor: 0,
        cache: None,
    }
}

impl WaveformView {
    /// The state it shares with the application.
    pub fn state(&self) -> &WaveformState {
        &self.state
    }

    /// Moves the marker at `index` to where `wanted` places it, and says so
    /// in an event. Returns whether it moved.
    fn move_to(&mut self, cx: &mut EventCx<'_>, index: usize, wanted: u64, within: Option<u64>) {
        let moved = self.state.change(|inner| {
            let frame = inner.place(index, wanted, within);
            let moved = inner.move_marker(index, frame);
            if moved {
                let id = inner.markers[index].id;
                inner.events.push(WaveformEvent::MarkerMoved { id, frame });
            }
            moved
        });
        if moved {
            cx.request_paint();
        }
    }

    /// Moves the marker at `index` one step: a pixel's worth of frames, or
    /// one with `fine`; to the next zero crossing when snapping.
    fn step(&mut self, cx: &mut EventCx<'_>, index: usize, forward: bool, fine: bool) {
        let (frame, wanted) = {
            let inner = self.state.lock();
            let geometry = Geometry::new(cx.rect(), cx.theme(), &inner);
            let Some(marker) = inner.markers.get(index) else {
                return;
            };
            let frame = marker.frame;
            let wanted = match &inner.waveform {
                Some(wave) if inner.snap => {
                    let within = wave.sample_rate().ceil() as u64;
                    wave.zero_crossing_beyond(frame, forward, within)
                }
                _ => {
                    let step = if fine {
                        1
                    } else {
                        geometry.per_pixel().round().max(1.0) as u64
                    };
                    Some(if forward {
                        frame.saturating_add(step)
                    } else {
                        frame.saturating_sub(step)
                    })
                }
            };
            (frame, wanted)
        };
        if let Some(wanted) = wanted.filter(|&wanted| wanted != frame) {
            self.move_to(cx, index, wanted, None);
        }
    }

    /// Zooms by `factor` (above 1 shows fewer frames) around `x`, or the
    /// middle.
    fn zoom(&mut self, cx: &mut EventCx<'_>, factor: f64, x: Option<f32>) {
        self.state.change(|inner| {
            let geometry = Geometry::new(cx.rect(), cx.theme(), inner);
            if geometry.len == 0 || !factor.is_finite() || factor <= 0.0 {
                return;
            }
            let x = x.unwrap_or(geometry.area.x + geometry.area.width * 0.5);
            let anchor = geometry.start + f64::from(x - geometry.area.x) * geometry.per_pixel();
            inner.shown = geometry.zoomed(geometry.span() / factor, anchor, x);
        });
        cx.request_paint();
    }

    /// Scrolls by `pixels` (above 0: later frames). Returns whether it moved.
    fn scroll(&mut self, cx: &mut EventCx<'_>, pixels: f32) -> bool {
        let moved = {
            let mut inner = self.state.lock();
            let geometry = Geometry::new(cx.rect(), cx.theme(), &inner);
            let span = geometry.span();
            let room = geometry.len as f64 - span;
            let start = (geometry.start + f64::from(pixels) * geometry.per_pixel())
                .clamp(0.0, room.max(0.0));
            if room <= 0.0 || start == geometry.start {
                false
            } else {
                inner.shown = Some((start, start + span));
                inner.version = inner.version.wrapping_add(1);
                true
            }
        };
        if moved {
            cx.request_paint();
        }
        moved
    }

    fn press(&mut self, cx: &mut EventCx<'_>, at: Point) -> Handled {
        cx.request_focus();
        cx.request_paint();
        let inner = self.state.lock();
        if inner.waveform.is_none() {
            return Handled::Yes;
        }
        let geometry = Geometry::new(cx.rect(), cx.theme(), &inner);
        let near = inner
            .markers
            .iter()
            .enumerate()
            .map(|(index, marker)| (index, at.x - geometry.x_of(marker.frame as f64)))
            // Of markers in one place, the one drawn last, on top, is taken.
            .rev()
            .filter(|&(_, offset)| offset.abs() <= GRAB)
            .min_by(|a, b| a.1.abs().total_cmp(&b.1.abs()));
        self.press = Some(match near {
            Some((index, offset)) => {
                self.cursor = index;
                Press::Marker {
                    index,
                    from: inner.markers[index].frame,
                    offset,
                }
            }
            None => Press::Select {
                frame: geometry.frame_at(at.x),
                at,
                moved: false,
                before: inner.selection.clone(),
            },
        });
        drop(inner);
        cx.capture_pointer();
        Handled::Yes
    }

    fn pointer_move(&mut self, cx: &mut EventCx<'_>, at: Point) -> Handled {
        let Some(press) = &mut self.press else {
            return Handled::No;
        };
        let rect = cx.rect();
        let theme = *cx.theme();
        match press {
            Press::Marker { index, offset, .. } => {
                let (index, offset) = (*index, *offset);
                let (wanted, within) = {
                    let inner = self.state.lock();
                    let geometry = Geometry::new(rect, &theme, &inner);
                    let within = (geometry.per_pixel() * SNAP_PIXELS).ceil().max(1.0) as u64;
                    (
                        geometry.frame_at(at.x - offset),
                        within.min(MAX_ZERO_SEARCH),
                    )
                };
                self.state.change(|inner| {
                    let frame = inner.place(index, wanted, Some(within));
                    inner.move_marker(index, frame);
                });
                cx.request_paint();
            }
            Press::Select {
                frame,
                at: from,
                moved,
                ..
            } => {
                if !*moved && (at.x - from.x).abs() < SELECT_START {
                    return Handled::Yes;
                }
                *moved = true;
                let anchor = *frame;
                self.state.change(|inner| {
                    let geometry = Geometry::new(rect, &theme, inner);
                    let here = geometry.frame_at(at.x);
                    let range = anchor.min(here)..anchor.max(here);
                    inner.selection = (!range.is_empty()).then_some(range);
                });
                cx.request_paint();
            }
        }
        Handled::Yes
    }

    fn release(&mut self, cx: &mut EventCx<'_>) -> Handled {
        let Some(press) = self.press.take() else {
            return Handled::No;
        };
        cx.release_pointer();
        cx.request_paint();
        self.state.change(|inner| match press {
            Press::Marker { index, from, .. } => {
                if let Some(marker) = inner.markers.get(index) {
                    if marker.frame != from {
                        let (id, frame) = (marker.id, marker.frame);
                        inner.events.push(WaveformEvent::MarkerMoved { id, frame });
                    }
                }
            }
            Press::Select {
                frame,
                moved,
                before,
                ..
            } => {
                if moved {
                    let selection = inner.selection.clone();
                    inner.events.push(WaveformEvent::Selected(selection));
                } else {
                    if before.is_some() {
                        inner.selection = None;
                        inner.events.push(WaveformEvent::Selected(None));
                    }
                    inner.events.push(WaveformEvent::Clicked(frame));
                }
            }
        });
        Handled::Yes
    }

    /// Ends a press, putting back what it changed.
    fn cancel(&mut self, cx: &mut EventCx<'_>) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        cx.release_pointer();
        cx.request_paint();
        self.state.change(|inner| match press {
            Press::Marker { index, from, .. } => {
                inner.move_marker(index, from);
            }
            Press::Select { before, .. } => inner.selection = before,
        });
        true
    }

    fn key(&mut self, cx: &mut EventCx<'_>, key: Key, modifiers: Modifiers) -> Handled {
        let count = self.state.lock().markers.len();
        let cursor = self.cursor.min(count.saturating_sub(1));
        match key {
            Key::Up | Key::Down if count > 0 => {
                self.cursor = if key == Key::Up {
                    cursor.saturating_sub(1)
                } else {
                    (cursor + 1).min(count - 1)
                };
                cx.request_paint();
            }
            Key::Left | Key::Right if count > 0 => {
                self.step(cx, cursor, key == Key::Right, modifiers.shift);
            }
            Key::Home | Key::End if count > 0 => {
                let (low, high) = self.state.lock().bounds(cursor);
                let wanted = if key == Key::Home { low } else { high };
                self.move_to(cx, cursor, wanted, None);
            }
            Key::Character('+' | '=') => {
                let x = self.marker_x(cx, cursor);
                self.zoom(cx, ZOOM_STEP, x);
            }
            Key::Character('-' | '_') => {
                let x = self.marker_x(cx, cursor);
                self.zoom(cx, 1.0 / ZOOM_STEP, x);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// Where the marker at `index` is drawn, if it is in sight.
    fn marker_x(&self, cx: &EventCx<'_>, index: usize) -> Option<f32> {
        let inner = self.state.lock();
        let geometry = Geometry::new(cx.rect(), cx.theme(), &inner);
        let x = geometry.x_of(inner.markers.get(index)?.frame as f64);
        (geometry.area.x..=geometry.area.right())
            .contains(&x)
            .then_some(x)
    }

    /// The channels' shapes for `geometry` at `scale`, from the cache if
    /// they were made for the same.
    fn shapes(&mut self, wave: &Arc<Waveform>, geometry: &Geometry, scale: f32) -> &[Shape] {
        let key = CacheKey {
            wave: Arc::as_ptr(wave) as usize,
            start: geometry.start,
            end: geometry.end,
            area: geometry.area,
            scale,
        };
        if self.cache.as_ref().is_none_or(|(kept, _)| *kept != key) {
            let lanes = lanes(geometry.area, wave.channel_count(), scale);
            let count = (geometry.area.width * scale).round().max(0.0) as usize;
            let per_column = if count > 0 {
                geometry.span() / count as f64
            } else {
                0.0
            };
            let shapes = lanes
                .iter()
                .enumerate()
                .map(|(channel, lane)| {
                    if per_column >= 1.0 {
                        Shape::Columns(columns(wave, channel, geometry.start, geometry.end, count))
                    } else {
                        Shape::Line(sample_line(wave, channel, geometry, *lane))
                    }
                })
                .collect();
            self.cache = Some((key, shapes));
        }
        self.cache.as_ref().map_or(&[], |(_, shapes)| shapes)
    }
}

/// The lanes of `count` channels in `area`, their edges on device pixels.
fn lanes(area: Rect, count: usize, scale: f32) -> Vec<Rect> {
    let count = count.max(1);
    (0..count)
        .map(|lane| {
            let top = snap_px(area.y + area.height * lane as f32 / count as f32, scale);
            let bottom = snap_px(
                area.y + area.height * (lane + 1) as f32 / count as f32,
                scale,
            );
            Rect::new(area.x, top, area.width, (bottom - top).max(0.0))
        })
        .collect()
}

/// A line through the samples of `channel` that `geometry` shows, in
/// `lane`: each sample at the middle of its frame.
fn sample_line(wave: &Waveform, channel: usize, geometry: &Geometry, lane: Rect) -> Path {
    let samples = wave.channel(channel);
    let first = geometry.start.floor().max(0.0) as usize;
    let last = (geometry.end.ceil() as usize + 1).min(samples.len());
    let mid = lane.y + lane.height * 0.5;
    let half = lane.height * 0.5;
    let mut path = Path::with_capacity(last.saturating_sub(first) + 1);
    for (step, frame) in (first..last).enumerate() {
        let point = Point::new(
            geometry.x_of(frame as f64 + 0.5),
            mid - samples[frame].clamp(-1.0, 1.0) * half,
        );
        if step == 0 {
            path.move_to(point);
        } else {
            path.line_to(point);
        }
    }
    path
}

impl Widget for WaveformView {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let inner = self.state.lock();
        let value = match &inner.waveform {
            None => "no sample".to_owned(),
            Some(wave) => {
                let (start, end) = inner.span();
                let seconds = |frame: f64| frame / wave.sample_rate();
                let mut value = format!(
                    "{}{:.2} s, {} channel{}; showing {:.2} s to {:.2} s",
                    if wave.name().is_empty() {
                        String::new()
                    } else {
                        format!("{}, ", wave.name())
                    },
                    wave.seconds(),
                    wave.channel_count(),
                    if wave.channel_count() == 1 { "" } else { "s" },
                    seconds(start),
                    seconds(end),
                );
                if let Some(selection) = &inner.selection {
                    value.push_str(&format!(
                        "; selected {:.3} s to {:.3} s",
                        seconds(selection.start as f64),
                        seconds(selection.end as f64)
                    ));
                }
                if let Some(playhead) = inner.playhead {
                    value.push_str(&format!("; playing at {:.3} s", seconds(playhead as f64)));
                }
                value
            }
        };
        AccessNode::new(Role::Group, self.name.as_str())
            .with_value(value)
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let inner = self.state.lock();
        let rect = cx.rect();
        let geometry = Geometry::new(rect, cx.theme(), &inner);
        let len = inner.len();
        for (index, marker) in inner.markers.iter().enumerate() {
            let x = geometry
                .x_of(marker.frame as f64)
                .clamp(rect.x, rect.right());
            let left = (x - GRAB).max(rect.x);
            let right = (x + GRAB).min(rect.right());
            let bounds = Rect::new(left, rect.y, (right - left).max(0.0), rect.height);
            let mut node = AccessNode::new(Role::Slider, marker.name.as_str())
                .with_value(frame_text(inner.waveform.as_deref(), marker.frame))
                .with_numeric(
                    marker.frame as f64,
                    AccessRange::new(0.0, len as f64, Some(1.0)),
                )
                .with_actions(Actions::ADJUST);
            node.state.focused = cx.is_focused() && index == self.cursor;
            items.push(AccessItem::new(index, node, bounds));
        }
    }

    fn measure(&mut self, cx: &MeasureCx<'_>) -> Size {
        let theme = cx.theme();
        Size::new(
            theme.sizes.slider_width * 2.0,
            theme.sizes.meter_height + 16.0,
        )
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.state.version();
        if self.version != Some(version) {
            self.version = Some(version);
            cx.request_paint();
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let scale = cx.scale();
        let palette = &theme.palette;
        let ring = RoundedRect::new(
            Rect::new(
                rect.x + 1.5,
                rect.y + 1.5,
                (rect.width - 3.0).max(0.0),
                (rect.height - 3.0).max(0.0),
            ),
            theme.corners.lg,
        );
        let (wave, geometry, selection, markers, playhead) = {
            let inner = self.state.lock();
            (
                inner.waveform.clone(),
                Geometry::new(rect, theme, &inner),
                inner.selection.clone(),
                inner.markers.clone(),
                inner.playhead,
            )
        };
        let style = value_style(theme).with_align(Align::Start);
        let Some(wave) = wave.filter(|wave| !wave.is_empty()) else {
            let text = line("No sample", &style.with_align(Align::Center), rect.width);
            let y = rect.y + (rect.height - text.size().height) * 0.5;
            scene.text(&text, Point::new(rect.x, y));
            focus_ring(scene, ring, theme, cx.focus_ring());
            return;
        };
        let area = geometry.area;
        let lanes = lanes(area, wave.channel_count(), scale);
        let focused = cx.is_focused();
        let cursor = self.cursor;
        let shapes = self.shapes(&wave, &geometry, scale);
        // The loop, a faint band of its markers' colour between them.
        let looped = {
            let mut frames = markers
                .iter()
                .filter(|marker| marker.kind == MarkerKind::Loop)
                .map(|marker| marker.frame);
            match (frames.next(), frames.next()) {
                (Some(a), Some(b)) => Some((a.min(b), a.max(b))),
                _ => None,
            }
        };
        let loop_color = marker_color(theme, MarkerKind::Loop);
        scene.clipped(area, |scene| {
            if let Some((from, to)) = looped {
                let left = geometry.x_of(from as f64);
                let right = geometry.x_of(to as f64);
                scene.fill(
                    RoundedRect::new(
                        Rect::new(left, area.y, (right - left).max(0.0), area.height),
                        theme.corners.md,
                    ),
                    loop_color.with_alpha(0x12),
                );
            }
            if let Some(selection) = &selection {
                let left = snap_px(geometry.x_of(selection.start as f64), scale);
                let right = snap_px(geometry.x_of(selection.end as f64), scale);
                scene.fill(
                    Rect::new(left, area.y, (right - left).max(0.0), area.height),
                    selection_color(theme),
                );
            }
            for (lane, shape) in lanes.iter().zip(shapes) {
                let mid = lane.y + lane.height * 0.5;
                hline(
                    scene,
                    mid,
                    (lane.x, lane.right()),
                    1.0,
                    scale,
                    palette.hairline_strong,
                );
                match shape {
                    Shape::Columns(columns) => {
                        // The span, deepest at the edges and brightest in
                        // the middle, and its core, brighter still.
                        let edge = palette.accent.with_alpha(0x8C);
                        let middle = palette
                            .accent
                            .lerp(palette.accent_bright, 0.6)
                            .with_alpha(0xE6);
                        let stops = [
                            Stop::new(0.0, edge),
                            Stop::new(0.5, middle),
                            Stop::new(1.0, edge),
                        ];
                        let span = LinearGradient {
                            start: Point::new(lane.x, lane.y),
                            end: Point::new(lane.x, lane.bottom()),
                            stops: &stops,
                            extend: Extend::Pad,
                        };
                        scene.columns(*lane, &columns.low, &columns.high, span);
                        scene.columns(
                            *lane,
                            &columns.core_low,
                            &columns.core_high,
                            palette.accent_bright.with_alpha(0xF2),
                        );
                    }
                    Shape::Line(path) => {
                        scene.stroke(path, Stroke::round(1.5), palette.accent_bright);
                    }
                }
            }
            for lane in lanes.iter().skip(1) {
                hline(
                    scene,
                    lane.y,
                    (lane.x, lane.right()),
                    1.0,
                    scale,
                    palette.hairline,
                );
            }
        });
        // Markers, over the lanes, with their names in the strip.
        let mut xs: Vec<f32> = markers
            .iter()
            .map(|marker| snap_px(geometry.x_of(marker.frame as f64), scale))
            .collect();
        xs.sort_by(f32::total_cmp);
        for (index, marker) in markers.iter().enumerate() {
            let x = snap_px(geometry.x_of(marker.frame as f64), scale);
            if !(rect.x..=rect.right()).contains(&x) {
                continue;
            }
            let color = marker_color(theme, marker.kind);
            let lit = focused && index == cursor;
            let pixels = if lit { 2.0 } else { 1.0 };
            // A marker at the very end is drawn just inside.
            let x = x.min(rect.right() - super::device_px(pixels, scale));
            vline(scene, x, (rect.y, rect.bottom()), pixels, scale, color);
            // Its name on a pill of its colour at the top: right of the
            // line if there is room before the next marker, left of it if
            // not.
            let next = xs
                .iter()
                .copied()
                .find(|&other| other > x + 0.5)
                .unwrap_or(rect.right());
            let chip_style = theme
                .typography
                .chip
                .with_color(palette.background)
                .with_align(Align::Start);
            let text = line(&marker.name, &chip_style, 120.0);
            let width = text.size().width + 10.0;
            let strip = geometry.strip;
            let height = (strip.height - 1.0).max(0.0);
            let left = if next - x >= width + 4.0 {
                x
            } else {
                (x - width + super::device_px(pixels, scale)).max(rect.x)
            };
            let chip = Rect::new(left, strip.y, width, height);
            scene.fill(RoundedRect::new(chip, height * 0.5), color);
            if lit {
                focus_ring(
                    scene,
                    RoundedRect::new(chip, height * 0.5),
                    theme,
                    cx.focus_ring(),
                );
            }
            let y = strip.y + (height - text.size().height) * 0.5;
            // The chip is as wide as its text.
            scene.text(&text, Point::new(left + 5.0, y));
        }
        if let Some(playhead) = playhead {
            let x = geometry.x_of(playhead as f64);
            if (area.x..area.right()).contains(&x) {
                vline(
                    scene,
                    x,
                    (area.y, area.bottom()),
                    2.0,
                    scale,
                    palette.thumb.with_alpha(0xC0),
                );
            }
        }
        focus_ring(scene, ring, theme, cx.focus_ring());
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                ..
            } => self.press(cx, at),
            Event::PointerMove { at, .. } => self.pointer_move(cx, at),
            Event::PointerUp { .. } => self.release(cx),
            Event::PointerCancel => {
                self.cancel(cx);
                Handled::No
            }
            Event::Wheel {
                at,
                delta,
                modifiers,
            } => {
                if self.state.lock().len() == 0 || !(delta.x.is_finite() && delta.y.is_finite()) {
                    return Handled::No;
                }
                if modifiers.control || modifiers.meta {
                    self.zoom(cx, ZOOM_STEP.powf(f64::from(delta.y)), Some(at.x));
                    return Handled::Yes;
                }
                let pixels = (delta.x - delta.y) * cx.theme().sizes.scroll_line;
                if pixels != 0.0 && self.scroll(cx, pixels) {
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            Event::Key(KeyEvent {
                key,
                modifiers,
                pressed: true,
            }) => {
                if self.press.is_some() {
                    return if key == Key::Escape && self.cancel(cx) {
                        Handled::Yes
                    } else {
                        Handled::No
                    };
                }
                if cx.is_focused() && !modifiers.alt {
                    self.key(cx, key, modifiers)
                } else {
                    Handled::No
                }
            }
            _ => Handled::No,
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        if key >= self.state.lock().markers.len() {
            return false;
        }
        match action {
            Action::Focus => {
                self.cursor = key;
                cx.request_paint();
            }
            Action::Increment | Action::Decrement => {
                self.step(cx, key, action == Action::Increment, false);
            }
            Action::SetValue(frame) if frame.is_finite() => {
                let frame = frame.round().max(0.0) as u64;
                let within = {
                    let inner = self.state.lock();
                    let geometry = Geometry::new(cx.rect(), cx.theme(), &inner);
                    (geometry.per_pixel() * SNAP_PIXELS).ceil().max(1.0) as u64
                };
                self.move_to(cx, key, frame, Some(within));
            }
            _ => return false,
        }
        true
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, _focused: bool) {
        cx.request_paint();
    }
}

impl std::fmt::Debug for WaveformView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaveformView")
            .field("name", &self.name)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
