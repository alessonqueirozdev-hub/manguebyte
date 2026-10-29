//! A map of zones over keys and velocities, as a sampler lays its samples
//! out.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_gfx::{Align, Point, Rect, RoundedRect, Scene, Size, Stroke};

use super::keys::{KeySpan, is_black, note_name};
use super::{focus_ring, glow_fill, hline, line, line_height, snap_px, vline};
use crate::access::{AccessItem, AccessNode, Action, Actions, Role};
use crate::drag::{DragEvent, DragPayload};
use crate::event::{Button, DropData, Event, Handled, Key, KeyEvent, Modifiers};
use crate::motion::{Animated, Lerp};
use crate::theme::Theme;
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// How near an edge of a zone, in logical pixels, a press takes the edge
/// rather than the whole zone.
const EDGE: f32 = 4.0;

/// How far the pointer moves before a press becomes a drag.
const DRAG_START: f32 = 3.0;

/// The highest velocity.
const TOP: u8 = 127;

/// A rectangle of keys and velocities that plays one sample, as a
/// [`ZoneMap`] shows it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Zone {
    /// The application's name for it, unique in a map.
    pub id: u64,
    /// What it is called, such as the sample's name.
    pub name: String,
    /// The lowest key it plays on.
    pub low_key: u8,
    /// The highest key it plays on.
    pub high_key: u8,
    /// The lowest velocity it plays at, from 0.
    pub low_velocity: u8,
    /// The highest velocity it plays at, to 127.
    pub high_velocity: u8,
    /// The key that plays the sample at its own pitch, if any.
    pub root: Option<u8>,
    /// Which of the theme's categorical [`hues`](crate::Palette::hues) it
    /// is drawn in: the same for zones of the same sample. `None` takes
    /// one from the id.
    pub hue: Option<u8>,
}

impl Zone {
    /// Zone `id` called `name` on the keys from `low_key` to `high_key`,
    /// in either order, at every velocity, with no root.
    pub fn new(id: u64, name: impl Into<String>, low_key: u8, high_key: u8) -> Zone {
        let (low, high) = (low_key.min(127), high_key.min(127));
        Zone {
            id,
            name: name.into(),
            low_key: low.min(high),
            high_key: low.max(high),
            low_velocity: 0,
            high_velocity: TOP,
            root: None,
            hue: None,
        }
    }

    /// The same zone drawn in the theme's categorical hue `index` (taken
    /// modulo the number of hues).
    #[must_use]
    pub fn with_hue(mut self, index: u8) -> Zone {
        self.hue = Some(index);
        self
    }

    /// The same zone at the velocities from `low` to `high`, in either
    /// order.
    #[must_use]
    pub fn with_velocities(mut self, low: u8, high: u8) -> Zone {
        let (low, high) = (low.min(TOP), high.min(TOP));
        self.low_velocity = low.min(high);
        self.high_velocity = low.max(high);
        self
    }

    /// The same zone with `root` as its root key.
    #[must_use]
    pub fn with_root(mut self, root: u8) -> Zone {
        self.root = Some(root.min(127));
        self
    }

    /// Whether it plays `key` at `velocity`.
    pub fn contains(&self, key: u8, velocity: u8) -> bool {
        (self.low_key..=self.high_key).contains(&key)
            && (self.low_velocity..=self.high_velocity).contains(&velocity)
    }

    /// Whether it and `other` play some key at some velocity both.
    pub fn overlaps(&self, other: &Zone) -> bool {
        self.low_key <= other.high_key
            && other.low_key <= self.high_key
            && self.low_velocity <= other.high_velocity
            && other.low_velocity <= self.high_velocity
    }
}

/// Something that happened in a zone map, for the application to act on;
/// it takes them with [`ZoneMapState::take_events`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ZoneEvent {
    /// A person moved or resized these zones, by a drag that ended, a key
    /// or assistive technology. Their new places are in the state.
    Edited(Vec<u64>),
    /// A drop made these zones, one a key from where it landed, from what
    /// `payload` carried: the application ties them to its samples.
    Created {
        /// The new zones, in the order of the names the payload gave.
        ids: Vec<u64>,
        /// What was dropped.
        payload: DragPayload,
    },
    /// A person deleted these zones; they are gone from the state.
    Removed(Vec<Zone>),
}

#[derive(Debug, Default)]
struct ZoneInner {
    zones: Vec<Zone>,
    selected: Vec<u64>,
    next_id: u64,
    events: Vec<ZoneEvent>,
    version: u64,
}

impl ZoneInner {
    fn is_selected(&self, id: u64) -> bool {
        self.selected.contains(&id)
    }

    fn select_only(&mut self, ids: impl IntoIterator<Item = u64>) {
        self.selected.clear();
        self.selected.extend(ids);
    }

    fn toggle(&mut self, id: u64) {
        match self.selected.iter().position(|&kept| kept == id) {
            Some(at) => {
                self.selected.remove(at);
            }
            None => self.selected.push(id),
        }
    }

    fn fresh_id(&mut self) -> u64 {
        let in_use = self.zones.iter().map(|zone| zone.id + 1).max().unwrap_or(0);
        let id = self.next_id.max(in_use);
        self.next_id = id + 1;
        id
    }
}

