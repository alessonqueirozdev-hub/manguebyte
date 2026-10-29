//! A piano keyboard that plays notes from the pointer and the computer's
//! keys, and shows the notes the application says are sounding and the
//! key ranges it marks.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use manguebyte_gfx::{Align, Circle, Color, Point, Radii, Rect, RoundedRect, Scene, Shadow, Size};

use super::keys::{KeySpan, is_black, note_name};
use super::{device_px, focus_ring, glow_fill, line, snap_px, value_style, vertical};
use crate::access::{AccessItem, AccessNode, Action, Actions, Role};
use crate::event::{Button, Event, Handled, Key, KeyEvent};
use crate::motion::{Animated, Lerp};
use crate::widget::{AccessCx, EventCx, MeasureCx, PaintCx, UpdateCx, Widget};

/// How tall the strip along the top that shows key ranges is, in logical
/// pixels.
const RANGE_STRIP: f32 = 8.0;

/// How far down the keys the black keys reach, as a fraction.
const BLACK_LENGTH: f32 = 0.62;

/// The letters that play notes, as on most music programs' computer
/// keyboards, and how many semitones above the C of their octave each
/// plays: the row from A plays the white keys, the row above it the black
/// keys between them.
const LETTERS: [(char, u8); 18] = [
    ('a', 0),
    ('w', 1),
    ('s', 2),
    ('e', 3),
    ('d', 4),
    ('f', 5),
    ('t', 6),
    ('g', 7),
    ('y', 8),
    ('h', 9),
    ('u', 10),
    ('j', 11),
    ('k', 12),
    ('o', 13),
    ('l', 14),
    ('p', 15),
    (';', 16),
    ('\'', 17),
];

/// A note starting or stopping, as a keyboard reports it: on the
/// interface's thread, with no time within an audio block. The
/// application takes them with [`KeyboardState::take_events`] once a
/// frame and passes them to its audio thread its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum NoteEvent {
    /// `note` starts, at `velocity` from 1 to 127.
    On {
        /// The MIDI note number.
        note: u8,
        /// How hard, from 1 to 127.
        velocity: u8,
    },
    /// `note` stops.
    Off {
        /// The MIDI note number.
        note: u8,
    },
}

/// A run of keys a keyboard marks, such as the keys a sample is played
/// on, with the key it sounds at its own pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyRange {
    /// The lowest key.
    pub low: u8,
    /// The highest key.
    pub high: u8,
    /// The key that plays the sample at its own pitch, if marked.
    pub root: Option<u8>,
}

impl KeyRange {
    /// The keys from `low` to `high`, in either order, with no root.
    pub fn new(low: u8, high: u8) -> KeyRange {
        KeyRange {
            low: low.min(high),
            high: low.max(high),
            root: None,
        }
    }

    /// The same range with `root` marked.
    #[must_use]
    pub fn with_root(mut self, root: u8) -> KeyRange {
        self.root = Some(root);
        self
    }

    /// Whether `note` is in the range.
    pub fn contains(&self, note: u8) -> bool {
        (self.low..=self.high).contains(&note)
    }
}

struct KeysInner {
    sounding: [bool; 128],
    down: [bool; 128],
    ranges: Vec<KeyRange>,
    events: Vec<NoteEvent>,
    version: u64,
}

impl Default for KeysInner {
    fn default() -> Self {
        KeysInner {
            sounding: [false; 128],
            down: [false; 128],
            ranges: Vec::new(),
            events: Vec::new(),
            version: 0,
        }
    }
}

/// What a [`PianoKeyboard`] shares with the application: the notes the
/// application says are sounding and the ranges it marks, which the
/// keyboard shows, and the notes a person plays, which the application
/// takes as [`NoteEvent`]s.
///
/// It is `Send`, so a plugin's editor spec can keep it. Its
/// [`version`](KeyboardState::version) counts every change.
///
/// ```
/// use manguebyte_ui::{KeyRange, KeyboardState};
///
/// let state = KeyboardState::new();
/// state.set_sounding([60, 64, 67]);
/// assert!(state.is_sounding(64) && !state.is_sounding(65));
/// state.set_ranges(vec![KeyRange::new(48, 59).with_root(55)]);
/// assert!(state.take_events().is_empty());
/// ```
#[derive(Clone, Default)]
pub struct KeyboardState {
    inner: Arc<Mutex<KeysInner>>,
}