/// What a [`ZoneMap`] shares with the application: the zones, which are
/// selected, and what a person did to them.
///
/// Both read and write it; its [`version`](ZoneMapState::version) counts
/// every change. It is `Send`, so a plugin's editor spec can keep it.
///
/// ```
/// use manguebyte_ui::{Zone, ZoneMapState};
///
/// let state = ZoneMapState::new();
/// state.set_zones(vec![Zone::new(7, "Kick", 36, 36).with_root(36)]);
/// let pad = state.add_zone(Zone::new(0, "Pad", 48, 72).with_velocities(0, 90));
/// assert_eq!(pad, 8, "a fresh id, whatever the one given");
/// state.set_selected([pad]);
/// assert_eq!(state.zone(pad).map(|z| z.high_velocity), Some(90));
/// assert!(state.is_selected(pad) && !state.is_selected(7));
/// ```
#[derive(Clone, Default)]
pub struct ZoneMapState {
    inner: Arc<Mutex<ZoneInner>>,
}

impl ZoneMapState {
    /// No zones.
    pub fn new() -> ZoneMapState {
        ZoneMapState::default()
    }

    fn lock(&self) -> MutexGuard<'_, ZoneInner> {
        // Nothing that can panic runs while the lock is held.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change<R>(&self, change: impl FnOnce(&mut ZoneInner) -> R) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        inner.version = inner.version.wrapping_add(1);
        result
    }

    /// A number that changes with every change to the state.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// The zones, in the order they are drawn: later ones over earlier.
    pub fn zones(&self) -> Vec<Zone> {
        self.lock().zones.clone()
    }

    /// The zone `id`, if there is one.
    pub fn zone(&self, id: u64) -> Option<Zone> {
        self.lock().zones.iter().find(|zone| zone.id == id).cloned()
    }

    /// Replaces the zones; the selection keeps the ids still there.
    pub fn set_zones(&self, zones: Vec<Zone>) {
        self.change(|inner| {
            inner.zones = zones;
            let ZoneInner {
                zones, selected, ..
            } = inner;
            selected.retain(|id| zones.iter().any(|zone| zone.id == *id));
        });
    }

    /// Adds `zone` over the others with an id no zone has had, and
    /// returns the id.
    pub fn add_zone(&self, mut zone: Zone) -> u64 {
        self.change(|inner| {
            zone.id = inner.fresh_id();
            let id = zone.id;
            inner.zones.push(zone);
            id
        })
    }

    /// The ids of the zones selected, in the order they were selected.
    pub fn selected(&self) -> Vec<u64> {
        self.lock().selected.clone()
    }

    /// Whether zone `id` is selected.
    pub fn is_selected(&self, id: u64) -> bool {
        self.lock().is_selected(id)
    }

    /// Selects the zones `ids` and no others.
    pub fn set_selected(&self, ids: impl IntoIterator<Item = u64>) {
        self.change(|inner| {
            let ids: Vec<u64> = ids
                .into_iter()
                .filter(|id| inner.zones.iter().any(|zone| zone.id == *id))
                .collect();
            inner.select_only(ids);
        });
    }

    /// What happened in the map since the application last asked, oldest
    /// first, forgetting it.
    pub fn take_events(&self) -> Vec<ZoneEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

impl std::fmt::Debug for ZoneMapState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("ZoneMapState")
            .field("zones", &inner.zones.len())
            .field("selected", &inner.selected)
            .field("version", &inner.version)
            .finish_non_exhaustive()
    }
}

/// Which edges of a zone a press holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    fn any(self) -> bool {
        self.left || self.right || self.top || self.bottom
    }
}

/// What a press took hold of.
#[derive(Debug, Clone)]
enum Hold {
    /// The zones selected, to move them.
    Move,
    /// Edges of one zone, to resize it.
    Resize { id: u64, edges: Edges },
    /// Nothing: a band selects what it crosses; `kept` stays selected.
    Band { kept: Vec<u64> },
}

#[derive(Debug, Clone)]
struct Press {
    at: Point,
    hold: Hold,
    /// The zones and the selection when it began, to move from and to
    /// put back.
    zones: Vec<Zone>,
    selected: Vec<u64>,
    /// A press on a zone already selected: on release without a drag, it
    /// becomes the only one selected.
    narrow: Option<u64>,
    moved: bool,
    /// Where the band reaches, while one is drawn.
    band: Option<Point>,
}

/// What a drop would make, while one is over the map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DropAt {
    key: u8,
    count: usize,
}

type Takes = Box<dyn Fn(&DragPayload) -> Option<Vec<String>>>;
type Names = Box<dyn Fn(u8) -> String>;

/// What a map takes from a drag unless told otherwise: files dropped from
/// other programs, a zone per file, named by its file name.
fn files(payload: &DragPayload) -> Option<Vec<String>> {
    let data = payload.get::<DropData>()?;
    let names: Vec<String> = data
        .files
        .iter()
        .map(|file| {
            file.file_name().map_or_else(
                || file.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            )
        })
        .collect();
    (!names.is_empty()).then_some(names)
}

/// A map of zones over keys, left to right, and velocities, 0 at the
/// bottom to 127 at the top, as a sampler lays its samples out.
///
/// The keys are laid out over a [`KeySpan`] as a
/// [`PianoKeyboard`](crate::PianoKeyboard) lays its keys out, a column
/// each, so a map placed above or below a keyboard over the same span and
/// the same width lines up with it key for key. Columns of black keys are
/// shaded, and a line marks each C.
///
/// - A click selects a zone; Ctrl (Command on macOS) and a click adds or
///   takes away one, Shift and a click adds one. A drag on a zone moves
///   every zone selected, by whole keys and whole velocities, all kept on
///   the map; a drag on an edge or a corner resizes that zone, its edges
///   landing between keys and between velocities. A drag on nothing
///   selects the zones it crosses. Escape during a drag puts everything
///   back.
/// - With the keyboard focus: Page Down and Page Up select the next and
///   previous zone; the arrows move the zones selected by a key or a
///   velocity (with Shift by an octave or ten); Alt and the arrows move
///   their top or right edge, Alt, Shift and the arrows their bottom or
///   left edge; Delete removes them; Ctrl+A selects every zone and Escape
///   none.
/// - Zones that overlap are drawn with their overlap marked in the theme's
///   warning colour, and say so to assistive technology.
/// - Drags it [takes](ZoneMap::takes) (by default, files dropped from other
///   programs) make zones where they land: one a key from the key under
///   the pointer for each name the drag gives, at every velocity, rooted
///   on its key, and selected.
///
/// Every change is in the [`ZoneMapState`], and each finished edit, drop
/// and deletion is a [`ZoneEvent`]: the application polls, nothing calls
/// it back. To assistive technology the map is a [`Role::List`] and each
/// zone a [`Role::ListItem`] whose value gives its keys, velocities, root
/// and overlaps, which can be pressed to select it.
pub struct ZoneMap {
    name: String,
    span: KeySpan,
    state: ZoneMapState,
    takes: Takes,
    names: Names,
    press: Option<Press>,
    drop_at: Option<DropAt>,
    /// The zone the keyboard is on.
    cursor: Option<u64>,
    version: Option<u64>,
    /// The zone under the pointer and how far its hover has come in, and
    /// the zone it left, fading out.
    hover_zone: Option<(u64, Animated<f32>)>,
    fading: Option<(u64, Animated<f32>)>,
}

/// A map called `name` of the zones `state` holds, over `span`.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{KeySpan, Role, Zone, ZoneEvent, ZoneMapState, zone_map};
///
/// let state = ZoneMapState::new();
/// state.set_zones(vec![Zone::new(1, "Bass", 48, 52).with_root(48)]);
/// // Two octaves across 240 pixels: ten pixels a key.
/// let view = zone_map("Zones", KeySpan::new(48, 71), state.clone());
/// let mut ui = Harness::new(view, Size::new(240.0, 128.0), 1.0);
/// ui.frame();
/// let bass = ui.get(Role::ListItem, "Bass");
/// assert_eq!(bass.value_text(), Some("C3 to E3, velocity 0 to 127, root C3"));
/// // Dragged 31 pixels right: three keys, and selected.
/// ui.drag(bass.center(), (31.0, 0.0), 3);
/// let moved = state.zone(1).expect("still there");
/// assert_eq!((moved.low_key, moved.high_key, moved.root), (51, 55, Some(51)));
/// assert!(matches!(state.take_events().as_slice(), [ZoneEvent::Edited(ids)] if ids == &[1]));
/// ```
pub fn zone_map(name: impl Into<String>, span: KeySpan, state: ZoneMapState) -> ZoneMap {
    ZoneMap {
        name: name.into(),
        span,
        state,
        takes: Box::new(files),
        names: Box::new(note_name),
        press: None,
        drop_at: None,
        cursor: None,
        version: None,
        hover_zone: None,
        fading: None,
    }
}

/// Whether Ctrl, or Command on macOS, is held.
fn command(modifiers: Modifiers) -> bool {
    modifiers.control || modifiers.meta
}

/// `value` moved by `by`, within `low..=high`.
fn offset(value: u8, by: i32, low: u8, high: u8) -> u8 {
    (i32::from(value) + by).clamp(i32::from(low), i32::from(high)) as u8
}

impl ZoneMap {
    /// The same map taking the drags `takes` gives names for: the names
    /// of the zones a drop makes, one a key, or `None` to refuse the drag.
    /// By default the map takes files dropped from other programs, a zone
    /// per file named by its file name.
    #[must_use]
    pub fn takes(
        mut self,
        takes: impl Fn(&DragPayload) -> Option<Vec<String>> + 'static,
    ) -> ZoneMap {
        self.takes = Box::new(takes);
        self
    }

    /// The same map naming keys with `names` (see
    /// [`PianoKeyboard::note_names`](crate::PianoKeyboard::note_names)).
    #[must_use]
    pub fn note_names(mut self, names: impl Fn(u8) -> String + 'static) -> ZoneMap {
        self.names = Box::new(names);
        self
    }

    /// The state it shares with the application.
    pub fn state(&self) -> &ZoneMapState {
        &self.state
    }

    /// The height of one velocity in `rect`.
    fn row(rect: Rect) -> f32 {
        rect.height / 128.0
    }

    /// Where `zone` is drawn in `rect`.
    fn zone_rect(span: KeySpan, rect: Rect, zone: &Zone) -> Option<Rect> {
        let low = zone.low_key.max(span.low());
        let high = zone.high_key.min(span.high());
        if low > high {
            return None;
        }
        let first = span.column(rect, low)?;
        let last = span.column(rect, high)?;
        let row = Self::row(rect);
        let top = rect.y + f32::from(TOP - zone.high_velocity) * row;
        let bottom = rect.y + f32::from(TOP + 1 - zone.low_velocity) * row;
        Some(Rect::new(
            first.x,
            top,
            last.right() - first.x,
            bottom - top,
        ))
    }