impl KeyboardState {
    /// Nothing sounding, no ranges.
    pub fn new() -> KeyboardState {
        KeyboardState::default()
    }

    fn lock(&self) -> MutexGuard<'_, KeysInner> {
        // Nothing that can panic runs while the lock is held.
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change<R>(&self, change: impl FnOnce(&mut KeysInner) -> R) -> R {
        let mut inner = self.lock();
        let result = change(&mut inner);
        inner.version = inner.version.wrapping_add(1);
        result
    }

    /// A number that changes with every change to the state.
    pub fn version(&self) -> u64 {
        self.lock().version
    }

    /// Lights `notes`, and no others: the notes the application says are
    /// sounding, from its voices or from MIDI coming in. Notes above 127
    /// are ignored. Nothing changes, and nothing is drawn again, if the
    /// same notes were lit already.
    pub fn set_sounding(&self, notes: impl IntoIterator<Item = u8>) {
        let mut sounding = [false; 128];
        for note in notes {
            if let Some(lit) = sounding.get_mut(usize::from(note)) {
                *lit = true;
            }
        }
        let mut inner = self.lock();
        if inner.sounding != sounding {
            inner.sounding = sounding;
            inner.version = inner.version.wrapping_add(1);
        }
    }

    /// Whether the application said `note` is sounding.
    pub fn is_sounding(&self, note: u8) -> bool {
        self.lock()
            .sounding
            .get(usize::from(note))
            .copied()
            .unwrap_or(false)
    }

    /// The notes a person holds down on the keyboard now.
    pub fn down(&self) -> Vec<u8> {
        let inner = self.lock();
        (0..128u8)
            .filter(|&note| inner.down[usize::from(note)])
            .collect()
    }

    /// Marks `ranges` on the keyboard, and no others.
    pub fn set_ranges(&self, ranges: Vec<KeyRange>) {
        self.change(|inner| inner.ranges = ranges);
    }

    /// The ranges marked.
    pub fn ranges(&self) -> Vec<KeyRange> {
        self.lock().ranges.clone()
    }

    /// The notes started and stopped since the application last asked,
    /// oldest first, forgetting them.
    pub fn take_events(&self) -> Vec<NoteEvent> {
        std::mem::take(&mut self.lock().events)
    }
}

impl std::fmt::Debug for KeyboardState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("KeyboardState")
            .field("ranges", &inner.ranges)
            .field("events", &inner.events.len())
            .field("version", &inner.version)
            .finish_non_exhaustive()
    }
}

type Names = Box<dyn Fn(u8) -> String>;

/// A piano keyboard over a [`KeySpan`]: 61 keys from C2 unless told
/// otherwise, or the 88 of a piano.
///
/// - A press on a key plays it, harder the further down the key it lands
///   (velocity 1 at the top, 127 at the bottom); dragging across keys
///   plays each in turn (a glissando), and releasing, or leaving the
///   keys, stops it.
/// - With the keyboard focus, the letters play notes as on most music
///   programs: A, S, D, F, G, H, J and K the white keys of an octave,
///   W, E, T, Y and U the black keys between them, then O, L, P, ; and '
///   on up. Z and X move the letters an octave down and up. The letters
///   can be turned off with
///   [`computer_keys`](PianoKeyboard::computer_keys). Left and Right
///   move the key the keyboard is on, Up and Down by an octave, and Space
///   plays it while held.
/// - Notes the application lights with
///   [`KeyboardState::set_sounding`] are drawn in the accent colour, as
///   are the keys held down; ranges it marks are bars in a strip along the
///   top, with a dot on their root key.
///
/// What is played is polled from the [`KeyboardState`] as
/// [`NoteEvent`]s: a note on when a key goes down, a note off when it
/// comes up, never two ons for one key however many ways it is held.
/// Closing the interface stops every note it still holds.
///
/// For assistive technology each key is a [`Role::Button`] named by its
/// note (C4 for middle C, or names of the application's own through
/// [`note_names`](PianoKeyboard::note_names)), whose value says whether it
/// is down, sounding, and in or the root of a range, and which can be
/// pressed to hold it down and pressed again to let it go.
pub struct PianoKeyboard {
    name: String,
    span: KeySpan,
    state: KeyboardState,
    names: Names,
    computer: bool,
    /// The C the letters play from.
    letters_from: u8,
    letter_velocity: u8,
    /// How many ways each note is held down.
    holds: [u8; 128],
    /// The note the pointer holds.
    pointer: Option<u8>,
    /// Letters held, and the notes they play.
    letters: Vec<(char, u8)>,
    /// Notes assistive technology pressed down.
    pressed: [bool; 128],
    /// The note Space plays: the key the keyboard is on.
    cursor: u8,
    space: Option<u8>,
    version: Option<u64>,
    /// The key under the pointer and how far its hover has come in, and
    /// the key it left, fading out.
    hover_key: Option<(u8, Animated<f32>)>,
    fading: Option<(u8, Animated<f32>)>,
}

/// A keyboard called `name` over `span`, sharing what it plays and shows
/// through `state`.
///
/// ```
/// use manguebyte_gfx::Size;
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{KeySpan, KeyboardState, NoteEvent, Role, piano_keyboard};
///
/// let state = KeyboardState::new();
/// let view = piano_keyboard("Keys", KeySpan::new(48, 71), state.clone());
/// let mut ui = Harness::new(view, Size::new(280.0, 90.0), 1.0);
/// ui.frame();
/// let c4 = ui.get(Role::Button, "C4");
/// ui.click(c4.center());
/// let events = state.take_events();
/// assert!(matches!(events[0], NoteEvent::On { note: 60, .. }));
/// assert_eq!(events[1], NoteEvent::Off { note: 60 });
///
/// // The application lights the notes its voices play.
/// state.set_sounding([64]);
/// ui.frame();
/// assert_eq!(ui.get(Role::Button, "E4").value_text(), Some("up, sounding"));
/// ```
pub fn piano_keyboard(
    name: impl Into<String>,
    span: KeySpan,
    state: KeyboardState,
) -> PianoKeyboard {
    let letters_from = if span.contains(60) {
        60
    } else {
        (u16::from(span.low()).div_ceil(12) * 12).min(120) as u8
    };
    PianoKeyboard {
        name: name.into(),
        span,
        state,
        names: Box::new(note_name),
        computer: true,
        letters_from: letters_from.min(120),
        letter_velocity: 100,
        holds: [0; 128],
        pointer: None,
        letters: Vec::new(),
        pressed: [false; 128],
        cursor: if span.contains(60) { 60 } else { span.low() },
        space: None,
        version: None,
        hover_key: None,
        fading: None,
    }
}

impl PianoKeyboard {
    /// Moves the hover to `key`: the key it leaves fades out.
    fn hover_to(&mut self, cx: &mut EventCx<'_>, key: Option<u8>) {
        if self.hover_key.map(|(at, _)| at) == key {
            return;
        }
        let (now, fast) = (cx.now(), cx.motion().fast());
        if let Some((left, mut amount)) = self.hover_key.take() {
            amount.transition(0.0, now, fast);
            self.fading = Some((left, amount));
        }
        self.hover_key = key.map(|key| {
            let mut amount = Animated::new(0.0);
            amount.transition(1.0, now, fast);
            (key, amount)
        });
        cx.request_paint();
    }

    /// The same keyboard naming notes with `names`, for people and for
    /// assistive technology: [`note_name_portuguese`](super::keys::note_name_portuguese)
    /// for one.
    #[must_use]
    pub fn note_names(mut self, names: impl Fn(u8) -> String + 'static) -> PianoKeyboard {
        self.names = Box::new(names);
        self
    }

    /// The same keyboard, playing from the computer's letters when it has
    /// the focus (`true`, the default) or not.
    #[must_use]
    pub fn computer_keys(mut self, on: bool) -> PianoKeyboard {
        self.computer = on;
        self
    }

    /// The same keyboard, with the letters playing at `velocity` (1 to
    /// 127; 100 unless set).
    #[must_use]
    pub fn letter_velocity(mut self, velocity: u8) -> PianoKeyboard {
        self.letter_velocity = velocity.clamp(1, 127);
        self
    }

    /// The state it shares with the application.
    pub fn state(&self) -> &KeyboardState {
        &self.state
    }