    /// The zone at `at` and the edges of it near `at`, the zone drawn last
    /// first.
    fn zone_at(&self, rect: Rect, zones: &[Zone], at: Point) -> Option<(u64, Edges)> {
        // Zones the pointer is inside come before those whose edge it is
        // just outside of.
        let inside = zones.iter().rev().filter(|zone| {
            Self::zone_rect(self.span, rect, zone).is_some_and(|area| area.contains(at))
        });
        let near = zones.iter().rev().filter(|zone| {
            Self::zone_rect(self.span, rect, zone)
                .is_some_and(|area| !area.contains(at) && area.outset(EDGE).contains(at))
        });
        for zone in inside.chain(near) {
            let Some(area) = Self::zone_rect(self.span, rect, zone) else {
                continue;
            };
            // A narrow zone keeps its middle third for moving it.
            let (across, down) = (EDGE.min(area.width / 3.0), EDGE.min(area.height / 3.0));
            let near = |a: f32, b: f32, reach: f32| (a - b).abs() <= reach;
            let (left, right) = (near(at.x, area.x, across), near(at.x, area.right(), across));
            let (top, bottom) = (near(at.y, area.y, down), near(at.y, area.bottom(), down));
            // A zone too small to tell its edges apart: the nearer one.
            let edges = Edges {
                left: left && (!right || (at.x - area.x).abs() < (at.x - area.right()).abs()),
                right: right && (!left || (at.x - area.x).abs() >= (at.x - area.right()).abs()),
                top: top && (!bottom || (at.y - area.y).abs() < (at.y - area.bottom()).abs()),
                bottom: bottom && (!top || (at.y - area.y).abs() >= (at.y - area.bottom()).abs()),
            };
            if edges.any() || area.contains(at) {
                return Some((zone.id, edges));
            }
        }
        None
    }

    fn press(&mut self, cx: &mut EventCx<'_>, at: Point, modifiers: Modifiers) -> Handled {
        cx.request_focus();
        cx.request_paint();
        cx.capture_pointer();
        let rect = cx.rect();
        let (zones, selected) = {
            let inner = self.state.lock();
            (inner.zones.clone(), inner.selected.clone())
        };
        let hit = self.zone_at(rect, &zones, at);
        let mut narrow = None;
        let hold = match hit {
            Some((id, edges)) => {
                self.cursor = Some(id);
                self.state.change(|inner| {
                    if command(modifiers) {
                        inner.toggle(id);
                    } else if modifiers.shift {
                        if !inner.is_selected(id) {
                            inner.selected.push(id);
                        }
                    } else if inner.is_selected(id) {
                        narrow = Some(id);
                    } else {
                        inner.select_only([id]);
                    }
                });
                if edges.any() {
                    Hold::Resize { id, edges }
                } else {
                    Hold::Move
                }
            }
            None => {
                let kept = if command(modifiers) || modifiers.shift {
                    selected.clone()
                } else {
                    self.state.change(|inner| inner.selected.clear());
                    Vec::new()
                };
                Hold::Band { kept }
            }
        };
        self.press = Some(Press {
            at,
            hold,
            zones,
            selected,
            narrow,
            moved: false,
            band: None,
        });
        Handled::Yes
    }

    fn pointer_move(&mut self, cx: &mut EventCx<'_>, at: Point) -> Handled {
        let rect = cx.rect();
        let span = self.span;
        let Some(press) = &mut self.press else {
            return Handled::No;
        };
        if !press.moved && (at.x - press.at.x).hypot(at.y - press.at.y) < DRAG_START {
            return Handled::Yes;
        }
        press.moved = true;
        let column = span.column_width(rect).max(f32::EPSILON);
        let row = Self::row(rect).max(f32::EPSILON);
        match &press.hold {
            Hold::Move => {
                let moving: Vec<&Zone> = press
                    .zones
                    .iter()
                    .filter(|zone| self.state.is_selected(zone.id))
                    .collect();
                if moving.is_empty() {
                    return Handled::Yes;
                }
                let keys = ((at.x - press.at.x) / column).round() as i32;
                let velocities = ((press.at.y - at.y) / row).round() as i32;
                let lowest = moving.iter().map(|zone| zone.low_key).min().unwrap_or(0);
                let highest = moving.iter().map(|zone| zone.high_key).max().unwrap_or(0);
                let softest = moving
                    .iter()
                    .map(|zone| zone.low_velocity)
                    .min()
                    .unwrap_or(0);
                let loudest = moving
                    .iter()
                    .map(|zone| zone.high_velocity)
                    .max()
                    .unwrap_or(0);
                let keys = keys.clamp(
                    i32::from(span.low()) - i32::from(lowest),
                    (i32::from(span.high()) - i32::from(highest))
                        .max(i32::from(span.low()) - i32::from(lowest)),
                );
                let velocities =
                    velocities.clamp(-i32::from(softest), i32::from(TOP) - i32::from(loudest));
                let moved: Vec<Zone> = moving
                    .iter()
                    .map(|zone| {
                        let mut zone = (*zone).clone();
                        zone.low_key = offset(zone.low_key, keys, 0, 127);
                        zone.high_key = offset(zone.high_key, keys, 0, 127);
                        zone.low_velocity = offset(zone.low_velocity, velocities, 0, TOP);
                        zone.high_velocity = offset(zone.high_velocity, velocities, 0, TOP);
                        zone.root = zone.root.map(|root| offset(root, keys, 0, 127));
                        zone
                    })
                    .collect();
                let original = press.zones.clone();
                self.state.change(|inner| {
                    inner.zones = original;
                    for zone in moved {
                        if let Some(kept) = inner.zones.iter_mut().find(|kept| kept.id == zone.id) {
                            *kept = zone;
                        }
                    }
                });
            }
            Hold::Resize { id, edges } => {
                let (id, edges) = (*id, *edges);
                let Some(before) = press.zones.iter().find(|zone| zone.id == id).cloned() else {
                    return Handled::Yes;
                };
                let mut zone = before.clone();
                let boundary = ((at.x - rect.x) / column)
                    .round()
                    .clamp(0.0, span.len() as f32) as i32;
                let level = ((at.y - rect.y) / row).round().clamp(0.0, 128.0) as i32;
                let key = |boundary: i32| i32::from(span.low()) + boundary;
                if edges.left {
                    zone.low_key = key(boundary)
                        .clamp(i32::from(span.low()), i32::from(before.high_key))
                        as u8;
                }
                if edges.right {
                    zone.high_key = (key(boundary) - 1)
                        .clamp(i32::from(before.low_key), i32::from(span.high()))
                        as u8;
                }
                if edges.top {
                    zone.high_velocity = (i32::from(TOP) - level)
                        .clamp(i32::from(before.low_velocity), i32::from(TOP))
                        as u8;
                }
                if edges.bottom {
                    zone.low_velocity = (i32::from(TOP) + 1 - level)
                        .clamp(0, i32::from(before.high_velocity))
                        as u8;
                }
                self.state.change(|inner| {
                    if let Some(kept) = inner.zones.iter_mut().find(|kept| kept.id == id) {
                        *kept = zone;
                    }
                });
            }
            Hold::Band { kept } => {
                press.band = Some(at);
                let band = Rect::from_points(press.at, at);
                let crossed: Vec<u64> = press
                    .zones
                    .iter()
                    .filter(|zone| {
                        Self::zone_rect(self.span, rect, zone)
                            .is_some_and(|area| area.intersect(&band).is_some())
                    })
                    .map(|zone| zone.id)
                    .collect();
                let mut chosen = kept.clone();
                chosen.extend(crossed.into_iter().filter(|id| !kept.contains(id)));
                self.state.change(|inner| inner.select_only(chosen));
            }
        }
        cx.request_paint();
        Handled::Yes
    }