    /// Where the keys are: the rectangle below the range strip.
    fn keys(rect: Rect) -> Rect {
        let strip = RANGE_STRIP.min(rect.height);
        Rect::new(
            rect.x,
            rect.y + strip,
            rect.width,
            (rect.height - strip).max(0.0),
        )
    }

    /// Where `note`'s key is drawn, in `keys`.
    fn key_rect(&self, keys: Rect, note: u8) -> Option<Rect> {
        if is_black(note) {
            let column = self.span.column(keys, note)?;
            let inset = column.width * 0.08;
            Some(Rect::new(
                column.x + inset,
                keys.y,
                (column.width - 2.0 * inset).max(0.0),
                keys.height * BLACK_LENGTH,
            ))
        } else {
            self.span.white_key(keys, note)
        }
    }

    /// The key at `at` and the velocity a press there plays, if any.
    fn key_at(&self, rect: Rect, at: Point) -> Option<(u8, u8)> {
        let keys = Self::keys(rect);
        if !keys.contains(at) {
            return None;
        }
        let along = |key: Rect| ((at.y - key.y) / key.height.max(1.0)).clamp(0.0, 1.0);
        let velocity = |fraction: f32| 1 + (fraction * 126.0).round() as u8;
        if let Some(note) = self.span.note_at(keys, at.x).filter(|&note| is_black(note)) {
            let key = self.key_rect(keys, note)?;
            if key.contains(at) {
                return Some((note, velocity(along(key))));
            }
        }
        let note = self.span.white_at(keys, at.x)?;
        Some((note, velocity(along(keys))))
    }

    /// Holds `note` down one more way; it starts if it was up.
    fn hold(&mut self, note: u8, velocity: u8) {
        let Some(holds) = self.holds.get_mut(usize::from(note)) else {
            return;
        };
        *holds = holds.saturating_add(1);
        if *holds == 1 {
            self.state.change(|inner| {
                inner.down[usize::from(note)] = true;
                inner.events.push(NoteEvent::On { note, velocity });
            });
        }
    }

    /// Lets `note` go one way; it stops once nothing holds it.
    fn let_go(&mut self, note: u8) {
        let Some(holds) = self.holds.get_mut(usize::from(note)) else {
            return;
        };
        if *holds == 0 {
            return;
        }
        *holds -= 1;
        if *holds == 0 {
            self.state.change(|inner| {
                inner.down[usize::from(note)] = false;
                inner.events.push(NoteEvent::Off { note });
            });
        }
    }

    /// Lets go of everything the letters, Space and assistive technology
    /// hold.
    fn let_go_of_keys(&mut self) {
        for (_, note) in std::mem::take(&mut self.letters) {
            self.let_go(note);
        }
        if let Some(note) = self.space.take() {
            self.let_go(note);
        }
        for note in 0..128u8 {
            if std::mem::take(&mut self.pressed[usize::from(note)]) {
                self.let_go(note);
            }
        }
    }

    fn pointer_to(&mut self, key: Option<(u8, u8)>) {
        let note = key.map(|(note, _)| note);
        if note == self.pointer {
            return;
        }
        if let Some(old) = self.pointer.take() {
            self.let_go(old);
        }
        if let Some((note, velocity)) = key {
            self.hold(note, velocity);
            self.pointer = Some(note);
        }
    }

    /// The highest C the letters may start from: the letters' notes stay
    /// below 128.
    fn letters_top() -> u8 {
        120
    }

    fn letter(&mut self, cx: &mut EventCx<'_>, key: KeyEvent) -> Handled {
        let Key::Character(c) = key.key else {
            return Handled::No;
        };
        let c = c.to_ascii_lowercase();
        if !key.pressed {
            return match self.letters.iter().position(|&(held, _)| held == c) {
                Some(at) => {
                    let (_, note) = self.letters.swap_remove(at);
                    self.let_go(note);
                    cx.request_paint();
                    Handled::Yes
                }
                None => Handled::No,
            };
        }
        match c {
            'z' => {
                self.letters_from = self.letters_from.saturating_sub(12);
                cx.request_paint();
                return Handled::Yes;
            }
            'x' => {
                self.letters_from = (self.letters_from + 12).min(Self::letters_top());
                cx.request_paint();
                return Handled::Yes;
            }
            _ => {}
        }
        let Some(&(_, offset)) = LETTERS.iter().find(|&&(letter, _)| letter == c) else {
            return Handled::No;
        };
        if self.letters.iter().any(|&(held, _)| held == c) {
            // The system repeats a key held down: the note is already on.
            return Handled::Yes;
        }
        let note = self.letters_from.saturating_add(offset);
        if note <= 127 {
            self.letters.push((c, note));
            self.hold(note, self.letter_velocity);
            cx.request_paint();
        }
        Handled::Yes
    }