    fn release(&mut self, cx: &mut EventCx<'_>) -> Handled {
        let Some(press) = self.press.take() else {
            return Handled::No;
        };
        cx.release_pointer();
        cx.request_paint();
        self.state.change(|inner| {
            if press.moved {
                if matches!(press.hold, Hold::Move | Hold::Resize { .. }) {
                    let edited: Vec<u64> = inner
                        .zones
                        .iter()
                        .filter(|zone| !press.zones.contains(zone))
                        .map(|zone| zone.id)
                        .collect();
                    if !edited.is_empty() {
                        inner.events.push(ZoneEvent::Edited(edited));
                    }
                }
            } else if let Some(id) = press.narrow {
                inner.select_only([id]);
            }
        });
        Handled::Yes
    }

    /// Ends a press, putting the zones and the selection back.
    fn cancel(&mut self, cx: &mut EventCx<'_>) -> bool {
        let Some(press) = self.press.take() else {
            return false;
        };
        cx.release_pointer();
        cx.request_paint();
        self.state.change(|inner| {
            inner.zones = press.zones;
            inner.selected = press.selected;
        });
        true
    }

    /// Changes every zone selected with `edit`, and reports those that
    /// changed.
    fn edit_selected(&mut self, cx: &mut EventCx<'_>, edit: impl Fn(&mut Zone, &[Zone])) {
        self.state.change(|inner| {
            let selected: Vec<Zone> = inner
                .zones
                .iter()
                .filter(|zone| inner.selected.contains(&zone.id))
                .cloned()
                .collect();
            let mut edited = Vec::new();
            for zone in inner.zones.iter_mut() {
                if selected.iter().any(|chosen| chosen.id == zone.id) {
                    let before = zone.clone();
                    edit(zone, &selected);
                    if *zone != before {
                        edited.push(zone.id);
                    }
                }
            }
            if !edited.is_empty() {
                inner.events.push(ZoneEvent::Edited(edited));
            }
        });
        cx.request_paint();
    }

    fn key(&mut self, cx: &mut EventCx<'_>, key: Key, modifiers: Modifiers) -> Handled {
        let span = self.span;
        match key {
            Key::PageDown | Key::PageUp => {
                let mut order = self.state.zones();
                order.sort_by_key(|zone| (zone.low_key, zone.low_velocity, zone.id));
                if order.is_empty() {
                    return Handled::No;
                }
                let at = self
                    .cursor
                    .and_then(|id| order.iter().position(|zone| zone.id == id));
                let next = match (at, key) {
                    (None, Key::PageDown) => 0,
                    (None, _) => order.len() - 1,
                    (Some(at), Key::PageDown) => (at + 1).min(order.len() - 1),
                    (Some(at), _) => at.saturating_sub(1),
                };
                let id = order[next].id;
                self.cursor = Some(id);
                self.state.change(|inner| inner.select_only([id]));
                cx.request_paint();
            }
            Key::Character(c) if command(modifiers) && c.eq_ignore_ascii_case(&'a') => {
                self.state.change(|inner| {
                    let all: Vec<u64> = inner.zones.iter().map(|zone| zone.id).collect();
                    inner.select_only(all);
                });
                cx.request_paint();
            }
            Key::Escape => {
                if self.state.selected().is_empty() {
                    return Handled::No;
                }
                self.state.change(|inner| inner.selected.clear());
                cx.request_paint();
            }
            Key::Delete | Key::Backspace => {
                let removed = self.state.change(|inner| {
                    let (gone, kept): (Vec<Zone>, Vec<Zone>) = std::mem::take(&mut inner.zones)
                        .into_iter()
                        .partition(|zone| inner.selected.contains(&zone.id));
                    inner.zones = kept;
                    inner.selected.clear();
                    if !gone.is_empty() {
                        inner.events.push(ZoneEvent::Removed(gone.clone()));
                    }
                    !gone.is_empty()
                });
                if !removed {
                    return Handled::No;
                }
                cx.request_paint();
            }
            Key::Left | Key::Right | Key::Up | Key::Down => {
                if self.state.selected().is_empty() {
                    return Handled::No;
                }
                let sign = if matches!(key, Key::Right | Key::Up) {
                    1
                } else {
                    -1
                };
                let sideways = matches!(key, Key::Left | Key::Right);
                if modifiers.alt {
                    // An edge: the top or right one, or with Shift the
                    // bottom or left one.
                    let low = modifiers.shift;
                    self.edit_selected(cx, |zone, _| match (sideways, low) {
                        (true, false) => {
                            zone.high_key = offset(zone.high_key, sign, zone.low_key, span.high());
                        }
                        (true, true) => {
                            zone.low_key = offset(zone.low_key, sign, span.low(), zone.high_key);
                        }
                        (false, false) => {
                            zone.high_velocity =
                                offset(zone.high_velocity, sign, zone.low_velocity, TOP);
                        }
                        (false, true) => {
                            zone.low_velocity =
                                offset(zone.low_velocity, sign, 0, zone.high_velocity);
                        }
                    });
                } else {
                    let step = if modifiers.shift {
                        if sideways { 12 } else { 10 }
                    } else {
                        1
                    };
                    let by = sign * step;
                    self.edit_selected(cx, |zone, selected| {
                        if sideways {
                            let lowest = selected.iter().map(|z| z.low_key).min().unwrap_or(0);
                            let highest = selected.iter().map(|z| z.high_key).max().unwrap_or(0);
                            let by = by.clamp(
                                i32::from(span.low()) - i32::from(lowest),
                                (i32::from(span.high()) - i32::from(highest))
                                    .max(i32::from(span.low()) - i32::from(lowest)),
                            );
                            zone.low_key = offset(zone.low_key, by, 0, 127);
                            zone.high_key = offset(zone.high_key, by, 0, 127);
                            zone.root = zone.root.map(|root| offset(root, by, 0, 127));
                        } else {
                            let softest =
                                selected.iter().map(|z| z.low_velocity).min().unwrap_or(0);
                            let loudest =
                                selected.iter().map(|z| z.high_velocity).max().unwrap_or(0);
                            let by =
                                by.clamp(-i32::from(softest), i32::from(TOP) - i32::from(loudest));
                            zone.low_velocity = offset(zone.low_velocity, by, 0, TOP);
                            zone.high_velocity = offset(zone.high_velocity, by, 0, TOP);
                        }
                    });
                }
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    /// What a person reads of `zone`, among `zones`.
    fn describe(&self, zone: &Zone, zones: &[Zone]) -> String {
        let names = &self.names;
        let mut value = format!(
            "{} to {}, velocity {} to {}",
            names(zone.low_key),
            names(zone.high_key),
            zone.low_velocity,
            zone.high_velocity
        );
        if let Some(root) = zone.root {
            value.push_str(&format!(", root {}", names(root)));
        }
        let overlapping: Vec<&str> = zones
            .iter()
            .filter(|other| other.id != zone.id && other.overlaps(zone))
            .map(|other| other.name.as_str())
            .collect();
        if !overlapping.is_empty() {
            value.push_str(&format!(", overlaps {}", overlapping.join(", ")));
        }
        value
    }

    /// Paints the grid, faint: black keys' columns barely shaded, a line
    /// at each C and at every 32 velocities.
    fn paint_grid(&self, scene: &mut Scene, rect: Rect, theme: &Theme, scale: f32) {
        let palette = &theme.palette;
        for note in self.span.notes() {
            let Some(column) = self.span.column(rect, note) else {
                continue;
            };
            if is_black(note) {
                let left = snap_px(column.x, scale);
                let right = snap_px(column.right(), scale);
                scene.fill(
                    Rect::new(left, rect.y, (right - left).max(0.0), rect.height),
                    palette.track.with_opacity(0.2),
                );
            } else if note % 12 == 0 && note != self.span.low() {
                vline(
                    scene,
                    column.x,
                    (rect.y, rect.bottom()),
                    1.0,
                    scale,
                    palette.hairline_strong,
                );
            }
        }
        let row = Self::row(rect);
        for velocity in [32u8, 64, 96] {
            let y = rect.y + f32::from(TOP + 1 - velocity) * row;
            hline(
                scene,
                y,
                (rect.x, rect.right()),
                1.0,
                scale,
                palette.hairline,
            );
        }
    }

    /// The text under a zone's name: its keys, and its velocities when
    /// they are not all of them.
    fn range_text(&self, zone: &Zone) -> String {
        let names = &self.names;
        let mut text = if zone.low_key == zone.high_key {
            names(zone.low_key)
        } else {
            format!("{}\u{2013}{}", names(zone.low_key), names(zone.high_key))
        };
        if zone.low_velocity > 0 || zone.high_velocity < TOP {
            text.push_str(&format!(
                " \u{b7} {}\u{2013}{}",
                zone.low_velocity, zone.high_velocity
            ));
        }
        text
    }

    /// Moves the hover to zone `id`: the zone it leaves fades out.
    fn hover_to(&mut self, cx: &mut EventCx<'_>, id: Option<u64>) {
        if self.hover_zone.map(|(at, _)| at) == id {
            return;
        }
        let (now, fast) = (cx.now(), cx.motion().fast());
        if let Some((left, mut amount)) = self.hover_zone.take() {
            amount.transition(0.0, now, fast);
            self.fading = Some((left, amount));
        }
        self.hover_zone = id.map(|id| {
            let mut amount = Animated::new(0.0);
            amount.transition(1.0, now, fast);
            (id, amount)
        });
        cx.request_paint();
    }
}

/// The shape a zone is drawn as: its area a pixel inside each edge, so
/// that zones side by side read apart, with round corners.
fn zone_shape(area: Rect, theme: &Theme) -> RoundedRect {
    let inner = Rect::new(
        area.x + 1.0,
        area.y + 1.0,
        (area.width - 2.0).max(0.0),
        (area.height - 2.0).max(0.0),
    );
    let radius = theme
        .corners
        .md
        .min(inner.width * 0.5)
        .min(inner.height * 0.5)
        .max(0.0);
    RoundedRect::new(inner, radius)
}

/// `shape`'s rectangle less `by` on each side, its corners less `by`.
fn inset(shape: RoundedRect, by: f32) -> RoundedRect {
    let r = shape.rect;
    RoundedRect::new(
        Rect::new(
            r.x + by,
            r.y + by,
            (r.width - 2.0 * by).max(0.0),
            (r.height - 2.0 * by).max(0.0),
        ),
        (shape.radii.top_left - by).max(0.0),
    )
}

impl Widget for ZoneMap {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let inner = self.state.lock();
        AccessNode::new(Role::List, self.name.as_str())
            .with_value(format!(
                "{} zones, {} selected",
                inner.zones.len(),
                inner.selected.len()
            ))
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let rect = cx.rect();
        let inner = self.state.lock();
        for (index, zone) in inner.zones.iter().enumerate() {
            let Some(area) = Self::zone_rect(self.span, rect, zone) else {
                continue;
            };
            let mut node = AccessNode::new(Role::ListItem, zone.name.as_str())
                .with_value(self.describe(zone, &inner.zones))
                .with_selected(inner.is_selected(zone.id))
                .with_actions(Actions::PRESS);
            node.state.focused = cx.is_focused() && self.cursor == Some(zone.id);
            items.push(AccessItem::new(index, node, area));
        }
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(self.span.len() as f32 * 9.0, 128.0)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.state.version();
        if self.version != Some(version) {
            self.version = Some(version);
            cx.request_paint();
        }
        for (_, amount) in [&mut self.hover_zone, &mut self.fading]
            .into_iter()
            .flatten()
        {
            cx.animate(amount);
        }
        if self.fading.as_ref().is_some_and(|(_, a)| !a.is_running()) {
            self.fading = None;
        }
    }

    fn hover_moved(&mut self, cx: &mut EventCx<'_>, at: Point) {
        let zones = self.state.zones();
        let id = self.zone_at(cx.rect(), &zones, at).map(|(id, _)| id);
        self.hover_to(cx, id);
    }

    fn hover_changed(&mut self, cx: &mut EventCx<'_>, hovered: bool) {
        if !hovered {
            self.hover_to(cx, None);
        }
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let theme = cx.theme();
        let rect = cx.rect();
        let scale = cx.scale();
        let palette = &theme.palette;
        self.paint_grid(scene, rect, theme, scale);
        let (zones, selected) = {
            let inner = self.state.lock();
            (inner.zones.clone(), inner.selected.clone())
        };
        let areas: Vec<Option<Rect>> = zones
            .iter()
            .map(|zone| Self::zone_rect(self.span, rect, zone))
            .collect();
        let mut name_style = theme.typography.label.with_align(Align::Start);
        name_style.size = 11.0;
        let mut range_style = theme.typography.caption.with_align(Align::Start);
        range_style.size = 10.0;
        let (name_height, range_height) = (line_height(&name_style), line_height(&range_style));
        let (hover_zone, fading) = (self.hover_zone, self.fading);
        let hover_of = |id: u64| {
            [hover_zone, fading]
                .into_iter()
                .flatten()
                .find(|(at, _)| *at == id)
                .map_or(0.0, |(_, amount)| amount.value())
        };
        let hue_of = |zone: &Zone| {
            let index = zone.hue.map_or(zone.id as usize, usize::from);
            palette.hues[index % palette.hues.len()]
        };
        // The chosen zones glow under everything.
        for (zone, area) in zones.iter().zip(&areas) {
            if let (Some(area), true) = (*area, selected.contains(&zone.id)) {
                glow_fill(scene, zone_shape(area, theme), palette.accent, theme, 0.8);
            }
        }
        for (zone, area) in zones.iter().zip(&areas) {
            let Some(area) = *area else {
                continue;
            };
            let hue = hue_of(zone);
            let shape = zone_shape(area, theme);
            let hover = hover_of(zone.id);
            let chosen = selected.contains(&zone.id);
            // Its fill, stronger under the pointer and when chosen.
            let fill = hue
                .fill
                .lerp(hue.ring, if chosen { 0.35 } else { 0.25 * hover });
            scene.fill(shape, fill);
            if let Some(root) = zone
                .root
                .filter(|&root| zone.low_key <= root && root <= zone.high_key)
            {
                if let Some(column) = self.span.column(area, root) {
                    let left = snap_px(column.x, scale);
                    let right = snap_px(column.right(), scale);
                    let height = (area.height * 0.25).min(3.0);
                    let mark = Rect::new(
                        left + 1.0,
                        area.bottom() - height - 3.0,
                        (right - left - 2.0).max(0.0),
                        height,
                    );
                    scene.fill(RoundedRect::new(mark, height * 0.5), hue.color);
                }
            }
            // Its name, and its range under it if both fit; nothing that
            // would not show at least a few letters.
            let pad = 6.0f32.min(shape.rect.width * 0.12);
            let room = shape.rect.width - 2.0 * pad;
            if room >= 14.0 && shape.rect.height >= name_height + 2.0 {
                let top = shape.rect.y + 4.0f32.min((shape.rect.height - name_height) * 0.5);
                let name = line(&zone.name, &name_style.with_color(hue.text), room);
                let range = line(
                    &self.range_text(zone),
                    &range_style.with_color(hue.text.with_alpha(0xB3)),
                    room,
                );
                let both =
                    shape.rect.height >= top - shape.rect.y + name_height + range_height + 2.0;
                scene.clipped(shape, |scene| {
                    scene.text(&name, Point::new(shape.rect.x + pad, top));
                    if both && !range.is_truncated() {
                        scene.text(&range, Point::new(shape.rect.x + pad, top + name_height));
                    }
                });
            }
        }
        // Overlaps, over the zones.
        for (first, a) in areas.iter().enumerate() {
            for b in &areas[first + 1..] {
                if let (Some(a), Some(b)) = (a, b) {
                    if let Some(both) = a.intersect(b).filter(|both| !both.is_empty()) {
                        scene.fill(zone_shape(both, theme), palette.danger.with_alpha(0x59));
                    }
                }
            }
        }
        // Their rings: the hue's hairline, or the accent's for the chosen.
        let ring = cx.focus_ring();
        for (zone, area) in zones.iter().zip(&areas) {
            let Some(area) = *area else {
                continue;
            };
            let shape = zone_shape(area, theme);
            if selected.contains(&zone.id) {
                scene.stroke(inset(shape, 0.75), Stroke::new(1.5), palette.accent);
            } else {
                scene.stroke(inset(shape, 0.5), Stroke::new(1.0), hue_of(zone).ring);
            }
            if self.cursor == Some(zone.id) {
                focus_ring(scene, inset(shape, 2.5), theme, ring);
            }
        }
        if let Some(drop_at) = self.drop_at {
            let last = drop_at
                .key
                .saturating_add(u8::try_from(drop_at.count.saturating_sub(1)).unwrap_or(u8::MAX))
                .min(self.span.high());
            if let (Some(first), Some(last)) = (
                self.span.column(rect, drop_at.key),
                self.span.column(rect, last),
            ) {
                let left = snap_px(first.x, scale);
                let right = snap_px(last.right(), scale);
                let target = Rect::new(left, rect.y, (right - left).max(0.0), rect.height);
                scene.fill(zone_shape(target, theme), palette.accent.with_alpha(0x4D));
                scene.stroke(
                    inset(zone_shape(target, theme), 0.75),
                    Stroke::new(1.5),
                    palette.accent,
                );
            }
        }
        if let Some(press) = &self.press {
            if let Some(reach) = press.band {
                let band = Rect::from_points(press.at, reach);
                scene.fill(band, palette.accent_soft);
                scene.stroke(band, Stroke::new(1.0), palette.accent.with_alpha(0x99));
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                modifiers,
                ..
            } => self.press(cx, at, modifiers),
            Event::PointerMove { at, .. } => self.pointer_move(cx, at),
            Event::PointerUp { .. } => self.release(cx),
            Event::PointerCancel => {
                self.cancel(cx);
                Handled::No
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
                if cx.is_focused() {
                    self.key(cx, key, modifiers)
                } else {
                    Handled::No
                }
            }
            _ => Handled::No,
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        let Some(id) = self.state.lock().zones.get(key).map(|zone| zone.id) else {
            return false;
        };
        match action {
            Action::Press => {
                self.cursor = Some(id);
                self.state.change(|inner| inner.select_only([id]));
            }
            Action::Focus => self.cursor = Some(id),
            _ => return false,
        }
        cx.request_paint();
        true
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, _focused: bool) {
        cx.request_paint();
    }

    fn drag(&mut self, cx: &mut EventCx<'_>, event: &DragEvent<'_>) -> Handled {
        let rect = cx.rect();
        match *event {
            DragEvent::Over { at, payload } => {
                let drop_at = self
                    .span
                    .note_at(rect, at.x)
                    .filter(|_| rect.contains(at))
                    .and_then(|key| {
                        let names = (self.takes)(payload)?;
                        (!names.is_empty()).then_some(DropAt {
                            key,
                            count: names.len(),
                        })
                    });
                if drop_at != self.drop_at {
                    self.drop_at = drop_at;
                    cx.request_paint();
                }
                if drop_at.is_some() {
                    Handled::Yes
                } else {
                    Handled::No
                }
            }
            DragEvent::Left => {
                if self.drop_at.take().is_some() {
                    cx.request_paint();
                }
                Handled::Yes
            }
            DragEvent::Drop { at, payload } => {
                self.drop_at = None;
                cx.request_paint();
                let Some(key) = self.span.note_at(rect, at.x) else {
                    return Handled::No;
                };
                let Some(names) = (self.takes)(payload).filter(|names| !names.is_empty()) else {
                    return Handled::No;
                };
                let high = self.span.high();
                let payload = payload.clone();
                let ids = self.state.change(|inner| {
                    let mut ids = Vec::new();
                    for (step, name) in names.into_iter().enumerate() {
                        let Some(key) = u8::try_from(usize::from(key) + step)
                            .ok()
                            .filter(|&key| key <= high)
                        else {
                            break;
                        };
                        let id = inner.fresh_id();
                        inner
                            .zones
                            .push(Zone::new(id, name, key, key).with_root(key));
                        ids.push(id);
                    }
                    inner.select_only(ids.clone());
                    inner.events.push(ZoneEvent::Created {
                        ids: ids.clone(),
                        payload,
                    });
                    ids
                });
                self.cursor = ids.last().copied();
                Handled::Yes
            }
            _ => Handled::No,
        }
    }
}

impl std::fmt::Debug for ZoneMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZoneMap")
            .field("name", &self.name)
            .field("span", &self.span)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