    fn cursor_key(&mut self, cx: &mut EventCx<'_>, key: KeyEvent) -> Handled {
        if key.key == Key::Character(' ') {
            match (key.pressed, self.space) {
                (true, None) => {
                    self.space = Some(self.cursor);
                    self.hold(self.cursor, self.letter_velocity);
                }
                (false, Some(note)) => {
                    self.space = None;
                    self.let_go(note);
                }
                _ => {}
            }
            cx.request_paint();
            return Handled::Yes;
        }
        if !key.pressed {
            return Handled::No;
        }
        let (low, high) = (self.span.low(), self.span.high());
        let cursor = match key.key {
            Key::Left => self.cursor.saturating_sub(1).max(low),
            Key::Right => self.cursor.saturating_add(1).min(high),
            Key::Down => self.cursor.saturating_sub(12).max(low),
            Key::Up => self.cursor.saturating_add(12).min(high),
            _ => return Handled::No,
        };
        self.cursor = cursor;
        cx.request_paint();
        Handled::Yes
    }

    /// What the value of `note`'s key says.
    fn key_value(&self, note: u8, inner: &KeysInner) -> String {
        let index = usize::from(note);
        let mut value = String::from(if inner.down[index] { "down" } else { "up" });
        if inner.sounding[index] {
            value.push_str(", sounding");
        }
        if inner.ranges.iter().any(|range| range.root == Some(note)) {
            value.push_str(", root of a range");
        } else if inner.ranges.iter().any(|range| range.contains(note)) {
            value.push_str(", in a range");
        }
        value
    }
}

impl Widget for PianoKeyboard {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        let names = &self.names;
        let mut value = format!("{} to {}", names(self.span.low()), names(self.span.high()));
        if self.computer {
            value.push_str(&format!(
                "; the letters play from {}",
                names(self.letters_from)
            ));
        }
        AccessNode::new(Role::Group, self.name.as_str())
            .with_value(value)
            .with_actions(Actions {
                focus: true,
                ..Actions::NONE
            })
    }

    fn access_items(&self, cx: &AccessCx<'_>, items: &mut Vec<AccessItem>) {
        let keys = Self::keys(cx.rect());
        let inner = self.state.lock();
        for note in self.span.notes() {
            let Some(bounds) = self.key_rect(keys, note) else {
                continue;
            };
            let down = inner.down[usize::from(note)];
            let mut node = AccessNode::new(Role::Button, (self.names)(note))
                .with_value(self.key_value(note, &inner))
                .with_checked(down)
                .with_actions(Actions::PRESS);
            node.state.focused = cx.is_focused() && self.cursor == note;
            items.push(AccessItem::new(usize::from(note), node, bounds));
        }
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        Size::new(self.span.len() as f32 * 9.0, RANGE_STRIP + 72.0)
    }

    fn update(&mut self, cx: &mut UpdateCx<'_>) {
        let version = self.state.version();
        if self.version != Some(version) {
            self.version = Some(version);
            cx.request_paint();
        }
        for (_, amount) in [&mut self.hover_key, &mut self.fading]
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
        let key = self.key_at(cx.rect(), at).map(|(note, _)| note);
        self.hover_to(cx, key);
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
        let keys = Self::keys(rect);
        let inner = self.state.lock();
        let gap = device_px(1.0, scale);
        let palette = &theme.palette;
        let lit = |note: u8| {
            let index = usize::from(note);
            inner.down[index] || inner.sounding[index]
        };
        let (hover_key, fading) = (self.hover_key, self.fading);
        let hover_of = |note: u8| {
            [hover_key, fading]
                .into_iter()
                .flatten()
                .find(|(at, _)| *at == note)
                .map_or(0.0, |(_, amount)| amount.value())
        };
        let label = value_style(theme).with_color(palette.key_black.with_alpha(0x99));
        // Keys rounded at their foot, as keys are.
        let foot = |key: Rect, radius: f32| {
            let r = radius.min(key.width * 0.4).max(0.0);
            RoundedRect::with_radii(
                key,
                Radii {
                    top_left: 0.0,
                    top_right: 0.0,
                    bottom_right: r,
                    bottom_left: r,
                },
            )
        };
        for note in self.span.notes().filter(|&note| !is_black(note)) {
            let Some(key) = self.span.white_key(keys, note) else {
                continue;
            };
            let left = snap_px(key.x, scale);
            let right = snap_px(key.right(), scale) - gap;
            let key = Rect::new(left, key.y, (right - left).max(0.0), key.height);
            let shape = foot(key, 6.0);
            if lit(note) {
                glow_fill(scene, shape, palette.accent, theme, 0.9);
                vertical(
                    scene,
                    shape,
                    (key.y, key.bottom()),
                    palette.accent_bright,
                    palette.accent,
                );
            } else {
                // A soft gradient, shaded at the top where the black keys
                // sit, a little darker under the pointer.
                let shade = Color::BLACK.with_opacity(0.06 * hover_of(note));
                vertical(
                    scene,
                    shape,
                    (key.y, key.bottom()),
                    palette.key_white_shade,
                    palette.key_white,
                );
                if shade.a > 0 {
                    scene.fill(shape, shade);
                }
            }
            // An edge that parts the key from a light surface behind it.
            scene.stroke(
                foot(
                    Rect::new(
                        key.x + 0.5,
                        key.y - 0.5,
                        (key.width - 1.0).max(0.0),
                        key.height,
                    ),
                    5.5,
                ),
                manguebyte_gfx::Stroke::new(1.0),
                palette.hairline,
            );
            // The lip at the foot of the key.
            scene.clipped(shape, |scene| {
                let lip = 3.0f32.min(key.height * 0.1);
                scene.fill(
                    Rect::new(key.x, key.bottom() - lip, key.width, lip),
                    Color::BLACK.with_alpha(0x14),
                );
            });
            if note % 12 == 0 {
                let text = line(
                    &(self.names)(note),
                    &label.with_align(Align::Center),
                    key.width,
                );
                if !text.is_truncated() {
                    let y = key.bottom() - text.size().height - 2.0;
                    scene.text(&text, Point::new(key.x, y));
                }
            }
        }
        for note in self.span.notes().filter(|&note| is_black(note)) {
            let Some(key) = self.key_rect(keys, note) else {
                continue;
            };
            let left = snap_px(key.x, scale);
            let right = snap_px(key.right(), scale);
            let bottom = snap_px(key.bottom(), scale);
            let key = Rect::new(left, key.y, (right - left).max(0.0), bottom - key.y);
            let shape = foot(key, 4.0);
            let lift = Shadow::new(Color::BLACK.with_alpha(0x80))
                .with_offset(0.0, 2.0)
                .with_blur(5.0);
            scene.shadow(shape, lift);
            if lit(note) {
                glow_fill(scene, shape, palette.accent, theme, 0.9);
                vertical(
                    scene,
                    shape,
                    (key.y, key.bottom()),
                    palette.accent_bright,
                    palette.accent,
                );
            } else {
                let top = palette
                    .key_black_top
                    .lerp(palette.text_muted, 0.25 * hover_of(note));
                vertical(scene, shape, (key.y, key.bottom()), top, palette.key_black);
                // Light along the foot.
                scene.clipped(shape, |scene| {
                    let lip = 2.0f32.min(key.height * 0.1);
                    scene.fill(
                        Rect::new(key.x, key.bottom() - lip, key.width, lip),
                        Color::WHITE.with_alpha(0x10),
                    );
                });
            }
        }
        // Ranges along the top, and a dot on each root key.
        let strip = Rect::new(rect.x, rect.y, rect.width, keys.y - rect.y);
        for range in &inner.ranges {
            let (low, high) = (
                range.low.max(self.span.low()),
                range.high.min(self.span.high()),
            );
            if low > high {
                continue;
            }
            let (Some(first), Some(last)) =
                (self.span.column(strip, low), self.span.column(strip, high))
            else {
                continue;
            };
            let left = snap_px(first.x, scale);
            let right = snap_px(last.right(), scale);
            let bar = Rect::new(
                left,
                strip.y + 1.0,
                (right - left).max(0.0),
                (strip.height - 2.0).max(0.0),
            );
            scene.fill(
                RoundedRect::new(bar, bar.height * 0.5),
                palette.accent.with_alpha(0x80),
            );
            if let Some(root) = range.root.filter(|&root| self.span.contains(root)) {
                if let Some(column) = self.span.column(strip, root) {
                    let left = snap_px(column.x, scale);
                    let right = snap_px(column.right(), scale);
                    let mark = Rect::new(left, strip.y, (right - left).max(0.0), strip.height);
                    scene.fill(
                        RoundedRect::new(mark, mark.width.min(mark.height) * 0.5),
                        palette.accent,
                    );
                }
                if let Some(key) = self.key_rect(keys, root) {
                    let radius = (key.width * 0.25).clamp(1.0, 4.0);
                    let center = Point::new(key.center().x, key.bottom() - radius - 3.0);
                    let center = if is_black(root) {
                        center
                    } else {
                        // On the part of a white key no black key covers.
                        Point::new(
                            self.span
                                .column(keys, root)
                                .map_or(center.x, |column| column.center().x),
                            center.y,
                        )
                    };
                    scene.fill(Circle::new(center, radius), palette.accent);
                }
            }
        }
        drop(inner);
        let ring = cx.focus_ring();
        if ring > 0.0 {
            if let Some(key) = self.key_rect(keys, self.cursor) {
                let ring_rect = Rect::new(
                    key.x + 1.5,
                    key.y + 1.5,
                    (key.width - 3.0).max(0.0),
                    (key.height - 3.0).max(0.0),
                );
                focus_ring(scene, foot(ring_rect, 4.0), theme, ring);
            }
        }
    }

    fn event(&mut self, cx: &mut EventCx<'_>, event: &Event) -> Handled {
        match *event {
            Event::PointerDown {
                at,
                button: Button::Primary,
                ..
            } => {
                cx.request_focus();
                cx.capture_pointer();
                let key = self.key_at(cx.rect(), at);
                if let Some((note, _)) = key {
                    self.cursor = note;
                }
                self.pointer_to(key);
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerMove { at, .. } if cx.has_pointer() => {
                let key = self.key_at(cx.rect(), at);
                self.pointer_to(key);
                cx.request_paint();
                Handled::Yes
            }
            Event::PointerUp { .. } | Event::PointerCancel if cx.has_pointer() => {
                self.pointer_to(None);
                cx.release_pointer();
                cx.request_paint();
                Handled::Yes
            }
            Event::Key(key) if cx.is_focused() => {
                let modifiers = key.modifiers;
                if modifiers.control || modifiers.alt || modifiers.meta {
                    return Handled::No;
                }
                if self.cursor_key(cx, key).is_yes() {
                    return Handled::Yes;
                }
                if self.computer {
                    self.letter(cx, key)
                } else {
                    Handled::No
                }
            }
            _ => Handled::No,
        }
    }

    fn act_item(&mut self, cx: &mut EventCx<'_>, key: usize, action: Action) -> bool {
        let Ok(note) = u8::try_from(key) else {
            return false;
        };
        if !self.span.contains(note) {
            return false;
        }
        match action {
            Action::Focus => self.cursor = note,
            Action::Press => {
                let index = usize::from(note);
                self.pressed[index] = !self.pressed[index];
                if self.pressed[index] {
                    self.hold(note, self.letter_velocity);
                } else {
                    self.let_go(note);
                }
            }
            _ => return false,
        }
        cx.request_paint();
        true
    }

    fn focus_changed(&mut self, cx: &mut EventCx<'_>, focused: bool) {
        if !focused {
            self.let_go_of_keys();
        }
        cx.request_paint();
    }
}

impl Drop for PianoKeyboard {
    fn drop(&mut self) {
        // Closing the interface stops what it still holds.
        self.pointer_to(None);
        self.let_go_of_keys();
    }
}

impl std::fmt::Debug for PianoKeyboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PianoKeyboard")
            .field("name", &self.name)
            .field("span", &self.span)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}
